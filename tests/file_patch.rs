use std::fs;

use anyhow::Result;
use arun::{storage::Store, worker};
use serde_json::json;
use sha2::{Digest, Sha256};

#[test]
fn scoped_atomic_edits_preserve_unrelated_content_and_reject_stale_or_ambiguous_input() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let path = directory.path().join("source.txt");
    let original = format!(
        "{}\nunique βeta\nunchanged tail\n",
        "preamble ".repeat(10000)
    );
    fs::write(&path, &original)?;
    let mut store = Store::open(&root)?;
    let run = store.create_run(
        "edit source",
        directory.path(),
        "fixture",
        json!(["workspace.read", "workspace.write"]),
        json!({"filesystem_scopes":{"read":["source.txt"],"write":["source.txt"]}}),
        "",
    )?;
    store.state(&run.id, "running", json!({}))?;
    let read = store.begin_operation(
        &run.id,
        "workspace.read",
        json!({"path":"source.txt"}),
        true,
    )?;
    store.operation_state(&read, "dispatched", None, json!({}))?;
    let contents = worker::execute(&root, &read.id)?;
    let digest = hex::encode(Sha256::digest(original.as_bytes()));
    assert_eq!(contents["sha256"], digest);
    for (old, expected) in [
        ("missing", digest.as_str()),
        ("preamble", digest.as_str()),
        (
            "unique βeta",
            "0000000000000000000000000000000000000000000000000000000000000000",
        ),
    ] {
        let operation = store.begin_operation(&run.id,"workspace.patch",json!({"path":"source.txt","expected_sha256":expected,"edits":[{"old":old,"new":"changed"}]}),false)?;
        store.operation_state(&operation, "dispatched", None, json!({}))?;
        assert!(worker::execute(&root, &operation.id).is_err());
        assert_eq!(store.operation(&operation.id)?.state, "dispatched");
        assert_eq!(fs::read_to_string(&path)?, original);
    }
    fs::write(directory.path().join("secret.txt"), "secret")?;
    let denied = store.begin_operation(
        &run.id,
        "workspace.patch",
        json!({"path":"secret.txt","edits":[{"old":"secret","new":"changed"}]}),
        false,
    )?;
    store.operation_state(&denied, "dispatched", None, json!({}))?;
    assert!(worker::execute(&root, &denied.id).is_err());
    assert_eq!(store.operation(&denied.id)?.state, "dispatched");
    assert_eq!(
        fs::read_to_string(directory.path().join("secret.txt"))?,
        "secret"
    );
    let operation = store.begin_operation(&run.id,"workspace.patch",json!({"path":"source.txt","expected_sha256":digest,"edits":[{"old":"unique βeta","new":"corrected βeta"}]}),false)?;
    store.operation_state(&operation, "dispatched", None, json!({}))?;
    let result = worker::execute(&root, &operation.id)?;
    let updated = original.replace("unique βeta", "corrected βeta");
    assert_eq!(fs::read_to_string(&path)?, updated);
    assert_eq!(result["edits"], 1);
    assert_eq!(result["bytes"], updated.len());
    assert_eq!(
        result["sha256"],
        hex::encode(Sha256::digest(updated.as_bytes()))
    );
    assert!(worker::execute(&root, &operation.id).is_err());
    assert_eq!(fs::read_to_string(&path)?, updated);
    Ok(())
}
