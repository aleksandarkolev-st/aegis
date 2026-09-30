//! Revalidate content receipts for files actually observed through file tools.
use crate::{
    filesystem::FileScopes,
    storage::{Operation, Store},
};
use anyhow::{Result, bail};
use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs::File, io::Read, path::Path};

pub(crate) fn record(
    transaction: &Transaction<'_>,
    operation: &Operation,
    result: Option<&Value>,
) -> Result<()> {
    if !matches!(
        operation.capability.as_str(),
        "workspace.read" | "workspace.read_batch" | "workspace.write" | "workspace.patch"
    ) {
        return Ok(());
    }
    let Some(result) = result else {
        return Ok(());
    };
    let entries: Vec<&Value> = if operation.capability == "workspace.read_batch" {
        result["selected"]
            .as_array()
            .into_iter()
            .flatten()
            .collect()
    } else {
        vec![result]
    };
    for entry in entries {
        let (Some(path), Some(hash)) = (entry["path"].as_str(), entry["sha256"].as_str()) else {
            continue;
        };
        let path = crate::filesystem::path_name(path)?;
        if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("File receipt has an invalid content hash");
        }
        let previous: Option<String> = transaction
            .query_row(
                "SELECT hash FROM observed_files WHERE run_id=?1 AND path=?2",
                params![operation.run_id, path],
                |row| row.get(0),
            )
            .optional()?;
        if previous.as_deref().is_some_and(|old| old != hash)
            && matches!(
                operation.capability.as_str(),
                "workspace.read" | "workspace.read_batch"
            )
        {
            crate::obligations::invalidate_workspace(
                transaction,
                &operation.run_id,
                json!({"reason":"observed file changed since its prior receipt","path":path}),
            )?;
        }
        if previous.is_none() {
            let count: i64 = transaction.query_row(
                "SELECT COUNT(*) FROM observed_files WHERE run_id=?1",
                [&operation.run_id],
                |row| row.get(0),
            )?;
            if count >= 1024 {
                bail!(
                    "File freshness ledger is bounded to 1024 observed paths; split the task before observing more"
                );
            }
        }
        transaction.execute("INSERT INTO observed_files(run_id,path,hash) VALUES (?1,?2,?3) ON CONFLICT(run_id,path) DO UPDATE SET hash=excluded.hash",params![operation.run_id,path,hash])?;
    }
    Ok(())
}

impl Store {
    pub(crate) fn changed_observed_files(&self, id: &str) -> Result<Vec<(String, String, String)>> {
        let run = self.run(id)?;
        if run.is_terminal() {
            return Ok(Vec::new());
        }
        let sources = {
            let mut statement = self
                .connection
                .prepare("SELECT path,hash FROM observed_files WHERE run_id=?1 ORDER BY path")?;
            statement
                .query_map([id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let scopes = FileScopes::from_configuration(&run.budgets)?.unwrap_or(FileScopes {
            read: vec!["**".into()],
            write: vec![],
        });
        let mut changed = Vec::new();
        for (path, expected) in sources {
            let digest = (|| -> Result<String> {
                let target = scopes.checked_path(Path::new(&run.workspace), &path, false)?;
                let file = File::open(target)?;
                let metadata = file.metadata()?;
                if !metadata.is_file() || metadata.len() > 2 * 1024 * 1024 {
                    bail!("Observed source is no longer a bounded regular file");
                }
                let mut bytes = Vec::new();
                file.take(2 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
                if bytes.len() > 2 * 1024 * 1024 {
                    bail!("Observed source grew beyond its bound");
                }
                Ok(hex::encode(Sha256::digest(&bytes)))
            })();
            let actual = digest.unwrap_or_else(|_| "unavailable".into());
            if actual != expected {
                changed.push((path, expected, actual));
            }
        }
        Ok(changed)
    }

    pub fn refresh_observed_files(&mut self, id: &str) -> Result<bool> {
        let changed = self.changed_observed_files(id)?;
        if changed.is_empty() {
            return Ok(false);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut paths = Vec::new();
        for (path, expected, actual) in changed {
            let count = transaction.execute(
                "UPDATE observed_files SET hash=?4 WHERE run_id=?1 AND path=?2 AND hash=?3",
                params![id, path, expected, actual],
            )?;
            if count > 0 {
                paths.push(path);
            }
        }
        if paths.is_empty() {
            return Ok(false);
        }
        crate::obligations::invalidate_workspace(
            &transaction,
            id,
            json!({"reason":"observed files changed outside their recorded file-tool receipts","paths":paths}),
        )?;
        transaction.commit()?;
        Ok(true)
    }

    pub(crate) fn check_observed_files(&self, id: &str) -> Result<()> {
        if !self.changed_observed_files(id)?.is_empty() {
            bail!("Observed workspace files changed; gather fresh evidence before completion");
        }
        Ok(())
    }
}
