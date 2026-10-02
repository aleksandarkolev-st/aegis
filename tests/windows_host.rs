#![cfg(windows)]

use anyhow::Result;
use arun::storage::Store;
use serde_json::json;
use std::{path::PathBuf, process::Command};

#[test]
fn actual_aegis_worker_executes_only_the_frozen_windows_host_grant() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/windows-host.mjs");
    let binary = env!("CARGO_BIN_EXE_arun");
    let registration = Command::new(binary)
        .args(["mcp", "add", "windows-host", "--trusted-host", "node"])
        .arg(&script)
        .arg("--trusted-host")
        .current_dir(directory.path())
        .output()?;
    assert!(
        registration.status.success(),
        "{}",
        String::from_utf8_lossy(&registration.stderr)
    );
    let mut store = Store::open(&root)?;
    let tools = store.mcp_tools()?;
    let tool = tools.iter().find(|tool| tool.name == "powershell").unwrap();
    let run = store.create_run(
        "Native host worker fixture",
        directory.path(),
        "custom",
        json!(["mcp:windows-host:powershell"]),
        json!({}),
        "",
    )?;
    store.state(&run.id, "running", json!({}))?;
    let operation = store.begin_operation_versioned(&run.id, "mcp.windows-host.powershell", tool.version,
        json!({"script":"[IO.File]::WriteAllText((Join-Path (Get-Location).Path 'worker-proof.txt'), 'trusted worker ✓', [Text.UTF8Encoding]::new($false)); [Console]::WriteLine('HOST_WORKER_OK')"}), false)?;
    store.operation_state(&operation, "dispatched", None, json!({}))?;
    let execution = Command::new(binary)
        .arg("worker")
        .arg(&root)
        .arg(&operation.id)
        .current_dir(directory.path())
        .output()?;
    assert!(
        execution.status.success(),
        "{}",
        String::from_utf8_lossy(&execution.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&execution.stdout)?;
    assert_eq!(result["isError"], false);
    assert!(result.to_string().contains("HOST_WORKER_OK"));
    assert_eq!(
        std::fs::read_to_string(directory.path().join("worker-proof.txt"))?,
        "trusted worker ✓"
    );

    let ungranted = store.create_run(
        "Frozen read-only tool fixture",
        directory.path(),
        "custom",
        json!(["mcp:windows-host:file_read"]),
        json!({}),
        "",
    )?;
    store.state(&ungranted.id, "running", json!({}))?;
    let denied = store.begin_operation_versioned(&ungranted.id, "mcp.windows-host.powershell", tool.version,
        json!({"script":"[IO.File]::WriteAllText((Join-Path (Get-Location).Path 'ungranted.txt'), 'forbidden')"}), false)?;
    store.operation_state(&denied, "dispatched", None, json!({}))?;
    let denial = Command::new(binary)
        .arg("worker")
        .arg(&root)
        .arg(&denied.id)
        .current_dir(directory.path())
        .output()?;
    assert!(!denial.status.success());
    assert!(String::from_utf8_lossy(&denial.stderr).contains("capability not granted"));
    assert!(!directory.path().join("ungranted.txt").exists());

    let long = store.create_run(
        "Configured native command deadline fixture",
        directory.path(),
        "custom",
        json!(["mcp:windows-host:powershell"]),
        json!({"wall_seconds":90,"process_seconds":45}),
        "",
    )?;
    store.state(&long.id, "running", json!({}))?;
    let slow = store.begin_operation_versioned(
        &long.id, "mcp.windows-host.powershell", tool.version,
        json!({"script":"Start-Sleep -Seconds 31; [Console]::WriteLine('LONG_HOST_COMMAND_OK')","timeout_seconds":40}), false,
    )?;
    store.operation_state(&slow, "dispatched", None, json!({}))?;
    let result = Command::new(binary)
        .arg("worker")
        .arg(&root)
        .arg(&slow.id)
        .current_dir(directory.path())
        .output()?;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stdout).contains("LONG_HOST_COMMAND_OK"));
    Ok(())
}
