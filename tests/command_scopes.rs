use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::Result;
use arun::storage::Store;
use serde_json::{Value, json};

#[test]
fn inline_scope_wizard_preserves_other_settings_and_requires_broadening_confirmation() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    fs::create_dir(&root)?;
    let original = json!({"provider":"custom","model":"fixture","endpoint":{"base_url":"http://127.0.0.1:9/v1","api_key_env":null,"response_format":"schema","allow_insecure":false},"write":true,"image":"approved-image","previous_run":null});
    fs::write(root.join("profile.json"), serde_json::to_vec(&original)?)?;
    let input = b"/settings\n4\n2\ncargo\ntest\n--offline\n\n2\n/settings\n4\n5\n1\n/quit\n";
    let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child.stdin.take().unwrap().write_all(input)?;
    let output = child.wait_with_output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("1 exact commands approved"));
    assert!(text.contains("Confirm broader command access"));
    let saved: Value = serde_json::from_slice(&fs::read(root.join("profile.json"))?)?;
    for key in [
        "provider",
        "model",
        "endpoint",
        "write",
        "image",
        "previous_run",
    ] {
        assert_eq!(saved[key], original[key]);
    }
    assert_eq!(
        saved["command_scopes"],
        json!({"commands":[{"program":"cargo","args":["test","--offline"]}]})
    );
    assert!(Store::open(&root)?.runs()?.is_empty());
    Ok(())
}

#[test]
fn advanced_scope_file_is_frozen_and_grants_only_its_programs() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let policy_path = directory.path().join("commands.json");
    let scopes = json!({"commands":[{"program":"cargo","args":["test","--offline"]}]});
    fs::write(&policy_path, serde_json::to_vec(&scopes)?)?;
    let output = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .args([
            "run",
            "fixture task",
            "--provider",
            "custom",
            "--model",
            "fixture",
            "--endpoint",
            "http://127.0.0.1:9/v1",
            "--command-scopes",
            "commands.json",
            "--image",
            "approved-image",
            "--wall-seconds",
            "1",
            "--foreground",
        ])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::write(&policy_path, r#"{"commands":[]}"#)?;
    let store = Store::open(&directory.path().join(".arun"))?;
    let runs = store.runs()?;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].budgets["command_scopes"], scopes);
    assert_eq!(
        runs[0].grants,
        json!(["workspace.read", "process.run", "process:cargo"])
    );
    assert!(store.operations(&runs[0].id)?.is_empty());
    let output = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .args([
            "run",
            "invalid",
            "--command-scopes",
            "commands.json",
            "--allow-process",
            "cargo",
        ])
        .output()?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("requires --image"));
    assert_eq!(store.runs()?.len(), 1);
    Ok(())
}
