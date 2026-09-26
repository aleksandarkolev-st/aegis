use anyhow::Result;
use arun::{storage::Store, worker};
use serde_json::json;

#[test]
fn narrowed_files_are_enforced_before_claim_and_search_never_returns_other_files() -> Result<()> {
    let directory = tempfile::tempdir()?;
    std::fs::create_dir(directory.path().join("src"))?;
    std::fs::write(directory.path().join("src/allowed.txt"), "marker approved")?;
    std::fs::write(directory.path().join("secret.txt"), "marker secret")?;
    let root = directory.path().join(".arun");
    let mut store = Store::open(&root)?;
    let run = store.create_run(
        "read",
        directory.path(),
        "fixture",
        json!([
            "workspace.read",
            "workspace.write",
            "process.run",
            "process:node"
        ]),
        json!({"filesystem_scopes":{"read":["src/**"],"write":[]},"command_scopes":{"commands":[{"program":"node","args":["check.mjs"]}]}}),
        "",
    )?;
    store.state(&run.id, "running", json!({}))?;
    for (capability, arguments) in [
        ("workspace.read", json!({"path":"secret.txt"})),
        (
            "workspace.write",
            json!({"path":"src/allowed.txt","content":"modified"}),
        ),
        ("process.run", json!({"program":"node","args":[]})),
    ] {
        let operation = store.begin_operation(&run.id, capability, arguments, false)?;
        store.operation_state(&operation, "dispatched", None, json!({}))?;
        assert!(worker::execute(&root, &operation.id).is_err());
        assert_eq!(store.operation(&operation.id)?.state, "dispatched");
    }
    let search =
        store.begin_operation(&run.id, "workspace.search", json!({"query":"marker"}), true)?;
    store.operation_state(&search, "dispatched", None, json!({}))?;
    let result = worker::execute(&root, &search.id)?;
    assert_eq!(result["matches"].as_array().unwrap().len(), 1);
    assert!(!result.to_string().contains("secret"));
    assert_eq!(
        std::fs::read_to_string(directory.path().join("src/allowed.txt"))?,
        "marker approved"
    );
    assert_eq!(store.event_count(&run.id, "operation.executing")?, 1);
    Ok(())
}

#[test]
#[ignore = "requires Docker and the local node:22-alpine image"]
fn scoped_container_cannot_read_other_files_or_write_read_only_scopes() -> Result<()> {
    let directory = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR"))?;
    std::fs::create_dir(directory.path().join("src"))?;
    std::fs::create_dir(directory.path().join("src/.git"))?;
    std::fs::write(directory.path().join("src/.git/config"), "SECRET_METADATA")?;
    std::fs::write(directory.path().join("src/read.txt"), "approved")?;
    std::fs::write(directory.path().join("src/edit.txt"), "original")?;
    std::fs::write(directory.path().join("secret.txt"), "SECRET_OTHER_FILE")?;
    let root = directory.path().join(".arun");
    let mut store = Store::open(&root)?;
    let run = store.create_run("scoped container",directory.path(),"fixture",json!(["workspace.read","workspace.write","process.run","process:node"]),json!({"filesystem_scopes":{"read":["src/**"],"write":["src/edit.txt"]},"container_image":"node:22-alpine"}),"")?;
    store.state(&run.id, "running", json!({}))?;
    let script = "const fs=require('fs'),assert=require('assert/strict');assert.equal(fs.existsSync('secret.txt'),false);assert.equal(fs.existsSync('src/.git/config'),false);assert.equal(fs.readFileSync('src/read.txt','utf8'),'approved');assert.throws(()=>fs.writeFileSync('src/read.txt','forbidden'));fs.writeFileSync('src/edit.txt','updated');console.log('SCOPED_ACCESS_OK');";
    let operation = store.begin_operation(
        &run.id,
        "process.run",
        json!({"program":"node","args":["-e",script]}),
        false,
    )?;
    store.operation_state(&operation, "dispatched", None, json!({}))?;
    let result = worker::execute(&root, &operation.id)?;
    let output = store.artifact(result["output_artifact"].as_str().unwrap())?;
    assert_eq!(
        result["exit_code"],
        0,
        "{}",
        String::from_utf8_lossy(&output)
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("src/edit.txt"))?,
        "updated"
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("src/read.txt"))?,
        "approved"
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("secret.txt"))?,
        "SECRET_OTHER_FILE"
    );
    Ok(())
}
