#![cfg(windows)]

use anyhow::Result;
use arun::storage::Store;
use serde_json::json;
use std::{
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

#[path = "support/http.rs"]
mod http;

#[test]
fn native_in_flight_write_requires_reconciliation_after_runner_death() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let binary = std::env::var("AEGIS_WINDOWS_HOST_BINARY")
        .unwrap_or_else(|_| env!("CARGO_BIN_EXE_arun").into());
    let script = std::env::var_os("AEGIS_WINDOWS_HOST_SCRIPT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/windows-host.mjs")
        });
    let registration = Command::new(&binary)
        .args(["mcp", "add", "windows-host", "--trusted-host", "node"])
        .arg(script)
        .arg("--trusted-host")
        .current_dir(directory.path())
        .output()?;
    assert!(registration.status.success());
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let endpoint = http::Endpoint::start(move |_| {
        let action = match observed.fetch_add(1, Ordering::SeqCst) {
            0 => json!({"kind":"search_capabilities","query":"powershell"}),
            1 => {
                json!({"kind":"checkpoint","checkpoint":{"decisions":["Never replay an uncertain native write"],"unresolved":[],"next_action":"Run the Windows write once","milestones":[]}})
            }
            2 => json!({"kind":"invoke","capability":"mcp.windows-host.powershell","args":{
                "script":"[IO.File]::AppendAllText((Join-Path (Get-Location).Path 'effects.txt'), 'effect' + [Environment]::NewLine); [Console]::WriteLine('IN_FLIGHT_EFFECT'); Start-Sleep -Seconds 15; [Console]::WriteLine('COMMAND_FINISHED')","timeout_seconds":20}}),
            _ => anyhow::bail!("Unsafe native restart must not request more inference"),
        };
        Ok((
            200,
            json!({"choices":[{"message":{"content":action.to_string()}}],"usage":{"prompt_tokens":10,"completion_tokens":2}}),
        ))
    })?;
    let mut command = Command::new(&binary);
    command
        .current_dir(directory.path())
        .args([
            "run",
            "Write once; preserve unknown effects rather than replaying",
            "--provider",
            "custom",
            "--endpoint",
            &endpoint.url,
            "--model",
            "fixture",
            "--mode",
            "durable",
            "--allow-write",
            "--allow-mcp",
            "windows-host:powershell",
            "--wall-seconds",
            "60",
            "--foreground",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = arun::process::spawn(command)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(12);
    loop {
        if directory.path().join("effects.txt").exists() {
            let store = Store::open(&root)?;
            let run = store.runs()?.remove(0);
            if store.events(&run.id)?.iter().any(|event| {
                event.kind == "operation.output"
                    && event.payload["text"]
                        .as_str()
                        .is_some_and(|text| text.contains("IN_FLIGHT_EFFECT"))
            }) {
                break;
            }
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline && child.try_wait()?.is_none(),
            "Native effect did not start"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    let store = Store::open(&root)?;
    let original = store.runs()?.remove(0);
    let pending = store.operations(&original.id)?.remove(0);
    assert!(pending.artifact.is_none());
    child.kill()?;
    child.wait()?;
    let recovered = Command::new(&binary)
        .args(["serve", root.to_str().unwrap(), &original.id])
        .output()?;
    endpoint.finish()?;
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    let store = Store::open(&root)?;
    let run = store.run(&original.id)?;
    assert_eq!(run.state, "waiting_recovery");
    assert_eq!(run.budgets, original.budgets);
    assert_eq!(store.unknown_count(&run.id)?, 1);
    let operations = store.operations(&run.id)?;
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0].id, pending.id);
    assert_eq!(operations[0].state, "outcome_unknown");
    assert!(operations[0].artifact.is_none());
    assert_eq!(store.event_count(&run.id, "operation.executing")?, 1);
    assert_eq!(store.event_count(&run.id, "operation.succeeded")?, 0);
    assert_eq!(store.event_count(&run.id, "run.completed")?, 0);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(store.model_tokens(&run.id)?, 36);
    assert_eq!(
        std::fs::read_to_string(directory.path().join("effects.txt"))?
            .lines()
            .collect::<Vec<_>>(),
        ["effect"]
    );
    Ok(())
}

#[test]
fn native_committed_write_survives_runner_death_without_repeating_the_effect() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let binary = std::env::var("AEGIS_WINDOWS_HOST_BINARY")
        .unwrap_or_else(|_| env!("CARGO_BIN_EXE_arun").into());
    let script = std::env::var_os("AEGIS_WINDOWS_HOST_SCRIPT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/windows-host.mjs")
        });
    let registration = Command::new(&binary)
        .args(["mcp", "add", "windows-host", "--trusted-host", "node"])
        .arg(script)
        .arg("--trusted-host")
        .current_dir(directory.path())
        .output()?;
    assert!(
        registration.status.success(),
        "{}",
        String::from_utf8_lossy(&registration.stderr)
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let (cut_tx, cut_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let endpoint = http::Endpoint::start(move |body| {
        let state = http::state(body)?;
        let action = match observed.fetch_add(1, Ordering::SeqCst) {
            0 => json!({"kind":"search_capabilities","query":"powershell"}),
            1 => {
                json!({"kind":"checkpoint","checkpoint":{"decisions":["Keep the committed Windows write; never repeat it"],"unresolved":[],"next_action":"Perform the native write once","milestones":[]}})
            }
            2 => {
                json!({"kind":"invoke","capability":"mcp.windows-host.powershell","args":{"script":"[IO.File]::AppendAllText((Join-Path (Get-Location).Path 'effects.txt'), 'effect' + [Environment]::NewLine); [Console]::WriteLine('NATIVE_EFFECT_COMMITTED')","timeout_seconds":10}})
            }
            3 => {
                assert_eq!(state["recent_operation_outcomes"][0]["state"], "succeeded");
                cut_tx.send(())?;
                release_rx.recv_timeout(Duration::from_secs(10))?;
                json!({"kind":"finish","summary":"Reply discarded after runner termination","evidence":[state["recent_operation_outcomes"][0]["artifact"]]})
            }
            4 => {
                assert!(
                    state["handoff"]["decisions"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|decision| decision
                            == "Keep the committed Windows write; never repeat it")
                );
                assert_eq!(state["recent_operation_outcomes"][0]["state"], "succeeded");
                json!({"kind":"finish","summary":"Recovered the existing native write receipt","evidence":[state["recent_operation_outcomes"][0]["artifact"]]})
            }
            _ => anyhow::bail!("Unexpected model turn after native recovery"),
        };
        Ok((
            200,
            json!({"choices":[{"message":{"content":action.to_string()}}],"usage":{"prompt_tokens":10,"completion_tokens":2}}),
        ))
    })?;
    let mut command = Command::new(&binary);
    command
        .current_dir(directory.path())
        .args([
            "run",
            "Perform a Windows write once and retain its receipt",
            "--provider",
            "custom",
            "--endpoint",
            &endpoint.url,
            "--model",
            "fixture",
            "--mode",
            "durable",
            "--allow-write",
            "--allow-mcp",
            "windows-host:powershell",
            "--wall-seconds",
            "90",
            "--foreground",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = arun::process::spawn(command)?;
    cut_rx.recv_timeout(Duration::from_secs(15))?;
    let store = Store::open(&root)?;
    let original = store.runs()?.remove(0);
    let committed = store.operations(&original.id)?;
    assert_eq!(committed.len(), 1);
    assert_eq!(committed[0].state, "succeeded");
    assert!(committed[0].artifact.is_some());
    child.kill()?;
    child.wait()?;
    release_tx.send(())?;
    let recovered = Command::new(&binary)
        .args(["serve", root.to_str().unwrap(), &original.id])
        .output()?;
    endpoint.finish()?;
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    let store = Store::open(&root)?;
    let run = store.run(&original.id)?;
    assert_eq!(run.state, "completed");
    assert_eq!(run.task, original.task);
    assert_eq!(run.budgets, original.budgets);
    assert_eq!(run.grants, original.grants);
    let operations = store.operations(&run.id)?;
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0].id, committed[0].id);
    assert_eq!(operations[0].artifact, committed[0].artifact);
    assert_eq!(store.event_count(&run.id, "operation.executing")?, 1);
    assert_eq!(store.event_count(&run.id, "operation.succeeded")?, 1);
    assert_eq!(store.unknown_count(&run.id)?, 0);
    assert_eq!(store.model_tokens(&run.id)?, 48);
    assert_eq!(calls.load(Ordering::SeqCst), 5);
    assert_eq!(
        std::fs::read_to_string(directory.path().join("effects.txt"))?
            .lines()
            .collect::<Vec<_>>(),
        ["effect"]
    );
    Ok(())
}
