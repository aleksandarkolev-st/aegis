use anyhow::Result;
use arun::{
    model::{Checkpoint, Milestone},
    storage::Store,
};
use serde_json::json;

#[test]
fn guided_greeting_and_followup_use_one_request_each_without_file_edits() -> Result<()> {
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    fs::create_dir(&root)?;
    fs::write(
        directory.path().join("AGENTS.md"),
        "Keep greeting replies conversational; never manufacture evidence",
    )?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    listener.set_nonblocking(true)?;
    let server = std::thread::spawn(move || -> Result<()> {
        for turn in 0..2 {
            let started = Instant::now();
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && started.elapsed() < Duration::from_secs(20) =>
                    {
                        std::thread::sleep(Duration::from_millis(10))
                    }
                    Err(error) => return Err(error.into()),
                }
            };
            stream.set_nonblocking(false)?;
            stream.set_read_timeout(Some(Duration::from_secs(10)))?;
            let mut bytes = Vec::new();
            let body = loop {
                let mut buffer = [0; 4096];
                let count = stream.read(&mut buffer)?;
                anyhow::ensure!(count != 0, "request closed before its body");
                bytes.extend_from_slice(&buffer[..count]);
                if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                    let headers = std::str::from_utf8(&bytes[..end])?;
                    let length: usize = headers
                        .lines()
                        .find_map(|line| {
                            line.split_once(':')
                                .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                                .and_then(|(_, value)| value.trim().parse().ok())
                        })
                        .unwrap();
                    if bytes.len() >= end + 4 + length {
                        break serde_json::from_slice::<serde_json::Value>(
                            &bytes[end + 4..end + 4 + length],
                        )?;
                    }
                }
            };
            assert_eq!(body["reasoning_effort"], "low");
            let prompt = body["messages"][0]["content"].as_str().unwrap();
            assert!(prompt.contains("Never create files"));
            assert!(prompt.contains("REVIEWED REPOSITORY GUIDANCE"));
            assert!(prompt.contains("Keep greeting replies conversational"));
            if turn == 1 {
                assert!(prompt.contains("Hello from the saved chat"));
            }
            let action = json!({"kind":"finish","summary":if turn == 0 { "Hello from the saved chat" } else { "I remember our greeting" },"evidence":[]});
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
    fs::write(
        root.join("profile.json"),
        serde_json::to_vec(
            &json!({"provider":"custom","model":"fixture","reasoning_effort":"low","endpoint":{"base_url":format!("http://{address}/v1"),"api_key_env":null},"write":true,"image":null,"previous_run":null}),
        )?,
    )?;
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
        .write_all(b"hello\n2\nremember our greeting?\n/model-not-a-command\n/quit\n")?;
    let output = child.wait_with_output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().unwrap()?;
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("Hello from the saved chat"));
    assert!(text.contains("I remember our greeting"));
    assert!(text.contains("Unknown shortcut"));
    assert_eq!(text.matches("Trust repository guidance?").count(), 1);
    assert!(!text.contains("completion requires evidence"));
    assert!(!directory.path().join("hello.txt").exists());
    let store = Store::open(&root)?;
    let runs = store.runs()?;
    assert_eq!(runs.len(), 2);
    assert!(runs.iter().all(|run| run.state == "answered"));
    for run in &runs {
        assert!(store.operations(&run.id)?.is_empty());
        assert_eq!(store.event_count(&run.id, "model.started")?, 1);
        assert_eq!(run.budgets["repository_rules"][0]["path"], "AGENTS.md");
    }
    assert_eq!(store.model_tokens(&runs[0].id)?, 12);
    Ok(())
}

#[test]
fn conversational_replies_are_durable_but_not_verified_work() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut store = Store::open(directory.path())?;
    let run = store.create_run("hello", directory.path(), "codex", json!([]), json!({}), "")?;
    store.state(&run.id, "running", json!({}))?;
    store.save_snapshot(&run.id)?;
    store.answer_run(&run.id, "Hello! What would you like to build?")?;
    assert!(store.run(&run.id)?.is_terminal());
    assert_eq!(store.run(&run.id)?.state, "answered");
    assert_eq!(
        store.run_summary(&run.id)?.as_deref(),
        Some("Hello! What would you like to build?")
    );
    assert_eq!(store.event_count(&run.id, "run.completed")?, 0);
    assert!(store.operations(&run.id)?.is_empty());
    assert!(store.evidence_artifacts(&run.id)?.is_empty());
    assert!(store.state(&run.id, "ready", json!({})).is_err());
    drop(store);
    let store = Store::open(directory.path())?;
    assert!(store.load_recovery(&run.id)?.is_some());
    assert_eq!(store.run(&run.id)?.state, "answered");
    assert_eq!(
        store.run_summary(&run.id)?.as_deref(),
        Some("Hello! What would you like to build?")
    );
    Ok(())
}

#[test]
fn replies_cannot_bypass_operations_plans_or_acceptance() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut store = Store::open(directory.path())?;
    for kind in ["operation", "plan", "acceptance"] {
        let budgets = if kind == "acceptance" {
            json!({"acceptance_check":{"name":"verify", "program":"node", "args":["check.mjs"],"image":"node:22-alpine","seconds":10}})
        } else {
            json!({})
        };
        let run = store.create_run(kind, directory.path(), "codex", json!([]), budgets, "")?;
        store.state(&run.id, "running", json!({}))?;
        if kind == "operation" {
            store.begin_operation(
                &run.id,
                "workspace.write",
                json!({"path":"hello.txt","content":"hello"}),
                false,
            )?;
        }
        if kind == "plan" {
            store.save_checkpoint(
                &run.id,
                &Checkpoint {
                    decisions: vec![],
                    unresolved: vec![],
                    next_action: "repair".into(),
                    milestones: vec![Milestone {
                        title: "Repair code".into(),
                        state: "active".into(),
                        evidence: vec![],
                    }],
                },
            )?;
        }
        assert!(store.answer_run(&run.id, "Claimed done").is_err());
        assert_eq!(store.run(&run.id)?.state, "running");
        assert!(store.run_summary(&run.id)?.is_none());
    }
    Ok(())
}
