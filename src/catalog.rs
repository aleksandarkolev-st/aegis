use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct Model {
    pub id: String,
    pub label: String,
}

pub struct Catalog {
    pub models: Vec<Model>,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteModel {
    pub id: String,
    pub label: String,
    pub reasoning_levels: Vec<String>,
    pub default_reasoning: Option<String>,
}

pub fn remote_value(provider: crate::direct::Provider, value: &Value) -> Result<Vec<RemoteModel>> {
    use crate::direct::Provider;

    let entries = value[match provider {
        Provider::ChatGpt => "models",
        Provider::Grok => "data",
    }]
    .as_array()
    .context("Provider returned an invalid model catalog")?;
    if entries.len() > 4096 {
        bail!("Provider catalog exceeds its model bound");
    }
    let mut entries = entries.iter().collect::<Vec<_>>();
    if provider == Provider::ChatGpt {
        entries.sort_by_key(|model| model["priority"].as_i64().unwrap_or(i64::MAX));
    }
    let mut models = Vec::<RemoteModel>::new();
    for entry in entries {
        let meta = &entry["_meta"];
        let (id, label, levels, default) = match provider {
            Provider::ChatGpt => {
                if entry["visibility"] != "list" {
                    continue;
                }
                (
                    entry["slug"].as_str(),
                    entry["display_name"].as_str(),
                    &entry["supported_reasoning_levels"],
                    entry["default_reasoning_level"].as_str(),
                )
            }
            Provider::Grok => {
                if entry.get("hidden").unwrap_or(&meta["hidden"]) == &Value::Bool(true) {
                    continue;
                }
                let supports = entry
                    .get("supportsReasoningEffort")
                    .or_else(|| entry.get("supports_reasoning_effort"))
                    .unwrap_or(&meta["supportsReasoningEffort"])
                    .as_bool()
                    == Some(true);
                let advertised = entry
                    .get("reasoningEfforts")
                    .or_else(|| entry.get("reasoning_efforts"))
                    .unwrap_or(&meta["reasoningEfforts"]);
                let levels = if !supports {
                    &Value::Null
                } else if advertised
                    .as_array()
                    .is_some_and(|levels| !levels.is_empty())
                {
                    advertised
                } else {
                    &entry["capabilities"]["reasoning_effort"]
                };
                (
                    entry["model"]
                        .as_str()
                        .or_else(|| entry["modelId"].as_str())
                        .or_else(|| entry["id"].as_str())
                        .or_else(|| meta["model"].as_str())
                        .or_else(|| meta["modelId"].as_str()),
                    entry["name"].as_str(),
                    levels,
                    entry["capabilities"]["default_reasoning_effort"].as_str(),
                )
            }
        };
        let Some(id) = id.filter(|id| valid_id(id)) else {
            continue;
        };
        if models.iter().any(|model| model.id == id) {
            continue;
        }
        let mut reasoning_levels = Vec::<String>::new();
        let mut marked_default = None;
        for level in levels.as_array().into_iter().flatten() {
            let effort = match provider {
                Provider::ChatGpt => level["effort"].as_str(),
                Provider::Grok => level.as_str().or_else(|| level["value"].as_str()),
            };
            if let Some(effort) = effort.filter(|effort| valid_effort(effort)) {
                if !reasoning_levels.iter().any(|value| value == effort) {
                    reasoning_levels.push(effort.to_owned());
                }
                if level["default"] == true && marked_default.is_none() {
                    marked_default = Some(effort);
                }
            }
        }
        let default_reasoning = default
            .or(marked_default)
            .filter(|effort| reasoning_levels.iter().any(|value| value == effort))
            .map(str::to_owned);
        models.push(RemoteModel {
            id: id.to_owned(),
            label: display_label(label.unwrap_or(id)),
            reasoning_levels,
            default_reasoning,
        });
        if models.len() == 256 {
            break;
        }
    }
    if models.is_empty() {
        let incoming = match provider {
            Provider::ChatGpt => value["models"].as_array(),
            Provider::Grok => value["data"].as_array(),
        }
        .map_or(0, Vec::len);
        bail!(
            "Provider advertised no selectable models for this account ({incoming} incoming entries)"
        );
    }
    Ok(models)
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 160
        && !id
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
        && crate::text::clean(id) == id
}

pub(crate) fn display_label(label: &str) -> String {
    let label = crate::terminal::fit(label, 100);
    let mut end = label.len().min(400);
    while !label.is_char_boundary(end) {
        end -= 1;
    }
    label[..end].to_owned()
}

pub fn valid_effort(effort: &str) -> bool {
    matches!(
        effort,
        "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra"
    )
}

pub fn discovery_error(error: &str) -> &'static str {
    let lower = error.to_lowercase();
    if ["401", "expired", "not signed in"]
        .iter()
        .any(|part| lower.contains(part))
    {
        "Sign in with F4 to load models for your account."
    } else if lower.contains("403") {
        "Your account doesn't permit model discovery for this provider."
    } else if ["429", "usage limit"]
        .iter()
        .any(|part| lower.contains(part))
    {
        "The provider's usage limit was reached; try again later."
    } else if ["timeout", "timed out", "connection failed"]
        .iter()
        .any(|part| lower.contains(part))
    {
        "The provider didn't respond. Check your connection and retry."
    } else if lower.contains("private model catalog") || lower.contains("storage") {
        "Aegis couldn't read its private model cache. F4 can reconnect this account."
    } else if lower.contains("no selectable models") {
        "This account advertised no selectable models."
    } else if lower.contains("sign-in changed") {
        "Your account changed during discovery. Refresh the model list."
    } else {
        "No model list is available yet. Sign in with F4 or enter a model ID."
    }
}

pub fn reasoning_from_value(provider: &str, id: &str, catalog: &Value) -> Vec<String> {
    let levels = match crate::provider::canonical(provider) {
        "codex" => catalog["models"]
            .as_array()
            .and_then(|models| {
                models
                    .iter()
                    .find(|model| model["slug"] == id && model["visibility"] == "list")
            })
            .map(|model| &model["supported_reasoning_levels"]),
        "grok" => catalog["models"]
            .as_object()
            .and_then(|models| {
                models.values().find(|model| {
                    model["info"]["id"] == id
                        && model["info"]["hidden"] != true
                        && model["info"]["supports_reasoning_effort"] == true
                })
            })
            .map(|model| &model["info"]["reasoning_efforts"]),
        _ => None,
    };
    let mut result = Vec::new();
    for level in levels.and_then(Value::as_array).into_iter().flatten() {
        if let Some(effort) = level[if crate::provider::canonical(provider) == "codex" {
            "effort"
        } else {
            "value"
        }]
        .as_str()
        {
            if valid_effort(effort) && !result.iter().any(|value| value == effort) {
                result.push(effort.to_owned());
            }
        }
    }
    result
}

pub fn reasoning_levels(provider: &str, id: &str) -> Result<Vec<String>> {
    let provider = crate::provider::canonical(provider);
    if provider == "custom" {
        return Ok(["none", "minimal", "low", "medium", "high", "xhigh"]
            .map(str::to_owned)
            .to_vec());
    }
    if let Ok(selected) = crate::direct::provider(provider) {
        let vault = crate::auth_store::Vault::user()?;
        if vault.load(selected.session_name())?.is_some() {
            return Ok(
                crate::provider_catalog::saved_for_selection(&vault, selected, || false)?
                    .into_iter()
                    .flat_map(|(models, _)| models)
                    .find(|model| model.id == id)
                    .map(|model| model.reasoning_levels)
                    .unwrap_or_default(),
            );
        }
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .context("Home directory unavailable")?;
    let path = match provider {
        "codex" => std::env::var_os("CODEX_HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(&home).join(".codex")),
        "grok" => std::path::PathBuf::from(home).join(".grok"),
        _ => return Ok(Vec::new()),
    }
    .join("models_cache.json");
    let mut bytes = Vec::new();
    File::open(path)?
        .take(8 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 8 * 1024 * 1024 {
        bail!("Model metadata exceeds 8 MiB");
    }
    Ok(reasoning_from_value(
        provider,
        id,
        &serde_json::from_slice(&bytes)?,
    ))
}

pub fn advertised_reasoning_default(provider: &str, id: &str) -> Result<Option<String>> {
    let Ok(selected) = crate::direct::provider(provider) else {
        return Ok(None);
    };
    let vault = crate::auth_store::Vault::user()?;
    if vault.load(selected.session_name())?.is_none() {
        return Ok(None);
    }
    Ok(
        crate::provider_catalog::saved_for_selection(&vault, selected, || false)?
            .into_iter()
            .flat_map(|(models, _)| models)
            .find(|model| model.id == id)
            .and_then(|model| model.default_reasoning),
    )
}

fn insert(models: &mut Vec<Model>, id: &str, label: &str) {
    if models.len() < 256 && valid_id(id) && !models.iter().any(|model| model.id == id) {
        models.push(Model {
            id: id.into(),
            label: crate::terminal::fit(label, 100),
        });
    }
}

pub fn codex_cache(path: &Path) -> Result<Catalog> {
    let file = File::open(path).context("No Codex model cache yet. Sign in with F4; provider default and manual selection remain available.")?;
    let mut bytes = Vec::new();
    file.take(8 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 8 * 1024 * 1024 {
        bail!("Codex model cache is too large");
    }
    let value: Value = serde_json::from_slice(&bytes)?;
    let mut models = Vec::new();
    for model in value["models"]
        .as_array()
        .context("Codex model cache has no catalog")?
    {
        if model["visibility"].as_str() != Some("list") {
            continue;
        }
        if let Some(id) = model["slug"].as_str() {
            insert(
                &mut models,
                id,
                model["display_name"].as_str().unwrap_or(id),
            );
        }
    }
    let fetched = value["fetched_at"].as_str().unwrap_or("date unavailable");
    Ok(Catalog {
        models,
        source: format!(
            "Codex cached catalog · {} · account access can change",
            crate::terminal::fit(fetched, 40)
        ),
    })
}

pub fn grok_output(text: &str) -> Catalog {
    let mut models = Vec::new();
    let mut listing = false;
    for line in text.lines() {
        if line.trim() == "Available models:" {
            listing = true;
            continue;
        }
        if !listing {
            continue;
        }
        let line = line.trim().trim_start_matches(['*', '-']).trim();
        if let Some(id) = line.split_whitespace().next() {
            if id.starts_with("grok-") {
                insert(&mut models, id, line);
            }
        }
    }
    let source = if text.to_lowercase().contains("not authenticated") {
        "Grok CLI fallback catalog · sign in with F4 to see account models"
    } else {
        "Grok CLI available models"
    };
    Catalog {
        models,
        source: source.into(),
    }
}

pub fn grok_cache(path: &Path) -> Result<Catalog> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(8 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 8 * 1024 * 1024 {
        bail!("Grok model cache is too large");
    }
    let value: Value = serde_json::from_slice(&bytes)?;
    let mut models = Vec::new();
    for (id, model) in value["models"]
        .as_object()
        .context("Grok cache has no model catalog")?
    {
        let info = &model["info"];
        if info["hidden"].as_bool() == Some(true) {
            continue;
        }
        if info.is_object() {
            insert(
                &mut models,
                info["id"].as_str().unwrap_or(id),
                info["name"].as_str().unwrap_or(id),
            );
        }
    }
    Ok(Catalog {
        models,
        source: format!(
            "Grok cached catalog · {} · account access can change",
            crate::terminal::fit(
                value["fetched_at"].as_str().unwrap_or("date unavailable"),
                40
            )
        ),
    })
}

pub fn native(provider: &str) -> Result<Catalog> {
    match crate::provider::canonical(provider) {
        "codex" => {
            let home = std::env::var_os("CODEX_HOME")
                .map(std::path::PathBuf::from)
                .or_else(|| {
                    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                        .map(|home| std::path::PathBuf::from(home).join(".codex"))
                })
                .context("Codex home is unavailable")?;
            codex_cache(&home.join("models_cache.json"))
        }
        "claude" => bail!("Claude sign-in is pending; use another provider"),
        "grok" => {
            let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                .context("Home directory unavailable")?;
            grok_cache(&std::path::PathBuf::from(home).join(".grok/models_cache.json"))
                .context("No cached Grok catalog is available; enter an advertised model ID")
        }
        _ => bail!("Unsupported native provider"),
    }
}

pub fn available(provider: &str, cancelled: impl Fn() -> bool) -> Result<Catalog> {
    let selected = crate::direct::provider(provider)?;
    let vault = crate::auth_store::Vault::user()?;
    if vault.load(selected.session_name())?.is_none() {
        let mut catalog = native(provider)?;
        catalog.source.push_str(" · not an Aegis sign-in");
        return Ok(catalog);
    }
    let models = crate::provider_catalog::get(&vault, selected, cancelled)?;
    Ok(owned_models(models))
}

fn owned_models(models: Vec<RemoteModel>) -> Catalog {
    Catalog {
        models: models
            .into_iter()
            .map(|model| Model {
                id: model.id,
                label: model.label,
            })
            .collect(),
        source: "Aegis account-bound catalog · cached up to 15 minutes · availability can change"
            .into(),
    }
}

pub fn saved_only_for_selection(provider: &str) -> Result<Option<Catalog>> {
    let Ok(selected) = crate::direct::provider(provider) else {
        return Ok(None);
    };
    let vault = crate::auth_store::Vault::user()?;
    saved_catalog(&vault, selected, false, || false)
}

pub fn refresh_for_selection(provider: &str, cancelled: impl Fn() -> bool) -> Result<Catalog> {
    let selected = crate::direct::provider(provider)?;
    let vault = crate::auth_store::Vault::user()?;
    if vault.load(selected.session_name())?.is_none() {
        return available(provider, cancelled);
    }
    match crate::provider_catalog::refresh(&vault, selected, &cancelled) {
        Ok(models) => Ok(owned_models(models)),
        Err(error) => saved_selection(&vault, selected, &cancelled, error),
    }
}

pub fn for_selection(provider: &str, cancelled: impl Fn() -> bool) -> Result<Catalog> {
    match available(provider, &cancelled) {
        Ok(catalog) => Ok(catalog),
        Err(error) => {
            let selected = crate::direct::provider(provider)?;
            let vault = crate::auth_store::Vault::user()?;
            saved_selection(&vault, selected, &cancelled, error)
        }
    }
}

fn saved_selection(
    vault: &crate::auth_store::Vault,
    provider: crate::direct::Provider,
    cancelled: impl Fn() -> bool,
    error: anyhow::Error,
) -> Result<Catalog> {
    if cancelled() {
        return Err(error);
    }
    saved_catalog(vault, provider, true, cancelled)?.ok_or(error)
}

fn saved_catalog(
    vault: &crate::auth_store::Vault,
    provider: crate::direct::Provider,
    refresh_failed: bool,
    cancelled: impl Fn() -> bool,
) -> Result<Option<Catalog>> {
    let Some((models, age)) =
        crate::provider_catalog::saved_for_selection(vault, provider, cancelled)?
    else {
        return Ok(None);
    };
    let age = match age {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", age / 60),
        3600..=86399 => format!("{}h ago", age / 3600),
        _ => format!("{}d ago", age / 86400),
    };
    Ok(Some(Catalog {
        models: models
            .into_iter()
            .map(|model| Model {
                id: model.id,
                label: model.label,
            })
            .collect(),
        source: format!(
            "{} · {age} · access may have changed",
            if refresh_failed {
                "Refresh unavailable; showing saved Aegis model list"
            } else {
                "Saved Aegis model list"
            }
        ),
    }))
}

pub fn endpoint_value(value: &Value) -> Result<Vec<Model>> {
    let mut models = Vec::new();
    for model in value["data"]
        .as_array()
        .context("Endpoint has no OpenAI-compatible model catalog")?
    {
        if let Some(id) = model["id"].as_str() {
            insert(&mut models, id, id);
        }
    }
    models.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    #[test]
    fn saved_picker_fallback_is_account_bound_and_never_used_after_cancellation() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let vault = crate::auth_store::Vault::new(directory.path().join("auth"));
        let provider = crate::direct::Provider::ChatGpt;
        let mut session = crate::auth_store::Session {
            provider: "chatgpt".into(),
            access_token: "fixture-access".into(),
            refresh_token: None,
            account_id: Some("fixture-account".into()),
            expires_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs()
                + 3600,
        };
        vault.save(&session)?;
        crate::provider_catalog::save(
            &vault,
            provider,
            &session.credentials()?,
            vec![RemoteModel {
                id: "fixture-model".into(),
                label: "Fixture model".into(),
                reasoning_levels: vec!["low".into(), "high".into()],
                default_reasoning: Some("low".into()),
            }],
            || false,
        )?;
        let immediate = saved_catalog(&vault, provider, false, || false)?
            .expect("same-account models should be immediately selectable");
        assert_eq!(immediate.models[0].id, "fixture-model");
        assert!(immediate.source.starts_with("Saved Aegis model list"));
        let catalog = saved_selection(&vault, provider, || false, anyhow::anyhow!("offline"))?;
        assert_eq!(catalog.models[0].id, "fixture-model");
        assert!(catalog.source.contains("access may have changed"));
        assert!(saved_selection(&vault, provider, || true, anyhow::anyhow!("cancelled")).is_err());
        session.account_id = Some("different-account".into());
        vault.save(&session)?;
        assert!(saved_selection(&vault, provider, || false, anyhow::anyhow!("offline")).is_err());
        Ok(())
    }

    #[test]
    fn remote_display_labels_have_byte_and_column_bounds_even_for_combining_marks() {
        let label = display_label(&format!("Example{}", "\u{301}".repeat(8192)));
        assert!(label.len() <= 400);
        assert!(std::str::from_utf8(label.as_bytes()).is_ok());
        assert_eq!(display_label("🦊 日本語\u{1b}\u{202e}"), "🦊 日本語");
        assert!(!valid_id("model\u{202e}"));
    }

    #[test]
    fn discovery_errors_are_actionable_without_raw_json_or_false_saved_task_claims() {
        for error in [
            "HTTP 401 fixture-secret",
            "HTTP 403 fixture-secret",
            "HTTP 429 fixture-secret",
            "Provider connection failed",
            "Aegis's private model catalog is invalid",
            "Sign-in changed during discovery",
            "{\"secret\":\"fixture-secret\"}",
        ] {
            let message = discovery_error(error);
            assert!(!message.contains("fixture-secret"));
            assert!(!message.contains('{'));
            assert!(!message.contains("task is saved"));
        }
        assert!(discovery_error("Saved sign-in expired").contains("F4"));
        assert!(discovery_error("HTTP 403").contains("doesn't permit"));
    }

    #[test]
    fn remote_chatgpt_catalog_preserves_advertised_priority_and_reasoning_only() -> Result<()> {
        let models = remote_value(
            crate::direct::Provider::ChatGpt,
            &json!({"models":[
                {"slug":"later","display_name":"Later","visibility":"list","priority":20,"default_reasoning_level":"ultra","supported_reasoning_levels":[{"effort":"low"},{"effort":"low"},{"effort":"new-effort"}],"base_instructions":"do not copy","tools":["shell"],"guardian":{"permissions":"approve"}},
                {"slug":"first","display_name":"First","visibility":"list","priority":1,"default_reasoning_level":"high","supported_reasoning_levels":[{"effort":"low"},{"effort":"high"}]},
                {"slug":"hidden","visibility":"hide","priority":0},
                {"slug":"first","visibility":"list","priority":30},
                {"slug":"bad model","visibility":"list"}
            ]}),
        )?;
        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["first", "later"]
        );
        assert_eq!(models[0].reasoning_levels, ["low", "high"]);
        assert_eq!(models[0].default_reasoning.as_deref(), Some("high"));
        assert_eq!(models[1].reasoning_levels, ["low"]);
        assert_eq!(models[1].default_reasoning, None);
        let serialized = serde_json::to_value(models)?;
        for model in serialized.as_array().unwrap() {
            assert_eq!(model.as_object().unwrap().len(), 4);
        }
        assert!(!serialized.to_string().contains("instructions"));
        assert!(!serialized.to_string().contains("guardian"));
        Ok(())
    }

    #[test]
    fn remote_grok_catalog_uses_inference_ids_and_advertised_reasoning_not_routing() -> Result<()> {
        let models = remote_value(
            crate::direct::Provider::Grok,
            &json!({"data":[
                {"id":"picker-id","model":"inference-id","name":"Example","supportsReasoningEffort":true,"reasoningEfforts":["low",{"value":"high","default":true},{"value":"spoof"}],"baseUrl":"https://untrusted.invalid","apiKey":"never-copy","extraHeaders":{"authorization":"never-copy"}},
                {"id":"meta-model","_meta":{"supportsReasoningEffort":true,"reasoningEfforts":[{"value":"xhigh"}]}},
                {"modelId":"capability-model","supports_reasoning_effort":true,"capabilities":{"reasoning_effort":["low","medium"],"default_reasoning_effort":"medium"}},
                {"id":"no-reasoning","supportsReasoningEffort":false,"reasoningEfforts":["high"]},
                {"id":"hidden","_meta":{"hidden":true}},
                {"id":"inference-id"}
            ]}),
        )?;
        assert_eq!(models.len(), 4);
        assert_eq!(models[0].id, "inference-id");
        assert_eq!(models[0].reasoning_levels, ["low", "high"]);
        assert_eq!(models[0].default_reasoning.as_deref(), Some("high"));
        assert_eq!(models[1].reasoning_levels, ["xhigh"]);
        assert_eq!(models[2].default_reasoning.as_deref(), Some("medium"));
        assert!(models[3].reasoning_levels.is_empty());
        let serialized = serde_json::to_string(&models)?;
        assert!(!serialized.contains("never-copy"));
        assert!(!serialized.contains("untrusted.invalid"));
        assert!(!serialized.contains("picker-id"));
        Ok(())
    }

    #[test]
    fn remote_catalog_limits_and_no_available_models_are_explicit() -> Result<()> {
        for value in [
            json!({}),
            json!({"data":[]}),
            json!({"data":[{"id":"hidden","hidden":true}]}),
        ] {
            assert!(remote_value(crate::direct::Provider::Grok, &value).is_err());
        }
        let entries = (0..4097)
            .map(|index| json!({"id":format!("model-{index}")}))
            .collect::<Vec<_>>();
        assert!(remote_value(crate::direct::Provider::Grok, &json!({"data":entries})).is_err());
        let models = remote_value(
            crate::direct::Provider::Grok,
            &json!({"data":entries[..4096]}),
        )?;
        assert_eq!(models.len(), 256);
        assert_eq!(models[255].id, "model-255");
        Ok(())
    }

    #[test]
    fn reasoning_levels_follow_visible_model_metadata_without_inventing_support() {
        let codex = json!({"models":[{"slug":"example","visibility":"list","supported_reasoning_levels":[{"effort":"low"},{"effort":"high"},{"effort":"high"},{"effort":"spoof"}]},{"slug":"hidden","visibility":"hide","supported_reasoning_levels":[{"effort":"high"}]}]});
        assert_eq!(
            reasoning_from_value("chatgpt", "example", &codex),
            ["low", "high"]
        );
        assert!(reasoning_from_value("codex", "hidden", &codex).is_empty());
        assert!(reasoning_from_value("codex", "unknown", &codex).is_empty());
        let grok = json!({"models":{"example":{"info":{"id":"example","supports_reasoning_effort":true,"reasoning_efforts":[{"value":"xhigh"},{"value":"low"}]}}}});
        assert_eq!(
            reasoning_from_value("grok", "example", &grok),
            ["xhigh", "low"]
        );
        assert!(reasoning_from_value("claude", "sonnet", &grok).is_empty());
    }

    #[test]
    fn codex_catalog_hides_internal_models_and_deduplicates() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("models_cache.json");
        fs::write(
            &path,
            serde_json::to_vec(&json!({"fetched_at":"fixture", "models":[
                {"slug":"visible-model","display_name":"Visible model","visibility":"list"},
                {"slug":"internal-model","visibility":"hide"},
                {"slug":"visible-model","visibility":"list"},
                {"slug":"spoof\u{001b}","visibility":"list"}
            ]}))?,
        )?;
        let catalog = codex_cache(&path)?;
        assert_eq!(catalog.models.len(), 1);
        assert_eq!(catalog.models[0].id, "visible-model");
        assert!(catalog.source.contains("cached"));
        Ok(())
    }

    #[test]
    fn grok_catalog_distinguishes_authenticated_and_fallback_results() {
        let catalog = grok_output(
            "You are not authenticated.\nDefault model: grok-example\nAvailable models:\n  * grok-example (default)\n    grok-fast\n",
        );
        assert_eq!(catalog.models.len(), 2);
        assert!(catalog.source.contains("fallback"));
        assert!(
            !grok_output("Available models:\n * grok-example\n")
                .source
                .contains("fallback")
        );
    }

    #[test]
    fn grok_cache_uses_only_public_display_fields_and_skips_hidden_models() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("models_cache.json");
        fs::write(
            &path,
            serde_json::to_vec(&json!({"fetched_at":"fixture", "models": {
                "grok-example":{"info":{"id":"grok-example","name":"Grok Example","hidden":false}, "api_key":"fixture-private-value"},
                "grok-hidden":{"info":{"id":"grok-hidden","hidden":true}}
            }}))?,
        )?;
        let catalog = grok_cache(&path)?;
        assert_eq!(
            catalog.models,
            vec![Model {
                id: "grok-example".into(),
                label: "Grok Example".into()
            }]
        );
        assert!(catalog.source.contains("cached"));
        assert!(!catalog.source.contains("fixture-private-value"));
        Ok(())
    }

    #[test]
    fn endpoint_catalog_is_bounded_sorted_and_deduplicated() -> Result<()> {
        let models =
            endpoint_value(&json!({"data":[{"id":"z"},{"id":"a"},{"id":"a"},{"id":"bad model"}]}))?;
        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "z"]
        );
        assert!(endpoint_value(&json!({"error":"unavailable"})).is_err());
        assert!(!valid_id(""));
        Ok(())
    }
}
