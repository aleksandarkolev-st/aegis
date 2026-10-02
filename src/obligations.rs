use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::storage::{Operation, Run, Store, append_event};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Obligation {
    pub id: i64,
    pub title: String,
    pub state: String,
    pub evidence: Vec<String>,
    pub verified_revision: Option<i64>,
    pub superseded_by: Option<i64>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proof {
    pub id: i64,
    pub evidence: Vec<String>,
}

fn titles(configuration: &Value) -> Result<Vec<String>> {
    let Some(entries) = configuration.get("obligations") else {
        return Ok(Vec::new());
    };
    let entries = entries
        .as_array()
        .context("obligations must be a list of user-approved requirement strings")?;
    if entries.len() > 20 {
        bail!("a task may have at most 20 explicit obligations");
    }
    let mut titles = Vec::new();
    for entry in entries {
        let title = entry
            .as_str()
            .context("each obligation must be a requirement string")?
            .trim();
        if title.is_empty() || title.len() > 200 || crate::text::clean(title) != title {
            bail!("obligation titles must be safe, nonempty text of at most 200 bytes");
        }
        if title == "Task request" || titles.iter().any(|existing| existing == title) {
            bail!("obligation titles must be distinct from each other and the task request");
        }
        titles.push(title.to_owned());
    }
    Ok(titles)
}

pub fn from_task(task: &str) -> Result<Vec<String>> {
    let mut in_requirements = false;
    let mut saw_requirements = false;
    let mut entries = Vec::new();
    for line in task.lines() {
        let line = line.trim();
        if line.eq_ignore_ascii_case("requirements:") {
            in_requirements = true;
            saw_requirements = true;
            continue;
        }
        if !in_requirements {
            continue;
        }
        if line.is_empty() {
            if !entries.is_empty() {
                in_requirements = false;
            }
            continue;
        }
        let bullet = line
            .strip_prefix("- ")
            .or_else(|| line.strip_prefix("* "))
            .or_else(|| {
                let (number, text) = line.split_once(". ")?;
                (!number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()))
                    .then_some(text)
            });
        let Some(title) = bullet else {
            bail!("list each requirement under Requirements: with a bullet or number");
        };
        entries.push(Value::String(title.trim().to_owned()));
    }
    if saw_requirements && entries.is_empty() {
        bail!("Requirements: needs at least one listed requirement");
    }
    titles(&serde_json::json!({"obligations":entries}))
}

// Explicit configuration may add requirements, but cannot erase the user's list.
pub(crate) fn freeze_requirements(task: &str, configuration: &mut Value) -> Result<()> {
    let mut configured = titles(configuration)?;
    for title in from_task(task)? {
        if !configured.contains(&title) {
            configured.push(title);
        }
    }
    configuration["obligations"] = serde_json::json!(configured);
    titles(configuration)?;
    Ok(())
}

pub(crate) fn insert(transaction: &Transaction<'_>, run: &Run) -> Result<()> {
    let titles = titles(&run.budgets)?;
    transaction.execute(
        "INSERT INTO workspace_revisions(run_id, revision) VALUES (?1, 0)",
        [&run.id],
    )?;
    transaction.execute(
        "INSERT INTO obligations(run_id, id, title, state, evidence) VALUES (?1, 0, 'Task request', 'open', '[]')",
        [&run.id],
    )?;
    for (index, title) in titles.iter().enumerate() {
        transaction.execute(
            "INSERT INTO obligations(run_id, id, title, state, evidence) VALUES (?1, ?2, ?3, 'open', '[]')",
            params![run.id, index as i64 + 1, title],
        )?;
    }
    Ok(())
}

pub(crate) fn record_operation(
    transaction: &Transaction<'_>,
    operation: &Operation,
    may_have_run: bool,
    workspace_already_invalidated: bool,
) -> Result<()> {
    let Some(current) = transaction
        .query_row(
            "SELECT revision FROM workspace_revisions WHERE run_id = ?1",
            [&operation.run_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
    else {
        return Ok(());
    };
    let invalidates = !workspace_already_invalidated
        && may_have_run
        && might_mutate_workspace(transaction, &operation.run_id, &operation.capability)?;
    let revision = if invalidates {
        current
            .checked_add(1)
            .context("workspace revision overflow")?
    } else {
        current
    };
    if invalidates {
        invalidate_workspace(
            transaction,
            &operation.run_id,
            serde_json::json!({"operation":operation.id,"capability":operation.capability}),
        )?;
    }
    transaction.execute(
        "INSERT INTO operation_revisions(operation_id, run_id, revision) VALUES (?1, ?2, ?3)
         ON CONFLICT(operation_id) DO UPDATE SET run_id=excluded.run_id, revision=excluded.revision",
        params![operation.id, operation.run_id, revision],
    )?;
    Ok(())
}

const LEGACY_CONTRACT_REVIEW: &str = "This saved task needs a reviewed contract before it can continue. Open F3 and choose 'Review and adopt legacy task', or use /goal add to review and adopt it locally.";

pub(crate) fn reviewed_contract(connection: &Connection, run_id: &str) -> Result<bool> {
    let root_is_valid: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM obligations WHERE run_id=?1 AND id=0 AND title='Task request')",
        [run_id],
        |row| row.get(0),
    )?;
    if !root_is_valid {
        return Ok(false);
    }
    let has_revision: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM workspace_revisions WHERE run_id=?1)",
        [run_id],
        |row| row.get(0),
    )?;
    if !has_revision {
        return Ok(false);
    }

    let saved: Option<(String, String)> = connection
        .query_row(
            "SELECT task,budgets FROM runs WHERE id=?1",
            [run_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((task, budgets)) = saved else {
        return Ok(false);
    };
    let Ok(budgets) = serde_json::from_str::<Value>(&budgets) else {
        return Ok(false);
    };
    // Current runs freeze this key at creation, including an empty list. Its
    // absence distinguishes older runs that only happen to have a root row.
    if !budgets.get("obligations").is_some_and(Value::is_array) {
        return Ok(false);
    }
    let Ok(mut expected) = titles(&budgets) else {
        return Ok(false);
    };
    let Ok(from_task) = from_task(&task) else {
        return Ok(false);
    };
    for title in from_task {
        if !expected.contains(&title) {
            expected.push(title);
        }
    }
    if expected.is_empty() {
        return Ok(true);
    }
    let mut statement =
        connection.prepare("SELECT title FROM obligations WHERE run_id=?1 AND id>0")?;
    let retained = statement
        .query_map([run_id], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(expected
        .iter()
        .all(|title| retained.iter().any(|saved| saved == title)))
}

pub(crate) fn ensure_reviewed_contract(connection: &Connection, run_id: &str) -> Result<()> {
    if reviewed_contract(connection, run_id)? {
        return Ok(());
    }
    let has_root: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM obligations WHERE run_id=?1 AND id=0)",
        [run_id],
        |row| row.get(0),
    )?;
    if has_root {
        bail!(
            "This task's contract ledger is incomplete. Review and adopt it locally from F3 before continuing."
        );
    }
    bail!("{LEGACY_CONTRACT_REVIEW}");
}

fn might_mutate_workspace(
    connection: &rusqlite::Connection,
    run_id: &str,
    capability: &str,
) -> Result<bool> {
    Ok(match capability {
        "workspace.write" | "workspace.patch" => true,
        "process.run" => {
            let grants: String =
                connection.query_row("SELECT grants FROM runs WHERE id = ?1", [run_id], |row| {
                    row.get(0)
                })?;
            serde_json::from_str::<Vec<String>>(&grants)?
                .iter()
                .any(|grant| grant == "workspace.write")
        }
        capability if capability.starts_with("mcp.") => {
            let grants: String =
                connection.query_row("SELECT grants FROM runs WHERE id = ?1", [run_id], |row| {
                    row.get(0)
                })?;
            serde_json::from_str::<Vec<String>>(&grants)?
                .iter()
                .any(|grant| grant == "workspace.write")
        }
        _ => false,
    })
}

pub(crate) fn claim_operation_revision(
    transaction: &Transaction<'_>,
    run_id: &str,
    operation_id: &str,
    capability: &str,
) -> Result<Option<i64>> {
    if might_mutate_workspace(transaction, run_id, capability)? {
        invalidate_workspace(
            transaction,
            run_id,
            serde_json::json!({
                "operation":operation_id,
                "capability":capability,
                "phase":"claimed"
            }),
        )?;
    }
    transaction
        .query_row(
            "SELECT revision FROM workspace_revisions WHERE run_id = ?1",
            [run_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
}

pub(crate) fn workspace_mutation_in_flight(
    connection: &rusqlite::Connection,
    run_id: &str,
) -> Result<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(
            SELECT 1
            FROM runs AS source
            JOIN runs AS peer ON peer.workspace = source.workspace
            JOIN operations AS operation ON operation.run_id = peer.id
            WHERE source.id = ?1
              AND operation.state = 'executing'
              AND (
                  operation.capability IN ('workspace.write', 'workspace.patch')
                  OR (
                      (operation.capability = 'process.run' OR operation.capability LIKE 'mcp.%')
                      AND EXISTS (
                          SELECT 1 FROM json_each(peer.grants) AS grant_item
                          WHERE grant_item.value = 'workspace.write'
                      )
                  )
              )
        )",
        [run_id],
        |row| row.get(0),
    )?)
}

pub(crate) fn invalidate_workspace(
    transaction: &Transaction<'_>,
    source: &str,
    detail: Value,
) -> Result<()> {
    let affected = {
        let mut statement=transaction.prepare("SELECT run.id, revision.revision FROM runs AS run JOIN workspace_revisions AS revision ON revision.run_id=run.id WHERE run.workspace=(SELECT workspace FROM runs WHERE id=?1) AND (run.id=?1 OR run.state NOT IN ('completed','answered','cancelled','failed'))")?;
        statement
            .query_map([source], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, previous) in affected {
        let next = previous
            .checked_add(1)
            .context("workspace revision overflow")?;
        transaction.execute(
            "UPDATE workspace_revisions SET revision=?2 WHERE run_id=?1",
            params![id, next],
        )?;
        transaction.execute(
            "UPDATE obligations SET state='stale' WHERE run_id=?1 AND state='verified'",
            [&id],
        )?;
        let mut payload = detail.clone();
        payload["revision"] = serde_json::json!(next);
        payload["source_run"] = serde_json::json!(source);
        append_event(transaction, &id, "workspace.revision", payload)?;
    }
    Ok(())
}

pub(crate) fn validate_completion(store: &Store, run_id: &str) -> Result<()> {
    for item in store
        .obligations(run_id)?
        .iter()
        .filter(|item| item.id > 0 && item.state == "verified")
    {
        for hash in &item.evidence {
            store.artifact(hash)?;
        }
    }
    validate_connection(&store.connection, run_id)
}

pub(crate) fn validate_finish_evidence(
    connection: &rusqlite::Connection,
    run_id: &str,
    evidence: &[String],
) -> Result<()> {
    let revision: Option<i64> = connection
        .query_row(
            "SELECT revision FROM workspace_revisions WHERE run_id=?1",
            [run_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(revision) = revision else {
        // Pre-revision runs have no freshness ledger; callers still validate
        // that each supplied artifact belongs to a successful operation.
        return Ok(());
    };
    for hash in evidence {
        if !current_evidence(connection, run_id, revision, hash)? {
            bail!(
                "every completion artifact must be successful evidence from the current workspace revision"
            );
        }
    }
    Ok(())
}

pub(crate) fn validate_connection(connection: &rusqlite::Connection, run_id: &str) -> Result<()> {
    let mut statement = connection.prepare(
        "SELECT id, state, evidence, verified_revision, superseded_by, reason FROM obligations WHERE run_id = ?1 ORDER BY id",
    )?;
    let obligations = statement
        .query_map([run_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure_reviewed_contract(connection, run_id)?;
    if obligations.len() == 1 {
        return Ok(());
    }
    let revision: i64 = connection.query_row(
        "SELECT revision FROM workspace_revisions WHERE run_id = ?1",
        [run_id],
        |row| row.get(0),
    )?;
    for (id, state, evidence, verified_revision, replacement, reason) in
        obligations.iter().filter(|item| item.0 > 0)
    {
        if state == "superseded" {
            if reason.as_deref().is_none_or(|text| text.trim().is_empty())
                || !replacement
                    .is_some_and(|next| next > *id && obligations.iter().any(|item| item.0 == next))
            {
                bail!("superseded obligation lacks a valid replacement and reason");
            }
            continue;
        }
        if state != "verified" || *verified_revision != Some(revision) {
            bail!(
                "kernel obligations remain open or stale; verify them at the current workspace revision before finishing"
            );
        }
        let evidence: Vec<String> = serde_json::from_str(evidence)?;
        if evidence.is_empty() || evidence.len() > 20 {
            bail!("verified obligation lacks bounded evidence");
        }
        for hash in evidence {
            if !current_evidence(connection, run_id, revision, &hash)? {
                bail!(
                    "verified obligation evidence is no longer successful and current for this run"
                );
            }
        }
    }
    Ok(())
}

pub(crate) fn current_evidence(
    connection: &rusqlite::Connection,
    run_id: &str,
    revision: i64,
    hash: &str,
) -> Result<bool> {
    if workspace_mutation_in_flight(connection, run_id)? {
        return Ok(false);
    }
    Ok(connection.query_row(
        "SELECT EXISTS(
            SELECT 1
            FROM operations AS operation
            JOIN operation_revisions AS revision
              ON revision.operation_id = operation.id AND revision.run_id = operation.run_id
            JOIN runs AS run ON run.id = operation.run_id
            WHERE operation.run_id = ?1
              AND operation.state = 'succeeded'
              AND revision.revision = ?2
              AND (operation.artifact = ?3 OR EXISTS (
                  SELECT 1 FROM operation_artifacts AS linked
                  WHERE linked.operation_id = operation.id AND linked.hash = ?3
              ))
              AND operation.started_revision IS NOT NULL
              AND (
                  operation.started_revision = ?2
                  OR (
                      operation.started_revision + 1 = ?2
                      AND (
                          operation.capability IN ('workspace.write', 'workspace.patch')
                          OR (
                              (operation.capability = 'process.run' OR operation.capability LIKE 'mcp.%')
                              AND EXISTS (
                                  SELECT 1 FROM json_each(run.grants) AS grant_item
                                  WHERE grant_item.value = 'workspace.write'
                              )
                          )
                      )
                  )
              )
        )",
        params![run_id, revision, hash], |row| row.get(0),
    )?)
}

impl Store {
    pub fn needs_legacy_contract_adoption(&self, run_id: &str) -> Result<bool> {
        Ok(!reviewed_contract(&self.connection, run_id)?)
    }

    /// Adopt an older run only after the caller has reviewed its saved task and
    /// explicitly supplied the complete requirement list to retain.
    pub fn adopt_legacy_contract(
        &mut self,
        run_id: &str,
        reviewed_sequence: i64,
        reviewed_task: &str,
        reviewed_requirements: &[String],
        addition_reason: Option<&str>,
    ) -> Result<i64> {
        uuid::Uuid::parse_str(run_id).context("invalid run ID")?;
        let database = std::path::Path::new(
            self.connection
                .path()
                .context("Task database path missing")?,
        );
        let root = database.parent().context("Task database root missing")?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(root.join(format!("run-{run_id}.lock")))?;
        fs2::FileExt::try_lock_exclusive(&lock)
            .context("Pause the task before reviewing and adopting its legacy contract")?;

        let run = self.run(run_id)?;
        if run.task != reviewed_task {
            bail!("This saved task changed during review; review it again before adopting it");
        }
        if run.is_terminal() {
            bail!("Ended tasks cannot adopt a new contract");
        }
        if run.state == "running" {
            bail!("Pause the task before reviewing and adopting its legacy contract");
        }
        if reviewed_contract(&self.connection, run_id)? {
            bail!("This task already has a reviewed contract");
        }
        let sequence: i64 = self.connection.query_row(
            "SELECT last_seq FROM run_projection WHERE run_id=?1",
            [run_id],
            |row| row.get(0),
        )?;
        if sequence != reviewed_sequence {
            bail!("This saved task changed during review; review it again before adopting it");
        }

        let unresolved: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM operations WHERE run_id=?1 AND state NOT IN ('succeeded','failed','cancelled')",
            [run_id],
            |row| row.get(0),
        )?;
        if unresolved > 0
            || crate::obligations::workspace_mutation_in_flight(&self.connection, run_id)?
        {
            bail!(
                "Reconcile unfinished operations and wait for workspace changes to stop before adopting this task"
            );
        }

        let approved = titles(&serde_json::json!({"obligations":reviewed_requirements}))?;
        let mut required = titles(&run.budgets)?;
        for title in from_task(&run.task)? {
            if !required.contains(&title) {
                required.push(title);
            }
        }
        let previous_obligations = self.obligations(run_id)?;
        for item in previous_obligations
            .iter()
            .filter(|item| item.id > 0 && item.state != "superseded")
        {
            if !required.contains(&item.title) {
                required.push(item.title.clone());
            }
        }
        for title in &required {
            if !approved.contains(title) {
                bail!("The reviewed contract must retain this saved requirement: {title}");
            }
        }
        let added: Vec<_> = approved
            .iter()
            .filter(|title| !required.contains(title))
            .collect();
        let addition_reason = addition_reason.map(str::trim);
        if !added.is_empty()
            && addition_reason.is_none_or(|reason| {
                reason.is_empty() || reason.len() > 500 || crate::text::clean(reason) != reason
            })
        {
            bail!("A new requirement needs a safe, explicit reason");
        }

        let fingerprint = self.capture_workspace_fingerprint(run_id)?;
        let mut budgets = run.budgets.clone();
        budgets["obligations"] = serde_json::json!(approved.clone());
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (task, current_budgets, current_state): (String, String, String) = transaction
            .query_row(
                "SELECT task,budgets,state FROM runs WHERE id=?1",
                [run_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
        let current_sequence: i64 = transaction.query_row(
            "SELECT last_seq FROM run_projection WHERE run_id=?1",
            [run_id],
            |row| row.get(0),
        )?;
        if task != run.task
            || serde_json::from_str::<Value>(&current_budgets)? != run.budgets
            || current_state != run.state
            || current_sequence != reviewed_sequence
        {
            bail!("This saved task changed during review; review it again before adopting it");
        }
        if matches!(
            current_state.as_str(),
            "completed" | "answered" | "cancelled" | "failed"
        ) || current_state == "running"
        {
            bail!("Pause the task before reviewing and adopting its legacy contract");
        }
        if reviewed_contract(&transaction, run_id)? {
            bail!("This task already has a reviewed contract");
        }
        let unresolved: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM operations WHERE run_id=?1 AND state NOT IN ('succeeded','failed','cancelled')",
            [run_id],
            |row| row.get(0),
        )?;
        if unresolved > 0 || crate::obligations::workspace_mutation_in_flight(&transaction, run_id)?
        {
            bail!(
                "Reconcile unfinished operations and wait for workspace changes to stop before adopting this task"
            );
        }

        let old_revision: Option<i64> = transaction
            .query_row(
                "SELECT revision FROM workspace_revisions WHERE run_id=?1",
                [run_id],
                |row| row.get(0),
            )
            .optional()?;
        let revision = old_revision
            .unwrap_or(0)
            .checked_add(1)
            .context("workspace revision overflow")?;
        transaction.execute(
            "INSERT INTO workspace_revisions(run_id,revision) VALUES (?1,?2)
             ON CONFLICT(run_id) DO UPDATE SET revision=excluded.revision",
            params![run_id, revision],
        )?;
        transaction.execute("DELETE FROM operation_revisions WHERE run_id=?1", [run_id])?;
        transaction.execute(
            "UPDATE operations SET started_revision=NULL WHERE run_id=?1",
            [run_id],
        )?;
        transaction.execute(
            "UPDATE milestones SET state='pending',evidence='[]' WHERE run_id=?1 AND state='completed'",
            [run_id],
        )?;
        transaction.execute("DELETE FROM obligations WHERE run_id=?1", [run_id])?;
        transaction.execute(
            "INSERT INTO obligations(run_id,id,title,state,evidence) VALUES (?1,0,'Task request','open','[]')",
            [run_id],
        )?;
        for (index, title) in approved.iter().enumerate() {
            transaction.execute(
                "INSERT INTO obligations(run_id,id,title,state,evidence) VALUES (?1,?2,?3,'open','[]')",
                params![run_id, index as i64 + 1, title],
            )?;
        }
        transaction.execute(
            "UPDATE runs SET budgets=?2,state='paused' WHERE id=?1",
            params![run_id, budgets.to_string()],
        )?;
        transaction.execute(
            "UPDATE pause_requests SET pending=0 WHERE run_id=?1",
            [run_id],
        )?;
        crate::freshness::save_workspace_fingerprint(&transaction, run_id, &fingerprint, revision)?;

        let proposal: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM run_projection WHERE run_id=?1 AND proposal IS NOT NULL)",
            [run_id],
            |row| row.get(0),
        )?;
        if proposal {
            append_event(
                &transaction,
                run_id,
                "completion.resolved",
                serde_json::json!({"outcome":"legacy_contract_adopted"}),
            )?;
        }
        if current_state != "paused" {
            append_event(
                &transaction,
                run_id,
                "run.paused",
                serde_json::json!({"reason":"paused for legacy contract review"}),
            )?;
        }
        append_event(
            &transaction,
            run_id,
            "legacy.contract.adopted",
            serde_json::json!({
                "previous_state":current_state,
                "previous_obligations":previous_obligations.iter().map(|item| serde_json::json!({"id":item.id,"title":&item.title,"state":&item.state})).collect::<Vec<_>>(),
                "requirements":approved,
                "additional_requirement_reason":addition_reason,
                "workspace_revision":revision,
                "policy":"The original task and event history are retained. Previous operation artifacts remain historical and cannot prove the adopted contract."
            }),
        )?;
        transaction.commit()?;
        Ok(revision)
    }

    pub fn add_obligation(&mut self, run_id: &str, title: &str, reason: &str) -> Result<i64> {
        let title = title.trim();
        titles(&serde_json::json!({"obligations":[title]}))?;
        let reason = reason.trim();
        if reason.is_empty() || reason.len() > 500 || crate::text::clean(reason) != reason {
            bail!("Adding a requirement needs a safe, explicit reason");
        }
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let state: String =
            transaction.query_row("SELECT state FROM runs WHERE id=?1", [run_id], |row| {
                row.get(0)
            })?;
        if matches!(state.as_str(), "ready" | "running") {
            bail!("pause active tasks before changing obligations");
        }
        if matches!(
            state.as_str(),
            "completed" | "answered" | "cancelled" | "failed"
        ) {
            bail!("Ended tasks cannot change obligations");
        }
        let (active,total):(i64,i64)=transaction.query_row("SELECT SUM(CASE WHEN id>0 AND state!='superseded' THEN 1 ELSE 0 END),COUNT(*) FROM obligations WHERE run_id=?1",[run_id],|row|Ok((row.get::<_,Option<i64>>(0)?.unwrap_or(0),row.get(1)?)))?;
        if total == 0 {
            bail!("Legacy tasks need a new reviewed contract before adding requirements");
        }
        if active >= 20 || total >= 101 {
            bail!("At most 20 active and 100 retained explicit obligations");
        }
        let duplicate: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM obligations WHERE run_id=?1 AND title=?2)",
            params![run_id, title],
            |row| row.get(0),
        )?;
        if duplicate {
            bail!("Requirement already exists in the retained contract");
        }
        let id: i64 = transaction.query_row(
            "SELECT MAX(id)+1 FROM obligations WHERE run_id=?1",
            [run_id],
            |row| row.get(0),
        )?;
        transaction.execute(
            "INSERT INTO obligations(run_id,id,title,state,evidence) VALUES (?1,?2,?3,'open','[]')",
            params![run_id, id, title],
        )?;
        append_event(
            &transaction,
            run_id,
            "obligation.added",
            serde_json::json!({"id":id,"title":title,"reason":reason,"source":"user"}),
        )?;
        transaction.commit()?;
        Ok(id)
    }

    pub fn supersede_obligation(
        &mut self,
        run_id: &str,
        id: i64,
        replacement: &str,
        reason: &str,
    ) -> Result<i64> {
        let replacement = replacement.trim();
        let reason = reason.trim();
        if id <= 0
            || replacement.is_empty()
            || replacement.len() > 200
            || crate::text::clean(replacement) != replacement
            || reason.is_empty()
            || reason.len() > 500
            || crate::text::clean(reason) != reason
        {
            bail!("supersession requires a safe replacement and explicit reason");
        }
        if self.run(run_id)?.is_terminal() {
            bail!("completed tasks cannot change obligations");
        }
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let state: String =
            transaction.query_row("SELECT state FROM runs WHERE id = ?1", [run_id], |row| {
                row.get(0)
            })?;
        if matches!(state.as_str(), "ready" | "running") {
            bail!("pause active tasks before changing obligations");
        }
        if matches!(
            state.as_str(),
            "completed" | "answered" | "cancelled" | "failed"
        ) {
            bail!("completed tasks cannot change obligations");
        }
        let current: Option<(String, String)> = transaction
            .query_row(
                "SELECT title, state FROM obligations WHERE run_id = ?1 AND id = ?2",
                params![run_id, id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((old_title, state)) = current else {
            bail!("explicit obligation not found");
        };
        if state == "superseded" {
            bail!("obligation is already superseded");
        }
        if old_title == replacement {
            bail!("replacement must change the obligation");
        }
        let duplicate: i64 = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM obligations WHERE run_id = ?1 AND title = ?2)",
            params![run_id, replacement],
            |row| row.get(0),
        )?;
        if duplicate != 0 {
            bail!("replacement obligation already exists");
        }
        let count: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM obligations WHERE run_id = ?1",
            [run_id],
            |row| row.get(0),
        )?;
        if count >= 101 {
            bail!("a task may have at most 100 retained obligation records");
        }
        let new_id: i64 = transaction.query_row(
            "SELECT COALESCE(MAX(id), 0) + 1 FROM obligations WHERE run_id = ?1",
            [run_id],
            |row| row.get(0),
        )?;
        transaction.execute(
            "INSERT INTO obligations(run_id, id, title, state, evidence) VALUES (?1, ?2, ?3, 'open', '[]')",
            params![run_id, new_id, replacement],
        )?;
        transaction.execute(
            "UPDATE obligations SET state = 'superseded', superseded_by = ?3, reason = ?4 WHERE run_id = ?1 AND id = ?2",
            params![run_id, id, new_id, reason],
        )?;
        append_event(
            &transaction,
            run_id,
            "obligation.superseded",
            serde_json::json!({"id":id,"replacement_id":new_id,"old_title":old_title,"title":replacement,"reason":reason}),
        )?;
        transaction.commit()?;
        Ok(new_id)
    }

    pub fn verify_obligation(&mut self, run_id: &str, id: i64, evidence: &[String]) -> Result<()> {
        self.verify_obligations(
            run_id,
            &[Proof {
                id,
                evidence: evidence.to_vec(),
            }],
        )
    }

    pub fn verify_obligations(&mut self, run_id: &str, proofs: &[Proof]) -> Result<()> {
        ensure_reviewed_contract(&self.connection, run_id)?;
        self.refresh_observed_files(run_id)?;
        if proofs.is_empty() || proofs.len() > 20 {
            bail!("verify 1..20 explicit obligations at a time");
        }
        if self.run(run_id)?.state != "running" {
            bail!("only a running task can verify obligations");
        }
        for proof in proofs {
            for hash in &proof.evidence {
                self.artifact(hash)?;
            }
        }
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let state: String =
            transaction.query_row("SELECT state FROM runs WHERE id = ?1", [run_id], |row| {
                row.get(0)
            })?;
        if state != "running" {
            bail!("only a running task can verify obligations");
        }
        let revision: i64 = transaction.query_row(
            "SELECT revision FROM workspace_revisions WHERE run_id = ?1",
            [run_id],
            |row| row.get(0),
        )?;
        let mut seen = std::collections::HashSet::new();
        for proof in proofs {
            if proof.id <= 0
                || proof.evidence.is_empty()
                || proof.evidence.len() > 20
                || !seen.insert(proof.id)
            {
                bail!("select distinct explicit obligations and 1..20 evidence artifacts each");
            }
            let state: Option<String> = transaction
                .query_row(
                    "SELECT state FROM obligations WHERE run_id = ?1 AND id = ?2",
                    params![run_id, proof.id],
                    |row| row.get(0),
                )
                .optional()?;
            if state.is_none() || state.as_deref() == Some("superseded") {
                bail!("obligation is unavailable or superseded");
            }
            for hash in &proof.evidence {
                if !current_evidence(&transaction, run_id, revision, hash)? {
                    bail!("obligation evidence must be successful and current for this run");
                }
            }
        }
        for proof in proofs {
            transaction.execute(
                "UPDATE obligations SET state = 'verified', evidence = ?3, verified_revision = ?4 WHERE run_id = ?1 AND id = ?2",
                params![run_id, proof.id, serde_json::to_string(&proof.evidence)?, revision],
            )?;
            append_event(
                &transaction,
                run_id,
                "obligation.verified",
                serde_json::json!({"id":proof.id,"evidence":proof.evidence,"revision":revision}),
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn obligations(&self, run_id: &str) -> Result<Vec<Obligation>> {
        let mut statement = self.connection.prepare(
            "SELECT id, title, state, evidence, verified_revision, superseded_by, reason FROM obligations WHERE run_id = ?1 ORDER BY id",
        )?;
        let rows = statement.query_map([run_id], |row| {
            let evidence: String = row.get(3)?;
            Ok(Obligation {
                id: row.get(0)?,
                title: row.get(1)?,
                state: row.get(2)?,
                evidence: serde_json::from_str(&evidence).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        3,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?,
                verified_revision: row.get(4)?,
                superseded_by: row.get(5)?,
                reason: row.get(6)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn workspace_revision(&self, run_id: &str) -> Result<Option<i64>> {
        self.connection
            .query_row(
                "SELECT revision FROM workspace_revisions WHERE run_id = ?1",
                [run_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn edits_in_another_task_stale_live_peer_proofs_but_preserve_completed_history() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let mut runs = Vec::new();
        let mut hashes = Vec::new();
        for _ in 0..3 {
            let run = store.create_run(
                "work",
                directory.path(),
                "custom",
                serde_json::json!(["workspace.write"]),
                serde_json::json!({"obligations":["API passes"]}),
                "",
            )?;
            store.state(&run.id, "running", serde_json::json!({}))?;
            let op =
                store.begin_operation(&run.id, "workspace.read", serde_json::json!({}), true)?;
            let hash = store.put_artifact(run.id.as_bytes())?;
            crate::storage::claim_test_operation(&mut store, &op)?;
            store.operation_state(&op, "succeeded", Some(&hash), serde_json::json!({}))?;
            store.verify_obligation(&run.id, 1, &[hash.clone()])?;
            hashes.push(hash);
            runs.push(run);
        }
        store.complete_run(&runs[2].id, "done", &[hashes[2].clone()])?;
        let history = store.obligations(&runs[2].id)?;
        store.request_pause(&runs[1].id)?;
        crate::pause::boundary(&mut store, &runs[1].id)?;
        let edit =
            store.begin_operation(&runs[0].id, "workspace.write", serde_json::json!({}), false)?;
        store.operation_state(&edit, "dispatched", None, serde_json::json!({}))?;
        store.claim_operation(&edit)?;
        let hash = store.put_artifact(b"edit")?;
        store.operation_state(&edit, "succeeded", Some(&hash), serde_json::json!({}))?;
        assert_eq!(store.workspace_revision(&runs[0].id)?, Some(2));
        assert_eq!(store.workspace_revision(&runs[1].id)?, Some(2));
        assert_eq!(store.obligations(&runs[1].id)?[1].state, "stale");
        assert_eq!(store.obligations(&runs[2].id)?, history);
        store.resume_paused(&runs[1].id)?;
        store.state(&runs[1].id, "running", serde_json::json!({}))?;
        assert!(
            store
                .verify_obligation(&runs[1].id, 1, &[hashes[1].clone()])
                .is_err()
        );
        assert!(
            store
                .complete_run(&runs[1].id, "done", &[hashes[1].clone()])
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn evidence_from_reads_overlapping_a_peer_mutation_is_not_current() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let grants = json!(["workspace.read", "workspace.write"]);
        let reader = store.create_run(
            "Check parser behavior",
            directory.path(),
            "custom",
            grants.clone(),
            json!({"obligations":["Parser behavior is correct"]}),
            "",
        )?;
        let editor = store.create_run(
            "Edit parser",
            directory.path(),
            "custom",
            grants,
            json!({"obligations":["Parser edit is correct"]}),
            "",
        )?;
        store.state(&reader.id, "running", json!({}))?;
        store.state(&editor.id, "running", json!({}))?;

        let early_read = store.begin_operation(
            &reader.id,
            "workspace.search",
            json!({"query":"parser"}),
            true,
        )?;
        crate::storage::claim_test_operation(&mut store, &early_read)?;

        let edit = store.begin_operation(
            &editor.id,
            "workspace.write",
            json!({"path":"parser.rs","content":"updated"}),
            false,
        )?;
        store.operation_state(&edit, "dispatched", None, json!({}))?;
        store.claim_operation(&edit)?;
        assert_eq!(store.workspace_revision(&reader.id)?, Some(1));

        let read_after_claim = store.begin_operation(
            &reader.id,
            "workspace.search",
            json!({"query":"parser"}),
            true,
        )?;
        crate::storage::claim_test_operation(&mut store, &read_after_claim)?;

        let early_artifact = store.put_artifact(b"search started before the edit")?;
        store.operation_state(&early_read, "succeeded", Some(&early_artifact), json!({}))?;
        let overlapping_artifact = store.put_artifact(b"search started during the edit")?;
        store.operation_state(
            &read_after_claim,
            "succeeded",
            Some(&overlapping_artifact),
            json!({}),
        )?;
        assert!(
            store
                .verify_obligation(&reader.id, 1, &[early_artifact.clone()])
                .is_err()
        );
        assert!(
            store
                .validate_completion(&reader.id, &[early_artifact.clone()])
                .is_err()
        );
        assert!(
            store
                .complete_run(&reader.id, "done", &[early_artifact.clone()])
                .is_err()
        );

        let edit_artifact = store.put_artifact(b"write result")?;
        store.operation_state(&edit, "succeeded", Some(&edit_artifact), json!({}))?;
        assert_eq!(store.workspace_revision(&reader.id)?, Some(2));
        store.verify_obligation(&editor.id, 1, &[edit_artifact.clone()])?;
        assert_eq!(store.obligations(&editor.id)?[1].state, "verified");

        assert!(
            store
                .verify_obligation(&reader.id, 1, &[early_artifact])
                .is_err()
        );
        assert!(
            store
                .verify_obligation(&reader.id, 1, &[overlapping_artifact])
                .is_err()
        );
        assert_eq!(store.obligations(&reader.id)?[1].state, "open");

        let fresh_read = store.begin_operation(
            &reader.id,
            "workspace.search",
            json!({"query":"parser"}),
            true,
        )?;
        crate::storage::claim_test_operation(&mut store, &fresh_read)?;
        let fresh_artifact = store.put_artifact(b"search started after the edit")?;
        store.operation_state(&fresh_read, "succeeded", Some(&fresh_artifact), json!({}))?;
        store.verify_obligation(&reader.id, 1, &[fresh_artifact])?;
        assert_eq!(store.obligations(&reader.id)?[1].state, "verified");
        Ok(())
    }

    #[test]
    fn configured_requirements_cannot_suppress_task_requirements() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let task = "Fix parser\nRequirements:\n- nested expressions\n- preserve API";
        for configured in [
            json!([]),
            json!(["nested expressions"]),
            json!(["run tests"]),
        ] {
            let run = store.create_run(
                task,
                directory.path(),
                "codex",
                json!([]),
                json!({"obligations":configured}),
                "",
            )?;
            let ledger = store.obligations(&run.id)?;
            assert!(ledger.iter().any(|item| item.title == "nested expressions"));
            assert!(ledger.iter().any(|item| item.title == "preserve API"));
        }
        assert!(
            store
                .create_run(
                    "Fix\nRequirements:\nmalformed",
                    directory.path(),
                    "codex",
                    json!([]),
                    json!({"obligations":[]}),
                    ""
                )
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn completion_rechecks_provenance_and_supersession_inside_transaction() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "work",
            directory.path(),
            "codex",
            json!([]),
            json!({"obligations":["Run tests"]}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let operation = store.begin_operation(&run.id, "workspace.read", json!({}), true)?;
        let evidence = store.put_artifact(b"test receipt")?;
        crate::storage::claim_test_operation(&mut store, &operation)?;
        store.operation_state(&operation, "succeeded", Some(&evidence), json!({}))?;
        store.verify_obligation(&run.id, 1, &[evidence.clone()])?;
        // Simulate a corrupted operation row so completion validation proves it
        // rechecks provenance even when a prior successful receipt is attached.
        store.connection.execute(
            "UPDATE operations SET state='failed' WHERE id=?1",
            [&operation.id],
        )?;
        assert!(
            store
                .complete_run(&run.id, "done", &[evidence.clone()])
                .is_err()
        );
        assert!(validate_connection(&store.connection, &run.id).is_err());
        store.connection.execute(
            "UPDATE operations SET state='succeeded' WHERE id=?1",
            [&operation.id],
        )?;
        store.state(&run.id, "paused", json!({}))?;
        let replacement = store.supersede_obligation(&run.id, 1, "Run all tests", "Approved")?;
        store.state(&run.id, "running", json!({}))?;
        store.verify_obligation(&run.id, replacement, &[evidence.clone()])?;
        store.connection.execute(
            "UPDATE obligations SET reason = '' WHERE run_id = ?1 AND id = 1",
            [&run.id],
        )?;
        assert!(validate_connection(&store.connection, &run.id).is_err());
        assert!(store.complete_run(&run.id, "done", &[evidence]).is_err());
        assert_eq!(store.run(&run.id)?.state, "running");
        Ok(())
    }

    #[test]
    fn user_requirement_bullets_become_immutable_run_obligations() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let task = "Refactor the parser.\n\nRequirements:\n- support nested expressions\n- preserve public API compatibility\n- add regression tests\n- full test suite must pass";
        let run = store.create_run(task, directory.path(), "codex", json!([]), json!({}), "")?;
        let obligations = store.obligations(&run.id)?;
        assert_eq!(obligations.len(), 5);
        assert_eq!(obligations[1].title, "support nested expressions");
        assert_eq!(obligations[4].title, "full test suite must pass");
        assert_eq!(run.budgets["obligations"].as_array().unwrap().len(), 4);
        assert_eq!(
            from_task("Explain how parsing works")?,
            Vec::<String>::new()
        );
        assert!(from_task("Do work\nRequirements:\n- first\nsecond without bullet").is_err());
        assert!(from_task("Do work\nRequirements:").is_err());
        Ok(())
    }

    #[test]
    fn explicit_obligations_survive_restart_and_model_replanning() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "Refactor the parser",
            directory.path(),
            "codex",
            json!([]),
            json!({"obligations":["Support nested expressions","Preserve public API","Add regression tests","Full suite passes"]}),
            "",
        )?;
        let original = store.obligations(&run.id)?;
        assert_eq!(original.len(), 5);
        assert_eq!(original[1].title, "Support nested expressions");
        assert!(original.iter().all(|obligation| obligation.state == "open"));
        assert_eq!(store.workspace_revision(&run.id)?, Some(0));
        store.save_checkpoint(
            &run.id,
            &crate::model::Checkpoint {
                decisions: vec![],
                unresolved: vec![],
                next_action: "compile".into(),
                milestones: vec![crate::model::Milestone {
                    title: "Implementation compiles".into(),
                    state: "active".into(),
                    evidence: vec![],
                }],
            },
        )?;
        assert_eq!(store.obligations(&run.id)?, original);
        drop(store);
        assert_eq!(
            Store::open(directory.path())?.obligations(&run.id)?,
            original
        );
        Ok(())
    }

    #[test]
    fn malformed_obligation_contract_is_rejected_before_run_creation() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        for configuration in [
            json!({"obligations":"not a list"}),
            json!({"obligations":[""]}),
            json!({"obligations":["same","same"]}),
            json!({"obligations":["Task request"]}),
            json!({"obligations":["a".repeat(201)]}),
        ] {
            assert!(
                store
                    .create_run(
                        "work",
                        directory.path(),
                        "codex",
                        json!([]),
                        configuration,
                        ""
                    )
                    .is_err()
            );
        }
        assert!(store.runs()?.is_empty());
        Ok(())
    }

    #[test]
    fn outstanding_obligations_block_finish_even_after_a_rewritten_plan() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "Refactor parser",
            directory.path(),
            "codex",
            json!([]),
            json!({"obligations":["Nested expressions","Public API compatibility"]}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let operation = store.begin_operation(&run.id, "workspace.read", json!({}), true)?;
        let evidence = store.put_artifact(b"fixture evidence")?;
        crate::storage::claim_test_operation(&mut store, &operation)?;
        store.operation_state(&operation, "succeeded", Some(&evidence), json!({}))?;
        store.verify_obligation(&run.id, 1, &[evidence.clone()])?;
        assert!(
            store
                .complete_run(&run.id, "done", &[evidence.clone()])
                .is_err()
        );
        assert!(store.answer_run(&run.id, "done without evidence").is_err());
        store.save_checkpoint(
            &run.id,
            &crate::model::Checkpoint {
                decisions: vec![],
                unresolved: vec![],
                next_action: "finish".into(),
                milestones: vec![crate::model::Milestone {
                    title: "Implementation compiles".into(),
                    state: "completed".into(),
                    evidence: vec![evidence.clone()],
                }],
            },
        )?;
        assert!(
            store
                .complete_run(&run.id, "done", &[evidence.clone()])
                .is_err()
        );
        store.verify_obligation(&run.id, 2, &[evidence.clone()])?;
        store.complete_run(&run.id, "done", &[evidence])?;
        assert_eq!(store.run(&run.id)?.state, "completed");
        assert_eq!(store.obligations(&run.id)?[0].state, "verified");
        Ok(())
    }

    #[test]
    fn committed_workspace_mutation_stales_evidence_and_survives_restart() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "Refactor parser",
            directory.path(),
            "codex",
            json!(["workspace.write"]),
            json!({"obligations":["Full suite passes"]}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let check = store.begin_operation(&run.id, "workspace.read", json!({}), true)?;
        let old_evidence = store.put_artifact(b"old test result")?;
        crate::storage::claim_test_operation(&mut store, &check)?;
        store.operation_state(&check, "succeeded", Some(&old_evidence), json!({}))?;
        store.verify_obligation(&run.id, 1, &[old_evidence.clone()])?;
        let edit = store.begin_operation(&run.id, "workspace.write", json!({}), false)?;
        store.operation_state(&edit, "dispatched", None, json!({}))?;
        store.claim_operation(&edit)?;
        let edit_evidence = store.put_artifact(b"new file result")?;
        store.operation_state(&edit, "succeeded", Some(&edit_evidence), json!({}))?;
        assert_eq!(store.workspace_revision(&run.id)?, Some(2));
        assert_eq!(store.obligations(&run.id)?[1].state, "stale");
        assert!(
            store
                .verify_obligation(&run.id, 1, &[old_evidence.clone()])
                .is_err()
        );
        assert!(
            store
                .complete_run(&run.id, "done", &[edit_evidence.clone()])
                .is_err()
        );
        store.operation_state(&edit, "succeeded", Some(&edit_evidence), json!({}))?;
        assert_eq!(store.workspace_revision(&run.id)?, Some(2));
        drop(store);
        let mut store = Store::open(directory.path())?;
        assert_eq!(store.obligations(&run.id)?[1].state, "stale");
        let current_check = store.begin_operation(&run.id, "workspace.read", json!({}), true)?;
        let current_evidence = store.put_artifact(b"current test result")?;
        crate::storage::claim_test_operation(&mut store, &current_check)?;
        store.operation_state(
            &current_check,
            "succeeded",
            Some(&current_evidence),
            json!({}),
        )?;
        store.verify_obligation(&run.id, 1, &[current_evidence.clone()])?;
        store.complete_run(&run.id, "done", &[current_evidence])?;
        assert_eq!(store.obligations(&run.id)?[1].verified_revision, Some(2));
        Ok(())
    }

    #[test]
    fn obligation_proof_batch_rejects_stale_or_foreign_evidence_atomically() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "Repair parser",
            directory.path(),
            "codex",
            json!([]),
            json!({"obligations":["Nested expressions","API compatibility"]}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let operation = store.begin_operation(&run.id, "workspace.read", json!({}), true)?;
        let evidence = store.put_artifact(b"current receipt")?;
        crate::storage::claim_test_operation(&mut store, &operation)?;
        store.operation_state(&operation, "succeeded", Some(&evidence), json!({}))?;
        let invalid = [
            Proof {
                id: 1,
                evidence: vec![evidence.clone()],
            },
            Proof {
                id: 2,
                evidence: vec!["0".repeat(64)],
            },
        ];
        assert!(store.verify_obligations(&run.id, &invalid).is_err());
        assert!(
            store
                .obligations(&run.id)?
                .iter()
                .all(|item| item.state == "open")
        );
        assert_eq!(store.event_count(&run.id, "obligation.verified")?, 0);
        Ok(())
    }

    #[test]
    fn approved_supersession_retains_predecessor_and_opens_replacement() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "Refactor parser",
            directory.path(),
            "codex",
            json!([]),
            json!({"obligations":["Preserve public API"]}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let operation = store.begin_operation(&run.id, "workspace.read", json!({}), true)?;
        let evidence = store.put_artifact(b"api compatibility proof")?;
        crate::storage::claim_test_operation(&mut store, &operation)?;
        store.operation_state(&operation, "succeeded", Some(&evidence), json!({}))?;
        store.verify_obligation(&run.id, 1, &[evidence.clone()])?;
        assert!(
            store
                .supersede_obligation(&run.id, 1, "Allow v2 API", "")
                .is_err()
        );
        assert!(
            store
                .supersede_obligation(&run.id, 0, "Change task", "user approved")
                .is_err()
        );
        assert_eq!(store.obligations(&run.id)?.len(), 2);
        store.state(&run.id, "paused", json!({}))?;
        let replacement = store.supersede_obligation(
            &run.id,
            1,
            "Allow v2 API",
            "User approved a breaking API change",
        )?;
        assert_eq!(replacement, 2);
        let ledger = store.obligations(&run.id)?;
        assert_eq!(ledger[1].state, "superseded");
        assert_eq!(ledger[1].evidence, vec![evidence.clone()]);
        assert_eq!(ledger[1].superseded_by, Some(2));
        assert_eq!(
            ledger[1].reason.as_deref(),
            Some("User approved a breaking API change")
        );
        assert_eq!(ledger[2].state, "open");
        assert!(
            store
                .complete_run(&run.id, "done", &[evidence.clone()])
                .is_err()
        );
        assert!(
            store
                .supersede_obligation(&run.id, 1, "Again", "user approved")
                .is_err()
        );
        assert_eq!(store.event_count(&run.id, "obligation.superseded")?, 1);
        store.state(&run.id, "running", json!({}))?;
        drop(store);
        let mut store = Store::open(directory.path())?;
        assert_eq!(store.obligations(&run.id)?, ledger);
        store.verify_obligation(&run.id, 2, &[evidence.clone()])?;
        store.complete_run(&run.id, "done", &[evidence])?;
        assert_eq!(store.run(&run.id)?.state, "completed");
        Ok(())
    }

    #[test]
    fn obligation_edits_are_rejected_while_running_and_allowed_after_pause() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "Refactor parser",
            directory.path(),
            "codex",
            json!([]),
            json!({"obligations":["Preserve public API"]}),
            "",
        )?;
        let original = store.obligations(&run.id)?;

        for state in ["ready", "running"] {
            store.state(&run.id, state, json!({}))?;
            assert!(
                store
                    .add_obligation(&run.id, "Add regression tests", "User approved")
                    .is_err()
            );
            assert!(
                store
                    .supersede_obligation(&run.id, 1, "Allow a v2 API", "User approved")
                    .is_err()
            );
            assert_eq!(store.obligations(&run.id)?, original);
            assert_eq!(store.event_count(&run.id, "obligation.added")?, 0);
            assert_eq!(store.event_count(&run.id, "obligation.superseded")?, 0);
        }

        store.request_pause(&run.id)?;
        assert!(crate::pause::boundary(&mut store, &run.id)?);
        assert_eq!(store.run(&run.id)?.state, "paused");

        store.add_obligation(&run.id, "Add regression tests", "User approved")?;
        store.supersede_obligation(&run.id, 1, "Allow a v2 API", "User approved")?;
        assert_eq!(store.event_count(&run.id, "obligation.added")?, 1);
        assert_eq!(store.event_count(&run.id, "obligation.superseded")?, 1);
        Ok(())
    }

    #[test]
    fn a_full_initial_contract_can_still_replace_one_requirement() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let titles: Vec<_> = (0..20)
            .map(|index| format!("Requirement {index}"))
            .collect();
        let run = store.create_run(
            "work",
            directory.path(),
            "codex",
            json!([]),
            json!({"obligations":titles}),
            "",
        )?;
        store.state(&run.id, "paused", json!({}))?;
        assert_eq!(
            store.supersede_obligation(&run.id, 1, "Updated requirement", "User approved")?,
            21
        );
        assert_eq!(store.obligations(&run.id)?.len(), 22);
        Ok(())
    }

    #[test]
    fn failed_write_capable_operation_stales_prior_proofs_once() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(&directory.path().join(".arun"))?;
        let run = store.create_run(
            "Refactor parser",
            directory.path(),
            "codex",
            json!(["workspace.write"]),
            json!({"obligations":["Full suite passes"]}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let check = store.begin_operation(&run.id, "workspace.read", json!({}), true)?;
        let receipt = store.put_artifact(b"test pass before edit")?;
        crate::storage::claim_test_operation(&mut store, &check)?;
        store.operation_state(&check, "succeeded", Some(&receipt), json!({}))?;
        store.verify_obligation(&run.id, 1, &[receipt.clone()])?;
        let command = store.begin_operation(&run.id, "process.run", json!({}), true)?;
        store.operation_state(&command, "dispatched", None, json!({}))?;
        store.operation_state(&command, "failed", None, json!({"exit_code":1}))?;
        assert_eq!(store.workspace_revision(&run.id)?, Some(1));
        assert_eq!(store.obligations(&run.id)?[1].state, "stale");
        assert!(store.verify_obligation(&run.id, 1, &[receipt]).is_err());
        store.operation_state(&command, "failed", None, json!({}))?;
        assert_eq!(store.workspace_revision(&run.id)?, Some(1));
        let undispatched = store.begin_operation(&run.id, "workspace.write", json!({}), false)?;
        store.operation_state(&undispatched, "cancelled", None, json!({}))?;
        assert_eq!(store.workspace_revision(&run.id)?, Some(1));
        let undispatched_process =
            store.begin_operation(&run.id, "process.run", json!({}), true)?;
        store.operation_state(&undispatched_process, "cancelled", None, json!({}))?;
        assert_eq!(store.workspace_revision(&run.id)?, Some(1));
        Ok(())
    }

    #[test]
    fn completion_waits_for_in_flight_write_and_unknown_outcome() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "Refactor parser",
            directory.path(),
            "codex",
            json!(["workspace.write"]),
            json!({"obligations":["Full suite passes"]}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let check = store.begin_operation(&run.id, "workspace.read", json!({}), true)?;
        let old_evidence = store.put_artifact(b"passing suite before edit")?;
        crate::storage::claim_test_operation(&mut store, &check)?;
        store.operation_state(&check, "succeeded", Some(&old_evidence), json!({}))?;
        store.verify_obligation(&run.id, 1, &[old_evidence.clone()])?;

        let edit = store.begin_operation(&run.id, "workspace.write", json!({}), false)?;
        for state in ["pending", "dispatched"] {
            if state == "dispatched" {
                store.operation_state(&edit, state, None, json!({}))?;
            }
            assert!(
                store
                    .validate_completion(&run.id, &[old_evidence.clone()])
                    .is_err()
            );
            assert!(
                store
                    .complete_run(&run.id, "done", &[old_evidence.clone()])
                    .is_err()
            );
        }
        store.operation_state(&edit, "outcome_unknown", None, json!({}))?;
        assert_eq!(store.obligations(&run.id)?[1].state, "stale");
        assert!(
            store
                .validate_completion(&run.id, &[old_evidence.clone()])
                .is_err()
        );
        assert!(
            store
                .complete_run(&run.id, "done", &[old_evidence])
                .is_err()
        );

        store.resolve_unknown(
            &run.id,
            &edit.id,
            false,
            "external check found no committed edit",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let recheck = store.begin_operation(&run.id, "workspace.read", json!({}), true)?;
        let current_evidence = store.put_artifact(b"passing suite after reconciliation")?;
        crate::storage::claim_test_operation(&mut store, &recheck)?;
        store.operation_state(&recheck, "succeeded", Some(&current_evidence), json!({}))?;
        store.verify_obligation(&run.id, 1, &[current_evidence.clone()])?;
        store.complete_run(&run.id, "done", &[current_evidence])?;
        assert_eq!(store.run(&run.id)?.state, "completed");
        Ok(())
    }

    #[test]
    fn legacy_adoption_preserves_history_and_requires_fresh_evidence() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let workspace = directory.path().join("workspace");
        std::fs::create_dir_all(&workspace)?;
        std::fs::write(workspace.join("source.txt"), b"saved source")?;
        let root = directory.path().join("state");
        let mut store = Store::open(&root)?;
        let task = "Repair the saved parser\nRequirements:\n- preserve nested expressions\n- add regression tests";
        let run = store.create_run(
            task,
            &workspace,
            "custom",
            json!(["workspace.read"]),
            json!({"obligations":["preserve public API"]}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let operation = store.begin_operation(
            &run.id,
            "workspace.read",
            json!({"path":"source.txt"}),
            true,
        )?;
        let old_evidence = store.put_artifact(b"old successful source read")?;
        crate::storage::claim_test_operation(&mut store, &operation)?;
        store.operation_state(&operation, "succeeded", Some(&old_evidence), json!({}))?;
        store.state(&run.id, "paused", json!({"fixture":true}))?;
        let original_events = store.events(&run.id)?;

        store
            .connection
            .execute("DELETE FROM obligations WHERE run_id=?1", [&run.id])?;
        store
            .connection
            .execute("DELETE FROM workspace_revisions WHERE run_id=?1", [&run.id])?;
        store.connection.execute(
            "DELETE FROM workspace_fingerprints WHERE run_id=?1",
            [&run.id],
        )?;
        assert!(store.needs_legacy_contract_adoption(&run.id)?);
        let run = store.run(&run.id)?;
        let mut requirements = run.budgets["obligations"]
            .as_array()
            .context("frozen requirements missing")?
            .iter()
            .map(|item| item.as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        requirements.push("enforce stable output schema".into());
        let sequence = original_events.last().unwrap().seq;
        let revision = store.adopt_legacy_contract(
            &run.id,
            sequence,
            &run.task,
            &requirements,
            Some("Keep the parser output compatible with its callers"),
        )?;

        let adopted = store.run(&run.id)?;
        assert_eq!(adopted.task, task);
        assert_eq!(adopted.state, "paused");
        assert!(!store.needs_legacy_contract_adoption(&run.id)?);
        assert_eq!(store.workspace_revision(&run.id)?, Some(revision));
        assert_eq!(revision, 1);
        let baseline: Option<(String, i64)> = store
            .connection
            .query_row(
                "SELECT fingerprint,revision FROM workspace_fingerprints WHERE run_id=?1",
                [&run.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let (fingerprint, baseline_revision) = baseline.context("adoption fingerprint missing")?;
        assert_eq!(baseline_revision, revision);
        assert_eq!(fingerprint.len(), 64);

        let obligations = store.obligations(&run.id)?;
        assert_eq!(obligations.len(), requirements.len() + 1);
        assert_eq!(obligations[0].id, 0);
        assert!(
            obligations
                .iter()
                .all(|item| item.state == "open" && item.evidence.is_empty())
        );
        assert_eq!(
            serde_json::to_value(&store.events(&run.id)?[..original_events.len()])?,
            serde_json::to_value(&original_events)?
        );
        assert_eq!(
            store.events(&run.id)?.last().unwrap().kind,
            "legacy.contract.adopted"
        );
        assert_eq!(
            store.connection.query_row(
                "SELECT COUNT(*) FROM operation_revisions WHERE run_id=?1",
                [&run.id],
                |row| row.get::<_, i64>(0),
            )?,
            0
        );
        assert_eq!(
            store.connection.query_row(
                "SELECT started_revision FROM operations WHERE id=?1",
                [&operation.id],
                |row| row.get::<_, Option<i64>>(0),
            )?,
            None
        );
        store.state(&run.id, "running", json!({}))?;
        assert!(
            store
                .verify_obligation(&run.id, 1, &[old_evidence])
                .is_err()
        );
        let fresh_operation = store.begin_operation(
            &run.id,
            "workspace.read",
            json!({"path":"source.txt"}),
            true,
        )?;
        let fresh_evidence = store.put_artifact(b"fresh post-adoption read")?;
        crate::storage::claim_test_operation(&mut store, &fresh_operation)?;
        store.operation_state(
            &fresh_operation,
            "succeeded",
            Some(&fresh_evidence),
            json!({}),
        )?;
        let proofs = store
            .obligations(&run.id)?
            .into_iter()
            .filter(|item| item.id > 0)
            .map(|item| Proof {
                id: item.id,
                evidence: vec![fresh_evidence.clone()],
            })
            .collect::<Vec<_>>();
        store.verify_obligations(&run.id, &proofs)?;
        store.complete_run(&run.id, "done", &[fresh_evidence])?;
        assert_eq!(store.run(&run.id)?.state, "completed");
        Ok(())
    }

    #[test]
    fn legacy_runs_cannot_resume_answer_or_finish_before_adoption() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let workspace = directory.path().join("workspace");
        std::fs::create_dir_all(&workspace)?;
        let mut store = Store::open(&directory.path().join("state"))?;
        let run = store.create_run(
            "Answer the saved request",
            &workspace,
            "custom",
            json!([]),
            json!({}),
            "",
        )?;
        store.state(&run.id, "paused", json!({}))?;
        store
            .connection
            .execute("DELETE FROM obligations WHERE run_id=?1", [&run.id])?;
        store
            .connection
            .execute("DELETE FROM workspace_revisions WHERE run_id=?1", [&run.id])?;

        let resume_error = store.resume_paused(&run.id).unwrap_err().to_string();
        assert!(resume_error.contains("F3"));
        assert_eq!(store.run(&run.id)?.state, "paused");
        assert!(
            store
                .complete_run(&run.id, "done", &[])
                .unwrap_err()
                .to_string()
                .contains("reviewed contract")
        );
        store.state(&run.id, "running", json!({}))?;
        assert!(
            store
                .answer_run(&run.id, "old task answer")
                .unwrap_err()
                .to_string()
                .contains("reviewed contract")
        );
        Ok(())
    }

    #[test]
    fn partial_contract_ledger_is_rejected_and_can_be_adopted() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let workspace = directory.path().join("workspace");
        std::fs::create_dir_all(&workspace)?;
        let mut store = Store::open(&directory.path().join("state"))?;
        let task = "Repair parser\nRequirements:\n- preserve public API\n- add regression tests";
        let run = store.create_run(task, &workspace, "custom", json!([]), json!({}), "")?;
        store.state(&run.id, "paused", json!({}))?;
        let sequence = store.events(&run.id)?.last().unwrap().seq;
        let adopted_requirements = store.run(&run.id)?.budgets["obligations"]
            .as_array()
            .context("task requirements were not frozen")?
            .iter()
            .map(|item| item.as_str().unwrap().to_owned())
            .collect::<Vec<_>>();

        // A root row and workspace revision do not make a partial ledger safe.
        store.connection.execute(
            "DELETE FROM obligations WHERE run_id=?1 AND id=2",
            [&run.id],
        )?;
        assert!(!reviewed_contract(&store.connection, &run.id)?);
        assert!(store.needs_legacy_contract_adoption(&run.id)?);
        let incomplete = ensure_reviewed_contract(&store.connection, &run.id)
            .unwrap_err()
            .to_string();
        assert!(incomplete.contains("ledger is incomplete"));

        let revision =
            store.adopt_legacy_contract(&run.id, sequence, task, &adopted_requirements, None)?;
        assert_eq!(revision, 1);
        assert!(reviewed_contract(&store.connection, &run.id)?);
        assert_eq!(store.obligations(&run.id)?.len(), 3);

        // A missing task root is also treated as incomplete, even after adoption.
        store.connection.execute(
            "DELETE FROM obligations WHERE run_id=?1 AND id=0",
            [&run.id],
        )?;
        assert!(!reviewed_contract(&store.connection, &run.id)?);
        assert!(
            ensure_reviewed_contract(&store.connection, &run.id)
                .unwrap_err()
                .to_string()
                .contains("Open F3")
        );
        Ok(())
    }

    #[test]
    fn legacy_adoption_rejects_stale_review_and_active_run_lock() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let workspace = directory.path().join("workspace");
        std::fs::create_dir_all(&workspace)?;
        let root = directory.path().join("state");
        let mut store = Store::open(&root)?;
        let run = store.create_run(
            "Review this saved task",
            &workspace,
            "custom",
            json!([]),
            json!({}),
            "",
        )?;
        store.state(&run.id, "paused", json!({}))?;
        store
            .connection
            .execute("DELETE FROM obligations WHERE run_id=?1", [&run.id])?;
        let reviewed = store.run(&run.id)?;
        let requirements = Vec::new();
        let reviewed_sequence = store.events(&run.id)?.last().unwrap().seq;

        let stale_task = store
            .adopt_legacy_contract(
                &run.id,
                reviewed_sequence,
                "Changed while the user reviewed it",
                &requirements,
                None,
            )
            .unwrap_err()
            .to_string();
        assert!(stale_task.contains("changed during review"));

        store.event(&run.id, "review.changed", json!({}))?;
        let stale_sequence = store
            .adopt_legacy_contract(
                &run.id,
                reviewed_sequence,
                &reviewed.task,
                &requirements,
                None,
            )
            .unwrap_err()
            .to_string();
        assert!(stale_sequence.contains("changed during review"));

        let current_sequence = store.events(&run.id)?.last().unwrap().seq;
        let lock_path = root.join(format!("run-{}.lock", run.id));
        let held_lock = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(lock_path)?;
        fs2::FileExt::try_lock_exclusive(&held_lock)?;
        let active = store
            .adopt_legacy_contract(
                &run.id,
                current_sequence,
                &reviewed.task,
                &requirements,
                None,
            )
            .unwrap_err()
            .to_string();
        assert!(active.contains("Pause the task"));
        assert!(store.needs_legacy_contract_adoption(&run.id)?);
        fs2::FileExt::unlock(&held_lock)?;
        Ok(())
    }
}
