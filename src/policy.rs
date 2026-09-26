use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_POLICY_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandScope {
    pub program: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandScopes {
    pub commands: Vec<CommandScope>,
}

impl CommandScopes {
    pub fn validate(&self) -> Result<()> {
        if self.commands.len() > 64 || serde_json::to_vec(self)?.len() as u64 > MAX_POLICY_BYTES {
            bail!("command scopes exceed 64 commands or 65536 bytes");
        }
        for command in &self.commands {
            if command.program.is_empty()
                || command.program.len() > 160
                || matches!(command.program.as_str(), "." | "..")
                || !command.program.chars().all(|character| {
                    character.is_ascii_alphanumeric() || "._-+".contains(character)
                })
            {
                bail!(
                    "command scopes require a plain program name, not a path or shell expression"
                );
            }
            if command.args.len() > 128
                || command
                    .args
                    .iter()
                    .any(|argument| argument.len() > 8192 || argument.contains('\0'))
            {
                bail!(
                    "command scopes require at most 128 arguments, each at most 8192 bytes without NUL"
                );
            }
        }
        Ok(())
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let mut bytes = Vec::new();
        File::open(path)?
            .take(MAX_POLICY_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_POLICY_BYTES {
            bail!("command scopes file exceeds 65536 bytes");
        }
        let scopes: Self = serde_json::from_slice(&bytes).context("invalid command scopes file")?;
        scopes.validate()?;
        Ok(scopes)
    }

    pub fn from_configuration(configuration: &Value) -> Result<Option<Self>> {
        let Some(value) = configuration
            .get("command_scopes")
            .filter(|value| !value.is_null())
        else {
            return Ok(None);
        };
        let scopes: Self =
            serde_json::from_value(value.clone()).context("invalid command scopes")?;
        scopes.validate()?;
        Ok(Some(scopes))
    }

    pub fn authorize(&self, arguments: &Value) -> Result<()> {
        self.validate()?;
        let requested: CommandScope = serde_json::from_value(arguments.clone())
            .context("command scope requires only program and args")?;
        if !self.commands.contains(&requested) {
            bail!("command arguments are outside the approved exact command scopes");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exact_scopes_preserve_argument_boundaries_order_and_case() -> Result<()> {
        let scopes: CommandScopes = serde_json::from_value(
            json!({"commands":[{"program":"cargo","args":["test","--offline"]}]}),
        )?;
        scopes.authorize(&json!({"program":"cargo","args":["test","--offline"]}))?;
        for requested in [
            json!({"program":"cargo","args":["test"]}),
            json!({"program":"cargo","args":["test --offline"]}),
            json!({"program":"cargo","args":["--offline","test"]}),
            json!({"program":"cargo","args":["test","--offline","--release"]}),
            json!({"program":"Cargo","args":["test","--offline"]}),
            json!({"program":"sh","args":["-c","cargo test --offline"]}),
            json!({"program":"cargo","args":["test","--offline"],"env":{"SECRET":"bad"}}),
        ] {
            assert!(scopes.authorize(&requested).is_err());
        }
        Ok(())
    }

    #[test]
    fn empty_scopes_deny_commands_but_absent_scopes_preserve_legacy_policy() -> Result<()> {
        assert!(CommandScopes::from_configuration(&json!({}))?.is_none());
        let scopes =
            CommandScopes::from_configuration(&json!({"command_scopes":{"commands":[]}}))?.unwrap();
        assert!(
            scopes
                .authorize(&json!({"program":"cargo","args":[]}))
                .is_err()
        );
        for configuration in [
            json!({"command_scopes":{}}),
            json!({"command_scopes":{"commands":[],"wildcard":true}}),
            json!({"command_scopes":{"commands":[{"program":"../cargo","args":[]}]}}),
            json!({"command_scopes":{"commands":[{"program":"cargo;echo","args":[]}]}}),
            json!({"command_scopes":{"commands":[{"program":"cargo","args":["\0"]}]}}),
        ] {
            assert!(CommandScopes::from_configuration(&configuration).is_err());
        }
        Ok(())
    }

    #[test]
    fn scope_files_are_bounded_and_frozen_as_values() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("commands.json");
        std::fs::write(
            &path,
            r#"{"commands":[{"program":"cargo","args":["test"]}]}"#,
        )?;
        let scopes = CommandScopes::from_file(&path)?;
        std::fs::write(&path, r#"{"commands":[]}"#)?;
        scopes.authorize(&json!({"program":"cargo","args":["test"]}))?;
        std::fs::write(&path, vec![b' '; MAX_POLICY_BYTES as usize + 1])?;
        assert!(CommandScopes::from_file(&path).is_err());
        let scopes = CommandScopes {
            commands: vec![
                CommandScope {
                    program: "cargo".into(),
                    args: vec![]
                };
                65
            ],
        };
        assert!(scopes.validate().is_err());
        Ok(())
    }
}
