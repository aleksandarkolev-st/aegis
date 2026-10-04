//! Task-owner messages survive delivery, archival, and driver restarts.
use anyhow::{Result, bail};
use rusqlite::{Connection, Transaction, params};
use serde_json::Value;

use crate::storage::{Event, Store};

pub(crate) fn ensure_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS task_owner_messages (
            run_id TEXT NOT NULL REFERENCES runs(id), seq INTEGER NOT NULL,
            payload TEXT NOT NULL, created_at INTEGER NOT NULL, delivered INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY(run_id,seq)
        );
        CREATE TABLE IF NOT EXISTS steering_projection (
            run_id TEXT PRIMARY KEY REFERENCES runs(id), turn_through INTEGER
        );",
    )?;
    Ok(())
}

pub(crate) fn track(
    transaction: &Transaction<'_>, run_id: &str, seq: i64,
    kind: &str, payload: &Value, created_at: i64,
) -> Result<()> {
    match kind {
        "run.created" => {
            transaction.execute("INSERT OR IGNORE INTO steering_projection(run_id) VALUES (?1)", [run_id])?;
        }
        "user.steering" => {
            transaction.execute("INSERT OR IGNORE INTO task_owner_messages(run_id,seq,payload,created_at) VALUES (?1,?2,?3,?4)",params![run_id,seq,payload.to_string(),created_at])?;
        }
        "model.started" => {
            // Capture the mailbox included in this request. Messages arriving
            // during inference must not be acknowledged by its response.
            transaction.execute("UPDATE steering_projection SET turn_through=COALESCE(?2,(SELECT MAX(seq) FROM task_owner_messages WHERE run_id=?1),0) WHERE run_id=?1",params![run_id,payload["steering_through"].as_i64()])?;
        }
        "model.response" => {
            transaction.execute("UPDATE task_owner_messages SET delivered=1 WHERE run_id=?1 AND seq <= COALESCE((SELECT turn_through FROM steering_projection WHERE run_id=?1),(SELECT MAX(seq) FROM task_owner_messages WHERE run_id=?1))",[run_id])?;
            transaction.execute("UPDATE steering_projection SET turn_through=NULL WHERE run_id=?1",[run_id])?;
        }
        "model.failed" => {
            transaction.execute("UPDATE steering_projection SET turn_through=NULL WHERE run_id=?1",[run_id])?;
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn restore(store: &Store) -> Result<()> {
    let runs = {
        let mut query = store.connection.prepare("SELECT id FROM runs WHERE NOT EXISTS(SELECT 1 FROM steering_projection WHERE run_id=runs.id)")?;
        query.query_map([], |row| row.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for run_id in runs {
        // One-time migration includes integrity-checked archived records.
        let events = store.events(&run_id)?;
        let transaction = store.connection.unchecked_transaction()?;
        transaction.execute("INSERT OR IGNORE INTO steering_projection(run_id) VALUES (?1)",[&run_id])?;
        for event in events {
            track(&transaction,&run_id,event.seq,&event.kind,&event.payload,event.created_at)?;
        }
        transaction.commit()?;
    }
    Ok(())
}

pub(crate) fn validate_consumed(connection: &Connection, run_id: &str) -> Result<()> {
    let pending: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM task_owner_messages WHERE run_id=?1 AND delivered=0)",
        [run_id], |row| row.get(0),
    )?;
    if pending {
        bail!("task-owner messages arrived after the last model request; read them before finishing");
    }
    Ok(())
}

impl Store {
    pub(crate) fn steering_messages(&self, run_id: &str, delivered: bool) -> Result<Vec<Event>> {
        let mut query = self.connection.prepare("SELECT seq,payload,created_at FROM task_owner_messages WHERE run_id=?1 AND delivered=?2 ORDER BY seq")?;
        let rows = query.query_map(params![run_id,delivered], |row| {
            let payload: String = row.get(1)?;
            Ok(Event {seq:row.get(0)?,kind:"user.steering".into(),payload:serde_json::from_str(&payload).map_err(|error|rusqlite::Error::FromSqlConversionFailure(1,rusqlite::types::Type::Text,Box::new(error)))?,created_at:row.get(2)?})
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn late_messages_prevent_both_reply_and_tool_completion_until_delivered() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join(".arun");
        let mut store = Store::open(&root)?;
        let mut mailbox = Store::open(&root)?;
        for tools in [false, true] {
            let run = store.create_run("Inspect", directory.path(), "custom", json!([]), json!({}), "")?;
            store.state(&run.id, "running", json!({}))?;
            let mut evidence = Vec::new();
            if tools {
                let operation = store.begin_operation(&run.id, "mcp.fixture.inspect", json!({}), true)?;
                crate::storage::claim_test_operation(&mut store, &operation)?;
                let hash = store.put_artifact(b"inspection result")?;
                store.operation_state(&operation, "succeeded", Some(&hash), json!({}))?;
                evidence.push(hash);
            }
            store.event(&run.id, "model.started", json!({}))?;
            store.event(&run.id, "model.response", json!({}))?;
            mailbox.steer(&run.id, "Also inspect the edge case before finishing")?;
            let finish = if tools { store.complete_run(&run.id, "Inspected", &evidence) } else { store.answer_run(&run.id, "Reply") };
            assert!(finish.unwrap_err().to_string().contains("task-owner messages"));
            assert_eq!(store.run(&run.id)?.state, "running");
            drop(store);
            store = Store::open(&root)?;
            assert_eq!(store.pending_steering(&run.id)?.len(), 1);
            store.event(&run.id, "model.started", json!({}))?;
            store.event(&run.id, "model.response", json!({}))?;
            if tools { store.complete_run(&run.id, "Inspected", &evidence)?; } else { store.answer_run(&run.id, "Reply")?; }
            assert!(store.run(&run.id)?.is_terminal());
            assert!(store.pending_steering(&run.id)?.is_empty());
        }
        Ok(())
    }

    #[test]
    fn mailbox_and_delivered_constraints_survive_archival_restart_and_migration() -> Result<()> {
        let directory=tempfile::tempdir()?;
        let mut store=Store::open(directory.path())?;
        let run=store.create_run("Explore",directory.path(),"custom",json!([]),json!({}),"")?;
        store.state(&run.id,"running",json!({}))?;
        store.steer(&run.id,"Preserve API")?;
        for _ in 0..1100 { store.event(&run.id,"telemetry",json!({}))?; }
        store.maintain_history(&run.id)?;
        assert_eq!(store.pending_steering(&run.id)?.len(),1);
        drop(store);
        let mut store=Store::open(directory.path())?;
        assert_eq!(store.pending_steering(&run.id)?.len(),1);
        store.event(&run.id,"model.started",json!({}))?;
        store.event(&run.id,"model.response",json!({}))?;
        assert!(store.pending_steering(&run.id)?.is_empty());
        assert_eq!(store.steering_messages(&run.id,true)?[0].payload["text"],"Preserve API");
        store.connection.execute("DELETE FROM task_owner_messages WHERE run_id=?1",[&run.id])?;
        store.connection.execute("DELETE FROM steering_projection WHERE run_id=?1",[&run.id])?;
        drop(store);
        let store=Store::open(directory.path())?;
        assert!(store.pending_steering(&run.id)?.is_empty());
        assert_eq!(store.steering_messages(&run.id,true)?.len(),1);
        Ok(())
    }

    #[test]
    fn response_only_acknowledges_the_mailbox_at_request_start() -> Result<()> {
        let directory=tempfile::tempdir()?;
        let mut store=Store::open(directory.path())?;
        let run=store.create_run("Explore",directory.path(),"custom",json!([]),json!({}),"")?;
        store.state(&run.id,"running",json!({}))?;
        store.steer(&run.id,"First")?;
        store.event(&run.id,"model.started",json!({}))?;
        store.steer(&run.id,"Second")?;
        store.event(&run.id,"model.response",json!({}))?;
        assert_eq!(store.pending_steering(&run.id)?[0].payload["text"],"Second");
        assert_eq!(store.steering_messages(&run.id,true)?[0].payload["text"],"First");
        store.event(&run.id,"model.started",json!({"steering_through":0}))?;
        store.event(&run.id,"model.response",json!({}))?;
        assert_eq!(store.pending_steering(&run.id)?.len(),1);
        Ok(())
    }
}
