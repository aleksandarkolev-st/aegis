use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

const CONFIG_FILE: &str = "remote.json";
const MAX_CONFIG_BYTES: u64 = 16 * 1024;

/// Local remote-control configuration. It contains environment variable names,
/// never NATS or relay credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub installation_id: String,
    pub actor_id: String,
    pub workspace: PathBuf,
    pub relay_admin_url: String,
    pub admin_token_env: String,
    pub nats_url: String,
    pub nats_token_env: String,
    #[serde(default)]
    pub nats_root_certificate: Option<PathBuf>,
}

impl Config {
    pub fn path(root: &Path) -> PathBuf {
        root.join(CONFIG_FILE)
    }

    pub fn load(root: &Path) -> Result<Self> {
        let path = Self::path(root);
        let metadata = fs::symlink_metadata(&path)
            .context("Aegis remote is not paired; run 'aegis remote pair' first")?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("Aegis remote configuration must be a regular file");
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                bail!("Aegis remote configuration cannot be a reparse point");
            }
        }
        if metadata.len() > MAX_CONFIG_BYTES {
            bail!("Aegis remote configuration exceeds its size limit");
        }
        let config: Self = serde_json::from_slice(&fs::read(path)?)
            .context("Aegis remote configuration is invalid")?;
        config.validate(root)?;
        Ok(config)
    }

    pub fn save(&self, root: &Path) -> Result<()> {
        self.validate(root)?;
        let path = Self::path(root);
        if let Ok(metadata) = fs::symlink_metadata(&path)
            && (metadata.file_type().is_symlink() || !metadata.is_file())
        {
            bail!("Aegis remote configuration cannot replace a link or non-file");
        }
        let bytes = serde_json::to_vec_pretty(self)?;
        if bytes.len() as u64 > MAX_CONFIG_BYTES {
            bail!("Aegis remote configuration exceeds its size limit");
        }
        let mut temporary = tempfile::NamedTempFile::new_in(root)?;
        use std::io::Write;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        temporary.persist(path).map_err(|error| error.error)?;
        Ok(())
    }

    pub fn validate(&self, root: &Path) -> Result<()> {
        uuid::Uuid::parse_str(&self.installation_id)
            .context("invalid Aegis remote installation ID")?;
        uuid::Uuid::parse_str(&self.actor_id).context("invalid Aegis remote actor ID")?;
        let expected_workspace = root
            .parent()
            .context("Aegis data directory has no workspace parent")?;
        let expected_workspace =
            dunce::canonicalize(expected_workspace).context("Aegis workspace is unavailable")?;
        let configured_workspace = dunce::canonicalize(&self.workspace)
            .context("configured Aegis workspace is unavailable")?;
        if expected_workspace != configured_workspace {
            bail!("remote workspace must be the parent of this Aegis data directory");
        }
        validate_env_name(&self.admin_token_env)?;
        validate_env_name(&self.nats_token_env)?;
        validate_admin_url(&self.relay_admin_url)?;
        if !self.nats_url.starts_with("tls://")
            || self.nats_url.len() > 2048
            || self.nats_url.bytes().any(|byte| byte.is_ascii_whitespace())
            || self.nats_url.contains('@')
        {
            bail!("NATS endpoint must be a TLS URL without embedded credentials");
        }
        if let Some(certificate) = &self.nats_root_certificate {
            let metadata = fs::symlink_metadata(certificate)
                .context("NATS root certificate is unavailable")?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!("NATS root certificate must be a regular file");
            }
        }
        Ok(())
    }
}

pub fn validate_env_name(name: &str) -> Result<()> {
    let mut bytes = name.bytes();
    let first = bytes.next();
    if !matches!(first, Some(b'A'..=b'Z' | b'a'..=b'z' | b'_'))
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        || name.len() > 128
    {
        bail!("environment variable name is invalid");
    }
    Ok(())
}

fn validate_admin_url(value: &str) -> Result<()> {
    let url = reqwest::Url::parse(value).context("relay admin URL is invalid")?;
    let local_http = url.scheme() == "http"
        && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if !matches!(url.scheme(), "https") && !local_http {
        bail!("relay admin URL must use HTTPS; HTTP is allowed only for localhost development");
    }
    if url.username() != ""
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("relay admin URL cannot contain credentials, query parameters, or fragments");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(root: &Path) -> Config {
        Config {
            installation_id: uuid::Uuid::new_v4().to_string(),
            actor_id: uuid::Uuid::new_v4().to_string(),
            workspace: root.parent().unwrap().to_path_buf(),
            relay_admin_url: "https://relay.example/admin".into(),
            admin_token_env: "AEGIS_RELAY_ADMIN_TOKEN".into(),
            nats_url: "tls://nats.example:4222".into(),
            nats_token_env: "AEGIS_NATS_TOKEN".into(),
            nats_root_certificate: None,
        }
    }

    #[test]
    fn config_roundtrip_keeps_secrets_out_and_pins_workspace() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        let root = workspace.path().join(".arun");
        fs::create_dir_all(&root)?;
        let config = fixture(&root);
        config.save(&root)?;
        assert_eq!(Config::load(&root)?, config);
        let contents = fs::read_to_string(Config::path(&root))?;
        assert!(!contents.contains("TOKEN_VALUE"));
        Ok(())
    }

    #[test]
    fn config_rejects_plaintext_remote_endpoints_and_nonlocal_http() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        let root = workspace.path().join(".arun");
        fs::create_dir_all(&root)?;
        let mut config = fixture(&root);
        config.nats_url = "nats://nats.example:4222".into();
        assert!(config.validate(&root).is_err());
        config = fixture(&root);
        config.relay_admin_url = "http://relay.example/admin".into();
        assert!(config.validate(&root).is_err());
        config.relay_admin_url = "http://127.0.0.1:8787".into();
        assert!(config.validate(&root).is_ok());
        Ok(())
    }
}
