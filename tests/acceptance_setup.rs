use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::Result;
use arun::storage::Store;
use serde_json::{Value, json};

#[test]
fn guided_setup_snapshots_the_approved_acceptance_check() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("check.json");
    let check = json!({"name":"Correct addition", "program":"node", "args":["-e","require('assert').equal(require('./math.cjs')(2,3),5)"], "image":"node:22-alpine", "seconds":15});
    fs::write(&path, serde_json::to_vec(&check)?)?;
    let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child.stdin.take().unwrap().write_all(
        format!(
            "5\nhttp://127.0.0.1:9/v1\n1\n\nfixture\n2\n/settings\n3\n2\n{}\n/quit\n",
            path.display()
        )
        .as_bytes(),
    )?;
    let output = child.wait_with_output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("read-only workspace, no network"));
    fs::write(path, b"{}")?;
    let profile: Value =
        serde_json::from_slice(&fs::read(directory.path().join(".arun/profile.json"))?)?;
    assert_eq!(profile["acceptance_check"], check);
    Ok(())
}

#[test]
fn advanced_cli_records_the_check_without_granting_a_model_verifier_tool() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let check = json!({"name":"Independent assertion", "program":"node", "args":["-e","process.exit(0)"], "image":"node:22-alpine", "seconds":5});
    fs::write(
        directory.path().join("check.json"),
        serde_json::to_vec(&check)?,
    )?;
    let output = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .env_remove("AEGIS_MISSING_ACCEPTANCE_TEST_KEY")
        .args([
            "run",
            "Check the fixture",
            "--provider",
            "custom",
            "--model",
            "fixture",
            "--endpoint",
            "http://127.0.0.1:9/v1",
            "--api-key-env",
            "AEGIS_MISSING_ACCEPTANCE_TEST_KEY",
            "--acceptance",
            "check.json",
            "--foreground",
        ])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let store = Store::open(&directory.path().join(".arun"))?;
    let runs = store.runs()?;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].budgets["acceptance_check"], check);
    assert_eq!(runs[0].acceptance, "Independent assertion");
    assert_eq!(runs[0].grants, json!(["workspace.read"]));
    assert_ne!(runs[0].state, "completed");
    Ok(())
}
