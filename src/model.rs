use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
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
    Finish {
        summary: String,
        evidence: Vec<String>,
    },
    Blocked {
        reason: String,
    },
}

const SCHEMA: &str = r#"{"type":"object","properties":{"kind":{"type":"string","enum":["search_capabilities","invoke","inspect_result","finish","blocked"]},"query":{"type":"string"},"capability":{"type":"string"},"args":{"type":"object"},"artifact":{"type":"string"},"summary":{"type":"string"},"evidence":{"type":"array","items":{"type":"string"}},"reason":{"type":"string"}},"required":["kind"],"additionalProperties":false}"#;

fn parse_action(raw: &str) -> Result<Action> {
    let trimmed = raw.trim();
    let trimmed = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed)
        .trim()
        .strip_suffix("```")
        .unwrap_or(trimmed)
        .trim();
    let value: Value = serde_json::from_str(trimmed).context("model response was not JSON")?;
    if value.get("kind").is_some() {
        return serde_json::from_value(value).context("invalid model action");
    }
    for key in ["result", "response", "content"] {
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
    let output = tempfile::tempdir_in(directory)?;
    let stdout_path = output.path().join("stdout");
    let stderr_path = output.path().join("stderr");
    let mut command = match provider {
        "codex" => {
            let schema_path = output.path().join("schema.json");
            std::fs::write(&schema_path, SCHEMA)?;
            let mut command = Command::new(if cfg!(windows) { "codex.cmd" } else { "codex" });
            command
                .args([
                    "exec",
                    "--ephemeral",
                    "--ignore-user-config",
                    "--ignore-rules",
                    "--sandbox",
                    "read-only",
                    "--skip-git-repo-check",
                    "--output-schema",
                ])
                .arg(&schema_path)
                .args(["-o"])
                .arg(output.path().join("reply"))
                .arg(prompt);
            command
        }
        "claude" => {
            let mut command = Command::new("claude");
            command.args([
                "-p",
                "--tools",
                "",
                "--strict-mcp-config",
                "--setting-sources",
                "",
                "--output-format",
                "json",
                "--json-schema",
                SCHEMA,
                prompt,
            ]);
            command
        }
        "grok" => {
            let mut command = Command::new("grok");
            command.args([
                "--no-subagents",
                "--tools",
                "",
                "--verbatim",
                "--json-schema",
                SCHEMA,
                "--single",
                prompt,
            ]);
            command
        }
        other => bail!("unsupported provider: {other}"),
    };
    command
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::from(File::create(&stdout_path)?))
        .stderr(Stdio::from(File::create(&stderr_path)?));
    let mut child = command
        .spawn()
        .with_context(|| format!("start {provider}; install and log in to its CLI first"))?;
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() > timeout {
            child.kill()?;
            child.wait()?;
            bail!(
                "{provider} model call exceeded {} seconds",
                timeout.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(100));
    };
    let mut stdout = String::new();
    File::open(&stdout_path)?.read_to_string(&mut stdout)?;
    let mut stderr = String::new();
    File::open(&stderr_path)?.read_to_string(&mut stderr)?;
    if !status.success() {
        bail!(
            "{provider} exited with {status}: {}",
            stderr.chars().take(800).collect::<String>()
        );
    }
    let raw = if provider == "codex" {
        std::fs::read_to_string(output.path().join("reply"))?
    } else {
        stdout
    };
    let action = parse_action(&raw).with_context(|| {
        format!(
            "{provider} returned: {}",
            raw.chars().take(800).collect::<String>()
        )
    })?;
    Ok((action, raw))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        Ok(())
    }
}
