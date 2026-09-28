use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

const MAX_BYTES: u64 = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Selection {
    pub provider: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
}

impl Selection {
    pub fn validate(&self) -> Result<()> {
        if !matches!(self.provider.as_str(), "codex" | "grok")
            || !crate::catalog::valid_id(&self.model)
            || self
                .reasoning_effort
                .as_ref()
                .is_some_and(|effort| !crate::catalog::valid_effort(effort))
        {
            bail!("Saved provider preference is invalid");
        }
        Ok(())
    }
}

pub(crate) struct SelectionStore {
    path: PathBuf,
}

impl SelectionStore {
    pub fn user() -> Result<Self> {
        let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
            .context("Home directory unavailable for preferences")?;
        Ok(Self {
            path: PathBuf::from(home).join(".aegis").join("selection.json"),
        })
    }

    fn parent(&self) -> Result<PathBuf> {
        let parent = self.path.parent().context("Invalid preference path")?;
        match fs::symlink_metadata(parent) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => bail!("Preference directory must be a real directory"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let builder = fs::DirBuilder::new();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    let mut builder = builder;
                    builder.mode(0o700);
                    builder.create(parent)?;
                }
                #[cfg(not(unix))]
                builder.create(parent)?;
                let metadata = fs::symlink_metadata(parent)?;
                if !metadata.is_dir() || metadata.file_type().is_symlink() {
                    bail!("Preference directory must be a real directory");
                }
            }
            Err(error) => return Err(error.into()),
        }
        Ok(parent.to_path_buf())
    }

    pub fn load(&self) -> Result<Option<Selection>> {
        let parent = self.path.parent().context("Invalid preference path")?;
        match fs::symlink_metadata(parent) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => bail!("Preference directory must be a real directory"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        let metadata = match fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > MAX_BYTES {
            bail!("Saved provider preference is not a bounded regular file");
        }
        let mut bytes = Vec::new();
        File::open(&self.path)?
            .take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_BYTES {
            bail!("Saved provider preference is too large");
        }
        let selection: Selection =
            serde_json::from_slice(&bytes).context("Saved provider preference is invalid")?;
        selection.validate()?;
        Ok(Some(selection))
    }

    pub fn save(&self, selection: &Selection) -> Result<()> {
        selection.validate()?;
        let parent = self.parent()?;
        match fs::symlink_metadata(&self.path) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
            Ok(_) => bail!("Saved provider preference is not a regular file"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temporary
                .as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        temporary.write_all(&serde_json::to_vec(selection)?)?;
        temporary.as_file().sync_all()?;
        temporary.persist(&self.path).map_err(|error| error.error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_round_trips_without_workspace_grants_or_credentials() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let store = SelectionStore {
            path: directory.path().join(".aegis/selection.json"),
        };
        assert!(store.load()?.is_none());
        let selection = Selection {
            provider: "codex".into(),
            model: "advertised-model".into(),
            reasoning_effort: Some("low".into()),
        };
        store.save(&selection)?;
        assert_eq!(store.load()?, Some(selection));
        let saved = fs::read_to_string(&store.path)?;
        assert!(!saved.contains("write"));
        assert!(!saved.contains("endpoint"));
        assert!(!saved.contains("token"));
        let updated = Selection {
            provider: "grok".into(),
            model: "new-model".into(),
            reasoning_effort: None,
        };
        store.save(&updated)?;
        assert_eq!(store.load()?, Some(updated));
        Ok(())
    }

    #[test]
    fn invalid_or_expanded_preferences_are_not_loaded() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let store = SelectionStore {
            path: directory.path().join("selection.json"),
        };
        for selection in [
            serde_json::json!({"provider":"custom","model":"model","reasoning_effort":null}),
            serde_json::json!({"provider":"codex","model":"bad model","reasoning_effort":null}),
            serde_json::json!({"provider":"grok","model":"model","reasoning_effort":"unknown"}),
            serde_json::json!({"provider":"codex","model":"model","reasoning_effort":null,"write":true}),
        ] {
            fs::write(&store.path, serde_json::to_vec(&selection)?)?;
            assert!(store.load().is_err());
        }
        fs::write(&store.path, vec![b'x'; MAX_BYTES as usize + 1])?;
        assert!(store.load().is_err());
        Ok(())
    }
}
