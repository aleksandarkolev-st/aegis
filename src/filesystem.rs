use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::storage::Run;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileScopes {
    pub read: Vec<String>,
    pub write: Vec<String>,
}

fn path_name(path: &str) -> Result<String> {
    let path = path.replace('\\', "/");
    if path.is_empty()
        || path.len() > 512
        || path
            .chars()
            .any(|character| character.is_control() || ":*?".contains(character))
        || path.split('/').any(|part| {
            part.is_empty()
                || matches!(part, "." | "..")
                || part.eq_ignore_ascii_case(".git")
                || part.eq_ignore_ascii_case(".arun")
        })
    {
        bail!("file scopes require a relative file or directory/** without traversal or metadata");
    }
    Ok(path)
}

fn covers(pattern: &str, path: &str) -> bool {
    pattern == "**"
        || pattern == path
        || pattern
            .strip_suffix("/**")
            .is_some_and(|prefix| path == prefix || path.starts_with(&format!("{prefix}/")))
}

impl FileScopes {
    pub fn validate(&self) -> Result<()> {
        if self.read.len() + self.write.len() > 64 || serde_json::to_vec(self)?.len() > 8192 {
            bail!("file scopes exceed 64 paths or 8192 bytes");
        }
        for pattern in self.read.iter().chain(&self.write) {
            if pattern != "**" {
                let path = pattern.strip_suffix("/**").unwrap_or(pattern);
                if path_name(path)? != path {
                    bail!("use forward slashes in file scope patterns");
                }
            }
        }
        for pattern in &self.write {
            if !self.read.iter().any(|read| covers(read, pattern)) {
                bail!("writable paths must also be covered by read scopes");
            }
        }
        Ok(())
    }

    pub fn from_configuration(configuration: &Value) -> Result<Option<Self>> {
        let Some(value) = configuration
            .get("filesystem_scopes")
            .filter(|value| !value.is_null())
        else {
            return Ok(None);
        };
        let scopes: Self =
            serde_json::from_value(value.clone()).context("invalid filesystem scopes")?;
        scopes.validate()?;
        Ok(Some(scopes))
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let mut bytes = Vec::new();
        File::open(path)?.take(8193).read_to_end(&mut bytes)?;
        if bytes.len() > 8192 {
            bail!("file scopes exceed 8192 bytes");
        }
        let scopes: Self = serde_json::from_slice(&bytes).context("invalid file scopes file")?;
        scopes.validate()?;
        Ok(scopes)
    }

    pub fn permits(&self, path: &str, write: bool) -> bool {
        path_name(path).is_ok_and(|path| {
            (if write { &self.write } else { &self.read })
                .iter()
                .any(|pattern| covers(pattern, &path))
        })
    }

    pub fn checked_path(&self, workspace: &Path, path: &str, write: bool) -> Result<PathBuf> {
        let name = path_name(path)?;
        if !self.permits(&name, write) {
            bail!("path is outside the approved file scopes");
        }
        let mut target = workspace.to_path_buf();
        for part in name.split('/') {
            target.push(part);
            match fs::symlink_metadata(&target) {
                Ok(metadata) => {
                    #[cfg(windows)]
                    let linked = {
                        use std::os::windows::fs::MetadataExt;
                        metadata.file_attributes() & 0x400 != 0
                    };
                    #[cfg(not(windows))]
                    let linked = metadata.file_type().is_symlink();
                    if linked {
                        bail!("scoped paths cannot traverse symbolic links or junctions");
                    }
                }
                Err(error) if write && error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(target)
    }

    pub fn authorize(&self, run: &Run, capability: &str, arguments: &Value) -> Result<()> {
        match capability {
            "workspace.read" | "workspace.write" => {
                let path = arguments["path"].as_str().context("file path missing")?;
                self.checked_path(
                    Path::new(&run.workspace),
                    path,
                    capability == "workspace.write",
                )?;
            }
            "workspace.search" if self.read.is_empty() => {
                bail!("no file paths are authorized for search")
            }
            "process.run" => {
                let writable = run
                    .grants
                    .as_array()
                    .is_some_and(|grants| grants.iter().any(|grant| grant == "workspace.write"));
                if !self.read.iter().any(|pattern| pattern == "**")
                    || (writable && !self.write.iter().any(|pattern| pattern == "**"))
                {
                    bail!(
                        "narrow file scopes currently forbid container commands; scoped mounts are required before dispatch"
                    );
                }
            }
            name if name.starts_with("mcp.") => {
                if !self.read.iter().any(|pattern| pattern == "**")
                    || !self.write.iter().any(|pattern| pattern == "**")
                {
                    bail!("MCP cannot bypass narrowed file scopes");
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scopes_are_bounded_exact_or_subtree_and_never_grant_access() -> Result<()> {
        let scopes = FileScopes {
            read: vec!["src/**".into(), "Cargo.toml".into()],
            write: vec!["src/edit.rs".into()],
        };
        scopes.validate()?;
        assert!(scopes.permits("src/read.rs", false));
        assert!(scopes.permits("src/edit.rs", true));
        assert!(!scopes.permits("src/read.rs", true));
        assert!(!scopes.permits("src-other/read.rs", false));
        assert!(!scopes.permits("src/../secret", false));
        assert!(!scopes.permits("src/.git/config", false));
        assert!(!scopes.permits("C:/secret", false));
        assert!(FileScopes::from_configuration(&json!({}))?.is_none());
        assert!(
            FileScopes {
                read: vec![],
                write: vec!["**".into()]
            }
            .validate()
            .is_err()
        );
        assert!(
            FileScopes {
                read: vec!["src/*".into()],
                write: vec![]
            }
            .validate()
            .is_err()
        );
        assert!(
            FileScopes {
                read: vec!["src\\file".into()],
                write: vec![]
            }
            .validate()
            .is_err()
        );
        Ok(())
    }
}
