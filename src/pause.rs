//! Durable user pause requests, acknowledged only at a kernel safe boundary.
use crate::storage::{Store, append_event};
use anyhow::{Result, bail};
use rusqlite::params;
use serde_json::json;

impl Store {
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
    let checkpoint = store
        .last_checkpoint(id)?
        .unwrap_or(crate::model::Checkpoint {
            decisions: vec![],
            unresolved: vec!["User paused before a next action was checkpointed".into()],
            next_action: "Inspect saved run state and continue the original task".into(),
            milestones: store.milestones(id)?,
        });
    store.save_checkpoint(id, &checkpoint)?;
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
