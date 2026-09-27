use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::Result;
use arun::{repository_rules, storage::Store};
use serde_json::json;

#[test]
fn reviewed_files_are_lossless_scoped_frozen_and_changed_versions_require_review() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let other = tempfile::tempdir()?;
    fs::create_dir(directory.path().join("src"))?;
    let text = format!(
        "# Rules\r\n\tPreserve interfaces\r\n{}\nMIDDLE_CONSTRAINT\n{}\n",
        "a".repeat(2500),
        "b".repeat(2500)
    );
    fs::write(directory.path().join("src/AGENTS.md"), &text)?;
    let root = directory.path().join(".arun");
    let mut store = Store::open(&root)?;
    let candidate = repository_rules::read(directory.path(), "src/AGENTS.md")?;
    assert_eq!(candidate.scope, "src/**");
    assert_eq!(candidate.text, text);
    store.review_repository_rule(directory.path(), &candidate, true)?;
    let run = store.create_run(
        "review",
        directory.path(),
        "codex",
        json!(["workspace.read"]),
        json!({"repository_rules":["caller injection"]}),
        "",
    )?;
    assert_eq!(run.budgets["repository_rules"][0]["text"], text);
    assert_eq!(run.grants, json!(["workspace.read"]));
    assert!(store.evidence_artifacts(&run.id)?.is_empty());
    store.save_snapshot(&run.id)?;
    fs::write(directory.path().join("src/AGENTS.md"), "New instructions")?;
    assert!(
        store
            .review_repository_rule(directory.path(), &candidate, true)
            .is_err()
    );
    assert!(
        store
            .create_run(
                "next",
                directory.path(),
                "codex",
                json!(["workspace.read"]),
                json!({}),
                ""
            )
            .is_err()
    );
    let replacement = repository_rules::read(directory.path(), "src/AGENTS.md")?;
    store.review_repository_rule(directory.path(), &replacement, true)?;
    let next = store.create_run(
        "next",
        directory.path(),
        "codex",
        json!(["workspace.read"]),
        json!({}),
        "",
    )?;
    assert_eq!(next.budgets["repository_rules"][0]["revision"], 2);
    assert!(store.repository_reviews(other.path())?.is_empty());
    assert!(
        store
            .remove_repository_review(other.path(), "src/AGENTS.md")
            .is_err()
    );
    drop(store);
    let store = Store::open(&root)?;
    store.load_recovery(&run.id)?;
    assert_eq!(
        store.run(&run.id)?.budgets["repository_rules"][0]["text"],
        text
    );
    Ok(())
}

#[test]
fn repository_rules_respect_read_scopes_ignore_decisions_and_exact_bounds() -> Result<()> {
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("AGENTS.md"),
        "Never create evidence-only files",
    )?;
    let mut store = Store::open(&directory.path().join(".arun"))?;
    let candidate = repository_rules::read(directory.path(), "AGENTS.md")?;
    store.review_repository_rule(directory.path(), &candidate, false)?;
    let ignored = store.create_run(
        "review",
        directory.path(),
        "codex",
        json!(["workspace.read"]),
        json!({}),
        "",
    )?;
    assert_eq!(ignored.budgets["repository_rules"], json!([]));
    store.review_repository_rule(directory.path(), &candidate, true)?;
    for (grants, budgets) in [
        (json!([]), json!({})),
        (
            json!(["workspace.read"]),
            json!({"filesystem_scopes":{"read":["src/**"],"write":[]}}),
        ),
    ] {
        let narrowed =
            store.create_run("review", directory.path(), "codex", grants, budgets, "")?;
        assert_eq!(narrowed.budgets["repository_rules"], json!([]));
    }
    for text in [
        "".into(),
        "x".repeat(8193),
        "é".repeat(4097),
        "\u{202e}spoof".into(),
        "\u{1b}[2J".into(),
        "ghp_placeholder".into(),
    ] {
        fs::write(directory.path().join("AGENTS.md"), text)?;
        assert!(repository_rules::read(directory.path(), "AGENTS.md").is_err());
    }
    for path in [
        "../AGENTS.md",
        ".arun/AGENTS.md",
        "README.md",
        "src\\AGENTS.md",
    ] {
        assert!(repository_rules::read(directory.path(), path).is_err());
    }
    store.remove_repository_review(directory.path(), "AGENTS.md")?;
    for index in 0..8 {
        let folder = format!("folder{index}");
        fs::create_dir(directory.path().join(&folder))?;
        fs::write(directory.path().join(format!("{folder}/AGENTS.md")), "safe")?;
        let candidate = repository_rules::read(directory.path(), &format!("{folder}/AGENTS.md"))?;
        store.review_repository_rule(directory.path(), &candidate, true)?;
    }
    fs::write(directory.path().join("CLAUDE.md"), "ninth")?;
    let ninth = repository_rules::read(directory.path(), "CLAUDE.md")?;
    assert!(
        store
            .review_repository_rule(directory.path(), &ninth, true)
            .is_err()
    );
    for rule in store.repository_reviews(directory.path())? {
        store.remove_repository_review(directory.path(), &rule.path)?;
    }
    for folder in ["first", "second"] {
        fs::create_dir(directory.path().join(folder))?;
        fs::write(
            directory.path().join(format!("{folder}/AGENTS.md")),
            "x".repeat(8192),
        )?;
        let candidate = repository_rules::read(directory.path(), &format!("{folder}/AGENTS.md"))?;
        let result = store.review_repository_rule(directory.path(), &candidate, true);
        assert_eq!(result.is_ok(), folder == "first");
    }
    Ok(())
}

#[test]
fn guided_repository_review_and_removal_need_no_json_or_model_calls() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    fs::create_dir(&root)?;
    fs::write(
        directory.path().join("AGENTS.md"),
        "Preserve the public API\nAsk about conflicting constraints",
    )?;
    fs::write(
        root.join("profile.json"),
        serde_json::to_vec(
            &json!({"provider":"custom","model":"fixture","endpoint":{"base_url":"http://127.0.0.1:9/v1","api_key_env":null},"write":false,"image":null}),
        )?,
    )?;
    let original = fs::read(root.join("profile.json"))?;
    for (input, count, expected) in [
        ("/instructions\n2\n1\n2\n/quit\n", 1, "Guidance approved"),
        ("/instructions\n2\n1\n2\n/quit\n", 0, "Review removed"),
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
        assert!(String::from_utf8_lossy(&output.stdout).contains(expected));
        let store = Store::open(&root)?;
        assert!(store.runs()?.is_empty());
        assert_eq!(store.repository_reviews(directory.path())?.len(), count);
        assert_eq!(fs::read(root.join("profile.json"))?, original);
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn repository_guidance_rejects_symlink_sources_and_directories() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    fs::write(outside.path().join("AGENTS.md"), "outside")?;
    std::os::unix::fs::symlink(
        outside.path().join("AGENTS.md"),
        directory.path().join("AGENTS.md"),
    )?;
    std::os::unix::fs::symlink(outside.path(), directory.path().join("nested"))?;
    assert!(repository_rules::read(directory.path(), "AGENTS.md").is_err());
    assert!(repository_rules::read(directory.path(), "nested/AGENTS.md").is_err());
    fs::create_dir(directory.path().join("CLAUDE.md"))?;
    assert!(repository_rules::read(directory.path(), "CLAUDE.md").is_err());
    Ok(())
}
