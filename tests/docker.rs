use anyhow::Result;
use arun::{storage::Store, worker};
use serde_json::json;

#[test]
#[ignore = "requires a running Docker daemon and the local node:22-alpine image"]
fn process_output_is_virtualized_and_state_is_masked() -> Result<()> {
    let directory = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR"))?;
    let root = directory.path().join(".arun");
    let mut store = Store::open(&root)?;
    std::fs::create_dir(directory.path().join(".git"))?;
    std::fs::write(directory.path().join(".git/config"), "HOST_GIT_SECRET")?;
    std::fs::create_dir_all(directory.path().join(".aegis/auth"))?;
    std::fs::write(
        directory.path().join(".aegis/auth/grok.session"),
        "HOST_AEGIS_SECRET",
    )?;
    let script = "const fs=require('fs');const assert=require('assert');assert.equal(fs.existsSync('/workspace/.aegis/auth/grok.session'),false);fs.writeFileSync('/workspace/.aegis/overlay','only ephemeral overlay');assert.equal(fs.existsSync('/workspace/.git/config'),false);fs.writeFileSync('/workspace/.git/config','only ephemeral overlay');console.log(fs.existsSync('/workspace/.arun/runs.sqlite'));console.log(JSON.stringify({operation:process.env.ARUN_OPERATION_ID,key:process.env.ARUN_IDEMPOTENCY_KEY}));process.stdout.write('X'.repeat(2000000))";
    let run = store.create_run(
        "test",
        directory.path(),
        "codex",
        json!([
            "workspace.read",
            "workspace.write",
            "process.run",
            "process:node"
        ]),
        json!({"container_image":"node:22-alpine","command_scopes":{"commands":[{"program":"node","args":["-e",script]}]}}),
        "",
    )?;
    store.state(&run.id, "running", json!({}))?;
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
    let receipt: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&bytes).lines().nth(1).unwrap())?;
    assert_eq!(
        receipt,
        json!({"operation":operation.id,"key":operation.idempotency_key})
    );
    assert!(bytes.len() > 2_000_000);
    // The receipt includes a bounded 1,200-character preview; the complete
    // two-megabyte output must remain only in the referenced artifact.
    assert!(result["preview"].as_str().unwrap().chars().count() <= 1200);
    assert!(result.to_string().len() < 2000);
    assert_eq!(
        std::fs::read_to_string(directory.path().join(".git/config"))?,
        "HOST_GIT_SECRET"
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join(".aegis/auth/grok.session"))?,
        "HOST_AEGIS_SECRET"
    );
    assert!(!directory.path().join(".aegis/overlay").exists());
    let summary = store.put_artifact(&serde_json::to_vec(&result)?)?;
    store.operation_state(&operation, "succeeded", Some(&summary), json!({}))?;
    assert!(store.has_evidence(&run.id, hash)?);
    Ok(())
}

#[test]
#[ignore = "requires Docker and the local node:22-alpine image"]
fn read_only_commands_work_without_metadata_directories_in_the_workspace() -> Result<()> {
    let directory = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR"))?;
    let workspace = directory.path().join("fresh");
    std::fs::create_dir(&workspace)?;
    let root = directory.path().join("state");
    let mut store = Store::open(&root)?;
    let run = store.create_run(
        "fresh workspace",
        &workspace,
        "verifier",
        json!(["process.run", "process:node"]),
        json!({"container_image":"node:22-alpine"}),
        "",
    )?;
    store.state(&run.id, "running", json!({}))?;
    let operation = store.begin_operation(&run.id, "process.run",
        json!({"program":"node","args":["-e","const fs=require('fs');const assert=require('assert');assert.equal(fs.existsSync('/workspace/.arun'),false);assert.equal(fs.existsSync('/workspace/.git'),false);assert.throws(()=>fs.writeFileSync('/workspace/unauthorized','bad'));console.log('FRESH_READ_ONLY_OK')"]}), false)?;
    store.operation_state(&operation, "dispatched", None, json!({}))?;
    let result = worker::execute(&root, &operation.id)?;
    let output = store.artifact(result["output_artifact"].as_str().unwrap())?;
    assert_eq!(
        result["exit_code"],
        0,
        "{}",
        String::from_utf8_lossy(&output)
    );
    assert!(String::from_utf8_lossy(&output).contains("FRESH_READ_ONLY_OK"));
    assert!(!workspace.join(".arun").exists());
    assert!(!workspace.join(".git").exists());
    assert!(!workspace.join("unauthorized").exists());
    Ok(())
}

#[test]
#[ignore = "requires Docker and the local aegis-linux-functional:local image"]
fn read_only_cargo_tests_execute_from_the_scoped_temporary_target() -> Result<()> {
    let directory = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR"))?;
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src"))?;
    std::fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"aegis-container-check\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )?;
    std::fs::write(
        workspace.join("Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"aegis-container-check\"\nversion = \"0.1.0\"\n",
    )?;
    std::fs::write(
        workspace.join("src/lib.rs"),
        "#[test] fn passes() { assert_eq!(2 + 2, 4); }\n",
    )?;
    let root = directory.path().join("state");
    let mut store = Store::open(&root)?;
    let run = store.create_run(
        "compile and execute isolated tests",
        &workspace,
        "fixture",
        json!(["process.run", "process:cargo"]),
        json!({"container_image":"aegis-linux-functional:local","process_seconds":45}),
        "",
    )?;
    store.state(&run.id, "running", json!({}))?;
    let operation = store.begin_operation(
        &run.id,
        "process.run",
        json!({"program":"cargo","args":["test","--locked","--offline"]}),
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
    assert!(!workspace.join("target").exists());
    Ok(())
}
