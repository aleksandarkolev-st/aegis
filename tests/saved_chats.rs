use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::Result;
use arun::storage::Store;
use serde_json::{Value, json};

#[test]
fn saved_chat_can_be_read_and_restored_without_restarting_tools() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let mut store = Store::open(&root)?;
    let first = store.create_run(
        "Original chat question",
        directory.path(),
        "codex",
        json!([]),
        json!({}),
        "",
    )?;
    store.state(&first.id, "running", json!({}))?;
    store.answer_run(&first.id, "Original chat reply")?;
    let followup = store.create_run(
        "Follow up question",
        directory.path(),
        "codex",
        json!([]),
        json!({"previous_run":first.id}),
        "",
    )?;
    store.state(&followup.id, "running", json!({}))?;
    store.answer_run(&followup.id, "Follow up reply")?;
    fs::write(
        root.join("profile.json"),
        serde_json::to_vec(
            &json!({"provider":"codex","model":null,"endpoint":null,"write":false,"image":null,"previous_run":first.id}),
        )?,
    )?;
    let original_events = store.events(&followup.id)?;
    for messages in [true, false] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
            .current_dir(directory.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child.stdin.take().unwrap().write_all(if messages {
            b"/sessions\n1\n2\n2\n3\n/quit\n"
        } else {
            b"/sessions\n1\n1\n/quit\n"
        })?;
        let output = child.wait_with_output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("Follow up reply"));
        if messages {
            assert!(text.contains("Original chat reply"));
        } else {
            let saved: Value = serde_json::from_slice(&fs::read(root.join("profile.json"))?)?;
            assert_eq!(saved["previous_run"], followup.id);
            assert!(text.contains("Chat restored"));
        }
        assert!(!text.contains("Welcome back"));
        assert!(!text.contains("Resume task"));
        assert!(!text.contains("Cancel task"));
        assert_eq!(
            serde_json::to_value(store.events(&followup.id)?)?,
            serde_json::to_value(&original_events)?
        );
        assert_eq!(store.runs()?.len(), 2);
    }
    Ok(())
}

#[test]
fn saved_chat_context_shows_recorded_token_breakdown_without_a_model_call() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let mut store = Store::open(&root)?;
    let run = store.create_run(
        "Explain the workspace",
        directory.path(),
        "codex",
        json!([]),
        json!({}),
        "",
    )?;
    store.state(&run.id, "running", json!({}))?;
    store.event(
        &run.id,
        "model.started",
        json!({"context_tokenizer":"o200k_base","schema_tokens":12,"tool_result_tokens":4,"raw_prompt_tokens":120}),
    )?;
    store.event(
        &run.id,
        "model.response",
        json!({"usage":{"input_tokens":90,"output_tokens":18,"cached_input_tokens":70,"source":"provider"}}),
    )?;
    store.answer_run(&run.id, "A recorded reply")?;
    fs::write(
        root.join("profile.json"),
        serde_json::to_vec(
            &json!({"provider":"codex","model":null,"endpoint":null,"write":false,"image":null,"previous_run":run.id}),
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
        .write_all(b"/context\n/quit\n")?;
    let output = child.wait_with_output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("90 input (70 reported cached) + 18 output tokens"));
    assert!(text.contains("calls 1 · estimated 0 · unaccounted 0"));
    assert!(text.contains("120 normalized units · 12 schema · 4 tool results"));
    assert!(text.contains("not provider billing"));
    assert_eq!(store.runs()?.len(), 1);
    assert_eq!(store.model_tokens(&run.id)?, 108);
    Ok(())
}

#[test]
fn saved_chat_context_shows_kernel_obligations_without_running_a_model() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let mut store = Store::open(&root)?;
    let run = store.create_run(
        "Repair parser\nRequirements:\n- nested expressions\n- public API compatibility",
        directory.path(),
        "codex",
        json!([]),
        json!({}),
        "",
    )?;
    fs::write(
        root.join("profile.json"),
        serde_json::to_vec(
            &json!({"provider":"codex","model":null,"endpoint":null,"write":false,"image":null,"previous_run":run.id}),
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
        .write_all(b"/context\n/quit\n")?;
    let output = child.wait_with_output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("2 user requirements"));
    assert!(text.contains("O1 · open"));
    assert!(text.contains("nested expressions"));
    assert!(text.contains("O2 · open"));
    assert!(text.contains("public API compatibility"));
    assert_eq!(store.runs()?.len(), 1);
    Ok(())
}
