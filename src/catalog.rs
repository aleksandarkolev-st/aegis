use std::fs::{self, File};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 160
        && !id
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
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

pub fn native(provider: &str) -> Result<Catalog> {
    match provider {
        "codex" => {
            let home = std::env::var_os("CODEX_HOME").map(std::path::PathBuf::from).or_else(|| {
                std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(|home| std::path::PathBuf::from(home).join(".codex"))
            }).context("Codex home is unavailable")?;
            codex_cache(&home.join("models_cache.json"))
        }
        "claude" => Ok(Catalog {
            models: [ ("sonnet", "Sonnet · balanced coding"), ("opus", "Opus · complex reasoning"), ("haiku", "Haiku · lightweight tasks") ].into_iter().map(|(id, label)| Model { id: id.into(), label: label.into() }).collect(),
            source: "Claude Code model aliases · resolved by your CLI; account access depends on your plan".into(),
        }),
        "grok" => {
            let directory = tempfile::tempdir()?;
            let stdout = directory.path().join("models");
            let mut command = Command::new(crate::provider::executable(provider)?);
            command.arg("models").stdin(Stdio::null()).stdout(Stdio::from(File::create(&stdout)?)).stderr(Stdio::null());
            let mut child = crate::process::spawn(command)?;
            let started = Instant::now();
            loop {
                if let Some(status) = child.try_wait()? {
                    if !status.success() { bail!("Grok couldn't list models. Sign in with F4 or use its default model."); }
                    break;
                }
                if started.elapsed() > Duration::from_secs(10) { bail!("Grok model discovery timed out"); }
                std::thread::sleep(Duration::from_millis(50));
            }
            if fs::metadata(&stdout)?.len() > 256 * 1024 { bail!("Grok model catalog is too large"); }
            Ok(grok_output(&fs::read_to_string(stdout)?))
        }
        _ => bail!("Unsupported native provider"),
    }
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
