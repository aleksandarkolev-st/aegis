use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::Result;
use arun::storage::Store;
use serde_json::{Value, json};

fn launch(workspace: &std::path::Path, input: &[u8]) -> Result<String> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    child.stdin.take().unwrap().write_all(input)?;
    let output = child.wait_with_output()?;
    assert!(output.status.success());
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[test]
fn inline_file_scopes_preserve_other_settings_and_require_broadening_confirmation() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    fs::create_dir(&root)?;
    let profile_path = root.join("profile.json");
    fs::write(
        &profile_path,
        serde_json::to_vec(
            &json!({"provider":"custom","model":"fixture","endpoint":{"base_url":"http://127.0.0.1:9/v1","api_key_env":null},"write":true,"image":null,"command_scopes":{"commands":[]}}),
        )?,
    )?;
    let output = launch(
        directory.path(),
        b"/settings\n7\n2\nsrc/**\n\nsrc/edit.rs\n\n/quit\n",
    )?;
    assert!(output.contains("Settings saved"));
    let profile: Value = serde_json::from_slice(&fs::read(&profile_path)?)?;
    assert_eq!(
        profile["filesystem_scopes"],
        json!({"read":["src/**"],"write":["src/edit.rs"]})
    );
    assert_eq!(profile["command_scopes"], json!({"commands":[]}));
    assert_eq!(profile["model"], "fixture");
    assert_eq!(profile["write"], true);
    let mut store = Store::open(&root)?;
    assert!(store.runs()?.is_empty());
    let frozen = store.create_run(
        "frozen",
        directory.path(),
        "fixture",
        json!(["workspace.read"]),
        json!({"filesystem_scopes":profile["filesystem_scopes"]}),
        "",
    )?;
    launch(directory.path(), b"/settings\n7\n5\n1\n/quit\n")?;
    let kept: Value = serde_json::from_slice(&fs::read(&profile_path)?)?;
    assert_eq!(kept["filesystem_scopes"], profile["filesystem_scopes"]);
    launch(directory.path(), b"/settings\n7\n5\n2\n/quit\n")?;
    let broader: Value = serde_json::from_slice(&fs::read(&profile_path)?)?;
    assert!(broader["filesystem_scopes"].is_null());
    assert_eq!(
        store.run(&frozen.id)?.budgets["filesystem_scopes"],
        profile["filesystem_scopes"]
    );
    Ok(())
}
