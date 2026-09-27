use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::Result;
use arun::auth_store::{Session, Vault};
use arun::storage::Store;
use serde_json::json;

fn command(directory: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_arun"));
    command
        .current_dir(directory)
        .env("HOME", directory.join("home"))
        .env("USERPROFILE", directory.join("home"))
        .env("CODEX_HOME", directory.join("home/.codex"))
        .env("PATH", directory)
        .env("AEGIS_PROVIDER_HOME", directory.join("managed"));
    command
}

fn trap(directory: &std::path::Path) -> Result<()> {
    for name in ["codex", "grok", "claude", "npm"] {
        let path = directory.join(if cfg!(windows) {
            format!("{name}.cmd")
        } else {
            name.into()
        });
        let script = if cfg!(windows) {
            "@echo NATIVE_PROVIDER_WAS_STARTED\r\n@echo bad > native-started.txt\r\n"
        } else {
            "#!/bin/sh\nprintf NATIVE_PROVIDER_WAS_STARTED\nprintf bad > native-started.txt\n"
        };
        fs::write(&path, script)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
        }
    }
    Ok(())
}

fn input(mut command: Command, text: &str) -> Result<std::process::Output> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child.stdin.take().unwrap().write_all(text.as_bytes())?;
    Ok(child.wait_with_output()?)
}

#[test]
fn login_back_never_launches_native_agents_or_creates_a_task() -> Result<()> {
    for provider in ["chatgpt", "codex", "grok"] {
        let directory = tempfile::tempdir()?;
        trap(directory.path())?;
        let mut command = command(directory.path());
        command.args(["login", provider]);
        let output = input(command, "3\n")?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("no provider CLI required"));
        assert!(!directory.path().join("native-started.txt").exists());
        assert!(!directory.path().join("home/.aegis").exists());
        assert!(
            Store::open(&directory.path().join(".arun"))?
                .runs()?
                .is_empty()
        );
    }
    Ok(())
}

#[test]
fn claude_pending_is_explicit_instead_of_a_native_cli_fallback() -> Result<()> {
    for provider in ["claude", "claude-code"] {
        let directory = tempfile::tempdir()?;
        trap(directory.path())?;
        let mut command = command(directory.path());
        command.args(["login", provider]);
        let output = command.output()?;
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("pending"));
        assert!(!directory.path().join("native-started.txt").exists());
    }
    Ok(())
}

#[test]
fn declining_setup_does_not_install_or_create_a_task() -> Result<()> {
    let directory = tempfile::tempdir()?;
    trap(directory.path())?;
    let output = input(command(directory.path()), "3\n3\n")?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!directory.path().join("managed").exists());
    assert!(!directory.path().join("native-started.txt").exists());
    assert!(
        Store::open(&directory.path().join(".arun"))?
            .runs()?
            .is_empty()
    );
    Ok(())
}

#[test]
fn sign_out_removes_only_owned_credentials_without_modifying_native_accounts() -> Result<()> {
    let directory = tempfile::tempdir()?;
    trap(directory.path())?;
    let vault = Vault::new(directory.path().join("home/.aegis/auth"));
    vault.save(&Session {
        provider: "grok".into(),
        access_token: "fixture-private-access".into(),
        refresh_token: Some("fixture-private-refresh".into()),
        account_id: None,
        expires_at: 2000000000,
    })?;
    fs::create_dir_all(directory.path().join("home/.grok"))?;
    let native = directory.path().join("home/.grok/auth.json");
    fs::write(&native, b"native credentials are not modified")?;
    let mut command = command(directory.path());
    command.args(["login", "grok"]);
    let output = input(command, "3\n")?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Signed out"));
    assert!(vault.load("grok")?.is_none());
    assert_eq!(fs::read(&native)?, b"native credentials are not modified");
    assert!(!directory.path().join("native-started.txt").exists());
    Ok(())
}

#[test]
fn normal_model_dispatch_fails_readably_without_launching_installed_clis() -> Result<()> {
    for provider in ["chatgpt", "grok", "claude"] {
        let directory = tempfile::tempdir()?;
        trap(directory.path())?;
        let mut command = command(directory.path());
        command.args([
            "run",
            "hello",
            "--provider",
            provider,
            "--model",
            "fixture-model",
            "--foreground",
        ]);
        let output = command.output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!directory.path().join("native-started.txt").exists());
        let store = Store::open(&directory.path().join(".arun"))?;
        let runs = store.runs()?;
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].budgets["provider_transport"], "aegis-direct-v1");
        assert_eq!(runs[0].state, "waiting_recovery");
        assert!(
            store
                .events(&runs[0].id)?
                .iter()
                .any(|event| event.kind == "model.failed")
        );
        assert_eq!(store.event_count(&runs[0].id, "operation.pending")?, 0);
    }
    Ok(())
}

#[test]
fn legacy_contracts_are_not_silently_reinterpreted_as_direct_provider_tasks() -> Result<()> {
    let directory = tempfile::tempdir()?;
    trap(directory.path())?;
    let root = directory.path().join(".arun");
    let mut store = Store::open(&root)?;
    let run = store.create_run(
        "old task",
        directory.path(),
        "codex",
        json!([]),
        json!({"model":"fixture-model", "mode":"durable", "actions":2, "wall_seconds":120}),
        "",
    )?;
    drop(store);
    let mut command = command(directory.path());
    command.args(["resume", &run.id, "--foreground"]);
    let output = command.output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let store = Store::open(&root)?;
    assert!(store.events(&run.id)?.iter().any(|event| {
        event.kind == "model.failed"
            && event.payload["error"]
                .as_str()
                .is_some_and(|message| message.contains("predates direct providers"))
    }));
    assert!(!directory.path().join("native-started.txt").exists());
    assert_eq!(store.run(&run.id)?.budgets, run.budgets);
    Ok(())
}
