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

pub(crate) struct Mount {
    pub source: PathBuf,
    pub target: String,
    pub writable: bool,
}

pub(crate) fn linked(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

pub(crate) fn path_name(path: &str) -> Result<String> {
    let path = path.replace('\\', "/");
    if path.is_empty()
        || path.len() > 512
        || path
            .chars()
            .any(|character| character.is_control() || ":*?".contains(character))
        || path.split('/').any(|part| {
            part.is_empty()
                || matches!(part, "." | "..")
                || part.ends_with(['.', ' '])
                || matches!(
                    part.split('.')
                        .next()
                        .unwrap_or("")
                        .to_ascii_uppercase()
                        .as_str(),
                    "CON"
                        | "PRN"
                        | "AUX"
                        | "NUL"
                        | "COM1"
                        | "COM2"
                        | "COM3"
                        | "COM4"
                        | "COM5"
                        | "COM6"
                        | "COM7"
                        | "COM8"
                        | "COM9"
                        | "LPT1"
                        | "LPT2"
                        | "LPT3"
                        | "LPT4"
                        | "LPT5"
                        | "LPT6"
                        | "LPT7"
                        | "LPT8"
                        | "LPT9"
                )
                || part.eq_ignore_ascii_case(".git")
                || part.eq_ignore_ascii_case(".arun")
        })
    {
        bail!(
            "file paths must be relative portable names without traversal, reserved devices or metadata"
        );
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
                    if linked(&metadata) {
                        bail!("scoped paths cannot traverse symbolic links or junctions");
                    }
                }
                Err(error) if write && error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(target)
    }

    pub(crate) fn may_descend(&self, path: &str) -> bool {
        path_name(path).is_ok_and(|path| {
            self.read
                .iter()
                .any(|pattern| covers(pattern, &path) || pattern.starts_with(&format!("{path}/")))
        })
    }

    pub(crate) fn mounts(&self, run: &Run) -> Result<(Vec<Mount>, Vec<String>)> {
        let workspace = Path::new(&run.workspace);
        let writable = run
            .grants
            .as_array()
            .is_some_and(|grants| grants.iter().any(|grant| grant == "workspace.write"));
        let mut mounts: Vec<Mount> = Vec::new();
        let mut masks = Vec::new();
        let mut scanned = 0;
        for (patterns, write) in [(&self.read, false), (&self.write, true)] {
            if write && !writable {
                continue;
            }
            for pattern in patterns {
                if patterns
                    .iter()
                    .any(|other| other != pattern && covers(other, pattern))
                {
                    continue;
                }
                let relative = if pattern == "**" {
                    ""
                } else {
                    pattern.strip_suffix("/**").unwrap_or(pattern)
                };
                let source = if relative.is_empty() {
                    workspace.to_path_buf()
                } else {
                    self.checked_path(workspace, relative, write)?
                };
                let metadata = fs::symlink_metadata(&source)
                    .context("scoped container mounts require existing files or directories")?;
                if linked(&metadata) {
                    bail!("scoped mounts cannot include links or junctions");
                }
                if (pattern == "**" || pattern.ends_with("/**")) != metadata.is_dir() {
                    bail!("directory scopes need /** and exact scopes must name files");
                }
                let mut pending = if metadata.is_dir() {
                    vec![source.clone()]
                } else {
                    Vec::new()
                };
                while let Some(directory) = pending.pop() {
                    for entry in fs::read_dir(directory)? {
                        let entry = entry?;
                        scanned += 1;
                        if scanned > 100_000 {
                            bail!("scoped mount inspection exceeds 100000 entries");
                        }
                        let metadata = fs::symlink_metadata(entry.path())?;
                        if linked(&metadata) {
                            bail!("scoped mount trees cannot include links or junctions");
                        }
                        let relative = entry
                            .path()
                            .strip_prefix(workspace)?
                            .to_string_lossy()
                            .replace('\\', "/");
                        let name = entry.file_name().to_string_lossy().into_owned();
                        if name.eq_ignore_ascii_case(".git") || name.eq_ignore_ascii_case(".arun") {
                            if !metadata.is_dir() {
                                bail!("scoped command metadata must be a directory to mask it");
                            }
                            masks.push(format!("/workspace/{relative}"));
                            continue;
                        }
                        path_name(&relative)?;
                        if metadata.is_dir() {
                            pending.push(entry.path());
                        }
                    }
                }
                let source = dunce::simplified(&source).to_path_buf();
                if source.to_string_lossy().contains(',') || relative.contains(',') {
                    bail!("scoped Docker mounts cannot contain commas");
                }
                let target = if relative.is_empty() {
                    "/workspace".into()
                } else {
                    format!("/workspace/{relative}")
                };
                if let Some(existing) = mounts.iter_mut().find(|mount| mount.target == target) {
                    existing.writable |= write;
                } else {
                    mounts.push(Mount {
                        source,
                        target,
                        writable: write,
                    });
                }
            }
        }
        masks.sort();
        masks.dedup();
        Ok((mounts, masks))
    }

    pub fn authorize(&self, run: &Run, capability: &str, arguments: &Value) -> Result<()> {
        match capability {
            "workspace.read" | "workspace.write" | "workspace.patch" => {
                let path = arguments["path"].as_str().context("file path missing")?;
                self.checked_path(
                    Path::new(&run.workspace),
                    path,
                    capability != "workspace.read",
                )?;
            }
            "workspace.search" if self.read.is_empty() => {
                bail!("no file paths are authorized for search")
            }
            "process.run" => {
                self.mounts(run)?;
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
        assert!(!scopes.permits("src/.git./config", false));
        assert!(!scopes.permits("src/NUL.txt", false));
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

    #[test]
    fn container_scope_plans_merge_overlays_and_do_not_grant_unapproved_writes() -> Result<()> {
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join("file.txt"), "original")?;
        let mut store = crate::storage::Store::open(&directory.path().join(".arun"))?;
        for grants in [
            serde_json::json!(["workspace.read"]),
            serde_json::json!(["workspace.read", "workspace.write"]),
        ] {
            let run = store.create_run(
                "scope",
                directory.path(),
                "fixture",
                grants.clone(),
                serde_json::json!({}),
                "",
            )?;
            let scopes = FileScopes {
                read: vec!["**".into(), "**".into()],
                write: vec!["**".into()],
            };
            let (mounts, masks) = scopes.mounts(&run)?;
            assert_eq!(mounts.len(), 1);
            assert_eq!(mounts[0].writable, grants.as_array().unwrap().len() == 2);
            assert!(masks.iter().any(|path| path.ends_with("/.arun")));
            assert_eq!(mounts[0].target, "/workspace");
        }
        Ok(())
    }
}
