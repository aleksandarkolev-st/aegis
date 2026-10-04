use anyhow::Result;
use arun::storage::Store;
use serde_json::json;
use std::{
    fs,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
#[path = "support/http.rs"]
mod http;

#[test]
fn pause_waits_for_current_action_then_restart_resumes_same_contract() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let fixture_root = root.clone();
    let requests = Arc::new(AtomicUsize::new(0));
    let observed = requests.clone();
    let endpoint = http::Endpoint::start(move |body| {
        let state = http::state(body)?;
        let mut store = Store::open(&fixture_root)?;
        let run = store.runs()?.remove(0);
        let action = match observed.fetch_add(1, Ordering::SeqCst) {
            0 => {
                store.request_pause(&run.id)?;
                json!({"kind":"invoke","capability":"workspace.write","args":{"path":"result.txt","content":"paused safely"}})
            }
            1 => {
                // Writes invalidate proof at claim and at committed completion.
                assert_eq!(state["workspace_revision"], 2);
                let obligation = state["obligations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|item| item["id"] == 1)
                    .unwrap();
                assert_eq!(obligation["title"], "write result");
                assert_eq!(obligation["state"], "open");
                assert!(state["handoff"].is_object());
                let hash = store.operations(&run.id)?[0].artifact.clone().unwrap();
                json!({"kind":"finish","summary":"Write verified","evidence":[hash],"obligations":[{"id":1,"evidence":[hash]}]})
            }
            _ => anyhow::bail!("Unexpected inference after pause"),
        };
        Ok((
            200,
            json!({"choices":[{"message":{"content":action.to_string()}}],"usage":{"prompt_tokens":10,"completion_tokens":2}}),
        ))
    })?;
    let output = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .args([
            "run",
            "Create result\nRequirements:\n- write result",
            "--provider",
            "custom",
            "--endpoint",
            &endpoint.url,
            "--model",
            "fixture",
            "--mode",
            "eager",
            "--allow-write",
            "--foreground",
        ])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let store = Store::open(&root)?;
    let original = store.runs()?.remove(0);
    assert_eq!(original.state, "paused");
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert_eq!(
        fs::read_to_string(directory.path().join("result.txt"))?,
        "paused safely"
    );
    assert_eq!(store.model_tokens(&original.id)?, 12);
    assert!(store.last_checkpoint(&original.id)?.is_some());
    assert_eq!(
        store.load_recovery(&original.id)?.unwrap().snapshot.state,
        "paused"
    );
    let stopped = Command::new(env!("CARGO_BIN_EXE_arun"))
        .args(["serve", root.to_str().unwrap(), &original.id])
        .output()?;
    assert!(stopped.status.success());
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    // Advance the simulated pause beyond the original wall deadline. The
    // resumed native runner must still contact the model under the same budget.
    let connection = rusqlite::Connection::open(root.join("runs.sqlite"))?;
    let paused_at = arun::storage::unix_time() - 7200;
    connection.execute(
        "UPDATE run_projection SET started_at=?2 WHERE run_id=?1",
        rusqlite::params![original.id, paused_at - 2],
    )?;
    connection.execute(
        "UPDATE pause_clock SET paused_at=?2 WHERE run_id=?1",
        rusqlite::params![original.id, paused_at],
    )?;
    drop(connection);
    let output = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .args(["resume", &original.id, "--foreground"])
        .output()?;
    endpoint.finish()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let store = Store::open(&root)?;
    let final_run = store.run(&original.id)?;
    assert_eq!(final_run.state, "completed");
    assert_eq!(final_run.task, original.task);
    assert_eq!(final_run.budgets, original.budgets);
    assert_eq!(final_run.grants, original.grants);
    assert_eq!(store.model_tokens(&original.id)?, 24);
    assert_eq!(store.workspace_revision(&original.id)?, Some(2));
    assert_eq!(store.operations(&original.id)?.len(), 1);
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    Ok(())
}

#[test]
fn stale_checkpoint_proof_does_not_prevent_a_safe_pause() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let mut store = Store::open(&root)?;
    let run = store.create_run(
        "repair",
        directory.path(),
        "custom",
        json!(["workspace.write"]),
        json!({}),
        "",
    )?;
    store.state(&run.id, "running", json!({}))?;
    let read = store.begin_operation(&run.id, "workspace.read", json!({}), true)?;
    store.operation_state(&read, "dispatched", None, json!({}))?;
    store.claim_operation(&read)?;
    let proof = store.put_artifact(b"old proof")?;
    store.operation_state(&read, "succeeded", Some(&proof), json!({}))?;
    let checkpoint = arun::model::Checkpoint {
        decisions: vec!["Keep the chosen design".into()],
        unresolved: vec![],
        next_action: "Recheck after edits".into(),
        milestones: vec![arun::model::Milestone {
            title: "Initial check".into(),
            state: "completed".into(),
            evidence: vec![proof.clone()],
        }],
    };
    let checkpoint_hash = store.save_checkpoint(&run.id, &checkpoint)?;
    let write = store.begin_operation(&run.id, "workspace.write", json!({}), false)?;
    store.operation_state(&write, "dispatched", None, json!({}))?;
    store.claim_operation(&write)?;
    let changed = store.put_artifact(b"changed")?;
    store.operation_state(&write, "succeeded", Some(&changed), json!({}))?;
    assert!(
        store
            .validate_completion(&run.id, &[proof.clone()])
            .is_err()
    );
    arun::pause::request(&root, &run.id)?;
    drop(store);
    let mut store = Store::open(&root)?;
    assert_eq!(store.run(&run.id)?.state, "paused");
    assert_eq!(
        store.last_checkpoint(&run.id)?.unwrap().decisions,
        checkpoint.decisions
    );
    assert_eq!(store.event_count(&run.id, "checkpoint.created")?, 1);
    assert!(store.artifact(&checkpoint_hash)?.len() > 0);
    assert_eq!(
        store.load_recovery(&run.id)?.unwrap().snapshot.state,
        "paused"
    );
    store.resume_paused(&run.id)?;
    store.state(&run.id, "running", json!({}))?;
    assert!(store.complete_run(&run.id, "done", &[proof]).is_err());
    Ok(())
}

#[test]
fn pending_pause_survives_history_archival_and_cannot_hide_unknown_outcomes() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let mut store = Store::open(&root)?;
    let run = store.create_run("work", directory.path(), "custom", json!([]), json!({}), "")?;
    store.state(&run.id, "running", json!({}))?;
    let op = store.begin_operation(&run.id, "workspace.write", json!({}), false)?;
    store.operation_state(&op, "dispatched", None, json!({}))?;
    store.operation_state(&op, "outcome_unknown", None, json!({}))?;
    arun::pause::request(&root, &run.id)?;
    for _ in 0..600 {
        store.event(&run.id, "telemetry", json!({}))?;
    }
    store.maintain_history(&run.id)?;
    drop(store);
    let mut store = Store::open(&root)?;
    assert!(store.pause_requested(&run.id)?);
    assert_ne!(store.run(&run.id)?.state, "paused");
    assert!(store.resume_paused(&run.id).is_err());
    assert!(store.event(&run.id, "model.started", json!({})).is_err());
    store.resolve_unknown(
        &run.id,
        &op.id,
        false,
        "Checked external state; nothing committed",
    )?;
    arun::pause::request(&root, &run.id)?;
    assert_eq!(store.run(&run.id)?.state, "paused");
    store.resume_paused(&run.id)?;
    assert_eq!(store.run(&run.id)?.state, "ready");
    assert!(!store.pause_requested(&run.id)?);
    Ok(())
}
