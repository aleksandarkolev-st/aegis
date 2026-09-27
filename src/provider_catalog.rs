use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::auth_store::Vault;
use crate::catalog::RemoteModel;
use crate::direct::{Credentials, Provider};

const TTL_SECONDS: u64 = 900;
const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedCatalog {
    version: u32,
    binding: String,
    fetched_at: u64,
    models: Vec<RemoteModel>,
}

fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

fn validate(models: &[RemoteModel], credentials: &Credentials) -> Result<()> {
    if models.is_empty() || models.len() > 256 {
        bail!("Aegis model catalog has an invalid model count");
    }
    let mut seen = std::collections::HashSet::new();
    for model in models {
        let mut efforts = std::collections::HashSet::new();
        if !crate::catalog::valid_id(&model.id)
            || !seen.insert(&model.id)
            || model.label.len() > 400
            || model.label != crate::catalog::display_label(&model.label)
            || model.reasoning_levels.len() > 8
            || model
                .reasoning_levels
                .iter()
                .any(|effort| !crate::catalog::valid_effort(effort) || !efforts.insert(effort))
            || model
                .default_reasoning
                .as_ref()
                .is_some_and(|effort| !model.reasoning_levels.contains(effort))
        {
            bail!("Aegis model catalog contains invalid metadata");
        }
        if credentials.redact(&model.id) != model.id
            || credentials.redact(&model.label) != model.label
        {
            bail!("Aegis model catalog cannot contain sign-in credentials");
        }
    }
    Ok(())
}

pub fn cached(
    vault: &Vault,
    provider: Provider,
    cancelled: impl Fn() -> bool,
) -> Result<Option<Vec<RemoteModel>>> {
    let _lock = vault.lock(provider.session_name(), TIMEOUT, &cancelled)?;
    if cancelled() {
        bail!("Model catalog request cancelled");
    }
    let Some(session) = vault.load(provider.session_name())? else {
        return Ok(None);
    };
    let Ok(credentials) = session.credentials() else {
        return Ok(None);
    };
    let Some(mut bytes) = vault.load_catalog(provider.session_name())? else {
        return Ok(None);
    };
    let catalog = serde_json::from_slice::<SavedCatalog>(&bytes);
    bytes.fill(0);
    let catalog = catalog.map_err(|_| anyhow!("Aegis's private model catalog is invalid"))?;
    let current = now()?;
    if catalog.version != 1
        || catalog.binding != credentials.catalog_binding(provider)
        || catalog.fetched_at > current
        || current.saturating_sub(catalog.fetched_at) >= TTL_SECONDS
    {
        return Ok(None);
    }
    validate(&catalog.models, &credentials)?;
    if cancelled() {
        bail!("Model catalog request cancelled before accepting metadata");
    }
    Ok(Some(catalog.models))
}

pub fn save(
    vault: &Vault,
    provider: Provider,
    credentials: &Credentials,
    models: Vec<RemoteModel>,
    cancelled: impl Fn() -> bool,
) -> Result<()> {
    validate(&models, credentials)?;
    let _lock = vault.lock(provider.session_name(), TIMEOUT, &cancelled)?;
    let current = vault
        .load(provider.session_name())?
        .context("Sign-in changed during model discovery; sign in and retry")?
        .credentials()?;
    let binding = credentials.catalog_binding(provider);
    if binding != current.catalog_binding(provider) {
        bail!("Sign-in changed during model discovery; refresh the model list");
    }
    let catalog = SavedCatalog {
        version: 1,
        binding,
        fetched_at: now()?,
        models,
    };
    if cancelled() {
        bail!("Model catalog request cancelled before saving metadata");
    }
    vault.save_catalog(provider.session_name(), serde_json::to_vec(&catalog)?)
}

pub fn get(
    vault: &Vault,
    provider: Provider,
    cancelled: impl Fn() -> bool,
) -> Result<Vec<RemoteModel>> {
    get_with(
        vault,
        provider,
        &cancelled,
        |credentials, remaining, interrupted| {
            crate::direct::models(provider, credentials, remaining, interrupted)
        },
    )
}

fn get_with(
    vault: &Vault,
    provider: Provider,
    cancelled: &impl Fn() -> bool,
    fetch: impl FnOnce(&Credentials, Duration, &dyn Fn() -> bool) -> Result<Vec<RemoteModel>>,
) -> Result<Vec<RemoteModel>> {
    let started = Instant::now();
    let interrupted = || cancelled() || started.elapsed() >= TIMEOUT;
    if let Some(models) = cached(vault, provider, interrupted)? {
        return Ok(models);
    }
    let credentials = crate::oauth::AuthClient::new(provider)?.credentials(vault, interrupted)?;
    let remaining = TIMEOUT.saturating_sub(started.elapsed());
    if remaining.is_zero() || interrupted() {
        bail!("Model catalog request cancelled or timed out");
    }
    let models = fetch(&credentials, remaining, &interrupted)?;
    save(vault, provider, &credentials, models.clone(), interrupted)?;
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth_store::Session;
    use std::fs;

    fn session(provider: Provider) -> Session {
        Session {
            provider: provider.session_name().into(),
            access_token: "catalog-private-access".into(),
            refresh_token: None,
            account_id: (provider == Provider::ChatGpt).then(|| "catalog-private-account".into()),
            expires_at: now().unwrap() + 3600,
        }
    }

    fn models() -> Vec<RemoteModel> {
        vec![RemoteModel {
            id: "advertised-model".into(),
            label: "Advertised model".into(),
            reasoning_levels: vec!["low".into(), "high".into()],
            default_reasoning: Some("low".into()),
        }]
    }

    #[test]
    fn owned_catalogs_are_private_session_bound_and_reused_without_a_fetch() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let vault = Vault::new(directory.path().join("auth"));
        for provider in [Provider::ChatGpt, Provider::Grok] {
            let session = session(provider);
            vault.save(&session)?;
            let original = fs::read(
                directory
                    .path()
                    .join(format!("auth/{}.session", provider.session_name())),
            )?;
            let mut fetches = 0;
            let first = get_with(&vault, provider, &|| false, |_, _, interrupted| {
                assert!(!interrupted());
                fetches += 1;
                Ok(models())
            })?;
            let reused = get_with(&vault, provider, &|| false, |_, _, _| {
                panic!("Fresh account-bound cache must avoid HTTP")
            })?;
            assert_eq!(fetches, 1);
            assert_eq!(first, reused);
            assert_eq!(first, models());
            let catalog = fs::read(
                directory
                    .path()
                    .join(format!("auth/{}.catalog", provider.session_name())),
            )?;
            assert!(
                !catalog
                    .windows(session.access_token.len())
                    .any(|bytes| bytes == session.access_token.as_bytes())
            );
            assert!(
                !catalog
                    .windows(
                        session
                            .account_id
                            .as_deref()
                            .unwrap_or("catalog-private-account")
                            .len()
                    )
                    .any(|bytes| bytes == b"catalog-private-account")
            );
            assert_eq!(
                fs::read(
                    directory
                        .path()
                        .join(format!("auth/{}.session", provider.session_name()))
                )?,
                original
            );
        }
        Ok(())
    }

    #[test]
    fn token_rotation_account_changes_and_expiry_never_reuse_another_catalog() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let vault = Vault::new(directory.path().join("auth"));
        let provider = Provider::ChatGpt;
        let mut current = session(provider);
        vault.save(&current)?;
        save(&vault, provider, &current.credentials()?, models(), || {
            false
        })?;
        current.account_id = Some("another-account".into());
        vault.save(&current)?;
        assert!(cached(&vault, provider, || false)?.is_none());
        save(&vault, provider, &current.credentials()?, models(), || {
            false
        })?;
        current.access_token = "another-private-token".into();
        vault.save(&current)?;
        assert!(cached(&vault, provider, || false)?.is_none());
        save(&vault, provider, &current.credentials()?, models(), || {
            false
        })?;
        current.expires_at = 1;
        vault.save(&current)?;
        assert!(cached(&vault, provider, || false)?.is_none());
        assert!(
            get_with(&vault, provider, &|| false, |_, _, _| panic!(
                "Expired unrefreshable session must not fetch"
            ))
            .is_err()
        );
        assert!(cached(&vault, Provider::Grok, || false)?.is_none());
        Ok(())
    }

    #[test]
    fn changed_or_removed_sign_in_during_discovery_cannot_commit_old_metadata() -> Result<()> {
        for logout in [false, true] {
            let directory = tempfile::tempdir()?;
            let vault = Vault::new(directory.path().join("auth"));
            let provider = Provider::ChatGpt;
            vault.save(&session(provider))?;
            let result = get_with(&vault, provider, &|| false, |_, _, _| {
                if logout {
                    crate::oauth::AuthClient::new(provider)?.logout(&vault, || false)?;
                } else {
                    let mut replacement = session(provider);
                    replacement.access_token = "replacement-private-token".into();
                    vault.save(&replacement)?;
                }
                Ok(models())
            });
            assert!(result.unwrap_err().to_string().contains("Sign-in changed"));
            assert!(vault.load_catalog(provider.session_name())?.is_none());
        }
        Ok(())
    }

    #[test]
    fn stale_future_and_unknown_version_metadata_are_misses_and_denials_never_fall_back()
    -> Result<()> {
        let directory = tempfile::tempdir()?;
        let vault = Vault::new(directory.path().join("auth"));
        let provider = Provider::Grok;
        let session = session(provider);
        vault.save(&session)?;
        for (version, fetched_at) in [(1, now()? - TTL_SECONDS), (1, now()? + 60), (2, now()?)] {
            let saved = SavedCatalog {
                version,
                fetched_at,
                binding: session.credentials()?.catalog_binding(provider),
                models: models(),
            };
            vault.save_catalog(provider.session_name(), serde_json::to_vec(&saved)?)?;
            assert!(cached(&vault, provider, || false)?.is_none());
            let error = get_with(&vault, provider, &|| false, |_, _, _| {
                bail!("Fixture HTTP 403 account refusal")
            })
            .unwrap_err();
            assert!(error.to_string().contains("403"));
            assert!(cached(&vault, provider, || false)?.is_none());
        }
        Ok(())
    }

    #[test]
    fn cancellation_does_not_save_metadata_or_dispatch_a_fetch() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let vault = Vault::new(directory.path().join("auth"));
        let provider = Provider::Grok;
        vault.save(&session(provider))?;
        assert!(
            get_with(&vault, provider, &|| true, |_, _, _| panic!(
                "Cancelled before dispatch"
            ))
            .is_err()
        );
        let cancelled = std::cell::Cell::new(false);
        assert!(
            get_with(&vault, provider, &|| cancelled.get(), |_, _, _| {
                cancelled.set(true);
                Ok(models())
            })
            .is_err()
        );
        assert!(vault.load_catalog(provider.session_name())?.is_none());
        Ok(())
    }

    #[test]
    fn invalid_or_credential_bearing_metadata_is_rejected_before_private_storage() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let vault = Vault::new(directory.path().join("auth"));
        let provider = Provider::ChatGpt;
        let session = session(provider);
        vault.save(&session)?;
        let original = fs::read(directory.path().join("auth/chatgpt.session"))?;
        for variant in 0..9 {
            let mut models = models();
            match variant {
                0 => models.clear(),
                1 => models.push(models[0].clone()),
                2 => models[0].label = "x".repeat(401),
                3 => models[0].id = "model\u{202e}".into(),
                4 => models[0].reasoning_levels.push("invented".into()),
                5 => models[0].reasoning_levels.push("low".into()),
                6 => models[0].default_reasoning = Some("xhigh".into()),
                7 => models[0].label = session.access_token.clone(),
                _ => models[0].id = session.account_id.clone().unwrap(),
            }
            let error =
                save(&vault, provider, &session.credentials()?, models, || false).unwrap_err();
            assert!(!format!("{error:#}").contains(&session.access_token));
            assert!(vault.load_catalog(provider.session_name())?.is_none());
        }
        assert_eq!(
            fs::read(directory.path().join("auth/chatgpt.session"))?,
            original
        );
        Ok(())
    }

    #[test]
    fn sign_out_removes_only_the_selected_owned_catalog_and_session() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let vault = Vault::new(directory.path().join("auth"));
        for provider in [Provider::ChatGpt, Provider::Grok] {
            let session = session(provider);
            vault.save(&session)?;
            save(&vault, provider, &session.credentials()?, models(), || {
                false
            })?;
        }
        assert!(crate::oauth::AuthClient::new(Provider::ChatGpt)?.logout(&vault, || false)?);
        assert!(vault.load("chatgpt")?.is_none());
        assert!(vault.load_catalog("chatgpt")?.is_none());
        assert_eq!(cached(&vault, Provider::Grok, || false)?, Some(models()));
        Ok(())
    }

    #[test]
    fn catalog_protection_and_file_bounds_cannot_be_confused_with_credentials() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join("auth");
        let vault = Vault::new(root.clone());
        let provider = Provider::ChatGpt;
        let session = session(provider);
        vault.save(&session)?;
        fs::copy(root.join("chatgpt.session"), root.join("chatgpt.catalog"))?;
        assert!(cached(&vault, provider, || false).is_err());
        fs::write(root.join("chatgpt.catalog"), vec![b'x'; 512 * 1024 + 1])?;
        assert!(cached(&vault, provider, || false).is_err());
        assert!(vault.load("chatgpt")?.is_some());
        Ok(())
    }

    #[test]
    fn fresh_owned_sign_in_clears_a_corrupt_cache_without_touching_another_provider() -> Result<()>
    {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join("auth");
        let vault = Vault::new(root.clone());
        for provider in [Provider::ChatGpt, Provider::Grok] {
            let session = session(provider);
            vault.save(&session)?;
            save(&vault, provider, &session.credentials()?, models(), || {
                false
            })?;
        }
        fs::write(root.join("chatgpt.catalog"), b"invalid-disposable-catalog")?;
        assert!(cached(&vault, Provider::ChatGpt, || false).is_err());
        crate::oauth::AuthClient::new(Provider::ChatGpt)?.save(
            &vault,
            &session(Provider::ChatGpt),
            || false,
        )?;
        assert!(vault.load_catalog("chatgpt")?.is_none());
        assert!(vault.load("chatgpt")?.is_some());
        assert_eq!(cached(&vault, Provider::Grok, || false)?, Some(models()));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn catalog_owner_permissions_and_links_are_enforced() -> Result<()> {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let directory = tempfile::tempdir()?;
        let root = directory.path().join("auth");
        let vault = Vault::new(root.clone());
        let session = session(Provider::ChatGpt);
        vault.save(&session)?;
        save(
            &vault,
            Provider::ChatGpt,
            &session.credentials()?,
            models(),
            || false,
        )?;
        let path = root.join("chatgpt.catalog");
        assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644))?;
        assert!(cached(&vault, Provider::ChatGpt, || false).is_err());
        fs::remove_file(&path)?;
        symlink(root.join("chatgpt.session"), &path)?;
        assert!(cached(&vault, Provider::ChatGpt, || false).is_err());
        assert!(
            save(
                &vault,
                Provider::ChatGpt,
                &session.credentials()?,
                models(),
                || false
            )
            .is_err()
        );
        assert!(
            crate::oauth::AuthClient::new(Provider::ChatGpt)?
                .logout(&vault, || false)
                .is_err()
        );
        Ok(())
    }
}
