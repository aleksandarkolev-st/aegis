//! Durable user pause requests, acknowledged only at a kernel safe boundary.
use crate::storage::{Store, append_event};
use anyhow::{Result, bail};
use rusqlite::{Connection, Transaction, params};
use serde_json::json;

pub(crate) fn ensure_clock_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS pause_clock (
        run_id TEXT PRIMARY KEY REFERENCES runs(id), seconds INTEGER NOT NULL DEFAULT 0,
        paused_at INTEGER, ended_at INTEGER
    );",
    )?;
    let columns = connection
        .prepare("PRAGMA table_info(pause_clock)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !columns.iter().any(|name| name == "ended_at") {
        let transaction = rusqlite::Transaction::new_unchecked(
            connection,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let columns = transaction
            .prepare("PRAGMA table_info(pause_clock)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if !columns.iter().any(|name| name == "ended_at") {
            transaction.execute_batch(
                "ALTER TABLE pause_clock ADD COLUMN ended_at INTEGER; DELETE FROM pause_clock;",
            )?;
        }
        transaction.commit()?;
    }
    Ok(())
}

pub(crate) fn track_clock(
    transaction: &Transaction<'_>,
    id: &str,
    kind: &str,
    timestamp: i64,
) -> Result<()> {
    match kind {
        "run.created" => {
            transaction.execute(
                "INSERT OR IGNORE INTO pause_clock(run_id) VALUES (?1)",
                [id],
            )?;
        }
        "run.paused" => {
            transaction.execute(
                "UPDATE pause_clock SET paused_at=COALESCE(paused_at,?2) WHERE run_id=?1",
                params![id, timestamp],
            )?;
        }
        "pause.resumed" | "run.running" => {
            transaction.execute("UPDATE pause_clock SET seconds=seconds+CASE WHEN paused_at IS NULL THEN 0 ELSE MAX(0,?2-paused_at) END,paused_at=NULL WHERE run_id=?1",params![id,timestamp])?;
        }
        "run.completed" | "run.answered" | "run.cancelled" | "run.failed" => {
            transaction.execute(
                "UPDATE pause_clock SET ended_at=COALESCE(ended_at,?2) WHERE run_id=?1",
                params![id, timestamp],
            )?;
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn restore_clock(store: &Store) -> Result<()> {
    let ids = store
        .connection
        .prepare(
            "SELECT id FROM runs WHERE NOT EXISTS(SELECT 1 FROM pause_clock WHERE run_id=runs.id)",
        )?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        loop {
            let events = store.events(&id)?;
            let seq = events.last().map_or(0, |event| event.seq);
            let transaction = rusqlite::Transaction::new_unchecked(
                &store.connection,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            let current: i64 = transaction.query_row(
                "SELECT last_seq FROM run_projection WHERE run_id=?1",
                [&id],
                |row| row.get(0),
            )?;
            if current != seq {
                continue;
            }
            if transaction.execute(
                "INSERT OR IGNORE INTO pause_clock(run_id) VALUES (?1)",
                [&id],
            )? == 1
            {
                for event in events {
                    track_clock(&transaction, &id, &event.kind, event.created_at)?;
                }
            }
            transaction.commit()?;
            break;
        }
    }
    Ok(())
}

impl Store {
    pub(crate) fn execution_elapsed_seconds(&self, id: &str) -> Result<u64> {
        let Some(start) = self.run_started_at(id)? else {
            return Ok(0);
        };
        let (paused, at, ended): (i64, Option<i64>, Option<i64>) = self.connection.query_row(
            "SELECT seconds,paused_at,ended_at FROM pause_clock WHERE run_id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let end = at.or(ended).unwrap_or_else(crate::storage::unix_time);
        Ok(end.saturating_sub(start).saturating_sub(paused).max(0) as u64)
    }
    pub fn pause_requested(&self, id: &str) -> Result<bool> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM pause_requests WHERE run_id=?1 AND pending=1)",
            [id],
            |row| row.get(0),
        )?)
    }

    pub fn request_pause(&mut self, id: &str) -> Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let state: String =
            transaction.query_row("SELECT state FROM runs WHERE id=?1", [id], |row| row.get(0))?;
        if matches!(
            state.as_str(),
            "completed" | "answered" | "failed" | "cancelled"
        ) {
            bail!("Ended tasks cannot pause");
        }
        let pending: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM pause_requests WHERE run_id=?1 AND pending=1)",
            [id],
            |row| row.get(0),
        )?;
        if !pending {
            transaction.execute("INSERT INTO pause_requests(run_id,pending) VALUES (?1,1) ON CONFLICT(run_id) DO UPDATE SET pending=1",[id])?;
            append_event(
                &transaction,
                id,
                "pause.requested",
                json!({"source":"user","boundary":"after the current action records its outcome"}),
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn acknowledge_pause(&mut self, id: &str) -> Result<bool> {
        if !self.pause_requested(id)? || self.run(id)?.is_terminal() {
            return Ok(false);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let pending: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM pause_requests WHERE run_id=?1 AND pending=1)",
            [id],
            |row| row.get(0),
        )?;
        if !pending {
            return Ok(false);
        }
        let unresolved:i64=transaction.query_row("SELECT COUNT(*) FROM operations WHERE run_id=?1 AND state NOT IN ('succeeded','failed','cancelled')",[id],|row|row.get(0))?;
        if unresolved > 0 {
            return Ok(false);
        }
        let changed=transaction.execute("UPDATE runs SET state='paused' WHERE id=?1 AND state NOT IN ('completed','answered','failed','cancelled','paused')",[id])?;
        if changed > 0 {
            append_event(
                &transaction,
                id,
                "run.paused",
                json!({"reason":"user pause at a safe boundary"}),
            )?;
        }
        transaction.commit()?;
        Ok(true)
    }

    pub fn resume_paused(&mut self, id: &str) -> Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        crate::obligations::ensure_reviewed_contract(&transaction, id)?;
        let state: String =
            transaction.query_row("SELECT state FROM runs WHERE id=?1", [id], |row| row.get(0))?;
        if matches!(
            state.as_str(),
            "completed" | "answered" | "failed" | "cancelled"
        ) {
            bail!("Ended tasks cannot resume");
        }
        let unknown: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM operations WHERE run_id=?1 AND state='outcome_unknown'",
            [id],
            |row| row.get(0),
        )?;
        if unknown > 0 {
            bail!("Reconcile unknown outcomes before resuming");
        }
        append_event(&transaction, id, "pause.resumed", json!({"source":"user"}))?;
        transaction.execute("UPDATE pause_requests SET pending=0 WHERE run_id=?1", [id])?;
        if matches!(state.as_str(), "paused" | "waiting_recovery") {
            transaction.execute("UPDATE runs SET state='ready' WHERE id=?1", params![id])?;
            append_event(
                &transaction,
                id,
                "run.ready",
                json!({"source":"user_resume"}),
            )?;
        }
        transaction.commit()?;
        Ok(())
    }
}

pub(crate) fn boundary(store: &mut Store, id: &str) -> Result<bool> {
    if store.run(id)?.state == "paused" {
        return Ok(true);
    }
    if !store.pause_requested(id)? || store.run(id)?.is_terminal() {
        return Ok(false);
    }
    if !store.unresolved(id)?.is_empty() || store.unknown_count(id)? > 0 {
        return Ok(false);
    }
    // Preserve the model's actual checkpoint and milestones; no invented next action.
    // An existing checkpoint is historical context; stopping must not rewrite
    // the model's plan or create another checkpoint merely to acknowledge pause.
    if store.last_checkpoint(id)?.is_none() {
        let checkpoint = crate::model::Checkpoint {
            decisions: vec![],
            unresolved: vec!["User paused before a next action was checkpointed".into()],
            next_action: "Inspect saved run state and continue the original task".into(),
            milestones: store.milestones(id)?,
        };
        store.save_checkpoint(id, &checkpoint)?;
    }
    let paused = store.acknowledge_pause(id)?;
    if paused {
        store.save_snapshot(id)?;
    }
    Ok(paused)
}

pub fn request(root: &std::path::Path, id: &str) -> Result<()> {
    uuid::Uuid::parse_str(id)?;
    let mut store = Store::open(root)?;
    store.request_pause(id)?;
    // The lock, not run.state, tells us whether a runner is alive.
    let lock = std::fs::File::options()
        .write(true)
        .create(true)
        .open(root.join(format!("run-{id}.lock")))?;
    if fs2::FileExt::try_lock_exclusive(&lock).is_ok() {
        boundary(&mut store, id)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prior_clock_schema_rebuilds_a_frozen_terminal_duration_from_archives() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join(".arun");
        let mut store = Store::open(&root)?;
        let run = store.create_run(
            "Continue",
            directory.path(),
            "custom",
            json!([]),
            json!({"wall_seconds":60}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        store.state(&run.id, "failed", json!({}))?;
        store.connection.execute(
            "UPDATE run_projection SET started_at=1000 WHERE run_id=?1",
            [&run.id],
        )?;
        store.connection.execute(
            "UPDATE events SET created_at=1000 WHERE run_id=?1 AND kind='run.running'",
            [&run.id],
        )?;
        store.connection.execute(
            "UPDATE events SET created_at=1010 WHERE run_id=?1 AND kind='run.failed'",
            [&run.id],
        )?;
        for _ in 0..1100 {
            store.event(&run.id, "telemetry", json!({}))?;
        }
        store.maintain_history(&run.id)?;
        store
            .connection
            .execute_batch("ALTER TABLE pause_clock DROP COLUMN ended_at")?;
        drop(store);
        let store = Store::open(&root)?;
        assert_eq!(store.execution_elapsed_seconds(&run.id)?, 10);
        let budget = crate::control::view(&store, &run.id, "budget", None)?;
        assert_eq!(budget["wall_seconds"]["used"], 10);
        assert_eq!(budget["wall_seconds"]["remaining"], 50);
        Ok(())
    }

    #[test]
    fn paused_time_is_excluded_after_archival_migration_and_restart_without_resetting_budget()
    -> Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join(".arun");
        let mut store = Store::open(&root)?;
        let run = store.create_run(
            "Continue",
            directory.path(),
            "custom",
            json!([]),
            json!({"wall_seconds":60}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        store.request_pause(&run.id)?;
        assert!(boundary(&mut store, &run.id)?);
        // Simulate ten active seconds followed by years safely paused.
        store.connection.execute(
            "UPDATE run_projection SET started_at=1000 WHERE run_id=?1",
            [&run.id],
        )?;
        store.connection.execute(
            "UPDATE events SET created_at=1000 WHERE run_id=?1 AND kind='run.running'",
            [&run.id],
        )?;
        store.connection.execute(
            "UPDATE events SET created_at=1010 WHERE run_id=?1 AND kind='run.paused'",
            [&run.id],
        )?;
        store.connection.execute(
            "UPDATE pause_clock SET paused_at=1010 WHERE run_id=?1",
            [&run.id],
        )?;
        assert_eq!(store.execution_elapsed_seconds(&run.id)?, 10);
        for _ in 0..1100 {
            store.event(&run.id, "telemetry", json!({}))?;
        }
        store.maintain_history(&run.id)?;
        store
            .connection
            .execute("DELETE FROM pause_clock WHERE run_id=?1", [&run.id])?;
        drop(store);
        let mut store = Store::open(&root)?;
        assert_eq!(store.execution_elapsed_seconds(&run.id)?, 10);
        assert_eq!(crate::kernel::remaining_seconds(&store, &run)?, 50);
        let budget = crate::control::view(&store, &run.id, "budget", None)?;
        assert_eq!(budget["wall_seconds"]["used"], 10);
        assert_eq!(budget["wall_seconds"]["remaining"], 50);
        store.resume_paused(&run.id)?;
        assert!((49..=50).contains(&crate::kernel::remaining_seconds(&store, &run)?));
        drop(store);
        let mut store = Store::open(&root)?;
        store.resume_paused(&run.id)?;
        assert!((49..=50).contains(&crate::kernel::remaining_seconds(&store, &run)?));
        // Real active time still exhausts the original allowance.
        store.connection.execute(
            "UPDATE run_projection SET started_at=started_at-51 WHERE run_id=?1",
            [&run.id],
        )?;
        assert_eq!(crate::kernel::remaining_seconds(&store, &run)?, 0);
        assert_eq!(store.run(&run.id)?.budgets, run.budgets);
        store.state(&run.id, "failed", json!({"reason":"budget exhausted"}))?;
        let elapsed = store.execution_elapsed_seconds(&run.id)?;
        let transaction = store.connection.unchecked_transaction()?;
        track_clock(
            &transaction,
            &run.id,
            "run.failed",
            crate::storage::unix_time() + 1000,
        )?;
        transaction.commit()?;
        assert_eq!(store.execution_elapsed_seconds(&run.id)?, elapsed);
        Ok(())
    }
}
