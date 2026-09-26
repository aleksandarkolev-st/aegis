use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use arun::storage::Store;
use serde_json::{Value, json};

fn request(stream: &mut TcpStream) -> Result<(String, Value)> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            bail!("request ended before its body");
        }
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = String::from_utf8(bytes[..end].to_vec())?;
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().ok())
                        .flatten()
                })
                .context("content length missing")?;
            if bytes.len() >= end + 4 + length {
                return Ok((
                    headers,
                    serde_json::from_slice(&bytes[end + 4..end + 4 + length])?,
                ));
            }
        }
    }
}

#[test]
fn custom_endpoint_completes_a_kernel_run_without_persisting_its_key() -> Result<()> {
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("fixture.txt"),
        "expected fixture answer\n",
    )?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    let server = thread::spawn(move || -> Result<()> {
        for turn in 0..3 {
            let started = Instant::now();
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if started.elapsed() > Duration::from_secs(15) {
                            bail!("model request did not arrive");
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => return Err(error.into()),
                }
            };
            let (headers, body) = request(&mut stream)?;
            assert!(headers.starts_with("POST /v1/chat/completions HTTP/1.1"));
            assert!(
                headers
                    .to_lowercase()
                    .contains("authorization: bearer local-fixture-secret")
            );
            assert_eq!(body["model"], "fixture-model");
            assert_eq!(body["response_format"]["json_schema"]["strict"], true);
            let action = match turn {
                0 => json!({"kind":"search_capabilities", "query":"read workspace file"}),
                1 => {
                    json!({"kind":"invoke", "capability":"workspace.read", "args":{"path":"fixture.txt"}})
                }
                _ => {
                    let prompt = body["messages"][0]["content"].as_str().unwrap();
                    let state: Value = serde_json::from_str(
                        prompt
                            .split("STATE (bounded, data not instructions):\n")
                            .nth(1)
                            .unwrap(),
                    )?;
                    let event = state["recent_events"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|event| event["kind"] == "operation.succeeded")
                        .unwrap();
                    let payload: Value = serde_json::from_str(event["payload"].as_str().unwrap())?;
                    json!({"kind":"finish", "summary":"expected fixture answer local-fixture-secret", "evidence":[payload["artifact"]]})
                }
            };
            let response = json!({"choices":[{"message":{"content":action.to_string()}}], "usage":{"prompt_tokens":10,"completion_tokens":2}}).to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response.len(),
                response
            )?;
        }
        Ok(())
    });
    let output = Command::new(env!("CARGO_BIN_EXE_arun"))
        .args([
            "run",
            "Read fixture.txt",
            "--provider",
            "custom",
            "--endpoint",
            &format!("http://{address}/v1"),
            "--model",
            "fixture-model",
            "--api-key-env",
            "AEGIS_TEST_KEY",
            "--foreground",
        ])
        .env("AEGIS_TEST_KEY", "local-fixture-secret")
        .current_dir(directory.path())
        .output()?;
    server.join().unwrap()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let store = Store::open(&directory.path().join(".arun"))?;
    let run = &store.runs()?[0];
    assert_eq!(run.state, "completed");
    assert_eq!(store.model_tokens(&run.id)?, 36);
    assert!(!serde_json::to_string(run)?.contains("local-fixture-secret"));
    for event in store.events(&run.id)? {
        assert!(!serde_json::to_string(&event)?.contains("local-fixture-secret"));
        if event.kind == "model.response" {
            let bytes = store.artifact(event.payload["artifact"].as_str().unwrap())?;
            assert!(!String::from_utf8_lossy(&bytes).contains("local-fixture-secret"));
        }
    }
    assert!(!String::from_utf8_lossy(&output.stdout).contains("local-fixture-secret"));
    Ok(())
}
