use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::storage::{Run, Store};

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

impl Store {
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
}
