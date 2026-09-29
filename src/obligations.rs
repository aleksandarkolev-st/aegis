use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension, Transaction, params};
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
) -> Result<()> {
    let already_recorded: i64 = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM operation_revisions WHERE operation_id = ?1)",
        [&operation.id],
        |row| row.get(0),
    )?;
    if already_recorded != 0 {
        return Ok(());
    }
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
    let might_mutate = match operation.capability.as_str() {
        "workspace.write" | "workspace.patch" => true,
        "process.run" => {
            let grants: String = transaction.query_row(
                "SELECT grants FROM runs WHERE id = ?1",
                [&operation.run_id],
                |row| row.get(0),
            )?;
            serde_json::from_str::<Vec<String>>(&grants)?
                .iter()
                .any(|grant| grant == "workspace.write")
        }
        capability if capability.starts_with("mcp.") => {
            let grants: String = transaction.query_row(
                "SELECT grants FROM runs WHERE id = ?1",
                [&operation.run_id],
                |row| row.get(0),
            )?;
            serde_json::from_str::<Vec<String>>(&grants)?
                .iter()
                .any(|grant| grant == "workspace.write")
        }
        _ => false,
    };
    let invalidates = might_mutate && may_have_run;
    let revision = if invalidates {
        current
            .checked_add(1)
            .context("workspace revision overflow")?
    } else {
        current
    };
    if invalidates {
        transaction.execute(
            "UPDATE workspace_revisions SET revision = ?2 WHERE run_id = ?1",
            params![operation.run_id, revision],
        )?;
        transaction.execute(
            "UPDATE obligations SET state = 'stale' WHERE run_id = ?1 AND state = 'verified'",
            [&operation.run_id],
        )?;
        append_event(
            transaction,
            &operation.run_id,
            "workspace.revision",
            serde_json::json!({"revision":revision,"operation":operation.id,"capability":operation.capability}),
        )?;
    }
    transaction.execute(
        "INSERT OR REPLACE INTO operation_revisions(operation_id, run_id, revision) VALUES (?1, ?2, ?3)",
        params![operation.id, operation.run_id, revision],
    )?;
    Ok(())
}

pub(crate) fn validate_completion(store: &Store, run_id: &str) -> Result<()> {
    let obligations = store.obligations(run_id)?;
    if obligations.len() <= 1 {
        return Ok(());
    }
    let revision = store
        .workspace_revision(run_id)?
        .context("obligation workspace revision missing")?;
    for obligation in obligations
        .iter()
        .skip(1)
        .filter(|item| item.state == "superseded")
    {
        let replacement = obligation
            .superseded_by
            .and_then(|id| obligations.iter().find(|item| item.id == id));
        if obligation.reason.as_deref().is_none_or(str::is_empty)
            || replacement.is_none_or(|item| item.id <= obligation.id)
        {
            bail!("superseded obligation lacks a valid replacement and reason");
        }
    }
    if obligations.iter().skip(1).any(|obligation| {
        obligation.state != "superseded"
            && (obligation.state != "verified" || obligation.verified_revision != Some(revision))
    }) {
        bail!(
            "kernel obligations remain open or stale; verify them at the current workspace revision before finishing"
        );
    }
    Ok(())
}

impl Store {
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
        if proofs.is_empty() || proofs.len() > 20 {
            bail!("verify 1..20 explicit obligations at a time");
        }
        if self.run(run_id)?.state != "running" {
            bail!("only a running task can verify obligations");
        }
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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
                let current: i64 = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM operations AS operation JOIN operation_revisions AS revision ON revision.operation_id = operation.id WHERE operation.run_id = ?1 AND operation.state = 'succeeded' AND revision.revision = ?2 AND (operation.artifact = ?3 OR EXISTS (SELECT 1 FROM operation_artifacts AS linked WHERE linked.operation_id = operation.id AND linked.hash = ?3)))",
                    params![run_id, revision, hash],
                    |row| row.get(0),
                )?;
                if current == 0 {
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
        store.operation_state(&check, "succeeded", Some(&old_evidence), json!({}))?;
        store.verify_obligation(&run.id, 1, &[old_evidence.clone()])?;
        let edit = store.begin_operation(&run.id, "workspace.write", json!({}), false)?;
        let edit_evidence = store.put_artifact(b"new file result")?;
        store.operation_state(&edit, "succeeded", Some(&edit_evidence), json!({}))?;
        assert_eq!(store.workspace_revision(&run.id)?, Some(1));
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
        assert_eq!(store.workspace_revision(&run.id)?, Some(1));
        drop(store);
        let mut store = Store::open(directory.path())?;
        assert_eq!(store.obligations(&run.id)?[1].state, "stale");
        let current_check = store.begin_operation(&run.id, "workspace.read", json!({}), true)?;
        let current_evidence = store.put_artifact(b"current test result")?;
        store.operation_state(
            &current_check,
            "succeeded",
            Some(&current_evidence),
            json!({}),
        )?;
        store.verify_obligation(&run.id, 1, &[current_evidence.clone()])?;
        store.complete_run(&run.id, "done", &[current_evidence])?;
        assert_eq!(store.obligations(&run.id)?[1].verified_revision, Some(1));
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
        drop(store);
        let mut store = Store::open(directory.path())?;
        assert_eq!(store.obligations(&run.id)?, ledger);
        store.verify_obligation(&run.id, 2, &[evidence.clone()])?;
        store.complete_run(&run.id, "done", &[evidence])?;
        assert_eq!(store.run(&run.id)?.state, "completed");
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
        let receipt = store.put_artifact(b"test pass before edit")?;
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
        store.operation_state(&recheck, "succeeded", Some(&current_evidence), json!({}))?;
        store.verify_obligation(&run.id, 1, &[current_evidence.clone()])?;
        store.complete_run(&run.id, "done", &[current_evidence])?;
        assert_eq!(store.run(&run.id)?.state, "completed");
        Ok(())
    }
}
