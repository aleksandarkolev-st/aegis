use anyhow::Result;
use arun::storage::Store;
use serde_json::json;
use std::{
    fs,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

#[path = "support/http.rs"]
mod http;

fn tested_binary() -> String {
    std::env::var("AEGIS_LIVE_EDITS_BINARY").unwrap_or_else(|_| env!("CARGO_BIN_EXE_arun").into())
}

fn response(action: serde_json::Value) -> Result<(u16, serde_json::Value)> {
    Ok((
        200,
        json!({"choices":[{"message":{"content":action.to_string()}}],"usage":{"prompt_tokens":10,"completion_tokens":2}}),
    ))
}

#[test]
fn writes_and_patch_removals_display_diffs_without_an_extra_model_request() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let endpoint = http::Endpoint::start(move |body| {
        let state = http::state(body)?;
        match observed.fetch_add(1, Ordering::SeqCst) {
            0 => response(
                json!({"kind":"invoke","capability":"workspace.write","args":{"path":"code.rs","content":"fn keep() {}\nfn remove() {}\n"}}),
            ),
            1 => response(
                json!({"kind":"invoke","capability":"workspace.patch","args":"{\"path\":\"code.rs\"}","args_edits":[{"old":"fn remove() {}\n","new":""}]}),
            ),
            2 => {
                assert!(!body.to_string().contains("Observed workspace change"));
                response(
                    json!({"kind":"finish","summary":"Code written and removed","evidence":[state["recent_operation_outcomes"][0]["artifact"]]}),
                )
            }
            _ => anyhow::bail!("unexpected model request"),
        }
    })?;
    let output = Command::new(tested_binary())
        .current_dir(directory.path())
        .args([
            "run",
            "Write code and remove a function",
            "--provider",
            "custom",
            "--model",
            "fixture",
            "--endpoint",
            &endpoint.url,
            "--mode",
            "eager",
            "--allow-write",
            "--foreground",
        ])
        .output()?;
    endpoint.finish()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Live edit"), "{stdout}");
    assert!(stdout.contains("+fn keep() {}"), "{stdout}");
    assert!(stdout.contains("-fn remove() {}"), "{stdout}");
    assert_eq!(
        fs::read_to_string(directory.path().join("code.rs"))?,
        "fn keep() {}\n"
    );
    let store = Store::open(&directory.path().join(".arun"))?;
    let run = store.runs()?.remove(0);
    assert_eq!(run.state, "completed");
    let events = store.events(&run.id)?;
    let diffs: Vec<_> = events
        .iter()
        .filter(|e| e.kind == "operation.diff")
        .collect();
    assert_eq!(diffs.len(), 2);
    for diff in diffs {
        let success = events
            .iter()
            .find(|e| e.kind == "operation.succeeded" && e.payload["id"] == diff.payload["id"])
            .unwrap();
        assert!(diff.seq < success.seq);
        assert_eq!(
            store.artifact(diff.payload["artifact"].as_str().unwrap())?,
            diff.payload["text"].as_str().unwrap().as_bytes()
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(store.model_tokens(&run.id)?, 36);
    Ok(())
}

#[test]
#[cfg(windows)]
fn native_shell_edits_create_and_delete_files_before_the_tool_finishes() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    fs::write(directory.path().join("code.rs"), "old code\n")?;
    fs::write(directory.path().join("removed.rs"), "removed code\n")?;
    let binary = tested_binary();
    let host = std::env::var_os("AEGIS_LIVE_EDITS_HOST_SCRIPT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/windows-host.mjs")
        });
    let registration = Command::new(&binary)
        .current_dir(directory.path())
        .args(["mcp", "add", "windows-host", "--trusted-host", "node"])
        .arg(host)
        .arg("--trusted-host")
        .output()?;
    assert!(
        registration.status.success(),
        "{}",
        String::from_utf8_lossy(&registration.stderr)
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let endpoint = http::Endpoint::start(move |body| {
        let state = http::state(body)?;
        match observed.fetch_add(1, Ordering::SeqCst) {
            0 => response(
                json!({"kind":"invoke","capability":"mcp.windows-host.powershell","args":{
                "script":"function Wait-Release($stage) { $end = [DateTime]::UtcNow.AddSeconds(20); while (!(Test-Path \".arun/release-$stage\")) { if ([DateTime]::UtcNow -gt $end) { throw 'Release deadline' }; Start-Sleep -Milliseconds 25 } }; [IO.File]::WriteAllText((Join-Path (Get-Location).Path 'code.rs'), \"new code`n\"); Wait-Release 1; [IO.File]::WriteAllText((Join-Path (Get-Location).Path 'added.rs'), \"added code`n\"); Wait-Release 2; Remove-Item -LiteralPath 'removed.rs'; Wait-Release 3",
                "timeout_seconds":60}}),
            ),
            1 => response(
                json!({"kind":"finish","summary":"Live edits verified","evidence":[state["recent_operation_outcomes"][0]["artifact"]]}),
            ),
            _ => anyhow::bail!("unexpected model request"),
        }
    })?;
    let log = directory.path().join(".arun/foreground.log");
    let file = fs::File::create(&log)?;
    let mut command = Command::new(&binary);
    command
        .current_dir(directory.path())
        .args([
            "run",
            "Modify, create and delete code files",
            "--provider",
            "custom",
            "--model",
            "fixture",
            "--endpoint",
            &endpoint.url,
            "--mode",
            "eager",
            "--allow-write",
            "--allow-mcp",
            "windows-host:powershell",
            "--process-seconds",
            "70",
            "--foreground",
        ])
        .stdout(file.try_clone()?)
        .stderr(file);
    let mut child = arun::process::spawn(command)?;
    for (stage, markers) in [
        (1, vec!["-old code", "+new code"]),
        (2, vec!["+++ b/added.rs", "+added code"]),
        (3, vec!["+++ /dev/null", "-removed code"]),
    ] {
        let deadline = Instant::now() + Duration::from_secs(18);
        loop {
            let store = Store::open(&root)?;
            if let Some(run) = store.runs()?.first() {
                let saved = store.events(&run.id)?.iter().any(|e| {
                    e.kind == "operation.diff"
                        && markers
                            .iter()
                            .all(|m| e.payload["text"].as_str().unwrap_or_default().contains(m))
                });
                let shown = fs::read_to_string(&log)?;
                if saved && markers.iter().all(|m| shown.contains(m)) {
                    assert!(
                        child.try_wait()?.is_none(),
                        "tool ended before stage {stage} was displayed"
                    );
                    let operations = store.operations(&run.id)?;
                    assert_eq!(operations.len(), 1);
                    assert!(
                        operations[0].artifact.is_none(),
                        "diff must appear before committed success"
                    );
                    break;
                }
            }
            anyhow::ensure!(
                Instant::now() < deadline && child.try_wait()?.is_none(),
                "missing stage {stage}: {}",
                fs::read_to_string(&log)?
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        fs::write(root.join(format!("release-{stage}")), "continue")?;
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while child.try_wait()?.is_none() {
        anyhow::ensure!(Instant::now() < deadline, "task did not finish");
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(child.wait()?.success(), "{}", fs::read_to_string(&log)?);
    endpoint.finish()?;
    let store = Store::open(&root)?;
    let run = store.runs()?.remove(0);
    assert_eq!(run.state, "completed");
    assert_eq!(store.event_count(&run.id, "operation.diff")?, 3);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(store.model_tokens(&run.id)?, 24);
    assert!(!directory.path().join("removed.rs").exists());
    Ok(())
}
