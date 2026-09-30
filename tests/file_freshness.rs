use anyhow::Result;
use arun::storage::Store;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs;

fn read_receipt(store: &mut Store, id: &str, source: &str) -> Result<String> {
    let op = store.begin_operation(id, "workspace.read", json!({"path":"source.txt"}), true)?;
    let hash=store.put_artifact(&serde_json::to_vec(&json!({"path":"source.txt","sha256":hex::encode(Sha256::digest(source.as_bytes())),"content":source}))?)?;
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
