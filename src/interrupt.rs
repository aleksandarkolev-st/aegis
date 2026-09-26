use anyhow::{Result, bail};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::storage::{Store, append_event};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Operation,
    Model,
}

impl Scope {
    fn name(self) -> &'static str {
        match self {
            Self::Operation => "operation",
            Self::Model => "model",
        }
    }
}

impl Store {
    pub fn request_interrupt(&mut self, run_id: &str, scope: Scope) -> Result<Option<String>> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let state: String =
            transaction.query_row("SELECT state FROM runs WHERE id = ?1", [run_id], |row| {
                row.get(0)
            })?;
        if !matches!(state.as_str(), "ready" | "running") {
            bail!("task has no active work to interrupt");
        }
        let target: Option<String> = match scope {
            Scope::Operation => transaction.query_row("SELECT id FROM operations WHERE run_id = ?1 AND state IN ('pending','dispatched','executing') ORDER BY rowid DESC LIMIT 1", [run_id], |row| row.get(0)).optional()?,
            Scope::Model => transaction.query_row("SELECT CAST(seq AS TEXT) FROM events WHERE run_id = ?1 AND kind = 'model.started' AND seq > COALESCE((SELECT MAX(seq) FROM events WHERE run_id = ?1 AND kind IN ('model.response','model.failed')), 0) ORDER BY seq DESC LIMIT 1", [run_id], |row| row.get(0)).optional()?,
        };
        if let Some(target) = &target {
            let changed = transaction.execute(
                "INSERT OR IGNORE INTO interrupts(run_id, scope, target) VALUES (?1, ?2, ?3)",
                params![run_id, scope.name(), target],
            )?;
            if changed == 1 {
                append_event(
                    &transaction,
                    run_id,
                    "interrupt.requested",
                    json!({"scope":scope.name(),"target":target}),
                )?;
            }
        }
        transaction.commit()?;
        Ok(target)
    }

    pub fn interrupt_requested(&self, run_id: &str, scope: Scope, target: &str) -> Result<bool> {
        Ok(self.connection.query_row("SELECT EXISTS(SELECT 1 FROM interrupts WHERE run_id=?1 AND scope=?2 AND target=?3 AND pending=1)", params![run_id,scope.name(),target], |row| row.get(0))?)
    }

    pub(crate) fn acknowledge_interrupt(
        &mut self,
        run_id: &str,
        scope: Scope,
        target: &str,
    ) -> Result<()> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let changed = transaction.execute("UPDATE interrupts SET pending=0 WHERE run_id=?1 AND scope=?2 AND target=?3 AND pending=1", params![run_id,scope.name(),target])?;
        if changed > 0 {
            append_event(
                &transaction,
                run_id,
                "interrupt.acknowledged",
                json!({"scope":scope.name(),"target":target}),
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn current_model_target(&self, run_id: &str) -> Result<String> {
        Ok(self.connection.query_row("SELECT CAST(seq AS TEXT) FROM events WHERE run_id=?1 AND kind='model.started' ORDER BY seq DESC LIMIT 1", [run_id], |row| row.get(0))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_interrupts_target_one_started_turn_not_future_recovery_turns() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "interrupt",
            directory.path(),
            "codex",
            json!([]),
            json!({}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        assert!(store.request_interrupt(&run.id, Scope::Model)?.is_none());
        store.event(&run.id, "model.started", json!({"turn":1}))?;
        let target = store.request_interrupt(&run.id, Scope::Model)?.unwrap();
        assert_eq!(
            store.request_interrupt(&run.id, Scope::Model)?,
            Some(target.clone())
        );
        assert_eq!(store.event_count(&run.id, "interrupt.requested")?, 1);
        store.event(&run.id, "model.failed", json!({"error":"interrupted"}))?;
        assert!(store.request_interrupt(&run.id, Scope::Model)?.is_none());
        store.acknowledge_interrupt(&run.id, Scope::Model, &target)?;
        store.event(&run.id, "model.started", json!({"turn":1}))?;
        let next = store.current_model_target(&run.id)?;
        assert_ne!(next, target);
        assert!(!store.interrupt_requested(&run.id, Scope::Model, &next)?);
        assert_eq!(store.run(&run.id)?.state, "running");
        Ok(())
    }
}
