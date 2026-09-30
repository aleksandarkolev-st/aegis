use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

pub const DEFAULT_MODEL_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;

pub fn validate_response_bytes(bytes: u64) -> Result<()> {
    if !(1024..=32 * 1024 * 1024).contains(&bytes) {
        bail!("model response capture limit must be 1024..33554432 bytes");
    }
    Ok(())
}

pub fn response_bytes(configuration: &serde_json::Value) -> Result<u64> {
    let bytes = match configuration.get("model_response_bytes") {
        Some(value) => value.as_u64().ok_or_else(|| {
            anyhow::anyhow!("model response capture limit must be an unsigned byte count")
        })?,
        None => DEFAULT_MODEL_RESPONSE_BYTES,
    };
    validate_response_bytes(bytes)?;
    Ok(bytes)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    #[serde(skip_serializing)]
    pub actions: u64,
    #[serde(skip_serializing)]
    pub model_tokens: u64,
    #[serde(skip_serializing)]
    pub tool_result_tokens: u64,
    pub wall_seconds: u64,
    pub model_seconds: u64,
    pub process_seconds: u64,
    pub context_chars: u64,
    pub model_response_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            actions: 0,
            model_tokens: 0,
            tool_result_tokens: 0,
            wall_seconds: 14_400,
            model_seconds: 180,
            process_seconds: 600,
            context_chars: 256_000,
            model_response_bytes: DEFAULT_MODEL_RESPONSE_BYTES,
        }
    }
}

impl Limits {
    pub fn quick() -> Self {
        Self {
            wall_seconds: 3600,
            process_seconds: 60,
            ..Self::default()
        }
    }

    pub fn validate(&self) -> Result<()> {
        validate_response_bytes(self.model_response_bytes)?;
        if !(1..=86_400).contains(&self.wall_seconds) {
            bail!("task duration must be 1..86400 seconds");
        }
        if !(1..=1800).contains(&self.model_seconds) {
            bail!("model-turn deadline must be 1..1800 seconds");
        }
        if !(1..=7200).contains(&self.process_seconds) {
            bail!("command deadline must be 1..7200 seconds");
        }
        if !(1..=4_000_000).contains(&self.context_chars) {
            bail!("context limit must be 1..4000000 characters");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_limits_default_for_legacy_contracts_and_reject_invalid_values() -> Result<()> {
        assert_eq!(
            response_bytes(&serde_json::json!({}))?,
            DEFAULT_MODEL_RESPONSE_BYTES
        );
        assert_eq!(
            response_bytes(&serde_json::json!({"model_response_bytes":1024}))?,
            1024
        );
        for value in [
            serde_json::json!(0),
            serde_json::json!(1023),
            serde_json::json!(33554433),
            serde_json::json!("1024"),
            serde_json::json!(null),
        ] {
            assert!(response_bytes(&serde_json::json!({"model_response_bytes":value})).is_err());
        }
        Ok(())
    }

    #[test]
    fn default_and_partial_saved_limits_support_bounded_multi_hour_work() -> Result<()> {
        let limits: Limits = serde_json::from_str("{}")?;
        limits.validate()?;
        assert_eq!(limits.wall_seconds, 14_400);
        assert_eq!(limits.process_seconds, 600);
        assert_eq!(Limits::quick().wall_seconds, 3600);
        let limits: Limits = serde_json::from_str(r#"{"process_seconds":7200}"#)?;
        limits.validate()?;
        Ok(())
    }

    #[test]
    fn custom_limits_are_not_unbounded_or_silently_misspelled() -> Result<()> {
        assert!(serde_json::from_str::<Limits>(r#"{"command_seconds":60}"#).is_err());
        for limits in [
            Limits {
                wall_seconds: 0,
                ..Limits::default()
            },
            Limits {
                process_seconds: 7201,
                ..Limits::default()
            },
        ] {
            assert!(limits.validate().is_err());
        }
        Ok(())
    }

    #[test]
    fn legacy_task_caps_load_but_are_not_written_to_new_profiles() -> Result<()> {
        let legacy: Limits = serde_json::from_value(serde_json::json!({
            "actions": 99,
            "model_tokens": 123456,
            "tool_result_tokens": 800000,
            "wall_seconds": 10800,
        }))?;
        assert_eq!(legacy.actions, 99);
        assert_eq!(legacy.model_tokens, 123456);
        assert_eq!(legacy.tool_result_tokens, 800000);
        let saved = serde_json::to_value(legacy)?;
        for removed in ["actions", "model_tokens", "tool_result_tokens"] {
            assert!(saved.get(removed).is_none(), "{removed} persisted");
        }
        assert_eq!(saved["wall_seconds"], 10800);
        Ok(())
    }
}
