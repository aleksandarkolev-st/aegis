use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::storage::Run;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub name: String,
    pub program: String,
    pub args: Vec<String>,
    pub image: String,
    #[serde(default = "default_seconds")]
    pub seconds: u64,
}

fn default_seconds() -> u64 {
    30
}

impl Check {
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() || self.name.len() > 500 {
            bail!("acceptance check needs a name of at most 500 bytes");
        }
        if self.program.is_empty()
            || self.program.len() > 100
            || !self.program.as_bytes()[0].is_ascii_alphanumeric()
            || !self
                .program
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        {
            bail!("acceptance program must be a container executable name, not a path or option");
        }
        if self.image.is_empty()
            || self.image.len() > 256
            || self.image.starts_with('-')
            || !self
                .image
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._/:@-".contains(&byte))
        {
            bail!("acceptance requires a valid locally available container image");
        }
        if !(1..=300).contains(&self.seconds)
            || self.args.len() > 64
            || self.args.iter().map(String::len).sum::<usize>() > 60_000
            || self.args.iter().any(|argument| argument.contains('\0'))
        {
            bail!("acceptance arguments or deadline exceed limits");
        }
        Ok(())
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        if fs::metadata(path)?.len() > 65_536 {
            bail!("acceptance configuration exceeds 64 KiB");
        }
        let check: Self = serde_json::from_slice(&fs::read(path)?)
            .context("acceptance file must contain a JSON check configuration")?;
        check.validate()?;
        Ok(check)
    }

    pub fn from_run(run: &Run) -> Result<Option<Self>> {
        let Some(value) = run
            .budgets
            .get("acceptance_check")
            .filter(|value| !value.is_null())
        else {
            return Ok(None);
        };
        let check: Self =
            serde_json::from_value(value.clone()).context("invalid acceptance configuration")?;
        check.validate()?;
        Ok(Some(check))
    }

    pub fn version(&self) -> Result<u32> {
        let hash = Sha256::digest(serde_json::to_vec(self)?);
        Ok(u32::from_be_bytes(hash[..4].try_into().unwrap()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freezes_validated_configurations_and_rejects_host_execution() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("acceptance.json");
        fs::write(
            &path,
            r#"{"name":"addition assertions","program":"node","args":["--input-type=module","-e","console.log('verified')"],"image":"node:22-alpine"}"#,
        )?;
        let check = Check::from_file(&path)?;
        assert_eq!(check.seconds, 30);
        let version = check.version()?;
        fs::write(&path, "not a configuration")?;
        assert_eq!(check.version()?, version);
        for program in ["/bin/sh", "../node", "--privileged", "node;echo"] {
            let mut invalid = check.clone();
            invalid.program = program.into();
            assert!(invalid.validate().is_err());
        }
        let mut changed = check;
        changed.args.push("different assertion".into());
        assert_ne!(changed.version()?, version);
        changed.seconds = 301;
        assert!(changed.validate().is_err());
        Ok(())
    }
}
