use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    SearchCapabilities {
        query: String,
    },
    Invoke {
        capability: String,
        args: Value,
    },
    InspectResult {
        artifact: String,
        query: String,
    },
    Checkpoint {
        checkpoint: Checkpoint,
    },
    Finish {
        summary: String,
        evidence: Vec<String>,
    },
    Blocked {
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Milestone {
    pub title: String,
    pub state: String,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Checkpoint {
    pub decisions: Vec<String>,
    pub unresolved: Vec<String>,
    pub next_action: String,
    pub milestones: Vec<Milestone>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub source: String,
}

#[derive(Debug)]
pub struct Response {
    pub action: Action,
    pub raw: String,
    pub usage: Option<Usage>,
}

const SCHEMA: &str = r#"{"type":"object","properties":{"kind":{"type":"string","enum":["search_capabilities","invoke","inspect_result","checkpoint","finish","blocked"]},"query":{"type":"string"},"capability":{"type":"string"},"args":{"type":"string"},"artifact":{"type":"string"},"checkpoint":{"type":"string"},"summary":{"type":"string"},"evidence":{"type":"array","items":{"type":"string"}},"reason":{"type":"string"}},"required":["kind","query","capability","args","artifact","checkpoint","summary","evidence","reason"],"additionalProperties":false}"#;

pub(crate) fn schema() -> Result<Value> {
    Ok(serde_json::from_str(SCHEMA)?)
}

pub(crate) fn parse_action(raw: &str) -> Result<Action> {
    let trimmed = raw.trim();
    let content = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed)
        .trim();
    let trimmed = content.strip_suffix("```").unwrap_or(content).trim();
    let mut value: Value = serde_json::from_str(trimmed).context("model response was not JSON")?;
    if value.get("kind").is_some() {
        let encoded = match value.get("kind").and_then(Value::as_str) {
            Some("invoke") => Some("args"),
            Some("checkpoint") => Some("checkpoint"),
            _ => None,
        };
        if let Some(key) = encoded {
            if let Some(contents) = value.get(key).and_then(Value::as_str) {
                value[key] = serde_json::from_str(contents)
                    .with_context(|| format!("invalid {key} JSON"))?;
            }
        }
        return serde_json::from_value(value).context("invalid model action");
    }
    if let Some(inner) = value
        .get("structured_output")
        .filter(|inner| inner.is_object())
    {
        return parse_action(&inner.to_string());
    }
    for key in ["result", "response", "content", "text"] {
        if let Some(inner) = value.get(key).and_then(Value::as_str) {
            return parse_action(inner);
        }
    }
    if let Some(inner) = value.pointer("/message/content").and_then(Value::as_str) {
        return parse_action(inner);
    }
    bail!("model response did not contain an action")
}

pub fn call(
    provider: &str,
    prompt: &str,
    directory: &Path,
    timeout: Duration,
) -> Result<(Action, String)> {
    let response = call_with_cancel(provider, prompt, directory, timeout, || false)?;
    Ok((response.action, response.raw))
}

pub fn login(provider: &str) -> Result<()> {
    crate::signin::run(provider, &crate::terminal::Terminal::default())?;
    Ok(())
}

pub fn call_with_cancel(
    provider: &str,
    prompt: &str,
    directory: &Path,
    timeout: Duration,
    cancelled: impl Fn() -> bool,
) -> Result<Response> {
    crate::direct::provider(provider)?;
    let started = std::time::Instant::now();
    let interrupted = || cancelled() || started.elapsed() >= timeout;
    let model = crate::catalog::available(provider, interrupted)?
        .models
        .into_iter()
        .next()
        .context("Choose a model explicitly before making a direct request")?
        .id;
    call_configured(
        provider,
        &serde_json::json!({"provider_transport":"aegis-direct-v1", "model":model}),
        prompt,
        directory,
        timeout.saturating_sub(started.elapsed()),
        interrupted,
    )
}

pub fn call_configured(
    provider: &str,
    configuration: &Value,
    prompt: &str,
    _directory: &Path,
    timeout: Duration,
    cancelled: impl Fn() -> bool,
) -> Result<Response> {
    let provider = crate::provider::canonical(provider);
    let response_bytes = crate::budget::response_bytes(configuration)?;
    let reasoning = configuration
        .get("reasoning_effort")
        .filter(|value| !value.is_null())
        .map(|value| {
            let effort = value
                .as_str()
                .context("reasoning effort must be a string")?;
            if !crate::catalog::valid_effort(effort) {
                bail!("invalid reasoning effort");
            }
            Ok(effort)
        })
        .transpose()?;
    if provider == "custom" {
        let endpoint: crate::endpoint::Endpoint = serde_json::from_value(
            configuration
                .get("endpoint")
                .context("custom endpoint configuration missing")?
                .clone(),
        )?;
        let model = configuration
            .get("model")
            .and_then(Value::as_str)
            .context("custom endpoints require a model ID")?;
        return endpoint.call_reasoned(
            model,
            prompt,
            timeout,
            cancelled,
            response_bytes,
            reasoning,
        );
    }
    let provider = crate::direct::provider(provider)?;
    if configuration["provider_transport"] != "aegis-direct-v1" {
        bail!(
            "This saved task predates direct providers; start a new task to review its provider and permissions. No native CLI was started"
        );
    }
    let model = configuration["model"]
        .as_str()
        .filter(|model| crate::catalog::valid_id(model))
        .context("Choose a model with F6 before starting this task")?;
    if timeout.is_zero() || cancelled() {
        bail!("Model request interrupted before dispatch");
    }
    let started = Instant::now();
    let interrupted = || cancelled() || started.elapsed() >= timeout;
    let credentials = crate::oauth::AuthClient::new(provider)?
        .credentials(&crate::auth_store::Vault::user()?, &interrupted)?;
    let remaining = timeout.saturating_sub(started.elapsed());
    if remaining.is_zero() {
        bail!("Model request deadline elapsed during sign-in refresh");
    }
    crate::direct::call(
        provider,
        &credentials,
        &crate::direct::Request {
            model,
            prompt,
            reasoning,
            timeout: remaining,
            response_bytes,
        },
        interrupted,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_execution_and_implicit_legacy_transport_are_not_available() -> Result<()> {
        for provider in ["claude", "claude-code", "unknown"] {
            assert!(
                call_configured(
                    provider,
                    &Value::Null,
                    "hello",
                    Path::new("."),
                    Duration::from_secs(1),
                    || false
                )
                .is_err()
            );
        }
        let error = call_configured(
            "codex",
            &Value::Null,
            "hello",
            Path::new("."),
            Duration::from_secs(1),
            || false,
        )
        .err()
        .unwrap()
        .to_string();
        assert!(error.contains("predates direct providers"));
        let error = call_configured(
            "grok",
            &serde_json::json!({"provider_transport":"aegis-direct-v1"}),
            "hello",
            Path::new("."),
            Duration::from_secs(1),
            || false,
        )
        .err()
        .unwrap()
        .to_string();
        assert!(error.contains("Choose a model"));
        Ok(())
    }

    #[test]
    fn parses_direct_and_wrapped_actions() -> Result<()> {
        let action = Action::SearchCapabilities {
            query: "file".into(),
        };
        let raw = serde_json::to_string(&action)?;
        assert_eq!(parse_action(&raw)?, action);
        assert_eq!(
            parse_action(&serde_json::json!({"result": raw}).to_string())?,
            action
        );
        assert_eq!(
            parse_action(&serde_json::json!({"text":raw}).to_string())?,
            action
        );
        assert_eq!(
            parse_action(&serde_json::json!({"structured_output":action}).to_string())?,
            action
        );
        assert_eq!(
            parse_action(&format!(
                "```json\n{}\n```",
                serde_json::to_string(&action)?
            ))?,
            action
        );
        Ok(())
    }
}
