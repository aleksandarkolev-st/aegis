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

#[test]
fn saved_task_replaces_a_requirement_only_after_terminal_confirmation() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let mut store = Store::open(&root)?;
    let run = store.create_run(
        "Refactor parser\nRequirements:\n- preserve public API",
        directory.path(),
        "codex",
        json!([]),
        json!({}),
        "",
    )?;
    store.state(&run.id, "paused", json!({}))?;
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
    child.stdin.take().unwrap().write_all(
        b"/sessions\n1\n4\n6\n1\nAllow a v2 API\nUser approved breaking compatibility\n2\n/context\n/quit\n",
    )?;
    let output = child.wait_with_output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("Approve this change?"));
    assert!(text.contains("Requirement updated"));
    assert!(text.contains("1 user requirement"));
    let obligations = store.obligations(&run.id)?;
    assert_eq!(obligations[1].state, "superseded");
    assert_eq!(obligations[1].superseded_by, Some(2));
    assert_eq!(obligations[2].title, "Allow a v2 API");
    assert_eq!(store.event_count(&run.id, "model.started")?, 0);
    Ok(())
}

#[test]
fn switched_provider_is_visible_in_saved_task_without_changing_run_identity() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let mut store = Store::open(&root)?;
    let run = store.create_run(
        "Review parser",
        directory.path(),
        "codex",
        json!([]),
        json!({"provider_transport":"aegis-direct-v1","model":"primary","fallback_routes":[{"provider":"grok","model":"fallback"}]}),
        "",
    )?;
    store.state(&run.id, "running", json!({}))?;
    store.event(&run.id, "model.started", json!({"turn":1}))?;
    store.event(
        &run.id,
        "model.failed",
        json!({"recoverable_reason":"usage_limit"}),
    )?;
    store.transition_provider(&run.id, arun::routing::Reason::UsageLimit)?;
    store.state(
        &run.id,
        "waiting_recovery",
        json!({"reason":"fixture paused"}),
    )?;
    fs::write(
        root.join("profile.json"),
        serde_json::to_vec(
            &json!({"provider":"codex","model":null,"endpoint":null,"write":false,"image":null,"previous_run":run.id}),
        )?,
    )?;
    let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .env("USERPROFILE", directory.path())
        .env("HOME", directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"/context\n/login\n1\n3\n/quit\n")?;
    let output = child.wait_with_output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("Grok / fallback · switched within this task")
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Current task · Grok / fallback"));
    assert!(String::from_utf8_lossy(&output.stdout).contains("New tasks · ChatGPT"));
    assert_eq!(store.run(&run.id)?.provider, "codex");
    assert_eq!(store.event_count(&run.id, "provider.transition")?, 1);
    Ok(())
}

#[test]
fn following_a_paused_switched_task_offers_its_active_provider_sign_in() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let mut store = Store::open(&root)?;
    let run = store.create_run(
        "Review parser",
        directory.path(),
        "codex",
        json!([]),
        json!({"provider_transport":"aegis-direct-v1","model":"primary","fallback_routes":[{"provider":"grok","model":"fallback"}]}),
        "",
    )?;
    store.state(&run.id, "running", json!({}))?;
    store.event(&run.id, "model.started", json!({"turn":1}))?;
    store.event(
        &run.id,
        "model.failed",
        json!({"recoverable_reason":"usage_limit"}),
    )?;
    store.transition_provider(&run.id, arun::routing::Reason::UsageLimit)?;
    store.event(&run.id, "model.started", json!({"turn":2}))?;
    store.event(&run.id, "model.failed", json!({"error":"HTTP 401"}))?;
    store.state(
        &run.id,
        "waiting_recovery",
        json!({"reason":"model unavailable"}),
    )?;
    fs::write(
        root.join("profile.json"),
        serde_json::to_vec(
            &json!({"provider":"codex","model":"primary","endpoint":null,"write":false,"image":null,"previous_run":run.id}),
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
        .write_all(b"/sessions\n1\n5\n2\n/quit\n")?;
    let output = child.wait_with_output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("Sign in to Grok and continue this task"));
    assert!(!text.contains("Sign in to ChatGPT and continue this task"));
    assert_eq!(store.run(&run.id)?.state, "waiting_recovery");
    assert_eq!(store.event_count(&run.id, "model.started")?, 2);
    Ok(())
}

#[test]
fn switched_custom_route_asks_for_its_key_without_rewriting_the_primary_profile() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let route = json!({"provider":"custom","model":"local-model","endpoint":{"base_url":"http://127.0.0.1:1234/v1","api_key_env":"ARUN_SESSION_API_KEY"}});
    let mut store = Store::open(&root)?;
    let run = store.create_run(
        "Review parser",
        directory.path(),
        "codex",
        json!([]),
        json!({"provider_transport":"aegis-direct-v1","model":"primary","fallback_routes":[route]}),
        "",
    )?;
    store.state(&run.id, "running", json!({}))?;
    store.event(&run.id, "model.started", json!({"turn":1}))?;
    store.event(
        &run.id,
        "model.failed",
        json!({"recoverable_reason":"usage_limit"}),
    )?;
    store.transition_provider(&run.id, arun::routing::Reason::UsageLimit)?;
    store.state(
        &run.id,
        "waiting_recovery",
        json!({"reason":"fixture paused"}),
    )?;
    let profile = json!({"provider":"codex","model":"primary","endpoint":null,"write":false,"image":null,"previous_run":run.id,"fallback_routes":[route]});
    let profile_bytes = serde_json::to_vec(&profile)?;
    fs::write(root.join("profile.json"), &profile_bytes)?;
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
        .write_all(b"initial-fixture-key\n/login\n1\nreplacement-fixture-key\n/quit\n")?;
    let output = child.wait_with_output()?;
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("Current task · Custom endpoint / local-model"));
    assert!(text.contains("New tasks · ChatGPT"));
    assert!(!text.contains("initial-fixture-key"));
    assert!(!text.contains("replacement-fixture-key"));
    assert_eq!(fs::read(root.join("profile.json"))?, profile_bytes);
    assert_eq!(store.current_route(&run.id)?.provider, "custom");
    Ok(())
}

#[test]
fn current_custom_task_with_a_different_endpoint_has_its_own_sign_in_choice() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let mut store = Store::open(&root)?;
    let run = store.create_run(
        "Review parser",
        directory.path(),
        "custom",
        json!([]),
        json!({"model":"saved-model","endpoint":{"base_url":"http://127.0.0.1:1234/v1","api_key_env":"FROZEN_KEY"}}),
        "",
    )?;
    store.state(
        &run.id,
        "waiting_recovery",
        json!({"reason":"fixture paused"}),
    )?;
    let profile = json!({"provider":"custom","model":"new-model","endpoint":{"base_url":"http://127.0.0.1:5678/v1","api_key_env":"ARUN_SESSION_API_KEY"},"write":false,"image":null,"previous_run":run.id});
    let original_profile = serde_json::to_vec(&profile)?;
    fs::write(root.join("profile.json"), &original_profile)?;
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
        .write_all(b"new-task-key\n/login\n1\nsaved-task-key\n/quit\n")?;
    let output = child.wait_with_output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("Current task · Custom endpoint / saved-model"));
    assert!(text.contains("New tasks · Custom endpoint"));
    assert!(!text.contains("saved-task-key"));
    assert_eq!(fs::read(root.join("profile.json"))?, original_profile);
    Ok(())
}

#[test]
fn saved_custom_task_does_not_resume_when_its_key_is_declined() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let mut store = Store::open(&root)?;
    let run = store.create_run(
        "Review parser",
        directory.path(),
        "custom",
        json!([]),
        json!({"model":"saved-model","endpoint":{"base_url":"http://127.0.0.1:1234/v1","api_key_env":"FROZEN_KEY"}}),
        "",
    )?;
    store.state(
        &run.id,
        "waiting_recovery",
        json!({"reason":"fixture paused"}),
    )?;
    fs::write(
        root.join("profile.json"),
        serde_json::to_vec(
            &json!({"provider":"custom","model":"new-model","endpoint":{"base_url":"http://127.0.0.1:5678/v1","api_key_env":"ARUN_SESSION_API_KEY"},"write":false,"image":null,"previous_run":run.id}),
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
        .write_all(b"new-task-key\n/sessions\n1\n6\n\n/quit\n")?;
    let output = child.wait_with_output()?;
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("Key needed"));
    assert_eq!(store.run(&run.id)?.state, "waiting_recovery");
    assert_eq!(store.event_count(&run.id, "model.started")?, 0);
    Ok(())
}
