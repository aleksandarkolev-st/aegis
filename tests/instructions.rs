use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::Result;
use arun::storage::Store;
use serde_json::json;

#[test]
fn instructions_are_scoped_revisioned_bounded_and_frozen_without_granting_access() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let other = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let mut store = Store::open(&root)?;
    let id = store.pin_instruction(
        directory.path(),
        "src/**",
        "Preserve public interfaces",
        None,
    )?;
    assert_eq!(
        store.pin_instruction(
            directory.path(),
            "src/**",
            "Preserve public interfaces",
            None
        )?,
        id
    );
    assert!(store.project_instructions(other.path())?.is_empty());
    assert!(
        store
            .pin_instruction(other.path(), "**", "wrong workspace", Some(&id))
            .is_err()
    );
    assert!(store.unpin_instruction(other.path(), &id).is_err());
    let run = store.create_run(
        "review",
        directory.path(),
        "codex",
        json!([]),
        json!({}),
        "",
    )?;
    store.save_snapshot(&run.id)?;
    store.pin_instruction(
        directory.path(),
        "src/api.rs",
        "Review interface changes",
        Some(&id),
    )?;
    assert_eq!(store.project_instructions(directory.path())?[0].revision, 2);
    store.unpin_instruction(directory.path(), &id)?;
    drop(store);
    let mut store = Store::open(&root)?;
    store.load_recovery(&run.id)?;
    let frozen = store.run(&run.id)?;
    assert_eq!(
        frozen.budgets["project_instructions"][0]["text"],
        "Preserve public interfaces"
    );
    assert_eq!(frozen.budgets["project_instructions"][0]["revision"], 1);
    assert_eq!(frozen.grants, json!([]));
    assert!(store.evidence_artifacts(&run.id)?.is_empty());
    assert!(store.project_instructions(directory.path())?.is_empty());
    for scope in [
        "",
        "../outside/**",
        "/absolute",
        ".arun/**",
        "src\\**",
        "*.rs",
    ] {
        assert!(
            store
                .pin_instruction(directory.path(), scope, "safe", None)
                .is_err()
        );
    }
    for text in [
        "",
        "ghp_placeholder",
        "github_pat_placeholder",
        "Bearer placeholder",
        "sk-placeholder",
        "\u{202e}spoof",
        "\u{1b}[2J",
    ] {
        assert!(
            store
                .pin_instruction(directory.path(), "**", text, None)
                .is_err()
        );
    }
    assert!(
        store
            .pin_instruction(directory.path(), "**", &"é".repeat(257), None)
            .is_err()
    );
    for index in 0..8 {
        store.pin_instruction(directory.path(), "**", &format!("rule {index}"), None)?;
    }
    assert!(
        store
            .pin_instruction(directory.path(), "**", "ninth", None)
            .is_err()
    );
    for rule in store.project_instructions(directory.path())? {
        store.unpin_instruction(directory.path(), &rule.id)?;
    }
    for index in 0..5 {
        store.pin_instruction(
            directory.path(),
            "**",
            &format!("{index}{}", "x".repeat(511)),
            None,
        )?;
    }
    assert!(
        store
            .pin_instruction(directory.path(), "**", &"y".repeat(512), None)
            .is_err()
    );
    assert_eq!(store.project_instructions(directory.path())?.len(), 5);
    Ok(())
}

#[test]
fn guided_instructions_can_be_added_edited_and_removed_without_model_calls() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    fs::create_dir(&root)?;
    fs::write(
        root.join("profile.json"),
        serde_json::to_vec(
            &json!({"provider":"custom","model":"fixture","endpoint":{"base_url":"http://127.0.0.1:9/v1","api_key_env":null},"write":false,"image":null}),
        )?,
    )?;
    let original = fs::read(root.join("profile.json"))?;
    for (input, text, revision) in [
        (
            "/settings\n9\n1\nPreserve interfaces\n2\nsrc/**\n/quit\n",
            "Preserve interfaces",
            1,
        ),
        (
            "/instructions\n2\n2\nReview interfaces carefully\n1\n/quit\n",
            "Review interfaces carefully",
            2,
        ),
    ] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
            .current_dir(directory.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child.stdin.take().unwrap().write_all(input.as_bytes())?;
        let output = child.wait_with_output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("Instruction pinned"));
        let store = Store::open(&root)?;
        assert!(store.runs()?.is_empty());
        let rules = store.project_instructions(directory.path())?;
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].text, text);
        assert_eq!(rules[0].revision, revision);
        assert_eq!(fs::read(root.join("profile.json"))?, original);
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"/instructions\n2\n3\n/quit\n")?;
    assert!(child.wait_with_output()?.status.success());
    assert!(
        Store::open(&root)?
            .project_instructions(directory.path())?
            .is_empty()
    );
    Ok(())
}
