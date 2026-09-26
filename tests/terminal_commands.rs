use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::Result;
use arun::storage::Store;
use serde_json::json;

#[test]
fn checkpoint_view_and_cancel_confirmation_preserve_unknown_operations() -> Result<()> {
    for cancel in [false, true] {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join(".arun");
        let mut store = Store::open(&root)?;
        let run = store.create_run(
            "paused write",
            directory.path(),
            "custom",
            json!(["workspace.write"]),
            json!({}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let operation = store.begin_operation(
            &run.id,
            "workspace.write",
            json!({"path":"fixture.txt","content":"never executed"}),
            false,
        )?;
        store.operation_state(&operation, "dispatched", None, json!({}))?;
        store.reconcile(&run.id)?;
        store.save_checkpoint(&run.id, &serde_json::from_value(json!({"decisions":["Keep the original API"],"unresolved":["Unknown write needs a receipt"],"next_action":"Verify the paused operation","milestones":[]}))?)?;
        fs::write(
            root.join("profile.json"),
            serde_json::to_vec(
                &json!({"provider":"custom","model":"fixture","endpoint":{"base_url":"http://127.0.0.1:9/v1","api_key_env":null},"write":true,"image":null,"previous_run":run.id}),
            )?,
        )?;
        let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
            .current_dir(directory.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child.stdin.take().unwrap().write_all(
            format!(
                "/checkpoint\n/cancel\n{}\n/quit\n",
                if cancel { 2 } else { 1 }
            )
            .as_bytes(),
        )?;
        let output = child.wait_with_output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(
            text.contains("Verify the paused operation") && text.contains("Keep the original API")
        );
        assert!(text.contains("Confirm cancellation"));
        assert_eq!(
            store.run(&run.id)?.state,
            if cancel {
                "cancelled"
            } else {
                "waiting_recovery"
            }
        );
        assert_eq!(store.operation(&operation.id)?.state, "outcome_unknown");
        assert_eq!(store.event_count(&run.id, "model.started")?, 0);
        assert_eq!(store.event_count(&run.id, "operation.dispatched")?, 1);
        assert!(!directory.path().join("fixture.txt").exists());
    }
    Ok(())
}
