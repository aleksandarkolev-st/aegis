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

use crate::mcp::{Server as McpServer, Tool as McpTool};
use crate::model::{Checkpoint as Handoff, Milestone};

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
    pub capability_version: u32,
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

pub fn unix_time() -> i64 {
    now()
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
               capability TEXT NOT NULL, capability_version INTEGER NOT NULL DEFAULT 1,
               arguments TEXT NOT NULL,
               idempotency_key TEXT NOT NULL, retry_safe INTEGER NOT NULL,
               state TEXT NOT NULL, artifact TEXT,
               UNIQUE(run_id, idempotency_key)
             );
             CREATE TABLE IF NOT EXISTS artifacts (
               hash TEXT PRIMARY KEY, bytes INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS milestones (
               run_id TEXT NOT NULL REFERENCES runs(id), position INTEGER NOT NULL,
               title TEXT NOT NULL, state TEXT NOT NULL, evidence TEXT NOT NULL,
               PRIMARY KEY(run_id, position)
             );
             CREATE TABLE IF NOT EXISTS activated (
               run_id TEXT NOT NULL REFERENCES runs(id), capability TEXT NOT NULL,
               version INTEGER NOT NULL, PRIMARY KEY(run_id, capability)
             );
             CREATE TABLE IF NOT EXISTS mcp_servers (
               name TEXT PRIMARY KEY, command TEXT NOT NULL, args TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS mcp_tools (
               server TEXT NOT NULL REFERENCES mcp_servers(name), name TEXT NOT NULL,
               description TEXT NOT NULL, input_schema TEXT NOT NULL, version INTEGER NOT NULL,
               PRIMARY KEY(server, name)
             );
             CREATE TABLE IF NOT EXISTS operation_artifacts (
               operation_id TEXT NOT NULL REFERENCES operations(id),
               hash TEXT NOT NULL REFERENCES artifacts(hash), kind TEXT NOT NULL,
               PRIMARY KEY(operation_id, hash)
             );",
        )?;
        let schema_version: i64 =
            connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if schema_version == 0 {
            connection.execute_batch(
                "INSERT OR IGNORE INTO activated(run_id, capability, version)
                 SELECT run_id, json_extract(payload, '$.id'), COALESCE(json_extract(payload, '$.version'), 1)
                 FROM events WHERE kind = 'capability.activated';
                 PRAGMA user_version=1;"
            )?;
        }
        if schema_version < 2 {
            let mut statement = connection.prepare("PRAGMA table_info(operations)")?;
            let columns = statement
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if !columns.iter().any(|column| column == "capability_version") {
                connection.execute_batch("ALTER TABLE operations ADD COLUMN capability_version INTEGER NOT NULL DEFAULT 1")?;
            }
            connection.execute_batch("PRAGMA user_version=2")?;
        }
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
        let workspace = dunce::canonicalize(workspace).context("workspace does not exist")?;
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
        transaction.execute(
            "INSERT INTO milestones VALUES (?1, 0, 'Task request', 'active', '[]')",
            [&run.id],
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

    pub fn recent_events(&self, run_id: &str, limit: i64) -> Result<Vec<Event>> {
        let mut statement = self.connection.prepare("SELECT seq, kind, payload, created_at FROM events WHERE run_id = ?1 ORDER BY seq DESC LIMIT ?2")?;
        let rows = statement.query_map(params![run_id, limit.max(0)], |row| {
            let payload: String = row.get(2)?;
            Ok(Event {
                seq: row.get(0)?,
                kind: row.get(1)?,
                payload: serde_json::from_str(&payload).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?,
                created_at: row.get(3)?,
            })
        })?;
        let mut events = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        events.reverse();
        Ok(events)
    }

    pub fn events_since(&self, run_id: &str, seq: i64) -> Result<Vec<Event>> {
        let mut statement = self.connection.prepare("SELECT seq, kind, payload, created_at FROM events WHERE run_id = ?1 AND seq > ?2 ORDER BY seq")?;
        let rows = statement.query_map(params![run_id, seq], |row| {
            let payload: String = row.get(2)?;
            Ok(Event {
                seq: row.get(0)?,
                kind: row.get(1)?,
                payload: serde_json::from_str(&payload).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?,
                created_at: row.get(3)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn event_count(&self, run_id: &str, kind: &str) -> Result<i64> {
        Ok(self.connection.query_row(
            "SELECT COUNT(*) FROM events WHERE run_id = ?1 AND kind = ?2",
            params![run_id, kind],
            |row| row.get(0),
        )?)
    }

    pub fn model_tokens(&self, run_id: &str) -> Result<u64> {
        let total: i64 = self.connection.query_row(
            "SELECT COALESCE(SUM(COALESCE(json_extract(payload, '$.usage.input_tokens'), 0) + COALESCE(json_extract(payload, '$.usage.output_tokens'), 0)), 0) FROM events WHERE run_id = ?1 AND kind = 'model.response'",
            [run_id], |row| row.get(0)
        )?;
        Ok(total.max(0) as u64)
    }

    pub fn run_started_at(&self, run_id: &str) -> Result<Option<i64>> {
        Ok(self.connection.query_row(
            "SELECT MIN(created_at) FROM events WHERE run_id = ?1 AND kind = 'run.running'",
            [run_id],
            |row| row.get(0),
        )?)
    }

    pub fn run_summary(&self, run_id: &str) -> Result<Option<String>> {
        Ok(self.connection.query_row(
            "SELECT substr(json_extract(payload, '$.summary'), 1, 4000) FROM events WHERE run_id = ?1 AND kind = 'run.completed' ORDER BY seq DESC LIMIT 1",
            [run_id],
            |row| row.get(0),
        ).optional()?)
    }

    pub fn activate(&mut self, run_id: &str, capability: &str, version: u32) -> Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute("INSERT INTO activated VALUES (?1, ?2, ?3) ON CONFLICT(run_id, capability) DO UPDATE SET version = excluded.version", params![run_id, capability, version])?;
        append_event(
            &transaction,
            run_id,
            "capability.activated",
            json!({"id": capability, "version": version}),
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn active_capabilities(&self, run_id: &str) -> Result<Vec<(String, u32)>> {
        let mut statement = self.connection.prepare(
            "SELECT capability, version FROM activated WHERE run_id = ?1 ORDER BY capability",
        )?;
        let rows = statement.query_map([run_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn register_mcp(&mut self, server: &McpServer, tools: &[McpTool]) -> Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute("INSERT INTO mcp_servers VALUES (?1, ?2, ?3) ON CONFLICT(name) DO UPDATE SET command = excluded.command, args = excluded.args",
            params![server.name, server.command, serde_json::to_string(&server.args)?])?;
        transaction.execute("DELETE FROM mcp_tools WHERE server = ?1", [&server.name])?;
        for tool in tools {
            if tool.server != server.name {
                bail!("MCP tool belongs to a different server");
            }
            transaction.execute(
                "INSERT INTO mcp_tools VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    server.name,
                    tool.name,
                    tool.description,
                    tool.input_schema.to_string(),
                    tool.version
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn mcp_server(&self, name: &str) -> Result<McpServer> {
        self.connection
            .query_row(
                "SELECT command, args FROM mcp_servers WHERE name = ?1",
                [name],
                |row| {
                    let args: String = row.get(1)?;
                    Ok(McpServer {
                        name: name.into(),
                        command: row.get(0)?,
                        args: serde_json::from_str(&args).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                1,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        })?,
                    })
                },
            )
            .with_context(|| format!("MCP server {name} is not registered"))
    }

    pub fn mcp_tools(&self) -> Result<Vec<McpTool>> {
        let mut statement = self.connection.prepare("SELECT server, name, description, input_schema, version FROM mcp_tools ORDER BY server, name")?;
        let rows = statement.query_map([], |row| {
            let schema: String = row.get(3)?;
            Ok(McpTool {
                server: row.get(0)?,
                name: row.get(1)?,
                description: row.get(2)?,
                input_schema: serde_json::from_str(&schema).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        3,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?,
                version: row.get(4)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn has_evidence(&self, run_id: &str, hash: &str) -> Result<bool> {
        let count: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM operations AS operation WHERE operation.run_id = ?1 AND operation.state = 'succeeded' AND (operation.artifact = ?2 OR EXISTS (SELECT 1 FROM operation_artifacts AS linked WHERE linked.operation_id = operation.id AND linked.hash = ?2))",
            params![run_id, hash], |row| row.get(0)
        )?;
        Ok(count > 0)
    }

    pub fn link_artifact(&mut self, operation_id: &str, hash: &str, kind: &str) -> Result<()> {
        self.connection.execute(
            "INSERT OR IGNORE INTO operation_artifacts VALUES (?1, ?2, ?3)",
            params![operation_id, hash, kind],
        )?;
        Ok(())
    }

    pub fn milestones(&self, run_id: &str) -> Result<Vec<Milestone>> {
        let mut statement = self.connection.prepare(
            "SELECT title, state, evidence FROM milestones WHERE run_id = ?1 ORDER BY position",
        )?;
        let rows = statement.query_map([run_id], |row| {
            let evidence: String = row.get(2)?;
            Ok(Milestone {
                title: row.get(0)?,
                state: row.get(1)?,
                evidence: serde_json::from_str(&evidence).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn last_checkpoint(&self, run_id: &str) -> Result<Option<Handoff>> {
        let hash: Option<String> = self.connection.query_row(
            "SELECT json_extract(payload, '$.artifact') FROM events WHERE run_id = ?1 AND kind = 'checkpoint.created' ORDER BY seq DESC LIMIT 1",
            [run_id], |row| row.get(0)
        ).optional()?;
        hash.map(|hash| serde_json::from_slice(&self.artifact(&hash)?).map_err(Into::into))
            .transpose()
    }

    pub fn save_checkpoint(&mut self, run_id: &str, checkpoint: &Handoff) -> Result<String> {
        if checkpoint.milestones.len() > 20
            || checkpoint.next_action.len() > 1000
            || checkpoint.decisions.len() > 20
            || checkpoint.unresolved.len() > 20
        {
            bail!("checkpoint exceeds limits");
        }
        for milestone in &checkpoint.milestones {
            if milestone.title.trim().is_empty()
                || milestone.title.len() > 200
                || !matches!(milestone.state.as_str(), "pending" | "active" | "completed")
            {
                bail!("invalid milestone");
            }
            if milestone.state == "completed" && milestone.evidence.is_empty() {
                bail!("completed milestone requires evidence");
            }
            for hash in &milestone.evidence {
                if !self.has_evidence(run_id, hash)? {
                    bail!("milestone evidence not from this run: {hash}");
                }
            }
        }
        let hash = self.put_artifact(&serde_json::to_vec(checkpoint)?)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !checkpoint.milestones.is_empty() {
            transaction.execute("DELETE FROM milestones WHERE run_id = ?1", [run_id])?;
            for (position, milestone) in checkpoint.milestones.iter().enumerate() {
                transaction.execute(
                    "INSERT INTO milestones VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        run_id,
                        position as i64,
                        milestone.title,
                        milestone.state,
                        serde_json::to_string(&milestone.evidence)?
                    ],
                )?;
            }
        }
        append_event(
            &transaction,
            run_id,
            "checkpoint.created",
            json!({"artifact": hash, "next_action": checkpoint.next_action}),
        )?;
        transaction.commit()?;
        Ok(hash)
    }

    pub fn complete_run(&mut self, run_id: &str, summary: &str, evidence: &[String]) -> Result<()> {
        if evidence.is_empty() {
            bail!("completion requires evidence");
        }
        for hash in evidence {
            if !self.has_evidence(run_id, hash)? {
                bail!("completion evidence is not a successful operation artifact: {hash}");
            }
        }
        let milestones = self.milestones(run_id)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state: String =
            transaction.query_row("SELECT state FROM runs WHERE id = ?1", [run_id], |row| {
                row.get(0)
            })?;
        if state != "running" {
            bail!("only a running run can complete");
        }
        if milestones.len() == 1 && milestones[0].title == "Task request" {
            transaction.execute(
                "UPDATE milestones SET state = 'completed', evidence = ?2 WHERE run_id = ?1",
                params![run_id, serde_json::to_string(evidence)?],
            )?;
        } else if milestones
            .iter()
            .any(|milestone| milestone.state != "completed")
        {
            bail!("all planned milestones must have evidence before completion");
        }
        transaction.execute(
            "UPDATE runs SET state = 'completed' WHERE id = ?1",
            [run_id],
        )?;
        append_event(
            &transaction,
            run_id,
            "run.completed",
            json!({"summary": summary, "evidence": evidence}),
        )?;
        transaction.commit()?;
        Ok(())
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
        let previous: String =
            transaction.query_row("SELECT state FROM runs WHERE id = ?1", [run_id], |row| {
                row.get(0)
            })?;
        if matches!(previous.as_str(), "completed" | "cancelled" | "failed") {
            bail!("terminal run cannot change state");
        }
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
        self.begin_operation_versioned(run_id, capability, 1, arguments, retry_safe)
    }

    pub fn begin_operation_versioned(
        &mut self,
        run_id: &str,
        capability: &str,
        capability_version: u32,
        arguments: Value,
        retry_safe: bool,
    ) -> Result<Operation> {
        let operation = Operation {
            id: Uuid::new_v4().to_string(),
            run_id: run_id.into(),
            capability: capability.into(),
            capability_version,
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
            "INSERT INTO operations(id, run_id, capability, capability_version, arguments, idempotency_key, retry_safe, state, artifact) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL)",
            params![
                operation.id,
                run_id,
                capability,
                capability_version,
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
            json!({"id": operation.id, "capability": capability, "version": capability_version, "arguments": operation.arguments, "idempotency_key": operation.idempotency_key}),
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

    pub fn claim_operation(&mut self, operation: &Operation) -> Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE operations SET state = 'executing' WHERE id = ?1 AND run_id = ?2 AND state = 'dispatched' AND EXISTS (SELECT 1 FROM runs WHERE id = ?2 AND state = 'running')",
            params![operation.id, operation.run_id],
        )?;
        if changed != 1 {
            bail!("operation has already been claimed or is no longer dispatched");
        }
        append_event(
            &transaction,
            &operation.run_id,
            "operation.executing",
            json!({"id":operation.id,"worker_pid":std::process::id()}),
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn operations(&self, run_id: &str) -> Result<Vec<Operation>> {
        let mut statement = self.connection.prepare("SELECT id, capability, capability_version, arguments, idempotency_key, retry_safe, state, artifact FROM operations WHERE run_id = ?1 ORDER BY rowid")?;
        let rows = statement.query_map([run_id], |row| {
            let arguments: String = row.get(3)?;
            Ok(Operation {
                id: row.get(0)?,
                run_id: run_id.into(),
                capability: row.get(1)?,
                capability_version: row.get(2)?,
                arguments: serde_json::from_str(&arguments).map_err(|err| {
                    rusqlite::Error::FromSqlConversionFailure(
                        3,
                        rusqlite::types::Type::Text,
                        Box::new(err),
                    )
                })?,
                idempotency_key: row.get(4)?,
                retry_safe: row.get(5)?,
                state: row.get(6)?,
                artifact: row.get(7)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn unknown_count(&self, run_id: &str) -> Result<i64> {
        Ok(self.connection.query_row(
            "SELECT COUNT(*) FROM operations WHERE run_id = ?1 AND state = 'outcome_unknown'",
            [run_id],
            |row| row.get(0),
        )?)
    }

    pub fn unresolved(&self, run_id: &str) -> Result<Vec<Operation>> {
        Ok(self
            .operations(run_id)?
            .into_iter()
            .filter(|operation| {
                matches!(
                    operation.state.as_str(),
                    "pending" | "dispatched" | "executing"
                )
            })
            .collect())
    }

    pub fn evidence_artifacts(&self, run_id: &str) -> Result<Vec<(String, String)>> {
        let mut statement = self.connection.prepare(
            "SELECT capability, artifact FROM operations WHERE run_id = ?1 AND state = 'succeeded' AND artifact IS NOT NULL UNION SELECT operation.capability || ' ' || linked.kind, linked.hash FROM operations AS operation JOIN operation_artifacts AS linked ON linked.operation_id = operation.id WHERE operation.run_id = ?1 AND operation.state = 'succeeded' ORDER BY 1, 2"
        )?;
        let rows = statement.query_map([run_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn resolve_unknown(
        &mut self,
        run_id: &str,
        operation_id: &str,
        succeeded: bool,
        note: &str,
    ) -> Result<()> {
        let operation = self.operation(operation_id)?;
        if operation.run_id != run_id || operation.state != "outcome_unknown" {
            bail!("operation is not an unknown outcome for this run");
        }
        if note.trim().is_empty() {
            bail!("reconciliation requires a note or external receipt");
        }
        let artifact = self.put_artifact(note.as_bytes())?;
        self.operation_state(
            &operation,
            if succeeded { "succeeded" } else { "failed" },
            Some(&artifact),
            json!({"reconciled_by": "user", "note_artifact": artifact}),
        )?;
        if self.unknown_count(run_id)? == 0
            && !matches!(
                self.run(run_id)?.state.as_str(),
                "completed" | "cancelled" | "failed"
            )
        {
            self.state(run_id, "ready", json!({"reconciled": operation_id}))?;
        }
        Ok(())
    }

    pub fn operation(&self, id: &str) -> Result<Operation> {
        self.connection.query_row(
            "SELECT run_id, capability, capability_version, arguments, idempotency_key, retry_safe, state, artifact FROM operations WHERE id = ?1",
            [id],
            |row| {
                let arguments: String = row.get(3)?;
                Ok(Operation {
                    id: id.into(), run_id: row.get(0)?, capability: row.get(1)?,
                    capability_version: row.get(2)?,
                    arguments: serde_json::from_str(&arguments).map_err(|err| rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(err)))?,
                    idempotency_key: row.get(4)?, retry_safe: row.get(5)?,
                    state: row.get(6)?, artifact: row.get(7)?,
                })
            },
        ).with_context(|| format!("operation {id} not found"))
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
    fn execution_clock_starts_at_first_running_event() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "queued",
            directory.path(),
            "codex",
            json!([]),
            json!({}),
            "",
        )?;
        assert_eq!(store.run_started_at(&run.id)?, None);
        store.state(&run.id, "running", json!({}))?;
        store.connection.execute(
            "UPDATE events SET created_at = 100 WHERE run_id = ?1 AND kind = 'run.running'",
            [&run.id],
        )?;
        store.state(&run.id, "waiting_recovery", json!({}))?;
        store.state(&run.id, "running", json!({}))?;
        assert_eq!(store.run_started_at(&run.id)?, Some(100));
        Ok(())
    }

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

    #[test]
    fn manual_reconciliation_requires_receipt_and_unlocks_run() -> Result<()> {
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
        let operation = store.begin_operation(&run.id, "external.write", json!({}), false)?;
        store.operation_state(&operation, "dispatched", None, json!({}))?;
        store.reconcile(&run.id)?;
        assert!(
            store
                .resolve_unknown(&run.id, &operation.id, true, "")
                .is_err()
        );
        store.resolve_unknown(&run.id, &operation.id, true, "external receipt 123")?;
        assert_eq!(store.unknown_count(&run.id)?, 0);
        assert_eq!(store.run(&run.id)?.state, "ready");
        assert!(store.has_evidence(
            &run.id,
            store.operation(&operation.id)?.artifact.as_deref().unwrap()
        )?);
        assert!(
            store
                .resolve_unknown(&run.id, &operation.id, true, "duplicate")
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn completed_run_cannot_be_cancelled() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run("done", directory.path(), "codex", json!([]), json!({}), "")?;
        store.state(&run.id, "completed", json!({}))?;
        assert!(store.state(&run.id, "cancelled", json!({})).is_err());
        assert_eq!(store.run(&run.id)?.state, "completed");
        Ok(())
    }

    #[test]
    fn checkpoint_survives_restart_and_gates_completion() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "repair",
            directory.path(),
            "codex",
            json!([]),
            json!({}),
            "test passes",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let operation = store.begin_operation(&run.id, "workspace.read", json!({}), true)?;
        let evidence = store.put_artifact(b"observed result")?;
        store.operation_state(&operation, "succeeded", Some(&evidence), json!({}))?;
        let checkpoint = Handoff {
            decisions: vec!["reproduce first".into()],
            unresolved: vec!["test pending".into()],
            next_action: "run test".into(),
            milestones: vec![
                Milestone {
                    title: "Inspect".into(),
                    state: "completed".into(),
                    evidence: vec![evidence.clone()],
                },
                Milestone {
                    title: "Test".into(),
                    state: "pending".into(),
                    evidence: vec![],
                },
            ],
        };
        store.save_checkpoint(&run.id, &checkpoint)?;
        assert!(
            store
                .complete_run(&run.id, "done", &[evidence.clone()])
                .is_err()
        );
        drop(store);
        let mut store = Store::open(directory.path())?;
        assert_eq!(store.last_checkpoint(&run.id)?, Some(checkpoint.clone()));
        let mut completed = checkpoint;
        completed.milestones[1].state = "completed".into();
        completed.milestones[1].evidence = vec![evidence.clone()];
        store.save_checkpoint(&run.id, &completed)?;
        store.complete_run(&run.id, "done", &[evidence])?;
        assert_eq!(store.run(&run.id)?.state, "completed");
        Ok(())
    }
}
