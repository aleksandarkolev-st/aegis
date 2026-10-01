use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
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

impl Run {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.state.as_str(),
            "completed" | "answered" | "cancelled" | "failed"
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectMemory {
    pub id: String,
    pub text: String,
    pub updated_at: i64,
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
    pub(crate) connection: Connection,
    artifacts: PathBuf,
}

pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock predates Unix epoch")
        .as_secs() as i64
}

fn remove_task_caps(configuration: &mut Value) {
    if let Some(configuration) = configuration.as_object_mut() {
        for key in ["actions", "model_tokens", "tool_result_tokens"] {
            configuration.remove(key);
        }
    }
}

pub fn unix_time() -> i64 {
    now()
}

pub(crate) fn append_event(
    transaction: &Transaction<'_>,
    run_id: &str,
    kind: &str,
    payload: Value,
) -> Result<()> {
    let timestamp = now();
    if kind == "model.started" {
        let paused: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM pause_requests WHERE run_id=?1 AND pending=1)",
            [run_id],
            |row| row.get(0),
        )?;
        if paused {
            bail!("user pause pending before inference");
        }
    }
    if kind == "model.started" && payload.get("context_tokenizer").is_some() {
        if payload["context_tokenizer"] != crate::tokenization::ENCODING {
            bail!("model attempt uses an unsupported context tokenizer");
        }
        let charge = payload["tool_result_tokens"]
            .as_u64()
            .context("model attempt is missing measured tool-result tokens")?;
        let used: u64 = transaction.query_row(
            "SELECT COALESCE((SELECT tokens FROM tool_token_projection WHERE run_id=?1),0)",
            [run_id],
            |row| row.get(0),
        )?;
        let total = used
            .checked_add(charge)
            .context("tool-result token accounting overflow")?;
        if total > i64::MAX as u64 {
            bail!("tool-result token usage exceeds the storage bound");
        }
        transaction.execute(
            "INSERT INTO tool_token_projection(run_id,tokens) VALUES (?1,?2) ON CONFLICT(run_id) DO UPDATE SET tokens=excluded.tokens",
            params![run_id,total],
        )?;
    }
    transaction.execute(
        "INSERT OR IGNORE INTO run_projection(run_id) VALUES (?1)",
        [run_id],
    )?;
    transaction.execute(
        "INSERT INTO events(run_id, seq, kind, payload, created_at) VALUES (?1, (SELECT last_seq + 1 FROM run_projection WHERE run_id = ?1), ?2, ?3, ?4)",
        params![run_id, kind, payload.to_string(), timestamp],
    )?;
    let tokens = if matches!(kind, "model.response" | "model.failed") {
        payload["usage"]["input_tokens"]
            .as_u64()
            .unwrap_or(0)
            .saturating_add(payload["usage"]["output_tokens"].as_u64().unwrap_or(0))
            .min(i64::MAX as u64) as i64
    } else {
        0
    };
    transaction.execute("UPDATE run_projection SET last_seq = last_seq + 1,
        model_tokens = MIN(9223372036854775807, model_tokens + ?2),
        started_at = CASE WHEN ?3 = 'run.running' THEN COALESCE(started_at, ?4) ELSE started_at END,
        checkpoint = CASE WHEN ?3 = 'checkpoint.created' THEN ?5 ELSE checkpoint END,
        proposal = CASE WHEN ?3 = 'completion.proposed' THEN ?5 WHEN ?3 = 'completion.resolved' THEN NULL ELSE proposal END,
        summary = CASE WHEN ?3 IN ('run.completed', 'run.answered') THEN ?6 ELSE summary END WHERE run_id = ?1",
        params![run_id, tokens, kind, timestamp, payload["artifact"].as_str(), payload["summary"].as_str().map(|summary| summary.chars().take(4000).collect::<String>())])?;
    transaction.execute(
        "INSERT INTO event_counts(run_id, kind, count) VALUES (?1, ?2, 1)
        ON CONFLICT(run_id, kind) DO UPDATE SET count = count + 1",
        params![run_id, kind],
    )?;
    Ok(())
}

fn terminal_operation_state(state: &str) -> bool {
    matches!(
        state,
        "succeeded" | "failed" | "cancelled" | "outcome_unknown"
    )
}

fn validate_operation_transition(
    operation: &Operation,
    next: &str,
    allow_unknown_resolution: bool,
) -> Result<()> {
    if !matches!(
        next,
        "dispatched" | "succeeded" | "failed" | "cancelled" | "outcome_unknown"
    ) {
        bail!("invalid operation state");
    }

    if allow_unknown_resolution
        && operation.state == "outcome_unknown"
        && matches!(next, "succeeded" | "failed")
    {
        return Ok(());
    }

    if operation.state == "failed" && operation.retry_safe && next == "dispatched" {
        return Ok(());
    }

    match operation.state.as_str() {
        "pending" => {
            if !matches!(next, "dispatched" | "succeeded" | "failed" | "cancelled") {
                bail!("invalid operation state transition from pending");
            }
        }
        "dispatched" => {
            if !matches!(
                next,
                "dispatched" | "succeeded" | "failed" | "cancelled" | "outcome_unknown"
            ) || (next == "dispatched" && !operation.retry_safe)
            {
                bail!("invalid operation state transition from dispatched");
            }
        }
        "executing" => {
            if !matches!(
                next,
                "dispatched" | "succeeded" | "failed" | "cancelled" | "outcome_unknown"
            ) || (next == "dispatched" && !operation.retry_safe)
            {
                bail!("invalid operation state transition from executing");
            }
        }
        _ => bail!("operation is already terminal"),
    }

    if next == "succeeded" && operation.state != "executing" {
        bail!("an operation must be claimed before it can succeed");
    }
    if next == "outcome_unknown"
        && (operation.retry_safe || !matches!(operation.state.as_str(), "dispatched" | "executing"))
    {
        bail!("only a dispatched, non-retry-safe operation can have an unknown outcome");
    }
    Ok(())
}

pub(crate) fn insert_run(transaction: &Transaction<'_>, run: &Run) -> Result<()> {
    let mut budgets = run.budgets.clone();
    remove_task_caps(&mut budgets);
    transaction.execute(
        "INSERT INTO runs VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            run.id,
            run.task,
            run.workspace,
            run.provider,
            run.grants.to_string(),
            budgets.to_string(),
            run.acceptance,
            run.state,
            run.created_at
        ],
    )?;
    append_event(
        transaction,
        &run.id,
        "run.created",
        json!({"task":run.task,"provider":run.provider}),
    )?;
    transaction.execute(
        "INSERT INTO milestones VALUES (?1, 0, 'Task request', 'active', '[]')",
        [&run.id],
    )?;
    crate::obligations::insert(transaction, run)?;
    crate::habits::record(transaction, run)?;
    Ok(())
}

fn artifact_bytes(connection: &Connection, artifacts: &Path, hash: &str) -> Result<Vec<u8>> {
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid artifact handle");
    }
    let exists: Option<i64> = connection
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
    File::open(artifacts.join(hash))?.read_to_end(&mut bytes)?;
    if hex::encode(Sha256::digest(&bytes)) != hash {
        bail!("artifact integrity check failed");
    }
    Ok(bytes)
}

fn validate_completed_milestone_evidence(
    connection: &Connection,
    run_id: &str,
    milestones: &[Milestone],
) -> Result<()> {
    let revision: Option<i64> = connection
        .query_row(
            "SELECT revision FROM workspace_revisions WHERE run_id = ?1",
            [run_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(revision) = revision else {
        return Ok(());
    };
    for milestone in milestones.iter().filter(|item| item.state == "completed") {
        if milestone.evidence.is_empty() {
            bail!("completed milestone requires evidence");
        }
        for hash in &milestone.evidence {
            if !crate::obligations::current_evidence(connection, run_id, revision, hash)? {
                bail!(
                    "completed milestone evidence is no longer successful and current for this run"
                );
            }
        }
    }
    Ok(())
}

fn validate_completed_milestone_artifacts(
    connection: &Connection,
    artifacts: &Path,
    milestones: &[Milestone],
) -> Result<()> {
    for milestone in milestones.iter().filter(|item| item.state == "completed") {
        for hash in &milestone.evidence {
            artifact_bytes(connection, artifacts, hash)?;
        }
    }
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
             CREATE TABLE IF NOT EXISTS project_memory (
               id TEXT PRIMARY KEY, workspace TEXT NOT NULL, text TEXT NOT NULL,
               updated_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS project_memory_workspace ON project_memory(workspace);
             CREATE TABLE IF NOT EXISTS project_instructions (
               id TEXT PRIMARY KEY, workspace TEXT NOT NULL, scope TEXT NOT NULL,
               text TEXT NOT NULL, revision INTEGER NOT NULL, updated_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS project_instructions_workspace ON project_instructions(workspace);
             CREATE TABLE IF NOT EXISTS repository_reviews (
               workspace TEXT NOT NULL, path TEXT NOT NULL, scope TEXT NOT NULL,
               sha256 TEXT NOT NULL, text TEXT NOT NULL, revision INTEGER NOT NULL,
               approved INTEGER NOT NULL, PRIMARY KEY(workspace,path)
             );
             CREATE TABLE IF NOT EXISTS learning_settings (workspace TEXT PRIMARY KEY, enabled INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS user_habits (
               workspace TEXT NOT NULL, category TEXT NOT NULL, choice TEXT NOT NULL,
               observations INTEGER NOT NULL, confirmed INTEGER NOT NULL,
               last_run TEXT REFERENCES runs(id), updated_at INTEGER NOT NULL,
               PRIMARY KEY(workspace,category)
             );
             CREATE TABLE IF NOT EXISTS workflow_experience (
               run_id TEXT PRIMARY KEY REFERENCES runs(id), workspace TEXT NOT NULL,
               topics TEXT NOT NULL, steps TEXT NOT NULL, verifier TEXT NOT NULL,
               evidence TEXT NOT NULL REFERENCES artifacts(hash), created_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS workflow_experience_workspace ON workflow_experience(workspace);
             CREATE TABLE IF NOT EXISTS network_receipts (
               operation_id TEXT NOT NULL REFERENCES operations(id), attempt INTEGER NOT NULL,
               run_id TEXT NOT NULL REFERENCES runs(id), reserved INTEGER NOT NULL,
               received INTEGER NOT NULL, complete INTEGER NOT NULL,
               PRIMARY KEY(operation_id,attempt)
             );
             CREATE INDEX IF NOT EXISTS network_receipts_run ON network_receipts(run_id);
             CREATE TABLE IF NOT EXISTS tool_token_projection (
               run_id TEXT PRIMARY KEY REFERENCES runs(id), tokens INTEGER NOT NULL
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
        if schema_version < 3 {
            let mut statement = connection.prepare("PRAGMA table_info(mcp_servers)")?;
            let columns = statement
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if !columns.iter().any(|column| column == "policy") {
                connection.execute_batch(
                    "ALTER TABLE mcp_servers ADD COLUMN policy TEXT NOT NULL DEFAULT '{}'",
                )?;
            }
            connection.execute_batch("PRAGMA user_version=3")?;
        }
        if schema_version < 4 {
            connection.execute_batch("BEGIN IMMEDIATE;
                CREATE TABLE IF NOT EXISTS run_projection (
                    run_id TEXT PRIMARY KEY REFERENCES runs(id), last_seq INTEGER NOT NULL DEFAULT 0,
                    model_tokens INTEGER NOT NULL DEFAULT 0, started_at INTEGER, checkpoint TEXT,
                    proposal TEXT, summary TEXT
                );
                CREATE TABLE IF NOT EXISTS event_counts (
                    run_id TEXT NOT NULL REFERENCES runs(id), kind TEXT NOT NULL, count INTEGER NOT NULL,
                    PRIMARY KEY(run_id, kind)
                );
                INSERT OR IGNORE INTO run_projection(run_id, last_seq, model_tokens, started_at, checkpoint, proposal, summary)
                SELECT id,
                    COALESCE((SELECT MAX(seq) FROM events WHERE run_id = runs.id), 0),
                    COALESCE((SELECT SUM(COALESCE(json_extract(payload, '$.usage.input_tokens'), 0) + COALESCE(json_extract(payload, '$.usage.output_tokens'), 0)) FROM events WHERE run_id = runs.id AND kind = 'model.response'), 0),
                    (SELECT MIN(created_at) FROM events WHERE run_id = runs.id AND kind = 'run.running'),
                    (SELECT json_extract(payload, '$.artifact') FROM events WHERE run_id = runs.id AND kind = 'checkpoint.created' ORDER BY seq DESC LIMIT 1),
                    (SELECT json_extract(payload, '$.artifact') FROM events WHERE run_id = runs.id AND kind = 'completion.proposed' AND seq > COALESCE((SELECT MAX(seq) FROM events WHERE run_id = runs.id AND kind = 'completion.resolved'), 0) ORDER BY seq DESC LIMIT 1),
                    (SELECT substr(json_extract(payload, '$.summary'), 1, 4000) FROM events WHERE run_id = runs.id AND kind IN ('run.completed', 'run.answered') ORDER BY seq DESC LIMIT 1)
                FROM runs;
                INSERT OR IGNORE INTO event_counts SELECT run_id, kind, COUNT(*) FROM events GROUP BY run_id, kind;
                CREATE INDEX IF NOT EXISTS operations_run_state ON operations(run_id, state);
                PRAGMA user_version=4;
                COMMIT;")?;
        }
        if schema_version < 5 {
            connection.execute_batch(
                "BEGIN IMMEDIATE;
                CREATE TABLE IF NOT EXISTS run_snapshots (
                    run_id TEXT PRIMARY KEY REFERENCES runs(id), sequence INTEGER NOT NULL,
                    artifact TEXT NOT NULL REFERENCES artifacts(hash)
                );
                CREATE TABLE IF NOT EXISTS event_archives (
                    run_id TEXT NOT NULL REFERENCES runs(id), first_seq INTEGER NOT NULL,
                    last_seq INTEGER NOT NULL, artifact TEXT NOT NULL REFERENCES artifacts(hash),
                    PRIMARY KEY(run_id, last_seq)
                );
                PRAGMA user_version=5;
                COMMIT;",
            )?;
        }
        if schema_version < 6 {
            connection.execute_batch(
                "BEGIN IMMEDIATE;
                CREATE TABLE IF NOT EXISTS interrupts (
                    run_id TEXT NOT NULL REFERENCES runs(id), scope TEXT NOT NULL,
                    target TEXT NOT NULL, pending INTEGER NOT NULL DEFAULT 1,
                    PRIMARY KEY(run_id, scope, target)
                );
                PRAGMA user_version=6;
                COMMIT;",
            )?;
        }
        let store = Self {
            connection,
            artifacts,
        };
        if schema_version < 7 {
            store.migrate_failed_model_usage()?;
        }
        if schema_version < 8 {
            store.connection.execute_batch(
                "BEGIN IMMEDIATE;
                CREATE TABLE IF NOT EXISTS workspace_revisions (
                    run_id TEXT PRIMARY KEY REFERENCES runs(id), revision INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS obligations (
                    run_id TEXT NOT NULL REFERENCES runs(id), id INTEGER NOT NULL,
                    title TEXT NOT NULL, state TEXT NOT NULL, evidence TEXT NOT NULL,
                    verified_revision INTEGER, superseded_by INTEGER, reason TEXT,
                    PRIMARY KEY(run_id, id)
                );
                PRAGMA user_version=8;
                COMMIT;",
            )?;
        }
        if schema_version < 9 {
            store.connection.execute_batch(
                "BEGIN IMMEDIATE;
                CREATE TABLE IF NOT EXISTS operation_revisions (
                    operation_id TEXT PRIMARY KEY REFERENCES operations(id),
                    run_id TEXT NOT NULL REFERENCES runs(id), revision INTEGER NOT NULL
                );
                PRAGMA user_version=9;
                COMMIT;",
            )?;
        }
        if schema_version < 10 {
            store.connection.execute_batch(
                "BEGIN IMMEDIATE;
                CREATE TABLE IF NOT EXISTS provider_routes (
                    run_id TEXT PRIMARY KEY REFERENCES runs(id), route TEXT NOT NULL
                );
                PRAGMA user_version=10;
                COMMIT;",
            )?;
        }
        if schema_version < 11 {
            store.connection.execute_batch(
                "BEGIN IMMEDIATE;
                UPDATE runs SET budgets = json_remove(budgets, '$.actions', '$.model_tokens', '$.tool_result_tokens');
                PRAGMA user_version=11;
                COMMIT;",
            )?;
        }
        if schema_version < 12 {
            let has_started_revision = {
                let mut statement = store.connection.prepare("PRAGMA table_info(operations)")?;
                statement
                    .query_map([], |row| row.get::<_, String>(1))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
                    .iter()
                    .any(|column| column == "started_revision")
            };
            store.connection.execute_batch("BEGIN IMMEDIATE;")?;
            if !has_started_revision {
                store
                    .connection
                    .execute_batch("ALTER TABLE operations ADD COLUMN started_revision INTEGER;")?;
            }
            store
                .connection
                .execute_batch("PRAGMA user_version=12; COMMIT;")?;
        }
        store.connection.execute_batch("CREATE TABLE IF NOT EXISTS pause_requests (run_id TEXT PRIMARY KEY REFERENCES runs(id), pending INTEGER NOT NULL)")?;
        store.connection.execute_batch("CREATE TABLE IF NOT EXISTS observed_files (run_id TEXT NOT NULL REFERENCES runs(id), path TEXT NOT NULL, hash TEXT NOT NULL, PRIMARY KEY(run_id,path))")?;
        Ok(store)
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
        self.create_run_with_id(
            &Uuid::new_v4().to_string(),
            task,
            workspace,
            provider,
            grants,
            budgets,
            acceptance,
        )
    }

    /// Creates a run with a caller supplied, validated ID. Remote task creation
    /// uses a stable ID derived from the relay request so a crash after run
    /// insertion but before its remote receipt cannot create a duplicate run.
    pub(crate) fn create_run_with_id(
        &mut self,
        id: &str,
        task: &str,
        workspace: &Path,
        provider: &str,
        grants: Value,
        mut budgets: Value,
        acceptance: &str,
    ) -> Result<Run> {
        Uuid::parse_str(id).context("invalid run ID")?;
        let workspace = dunce::canonicalize(workspace).context("workspace does not exist")?;
        if !budgets.is_object() {
            bail!("run configuration must be an object");
        }
        remove_task_caps(&mut budgets);
        crate::obligations::freeze_requirements(task, &mut budgets)?;
        if budgets.get("context_tokenizer").is_none() {
            budgets["context_tokenizer"] = json!(crate::tokenization::ENCODING);
        }
        crate::tokenization::validate(&budgets)?;
        budgets["project_memory"] = json!(self.project_memory(&workspace)?);
        budgets["project_instructions"] = json!(self.project_instructions(&workspace)?);
        crate::instructions::frozen(&budgets)?;
        budgets["repository_rules"] =
            json!(self.repository_snapshot(&workspace, &grants, &budgets)?);
        budgets["learning_enabled"] = json!(self.learning_enabled(&workspace)?);
        budgets["user_habits"] = json!(self.learned_habits(&workspace, task)?);
        budgets["workflow_patterns"] =
            json!(self.learned_patterns(&workspace, task, &budgets, &grants)?);
        let run = Run {
            id: id.to_owned(),
            task: task.to_owned(),
            workspace: workspace.to_string_lossy().into_owned(),
            provider: crate::provider::canonical(provider).to_owned(),
            grants,
            budgets,
            acceptance: acceptance.to_owned(),
            state: "ready".into(),
            created_at: now(),
        };
        crate::acceptance::Check::from_run(&run)?;
        crate::policy::CommandScopes::from_configuration(&run.budgets)?;
        crate::filesystem::FileScopes::from_configuration(&run.budgets)?;
        crate::network::NetworkScopes::from_configuration(&run.budgets)?;
        crate::routing::approved(&run)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        insert_run(&transaction, &run)?;
        transaction.commit()?;
        Ok(run)
    }

    pub fn project_memory(&self, workspace: &Path) -> Result<Vec<ProjectMemory>> {
        let workspace = dunce::canonicalize(workspace)?
            .to_string_lossy()
            .into_owned();
        let mut statement = self.connection.prepare("SELECT id,text,updated_at FROM project_memory WHERE workspace=?1 ORDER BY updated_at,id")?;
        statement
            .query_map([workspace], |row| {
                Ok(ProjectMemory {
                    id: row.get(0)?,
                    text: row.get(1)?,
                    updated_at: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn remember(
        &mut self,
        workspace: &Path,
        text: &str,
        replace: Option<&str>,
    ) -> Result<String> {
        let text = text.trim();
        if text.is_empty() || text.len() > 512 || crate::text::clean(text) != text {
            bail!("memory must be safe, nonempty text of at most 512 UTF-8 bytes");
        }
        let lower = text.to_lowercase();
        if lower.contains("ghp_")
            || lower.contains("bearer ")
            || lower.split_whitespace().any(|word| word.starts_with("sk-"))
        {
            bail!("do not store credentials in project memory");
        }
        let workspace = dunce::canonicalize(workspace)?
            .to_string_lossy()
            .into_owned();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if replace.is_none() {
            if let Some(id) = transaction
                .query_row(
                    "SELECT id FROM project_memory WHERE workspace=?1 AND text=?2",
                    params![workspace, text],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
            {
                return Ok(id);
            }
        }
        let id = replace
            .map(str::to_owned)
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        if replace.is_some()
            && transaction.query_row(
                "SELECT COUNT(*) FROM project_memory WHERE workspace=?1 AND id=?2",
                params![workspace, id],
                |row| row.get::<_, u64>(0),
            )? == 0
        {
            bail!("memory does not belong to this workspace");
        }
        let (count, bytes) = transaction.query_row("SELECT COUNT(*),COALESCE(SUM(length(CAST(text AS BLOB))),0) FROM project_memory WHERE workspace=?1 AND id<>?2", params![workspace,id], |row| Ok((row.get::<_,u64>(0)?,row.get::<_,u64>(1)?)))?;
        if count >= 16 || bytes + text.len() as u64 > 4096 {
            bail!(
                "project memory is limited to 16 notes and 4096 UTF-8 bytes; edit or remove an old note first"
            );
        }
        transaction.execute("INSERT INTO project_memory(id,workspace,text,updated_at) VALUES (?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET text=excluded.text,updated_at=excluded.updated_at", params![id,workspace,text,now()])?;
        transaction.commit()?;
        Ok(id)
    }

    pub fn forget(&mut self, workspace: &Path, id: &str) -> Result<()> {
        let workspace = dunce::canonicalize(workspace)?
            .to_string_lossy()
            .into_owned();
        if self.connection.execute(
            "DELETE FROM project_memory WHERE workspace=?1 AND id=?2",
            params![workspace, id],
        )? != 1
        {
            bail!("memory does not belong to this workspace");
        }
        Ok(())
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
        self.events_since(run_id, 0)
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

    pub fn recent_context_events(&self, run_id: &str, limit: i64) -> Result<Vec<Event>> {
        let mut statement = self.connection.prepare(
            "SELECT seq, kind, payload, created_at FROM events WHERE run_id = ?1 AND kind NOT IN ('model.started','operation.dispatched','operation.executing','operation.output') ORDER BY seq DESC LIMIT ?2",
        )?;
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

    pub fn pending_steering(&self, run_id: &str) -> Result<Vec<Event>> {
        let mut statement = self.connection.prepare(
            "SELECT seq, kind, payload, created_at FROM events WHERE run_id = ?1 AND kind = 'user.steering' AND seq > COALESCE((SELECT MAX(seq) FROM events WHERE run_id = ?1 AND kind = 'model.response'), 0) ORDER BY seq",
        )?;
        let rows = statement.query_map([run_id], |row| {
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

    pub fn events_since(&self, run_id: &str, seq: i64) -> Result<Vec<Event>> {
        let transaction = self.connection.unchecked_transaction()?;
        let seq = seq.max(0);
        let mut events = self.archived_events_since(run_id, seq)?;
        events.extend(self.hot_events_since(run_id, seq)?);
        let last: i64 = transaction.query_row(
            "SELECT COALESCE((SELECT last_seq FROM run_projection WHERE run_id = ?1), 0)",
            [run_id],
            |row| row.get(0),
        )?;
        if seq < last
            && (events.first().map(|event| event.seq) != Some(seq + 1)
                || events.last().map(|event| event.seq) != Some(last)
                || events.windows(2).any(|pair| pair[1].seq != pair[0].seq + 1))
        {
            bail!("audit event sequence has a gap");
        }
        transaction.commit()?;
        Ok(events)
    }

    pub(crate) fn hot_events_since(&self, run_id: &str, seq: i64) -> Result<Vec<Event>> {
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
            "SELECT COALESCE((SELECT count FROM event_counts WHERE run_id = ?1 AND kind = ?2), 0)",
            params![run_id, kind],
            |row| row.get(0),
        )?)
    }

    pub fn model_tokens(&self, run_id: &str) -> Result<u64> {
        let total: i64 = self.connection.query_row(
            "SELECT COALESCE((SELECT model_tokens FROM run_projection WHERE run_id = ?1), 0)",
            [run_id],
            |row| row.get(0),
        )?;
        Ok(total.max(0) as u64)
    }

    pub fn tool_result_tokens(&self, run_id: &str) -> Result<u64> {
        Ok(self.connection.query_row(
            "SELECT COALESCE((SELECT tokens FROM tool_token_projection WHERE run_id=?1),0)",
            [run_id],
            |row| row.get(0),
        )?)
    }

    pub fn run_started_at(&self, run_id: &str) -> Result<Option<i64>> {
        Ok(self.connection.query_row(
            "SELECT started_at FROM run_projection WHERE run_id = ?1",
            [run_id],
            |row| row.get(0),
        )?)
    }

    pub fn run_summary(&self, run_id: &str) -> Result<Option<String>> {
        Ok(self
            .connection
            .query_row(
                "SELECT summary FROM run_projection WHERE run_id = ?1",
                [run_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    pub fn activate(&mut self, run_id: &str, capability: &str, version: u32) -> Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "DELETE FROM activated WHERE run_id = ?1 AND capability = ?2",
            params![run_id, capability],
        )?;
        transaction.execute(
            "INSERT INTO activated VALUES (?1, ?2, ?3)",
            params![run_id, capability, version],
        )?;
        let evicted = {
            let mut statement = transaction.prepare(
                "SELECT capability, version FROM activated WHERE run_id = ?1 ORDER BY rowid DESC LIMIT -1 OFFSET 8",
            )?;
            statement
                .query_map([run_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (id, version) in evicted {
            transaction.execute(
                "DELETE FROM activated WHERE run_id = ?1 AND capability = ?2",
                params![run_id, id],
            )?;
            append_event(
                &transaction,
                run_id,
                "capability.deactivated",
                json!({"id": id, "version": version, "reason": "bounded working set"}),
            )?;
        }
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

    pub fn working_capabilities(&self, run_id: &str) -> Result<Vec<(String, u32)>> {
        let mut statement = self.connection.prepare(
            "SELECT capability, version FROM activated WHERE run_id = ?1 ORDER BY rowid DESC LIMIT 8",
        )?;
        let rows = statement.query_map([run_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn register_mcp(&mut self, server: &McpServer, tools: &[McpTool]) -> Result<()> {
        crate::mcp::validate(server)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute("INSERT INTO mcp_servers(name, command, args, policy) VALUES (?1, ?2, ?3, ?4) ON CONFLICT(name) DO UPDATE SET command = excluded.command, args = excluded.args, policy = excluded.policy",
            params![server.name, server.command, serde_json::to_string(&server.args)?, serde_json::to_string(&server.policy)?])?;
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
                "SELECT command, args, policy FROM mcp_servers WHERE name = ?1",
                [name],
                |row| {
                    let args: String = row.get(1)?;
                    let policy: String = row.get(2)?;
                    Ok(McpServer {
                        name: name.into(),
                        command: row.get(0)?,
                        policy: serde_json::from_str(&policy).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                2,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        })?,
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

    pub fn has_operation_artifact(&self, run_id: &str, hash: &str) -> Result<bool> {
        let count: i64 = self.connection.query_row("SELECT COUNT(*) FROM operations AS operation WHERE operation.run_id = ?1 AND (operation.artifact = ?2 OR EXISTS (SELECT 1 FROM operation_artifacts AS linked WHERE linked.operation_id = operation.id AND linked.hash = ?2))",params![run_id,hash],|row|row.get(0))?;
        Ok(count > 0)
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
        let hash: Option<String> = self
            .connection
            .query_row(
                "SELECT checkpoint FROM run_projection WHERE run_id = ?1",
                [run_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
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
            if milestone.title.trim().is_empty() || milestone.title.len() > 200 {
                bail!("milestone title must contain 1..200 bytes of nonblank text");
            }
            if !matches!(milestone.state.as_str(), "pending" | "active" | "completed") {
                bail!("milestone state must be pending, active, or completed");
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
        validate_completed_milestone_artifacts(
            &self.connection,
            &self.artifacts,
            &checkpoint.milestones,
        )?;
        self.refresh_observed_files(run_id)?;
        let hash = self.put_artifact(&serde_json::to_vec(checkpoint)?)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_completed_milestone_evidence(&transaction, run_id, &checkpoint.milestones)?;
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

    pub fn validate_completion(&self, run_id: &str, evidence: &[String]) -> Result<()> {
        self.validate_completion_except_operation(run_id, evidence, None)
    }

    pub(crate) fn validate_completion_except_operation(
        &self,
        run_id: &str,
        evidence: &[String],
        allowed_operation: Option<&str>,
    ) -> Result<()> {
        self.check_observed_files(run_id)?;
        if crate::obligations::workspace_mutation_in_flight(&self.connection, run_id)? {
            bail!("completion waits for mutating operations in the shared workspace");
        }
        if evidence.is_empty() {
            bail!("completion requires evidence");
        }
        let unresolved: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM operations WHERE run_id = ?1 AND state NOT IN ('succeeded','failed','cancelled') AND (?2 IS NULL OR id != ?2)",
            params![run_id, allowed_operation],
            |row| row.get(0),
        )?;
        if unresolved != 0 {
            bail!("completion requires all operations to finish or be reconciled");
        }
        for hash in evidence {
            if !self.has_evidence(run_id, hash)? {
                bail!("completion evidence is not a successful operation artifact: {hash}");
            }
            self.artifact(hash)?;
        }
        crate::obligations::validate_finish_evidence(&self.connection, run_id, evidence)?;
        crate::obligations::validate_completion(self, run_id)?;
        let milestones = self.milestones(run_id)?;
        validate_completed_milestone_evidence(&self.connection, run_id, &milestones)?;
        validate_completed_milestone_artifacts(&self.connection, &self.artifacts, &milestones)?;
        if self.run(run_id)?.state != "running" {
            bail!("only a running run can complete");
        }
        if !(milestones.len() == 1 && milestones[0].title == "Task request")
            && milestones
                .iter()
                .any(|milestone| milestone.state != "completed")
        {
            bail!("all planned milestones must have evidence before completion");
        }
        Ok(())
    }

    pub fn answer_run(&mut self, run_id: &str, summary: &str) -> Result<()> {
        if summary.trim().is_empty() || summary.len() > 64 * 1024 {
            bail!("a conversational reply must contain 1..65536 bytes");
        }
        let run = self.run(run_id)?;
        if crate::acceptance::Check::from_run(&run)?.is_some() {
            bail!("a reply cannot replace the configured acceptance check");
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state: String =
            transaction.query_row("SELECT state FROM runs WHERE id = ?1", [run_id], |row| {
                row.get(0)
            })?;
        let operations: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM operations WHERE run_id = ?1",
            [run_id],
            |row| row.get(0),
        )?;
        let planned: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM milestones WHERE run_id = ?1 AND title != 'Task request'",
            [run_id],
            |row| row.get(0),
        )?;
        if state != "running" || operations != 0 || planned != 0 {
            bail!(
                "a conversational reply cannot replace started or planned tool work; finish with evidence instead"
            );
        }
        let explicit: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM obligations WHERE run_id = ?1 AND id > 0",
            [run_id],
            |row| row.get(0),
        )?;
        if explicit > 0 {
            bail!("a conversational reply cannot bypass kernel-owned obligations");
        }
        transaction.execute("UPDATE runs SET state = 'answered' WHERE id = ?1", [run_id])?;
        append_event(
            &transaction,
            run_id,
            "run.answered",
            json!({"summary":summary,"verified":false}),
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn completion_proposal(&self, run_id: &str) -> Result<Option<String>> {
        Ok(self
            .connection
            .query_row(
                "SELECT proposal FROM run_projection WHERE run_id = ?1",
                [run_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    pub fn complete_run(&mut self, run_id: &str, summary: &str, evidence: &[String]) -> Result<()> {
        self.refresh_observed_files(run_id)?;
        self.validate_completion(run_id, evidence)?;
        let run = self.run(run_id)?;
        let acceptance = crate::acceptance::verified_result(self, &run, summary, evidence)?;
        let proposal = self.completion_proposal(run_id)?;
        let receipt_milestones = self.milestones(run_id)?;
        let prior_transitions = self
            .events(run_id)?
            .into_iter()
            .filter(|event| event.kind == "provider.transition")
            .map(|event| (event.seq, event.payload))
            .collect::<Vec<_>>();
        let mut hashes = std::collections::BTreeSet::new();
        hashes.extend(evidence.iter().cloned());
        for item in self
            .obligations(run_id)?
            .iter()
            .filter(|item| item.state == "verified")
        {
            hashes.extend(item.evidence.iter().cloned());
        }
        hashes.extend(acceptance.iter().cloned());
        for milestone in receipt_milestones
            .iter()
            .filter(|milestone| milestone.state == "completed")
        {
            hashes.extend(milestone.evidence.iter().cloned());
        }
        let mut receipts = std::collections::BTreeMap::new();
        for hash in hashes {
            let bytes = self.artifact(&hash)?;
            let result = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
            let mut receipt = json!({});
            for key in [
                "path",
                "sha256",
                "bytes",
                "exit_code",
                "elapsed_ms",
                "output_artifact",
            ] {
                if let Some(value) = result.get(key) {
                    receipt[key] = value.clone();
                }
            }
            receipts.insert(hash, receipt);
        }
        let artifact_directory = self.artifacts.clone();
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
        let unresolved: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM operations WHERE run_id = ?1 AND state NOT IN ('succeeded','failed','cancelled')",
            [run_id],
            |row| row.get(0),
        )?;
        if unresolved != 0 {
            bail!("operations changed before completion; finish or reconcile them first");
        }
        if crate::obligations::workspace_mutation_in_flight(&transaction, run_id)? {
            bail!("workspace mutation started before completion could be committed");
        }
        crate::obligations::validate_connection(&transaction, run_id)?;
        crate::obligations::validate_finish_evidence(&transaction, run_id, evidence)?;
        let mut milestones = {
            let mut statement = transaction.prepare(
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
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        if milestones.len() == 1 && milestones[0].title == "Task request" {
            transaction.execute(
                "UPDATE milestones SET state = 'completed', evidence = ?2 WHERE run_id = ?1",
                params![run_id, serde_json::to_string(evidence)?],
            )?;
            milestones[0].state = "completed".into();
            milestones[0].evidence = evidence.to_vec();
        } else if milestones
            .iter()
            .any(|milestone| milestone.state != "completed")
        {
            bail!("all planned milestones must have evidence before completion");
        }
        validate_completed_milestone_evidence(&transaction, run_id, &milestones)?;
        validate_completed_milestone_artifacts(&transaction, &artifact_directory, &milestones)?;
        let revision: Option<i64> = transaction
            .query_row(
                "SELECT revision FROM workspace_revisions WHERE run_id = ?1",
                [run_id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(revision) = revision {
            transaction.execute(
                "UPDATE obligations SET state = 'verified', evidence = ?2, verified_revision = ?3 WHERE run_id = ?1 AND id = 0",
                params![run_id, serde_json::to_string(evidence)?, revision],
            )?;
        }
        transaction.execute(
            "UPDATE runs SET state = 'completed' WHERE id = ?1",
            [run_id],
        )?;
        if acceptance.is_some() {
            append_event(
                &transaction,
                run_id,
                "completion.resolved",
                json!({"proposal":proposal,"outcome":"passed","acceptance":acceptance}),
            )?;
        }
        append_event(
            &transaction,
            run_id,
            "run.completed",
            json!({"summary": summary, "evidence": evidence, "acceptance":acceptance,
                "completion":crate::control::completion_report(&transaction, run_id, acceptance.as_deref(),&prior_transitions,&receipts)?}),
        )?;
        if let Some(verified) = acceptance.as_deref() {
            crate::learning::record(&transaction, &run, verified)?;
        }
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
        if matches!(
            previous.as_str(),
            "completed" | "answered" | "cancelled" | "failed"
        ) {
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
            json!({"id": operation.id, "capability": capability, "version": capability_version, "arguments": operation.arguments, "idempotency_key": operation.idempotency_key, "retry_safe":retry_safe}),
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
        self.update_operation_state(operation, state, artifact, detail, false)
    }

    fn update_operation_state(
        &mut self,
        operation: &Operation,
        state: &str,
        artifact: Option<&str>,
        detail: Value,
        allow_unknown_resolution: bool,
    ) -> Result<()> {
        let artifact_directory = self.artifacts.clone();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut canonical: Operation = transaction.query_row(
            "SELECT capability, capability_version, arguments, idempotency_key, retry_safe, state, artifact
             FROM operations WHERE id = ?1 AND run_id = ?2",
            params![operation.id, operation.run_id],
            |row| {
                let arguments: String = row.get(2)?;
                Ok(Operation {
                    id: operation.id.clone(),
                    run_id: operation.run_id.clone(),
                    capability: row.get(0)?,
                    capability_version: row.get(1)?,
                    arguments: serde_json::from_str(&arguments).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            2,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?,
                    idempotency_key: row.get(3)?,
                    retry_safe: row.get(4)?,
                    state: row.get(5)?,
                    artifact: row.get(6)?,
                })
            },
        )?;
        let resolving_unknown = allow_unknown_resolution
            && canonical.state == "outcome_unknown"
            && matches!(state, "succeeded" | "failed");
        let retrying_safe_failure =
            canonical.state == "failed" && canonical.retry_safe && state == "dispatched";
        if terminal_operation_state(&canonical.state)
            && !resolving_unknown
            && !retrying_safe_failure
        {
            if canonical.state == state
                && artifact.is_none_or(|hash| canonical.artifact.as_deref() == Some(hash))
            {
                return Ok(());
            }
            bail!("terminal operation states cannot be rewritten");
        }
        validate_operation_transition(&canonical, state, allow_unknown_resolution)?;

        let result_artifact = artifact.or(canonical.artifact.as_deref());
        let observed_result = if state == "succeeded"
            && matches!(
                canonical.capability.as_str(),
                "workspace.read" | "workspace.read_batch" | "workspace.write" | "workspace.patch"
            ) {
            result_artifact
                .map(|hash| artifact_bytes(&transaction, &artifact_directory, hash))
                .transpose()?
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        } else {
            None
        };
        let previous = canonical.state.clone();
        transaction.execute(
            "UPDATE operations SET state = ?2, artifact = COALESCE(?3, artifact) WHERE id = ?1",
            params![canonical.id, state, artifact],
        )?;
        canonical.state = state.to_owned();
        if let Some(artifact) = artifact {
            canonical.artifact = Some(artifact.to_owned());
        }
        if terminal_operation_state(state) {
            crate::freshness::record(&transaction, &canonical, observed_result.as_ref())?;
            // `outcome_unknown` already invalidated the workspace for this
            // attempt. Reconciliation resolves that same attempt, it does
            // not constitute another workspace mutation.
            if previous != "outcome_unknown" {
                let may_have_run = state == "succeeded"
                    || matches!(previous.as_str(), "dispatched" | "executing");
                crate::obligations::record_operation(&transaction, &canonical, may_have_run)?;
            }
        }
        append_event(
            &transaction,
            &canonical.run_id,
            &format!("operation.{state}"),
            json!({"id": canonical.id, "artifact": artifact, "detail": detail}),
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn claim_operation(&mut self, operation: &Operation) -> Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let capability: Option<String> = transaction
            .query_row(
                "SELECT operation.capability FROM operations AS operation
                 WHERE operation.id = ?1 AND operation.run_id = ?2
                   AND operation.state = 'dispatched'
                   AND EXISTS (SELECT 1 FROM runs WHERE id = ?2 AND state = 'running')",
                params![operation.id, operation.run_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(capability) = capability else {
            bail!("operation has already been claimed or is no longer dispatched");
        };
        let started_revision = crate::obligations::claim_operation_revision(
            &transaction,
            &operation.run_id,
            &operation.id,
            &capability,
        )?;
        let changed = transaction.execute(
            "UPDATE operations SET state = 'executing', started_revision = ?3 WHERE id = ?1 AND run_id = ?2 AND state = 'dispatched' AND EXISTS (SELECT 1 FROM runs WHERE id = ?2 AND state = 'running')",
            params![operation.id, operation.run_id, started_revision],
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
        self.selected_operations(run_id, false)
    }

    fn selected_operations(&self, run_id: &str, unresolved: bool) -> Result<Vec<Operation>> {
        let query = if unresolved {
            "SELECT id, capability, capability_version, arguments, idempotency_key, retry_safe, state, artifact FROM operations WHERE run_id = ?1 AND state IN ('pending', 'dispatched', 'executing') ORDER BY rowid"
        } else {
            "SELECT id, capability, capability_version, arguments, idempotency_key, retry_safe, state, artifact FROM operations WHERE run_id = ?1 ORDER BY rowid"
        };
        let mut statement = self.connection.prepare(query)?;
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
        self.selected_operations(run_id, true)
    }

    pub fn inspectable_artifacts(&self, run_id: &str) -> Result<Vec<(String, String)>> {
        let mut query = self.connection.prepare("SELECT capability || ' [' || state || ']', artifact FROM operations WHERE run_id=?1 AND artifact IS NOT NULL UNION SELECT operation.capability || ' [' || operation.state || '] ' || linked.kind,linked.hash FROM operations AS operation JOIN operation_artifacts AS linked ON linked.operation_id=operation.id WHERE operation.run_id=?1 ORDER BY 1,2")?;
        Ok(query
            .query_map([run_id], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
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
        self.update_operation_state(
            &operation,
            if succeeded { "succeeded" } else { "failed" },
            Some(&artifact),
            json!({"reconciled_by": "user", "note_artifact": artifact}),
            true,
        )?;
        if self.unknown_count(run_id)? == 0 && !self.run(run_id)?.is_terminal() {
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
            if operation.state == "pending" {
                self.event(
                    run_id,
                    "operation.dispatch_ready",
                    json!({"id":operation.id,"reason":"intent was never dispatched"}),
                )?;
            } else if operation.retry_safe {
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
            }
        }
        let unknown = self.unknown_count(run_id)?;
        let run = self.run(run_id)?;
        if unknown > 0 && run.state != "waiting_recovery" && !run.is_terminal() {
            self.state(
                run_id,
                "waiting_recovery",
                json!({"unknown_operations": unknown}),
            )?;
        }
        Ok(unresolved)
    }

    pub fn put_artifact(&self, bytes: &[u8]) -> Result<String> {
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
        artifact_bytes(&self.connection, &self.artifacts, hash)
    }
}

#[cfg(test)]
pub(crate) fn claim_test_operation(store: &mut Store, operation: &Operation) -> Result<()> {
    let run = store.run(&operation.run_id)?;
    if run.is_terminal() {
        bail!("cannot claim an operation for a terminal test fixture run");
    }
    if run.state != "running" {
        store.state(&operation.run_id, "running", json!({"test_fixture":true}))?;
    }
    let mut current = store.operation(&operation.id)?;
    if current.state == "pending" {
        store.operation_state(&current, "dispatched", None, json!({"test_fixture":true}))?;
        current = store.operation(&operation.id)?;
    }
    if current.state == "dispatched" {
        store.claim_operation(&current)?;
    } else if current.state != "executing" {
        bail!("test fixture can only claim an unresolved operation");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_output_chunks_are_excluded_from_recent_model_context() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "exclude live process output from model context",
            directory.path(),
            "codex",
            json!(["process.run"]),
            json!({}),
            "",
        )?;
        store.event(
            &run.id,
            "operation.output",
            json!({"stream":"combined","text":"live output"}),
        )?;
        store.event(&run.id, "model.failed", json!({"error":"visible status"}))?;

        let context = store.recent_context_events(&run.id, 12)?;
        assert!(context.iter().all(|event| event.kind != "operation.output"));
        assert!(context.iter().any(|event| event.kind == "model.failed"));
        Ok(())
    }

    #[test]
    fn projection_migration_preserves_history_and_updates_atomically() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "history",
            directory.path(),
            "codex",
            json!([]),
            json!({}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        store.event(
            &run.id,
            "model.response",
            json!({"usage":{"input_tokens":123,"output_tokens":7}}),
        )?;
        let proposal = store.put_artifact(b"proposal")?;
        store.event(&run.id, "completion.proposed", json!({"artifact":proposal}))?;
        let original = store.events(&run.id)?;
        store.connection.execute_batch(
            "DROP TABLE run_projection; DROP TABLE event_counts; PRAGMA user_version=3;",
        )?;
        drop(store);
        let mut store = Store::open(directory.path())?;
        assert_eq!(store.model_tokens(&run.id)?, 130);
        assert_eq!(store.event_count(&run.id, "model.response")?, 1);
        assert_eq!(store.completion_proposal(&run.id)?, Some(proposal));
        assert_eq!(store.run_started_at(&run.id)?, Some(original[1].created_at));
        {
            let transaction = store
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            append_event(
                &transaction,
                &run.id,
                "model.response",
                json!({"usage":{"input_tokens":999}}),
            )?;
        }
        assert_eq!(store.model_tokens(&run.id)?, 130);
        assert_eq!(store.event_count(&run.id, "model.response")?, 1);
        store.event(&run.id, "completion.resolved", json!({}))?;
        assert_eq!(store.completion_proposal(&run.id)?, None);
        assert_eq!(
            store.events(&run.id)?.last().unwrap().seq,
            original.len() as i64 + 1
        );
        Ok(())
    }

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
        store.connection.execute(
            "UPDATE run_projection SET started_at = 100 WHERE run_id = ?1",
            [&run.id],
        )?;
        store.state(&run.id, "waiting_recovery", json!({}))?;
        store.state(&run.id, "running", json!({}))?;
        assert_eq!(store.run_started_at(&run.id)?, Some(100));
        Ok(())
    }

    #[test]
    fn migration_removes_obsolete_task_turn_and_token_caps_from_saved_runs() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "legacy task",
            directory.path(),
            "codex",
            json!([]),
            json!({"wall_seconds":120}),
            "",
        )?;
        store.connection.execute(
            "UPDATE runs SET budgets = json_set(budgets, '$.actions', 20, '$.model_tokens', 5000, '$.tool_result_tokens', 9000) WHERE id = ?1",
            [&run.id],
        )?;
        store.connection.execute_batch("PRAGMA user_version=10")?;
        drop(store);

        let store = Store::open(directory.path())?;
        let migrated = store.run(&run.id)?;
        for key in ["actions", "model_tokens", "tool_result_tokens"] {
            assert!(migrated.budgets.get(key).is_none(), "{key} remained saved");
        }
        assert_eq!(migrated.budgets["wall_seconds"], 120);
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
        store.state(&run.id, "running", json!({}))?;
        let operation =
            store.begin_operation(&run.id, "workspace.read", json!({"path": "a"}), true)?;
        store.operation_state(&operation, "dispatched", None, json!({}))?;
        store.claim_operation(&operation)?;
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
            vec![1, 2, 3, 4, 5, 6]
        );
        assert_eq!(store.artifact(&hash)?, b"large output");
        assert!(store.unresolved(&run.id)?.is_empty());
        Ok(())
    }

    #[test]
    fn operation_state_uses_canonical_row_and_requires_claim_for_side_effects() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "preserve canonical operation identity",
            directory.path(),
            "codex",
            json!(["workspace.write"]),
            json!({}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;

        let read = store.begin_operation_versioned(
            &run.id,
            "workspace.read",
            7,
            json!({"path":"canonical.txt"}),
            true,
        )?;
        store.operation_state(&read, "dispatched", None, json!({}))?;
        store.claim_operation(&read)?;
        let forged_artifact = store.put_artifact(
            br#"{"sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#,
        )?;
        let mut forged = read.clone();
        forged.capability = "workspace.write".into();
        forged.capability_version = 99;
        forged.arguments = json!({"path":"forged.txt"});
        forged.retry_safe = false;
        assert!(store
            .operation_state(&forged, "outcome_unknown", None, json!({}))
            .is_err());
        assert_eq!(store.operation(&read.id)?.state, "executing");
        store.operation_state(&forged, "succeeded", Some(&forged_artifact), json!({}))?;

        assert_eq!(store.workspace_revision(&run.id)?, Some(0));
        assert_eq!(
            store.connection.query_row(
                "SELECT path FROM observed_files WHERE run_id=?1",
                [&run.id],
                |row| row.get::<_, String>(0),
            )?,
            "canonical.txt"
        );
        let stored = store.operation(&read.id)?;
        assert_eq!(stored.capability, "workspace.read");
        assert_eq!(stored.capability_version, 7);
        assert_eq!(stored.arguments, json!({"path":"canonical.txt"}));
        assert!(stored.retry_safe);
        Ok(())
    }

    #[test]
    fn every_success_requires_claim_and_safe_failed_reads_can_retry() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "read proof",
            directory.path(),
            "codex",
            json!([]),
            json!({}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;

        let read = store.begin_operation(&run.id, "workspace.read", json!({}), true)?;
        let receipt = store.put_artifact(b"read receipt")?;
        assert!(store
            .operation_state(&read, "succeeded", Some(&receipt), json!({}))
            .is_err());
        store.operation_state(&read, "dispatched", None, json!({}))?;
        assert!(store
            .operation_state(&read, "succeeded", Some(&receipt), json!({}))
            .is_err());
        store.claim_operation(&read)?;
        store.operation_state(&read, "succeeded", Some(&receipt), json!({}))?;

        let retry = store.begin_operation(&run.id, "workspace.read", json!({}), true)?;
        store.operation_state(&retry, "failed", None, json!({}))?;
        store.operation_state(&retry, "dispatched", None, json!({}))?;
        store.claim_operation(&retry)?;
        let retry_receipt = store.put_artifact(b"retried read receipt")?;
        store.operation_state(&retry, "succeeded", Some(&retry_receipt), json!({}))?;
        assert_eq!(store.operation(&retry.id)?.state, "succeeded");
        Ok(())
    }

    #[test]
    fn operation_state_rejects_invalid_transitions_and_terminal_rewrites() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "require dispatch and claim before a write succeeds",
            directory.path(),
            "codex",
            json!(["workspace.write"]),
            json!({}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let write = store.begin_operation(
            &run.id,
            "workspace.write",
            json!({"path":"fixture.txt","content":"verified"}),
            false,
        )?;
        let result = store.put_artifact(
            br#"{"path":"fixture.txt","bytes":8,"sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}"#,
        )?;

        assert!(store
            .operation_state(&write, "unknown", None, json!({}))
            .is_err());
        assert!(store
            .operation_state(&write, "succeeded", Some(&result), json!({}))
            .is_err());
        assert_eq!(store.operation(&write.id)?.state, "pending");

        store.operation_state(&write, "dispatched", None, json!({}))?;
        assert!(store
            .operation_state(&write, "succeeded", Some(&result), json!({}))
            .is_err());
        store.claim_operation(&write)?;
        store.operation_state(&write, "succeeded", Some(&result), json!({}))?;
        assert!(store
            .operation_state(&write, "failed", None, json!({}))
            .is_err());
        store.operation_state(
            &write,
            "succeeded",
            Some(&result),
            json!({"duplicate":true}),
        )?;
        assert_eq!(store.operation(&write.id)?.state, "succeeded");
        assert_eq!(store.workspace_revision(&run.id)?, Some(2));
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
    fn reconcile_recovers_idempotently_after_unknown_outcome_is_recorded() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "resume reconciliation",
            directory.path(),
            "grok",
            json!([]),
            json!({}),
            "receipt",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let operation = store.begin_operation(&run.id, "external.write", json!({}), false)?;
        store.operation_state(&operation, "dispatched", None, json!({}))?;
        store.operation_state(&operation, "outcome_unknown", None, json!({}))?;

        assert_eq!(store.run(&run.id)?.state, "running");
        assert!(store.reconcile(&run.id)?.is_empty());
        assert_eq!(store.run(&run.id)?.state, "waiting_recovery");
        let unknown_events = store.event_count(&run.id, "operation.outcome_unknown")?;
        let waiting_events = store.event_count(&run.id, "run.waiting_recovery")?;
        assert_eq!(unknown_events, 1);
        assert_eq!(waiting_events, 1);

        assert!(store.reconcile(&run.id)?.is_empty());
        assert_eq!(store.operation(&operation.id)?.state, "outcome_unknown");
        assert_eq!(store.event_count(&run.id, "operation.outcome_unknown")?, 1);
        assert_eq!(store.event_count(&run.id, "run.waiting_recovery")?, 1);
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
        assert!(store
            .resolve_unknown(&run.id, &operation.id, true, "")
            .is_err());
        store.resolve_unknown(&run.id, &operation.id, true, "external receipt 123")?;
        assert_eq!(store.unknown_count(&run.id)?, 0);
        assert_eq!(store.run(&run.id)?.state, "ready");
        assert!(store.has_evidence(
            &run.id,
            store.operation(&operation.id)?.artifact.as_deref().unwrap()
        )?);
        assert!(store
            .resolve_unknown(&run.id, &operation.id, true, "duplicate")
            .is_err());
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
    fn capability_working_sets_evict_durably_and_reactivate_without_losing_history() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run("work", directory.path(), "codex", json!([]), json!({}), "")?;
        let operation = store.begin_operation(&run.id, "tool_00", json!({}), true)?;
        let evidence = store.put_artifact(b"recorded before schema eviction")?;
        claim_test_operation(&mut store, &operation)?;
        store.operation_state(&operation, "succeeded", Some(&evidence), json!({}))?;
        for index in 0..12 {
            store.activate(&run.id, &format!("tool_{index:02}"), 1)?;
        }
        let active = store.active_capabilities(&run.id)?;
        assert_eq!(active.len(), 8);
        assert_eq!(active[0].0, "tool_04");
        store.save_snapshot(&run.id)?;
        store.activate(&run.id, "tool_04", 2)?;
        store.activate(&run.id, "tool_12", 1)?;
        assert_eq!(store.working_capabilities(&run.id)?.len(), 8);
        assert!(store
            .active_capabilities(&run.id)?
            .contains(&("tool_04".into(), 2)));
        assert!(!store
            .active_capabilities(&run.id)?
            .iter()
            .any(|(id, _)| id == "tool_05"));
        assert!(store.load_recovery(&run.id)?.is_some());
        drop(store);
        let mut store = Store::open(directory.path())?;
        assert!(store.load_recovery(&run.id)?.is_some());
        assert_eq!(store.active_capabilities(&run.id)?.len(), 8);
        assert!(store.has_evidence(&run.id, &evidence)?);
        assert_eq!(store.operation(&operation.id)?.state, "succeeded");
        assert_eq!(store.event_count(&run.id, "capability.deactivated")?, 5);
        assert_eq!(store.event_count(&run.id, "capability.activated")?, 14);
        let legacy = store.create_run(
            "legacy",
            directory.path(),
            "codex",
            json!([]),
            json!({}),
            "",
        )?;
        for index in 0..12 {
            store.connection.execute(
                "INSERT INTO activated VALUES (?1, ?2, 1)",
                params![legacy.id, format!("legacy_{index:02}")],
            )?;
        }
        assert_eq!(store.active_capabilities(&legacy.id)?.len(), 12);
        let working = store.working_capabilities(&legacy.id)?;
        assert_eq!(working.len(), 8);
        assert_eq!(working[0].0, "legacy_11");
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
        claim_test_operation(&mut store, &operation)?;
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
        let mut invalid = checkpoint.clone();
        invalid.milestones[0].state = "complete".into();
        assert_eq!(
            store
                .save_checkpoint(&run.id, &invalid)
                .unwrap_err()
                .to_string(),
            "milestone state must be pending, active, or completed"
        );
        invalid.milestones[0].state = "completed".into();
        invalid.milestones[0].title = " ".into();
        assert_eq!(
            store
                .save_checkpoint(&run.id, &invalid)
                .unwrap_err()
                .to_string(),
            "milestone title must contain 1..200 bytes of nonblank text"
        );
        assert_eq!(store.last_checkpoint(&run.id)?, Some(checkpoint.clone()));
        assert!(store
            .complete_run(&run.id, "done", &[evidence.clone()])
            .is_err());
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

    #[test]
    fn completed_milestones_require_current_evidence_at_save_and_completion() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "repair",
            directory.path(),
            "codex",
            json!(["workspace.write"]),
            json!({}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;

        let initial_read = store.begin_operation(&run.id, "workspace.read", json!({}), true)?;
        let old_evidence = store.put_artifact(b"revision zero proof")?;
        claim_test_operation(&mut store, &initial_read)?;
        store.operation_state(&initial_read, "succeeded", Some(&old_evidence), json!({}))?;
        let mut checkpoint = Handoff {
            decisions: vec![],
            unresolved: vec![],
            next_action: "finish repair".into(),
            milestones: vec![Milestone {
                title: "Repair is verified".into(),
                state: "completed".into(),
                evidence: vec![old_evidence.clone()],
            }],
        };
        store.save_checkpoint(&run.id, &checkpoint)?;
        assert_eq!(store.workspace_revision(&run.id)?, Some(0));

        let mutation = store.begin_operation(&run.id, "workspace.write", json!({}), true)?;
        store.operation_state(&mutation, "dispatched", None, json!({}))?;
        store.claim_operation(&mutation)?;
        let mutation_receipt = store.put_artifact(b"workspace changed")?;
        store.operation_state(&mutation, "succeeded", Some(&mutation_receipt), json!({}))?;
        assert_eq!(store.workspace_revision(&run.id)?, Some(2));

        let final_read = store.begin_operation(&run.id, "workspace.read", json!({}), true)?;
        let unrelated_fresh_evidence = store.put_artifact(b"unrelated fresh proof")?;
        claim_test_operation(&mut store, &final_read)?;
        store.operation_state(
            &final_read,
            "succeeded",
            Some(&unrelated_fresh_evidence),
            json!({}),
        )?;
        assert_ne!(old_evidence, unrelated_fresh_evidence);
        assert!(store
            .validate_completion(&run.id, &[unrelated_fresh_evidence.clone()])
            .is_err());
        assert!(store.save_checkpoint(&run.id, &checkpoint).is_err());
        assert!(store
            .complete_run(&run.id, "done", &[unrelated_fresh_evidence.clone()])
            .is_err());
        assert_eq!(store.run(&run.id)?.state, "running");

        checkpoint.milestones[0].evidence = vec![unrelated_fresh_evidence.clone()];
        store.save_checkpoint(&run.id, &checkpoint)?;
        store.complete_run(&run.id, "done", &[unrelated_fresh_evidence])?;
        assert_eq!(store.run(&run.id)?.state, "completed");
        Ok(())
    }

    #[test]
    fn completed_milestone_evidence_is_integrity_checked() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "repair",
            directory.path(),
            "codex",
            json!([]),
            json!({}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let milestone_operation =
            store.begin_operation(&run.id, "workspace.read", json!({}), true)?;
        let milestone_evidence = store.put_artifact(b"milestone proof")?;
        claim_test_operation(&mut store, &milestone_operation)?;
        store.operation_state(
            &milestone_operation,
            "succeeded",
            Some(&milestone_evidence),
            json!({}),
        )?;
        let final_operation = store.begin_operation(&run.id, "workspace.read", json!({}), true)?;
        let final_evidence = store.put_artifact(b"final proof")?;
        claim_test_operation(&mut store, &final_operation)?;
        store.operation_state(
            &final_operation,
            "succeeded",
            Some(&final_evidence),
            json!({}),
        )?;
        let checkpoint = Handoff {
            decisions: vec![],
            unresolved: vec![],
            next_action: "finish repair".into(),
            milestones: vec![Milestone {
                title: "Repair is verified".into(),
                state: "completed".into(),
                evidence: vec![milestone_evidence.clone()],
            }],
        };
        store.save_checkpoint(&run.id, &checkpoint)?;

        std::fs::write(
            directory.path().join("artifacts").join(&milestone_evidence),
            b"tampered milestone proof",
        )?;
        assert!(store.save_checkpoint(&run.id, &checkpoint).is_err());
        assert!(store
            .validate_completion(&run.id, &[final_evidence.clone()])
            .is_err());
        assert!(store
            .complete_run(&run.id, "done", &[final_evidence])
            .is_err());
        assert_eq!(store.run(&run.id)?.state, "running");
        Ok(())
    }

    #[test]
    fn default_task_milestone_rejects_mixed_stale_and_current_completion_proof() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "repair",
            directory.path(),
            "codex",
            json!(["workspace.write"]),
            json!({}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let initial_read = store.begin_operation(&run.id, "workspace.read", json!({}), true)?;
        let old_evidence = store.put_artifact(b"revision zero proof")?;
        claim_test_operation(&mut store, &initial_read)?;
        store.operation_state(&initial_read, "succeeded", Some(&old_evidence), json!({}))?;

        let mutation = store.begin_operation(&run.id, "workspace.write", json!({}), true)?;
        store.operation_state(&mutation, "dispatched", None, json!({}))?;
        store.claim_operation(&mutation)?;
        let mutation_receipt = store.put_artifact(b"workspace changed")?;
        store.operation_state(&mutation, "succeeded", Some(&mutation_receipt), json!({}))?;
        assert_eq!(store.workspace_revision(&run.id)?, Some(2));

        let final_read = store.begin_operation(&run.id, "workspace.read", json!({}), true)?;
        let current_evidence = store.put_artifact(b"revision one proof")?;
        claim_test_operation(&mut store, &final_read)?;
        store.operation_state(&final_read, "succeeded", Some(&current_evidence), json!({}))?;
        assert!(store
            .complete_run(
                &run.id,
                "done",
                &[old_evidence.clone(), current_evidence.clone()],
            )
            .is_err());
        assert_eq!(store.milestones(&run.id)?[0].state, "active");

        store.complete_run(&run.id, "done", &[current_evidence.clone()])?;
        assert_eq!(store.run(&run.id)?.state, "completed");
        assert_eq!(store.milestones(&run.id)?[0].state, "completed");
        assert_eq!(
            store.milestones(&run.id)?[0].evidence,
            vec![current_evidence]
        );
        Ok(())
    }
}
