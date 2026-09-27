use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::storage::Store;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Instruction {
    pub id: String,
    pub scope: String,
    pub text: String,
    pub revision: u64,
    pub updated_at: i64,
}

fn validate(scope: &str, text: &str) -> Result<()> {
    if scope.len() > 128 {
        bail!("instruction scope exceeds 128 UTF-8 bytes");
    }
    crate::filesystem::FileScopes {
        read: vec![scope.into()],
        write: Vec::new(),
    }
    .validate()?;
    if text.is_empty() || text.len() > 512 || crate::text::clean(text) != text {
        bail!("instructions need safe, nonempty text of at most 512 UTF-8 bytes");
    }
    reject_credentials(text)
}

pub(crate) fn reject_credentials(text: &str) -> Result<()> {
    let lower = text.to_lowercase();
    if [
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "bearer ",
    ]
    .iter()
    .any(|prefix| lower.contains(prefix))
        || lower.split_whitespace().any(|word| word.starts_with("sk-"))
    {
        bail!("do not store credentials in instructions");
    }
    Ok(())
}

pub(crate) fn frozen(configuration: &Value) -> Result<Vec<Instruction>> {
    let Some(value) = configuration.get("project_instructions") else {
        return Ok(Vec::new());
    };
    let instructions: Vec<Instruction> =
        serde_json::from_value(value.clone()).context("invalid frozen instruction ledger")?;
    let mut bytes = 0;
    let mut seen = std::collections::HashSet::new();
    for instruction in &instructions {
        validate(&instruction.scope, &instruction.text)?;
        Uuid::parse_str(&instruction.id)?;
        if instruction.revision == 0 || !seen.insert(&instruction.id) {
            bail!("invalid instruction revision or duplicate identity");
        }
        bytes += instruction.text.len() + instruction.scope.len();
    }
    if instructions.len() > 8 || bytes > 3072 {
        bail!("instruction ledger exceeds eight rules or 3072 UTF-8 bytes");
    }
    Ok(instructions)
}

impl Store {
    pub fn project_instructions(&self, workspace: &Path) -> Result<Vec<Instruction>> {
        let workspace = dunce::canonicalize(workspace)?
            .to_string_lossy()
            .into_owned();
        let mut statement = self.connection.prepare(
            "SELECT id,scope,text,revision,updated_at FROM project_instructions WHERE workspace=?1 ORDER BY scope,id",
        )?;
        statement
            .query_map([workspace], |row| {
                Ok(Instruction {
                    id: row.get(0)?,
                    scope: row.get(1)?,
                    text: row.get(2)?,
                    revision: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn pin_instruction(
        &mut self,
        workspace: &Path,
        scope: &str,
        text: &str,
        replace: Option<&str>,
    ) -> Result<String> {
        let text = text.trim();
        let scope = scope.trim();
        validate(scope, text)?;
        let workspace = dunce::canonicalize(workspace)?
            .to_string_lossy()
            .into_owned();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if replace.is_none() {
            if let Some(id) = transaction.query_row(
                "SELECT id FROM project_instructions WHERE workspace=?1 AND scope=?2 AND text=?3",
                params![workspace,scope,text],
                |row| row.get::<_,String>(0),
            ).optional()? {
                return Ok(id);
            }
        }
        let id = replace
            .map(str::to_owned)
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let revision = if replace.is_some() {
            transaction
                .query_row(
                    "SELECT revision FROM project_instructions WHERE workspace=?1 AND id=?2",
                    params![workspace, id],
                    |row| row.get::<_, u64>(0),
                )
                .optional()?
                .context("instruction does not belong to this workspace")?
                .checked_add(1)
                .context("instruction revision overflow")?
        } else {
            1
        };
        let (count, bytes) = transaction.query_row(
            "SELECT COUNT(*),COALESCE(SUM(length(CAST(text AS BLOB))+length(CAST(scope AS BLOB))),0) FROM project_instructions WHERE workspace=?1 AND id<>?2",
            params![workspace,id],
            |row| Ok((row.get::<_,u64>(0)?,row.get::<_,u64>(1)?)),
        )?;
        if count >= 8 || bytes + text.len() as u64 + scope.len() as u64 > 3072 {
            bail!(
                "instructions are limited to eight rules / 3072 UTF-8 bytes; edit or remove a rule first"
            );
        }
        transaction.execute(
            "INSERT INTO project_instructions(id,workspace,scope,text,revision,updated_at) VALUES (?1,?2,?3,?4,?5,?6) ON CONFLICT(id) DO UPDATE SET scope=excluded.scope,text=excluded.text,revision=excluded.revision,updated_at=excluded.updated_at",
            params![id,workspace,scope,text,revision,crate::storage::now()],
        )?;
        transaction.commit()?;
        Ok(id)
    }

    pub fn unpin_instruction(&mut self, workspace: &Path, id: &str) -> Result<()> {
        let workspace = dunce::canonicalize(workspace)?
            .to_string_lossy()
            .into_owned();
        if self.connection.execute(
            "DELETE FROM project_instructions WHERE workspace=?1 AND id=?2",
            params![workspace, id],
        )? != 1
        {
            bail!("instruction does not belong to this workspace");
        }
        Ok(())
    }
}
