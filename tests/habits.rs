use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::Result;
use arun::storage::Store;
use serde_json::json;

#[test]
fn habits_can_be_reviewed_corrected_paused_and_reset_without_a_model_call() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let mut store = Store::open(&root)?;
    store.confirm_habit(directory.path(), "package_manager", "pnpm")?;
    fs::write(
        root.join("profile.json"),
        serde_json::to_vec(
            &json!({"provider":"custom","model":"fixture","endpoint":{"base_url":"http://127.0.0.1:9/v1","api_key_env":null},"write":false,"image":null}),
        )?,
    )?;
    let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"/memory\n2\n5\n1\n3\n2\n/memory\n2\n2\n/quit\n")?;
    let output = child.wait_with_output()?;
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("Habit updated"));
    assert_eq!(store.habits(directory.path())?[0].choice, "npm");
    assert!(store.habits(directory.path())?[0].confirmed);
    assert!(!store.learning_enabled(directory.path())?);
    assert!(store.runs()?.is_empty());
    let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"/memory\n2\n4\n2\n/quit\n")?;
    assert!(child.wait_with_output()?.status.success());
    assert!(store.habits(directory.path())?.is_empty());
    assert!(store.runs()?.is_empty());
    Ok(())
}
