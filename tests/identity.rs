use anyhow::Result;
use arun::storage::Store;
use serde_json::json;
use std::process::Command;

#[test]
fn actual_binary_answers_identity_from_route_without_a_model_call() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let output = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .args([
            "run",
            "what model are u",
            "--provider",
            "custom",
            "--endpoint",
            "http://127.0.0.1:9/v1",
            "--model",
            "gpt-6-luna",
            "--reasoning",
            "low",
            "--foreground",
        ])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let store = Store::open(&directory.path().join(".arun"))?;
    let run = store.runs()?.remove(0);
    assert_eq!(run.state, "answered");
    assert_eq!(store.event_count(&run.id, "model.started")?, 0);
    let answer = store
        .events(&run.id)?
        .into_iter()
        .find(|event| event.kind == "run.answered")
        .unwrap();
    assert!(
        answer.payload["summary"]
            .as_str()
            .unwrap()
            .contains("gpt-6-luna")
    );
    assert!(
        answer.payload["summary"]
            .as_str()
            .unwrap()
            .contains("Reasoning: low")
    );
    assert_eq!(answer.payload["verified"], false);
    assert_eq!(store.model_tokens(&run.id)?, 0);
    Ok(())
}

#[test]
fn metadata_identity_uses_current_fallback() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let mut store = Store::open(&root)?;
    let run = store.create_run("what model are u",directory.path(),"codex",json!([]),json!({"provider_transport":"aegis-direct-v1","model":"primary","fallback_routes":[{"provider":"grok","model":"fallback","reasoning_effort":"low"}]}),"")?;
    store.state(&run.id, "running", json!({}))?;
    store.event(
        &run.id,
        "model.failed",
        json!({"recoverable_reason":"usage_limit"}),
    )?;
    store.transition_provider(&run.id, arun::routing::Reason::UsageLimit)?;
    store.state(&run.id, "ready", json!({}))?;
    arun::kernel::drive(&root, &run.id)?;
    let answer = store
        .events(&run.id)?
        .into_iter()
        .find(|event| event.kind == "run.answered")
        .unwrap();
    let summary = answer.payload["summary"].as_str().unwrap();
    assert!(
        summary.contains("fallback") && summary.contains("Grok") && !summary.contains("primary")
    );
    assert_eq!(store.event_count(&run.id, "model.started")?, 0);
    Ok(())
}
