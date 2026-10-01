use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::storage::{Run, Store};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub capability: String,
    pub version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pattern {
    pub steps: Vec<Step>,
    pub verified_runs: usize,
    pub evidence: Vec<String>,
    pub source_runs: Vec<String>,
}

fn topics(task: &str) -> Vec<String> {
    let words: Vec<_> = task
        .split(|character: char| !character.is_ascii_alphanumeric())
        .map(str::to_ascii_lowercase)
        .collect();
    [
        "repair",
        "fix",
        "test",
        "parser",
        "async",
        "cache",
        "queue",
        "cancel",
        "stream",
        "build",
        "memory",
        "database",
        "transaction",
        "timeout",
        "race",
        "unicode",
    ]
    .into_iter()
    .filter(|topic| words.iter().any(|word| word == topic))
    .take(8)
    .map(str::to_owned)
    .collect()
}

fn verifier(configuration: &Value) -> Result<Option<String>> {
    configuration
        .get("acceptance_check")
        .filter(|check| !check.is_null())
        .map(|check| Ok(hex::encode(Sha256::digest(serde_json::to_vec(check)?))))
        .transpose()
}

pub(crate) fn record(transaction: &Transaction<'_>, run: &Run, evidence: &str) -> Result<()> {
    if run.budgets["learning_enabled"] != true {
        return Ok(());
    }
    let enabled: Option<bool> = transaction
        .query_row(
            "SELECT enabled FROM learning_settings WHERE workspace=?1",
            [&run.workspace],
            |row| row.get(0),
        )
        .optional()?;
    if enabled == Some(false) {
        return Ok(());
    }
    let Some(verifier) = verifier(&run.budgets)? else {
        return Ok(());
    };
    let topics = topics(&run.task);
    if topics.is_empty() {
        return Ok(());
    }
    let manifests = crate::capability::registry();
    let mut statement = transaction.prepare("SELECT capability,capability_version FROM operations WHERE run_id=?1 AND state='succeeded' AND artifact IS NOT NULL ORDER BY rowid LIMIT 129")?;
    let rows = statement.query_map([&run.id], |row| {
        Ok(Step {
            capability: row.get(0)?,
            version: row.get(1)?,
        })
    })?;
    let mut steps = Vec::new();
    let mut seen = 0;
    for step in rows {
        let step = step?;
        seen += 1;
        if seen > 128 {
            return Ok(());
        }
        if step.capability == crate::acceptance::CAPABILITY {
            continue;
        }
        if !manifests
            .iter()
            .any(|manifest| manifest.id == step.capability && manifest.version == step.version)
        {
            return Ok(());
        }
        if manifests
            .iter()
            .any(|manifest| manifest.id == step.capability && manifest.version == step.version)
            && steps.last() != Some(&step)
        {
            steps.push(step);
        }
    }
    if steps.is_empty() || steps.len() > 8 {
        return Ok(());
    }
    transaction.execute("INSERT OR IGNORE INTO workflow_experience(run_id,workspace,topics,steps,verifier,evidence,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7)", params![run.id,run.workspace,serde_json::to_string(&topics)?,serde_json::to_string(&steps)?,verifier,evidence,crate::storage::unix_time()])?;
    transaction.execute("DELETE FROM workflow_experience WHERE workspace=?1 AND run_id NOT IN (SELECT run_id FROM workflow_experience WHERE workspace=?1 ORDER BY created_at DESC,rowid DESC LIMIT 128)", [&run.workspace])?;
    Ok(())
}

impl Store {
    pub fn learning_enabled(&self, workspace: &Path) -> Result<bool> {
        let workspace = dunce::canonicalize(workspace)?;
        Ok(self
            .connection
            .query_row(
                "SELECT enabled FROM learning_settings WHERE workspace=?1",
                [workspace.to_string_lossy().as_ref()],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(true))
    }

    pub fn set_learning(&mut self, workspace: &Path, enabled: bool) -> Result<()> {
        let workspace = dunce::canonicalize(workspace)?;
        self.connection.execute("INSERT INTO learning_settings(workspace,enabled) VALUES (?1,?2) ON CONFLICT(workspace) DO UPDATE SET enabled=excluded.enabled", params![workspace.to_string_lossy(),enabled])?;
        Ok(())
    }

    pub fn reset_learning(&mut self, workspace: &Path) -> Result<()> {
        let workspace = dunce::canonicalize(workspace)?;
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "DELETE FROM workflow_experience WHERE workspace=?1",
            [workspace.to_string_lossy().as_ref()],
        )?;
        transaction.execute(
            "DELETE FROM user_habits WHERE workspace=?1",
            [workspace.to_string_lossy().as_ref()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn learning_count(&self, workspace: &Path) -> Result<usize> {
        let workspace = dunce::canonicalize(workspace)?;
        Ok(self.connection.query_row(
            "SELECT COUNT(*) FROM workflow_experience WHERE workspace=?1",
            [workspace.to_string_lossy().as_ref()],
            |row| row.get(0),
        )?)
    }

    pub fn learned_patterns(
        &self,
        workspace: &Path,
        task: &str,
        configuration: &Value,
        grants: &Value,
    ) -> Result<Vec<Pattern>> {
        if !self.learning_enabled(workspace)? {
            return Ok(Vec::new());
        }
        let Some(verifier) = verifier(configuration)? else {
            return Ok(Vec::new());
        };
        let topics = topics(task);
        if topics.is_empty() {
            return Ok(Vec::new());
        }
        let workspace = dunce::canonicalize(workspace)?;
        let mut statement = self.connection.prepare("SELECT topics,steps,evidence,run_id FROM workflow_experience WHERE workspace=?1 AND verifier=?2 AND created_at>=?3 ORDER BY created_at DESC,rowid DESC LIMIT 128")?;
        let rows = statement.query_map(
            params![
                workspace.to_string_lossy(),
                verifier,
                crate::storage::unix_time() - 30 * 86400
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )?;
        let manifests = crate::capability::registry();
        let mut patterns = BTreeMap::<String, Pattern>::new();
        for row in rows {
            let (previous_topics, key, evidence, source_run) = row?;
            let previous_topics: Vec<String> = serde_json::from_str(&previous_topics)?;
            if topics
                .iter()
                .filter(|topic| previous_topics.contains(topic))
                .count()
                < 2
            {
                continue;
            }
            let steps: Vec<Step> = serde_json::from_str(&key)?;
            if !steps.iter().all(|step| {
                manifests.iter().any(|manifest| {
                    manifest.id == step.capability
                        && manifest.version == step.version
                        && grants.as_array().is_some_and(|grants| {
                            grants
                                .iter()
                                .any(|grant| grant.as_str() == Some(&manifest.permission))
                        })
                })
            }) {
                continue;
            }
            let pattern = patterns.entry(key).or_insert(Pattern {
                steps,
                verified_runs: 0,
                evidence: Vec::new(),
                source_runs: Vec::new(),
            });
            pattern.verified_runs += 1;
            if pattern.evidence.len() < 2 {
                pattern.evidence.push(evidence);
                pattern.source_runs.push(source_run);
            }
        }
        let mut patterns: Vec<_> = patterns
            .into_values()
            .filter(|pattern| pattern.verified_runs >= 2)
            .collect();
        patterns.sort_by_key(|pattern| std::cmp::Reverse(pattern.verified_runs));
        patterns.truncate(2);
        Ok(patterns)
    }

    pub fn learning_paths(&self, workspace: &Path) -> Result<Vec<(Vec<Step>, usize)>> {
        let workspace = dunce::canonicalize(workspace)?;
        let mut statement = self.connection.prepare("SELECT steps,COUNT(*) FROM workflow_experience WHERE workspace=?1 GROUP BY steps ORDER BY COUNT(*) DESC,steps LIMIT 8")?;
        let rows = statement.query_map([workspace.to_string_lossy().as_ref()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, usize>(1)?))
        })?;
        rows.map(|row| {
            let (steps, count) = row?;
            Ok((serde_json::from_str(&steps)?, count))
        })
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn accepted(store: &mut Store, workspace: &Path, checked: bool) -> Result<Run> {
        let check = crate::acceptance::Check {
            name: "fixture".into(),
            program: "node".into(),
            args: vec!["check.mjs".into()],
            image: "node:22-alpine".into(),
            seconds: 30,
        };
        let run = store.create_run(
            "repair parser test",
            workspace,
            "fixture",
            json!(["workspace.read"]),
            if checked {
                json!({"acceptance_check":check})
            } else {
                json!({})
            },
            "fixture",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let read = store.begin_operation(
            &run.id,
            "workspace.read",
            json!({"path":"private-path"}),
            true,
        )?;
        let evidence = store.put_artifact(b"private tool transcript")?;
        crate::storage::claim_test_operation(store, &read)?;
        store.operation_state(&read, "succeeded", Some(&evidence), json!({}))?;
        if checked {
            crate::acceptance::propose(store, &run, "done", std::slice::from_ref(&evidence))?;
            let proposal = store.completion_proposal(&run.id)?.unwrap();
            let operation = store.begin_operation_versioned(
                &run.id,
                crate::acceptance::CAPABILITY,
                check.version()?,
                json!({"proposal":proposal}),
                true,
            )?;
            let passed = store.put_artifact(br#"{"exit_code":0}"#)?;
            crate::storage::claim_test_operation(store, &operation)?;
            store.operation_state(&operation, "succeeded", Some(&passed), json!({}))?;
        }
        store.complete_run(&run.id, "done", &[evidence])?;
        Ok(run)
    }

    #[test]
    fn verified_repetition_learns_without_transcripts_and_freezes_future_hints() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let other = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        accepted(&mut store, directory.path(), false)?;
        assert_eq!(store.learning_count(directory.path())?, 0);
        let first = accepted(&mut store, directory.path(), true)?;
        assert!(
            store
                .learned_patterns(directory.path(), &first.task, &first.budgets, &first.grants)?
                .is_empty()
        );
        accepted(&mut store, directory.path(), true)?;
        let patterns =
            store.learned_patterns(directory.path(), &first.task, &first.budgets, &first.grants)?;
        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0].verified_runs, 2);
        assert_eq!(patterns[0].source_runs.len(), 2);
        assert!(!serde_json::to_string(&patterns)?.contains("private"));
        assert!(
            store
                .learned_patterns(other.path(), &first.task, &first.budgets, &first.grants)?
                .is_empty()
        );
        assert!(
            store
                .learned_patterns(
                    directory.path(),
                    "async cache",
                    &first.budgets,
                    &first.grants
                )?
                .is_empty()
        );
        assert!(
            store
                .learned_patterns(directory.path(), &first.task, &first.budgets, &json!([]))?
                .is_empty()
        );
        let mut changed = first.budgets.clone();
        changed["acceptance_check"]["args"] = json!(["different.mjs"]);
        assert!(
            store
                .learned_patterns(directory.path(), &first.task, &changed, &first.grants)?
                .is_empty()
        );
        let frozen = store.create_run(
            &first.task,
            directory.path(),
            "fixture",
            first.grants.clone(),
            first.budgets.clone(),
            "",
        )?;
        assert_eq!(
            frozen.budgets["workflow_patterns"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        store.reset_learning(directory.path())?;
        assert_eq!(store.learning_count(directory.path())?, 0);
        assert_eq!(
            store.run(&frozen.id)?.budgets["workflow_patterns"],
            frozen.budgets["workflow_patterns"]
        );
        store.set_learning(directory.path(), false)?;
        accepted(&mut store, directory.path(), true)?;
        assert_eq!(store.learning_count(directory.path())?, 0);
        drop(store);
        assert!(!Store::open(directory.path())?.learning_enabled(directory.path())?);
        Ok(())
    }

    #[test]
    fn experience_window_is_bounded_and_expired_paths_are_not_suggested() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        for _ in 0..130 {
            accepted(&mut store, directory.path(), true)?;
        }
        assert_eq!(store.learning_count(directory.path())?, 128);
        let run = store.runs()?.remove(0);
        assert_eq!(store.learning_paths(directory.path())?.len(), 1);
        store
            .connection
            .execute("UPDATE workflow_experience SET created_at=0", [])?;
        assert!(
            store
                .learned_patterns(directory.path(), &run.task, &run.budgets, &run.grants)?
                .is_empty()
        );
        Ok(())
    }
}
