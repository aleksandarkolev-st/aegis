use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

const MAX_PLAIN_BYTES: usize = 128 * 1024;
const MAX_STORED_BYTES: usize = 256 * 1024;

#[cfg(windows)]
const HEADER: &[u8] = b"AEGIS-AUTH-DPAPI-1\n";
#[cfg(not(windows))]
const HEADER: &[u8] = b"AEGIS-AUTH-OWNER-1\n";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Session {
    pub provider: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub account_id: Option<String>,
    pub expires_at: u64,
}

impl Session {
    pub fn credentials(&self) -> Result<crate::direct::Credentials> {
        self.validate(&self.provider)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        if self.expires_at <= now {
            bail!("Aegis sign-in has expired; refresh or sign in again");
        }
        crate::direct::Credentials::new(self.access_token.clone(), self.account_id.clone())
    }

    fn validate(&self, provider: &str) -> Result<()> {
        provider_name(provider)?;
        if self.provider != provider || self.expires_at == 0 || self.expires_at > i64::MAX as u64 {
            bail!("Saved sign-in has an invalid provider or expiration");
        }
        if (provider == "chatgpt" && self.account_id.is_none())
            || (provider == "grok" && self.account_id.is_some())
        {
            bail!("Saved sign-in does not match the selected provider account");
        }
        crate::direct::Credentials::new(self.access_token.clone(), self.account_id.clone())?;
        if self.refresh_token.as_ref().is_some_and(|token| {
            token.is_empty()
                || token.len() > 32768
                || !token.bytes().all(|byte| byte.is_ascii_graphic())
        }) {
            bail!("Saved refresh credential is invalid; sign in again");
        }
        Ok(())
    }
}

fn provider_name(provider: &str) -> Result<&str> {
    match provider {
        "chatgpt" | "grok" => Ok(provider),
        _ => bail!("Only direct ChatGPT and Grok sign-ins can be stored here"),
    }
}

fn checked_metadata(path: &Path, directory: bool) -> Result<fs::Metadata> {
    let metadata =
        fs::symlink_metadata(path).map_err(|_| anyhow!("Aegis sign-in storage is unavailable"))?;
    if metadata.file_type().is_symlink()
        || (directory && !metadata.is_dir())
        || (!directory && !metadata.is_file())
    {
        bail!("Aegis sign-in storage cannot use linked or unexpected file types");
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            bail!("Aegis sign-in storage cannot use reparse points");
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!("Aegis sign-in storage must be accessible only to its owner");
        }
    }
    Ok(metadata)
}

pub struct Vault {
    root: PathBuf,
}

impl Vault {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn user() -> Result<Self> {
        let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
            .context("Your home directory is unavailable for sign-in storage")?;
        Ok(Self::new(PathBuf::from(home).join(".aegis").join("auth")))
    }

    pub(crate) fn lock(
        &self,
        provider: &str,
        timeout: std::time::Duration,
        cancelled: &impl Fn() -> bool,
    ) -> Result<File> {
        provider_name(provider)?;
        if timeout.is_zero() || cancelled() {
            bail!("Sign-in storage operation was cancelled");
        }
        self.prepare()?;
        let path = self.root.join(format!("{provider}.lock"));
        if fs::symlink_metadata(&path).is_ok() {
            checked_metadata(&path, false)?;
        }
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(&path)
            .map_err(|_| anyhow!("Could not open the private sign-in lock"))?;
        checked_metadata(&path, false)?;
        let started = std::time::Instant::now();
        loop {
            if cancelled() {
                bail!("Sign-in storage operation was cancelled");
            }
            match fs2::FileExt::try_lock_exclusive(&file) {
                Ok(()) => return Ok(file),
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
                {
                    if started.elapsed() >= timeout {
                        bail!("Another Aegis sign-in is busy; try again shortly");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                Err(_) => bail!("Could not acquire the private sign-in lock"),
            }
        }
    }

    fn path(&self, provider: &str) -> Result<PathBuf> {
        Ok(self
            .root
            .join(format!("{}.session", provider_name(provider)?)))
    }

    fn check_ancestors(&self) -> Result<()> {
        if !self.root.is_absolute() {
            bail!("Aegis sign-in storage needs an absolute user-owned location");
        }
        for ancestor in self.root.ancestors() {
            match fs::symlink_metadata(ancestor) {
                Ok(metadata) if metadata.is_dir() && !crate::filesystem::linked(&metadata) => {}
                Ok(_) => {
                    bail!("Aegis sign-in storage cannot traverse linked or unexpected directories")
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => bail!("Aegis sign-in storage location is unavailable"),
            }
        }
        Ok(())
    }

    fn prepare(&self) -> Result<()> {
        self.check_ancestors()?;
        if fs::symlink_metadata(&self.root).is_err() {
            let parent = self
                .root
                .parent()
                .context("Sign-in storage needs a parent directory")?;
            fs::create_dir_all(parent)
                .map_err(|_| anyhow!("Could not create Aegis account directory"))?;
            #[cfg(unix)]
            let mut builder = fs::DirBuilder::new();
            #[cfg(not(unix))]
            let builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&self.root) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => bail!("Could not create private Aegis sign-in storage"),
            }
        }
        checked_metadata(&self.root, true)?;
        self.check_ancestors()?;
        Ok(())
    }

    pub fn save(&self, session: &Session) -> Result<()> {
        session.validate(&session.provider)?;
        let path = self.path(&session.provider)?;
        self.prepare()?;
        if fs::symlink_metadata(&path).is_ok() {
            checked_metadata(&path, false)?;
        }
        let mut plain = serde_json::to_vec(session)?;
        if plain.len() > MAX_PLAIN_BYTES {
            plain.fill(0);
            bail!("Sign-in credential exceeds the storage bound");
        }
        let sealed = protect(&session.provider, &plain);
        plain.fill(0);
        let sealed = sealed?;
        if sealed.len() + HEADER.len() > MAX_STORED_BYTES {
            bail!("Protected sign-in credential exceeds the storage bound");
        }
        let mut temporary = tempfile::NamedTempFile::new_in(&self.root)
            .map_err(|_| anyhow!("Could not create a private sign-in file"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temporary
                .as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        temporary.write_all(HEADER)?;
        temporary.write_all(&sealed)?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(&path)
            .map_err(|_| anyhow!("Could not atomically save Aegis sign-in"))?;
        checked_metadata(&path, false)?;
        Ok(())
    }

    pub fn load(&self, provider: &str) -> Result<Option<Session>> {
        let path = self.path(provider)?;
        self.check_ancestors()?;
        match fs::symlink_metadata(&self.root) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => bail!("Aegis sign-in storage is unavailable"),
            Ok(_) => {
                checked_metadata(&self.root, true)?;
            }
        }
        let metadata = match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => bail!("Saved Aegis sign-in is unavailable"),
            Ok(_) => checked_metadata(&path, false)?,
        };
        if metadata.len() > MAX_STORED_BYTES as u64 {
            bail!("Saved sign-in exceeds the storage bound");
        }
        let file = File::open(&path).map_err(|_| anyhow!("Could not open saved Aegis sign-in"))?;
        let mut bytes = Vec::new();
        file.take(MAX_STORED_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| anyhow!("Could not read saved Aegis sign-in"))?;
        if bytes.len() > MAX_STORED_BYTES || !bytes.starts_with(HEADER) {
            bail!("Saved sign-in has an invalid protection format; sign in again");
        }
        let mut plain = unprotect(provider, &bytes[HEADER.len()..])?;
        if plain.len() > MAX_PLAIN_BYTES {
            plain.fill(0);
            bail!("Saved sign-in exceeds its plaintext bound");
        }
        let session = serde_json::from_slice::<Session>(&plain);
        plain.fill(0);
        let session =
            session.map_err(|_| anyhow!("Saved Aegis sign-in is invalid; sign in again"))?;
        session.validate(provider)?;
        Ok(Some(session))
    }

    pub fn remove(&self, provider: &str) -> Result<bool> {
        let path = self.path(provider)?;
        self.check_ancestors()?;
        if !self.root.try_exists()? {
            return Ok(false);
        }
        checked_metadata(&self.root, true)?;
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(_) => bail!("Saved Aegis sign-in is unavailable"),
            Ok(_) => {
                checked_metadata(&path, false)?;
                fs::remove_file(path)
                    .map_err(|_| anyhow!("Could not remove saved Aegis sign-in"))?;
                Ok(true)
            }
        }
    }
}

#[cfg(not(windows))]
fn protect(_provider: &str, bytes: &[u8]) -> Result<Vec<u8>> {
    Ok(bytes.to_vec())
}

#[cfg(not(windows))]
fn unprotect(_provider: &str, bytes: &[u8]) -> Result<Vec<u8>> {
    Ok(bytes.to_vec())
}

#[cfg(windows)]
fn protect(provider: &str, bytes: &[u8]) -> Result<Vec<u8>> {
    dpapi(provider, bytes, true)
}

#[cfg(windows)]
fn unprotect(provider: &str, bytes: &[u8]) -> Result<Vec<u8>> {
    dpapi(provider, bytes, false)
}

#[cfg(windows)]
fn dpapi(provider: &str, bytes: &[u8], encrypt: bool) -> Result<Vec<u8>> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };
    struct Output(CRYPT_INTEGER_BLOB);
    impl Drop for Output {
        fn drop(&mut self) {
            if !self.0.pbData.is_null() {
                unsafe {
                    std::ptr::write_bytes(self.0.pbData, 0, self.0.cbData as usize);
                    LocalFree(self.0.pbData.cast());
                }
            }
        }
    }
    if bytes.is_empty() || bytes.len() > MAX_STORED_BYTES {
        bail!("Invalid protected sign-in size");
    }
    let mut input = bytes.to_vec();
    let mut entropy = format!("aegis/auth/v1/{provider}").into_bytes();
    let input_blob = CRYPT_INTEGER_BLOB {
        cbData: input.len() as u32,
        pbData: input.as_mut_ptr(),
    };
    let entropy_blob = CRYPT_INTEGER_BLOB {
        cbData: entropy.len() as u32,
        pbData: entropy.as_mut_ptr(),
    };
    let mut output = Output(CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    });
    let success = unsafe {
        if encrypt {
            CryptProtectData(
                &input_blob,
                std::ptr::null(),
                &entropy_blob,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output.0,
            )
        } else {
            CryptUnprotectData(
                &input_blob,
                std::ptr::null_mut(),
                &entropy_blob,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output.0,
            )
        }
    };
    input.fill(0);
    if success == 0
        || output.0.pbData.is_null()
        || output.0.cbData == 0
        || output.0.cbData as usize > MAX_STORED_BYTES
    {
        bail!("Windows could not protect or unlock this Aegis sign-in; sign in again");
    }
    Ok(unsafe { std::slice::from_raw_parts(output.0.pbData, output.0.cbData as usize) }.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(provider: &str) -> Session {
        Session {
            provider: provider.into(),
            access_token: "fixture-private-access".into(),
            refresh_token: Some("fixture-private-refresh".into()),
            account_id: (provider == "chatgpt").then(|| "fixture-account".into()),
            expires_at: 2000000000,
        }
    }

    #[test]
    fn private_atomic_vault_roundtrips_updates_and_removes_only_the_chosen_provider() -> Result<()>
    {
        let directory = tempfile::tempdir()?;
        let vault = Vault::new(directory.path().join("auth"));
        assert!(vault.load("chatgpt")?.is_none());
        assert!(!vault.root.exists());
        vault.save(&session("chatgpt"))?;
        vault.save(&session("grok"))?;
        let mut current = vault.load("chatgpt")?.unwrap();
        assert_eq!(current.access_token, "fixture-private-access");
        assert_eq!(
            current.refresh_token.as_deref(),
            Some("fixture-private-refresh")
        );
        current.access_token = "replaced-private-access".into();
        vault.save(&current)?;
        assert_eq!(
            vault.load("chatgpt")?.unwrap().access_token,
            "replaced-private-access"
        );
        assert_eq!(fs::read_dir(&vault.root)?.count(), 2);
        assert!(vault.remove("chatgpt")?);
        assert!(!vault.remove("chatgpt")?);
        assert!(vault.load("grok")?.is_some());
        assert!(!vault.load("chatgpt")?.is_some());
        Ok(())
    }

    #[test]
    fn vault_rejects_unknown_providers_wrong_accounts_corruption_and_oversize_without_secret_errors()
    -> Result<()> {
        let directory = tempfile::tempdir()?;
        let vault = Vault::new(directory.path().join("auth"));
        assert!(vault.load("../chatgpt").is_err());
        assert!(vault.load("claude").is_err());
        assert!(
            Vault::new("relative-auth".into())
                .save(&session("chatgpt"))
                .is_err()
        );
        let mut expired = session("chatgpt");
        expired.expires_at = 1;
        assert!(expired.credentials().is_err());
        let mut invalid = session("chatgpt");
        invalid.account_id = None;
        assert!(vault.save(&invalid).is_err());
        assert!(!vault.root.exists());
        invalid = session("grok");
        invalid.refresh_token = Some("fixture-private-refresh\ninvalid".into());
        assert!(
            !vault
                .save(&invalid)
                .unwrap_err()
                .to_string()
                .contains("fixture-private-refresh")
        );
        vault.save(&session("chatgpt"))?;
        fs::copy(vault.path("chatgpt")?, vault.path("grok")?)?;
        assert!(vault.load("grok").is_err());
        fs::write(vault.path("chatgpt")?, b"fixture-private-access")?;
        assert!(
            !vault
                .load("chatgpt")
                .err()
                .unwrap()
                .to_string()
                .contains("fixture-private-access")
        );
        fs::write(vault.path("chatgpt")?, vec![b'x'; MAX_STORED_BYTES + 1])?;
        assert!(vault.load("chatgpt").is_err());
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_uses_user_bound_dpapi_never_plaintext_and_rejects_tampering() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let vault = Vault::new(directory.path().join("auth"));
        vault.save(&session("chatgpt"))?;
        let path = vault.path("chatgpt")?;
        let mut bytes = fs::read(&path)?;
        assert!(
            !bytes
                .windows("fixture-private-access".len())
                .any(|window| window == b"fixture-private-access")
        );
        assert!(
            !bytes
                .windows("fixture-private-refresh".len())
                .any(|window| window == b"fixture-private-refresh")
        );
        let offset = bytes.len() - 1;
        bytes[offset] ^= 0xff;
        fs::write(path, bytes)?;
        assert!(vault.load("chatgpt").is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn unix_owner_only_permissions_and_link_rejection_are_enforced() -> Result<()> {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let directory = tempfile::tempdir()?;
        let vault = Vault::new(directory.path().join("auth"));
        vault.save(&session("chatgpt"))?;
        assert_eq!(
            fs::metadata(&vault.root)?.permissions().mode() & 0o777,
            0o700
        );
        let path = vault.path("chatgpt")?;
        assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644))?;
        assert!(vault.load("chatgpt").is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        let linked = vault.path("grok")?;
        symlink(&path, &linked)?;
        assert!(vault.load("grok").is_err());
        assert!(vault.save(&session("grok")).is_err());
        assert!(vault.remove("grok").is_err());
        assert!(vault.load("chatgpt")?.is_some());
        let parent = directory.path().join("parent-link");
        symlink(&vault.root, &parent)?;
        let linked_vault = Vault::new(parent.join("nested"));
        assert!(linked_vault.save(&session("chatgpt")).is_err());
        assert!(!vault.root.join("nested").exists());
        assert!(linked_vault.load("chatgpt").is_err());
        Ok(())
    }
}
