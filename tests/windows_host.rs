#![cfg(windows)]

use anyhow::Result;
use arun::storage::Store;
use serde_json::json;
use std::{path::PathBuf, process::Command};

#[path = "support/http.rs"]
mod http;

fn native_runtime() -> (String, PathBuf) {
    let binary = std::env::var("AEGIS_WINDOWS_HOST_BINARY")
        .unwrap_or_else(|_| env!("CARGO_BIN_EXE_arun").into());
    let script = std::env::var_os("AEGIS_WINDOWS_HOST_SCRIPT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/windows-host.mjs")
        });
    (binary, script)
}

#[test]
fn durable_native_command_text_reaches_the_next_decision_without_inspection() -> Result<()> {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let directory = tempfile::tempdir()?;
    let (binary, script) = native_runtime();
    let registered = Command::new(&binary)
        .current_dir(directory.path())
        .args(["mcp", "add", "windows-host", "--trusted-host", "node"])
        .arg(script)
        .arg("--trusted-host")
        .output()?;
    assert!(
        registered.status.success(),
        "{}",
        String::from_utf8_lossy(&registered.stderr)
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let endpoint = http::Endpoint::start(move |body| {
        let state = http::state(body)?;
        let action = match observed.fetch_add(1, Ordering::SeqCst) {
            0 => json!({"kind":"search_capabilities","query":"mcp.windows-host.powershell"}),
            1 => {
                json!({"kind":"invoke","capability":"mcp.windows-host.powershell","args":{"script":"[Console]::WriteLine('COMMITTED_CONTEXT_RESULT')"}})
            }
            2 => {
                let payload = &state["recent_events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|e| e["kind"] == "operation.succeeded")
                    .unwrap()["payload"];
                assert_eq!(payload["command"]["exit_code"], 0);
                assert_eq!(payload["command"]["timed_out"], false);
                assert!(
                    payload["command"]["stdout"]
                        .as_str()
                        .unwrap()
                        .contains("COMMITTED_CONTEXT_RESULT")
                );
                assert_eq!(payload["text_complete"], true);
                assert_eq!(payload["isError"], false);
                json!({"kind":"finish","summary":"Native result used without inspection","evidence":[payload["artifact"]]})
            }
            _ => anyhow::bail!("unexpected result inspection or extra model request"),
        };
        Ok((
            200,
            json!({"choices":[{"message":{"content":action.to_string()}}],"usage":{"prompt_tokens":10,"completion_tokens":2}}),
        ))
    })?;
    let output = Command::new(&binary)
        .current_dir(directory.path())
        .args([
            "run",
            "Inspect native command output",
            "--provider",
            "custom",
            "--model",
            "fixture",
            "--endpoint",
            &endpoint.url,
            "--mode",
            "durable",
            "--allow-mcp",
            "windows-host:powershell",
            "--foreground",
        ])
        .output()?;
    endpoint.finish()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let store = Store::open(&directory.path().join(".arun"))?;
    let run = store.runs()?.remove(0);
    assert_eq!(run.state, "completed");
    assert_eq!(store.operations(&run.id)?.len(), 1);
    assert_eq!(store.event_count(&run.id, "artifact.inspected")?, 0);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(store.model_tokens(&run.id)?, 36);
    Ok(())
}

#[test]
fn native_powershell_output_is_saved_and_displayed_before_completion() -> Result<()> {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::{Duration, Instant};
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let (binary, script) = native_runtime();
    let registered = Command::new(&binary)
        .args(["mcp", "add", "windows-host", "--trusted-host", "node"])
        .arg(script)
        .arg("--trusted-host")
        .current_dir(directory.path())
        .output()?;
    assert!(
        registered.status.success(),
        "{}",
        String::from_utf8_lossy(&registered.stderr)
    );
    let requests = Arc::new(AtomicUsize::new(0));
    let observed = requests.clone();
    let endpoint = http::Endpoint::start(move |body| {
        let state = http::state(body)?;
        let action = match observed.fetch_add(1, Ordering::SeqCst) {
            0 => json!({"kind":"invoke","capability":"mcp.windows-host.powershell","args":{
                "script":"[Console]::WriteLine(('NATIVE_' + 'LIVE_Київ_🦀')); $end = [DateTime]::UtcNow.AddSeconds(12); while (!(Test-Path '.arun/release.txt')) { if ([DateTime]::UtcNow -gt $end) { throw 'Fixture release deadline' }; Start-Sleep -Milliseconds 25 }; [Console]::WriteLine('RELEASED')",
                "timeout_seconds":15}}),
            1 => {
                assert_eq!(state["workspace_revision"], 0);
                assert_eq!(state["recent_operation_outcomes"][0]["successful_current_evidence"], true);
                json!({"kind":"finish","summary":"Native output verified","evidence":[state["recent_operation_outcomes"][0]["artifact"]]})
            }
            _ => anyhow::bail!("unexpected additional model request"),
        };
        Ok((
            200,
            json!({"choices":[{"message":{"content":action.to_string()}}],"usage":{"prompt_tokens":10,"completion_tokens":2}}),
        ))
    })?;
    let display = root.join("foreground.log");
    let file = std::fs::File::create(&display)?;
    let mut command = Command::new(&binary);
    command
        .current_dir(directory.path())
        .args([
            "run",
            "Show native output while execution continues",
            "--provider",
            "custom",
            "--endpoint",
            &endpoint.url,
            "--model",
            "fixture",
            "--mode",
            "eager",
            "--allow-write",
            "--allow-mcp",
            "windows-host:powershell",
            "--foreground",
        ])
        .stdout(file.try_clone()?)
        .stderr(file);
    let mut child = arun::process::spawn(command)?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let marker = "NATIVE_LIVE_Київ_🦀";
    loop {
        if let Ok(store) = Store::open(&root) {
            if let Some(run) = store.runs()?.first() {
                let saved = store.events(&run.id)?.iter().any(|event| {
                    event.kind == "operation.output"
                        && event.payload["text"]
                            .as_str()
                            .is_some_and(|text| text.contains(marker))
                });
                let shown = std::fs::read_to_string(&display)?.contains(marker);
                if saved && shown {
                    assert!(
                        child.try_wait()?.is_none(),
                        "command exited before live output was observed"
                    );
                    let operations = store.operations(&run.id)?;
                    assert_eq!(operations.len(), 1);
                    assert!(
                        operations[0].artifact.is_none(),
                        "live preview must precede completed evidence"
                    );
                    break;
                }
            }
        }
        anyhow::ensure!(
            Instant::now() < deadline && child.try_wait()?.is_none(),
            "native live output missing: {}",
            std::fs::read_to_string(&display)?
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    std::fs::write(root.join("release.txt"), "release")?;
    while child.try_wait()?.is_none() {
        anyhow::ensure!(
            Instant::now() < deadline + Duration::from_secs(10),
            "foreground failed to finish"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        child.wait()?.success(),
        "{}",
        std::fs::read_to_string(&display)?
    );
    endpoint.finish()?;
    let store = Store::open(&root)?;
    let run = store.runs()?.remove(0);
    assert_eq!(run.state, "completed");
    assert_eq!(store.operations(&run.id)?.len(), 1);
    assert_eq!(store.model_tokens(&run.id)?, 24);
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    Ok(())
}

#[test]
fn actual_aegis_worker_executes_only_the_frozen_windows_host_grant() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let (binary, script) = native_runtime();
    let registration = Command::new(&binary)
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
    let execution = Command::new(&binary)
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
    let denial = Command::new(&binary)
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
    let result = Command::new(&binary)
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
