//! Detect unchanged outcomes, while treating new exploratory findings as progress.
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    model::Action,
    storage::{Run, Store, append_event},
};

pub(crate) fn ensure_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS loop_progress (
        run_id TEXT PRIMARY KEY REFERENCES runs(id), epoch TEXT NOT NULL,
        unchanged INTEGER NOT NULL, last_error TEXT
    );
    CREATE TABLE IF NOT EXISTS loop_observations (
        run_id TEXT NOT NULL REFERENCES runs(id), signature TEXT NOT NULL,
        PRIMARY KEY(run_id,signature)
    );",
    )?;
    Ok(())
}

fn digest(value: &Value) -> String {
    hex::encode(Sha256::digest(value.to_string().as_bytes()))
}

fn stable(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            for field in ["elapsed_ms", "duration_ms", "elapsed_seconds"] {
                fields.remove(field);
            }
            for value in fields.values_mut() {
                stable(value);
            }
        }
        Value::Array(items) => {
            for value in items {
                stable(value);
            }
        }
        Value::String(text) => {
            if let Ok(mut nested) = serde_json::from_str::<Value>(text) {
                if nested.is_object() || nested.is_array() {
                    stable(&mut nested);
                    *text = nested.to_string();
                }
            }
        }
        _ => {}
    }
}

pub(crate) fn observe(
    store: &mut Store,
    run: &Run,
    action: &Action,
    error: Option<&str>,
) -> Result<()> {
    if store.run(&run.id)?.state != "running" {
        return Ok(());
    }
    let outcome = if let Some(error) = error {
        json!({"error":error})
    } else {
        match action {
            Action::Invoke { .. } => {
                let receipt:Option<(String,Option<String>)>=store.connection.query_row("SELECT state,artifact FROM operations WHERE run_id=?1 ORDER BY rowid DESC LIMIT 1",[&run.id],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
                let Some((state, Some(hash))) = receipt else {
                    return Ok(());
                };
                let bytes = store.artifact(&hash)?;
                let mut value: Value =
                    serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({"artifact":hash}));
                // Hash the full receipt locally, including image bytes: a new
                // screenshot is new information even when its size is unchanged.
                // Only the digest is persisted; receipt data never enters policy.
                stable(&mut value);
                json!({"state":state,"result":value})
            }
            Action::SearchCapabilities { .. } => {
                let event = store.recent_context_events(&run.id, 1)?;
                json!({"matches":event.first().map(|event|&event.payload["matches"])})
            }
            Action::InspectResult { artifact, query } => json!({"artifact":artifact,"query":query}),
            Action::Checkpoint { .. } | Action::Remember { .. } => json!({"maintenance":true}),
            Action::AskUser { query } => json!({"question":query}),
            Action::VerifyObligations { obligations } => json!({"verified":obligations}),
            Action::Finish { .. } | Action::Blocked { .. } => return Ok(()),
        }
    };
    let owner: i64 = store.connection.query_row(
        "SELECT COALESCE(MAX(seq),0) FROM task_owner_messages WHERE run_id=?1",
        [&run.id],
        |row| row.get(0),
    )?;
    let observed: i64 = store.connection.query_row(
        "SELECT COALESCE(MAX(changed_seq),0) FROM observed_files WHERE run_id=?1",
        [&run.id],
        |row| row.get(0),
    )?;
    let source: Option<String> = store
        .connection
        .query_row(
            "SELECT fingerprint FROM workspace_fingerprints WHERE run_id=?1",
            [&run.id],
            |row| row.get(0),
        )
        .optional()?;
    let epoch = digest(&json!({"source":source,"observed":observed,"owner":owner}));
    let mut key_action = serde_json::to_value(action)?;
    // Model-authored prose is not new environmental evidence. A different
    // checkpoint wording must not disguise a loop with no new observations.
    let maintenance =
        matches!(action, Action::Remember { .. } | Action::Checkpoint { .. }) && error.is_none();
    if maintenance {
        key_action = json!({"maintenance":true});
    }
    let failed = error.is_some()
        || outcome["state"] == "failed"
        || outcome["result"]["exit_code"]
            .as_i64()
            .is_some_and(|code| code != 0)
        || outcome["result"]["isError"] == true;
    // Equivalent failures are the same blocker even if a retry changes its
    // arguments. Successful exploratory actions still retain their full key.
    let signature_action = if failed { json!({"failure":true}) } else { key_action.clone() };
    let signature = digest(&json!({"action":signature_action,"outcome":outcome,"epoch":epoch}));
    let transaction = store.connection.unchecked_transaction()?;
    let previous: Option<(String, i64)> = transaction
        .query_row(
            "SELECT epoch,unchanged FROM loop_progress WHERE run_id=?1",
            [&run.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let novel = transaction.execute(
        "INSERT OR IGNORE INTO loop_observations(run_id,signature) VALUES (?1,?2)",
        params![run.id, signature],
    )? == 1;
    let unchanged = if previous.as_ref().is_some_and(|(old, _)| old != &epoch) {
        0
    } else if novel && !maintenance {
        0
    } else {
        previous.map(|(_, count)| count).unwrap_or(0) + 1
    };
    transaction.execute("INSERT INTO loop_progress(run_id,epoch,unchanged,last_error) VALUES (?1,?2,?3,?4) ON CONFLICT(run_id) DO UPDATE SET epoch=excluded.epoch,unchanged=excluded.unchanged,last_error=excluded.last_error",params![run.id,epoch,unchanged,error])?;
    let limit = if failed { 2 } else { 4 };
    if unchanged >= limit {
        append_event(
            &transaction,
            &run.id,
            "loop.stalled",
            json!({"unchanged":unchanged,"error":error,"action":key_action,"reason":"repeated actions produced no new information or verified workspace progress"}),
        )?;
        transaction.execute(
            "UPDATE runs SET state='waiting_recovery' WHERE id=?1 AND state='running'",
            [&run.id],
        )?;
        append_event(
            &transaction,
            &run.id,
            "run.waiting_recovery",
            json!({"reason":"repeated unchanged actions; resume with a different approach or new task-owner input"}),
        )?;
    }
    transaction.commit()?;
    Ok(())
}

pub(crate) fn add_context(store: &Store, run: &Run, context: &mut Value) -> Result<()> {
    let state:Option<(i64,Option<String>)>=store.connection.query_row("SELECT unchanged,last_error FROM loop_progress WHERE run_id=?1 AND (unchanged>=2 OR (unchanged>0 AND last_error IS NOT NULL))",[&run.id],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
    if let Some((unchanged, error)) = state {
        context["progress_guard"] = json!({"unchanged":unchanged,"last_error":error,"policy":"Recent actions repeated unchanged outcomes. Reuse mapped information and choose a different approach. New relevant searches, files, ranges, results or actual workspace changes count as progress. Exploration needs findings, not invented edits. If waiting for external state, use a bounded granted wait/poll tool or blocked instead of busy repetition. Unchanged retries will pause durably."});
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_retry_arguments_cannot_disguise_the_same_permission_blocker() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run("Explore", directory.path(), "custom", json!([]), json!({}), "")?;
        store.state(&run.id, "running", json!({}))?;
        for retry in 0..8 {
            observe(&mut store, &run, &Action::Invoke {
                capability: format!("unavailable.{retry}"), args: json!({"retry":retry}),
            }, Some("capability not granted"))?;
        }
        assert_eq!(store.run(&run.id)?.state, "waiting_recovery");
        assert_eq!(store.event_count(&run.id, "loop.stalled")?, 1);
        store.state(&run.id, "running", json!({}))?;
        store.steer(&run.id, "Use the newly granted alternate approach")?;
        observe(&mut store, &run, &Action::Invoke { capability: "alternate".into(), args: json!({}) }, Some("capability not granted"))?;
        assert_eq!(store.run(&run.id)?.state, "running");
        Ok(())
    }

    #[test]
    fn unchanged_rejections_pause_and_the_guard_survives_restart() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "Explore",
            directory.path(),
            "custom",
            json!([]),
            json!({}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let action = Action::Invoke {
            capability: "unavailable".into(),
            args: json!({}),
        };
        observe(&mut store, &run, &action, Some("capability not granted"))?;
        observe(&mut store, &run, &action, Some("capability not granted"))?;
        drop(store);
        let mut store = Store::open(directory.path())?;
        observe(&mut store, &run, &action, Some("capability not granted"))?;
        assert_eq!(store.run(&run.id)?.state, "waiting_recovery");
        assert_eq!(store.event_count(&run.id, "loop.stalled")?, 1);
        store.state(&run.id, "running", json!({}))?;
        store.steer(&run.id, "Use a different granted tool")?;
        observe(&mut store, &run, &action, Some("capability not granted"))?;
        assert_eq!(store.run(&run.id)?.state, "running");
        Ok(())
    }

    #[test]
    fn new_exploratory_queries_progress_but_cycling_known_results_does_not() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "Explore",
            directory.path(),
            "custom",
            json!([]),
            json!({}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        for index in 0..40 {
            let action = Action::InspectResult {
                artifact: "fixture".into(),
                query: format!("@slice {} 4000", index * 4000),
            };
            observe(&mut store, &run, &action, None)?;
        }
        assert_eq!(store.run(&run.id)?.state, "running");
        for index in 0..4 {
            let action = Action::InspectResult {
                artifact: "fixture".into(),
                query: format!("@slice {} 4000", (index % 2) * 4000),
            };
            observe(&mut store, &run, &action, None)?;
        }
        assert_eq!(store.run(&run.id)?.state, "waiting_recovery");
        Ok(())
    }

    #[test]
    fn unchanged_failed_commands_stall_despite_conservative_revision_bumps() -> Result<()> {
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join("source.txt"), "original")?;
        let mut store = Store::open(&directory.path().join(".arun"))?;
        let run = store.create_run(
            "Diagnose a failing command",
            directory.path(),
            "custom",
            json!(["workspace.read", "workspace.write", "process.run"]),
            json!({}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let action = Action::Invoke {
            capability: "process.run".into(),
            args: json!({"command":"failing fixture"}),
        };
        for _ in 0..3 {
            let operation = store.begin_operation(
                &run.id,
                "process.run",
                json!({"command":"failing fixture"}),
                true,
            )?;
            crate::storage::claim_test_operation(&mut store, &operation)?;
            let artifact = store.put_artifact(br#"{"exit_code":1,"stderr":"same failure"}"#)?;
            store.operation_state(&operation, "failed", Some(&artifact), json!({}))?;
            observe(&mut store, &run, &action, None)?;
        }
        assert_eq!(store.run(&run.id)?.state, "waiting_recovery");
        assert!(store.workspace_revision(&run.id)?.unwrap() > 0);
        store.state(&run.id, "running", json!({}))?;
        std::fs::write(directory.path().join("source.txt"), "fixed source")?;
        let operation = store.begin_operation(
            &run.id,
            "process.run",
            json!({"command":"failing fixture"}),
            true,
        )?;
        crate::storage::claim_test_operation(&mut store, &operation)?;
        let artifact = store.put_artifact(br#"{"exit_code":1,"stderr":"same failure"}"#)?;
        store.operation_state(&operation, "failed", Some(&artifact), json!({}))?;
        observe(&mut store, &run, &action, None)?;
        assert_eq!(
            store.run(&run.id)?.state,
            "running",
            "An actual source change permits a fresh attempt"
        );
        Ok(())
    }
}
