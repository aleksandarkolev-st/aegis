use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::Result;
use arun::storage::Store;
use serde_json::json;

#[path = "support/http.rs"]
mod http;

#[test]
fn direct_capture_limits_reject_oversized_actions_and_envelopes_before_execution() -> Result<()> {
    for oversized_action in [false, true] {
        let directory = tempfile::tempdir()?;
        let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = std::sync::Arc::clone(&requests);
        let endpoint = http::Endpoint::start(move |body| {
            assert_eq!(http::state(body)?["task"], "Capture fixture");
            observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let action = json!({"kind":"invoke","capability":"workspace.write",
                "args":{"path":"should-not-exist.txt","text":if oversized_action { "x".repeat(2048) } else { "blocked".into() }}});
            let mut response = json!({"choices":[{"message":{"content":action.to_string()}}],
                "usage":{"prompt_tokens":10,"completion_tokens":2}});
            if !oversized_action {
                response["padding"] = json!("x".repeat(2048));
            }
            assert!(response.to_string().len() > 1024);
            Ok((200, response))
        })?;
        let started = Instant::now();
        let output = Command::new(env!("CARGO_BIN_EXE_arun"))
            .current_dir(directory.path())
            .env("PATH", directory.path())
            .args([
                "run",
                "Capture fixture",
                "--provider",
                "custom",
                "--endpoint",
                &endpoint.url,
                "--model",
                "fixture-model",
                "--model-response-bytes",
                "1024",
                "--foreground",
            ])
            .output()?;
        endpoint.finish()?;
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "capture did not stop promptly: oversized_action={oversized_action}"
        );
        let store = Store::open(&directory.path().join(".arun"))?;
        let run = &store.runs()?[0];
        assert_eq!(run.budgets["model_response_bytes"], 1024);
        assert_eq!(run.state, "waiting_recovery");
        assert_eq!(store.event_count(&run.id, "model.response")?, 0);
        assert!(store.operations(&run.id)?.is_empty());
        assert!(!directory.path().join("should-not-exist.txt").exists());
        assert_eq!(store.event_count(&run.id, "model.failed")?, 1);
        assert_eq!(store.model_tokens(&run.id)?, 0);
        assert_eq!(
            arun::trace::metrics(&store.events(&run.id)?).unaccounted_model_attempts,
            1
        );
        assert!(store.events(&run.id)?.iter().any(|event| {
            event.kind == "model.failed"
                && event.payload["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("configured byte limit"))
        }));
    }
    Ok(())
}

#[test]
fn custom_response_budget_rejects_advertised_and_chunked_oversized_bodies() -> Result<()> {
    for chunked in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let server = std::thread::spawn(move || -> Result<()> {
            let (mut stream, _) = listener.accept()?;
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            let mut bytes = Vec::new();
            let mut buffer = [0; 4096];
            while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = stream.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..count]);
            }
            if chunked {
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n800\r\n{}\r\n0\r\n\r\n",
                    "x".repeat(2048)
                )?;
            } else {
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: 2048\r\nConnection: close\r\n\r\n"
                )?;
            }
            Ok(())
        });
        let endpoint = arun::endpoint::Endpoint {
            base_url: format!("http://{address}/v1"),
            api_key_env: None,
            response_format: arun::endpoint::ResponseFormat::None,
            allow_insecure: false,
        };
        let error = endpoint
            .call_bounded("fixture", "prompt", Duration::from_secs(5), || false, 1024)
            .unwrap_err();
        assert!(
            error.to_string().contains("configured byte limit"),
            "{error:#}"
        );
        server.join().unwrap()?;
    }
    Ok(())
}
