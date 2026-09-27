use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::{TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::filesystem::FileScopes;
use crate::storage::Store;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryRule {
    pub path: String,
    pub scope: String,
    pub sha256: String,
    pub text: String,
    pub revision: u64,
    pub approved: bool,
}

fn scope(path: &str) -> Result<String> {
    let name = crate::filesystem::path_name(path)?;
    if name != path || !matches!(name.rsplit('/').next(), Some("AGENTS.md" | "CLAUDE.md")) {
        bail!("choose a relative AGENTS.md or CLAUDE.md file using forward slashes");
    }
    Ok(name
        .rsplit_once('/')
        .map_or_else(|| "**".into(), |(parent, _)| format!("{parent}/**")))
}

fn validate(rule: &RepositoryRule) -> Result<()> {
    if rule.path.len() > 128 || scope(&rule.path)? != rule.scope || rule.revision == 0 {
        bail!("invalid repository guidance identity or scope");
    }
    if rule.text.trim().is_empty() || rule.text.len() > 8192 {
        bail!(
            "repository guidance must be nonempty and at most 8192 UTF-8 bytes; split by subtree rather than truncate"
        );
    }
    let display = rule.text.replace("\r\n", "\n").replace('\t', " ");
    if crate::text::clean(&display) != display {
        bail!("repository guidance contains unsafe control or directional characters");
    }
    crate::instructions::reject_credentials(&rule.text)?;
    if hex::encode(Sha256::digest(rule.text.as_bytes())) != rule.sha256 {
        bail!("repository guidance content hash does not match");
    }
    Ok(())
}

pub fn read(workspace: &Path, path: &str) -> Result<RepositoryRule> {
    let scope = scope(path)?;
    let workspace = dunce::canonicalize(workspace)?;
    let target = FileScopes {
        read: vec!["**".into()],
        write: Vec::new(),
    }
    .checked_path(&workspace, path, false)?;
    if !std::fs::metadata(&target)?.is_file() {
        bail!("repository guidance must be a regular file");
    }
    let mut text = String::new();
    File::open(target)?.take(8193).read_to_string(&mut text)?;
    let rule = RepositoryRule {
        path: path.into(),
        scope,
        sha256: hex::encode(Sha256::digest(text.as_bytes())),
        text,
        revision: 1,
        approved: false,
    };
    validate(&rule)?;
    Ok(rule)
}

pub(crate) fn frozen(configuration: &Value) -> Result<Vec<RepositoryRule>> {
    let Some(value) = configuration.get("repository_rules") else {
        return Ok(Vec::new());
    };
    let rules: Vec<RepositoryRule> = serde_json::from_value(value.clone())?;
    let mut paths = std::collections::HashSet::new();
    let mut bytes = 0;
    for rule in &rules {
        validate(rule)?;
        if !rule.approved || !paths.insert(&rule.path) {
            bail!("repository guidance must be explicitly reviewed and uniquely scoped");
        }
        bytes += rule.text.len() + rule.path.len() + rule.scope.len();
    }
    if rules.len() > 8 || bytes > 16384 {
        bail!(
            "repository guidance exceeds eight files or 16384 UTF-8 bytes; nothing was truncated"
        );
    }
    Ok(rules)
}

impl Store {
    pub fn repository_reviews(&self, workspace: &Path) -> Result<Vec<RepositoryRule>> {
        let workspace = dunce::canonicalize(workspace)?
            .to_string_lossy()
            .into_owned();
        let mut statement = self.connection.prepare(
            "SELECT path,scope,sha256,text,revision,approved FROM repository_reviews WHERE workspace=?1 ORDER BY path",
        )?;
        statement
            .query_map([workspace], |row| {
                Ok(RepositoryRule {
                    path: row.get(0)?,
                    scope: row.get(1)?,
                    sha256: row.get(2)?,
                    text: row.get(3)?,
                    revision: row.get(4)?,
                    approved: row.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn review_repository_rule(
        &mut self,
        workspace: &Path,
        candidate: &RepositoryRule,
        approved: bool,
    ) -> Result<()> {
        validate(candidate)?;
        let current = read(workspace, &candidate.path)?;
        if current.sha256 != candidate.sha256 {
            bail!("guidance changed while being reviewed; inspect it again before approving");
        }
        let workspace = dunce::canonicalize(workspace)?
            .to_string_lossy()
            .into_owned();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let revision: u64 = transaction.query_row(
            "SELECT COALESCE(MAX(revision),0) FROM repository_reviews WHERE workspace=?1 AND path=?2",
            params![workspace,candidate.path], |row| row.get(0),
        )?;
        let revision = revision
            .checked_add(1)
            .context("repository guidance revision overflow")?;
        let (count, bytes): (u64, u64) = transaction.query_row(
            "SELECT COUNT(*),COALESCE(SUM(length(CAST(text AS BLOB))+length(CAST(path AS BLOB))+length(CAST(scope AS BLOB))),0) FROM repository_reviews WHERE workspace=?1 AND path<>?2",
            params![workspace,candidate.path], |row| Ok((row.get(0)?,row.get(1)?)),
        )?;
        if count >= 8
            || bytes
                + candidate.text.len() as u64
                + candidate.path.len() as u64
                + candidate.scope.len() as u64
                > 16384
        {
            bail!(
                "repository review storage exceeds eight files or 16384 UTF-8 bytes; remove a review first"
            );
        }
        transaction.execute(
            "INSERT INTO repository_reviews(workspace,path,scope,sha256,text,revision,approved) VALUES (?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(workspace,path) DO UPDATE SET scope=excluded.scope,sha256=excluded.sha256,text=excluded.text,revision=excluded.revision,approved=excluded.approved",
            params![workspace,candidate.path,candidate.scope,candidate.sha256,candidate.text,revision,approved],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn remove_repository_review(&mut self, workspace: &Path, path: &str) -> Result<()> {
        let workspace = dunce::canonicalize(workspace)?
            .to_string_lossy()
            .into_owned();
        if self.connection.execute(
            "DELETE FROM repository_reviews WHERE workspace=?1 AND path=?2",
            params![workspace, path],
        )? != 1
        {
            bail!("repository review does not belong to this workspace");
        }
        Ok(())
    }

    pub(crate) fn repository_snapshot(
        &self,
        workspace: &Path,
        grants: &Value,
        configuration: &Value,
    ) -> Result<Vec<RepositoryRule>> {
        if !grants
            .as_array()
            .is_some_and(|grants| grants.iter().any(|grant| grant == "workspace.read"))
        {
            return Ok(Vec::new());
        }
        let scopes = FileScopes::from_configuration(configuration)?;
        let mut rules = Vec::new();
        for rule in self
            .repository_reviews(workspace)?
            .into_iter()
            .filter(|rule| rule.approved)
        {
            if scopes
                .as_ref()
                .is_some_and(|scopes| !scopes.permits(&rule.path, false))
            {
                continue;
            }
            let current = read(workspace, &rule.path).with_context(|| {
                format!("review {} again from F7 Project instructions", rule.path)
            })?;
            if current.sha256 != rule.sha256 {
                bail!(
                    "{} changed since approval; review it again from F7 Project instructions",
                    rule.path
                );
            }
            rules.push(rule);
        }
        frozen(&serde_json::json!({"repository_rules":rules}))
    }
}
