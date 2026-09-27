use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::Result;
use arun::storage::Store;
use serde_json::json;

#[test]
fn guided_recovery_requires_a_receipt_and_does_not_repeat_a_write() -> Result<()> {
    for (choice, note, expected, cancelled) in [
        ("1", "", "outcome_unknown", false),
        ("1", "verified external receipt", "succeeded", false),
        ("2", "verified failed receipt", "failed", false),
        ("1", "verified after cancellation", "succeeded", true),
    ] {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join(".arun");
        fs::write(directory.path().join("fixture.txt"), "untouched")?;
        let mut store = Store::open(&root)?;
        let run = store.create_run(
            "write fixture",
            directory.path(),
            "custom",
            json!(["workspace.write"]),
            json!({}),
            "verify outcome",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let operation = store.begin_operation(
            &run.id,
            "workspace.write",
            json!({"path":"fixture.txt", "content":"would be duplicated"}),
            false,
        )?;
        store.operation_state(&operation, "dispatched", None, json!({}))?;
        store.reconcile(&run.id)?;
        if cancelled {
            store.state(&run.id, "cancelled", json!({}))?;
        }
        fs::write(
            root.join("profile.json"),
            serde_json::to_vec(
                &json!({"provider":"custom", "model":"fixture", "endpoint":{"base_url":"http://127.0.0.1:9/v1", "api_key_env":null}, "write":true, "image":null, "previous_run":run.id}),
            )?,
        )?;
        let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
            .current_dir(directory.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(format!("/sessions\n1\n4\n5\n1\n{choice}\n{note}\n/quit\n").as_bytes())?;
        let output = child.wait_with_output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("Record an externally verified outcome"));
        assert_eq!(store.operation(&operation.id)?.state, expected);
        assert_eq!(store.event_count(&run.id, "operation.dispatched")?, 1);
        assert_eq!(store.event_count(&run.id, "model.started")?, 0);
        assert_eq!(
            fs::read_to_string(directory.path().join("fixture.txt"))?,
            "untouched"
        );
        if note.is_empty() {
            assert_eq!(store.run(&run.id)?.state, "waiting_recovery");
            assert!(stdout.contains("A verification note is required"));
        } else {
            assert_eq!(
                store.run(&run.id)?.state,
                if cancelled { "cancelled" } else { "ready" }
            );
            let artifacts = store.evidence_artifacts(&run.id)?;
            assert_eq!(artifacts.len(), usize::from(expected == "succeeded"));
        }
    }
    Ok(())
}
