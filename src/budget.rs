use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub actions: u64,
    pub model_tokens: u64,
    pub wall_seconds: u64,
    pub model_seconds: u64,
    pub process_seconds: u64,
    pub context_chars: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            actions: 200,
            model_tokens: 800_000,
            wall_seconds: 14_400,
            model_seconds: 180,
            process_seconds: 600,
            context_chars: 256_000,
        }
    }
}

impl Limits {
    pub fn quick() -> Self {
        Self {
            actions: 80,
            wall_seconds: 3600,
            process_seconds: 60,
            ..Self::default()
        }
    }

    pub fn validate(&self) -> Result<()> {
        if !(1..=1000).contains(&self.actions) {
            bail!("action limit must be 1..1000");
        }
        if !(1..=100_000_000).contains(&self.model_tokens) {
            bail!("model token limit must be 1..100000000");
        }
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
    fn default_and_partial_saved_limits_support_bounded_multi_hour_work() -> Result<()> {
        let limits: Limits = serde_json::from_str("{}")?;
        limits.validate()?;
        assert_eq!(limits.wall_seconds, 14_400);
        assert_eq!(limits.process_seconds, 600);
        assert_eq!(Limits::quick().wall_seconds, 3600);
        let limits: Limits = serde_json::from_str(r#"{"process_seconds":7200}"#)?;
        limits.validate()?;
        assert_eq!(limits.actions, 200);
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
                actions: 1001,
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
}
