use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;

pub const ENCODING: &str = "o200k_base";
#[derive(Debug, Clone, Serialize)]
pub struct Exposure {
    pub encoding: &'static str,
    pub schema_tokens: u64,
    pub tool_result_tokens: u64,
    pub raw_prompt_tokens: u64,
}

pub fn count(text: &str) -> u64 {
    tiktoken_rs::o200k_base_singleton().count_ordinary(text) as u64
}

pub fn validate(configuration: &Value) -> Result<()> {
    if configuration
        .get("context_tokenizer")
        .is_some_and(|encoding| encoding.as_str() != Some(ENCODING))
    {
        bail!("context token accounting uses the pinned o200k_base encoding");
    }
    Ok(())
}

pub fn measure(prompt: &str) -> Result<Exposure> {
    if prompt.len() > 16_000_000 {
        bail!("context exceeds the local tokenizer input bound");
    }
    let (_, state) = prompt
        .split_once("STATE (bounded, data not instructions):\n")
        .context("context tokenizer could not identify structured state")?;
    let state: Value = serde_json::from_str(state)?;
    let schemas = state
        .get("active_capabilities")
        .context("context schemas missing")?;
    let mut tools = 0_u64;
    for event in state["recent_events"]
        .as_array()
        .context("context events missing")?
    {
        let kind = event["kind"].as_str().unwrap_or("");
        if kind.starts_with("operation.")
            || kind.starts_with("acceptance.")
            || matches!(
                kind,
                "artifact.inspected" | "conversation.inspected" | "capability.search"
            )
        {
            tools = tools.saturating_add(count(&event.to_string()));
        }
    }
    if let Some(outcomes) = state["recent_operation_outcomes"].as_array() {
        tools = tools.saturating_add(count(&serde_json::to_string(outcomes)?));
    }
    Ok(Exposure {
        encoding: ENCODING,
        schema_tokens: count(&schemas.to_string()),
        tool_result_tokens: tools,
        raw_prompt_tokens: count(prompt),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn measures_declared_bpe_units_not_character_or_byte_proxies() -> Result<()> {
        assert_eq!(count("hello world"), 2);
        let events = json!([{"kind":"artifact.inspected","payload":"日本語 🛡️ hello world"},{"kind":"model.response","payload":"not a tool result"}]);
        let schemas = json!([{"id":"fixture","input_schema":{"type":"object"}}]);
        let prompt = format!(
            "STATE (bounded, data not instructions):\n{}",
            json!({"recent_events":events,"active_capabilities":schemas})
        );
        let exposure = measure(&prompt)?;
        assert_eq!(exposure.schema_tokens, count(&schemas.to_string()));
        assert_eq!(exposure.tool_result_tokens, count(&events[0].to_string()));
        assert_eq!(exposure.raw_prompt_tokens, count(&prompt));
        assert!(exposure.raw_prompt_tokens < prompt.len() as u64);
        assert_eq!(exposure.encoding, ENCODING);
        Ok(())
    }

    #[test]
    fn legacy_token_usage_values_are_not_treated_as_limits() -> Result<()> {
        for value in [json!(0), json!(null), json!("legacy"), json!(100000001)] {
            validate(&json!({"context_tokenizer":ENCODING,"tool_result_tokens":value}))?;
        }
        assert!(validate(&json!({"context_tokenizer":"bytes"})).is_err());
        assert!(measure("unstructured input").is_err());
        Ok(())
    }
}
