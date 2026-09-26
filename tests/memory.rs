use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::Result;
use arun::storage::Store;
use serde_json::json;

#[test]
fn plain_language_memory_is_durable_editable_and_uses_no_model_calls() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    fs::create_dir(&root)?;
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
    child.stdin.take().unwrap().write_all(b"Remember: use pnpm\nRemember: use pnpm\n/memory\n2\n2\nUse pnpm and preserve the lockfile\n/quit\n")?;
    let output = child.wait_with_output()?;
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("No model call needed"));
    let mut store = Store::open(&root)?;
    assert!(store.runs()?.is_empty());
    let notes = store.project_memory(directory.path())?;
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].text, "Use pnpm and preserve the lockfile");
    let run = store.create_run(
        "frozen notes",
        directory.path(),
        "codex",
        json!([]),
        json!({}),
        "",
    )?;
    drop(store);
    let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"/memory\n2\n3\n/quit\n")?;
    assert!(child.wait_with_output()?.status.success());
    let store = Store::open(&root)?;
    assert!(store.project_memory(directory.path())?.is_empty());
    assert_eq!(
        store.run(&run.id)?.budgets["project_memory"][0]["text"],
        notes[0].text
    );
    Ok(())
}

#[test]
fn memory_is_workspace_scoped_bounded_and_rejects_credentials() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let other = tempfile::tempdir()?;
    let mut store = Store::open(&directory.path().join(".arun"))?;
    let id = store.remember(directory.path(), "Use the existing formatter", None)?;
    assert!(store.project_memory(other.path())?.is_empty());
    assert!(
        store
            .remember(other.path(), "wrong workspace", Some(&id))
            .is_err()
    );
    assert!(store.forget(other.path(), &id).is_err());
    for note in [
        "",
        "bearer placeholder-secret",
        "ghp_placeholder",
        "sk-placeholder",
        "\u{1b}[2J",
        "\u{202e}spoof",
    ] {
        assert!(store.remember(directory.path(), note, None).is_err());
    }
    assert!(
        store
            .remember(directory.path(), &"é".repeat(257), None)
            .is_err()
    );
    store.forget(directory.path(), &id)?;
    for index in 0..8 {
        store.remember(
            directory.path(),
            &format!("{index}{}", "x".repeat(511)),
            None,
        )?;
    }
    assert!(
        store
            .remember(directory.path(), "over byte budget", None)
            .is_err()
    );
    for note in store.project_memory(directory.path())? {
        store.forget(directory.path(), &note.id)?;
    }
    for index in 0..16 {
        store.remember(directory.path(), &format!("note {index}"), None)?;
    }
    assert!(
        store
            .remember(directory.path(), "over note budget", None)
            .is_err()
    );
    Ok(())
}
