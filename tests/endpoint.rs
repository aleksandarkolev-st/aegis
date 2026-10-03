use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use arun::model::Checkpoint;
use arun::routing::Reason;
use arun::storage::Store;
use serde_json::{Value, json};
#[path = "support/operation.rs"]
mod operation_fixture;

fn request(stream: &mut TcpStream) -> Result<(String, Value)> {
    stream.set_nonblocking(false)?;
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
                .unwrap_or(0);
            if bytes.len() >= end + 4 + length {
                return Ok((
                    headers,
                    if length == 0 {
                        Value::Null
                    } else {
                        serde_json::from_slice(&bytes[end + 4..end + 4 + length])?
                    },
                ));
            }
        }
    }
}

#[test]
fn recorded_direct_failure_resumes_same_run_on_local_endpoint() -> Result<()> {
    local_fallback_continuation(None)
}

#[test]
fn recorded_direct_failure_resumes_with_private_fallback_key() -> Result<()> {
    local_fallback_continuation(Some("private-fallback-run-key"))
}

fn local_fallback_continuation(key: Option<&str>) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    let mut store = Store::open(&root)?;
    let run = store.create_run(
        "Refactor parser\nRequirements:\n- preserve API",
        directory.path(),
        "codex",
        json!([]),
        json!({"provider_transport":"aegis-direct-v1","model":"primary","fallback_routes":[{"provider":"custom","model":"local-fixture","endpoint":{"base_url":format!("http://{address}/v1"),"api_key_env":key.map(|_| "ARUN_SESSION_API_KEY"),"response_format":"schema","allow_insecure":false}}]}),
        "",
    )?;
    store.state(&run.id, "running", json!({}))?;
    let operation = store.begin_operation(&run.id, "workspace.read", json!({}), true)?;
    let evidence = store.put_artifact(b"API compatibility proof")?;
    operation_fixture::claim_fixture_operation(&mut store, &operation.id)?;
    store.operation_state(&operation, "succeeded", Some(&evidence), json!({}))?;
    store.verify_obligation(&run.id, 1, &[evidence.clone()])?;
    let checkpoint = Checkpoint {
        decisions: vec!["keep public API".into()],
        unresolved: Vec::new(),
        next_action: "finish after reviewing proof".into(),
        milestones: Vec::new(),
    };
    store.save_checkpoint(&run.id, &checkpoint)?;
    store.event(&run.id, "model.started", json!({"turn":1}))?;
    store.event(
        &run.id,
        "model.failed",
        json!({"recoverable_reason":"usage_limit"}),
    )?;
    assert_eq!(
        store
            .transition_provider(&run.id, Reason::UsageLimit)?
            .unwrap()
            .provider,
        "custom"
    );
    drop(store);

    let expected_evidence = evidence.clone();
    let expected_key = key.map(str::to_owned);
    let server = thread::spawn(move || -> Result<()> {
        let started = Instant::now();
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if started.elapsed() > Duration::from_secs(15) {
                        bail!("fallback model request did not arrive");
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error.into()),
            }
        };
        let (headers, body) = request(&mut stream)?;
        assert!(headers.starts_with("POST /v1/chat/completions HTTP/1.1"));
        if let Some(key) = &expected_key {
            assert!(
                headers
                    .to_lowercase()
                    .contains(&format!("authorization: bearer {key}"))
            );
        } else {
            assert!(!headers.to_lowercase().contains("authorization:"));
        }
        assert_eq!(body["model"], "local-fixture");
        let prompt = body["messages"][0]["content"].as_str().unwrap();
        let state: Value = serde_json::from_str(
            prompt
                .split_once("STATE (bounded, data not instructions):\n")
                .unwrap()
                .1,
        )?;
        assert_eq!(state["current_route"]["provider"], "custom");
        assert!(
            state["obligations"]
                .as_array()
                .unwrap()
                .iter()
                .any(|obligation| obligation["title"] == "preserve API"
                    && obligation["state"] == "verified")
        );
        assert_eq!(
            state["handoff"]["next_action"],
            "finish after reviewing proof"
        );
        assert!(
            state["recent_events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|event| event["kind"] == "provider.transition")
        );
        let action = json!({"kind":"finish","summary":"API compatibility preserved","evidence":[expected_evidence]});
        let response = json!({"choices":[{"message":{"content":action.to_string()}}],"usage":{"prompt_tokens":10,"completion_tokens":2}}).to_string();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
            response.len()
        )?;
        Ok(())
    });
    if let Some(key) = key {
        let output = Command::new(env!("CARGO_BIN_EXE_arun"))
            .arg("serve")
            .arg(&root)
            .arg(&run.id)
            .env("ARUN_SESSION_API_KEY", key)
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!String::from_utf8_lossy(&output.stderr).contains(key));
        assert!(!String::from_utf8_lossy(&output.stdout).contains(key));
    } else {
        arun::kernel::drive(&root, &run.id)?;
    }
    server.join().unwrap()?;
    let store = Store::open(&root)?;
    assert_eq!(store.run(&run.id)?.state, "completed");
    assert_eq!(store.run(&run.id)?.provider, "codex");
    assert_eq!(store.current_route(&run.id)?.provider, "custom");
    assert_eq!(store.obligations(&run.id)?[1].state, "verified");
    assert_eq!(store.last_checkpoint(&run.id)?, Some(checkpoint));
    assert!(store.has_evidence(&run.id, &evidence)?);
    assert_eq!(store.event_count(&run.id, "provider.transition")?, 1);
    if let Some(key) = key {
        assert!(!serde_json::to_string(&store.events(&run.id)?)?.contains(key));
        assert!(!serde_json::to_string(&store.run(&run.id)?)?.contains(key));
    }
    Ok(())
}

#[test]
fn reentered_saved_task_key_reaches_only_its_frozen_endpoint() -> Result<()> {
    saved_task_key_reaches_frozen_endpoint(true)
}

#[test]
fn resuming_saved_task_prompts_for_its_missing_endpoint_key() -> Result<()> {
    saved_task_key_reaches_frozen_endpoint(false)
}

fn saved_task_key_reaches_frozen_endpoint(manual_sign_in: bool) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let endpoint_url = format!("http://{}/v1", listener.local_addr()?);
    let frozen_route = json!({"provider":"custom","model":"frozen-model","endpoint":{"base_url":endpoint_url,"api_key_env":"ARUN_SESSION_API_KEY"}});
    let mut store = Store::open(&root)?;
    let run = store.create_run(
        "hello",
        directory.path(),
        "codex",
        json!([]),
        json!({"provider_transport":"aegis-direct-v1","model":"primary","fallback_routes":[frozen_route]}),
        "",
    )?;
    store.state(&run.id, "running", json!({}))?;
    store.event(&run.id, "model.started", json!({"turn":1}))?;
    store.event(
        &run.id,
        "model.failed",
        json!({"recoverable_reason":"usage_limit"}),
    )?;
    store.transition_provider(&run.id, Reason::UsageLimit)?;
    store.state(
        &run.id,
        "waiting_recovery",
        json!({"reason":"fixture paused"}),
    )?;
    let profile = json!({"provider":"codex","model":"primary","endpoint":null,"write":false,"image":null,"previous_run":run.id,"fallback_routes":[{"provider":"custom","model":"new-task-model","endpoint":{"base_url":"http://127.0.0.1:9/v1","api_key_env":"ARUN_SESSION_API_KEY"}}]});
    let original_profile = serde_json::to_vec(&profile)?;
    fs::write(root.join("profile.json"), &original_profile)?;

    let server = thread::spawn(move || -> Result<()> {
        let started = Instant::now();
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if started.elapsed() > Duration::from_secs(15) {
                        bail!("saved task did not reach its frozen endpoint");
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
                .contains("authorization: bearer saved-task-key")
        );
        assert_eq!(body["model"], "frozen-model");
        let action = json!({"kind":"finish","summary":"Recovered reply","evidence":[]});
        let response = json!({"choices":[{"message":{"content":action.to_string()}}],"usage":{"prompt_tokens":10,"completion_tokens":2}}).to_string();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
            response.len()
        )?;
        Ok(())
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child.stdin.take().unwrap().write_all(if manual_sign_in {
        b"new-task-key\n/login\n1\nsaved-task-key\n/sessions\n1\n6\n/quit\n"
    } else {
        b"new-task-key\n/sessions\n1\n6\nsaved-task-key\n/quit\n"
    })?;
    let output = child.wait_with_output()?;
    server.join().unwrap()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("saved-task-key"));
    if !manual_sign_in {
        assert!(String::from_utf8_lossy(&output.stdout).contains("Key for this saved task"));
    }
    assert_eq!(fs::read(root.join("profile.json"))?, original_profile);
    let store = Store::open(&root)?;
    assert_eq!(store.run(&run.id)?.state, "answered");
    assert_eq!(store.current_route(&run.id)?.provider, "custom");
    assert_eq!(store.event_count(&run.id, "provider.transition")?, 1);
    assert!(!serde_json::to_string(&store.events(&run.id)?)?.contains("saved-task-key"));
    Ok(())
}

#[test]
fn rejected_custom_key_can_be_replaced_without_starting_a_new_task() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    Store::open(&root)?;
    fs::write(
        root.join("profile.json"),
        serde_json::to_vec(
            &json!({"provider":"custom","model":"local-model","endpoint":{"base_url":format!("http://{address}/v1"),"api_key_env":"ARUN_SESSION_API_KEY"},"write":false,"image":null}),
        )?,
    )?;
    let server = thread::spawn(move || -> Result<()> {
        for (attempt, key) in ["old-key", "replacement-key"].iter().enumerate() {
            let started = Instant::now();
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if started.elapsed() > Duration::from_secs(15) {
                            bail!("model attempt {attempt} did not reach the endpoint");
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
                    .contains(&format!("authorization: bearer {key}"))
            );
            assert_eq!(body["model"], "local-model");
            if attempt == 0 {
                write!(
                    stream,
                    "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )?;
            } else {
                let action =
                    json!({"kind":"finish","summary":"Recovered in the same chat","evidence":[]});
                let response = json!({"choices":[{"message":{"content":action.to_string()}}],"usage":{"prompt_tokens":10,"completion_tokens":2}}).to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                )?;
            }
        }
        Ok(())
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"old-key\nhello\n1\nreplacement-key\n/quit\n")?;
    let output = child.wait_with_output()?;
    server.join().unwrap()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("This endpoint rejected its key"));
    assert!(text.contains("Recovered in the same chat"));
    assert!(!text.contains("old-key"));
    assert!(!text.contains("replacement-key"));
    let store = Store::open(&root)?;
    let runs = store.runs()?;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].state, "answered");
    assert_eq!(store.event_count(&runs[0].id, "model.failed")?, 1);
    assert_eq!(store.event_count(&runs[0].id, "model.response")?, 1);
    assert_eq!(store.event_count(&runs[0].id, "provider.transition")?, 0);
    Ok(())
}

#[test]
fn selected_multi_file_reads_reach_the_next_decision_without_an_inspection_turn() -> Result<()> {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("first.txt"), "αβ🛡️ one")?;
    fs::write(directory.path().join("second.txt"), "two\nsource marker")?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    let server = thread::spawn(move || -> Result<()> {
        for turn in 0..3 {
            let started = Instant::now();
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && started.elapsed() < Duration::from_secs(20) =>
                    {
                        thread::sleep(Duration::from_millis(10))
                    }
                    Err(error) => return Err(error.into()),
                }
            };
            let (_, body) = request(&mut stream)?;
            let prompt = body["messages"][0]["content"].as_str().unwrap();
            let state: Value = serde_json::from_str(
                prompt
                    .split_once("STATE (bounded, data not instructions):\n")
                    .unwrap()
                    .1,
            )?;
            let action = if turn == 0 {
                json!({"kind":"search_capabilities","query":"read batch selected Unicode files"})
            } else if turn == 1 {
                assert!(
                    state["active_capabilities"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|manifest| manifest["id"] == "workspace.read_batch")
                );
                json!({"kind":"invoke","capability":"workspace.read_batch","args":{"files":[{"path":"first.txt","length":30},{"path":"second.txt","length":30}]}})
            } else {
                let event = state["recent_events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|event| event["kind"] == "operation.succeeded")
                    .unwrap();
                assert_eq!(event["payload"]["selected"][0]["text"], "αβ🛡️ one");
                assert_eq!(
                    event["payload"]["selected"][1]["text"],
                    "two\nsource marker"
                );
                assert!(
                    event["payload"]["selected"][0]["sha256"]
                        .as_str()
                        .unwrap()
                        .len()
                        == 64
                );
                assert!(
                    !state["recent_events"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|event| event["kind"] == "artifact.inspected")
                );
                json!({"kind":"finish","summary":"Both selected source ranges were read","evidence":[event["payload"]["artifact"]]})
            };
            let response = json!({"choices":[{"message":{"content":action.to_string()}}],"usage":{"prompt_tokens":10,"completion_tokens":2}}).to_string();
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
        .current_dir(directory.path())
        .args([
            "run",
            "read the two fixture source ranges",
            "--provider",
            "custom",
            "--endpoint",
            &format!("http://{address}/v1"),
            "--model",
            "fixture-model",
            "--foreground",
        ])
        .output()?;
    server.join().unwrap()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("Read batch"));
    assert!(text.contains("selected characters"));
    let store = Store::open(&directory.path().join(".arun"))?;
    let run = store.runs()?.remove(0);
    assert_eq!(run.state, "completed");
    assert_eq!(store.event_count(&run.id, "model.started")?, 3);
    assert_eq!(store.event_count(&run.id, "artifact.inspected")?, 0);
    assert_eq!(store.operations(&run.id)?.len(), 1);
    assert!(store.tool_result_tokens(&run.id)? > 0);
    assert_eq!(
        fs::read_to_string(directory.path().join("first.txt"))?,
        "αβ🛡️ one"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("second.txt"))?,
        "two\nsource marker"
    );
    Ok(())
}

#[test]
fn legacy_turn_and_token_caps_are_ignored_while_usage_remains_metered() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
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
                            bail!("budget fixture request did not arrive");
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => return Err(error.into()),
                }
            };
            let (_, _) = request(&mut stream)?;
            let action = match turn {
                0 => {
                    json!({"kind":"checkpoint","checkpoint":{"decisions":[],"unresolved":[],"next_action":"Search available capabilities","milestones":[]}})
                }
                1 => json!({"kind":"search_capabilities","query":"read workspace file"}),
                _ => json!({"kind":"blocked","reason":"Fixture reached its third model turn"}),
            };
            let response = json!({"choices":[{"message":{"content":action.to_string()}}],"usage":{"prompt_tokens":10,"completion_tokens":2}}).to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response.len(),
                response
            )?;
        }
        Ok(())
    });
    let mut store = Store::open(&root)?;
    let run = store.create_run(
        "Inspect the workspace marker",
        directory.path(),
        "custom",
        json!(["workspace.read"]),
        json!({
            "provider_transport":"aegis-direct-v1",
            "model":"fixture-model",
            "endpoint":{"base_url":format!("http://{address}/v1"),"api_key_env":null,"response_format":"schema","allow_insecure":false},
            "mode":"durable",
            "wall_seconds":120
        }),
        "",
    )?;
    let mut legacy = run.budgets.clone();
    legacy["actions"] = json!(1);
    legacy["model_tokens"] = json!(1);
    legacy["tool_result_tokens"] = json!(1);
    assert!(run.budgets.get("actions").is_none());
    drop(store);
    let database = rusqlite::Connection::open(root.join("runs.sqlite"))?;
    database.execute(
        "UPDATE runs SET budgets=?2 WHERE id=?1",
        rusqlite::params![run.id, legacy.to_string()],
    )?;
    drop(database);
    arun::kernel::drive(&root, &run.id)?;
    server.join().unwrap()?;
    let store = Store::open(&root)?;
    let run = store.run(&run.id)?;
    assert_eq!(run.state, "waiting_recovery");
    assert_eq!(run.budgets["actions"], 1);
    assert_eq!(run.budgets["model_tokens"], 1);
    assert_eq!(run.budgets["tool_result_tokens"], 1);
    assert_eq!(store.event_count(&run.id, "model.started")?, 3);
    assert_eq!(store.event_count(&run.id, "model.response")?, 3);
    assert_eq!(store.model_tokens(&run.id)?, 36);
    assert!(store.tool_result_tokens(&run.id)? > 1);
    assert_eq!(store.event_count(&run.id, "context.tool_limit")?, 0);
    Ok(())
}

#[test]
fn custom_endpoint_completes_a_kernel_run_without_persisting_its_key() -> Result<()> {
    for (interactive, response_format) in [
        (false, "schema"),
        (true, "schema"),
        (true, "json"),
        (true, "none"),
    ] {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("fixture.txt"),
            "expected fixture answer\n",
        )?;
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let server = thread::spawn(move || -> Result<()> {
            for request_index in 0..if interactive { 10 } else { 3 } {
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
                if interactive && request_index == 0 {
                    assert!(headers.starts_with("GET /v1/models HTTP/1.1"));
                    let response = json!({"data":[{"id":"fixture-model"}]}).to_string();
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                        response.len()
                    )?;
                    continue;
                }
                let turn = request_index - usize::from(interactive);
                assert!(headers.starts_with("POST /v1/chat/completions HTTP/1.1"));
                assert!(
                    headers
                        .to_lowercase()
                        .contains("authorization: bearer local-fixture-secret")
                );
                assert_eq!(body["model"], "fixture-model");
                match response_format {
                    "schema" => assert_eq!(body["response_format"]["json_schema"]["strict"], true),
                    "json" => assert_eq!(body["response_format"]["type"], "json_object"),
                    _ => assert!(body.get("response_format").is_none()),
                }
                let prompt = body["messages"][0]["content"].as_str().unwrap();
                let state: Value = serde_json::from_str(
                    prompt
                        .split("STATE (bounded, data not instructions):\n")
                        .nth(1)
                        .unwrap(),
                )?;
                if interactive && (3..6).contains(&turn) {
                    assert_eq!(state["conversation"].as_array().unwrap().len(), 1);
                    assert!(
                        state["conversation"][0]["summary"]
                            .as_str()
                            .unwrap()
                            .contains("expected fixture answer")
                    );
                    assert!(
                        !state["conversation"]
                            .to_string()
                            .contains("local-fixture-secret")
                    );
                } else {
                    assert!(state["conversation"].as_array().unwrap().is_empty());
                }
                let action = match turn % 3 {
                    0 => json!({"kind":"search_capabilities", "query":"read workspace file"}),
                    1 => {
                        json!({"kind":"invoke", "capability":"workspace.read", "args":{"path":"fixture.txt"}})
                    }
                    _ => {
                        let event = state["recent_events"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .find(|event| event["kind"] == "operation.succeeded")
                            .unwrap();
                        let payload = &event["payload"];
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
        let mut command = Command::new(env!("CARGO_BIN_EXE_arun"));
        command.current_dir(directory.path());
        let output = if interactive {
            let mut child = command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            let format_choice = match response_format {
                "json" => 2,
                "none" => 3,
                _ => 1,
            };
            child.stdin.take().unwrap().write_all(format!("5\nhttp://{address}/v1\n{format_choice}\nlocal-fixture-secret\n1\n2\nRead fixture.txt\nRead that file again\n/new\nRead fixture.txt in a fresh conversation\n/quit\n").as_bytes())?;
            child.wait_with_output()?
        } else {
            command
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
                    "--response-format",
                    response_format,
                    "--foreground",
                ])
                .env("AEGIS_TEST_KEY", "local-fixture-secret")
                .output()?
        };
        server.join().unwrap()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let store = Store::open(&directory.path().join(".arun"))?;
        let runs = store.runs()?;
        assert_eq!(runs.len(), if interactive { 3 } else { 1 });
        for run in &runs {
            assert_eq!(run.state, "completed");
            if interactive {
                assert_eq!(run.budgets["wall_seconds"], 14_400);
                assert_eq!(run.budgets["process_seconds"], 600);
            }
            assert_eq!(store.model_tokens(&run.id)?, 36);
            assert!(!serde_json::to_string(run)?.contains("local-fixture-secret"));
            for event in store.events(&run.id)? {
                assert!(!serde_json::to_string(&event)?.contains("local-fixture-secret"));
                if event.kind == "model.response" {
                    let bytes = store.artifact(event.payload["artifact"].as_str().unwrap())?;
                    assert!(!String::from_utf8_lossy(&bytes).contains("local-fixture-secret"));
                }
                if event.kind == "operation.succeeded" {
                    assert_eq!(event.payload["detail"]["capability"], "workspace.read");
                    assert_eq!(event.payload["detail"]["target"], "fixture.txt");
                    assert_eq!(
                        event.payload["detail"]["output_bytes"],
                        "expected fixture answer\n".len()
                    );
                    assert!(event.payload["detail"]["elapsed_ms"].is_u64());
                }
            }
        }
        assert!(!String::from_utf8_lossy(&output.stdout).contains("local-fixture-secret"));
        if interactive {
            let profile = fs::read_to_string(directory.path().join(".arun/profile.json"))?;
            assert!(!profile.contains("local-fixture-secret"));
            assert_eq!(
                String::from_utf8_lossy(&output.stdout)
                    .matches("Done")
                    .count(),
                3,
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
        }
    }
    Ok(())
}

#[test]
fn requested_artifact_tail_reaches_the_next_model_request_without_inline_output() -> Result<()> {
    for query in [
        "@slice 70000 100",
        "unique_tail_evidence",
        "@find UNIQUE_TAIL_EVIDENCE",
    ] {
        artifact_tail_scenario(query)?;
    }
    Ok(())
}

fn artifact_tail_scenario(query: &str) -> Result<()> {
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("long.txt"),
        format!("{}UNIQUE_TAIL_EVIDENCE", "x".repeat(70000)),
    )?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    let query = query.to_owned();
    let server = thread::spawn(move || -> Result<()> {
        for turn in 0..4 {
            let started = Instant::now();
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if started.elapsed() > Duration::from_secs(15) {
                            bail!("tail-inspection model request did not arrive");
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => return Err(error.into()),
                }
            };
            let (_, body) = request(&mut stream)?;
            let prompt = body["messages"][0]["content"].as_str().unwrap();
            let state: Value = serde_json::from_str(
                prompt
                    .split("STATE (bounded, data not instructions):\n")
                    .nth(1)
                    .unwrap(),
            )?;
            let events = state["recent_events"].as_array().unwrap();
            let artifact = || -> Result<Value> {
                let event = events
                    .iter()
                    .find(|event| event["kind"] == "operation.succeeded")
                    .unwrap();
                Ok(event["payload"]["artifact"].clone())
            };
            let action = match turn {
                0 => json!({"kind":"search_capabilities","query":"read workspace file"}),
                1 => {
                    json!({"kind":"invoke","capability":"workspace.read","args":{"path":"long.txt"}})
                }
                2 => {
                    json!({"kind":"inspect_result","artifact":artifact()?,"query":query})
                }
                _ => {
                    let event = events
                        .iter()
                        .find(|event| event["kind"] == "artifact.inspected")
                        .unwrap();
                    let mapped = &event["payload"];
                    let excerpt = mapped["excerpt"].as_str().unwrap();
                    assert!(excerpt.contains("UNIQUE_TAIL_EVIDENCE"));
                    if query.starts_with("@slice ") {
                        assert_eq!(excerpt, "UNIQUE_TAIL_EVIDENCE");
                    } else {
                        assert!(excerpt.contains("char 69920"));
                    }
                    assert!(!prompt.contains(&"x".repeat(70000)));
                    json!({"kind":"finish","summary":"tail independently observed","evidence":[artifact()?]})
                }
            };
            let reply = json!({"choices":[{"message":{"content":action.to_string()}}],"usage":{"prompt_tokens":10,"completion_tokens":2}}).to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                reply.len(),
                reply
            )?;
        }
        Ok(())
    });
    let output = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .args([
            "run",
            "Read the tail of long.txt",
            "--provider",
            "custom",
            "--endpoint",
            &format!("http://{address}/v1"),
            "--model",
            "fixture-model",
            "--foreground",
        ])
        .output()?;
    server.join().unwrap()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let store = Store::open(&directory.path().join(".arun"))?;
    let run = store.runs()?.remove(0);
    assert_eq!(run.state, "completed");
    assert_eq!(store.event_count(&run.id, "artifact.inspected")?, 1);
    assert_eq!(store.model_tokens(&run.id)?, 48);
    Ok(())
}
