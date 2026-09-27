use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::Result;
use arun::storage::Store;
use serde_json::json;

#[path = "support/http.rs"]
mod http;

#[test]
fn guided_and_advanced_http_failures_show_recovery_hints_not_provider_json() -> Result<()> {
    for guided in [false, true] {
        let directory = tempfile::tempdir()?;
        let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = std::sync::Arc::clone(&requests);
        let endpoint = http::Endpoint::start(move |body| {
            assert_eq!(http::state(body)?["task"], "Read the workspace");
            observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok((
                401,
                json!({"error":{"type":"authentication_error","message":"OAuth access token has expired"},
                "privateDiagnostic":"fixture-private-diagnostic"}),
            ))
        })?;
        let root = directory.path().join(".arun");
        fs::create_dir(&root)?;
        fs::write(
            root.join("profile.json"),
            serde_json::to_vec(
                &json!({"provider":"custom","model":"fixture-model","endpoint":{"base_url":endpoint.url,"api_key_env":null},"write":false,"image":null}),
            )?,
        )?;
        let mut command = Command::new(env!("CARGO_BIN_EXE_arun"));
        command
            .current_dir(directory.path())
            .env("PATH", directory.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = if guided {
            let mut child = command.stdin(Stdio::piped()).spawn()?;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(b"Read the workspace\n/quit\n")?;
            child.wait_with_output()?
        } else {
            command
                .args([
                    "run",
                    "Read the workspace",
                    "--provider",
                    "custom",
                    "--endpoint",
                    &endpoint.url,
                    "--model",
                    "fixture-model",
                    "--foreground",
                ])
                .output()?
        };
        endpoint.finish()?;
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("F4"));
        assert!(!text.contains("privateDiagnostic"));
        assert!(!text.contains("fixture-private-diagnostic"));
        assert!(!text.contains("\"type\""));
        let store = Store::open(&root)?;
        let runs = store.runs()?;
        assert_eq!(runs.len(), 1);
        let run = &runs[0];
        assert_eq!(run.state, "waiting_recovery");
        assert_eq!(store.event_count(&run.id, "model.failed")?, 1);
        assert!(store.operations(&run.id)?.is_empty());
        assert!(store.events(&run.id)?.iter().any(|event| {
            event.kind == "model.failed"
                && event.payload["error"].as_str().is_some_and(|error| {
                    error.contains("HTTP 401") && error.contains("body omitted")
                })
        }));
        assert!(
            !serde_json::to_string(&store.events(&run.id)?)?.contains("fixture-private-diagnostic")
        );
        assert_eq!(store.event_count(&run.id, "model.response")?, 0);
        assert_eq!(store.model_tokens(&run.id)?, 0);
        assert_eq!(
            arun::trace::metrics(&store.events(&run.id)?).unaccounted_model_attempts,
            1
        );
    }
    Ok(())
}
