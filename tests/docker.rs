use anyhow::Result;
use arun::{storage::Store, worker};
use serde_json::json;

#[test]
#[ignore = "requires a running Docker daemon and the local node:22-alpine image"]
fn process_output_is_virtualized_and_state_is_masked() -> Result<()> {
    let directory = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR"))?;
    let root = directory.path().join(".arun");
    let mut store = Store::open(&root)?;
    let run = store.create_run(
        "test",
        directory.path(),
        "codex",
        json!(["workspace.read", "process.run", "process:node"]),
        json!({"container_image":"node:22-alpine"}),
        "",
    )?;
    store.state(&run.id, "running", json!({}))?;
    let script = "const fs=require('fs');console.log(fs.existsSync('/workspace/.arun/runs.sqlite'));process.stdout.write('X'.repeat(2000000))";
    let operation = store.begin_operation(
        &run.id,
        "process.run",
        json!({"program":"node","args":["-e",script]}),
        false,
    )?;
    store.operation_state(&operation, "dispatched", None, json!({}))?;
    let result = worker::execute(&root, &operation.id)?;
    let hash = result["output_artifact"].as_str().unwrap();
    let bytes = store.artifact(hash)?;
    assert_eq!(
        result["exit_code"],
        0,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    assert!(bytes.starts_with(b"false\n"));
    assert!(bytes.len() > 2_000_000);
    assert!(result.to_string().len() < 1000);
    let summary = store.put_artifact(&serde_json::to_vec(&result)?)?;
    store.operation_state(&operation, "succeeded", Some(&summary), json!({}))?;
    assert!(store.has_evidence(&run.id, hash)?);
    Ok(())
}
