use std::fs::File;
use std::io::{Read, Write};
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

fn reported_usage(provider: &str, stdout: &str) -> Option<Usage> {
    let value: Value = if provider == "codex" {
        stdout
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|event| event["type"] == "turn.completed")?
    } else {
        serde_json::from_str(stdout).ok()?
    };
    let usage = value.get("usage")?;
    Some(Usage {
        input_tokens: usage.get("input_tokens")?.as_u64()?,
        output_tokens: usage.get("output_tokens")?.as_u64()?,
        cached_input_tokens: usage
            .get("cached_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        source: "provider".into(),
    })
}

const SCHEMA: &str = r#"{"type":"object","properties":{"kind":{"type":"string","enum":["search_capabilities","invoke","inspect_result","checkpoint","finish","blocked"]},"query":{"type":"string"},"capability":{"type":"string"},"args":{"type":"string"},"artifact":{"type":"string"},"checkpoint":{"type":"string"},"summary":{"type":"string"},"evidence":{"type":"array","items":{"type":"string"}},"reason":{"type":"string"}},"required":["kind","query","capability","args","artifact","checkpoint","summary","evidence","reason"],"additionalProperties":false}"#;

fn parse_action(raw: &str) -> Result<Action> {
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
    let response = call_with_cancel(provider, prompt, directory, timeout, || false)?;
    Ok((response.action, response.raw))
}

pub fn call_with_cancel(
    provider: &str,
    prompt: &str,
    directory: &Path,
    timeout: Duration,
    cancelled: impl Fn() -> bool,
) -> Result<Response> {
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
                    "--json",
                    "--output-schema",
                ])
                .arg(&schema_path)
                .args(["-o"])
                .arg(output.path().join("reply"))
                .arg("-");
            for feature in [
                "apps",
                "browser_use",
                "computer_use",
                "image_generation",
                "multi_agent",
                "plugins",
                "shell_tool",
                "skill_search",
                "view_image",
                "tool_suggest",
                "sleep_tool",
                "goals",
            ] {
                command.args(["--disable", feature]);
            }
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
        .stdin(if provider == "codex" {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::from(File::create(&stdout_path)?))
        .stderr(Stdio::from(File::create(&stderr_path)?));
    let mut child = command
        .spawn()
        .with_context(|| format!("start {provider}; install and log in to its CLI first"))?;
    if provider == "codex" {
        child
            .stdin
            .take()
            .context("Codex stdin unavailable")?
            .write_all(prompt.as_bytes())?;
    }
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
        if cancelled() {
            child.kill()?;
            child.wait()?;
            bail!("run cancelled during model call");
        }
        thread::sleep(Duration::from_millis(100));
    };
    let mut stdout = String::new();
    File::open(&stdout_path)?.read_to_string(&mut stdout)?;
    let mut stderr = String::new();
    File::open(&stderr_path)?.read_to_string(&mut stderr)?;
    if !status.success() {
        bail!(
            "{provider} exited with {status}: stdout={} stderr={}",
            stdout.chars().take(800).collect::<String>(),
            stderr.chars().take(800).collect::<String>()
        );
    }
    let reported = reported_usage(provider, &stdout);
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
    let usage = Some(reported.unwrap_or_else(|| Usage {
        input_tokens: (prompt.chars().count() as u64).div_ceil(4),
        output_tokens: (raw.chars().count() as u64).div_ceil(4),
        cached_input_tokens: 0,
        source: "estimated".into(),
    }));
    Ok(Response { action, raw, usage })
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
