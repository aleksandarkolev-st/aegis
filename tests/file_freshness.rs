use anyhow::Result;
use arun::storage::Store;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs;
#[path = "support/http.rs"]
mod http;

fn read_receipt(store: &mut Store, id: &str, source: &str) -> Result<String> {
    let op = store.begin_operation(id, "workspace.read", json!({"path":"source.txt"}), true)?;
    let hash = store.put_artifact(&serde_json::to_vec(
        &json!({"sha256":hex::encode(Sha256::digest(source.as_bytes())),"content":source}),
    )?)?;
    store.operation_state(&op, "succeeded", Some(&hash), json!({}))?;
    Ok(hash)
}

#[test]
fn external_edits_and_deletion_cannot_reuse_observed_file_proofs_after_restart() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    fs::write(directory.path().join("source.txt"), "before")?;
    let mut store = Store::open(&root)?;
    let run = store.create_run(
        "work",
        directory.path(),
        "custom",
        json!(["workspace.read"]),
        json!({"obligations":["Read source"]}),
        "",
    )?;
    store.state(&run.id, "running", json!({}))?;
    let old = read_receipt(&mut store, &run.id, "before")?;
    store.verify_obligation(&run.id, 1, &[old.clone()])?;
    fs::write(directory.path().join("source.txt"), "edited")?;
    let events = store.events(&run.id)?.len();
    let blockers = arun::control::view(&store, &run.id, "verify", None)?;
    assert!(
        blockers["blockers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item.as_str().unwrap().contains("source.txt"))
    );
    assert_eq!(store.events(&run.id)?.len(), events);
    drop(store);
    let mut store = Store::open(&root)?;
    assert!(store.complete_run(&run.id, "done", &[old.clone()]).is_err());
    assert_eq!(store.workspace_revision(&run.id)?, Some(1));
    assert_eq!(store.obligations(&run.id)?[1].state, "stale");
    assert!(store.verify_obligation(&run.id, 1, &[old]).is_err());
    assert!(!store.refresh_observed_files(&run.id)?);
    let current = read_receipt(&mut store, &run.id, "edited")?;
    store.verify_obligation(&run.id, 1, &[current.clone()])?;
    fs::remove_file(directory.path().join("source.txt"))?;
    assert!(
        store
            .verify_obligation(&run.id, 1, &[current.clone()])
            .is_err()
    );
    assert_eq!(store.workspace_revision(&run.id)?, Some(2));
    fs::write(directory.path().join("source.txt"), "final")?;
    let fresh = read_receipt(&mut store, &run.id, "final")?;
    assert_eq!(store.workspace_revision(&run.id)?, Some(3));
    store.verify_obligation(&run.id, 1, &[fresh.clone()])?;
    store.complete_run(&run.id, "read current source", &[fresh])?;
    assert_eq!(store.run(&run.id)?.state, "completed");
    Ok(())
}

#[test]
fn corrupt_proof_artifacts_and_stale_generic_finish_evidence_are_rejected() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let mut store = Store::open(&root)?;
    let run = store.create_run(
        "work",
        directory.path(),
        "custom",
        json!(["workspace.write"]),
        json!({}),
        "",
    )?;
    store.state(&run.id, "running", json!({}))?;
    let old_op = store.begin_operation(&run.id, "workspace.read", json!({}), true)?;
    let old = store.put_artifact(b"before edit")?;
    store.operation_state(&old_op, "succeeded", Some(&old), json!({}))?;
    let edit = store.begin_operation(&run.id, "workspace.write", json!({}), false)?;
    store.operation_state(&edit, "dispatched", None, json!({}))?;
    store.operation_state(&edit, "failed", None, json!({}))?;
    assert!(store.complete_run(&run.id, "done", &[old.clone()]).is_err());
    let fresh_op = store.begin_operation(&run.id, "workspace.read", json!({}), true)?;
    let fresh = store.put_artifact(b"after edit")?;
    store.operation_state(&fresh_op, "succeeded", Some(&fresh), json!({}))?;
    let id = store.add_obligation(&run.id, "Read source", "Approved coverage")?;
    store.verify_obligation(&run.id, id, &[fresh.clone()])?;
    fs::write(root.join("artifacts").join(&fresh), b"corrupted")?;
    assert!(
        store
            .verify_obligation(&run.id, id, &[fresh.clone()])
            .is_err()
    );
    assert!(
        store
            .complete_run(&run.id, "done", &[fresh.clone()])
            .is_err()
    );
    assert_eq!(store.run(&run.id)?.state, "running");
    Ok(())
}

#[test]
fn actual_file_read_and_external_edit_force_fresh_proof_before_binary_completion() -> Result<()> {
    use std::{
        process::Command,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let source = directory.path().join("source.txt");
    fs::write(&source, "before")?;
    let requests = Arc::new(AtomicUsize::new(0));
    let observed = requests.clone();
    let endpoint = http::Endpoint::start(move |body| {
        let state = http::state(body)?;
        let store = Store::open(&root)?;
        let run = store.runs()?.remove(0);
        let action = match observed.fetch_add(1, Ordering::SeqCst) {
            0 | 2 => {
                if store.event_count(&run.id, "model.response")? > 1 {
                    assert_eq!(state["workspace_revision"], 1);
                    assert!(store.event_count(&run.id, "action.rejected")? > 0);
                }
                json!({"kind":"invoke","capability":"workspace.read","args":{"path":"source.txt"}})
            }
            call @ (1 | 3) => {
                if call == 1 {
                    fs::write(&source, "after external edit")?;
                }
                let hash = store
                    .operations(&run.id)?
                    .last()
                    .unwrap()
                    .artifact
                    .clone()
                    .unwrap();
                json!({"kind":"finish","summary":"Source verified","evidence":[hash],"obligations":[{"id":1,"evidence":[hash]}]})
            }
            _ => anyhow::bail!("Unexpected extra inference"),
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
            "Read source\nRequirements:\n- read current source",
            "--provider",
            "custom",
            "--endpoint",
            &endpoint.url,
            "--model",
            "fixture",
            "--mode",
            "eager",
            "--foreground",
        ])
        .output()?;
    endpoint.finish()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(requests.load(Ordering::SeqCst), 4);
    let store = Store::open(&directory.path().join(".arun"))?;
    let run = store.runs()?.remove(0);
    assert_eq!(run.state, "completed");
    assert_eq!(store.obligations(&run.id)?[1].verified_revision, Some(1));
    assert_eq!(store.event_count(&run.id, "action.rejected")?, 1);
    Ok(())
}
