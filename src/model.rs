use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    AskUser {
        query: String,
    },
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
    Remember {
        summary: String,
        artifact: String,
    },
    VerifyObligations {
        obligations: Vec<crate::obligations::Proof>,
    },
    Finish {
        summary: String,
        evidence: Vec<String>,
        #[serde(default)]
        obligations: Vec<crate::obligations::Proof>,
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
    #[serde(default)]
    pub cached_input_reported: bool,
    pub source: String,
}

#[derive(Debug)]
pub struct Response {
    pub action: Action,
    pub raw: String,
    pub usage: Option<Usage>,
}

const SCHEMA: &str = r#"{"type":"object","properties":{"kind":{"type":"string","enum":["ask_user","search_capabilities","invoke","inspect_result","checkpoint","verify_obligations","finish","blocked"]},"query":{"type":"string"},"capability":{"type":"string"},"args":{"type":"string"},"artifact":{"type":"string"},"checkpoint":{"type":"string"},"summary":{"type":"string"},"evidence":{"type":"array","items":{"type":"string"}},"obligations":{"type":"array","items":{"type":"object","properties":{"id":{"type":"integer"},"evidence":{"type":"array","items":{"type":"string"}}},"required":["id","evidence"],"additionalProperties":false}},"reason":{"type":"string"}},"required":["kind","query","capability","args","artifact","checkpoint","summary","evidence","obligations","reason"],"additionalProperties":false}"#;

pub(crate) fn schema() -> Result<Value> {
    let mut schema: Value = serde_json::from_str(SCHEMA)?;
    schema["properties"]["kind"]["enum"].as_array_mut().context("action kinds")?.push(serde_json::json!("remember"));
    schema["properties"]["args"]["description"] = serde_json::json!(
        "JSON object string; empty metadata is \"{}\". Omit fields supplied by args_text or args_edits."
    );
    schema["properties"]["artifact"]["description"] = serde_json::json!(
        "inspect_result: supplied handle, never file sha256. remember: user:<owner_batch_through>."
    );
    schema["properties"]["args_text_field"] = serde_json::json!({"type":"string","enum":["","content","script"],"description":"Argument supplied by args_text; otherwise empty."});
    schema["properties"]["args_text"] = serde_json::json!({"type":"string","description":"Selected content/script; escape once, omit from args."});
    schema["properties"]["args_edits"] = serde_json::json!({"type":"array","maxItems":32,"items":{"type":"object","additionalProperties":false,"properties":{"old":{"type":"string","minLength":1},"new":{"type":"string"}},"required":["old","new"]},"description":"workspace.patch replacements; escape once, omit edits from args. Otherwise empty."});
    let required = schema["required"]
        .as_array_mut()
        .context("action schema required fields")?;
    required.push(serde_json::json!("args_text_field"));
    required.push(serde_json::json!("args_text"));
    required.push(serde_json::json!("args_edits"));
    Ok(schema)
}

/// Only provider request schemas need decision-first property order. Persisted
/// values keep serde_json's existing canonical map order and contract hashes.
pub(crate) fn request_body(body: &Value) -> impl Serialize + '_ {
    WireValue {
        value: body,
        scope: SchemaScope::Root,
    }
}

#[derive(Clone, Copy)]
enum SchemaScope {
    Root,
    Text,
    Format,
    ResponseFormat,
    JsonSchema,
    Schema,
    Properties,
    Other,
}

impl SchemaScope {
    fn child(self, key: &str) -> Self {
        match (self, key) {
            (Self::Root, "text") => Self::Text,
            (Self::Text, "format") => Self::Format,
            (Self::Format, "schema") => Self::Schema,
            (Self::Root, "response_format") => Self::ResponseFormat,
            (Self::ResponseFormat, "json_schema") => Self::JsonSchema,
            (Self::JsonSchema, "schema") => Self::Schema,
            (Self::Schema, "properties") => Self::Properties,
            _ => Self::Other,
        }
    }
}

struct WireValue<'a> {
    value: &'a Value,
    scope: SchemaScope,
}

impl Serialize for WireValue<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{SerializeMap, SerializeSeq};
        const ORDER: &[&str] = &[
            "kind",
            "capability",
            "args_text_field",
            "args_text",
            "args_edits",
            "args",
            "query",
            "artifact",
            "checkpoint",
            "summary",
            "evidence",
            "obligations",
            "reason",
        ];
        match self.value {
            Value::Object(fields) => {
                let mut output = serializer.serialize_map(Some(fields.len()))?;
                if matches!(self.scope, SchemaScope::Properties) {
                    for key in ORDER {
                        if let Some(value) = fields.get(*key) {
                            output.serialize_entry(key, value)?;
                        }
                    }
                }
                for (key, value) in fields {
                    if matches!(self.scope, SchemaScope::Properties)
                        && ORDER.contains(&key.as_str())
                    {
                        continue;
                    }
                    output.serialize_entry(
                        key,
                        &WireValue {
                            value,
                            scope: self.scope.child(key),
                        },
                    )?;
                }
                output.end()
            }
            Value::Array(values) => {
                let mut output = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    output.serialize_element(&WireValue {
                        value,
                        scope: SchemaScope::Other,
                    })?;
                }
                output.end()
            }
            _ => self.value.serialize(serializer),
        }
    }
}

pub(crate) fn parse_action(raw: &str) -> Result<Action> {
    let trimmed = raw.trim();
    let content = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed)
        .trim();
    let trimmed = content.strip_suffix("```").unwrap_or(content).trim();
    let mut value: Value = match serde_json::from_str(trimmed) {
        Ok(value) => value,
        Err(error) => {
            // Some transports emit the same action twice, with reordered object keys.
            // Accept that exact semantic duplicate once; never select among different actions.
            let mut values = serde_json::Deserializer::from_str(trimmed).into_iter::<Value>();
            let first = values.next().transpose()?;
            let second = values.next().transpose()?;
            if let (Some(first), Some(second), None) = (first, second, values.next()) {
                let first = parse_action(&first.to_string())?;
                let second = parse_action(&second.to_string())?;
                if first == second {
                    return Ok(first);
                }
            }
            return Err(error).context("model response was not one unambiguous JSON action");
        }
    };
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
        // Large code/script strings need only the outer JSON escaping. The resulting
        // argument object still passes the ordinary capability/grant/schema checks.
        let text_field = value
            .get("args_text_field")
            .map(|field| field.as_str().context("args_text_field must be a string"))
            .transpose()?
            .unwrap_or("")
            .to_owned();
        let text = value
            .get("args_text")
            .map(|text| text.as_str().context("args_text must be a string"))
            .transpose()?
            .unwrap_or("")
            .to_owned();
        if text_field.is_empty() {
            if !text.is_empty() {
                bail!("args_text requires a selected field");
            }
        } else {
            if value["kind"] != "invoke" || !matches!(text_field.as_str(), "content" | "script") {
                bail!("separated argument text requires invoke and content or script");
            }
            let arguments = value["args"]
                .as_object_mut()
                .context("separated text requires object args")?;
            if arguments.contains_key(&text_field) {
                bail!("conflicting argument text fields");
            }
            arguments.insert(text_field.clone(), Value::String(text));
        }
        if let Some(edits) = value.get("args_edits") {
            let edits = edits.as_array().context("args_edits must be an array")?;
            if !edits.is_empty() {
                if value["kind"] != "invoke"
                    || value["capability"] != "workspace.patch"
                    || !text_field.is_empty()
                {
                    bail!("separated edits require workspace.patch without args_text");
                }
                if edits.len() > 32
                    || edits.iter().any(|edit| {
                        edit.as_object().is_none_or(|fields| fields.len() != 2)
                            || edit["old"].as_str().is_none_or(str::is_empty)
                            || !edit["new"].is_string()
                    })
                {
                    bail!("separated edits require one to 32 exact old/new string pairs");
                }
                let edits = Value::Array(edits.clone());
                let arguments = value["args"]
                    .as_object_mut()
                    .context("separated edits require object args")?;
                if arguments.contains_key("edits") {
                    bail!("conflicting argument edits fields");
                }
                arguments.insert("edits".into(), edits);
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
    crate::signin::command(provider, &crate::terminal::Terminal::default())
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
    call_configured_with_images(
        provider,
        configuration,
        prompt,
        _directory,
        timeout,
        &[],
        cancelled,
    )
}

pub(crate) fn call_configured_with_images(
    provider: &str,
    configuration: &Value,
    prompt: &str,
    _directory: &Path,
    timeout: Duration,
    images: &[crate::image::InputImage],
    cancelled: impl Fn() -> bool,
) -> Result<Response> {
    call_configured_with_progress(
        provider,
        configuration,
        prompt,
        _directory,
        timeout,
        images,
        cancelled,
        &|_, _| Ok(()),
    )
}

pub(crate) fn call_configured_with_progress(
    provider: &str,
    configuration: &Value,
    prompt: &str,
    _directory: &Path,
    timeout: Duration,
    images: &[crate::image::InputImage],
    cancelled: impl Fn() -> bool,
    progress: &dyn Fn(&str, &str) -> Result<()>,
) -> Result<Response> {
    let provider = crate::provider::canonical(provider);
    if !images.is_empty() && provider != "codex" {
        bail!(
            "Visual inputs are currently supported only by direct ChatGPT routes; this task uses {provider}"
        );
    }
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
    if provider == "claude-api" {
        if configuration["provider_transport"] != "aegis-claude-api-v1" {
            bail!(
                "Claude API tasks require an explicit API-key route; Claude Code subscription login is separate and pending"
            );
        }
        let model = configuration["model"]
            .as_str()
            .filter(|model| crate::catalog::valid_id(model))
            .context("Claude API tasks require a selected model")?;
        let reference = configuration["api_key_env"]
            .as_str()
            .filter(|reference| crate::routing::valid_key_reference(reference))
            .context("Claude API tasks require a valid session-only API-key reference")?;
        let key = std::env::var(reference).with_context(|| {
            format!("Claude API key for {reference} is missing; enter it again")
        })?;
        let output_tokens = configuration
            .get("output_tokens")
            .map(|value| {
                value
                    .as_u64()
                    .context("Claude API output_tokens must be a count")
            })
            .transpose()?
            .unwrap_or(4096);
        return crate::claude_api::call(
            model,
            prompt,
            &key,
            output_tokens,
            response_bytes,
            reasoning,
            timeout,
            cancelled,
        );
    }
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
    crate::direct::call_with_progress(
        provider,
        &credentials,
        &crate::direct::Request {
            model,
            prompt,
            reasoning,
            timeout: remaining,
            response_bytes,
            images,
        },
        interrupted,
        progress,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_schema_decides_action_and_text_before_arguments_without_changing_persisted_values()
    -> Result<()> {
        for mut body in [
            serde_json::json!({"text":{"format":{"schema":schema()?}}}),
            serde_json::json!({"response_format":{"json_schema":{"schema":schema()?}}}),
        ] {
            body["unrelated"] = serde_json::json!({"properties":{"z":2,"a":1},"prompt":"quoted \"schema\" Київ\\path"});
            let canonical = serde_json::to_vec(&body)?;
            let wire = serde_json::to_string(&request_body(&body))?;
            assert_eq!(serde_json::from_str::<Value>(&wire)?, body);
            assert_eq!(serde_json::to_vec(&body)?, canonical);
            assert!(wire.contains("\"properties\":{\"kind\":"));
            let kind = wire.find("\"kind\":{").unwrap();
            let capability = wire.find("\"capability\":{").unwrap();
            let selector = wire.find("\"args_text_field\":{").unwrap();
            let text = wire.find("\"args_text\":{").unwrap();
            let args = wire.find("\"args\":{").unwrap();
            assert!(kind < capability && capability < selector && selector < text && text < args);
            assert!(wire.contains("\"properties\":{\"a\":1,\"z\":2}"));
        }
        Ok(())
    }

    #[test]
    fn separated_code_text_round_trips_without_a_second_json_escape_layer() -> Result<()> {
        let text =
            "const path = 'C:\\windows\\a';\nconst json = {\"quote\": '\"', unicode: 'Київ 🦀'};\n"
                .repeat(1000);
        for (capability, field, arguments) in [
            (
                "workspace.write",
                "content",
                serde_json::json!({"path":"code.mjs"}),
            ),
            (
                "mcp.windows-host.powershell",
                "script",
                serde_json::json!({"timeout_seconds":30}),
            ),
        ] {
            let raw = serde_json::json!({"kind":"invoke","capability":capability,"args":arguments.to_string(),"args_text_field":field,"args_text":text});
            let mut expected = arguments;
            expected[field] = Value::String(text.clone());
            assert_eq!(
                parse_action(&raw.to_string())?,
                Action::Invoke {
                    capability: capability.into(),
                    args: expected
                }
            );
        }
        for invalid in [
            serde_json::json!({"kind":"invoke","capability":"workspace.write","args":"{}","args_text_field":"path","args_text":"outside"}),
            serde_json::json!({"kind":"invoke","capability":"workspace.write","args":"{\"content\":\"first\"}","args_text_field":"content","args_text":"second"}),
            serde_json::json!({"kind":"invoke","capability":"workspace.write","args":"{}","args_text_field":"","args_text":"orphan"}),
            serde_json::json!({"kind":"finish","summary":"done","evidence":[],"args_text_field":"script","args_text":"unexpected"}),
        ] {
            assert!(parse_action(&invalid.to_string()).is_err());
        }
        assert_eq!(schema()?["additionalProperties"], false);
        Ok(())
    }

    #[test]
    fn separated_patch_edits_preserve_code_and_reject_ambiguous_inputs() -> Result<()> {
        let old = "export function encodeRow(fields) { return fields.join(','); }\n";
        let new = "export function encodeRow(fields) {\n  return fields.map(s => /[,\"\\r\\n]/.test(s) ? '\"' + s.replaceAll('\"', '\"\"') + '\"' : s).join(',');\n}\n";
        let edits = serde_json::json!([{ "old":old,"new":new }]);
        let raw = serde_json::json!({"kind":"invoke","capability":"workspace.patch","args":"{\"path\":\"csv.mjs\"}","args_edits":edits});
        assert_eq!(
            parse_action(&raw.to_string())?,
            Action::Invoke {
                capability: "workspace.patch".into(),
                args: serde_json::json!({"path":"csv.mjs","edits":edits})
            }
        );
        for changes in [
            serde_json::json!({"capability":"workspace.write"}),
            serde_json::json!({"args":"{\"path\":\"csv.mjs\",\"edits\":[]}"}),
            serde_json::json!({"args":[]}),
            serde_json::json!({"args_edits":{}}),
            serde_json::json!({"args_edits":[{"old":"","new":"code"}]}),
            serde_json::json!({"args_edits":[{"old":"code","new":1}]}),
            serde_json::json!({"args_edits":[{"old":"code","new":"","extra":true}]}),
            serde_json::json!({"args_edits":vec![serde_json::json!({"old":"a","new":"b"});33]}),
            serde_json::json!({"args_text_field":"script","args_text":"code"}),
        ] {
            let mut invalid = raw.clone();
            for (key, value) in changes.as_object().unwrap() {
                invalid[key] = value.clone();
            }
            assert!(parse_action(&invalid.to_string()).is_err(), "{changes}");
        }
        // The additive wire format does not change persisted Action or legacy object args.
        let legacy = serde_json::json!({"kind":"invoke","capability":"workspace.patch","args":{"path":"csv.mjs","edits":edits}});
        assert_eq!(
            parse_action(&legacy.to_string())?,
            parse_action(&raw.to_string())?
        );
        let mut empty = legacy.clone();
        empty["args_edits"] = serde_json::json!([]);
        assert_eq!(
            parse_action(&empty.to_string())?,
            parse_action(&raw.to_string())?
        );
        assert_eq!(
            schema()?["properties"]["args_edits"]["items"]["additionalProperties"],
            false
        );
        Ok(())
    }

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
    fn visual_inputs_fail_clearly_on_non_chatgpt_routes() -> Result<()> {
        let image = crate::image::InputImage {
            mime_type: "image/png".into(),
            bytes: b"\x89PNG\r\n\x1a\nfixture".to_vec(),
            artifact_hash: "a".repeat(64),
            content_index: 0,
        };
        for provider in ["grok", "claude-api", "custom"] {
            let error = call_configured_with_images(
                provider,
                &Value::Null,
                "analyze this image",
                Path::new("."),
                Duration::from_secs(1),
                std::slice::from_ref(&image),
                || false,
            )
            .err()
            .context("visual input unexpectedly accepted")?
            .to_string();
            assert!(
                error.contains("only by direct ChatGPT routes"),
                "{provider}: {error}"
            );
        }
        Ok(())
    }

    #[test]
    fn claude_api_never_reuses_claude_code_subscription_authentication() -> Result<()> {
        let error = call_configured(
            "claude-api",
            &serde_json::json!({"model":"account-model"}),
            "hello",
            Path::new("."),
            Duration::from_secs(1),
            || false,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("explicit API-key route"));
        let error = call_configured(
            "claude-api",
            &serde_json::json!({"provider_transport":"aegis-claude-api-v1","model":"account-model","api_key_env":"BAD-NAME"}),
            "hello",
            Path::new("."),
            Duration::from_secs(1),
            || false,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("API-key reference"));
        Ok(())
    }

    #[test]
    fn identical_duplicate_actions_apply_once_and_distinct_or_extra_actions_are_rejected()
    -> Result<()> {
        let first = r#"{"kind":"invoke","capability":"workspace.write","args":"{\"path\":\"a.txt\",\"content\":\"hello\"}"}"#;
        let reordered = r#"{"args":{"content":"hello","path":"a.txt"},"capability":"workspace.write","kind":"invoke"}"#;
        assert_eq!(
            parse_action(&format!("{first}\n{reordered}"))?,
            parse_action(first)?
        );
        assert!(
            parse_action(&format!(
                "{first}{}",
                reordered.replace("hello", "different")
            ))
            .is_err()
        );
        assert!(parse_action(&format!("{first}{reordered}{first}")).is_err());
        assert!(parse_action(&format!("I will do this: {first}")).is_err());
        assert!(parse_action(&format!("{first}{reordered} trailing text")).is_err());
        assert!(parse_action(&format!("{first}{{")).is_err());
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
