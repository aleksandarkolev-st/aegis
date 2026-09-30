use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::Result;
use arun::storage::Store;
use serde_json::{Value, json};

fn run_menu(directory: &std::path::Path, input: &str) -> Result<std::process::Output> {
    let home = directory.join("home");
    let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("CODEX_HOME", home.join(".codex"))
        .env("PATH", directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child.stdin.take().unwrap().write_all(input.as_bytes())?;
    Ok(child.wait_with_output()?)
}

fn setup(directory: &std::path::Path) -> Result<(arun::storage::Run, Value)> {
    let root = directory.join(".arun");
    let mut store = Store::open(&root)?;
    store.remember(directory, "Keep the existing parser shape", None)?;
    let source = store.create_run(
        "Finish parser work",
        directory,
        "codex",
        json!(["workspace.read"]),
        json!({"model":"old-model","actions":11,"model_tokens":5000,"wall_seconds":120,
            "filesystem_scopes":{"read":["src/**"],"write":[]}}),
        "Verify parser behavior",
    )?;
    let profile = json!({"provider":"codex","model":"new-model","endpoint":null,
        "reasoning_effort":"high","write":true,"image":"different-profile-image",
        "previous_run":null,"limits":{"actions":99,"model_tokens":9000,"wall_seconds":600}});
    fs::write(root.join("profile.json"), serde_json::to_vec(&profile)?)?;
    let home = directory.join("home/.codex");
    fs::create_dir_all(&home)?;
    fs::write(
        home.join("models_cache.json"),
        serde_json::to_vec(&json!({"models":[{
            "slug":"new-model","display_name":"New Model","visibility":"list",
            "supported_reasoning_levels":[{"effort":"low"},{"effort":"high"}],
            "default_reasoning_level":"low"
        }]}))?,
    )?;
    let grok_home = directory.join("home/.grok");
    fs::create_dir_all(&grok_home)?;
    fs::write(
        grok_home.join("models_cache.json"),
        serde_json::to_vec(&json!({"models":{
            "grok-new":{"info":{"id":"grok-new","name":"New Grok Model","hidden":false}}
        }}))?,
    )?;
    for name in ["codex", "grok", "claude", "npm"] {
        let path = directory.join(if cfg!(windows) {
            format!("{name}.cmd")
        } else {
            name.into()
        });
        fs::write(
            &path,
            if cfg!(windows) {
                "@echo NATIVE_PROVIDER_WAS_STARTED\r\n@echo bad > native-started.txt\r\n"
            } else {
                "#!/bin/sh\nprintf NATIVE_PROVIDER_WAS_STARTED\nprintf bad > native-started.txt\n"
            },
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
        }
    }
    Ok((source, profile))
}

#[test]
fn declining_review_keeps_task_profile_history_and_permissions_unchanged() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (source, profile) = setup(directory.path())?;
    let root = directory.path().join(".arun");
    let original = serde_json::to_value(Store::open(&root)?.events(&source.id)?)?;
    let output = run_menu(directory.path(), "/sessions\n1\n6\n1\n1\n1\n2\n/quit\n")?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("Review continuation"), "{text}");
    assert!(text.contains("Usage and time restart"));
    assert!(!text.contains("Continuation saved"));
    let store = Store::open(&root)?;
    assert_eq!(store.runs()?.len(), 1);
    assert_eq!(serde_json::to_value(store.events(&source.id)?)?, original);
    assert_eq!(
        serde_json::to_value(store.run(&source.id)?)?,
        serde_json::to_value(source)?
    );
    let saved: Value = serde_json::from_slice(&fs::read(root.join("profile.json"))?)?;
    for (key, value) in profile.as_object().unwrap() {
        if key != "limits" {
            assert_eq!(&saved[key], value);
        }
    }
    assert_eq!(saved["limits"], profile["limits"]);
    assert!(!directory.path().join("native-started.txt").exists());
    Ok(())
}

#[test]
fn confirmed_continuation_is_saved_even_when_sign_in_is_cancelled() -> Result<()> {
    for (ended, provider_choice) in [(false, 1), (true, 1), (false, 2), (true, 2)] {
        let directory = tempfile::tempdir()?;
        let (source, _) = setup(directory.path())?;
        let root = directory.path().join(".arun");
        let mut store = Store::open(&root)?;
        if ended {
            store.state(&source.id, "cancelled", json!({"source":"user"}))?;
        }
        let original_run = serde_json::to_value(store.run(&source.id)?)?;
        let original_events = serde_json::to_value(store.events(&source.id)?)?;
        drop(store);
        let action = if ended { 5 } else { 6 };
        let reasoning = if provider_choice == 1 { "1\n" } else { "" };
        let output = run_menu(
            directory.path(),
            &format!("/sessions\n1\n{action}\n{provider_choice}\n1\n{reasoning}1\n3\n/quit\n"),
        )?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("Continuation saved"), "{text}");
        assert!(text.contains("no provider CLI required"));
        let store = Store::open(&root)?;
        let child = store
            .runs()?
            .into_iter()
            .find(|run| run.id != source.id)
            .unwrap();
        assert_eq!(store.runs()?.len(), 2);
        assert_eq!(serde_json::to_value(store.run(&source.id)?)?, original_run);
        assert_eq!(
            serde_json::to_value(store.events(&source.id)?)?,
            original_events
        );
        assert_eq!(child.state, "ready");
        assert_eq!(child.grants, source.grants);
        for removed in ["actions", "model_tokens", "tool_result_tokens"] {
            assert!(child.budgets.get(removed).is_none(), "{removed} persisted");
        }
        assert_eq!(
            child.budgets["filesystem_scopes"],
            source.budgets["filesystem_scopes"]
        );
        assert_eq!(
            child.budgets["project_memory"],
            source.budgets["project_memory"]
        );
        assert_eq!(child.budgets["provider_transport"], "aegis-direct-v1");
        assert_eq!(
            child.budgets["model"],
            if provider_choice == 1 {
                "new-model"
            } else {
                "grok-new"
            }
        );
        assert_eq!(
            child.provider,
            if provider_choice == 1 {
                "codex"
            } else {
                "grok"
            }
        );
        assert_eq!(child.budgets["previous_run"], source.id);
        assert!(store.operations(&child.id)?.is_empty());
        assert_eq!(store.event_count(&child.id, "model.started")?, 0);
        let saved: Value = serde_json::from_slice(&fs::read(root.join("profile.json"))?)?;
        assert_eq!(saved["previous_run"], child.id);
        assert!(!directory.path().join("native-started.txt").exists());
    }
    Ok(())
}

#[test]
fn unresolved_outcomes_do_not_open_sign_in_or_create_new_work() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (source, _) = setup(directory.path())?;
    let root = directory.path().join(".arun");
    let mut store = Store::open(&root)?;
    let operation = store.begin_operation(
        &source.id,
        "workspace.read",
        json!({"path":"src/parser.rs"}),
        true,
    )?;
    store.operation_state(&operation, "outcome_unknown", None, json!({}))?;
    let original = serde_json::to_value(store.events(&source.id)?)?;
    drop(store);
    let output = run_menu(directory.path(), "/sessions\n1\n6\n/quit\n")?;
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("uncertain outcomes still need review"));
    assert!(!text.contains("Connection for the new task"));
    assert!(!text.contains("Aegis sign-in"));
    let store = Store::open(&root)?;
    assert_eq!(store.runs()?.len(), 1);
    assert_eq!(serde_json::to_value(store.events(&source.id)?)?, original);
    assert!(!directory.path().join("native-started.txt").exists());
    Ok(())
}
