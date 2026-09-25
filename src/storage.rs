use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub task: String,
    pub workspace: String,
    pub provider: String,
    pub grants: Value,
    pub budgets: Value,
    pub acceptance: String,
    pub state: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub seq: i64,
    pub kind: String,
    pub payload: Value,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Operation {
    pub id: String,
    pub run_id: String,
    pub capability: String,
    pub arguments: Value,
    pub idempotency_key: String,
    pub retry_safe: bool,
    pub state: String,
    pub artifact: Option<String>,
}

pub struct Store {
    connection: Connection,
    artifacts: PathBuf,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock predates Unix epoch")
        .as_secs() as i64
}

fn append_event(
    transaction: &Transaction<'_>,
    run_id: &str,
    kind: &str,
    payload: Value,
) -> Result<()> {
    transaction.execute(
        "INSERT INTO events(run_id, seq, kind, payload, created_at) VALUES (?1, (SELECT COALESCE(MAX(seq), 0) + 1 FROM events WHERE run_id = ?1), ?2, ?3, ?4)",
        params![run_id, kind, payload.to_string(), now()],
    )?;
    Ok(())
}

impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)?;
        let artifacts = root.join("artifacts");
        fs::create_dir_all(&artifacts)?;
        let connection = Connection::open(root.join("runs.sqlite"))?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=FULL;
             PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS runs (
               id TEXT PRIMARY KEY, task TEXT NOT NULL, workspace TEXT NOT NULL,
               provider TEXT NOT NULL, grants TEXT NOT NULL, budgets TEXT NOT NULL,
               acceptance TEXT NOT NULL, state TEXT NOT NULL, created_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS events (
               run_id TEXT NOT NULL REFERENCES runs(id), seq INTEGER NOT NULL,
               kind TEXT NOT NULL, payload TEXT NOT NULL, created_at INTEGER NOT NULL,
               PRIMARY KEY(run_id, seq)
             );
             CREATE TABLE IF NOT EXISTS operations (
               id TEXT PRIMARY KEY, run_id TEXT NOT NULL REFERENCES runs(id),
               capability TEXT NOT NULL, arguments TEXT NOT NULL,
               idempotency_key TEXT NOT NULL, retry_safe INTEGER NOT NULL,
               state TEXT NOT NULL, artifact TEXT,
               UNIQUE(run_id, idempotency_key)
             );
             CREATE TABLE IF NOT EXISTS artifacts (
               hash TEXT PRIMARY KEY, bytes INTEGER NOT NULL
             );",
        )?;
        Ok(Self {
            connection,
            artifacts,
        })
    }

    pub fn create_run(
        &mut self,
        task: &str,
        workspace: &Path,
        provider: &str,
        grants: Value,
        budgets: Value,
        acceptance: &str,
    ) -> Result<Run> {
        let workspace = workspace
            .canonicalize()
            .context("workspace does not exist")?;
        let run = Run {
            id: Uuid::new_v4().to_string(),
            task: task.to_owned(),
            workspace: workspace.to_string_lossy().into_owned(),
            provider: provider.to_owned(),
            grants,
            budgets,
            acceptance: acceptance.to_owned(),
            state: "ready".into(),
            created_at: now(),
        };
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO runs VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                run.id,
                run.task,
                run.workspace,
                run.provider,
                run.grants.to_string(),
                run.budgets.to_string(),
                run.acceptance,
                run.state,
                run.created_at
            ],
        )?;
        append_event(
            &transaction,
            &run.id,
            "run.created",
            json!({"task": run.task, "provider": run.provider}),
        )?;
        transaction.commit()?;
        Ok(run)
    }

    pub fn run(&self, id: &str) -> Result<Run> {
        self.connection.query_row(
            "SELECT id, task, workspace, provider, grants, budgets, acceptance, state, created_at FROM runs WHERE id = ?1",
            [id],
            |row| {
                let grants: String = row.get(4)?;
                let budgets: String = row.get(5)?;
                Ok(Run {
                    id: row.get(0)?, task: row.get(1)?, workspace: row.get(2)?,
                    provider: row.get(3)?, grants: serde_json::from_str(&grants).map_err(|err| rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(err)))?,
                    budgets: serde_json::from_str(&budgets).map_err(|err| rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(err)))?,
                    acceptance: row.get(6)?, state: row.get(7)?, created_at: row.get(8)?,
                })
            },
        ).with_context(|| format!("run {id} not found"))
    }

    pub fn runs(&self) -> Result<Vec<Run>> {
        let mut statement = self
            .connection
            .prepare("SELECT id FROM runs ORDER BY created_at DESC")?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.iter().map(|id| self.run(id)).collect()
    }

    pub fn events(&self, run_id: &str) -> Result<Vec<Event>> {
        let mut statement = self.connection.prepare(
            "SELECT seq, kind, payload, created_at FROM events WHERE run_id = ?1 ORDER BY seq",
        )?;
        let rows = statement.query_map([run_id], |row| {
            let payload: String = row.get(2)?;
            Ok(Event {
                seq: row.get(0)?,
                kind: row.get(1)?,
                payload: serde_json::from_str(&payload).map_err(|err| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Text,
                        Box::new(err),
                    )
                })?,
                created_at: row.get(3)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn event(&mut self, run_id: &str, kind: &str, payload: Value) -> Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        append_event(&transaction, run_id, kind, payload)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn state(&mut self, run_id: &str, state: &str, payload: Value) -> Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "UPDATE runs SET state = ?2 WHERE id = ?1",
            params![run_id, state],
        )?;
        append_event(&transaction, run_id, &format!("run.{state}"), payload)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn begin_operation(
        &mut self,
        run_id: &str,
        capability: &str,
        arguments: Value,
        retry_safe: bool,
    ) -> Result<Operation> {
        let operation = Operation {
            id: Uuid::new_v4().to_string(),
            run_id: run_id.into(),
            capability: capability.into(),
            arguments,
            idempotency_key: Uuid::new_v4().to_string(),
            retry_safe,
            state: "pending".into(),
            artifact: None,
        };
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO operations VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL)",
            params![
                operation.id,
                run_id,
                capability,
                operation.arguments.to_string(),
                operation.idempotency_key,
                retry_safe,
                operation.state
            ],
        )?;
        append_event(
            &transaction,
            run_id,
            "operation.pending",
            json!({"id": operation.id, "capability": capability, "arguments": operation.arguments, "idempotency_key": operation.idempotency_key}),
        )?;
        transaction.commit()?;
        Ok(operation)
    }

    pub fn operation_state(
        &mut self,
        operation: &Operation,
        state: &str,
        artifact: Option<&str>,
        detail: Value,
    ) -> Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "UPDATE operations SET state = ?2, artifact = COALESCE(?3, artifact) WHERE id = ?1",
            params![operation.id, state, artifact],
        )?;
        append_event(
            &transaction,
            &operation.run_id,
            &format!("operation.{state}"),
            json!({"id": operation.id, "artifact": artifact, "detail": detail}),
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn unresolved(&self, run_id: &str) -> Result<Vec<Operation>> {
        let mut statement = self.connection.prepare("SELECT id, capability, arguments, idempotency_key, retry_safe, state, artifact FROM operations WHERE run_id = ?1 AND state IN ('pending', 'dispatched') ORDER BY rowid")?;
        let rows = statement.query_map([run_id], |row| {
            let arguments: String = row.get(2)?;
            Ok(Operation {
                id: row.get(0)?,
                run_id: run_id.into(),
                capability: row.get(1)?,
                arguments: serde_json::from_str(&arguments).map_err(|err| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Text,
                        Box::new(err),
                    )
                })?,
                idempotency_key: row.get(3)?,
                retry_safe: row.get(4)?,
                state: row.get(5)?,
                artifact: row.get(6)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn reconcile(&mut self, run_id: &str) -> Result<Vec<Operation>> {
        let unresolved = self.unresolved(run_id)?;
        for operation in &unresolved {
            if operation.retry_safe {
                self.event(
                    run_id,
                    "operation.retry_ready",
                    json!({"id": operation.id, "idempotency_key": operation.idempotency_key}),
                )?;
            } else {
                self.operation_state(
                    operation,
                    "outcome_unknown",
                    None,
                    json!({"reason": "interrupted non-idempotent operation"}),
                )?;
                self.state(
                    run_id,
                    "waiting_recovery",
                    json!({"operation": operation.id}),
                )?;
            }
        }
        Ok(unresolved)
    }

    pub fn put_artifact(&mut self, bytes: &[u8]) -> Result<String> {
        let hash = hex::encode(Sha256::digest(bytes));
        let path = self.artifacts.join(&hash);
        if !path.exists() {
            let temporary = self.artifacts.join(format!(".{}-{}", hash, Uuid::new_v4()));
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            if path.exists() {
                fs::remove_file(&temporary)?;
            } else {
                fs::rename(&temporary, &path)?;
            }
        }
        self.connection.execute(
            "INSERT OR IGNORE INTO artifacts(hash, bytes) VALUES (?1, ?2)",
            params![hash, bytes.len() as i64],
        )?;
        Ok(hash)
    }

    pub fn artifact(&self, hash: &str) -> Result<Vec<u8>> {
        if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("invalid artifact handle");
        }
        let exists: Option<i64> = self
            .connection
            .query_row(
                "SELECT bytes FROM artifacts WHERE hash = ?1",
                [hash],
                |row| row.get(0),
            )
            .optional()?;
        if exists.is_none() {
            bail!("artifact not recorded");
        }
        let mut bytes = Vec::new();
        File::open(self.artifacts.join(hash))?.read_to_end(&mut bytes)?;
        if hex::encode(Sha256::digest(&bytes)) != hash {
            bail!("artifact integrity check failed");
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reopens_with_ordered_events_and_artifacts() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "repair",
            directory.path(),
            "codex",
            json!(["workspace.read"]),
            json!({"actions": 10}),
            "tests pass",
        )?;
        let operation =
            store.begin_operation(&run.id, "workspace.read", json!({"path": "a"}), true)?;
        store.operation_state(&operation, "dispatched", None, json!({}))?;
        let hash = store.put_artifact(b"large output")?;
        store.operation_state(&operation, "succeeded", Some(&hash), json!({"bytes": 12}))?;
        drop(store);
        let store = Store::open(directory.path())?;
        assert_eq!(
            store
                .events(&run.id)?
                .iter()
                .map(|event| event.seq)
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );
        assert_eq!(store.artifact(&hash)?, b"large output");
        assert!(store.unresolved(&run.id)?.is_empty());
        Ok(())
    }

    #[test]
    fn recovery_never_retries_unsafe_operation() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "publish",
            directory.path(),
            "grok",
            json!([]),
            json!({}),
            "receipt",
        )?;
        let safe = store.begin_operation(&run.id, "workspace.read", json!({}), true)?;
        let unsafe_operation =
            store.begin_operation(&run.id, "external.write", json!({}), false)?;
        store.operation_state(&unsafe_operation, "dispatched", None, json!({}))?;
        drop(store);
        let mut store = Store::open(directory.path())?;
        assert_eq!(store.reconcile(&run.id)?.len(), 2);
        assert_eq!(store.unresolved(&run.id)?.len(), 1);
        assert_eq!(store.unresolved(&run.id)?[0].id, safe.id);
        assert_eq!(store.run(&run.id)?.state, "waiting_recovery");
        Ok(())
    }
}
