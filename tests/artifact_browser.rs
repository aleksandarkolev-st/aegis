use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::Result;
use arun::storage::Store;
use serde_json::json;
#[path = "support/operation.rs"]
mod operation_fixture;

#[test]
fn guided_artifact_ranges_and_literal_search_need_no_commands_or_model_calls() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let mut store = Store::open(&root)?;
    let run = store.create_run(
        "saved evidence fixture",
        directory.path(),
        "custom",
        json!([]),
        json!({}),
        "",
    )?;
    let original = format!(
        "{}\nLINE_TWO_UNIQUE\n@slice literal needle\n",
        "a".repeat(5000)
    );
    let hash = store.put_artifact(original.as_bytes())?;
    let operation = store.begin_operation(&run.id, "fixture.read", json!({}), true)?;
    operation_fixture::claim_fixture_operation(&mut store, &operation.id)?;
    store.operation_state(&operation, "succeeded", Some(&hash), json!({}))?;
    store.state(&run.id, "completed", json!({}))?;
    fs::write(
        root.join("profile.json"),
        serde_json::to_vec(&json!({
            "provider":"custom", "model":"never-called",
            "endpoint":{"base_url":"http://127.0.0.1:9/v1", "api_key_env":null},
            "write":false, "image":null, "previous_run":run.id
        }))?,
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
        .write_all(b"/artifacts\n1\n3\nbad\n2\n0\n1\n4\n5001\n15\n2\n@slice literal\n5\n/quit\n")?;
    let output = child.wait_with_output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Read lines") && stdout.contains("Read characters"));
    assert!(stdout.contains("Choose a number"));
    assert!(stdout.matches("LINE_TWO_UNIQUE").count() >= 2);
    assert!(stdout.contains("@slice literal needle"));
    assert!(!stdout.contains(&"a".repeat(4000)));
    assert_eq!(store.artifact(&hash)?, original.as_bytes());
    assert_eq!(store.operations(&run.id)?.len(), 1);
    assert_eq!(store.event_count(&run.id, "model.started")?, 0);
    assert_eq!(store.event_count(&run.id, "artifact.inspected")?, 0);
    Ok(())
}
