use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    endpoint::{Endpoint, ResponseFormat},
    kernel, provider,
    selection::{Selection, SelectionStore},
    storage::Store,
    terminal::{Input, RawMode, Terminal, Tone},
    trace,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Profile {
    #[serde(deserialize_with = "profile_provider")]
    provider: String,
    model: Option<String>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    fallback_routes: Vec<crate::routing::Route>,
    endpoint: Option<Endpoint>,
    #[serde(default)]
    api_key_env: Option<String>,
    write: bool,
    image: Option<String>,
    #[serde(default)]
    limits: crate::budget::Limits,
    #[serde(default)]
    acceptance_check: Option<crate::acceptance::Check>,
    #[serde(default)]
    command_scopes: Option<crate::policy::CommandScopes>,
    #[serde(default)]
    filesystem_scopes: Option<crate::filesystem::FileScopes>,
    #[serde(default)]
    network_scopes: Option<crate::network::NetworkScopes>,
    #[serde(default)]
    previous_run: Option<String>,
}

struct TaskSecret {
    run_id: String,
    target: String,
    value: String,
}

impl TaskSecret {
    fn matches(&self, run: &crate::storage::Run, route: &crate::routing::Route) -> bool {
        self.run_id == run.id && route_secret_target(run, route).as_deref() == Some(&self.target)
    }
}

fn route_secret_target(
    _run: &crate::storage::Run,
    route: &crate::routing::Route,
) -> Option<String> {
    if route.provider == "claude-api" {
        return Some("claude-api".into());
    }
    route
        .endpoint
        .as_ref()
        .map(|endpoint| endpoint.base_url.clone())
}

fn route_key_reference<'a>(
    run: &'a crate::storage::Run,
    route: &'a crate::routing::Route,
) -> Option<&'a str> {
    if route.provider == "claude-api" {
        return if run.provider == "claude-api" {
            run.budgets["api_key_env"].as_str()
        } else {
            route.api_key_env.as_deref()
        };
    }
    route
        .endpoint
        .as_ref()
        .and_then(|endpoint| endpoint.api_key_env.as_deref())
}

fn profile_provider<'de, Decoder: serde::Deserializer<'de>>(
    decoder: Decoder,
) -> std::result::Result<String, Decoder::Error> {
    let provider = String::deserialize(decoder)?;
    Ok(crate::provider::canonical(&provider).to_owned())
}

fn name(provider: &str) -> &str {
    match crate::provider::canonical(provider) {
        "codex" => "ChatGPT",
        "claude-api" => "Claude API",
        "claude" => "Claude · pending",
        "grok" => "Grok",
        "custom" => "Custom endpoint",
        other => other,
    }
}

fn field(terminal: &Terminal, label: &str, secret: bool) -> Result<Option<String>> {
    match terminal.input(label, secret, &[])? {
        Input::Submit(value) => Ok(Some(value)),
        _ => Ok(None),
    }
}

fn goal_task_submission(request: &str) -> Option<&str> {
    let rest = request
        .strip_prefix("/goal")
        .or_else(|| request.strip_prefix("/contract"))?;
    if !rest.chars().next().is_some_and(char::is_whitespace) {
        return None;
    }
    let task = rest.trim_start();
    if task.is_empty()
        || matches!(
            task.split_whitespace().next()?,
            "add" | "replace" | "history"
        )
    {
        return None;
    }
    Some(task)
}

fn load_catalog(
    terminal: &Terminal,
    profile: &Profile,
    secret: Option<&str>,
    refresh: bool,
) -> Result<Option<Result<crate::catalog::Catalog>>> {
    let provider = profile.provider.clone();
    let endpoint = profile.endpoint.clone();
    let api_key_env = profile.api_key_env.clone();
    let secret = secret.map(str::to_owned);
    crate::background::run(
        terminal,
        "Discovering your available models",
        move |cancelled| {
            let interrupted = || cancelled.load(std::sync::atomic::Ordering::Acquire);
            Ok(if provider == "claude-api" {
                let key = secret.or_else(|| api_key_env.and_then(|name| std::env::var(name).ok()));
                key.as_deref()
                    .ok_or_else(|| anyhow::anyhow!("Claude API key is needed for model discovery"))
                    .and_then(|key| crate::claude_api::models(key, interrupted))
                    .map(|models| crate::catalog::Catalog {
                        models,
                        source: "Claude API models available to this key".into(),
                    })
            } else if let Some(endpoint) = endpoint {
                endpoint
                    .models_with_cancel(secret.as_deref(), interrupted)
                    .map(|models| crate::catalog::Catalog {
                        models,
                        source: "Your endpoint's /models catalog".into(),
                    })
            } else {
                if refresh {
                    crate::catalog::refresh_for_selection(&provider, interrupted)
                } else {
                    crate::catalog::for_selection(&provider, interrupted)
                }
            })
        },
    )
}

fn choose_model(
    terminal: &Terminal,
    profile: &Profile,
    secret: Option<&str>,
) -> Result<Option<Option<String>>> {
    let saved =
        if terminal.interactive && profile.endpoint.is_none() && profile.provider != "claude-api" {
            crate::catalog::saved_only_for_selection(&profile.provider)?
        } else {
            None
        };
    let mut refresh_available = saved.is_some();
    let mut catalog = if let Some(catalog) = saved {
        Ok(catalog)
    } else {
        terminal.message(Tone::Quiet, "Models", "Loading the provider catalog…")?;
        let Some(catalog) = load_catalog(terminal, profile, secret, false)? else {
            return Ok(None);
        };
        catalog
    };
    loop {
        let models = match &catalog {
            Ok(catalog) => {
                if terminal.interactive {
                    terminal.message(Tone::Quiet, "", &catalog.source)?;
                } else {
                    terminal.message(Tone::Quiet, "Catalog", &catalog.source)?;
                }
                catalog.models.as_slice()
            }
            Err(error) => {
                terminal.message(
                    Tone::Quiet,
                    "Catalog",
                    &format!(
                        "{} You can enter an advertised model ID.",
                        crate::catalog::discovery_error(&error.to_string())
                    ),
                )?;
                &[]
            }
        };
        let mut choices = Vec::new();
        let mut values = Vec::new();
        for model in models {
            let current = if profile.model.as_deref() == Some(model.id.as_str()) {
                " · current"
            } else {
                ""
            };
            choices.push(if model.label.is_empty() || model.label == model.id {
                format!("{}{current}", model.id)
            } else {
                format!("{} [{}]{current}", model.label, model.id)
            });
            values.push(Some(model.id.clone()));
        }
        if let Some(current) = &profile.model {
            if !values.iter().any(|value| value.as_ref() == Some(current)) {
                choices.push(format!("{current} · current (not in catalog)"));
                values.push(Some(current.clone()));
            }
        }
        let manual = choices.len();
        choices.push("Enter another model ID…".into());
        let refresh = if refresh_available {
            let index = choices.len();
            choices.push("Refresh live model list…".into());
            Some(index)
        } else {
            None
        };
        let selected = if choices.len() == 1 {
            Some(0)
        } else {
            terminal.select_at(
                &format!("{} · choose a model", name(&profile.provider)),
                &choices,
                values
                    .iter()
                    .position(|model| model == &profile.model)
                    .unwrap_or(0),
            )?
        };
        let Some(selected) = selected else {
            return Ok(None);
        };
        if refresh == Some(selected) {
            terminal.message(Tone::Quiet, "Models", "Checking the live catalog…")?;
            let Some(updated) = load_catalog(terminal, profile, secret, true)? else {
                return Ok(None);
            };
            catalog = updated;
            refresh_available = false;
            continue;
        }
        if selected == manual {
            let Some(id) = field(terminal, "  Model ID › ", false)? else {
                return Ok(None);
            };
            let id = id.trim();
            if !crate::catalog::valid_id(id) {
                terminal.message(
                    Tone::Warning,
                    "Model",
                    "Use a nonempty model ID without spaces or control characters (up to 160 bytes).",
                )?;
                return Ok(None);
            }
            return Ok(Some(Some(id.into())));
        }
        return Ok(Some(values[selected].clone()));
    }
}

fn show_selection(terminal: &Terminal, profile: &Profile) -> Result<()> {
    terminal.message(
        Tone::Accent,
        "Model",
        &format!(
            "{} · {} · reasoning {}",
            name(&profile.provider),
            profile.model.as_deref().unwrap_or("provider default"),
            profile.reasoning_effort.as_deref().unwrap_or("default")
        ),
    )
}

fn switch_provider(
    root: &Path,
    terminal: &Terminal,
    profile: &mut Profile,
    secret: &mut Option<String>,
) -> Result<()> {
    if let Some((selected, key)) = configure_provider(terminal)? {
        profile.provider = selected.provider;
        profile.model = selected.model;
        profile.reasoning_effort = None;
        profile.endpoint = selected.endpoint;
        profile.api_key_env = selected.api_key_env;
        profile.fallback_routes.clear();
        *secret = key;
        save(root, profile)?;
        remember_selection(terminal, profile)?;
        show_selection(terminal, profile)?;
        terminal.message(Tone::Quiet, "Settings kept", "Workspace permissions, budgets and saved tasks are unchanged. This selection applies to new tasks; saved tasks retain their original provider.")?;
    }
    Ok(())
}

fn switch_model(
    root: &Path,
    terminal: &Terminal,
    profile: &mut Profile,
    secret: Option<&str>,
) -> Result<()> {
    if let Some(model) = choose_model(terminal, profile, secret)? {
        let mut candidate = profile.clone();
        if candidate.model != model {
            candidate.reasoning_effort = None;
        }
        candidate.model = model;
        if terminal.interactive {
            let Some(effort) = choose_reasoning(terminal, &candidate, secret, false)? else {
                return Ok(());
            };
            candidate.reasoning_effort = effort;
        }
        *profile = candidate;
        save(root, profile)?;
        remember_selection(terminal, profile)?;
        show_selection(terminal, profile)?;
        terminal.message(
            Tone::Quiet,
            "",
            "New tasks use this model; saved tasks keep their original selection.",
        )?;
    }
    Ok(())
}

fn choose_reasoning(
    terminal: &Terminal,
    profile: &Profile,
    secret: Option<&str>,
    refresh: bool,
) -> Result<Option<Option<String>>> {
    if refresh && let Ok(provider) = crate::direct::provider(&profile.provider) {
        if crate::auth_store::Vault::user()?
            .load(provider.session_name())?
            .is_some()
        {
            let Some(result) = load_catalog(terminal, profile, None, true)? else {
                return Ok(None);
            };
            if let Err(error) = result {
                terminal.message(
                    Tone::Warning,
                    "Reasoning",
                    crate::catalog::discovery_error(&error.to_string()),
                )?;
                return Ok(None);
            }
        }
    }
    let levels = if profile.provider == "claude-api" {
        let Some(model) = profile.model.clone() else {
            return Ok(Some(None));
        };
        let key = secret.map(str::to_owned).or_else(|| {
            profile
                .api_key_env
                .as_deref()
                .and_then(|reference| std::env::var(reference).ok())
        });
        let Some(key) = key else {
            terminal.message(Tone::Warning, "Reasoning", "Enter your Claude API key with F4 to inspect this model's supported effort levels.")?;
            return Ok(None);
        };
        let result = crate::background::run(
            terminal,
            "Checking supported Claude effort",
            move |cancelled| {
                Ok(crate::claude_api::efforts(&model, &key, || {
                    cancelled.load(std::sync::atomic::Ordering::Acquire)
                }))
            },
        )?;
        let Some(result) = result else {
            return Ok(None);
        };
        match result {
            Ok(levels) => levels,
            Err(error) => {
                terminal.message(Tone::Warning, "Reasoning unavailable", &error.to_string())?;
                return Ok(None);
            }
        }
    } else {
        profile
            .model
            .as_deref()
            .map(|model| {
                crate::catalog::reasoning_levels(&profile.provider, model).unwrap_or_default()
            })
            .unwrap_or_default()
    };
    if levels.is_empty() {
        terminal.message(
            Tone::Quiet,
            "Reasoning",
            "Provider-managed · choose a catalog model to see its supported levels.",
        )?;
        return Ok(Some(None));
    }
    if profile.provider == "custom" {
        terminal.message(Tone::Quiet, "Endpoint option", "Explicitly sends reasoning_effort; your endpoint/model must support it. Default omits the field.")?;
    }
    let default = profile.model.as_deref().and_then(|id| {
        crate::catalog::advertised_reasoning_default(&profile.provider, id)
            .ok()
            .flatten()
    });
    let mut choices = vec![match default {
        Some(effort) => format!("Provider default · {effort} · no override"),
        None => "Provider default · no override".into(),
    }];
    choices.extend(levels.iter().map(|effort| {
        format!(
            "{effort} · {}{}",
            match effort.as_str() {
                "none" | "minimal" | "low" => "quicker responses",
                "medium" => "balanced depth",
                "high" => "deeper problem solving",
                _ => "highest depth · may consume more usage",
            },
            if profile.reasoning_effort.as_ref() == Some(effort) {
                " · current"
            } else {
                ""
            }
        )
    }));
    let current = profile
        .reasoning_effort
        .as_ref()
        .and_then(|effort| levels.iter().position(|level| level == effort))
        .map(|index| index + 1)
        .unwrap_or(0);
    Ok(terminal
        .select_at("Reasoning effort", &choices, current)?
        .map(|index| {
            if index == 0 {
                None
            } else {
                Some(levels[index - 1].clone())
            }
        }))
}

fn switch_reasoning(
    root: &Path,
    terminal: &Terminal,
    profile: &mut Profile,
    secret: Option<&str>,
) -> Result<()> {
    if let Some(effort) = choose_reasoning(terminal, profile, secret, true)? {
        profile.reasoning_effort = effort;
        save(root, profile)?;
        remember_selection(terminal, profile)?;
        show_selection(terminal, profile)?;
    }
    Ok(())
}

fn blank_profile(provider: String) -> Profile {
    Profile {
        provider,
        model: None,
        reasoning_effort: None,
        fallback_routes: Vec::new(),
        endpoint: None,
        api_key_env: None,
        write: false,
        image: None,
        limits: crate::budget::Limits::default(),
        acceptance_check: None,
        command_scopes: None,
        filesystem_scopes: None,
        network_scopes: None,
        previous_run: None,
    }
}

fn profile_from_selection(selection: Selection) -> Profile {
    let mut profile = blank_profile(selection.provider);
    profile.model = Some(selection.model);
    profile.reasoning_effort = selection.reasoning_effort;
    profile
}

fn configure_provider(terminal: &Terminal) -> Result<Option<(Profile, Option<String>)>> {
    let choices = [
        "ChatGPT — direct account sign-in",
        "Claude — subscription sign-in pending",
        "Claude API — Console key, separate billing",
        "Grok — direct account sign-in",
        "Custom OpenAI-compatible endpoint",
    ]
    .map(str::to_owned);
    let Some(choice) = terminal.select("Choose your provider", &choices)? else {
        return Ok(None);
    };
    let provider = ["codex", "claude", "claude-api", "grok", "custom"][choice].to_owned();
    let mut profile = blank_profile(provider);
    let mut secret = None;
    if profile.provider == "claude-api" {
        terminal.message(Tone::Quiet, "Claude API", "Uses a Claude Console API key with separate API billing, not Claude Code subscription login.")?;
        let Some(key) = field(terminal, "  Claude API key (hidden) › ", true)? else {
            return Ok(None);
        };
        if let Err(error) = crate::claude_api::validate_key(&key) {
            terminal.message(Tone::Warning, "Key needed", &error.to_string())?;
            return Ok(None);
        }
        profile.api_key_env = Some("ARUN_SESSION_API_KEY".into());
        secret = Some(key);
    } else if profile.provider == "custom" {
        let Some(url) = field(terminal, "  Endpoint URL › ", false)? else {
            return Ok(None);
        };
        let formats = [
            "Strict JSON schema — recommended when supported",
            "JSON object — for servers without schema support",
            "Prompt-only JSON — for minimal compatible servers",
        ]
        .map(str::to_owned);
        let Some(format) = terminal.select("Endpoint response format", &formats)? else {
            return Ok(None);
        };
        let Some(key) = field(terminal, "  API key (hidden; optional) › ", true)? else {
            return Ok(None);
        };
        if !key.trim().is_empty() {
            secret = Some(key);
        }
        let endpoint = Endpoint {
            base_url: url,
            api_key_env: secret.as_ref().map(|_| "ARUN_SESSION_API_KEY".into()),
            response_format: match format {
                1 => ResponseFormat::Json,
                2 => ResponseFormat::None,
                _ => ResponseFormat::Schema,
            },
            allow_insecure: false,
        };
        if let Err(error) = endpoint.url() {
            terminal.message(Tone::Warning, "!", &error.to_string())?;
            return Ok(None);
        }
        profile.endpoint = Some(endpoint);
    } else {
        if !ensure_provider(terminal, &profile.provider)? {
            return Ok(None);
        }
        terminal.message(Tone::Quiet, "Ready", "Using Aegis's saved account sign-in. F4 manages sign-in; F6 chooses the model and reasoning.")?;
    }
    {
        let Some(model) = choose_model(terminal, &profile, secret.as_deref())? else {
            return Ok(None);
        };
        profile.model = model;
    }
    Ok(Some((profile, secret)))
}

fn configure(terminal: &Terminal) -> Result<Option<(Profile, Option<String>)>> {
    let saved = if terminal.interactive {
        match SelectionStore::user().and_then(|store| store.load()) {
            Ok(selection) => selection,
            Err(_) => {
                terminal.message(
                    Tone::Quiet,
                    "Preferences",
                    "Saved account shortcut is unavailable; choose a provider instead.",
                )?;
                None
            }
        }
    } else {
        None
    };
    let selected = if let Some(selection) = saved {
        let choices = [
            format!(
                "Use saved {} · {} · reasoning {}",
                name(&selection.provider),
                selection.model,
                selection
                    .reasoning_effort
                    .as_deref()
                    .unwrap_or("provider default")
            ),
            "Choose another provider".into(),
        ];
        match terminal.select(
            "Quick start · workspace permissions are reviewed next",
            &choices,
        )? {
            Some(0) => {
                let profile = profile_from_selection(selection);
                if !ensure_provider(terminal, &profile.provider)? {
                    return Ok(None);
                }
                Some((profile, None))
            }
            Some(1) => configure_provider(terminal)?,
            _ => return Ok(None),
        }
    } else {
        configure_provider(terminal)?
    };
    let Some((mut profile, secret)) = selected else {
        return Ok(None);
    };
    if !configure_environment(terminal, &mut profile)? {
        return Ok(None);
    }
    terminal.message(Tone::Success, "You're set", "Describe what you want to build. Sensible budgets and evidence checks are already on; F7 is there if you want to customize them.")?;
    Ok(Some((profile, secret)))
}

fn configure_environment(terminal: &Terminal, profile: &mut Profile) -> Result<bool> {
    let permissions = [
        "Allow workspace edits",
        "Review only — no edits",
        "Workspace edits and isolated container commands",
    ]
    .map(str::to_owned);
    let Some(permission) = terminal.select("Workspace permissions", &permissions)? else {
        return Ok(false);
    };
    profile.write = permission != 1;
    profile.image = None;
    if permission == 2 {
        let docker = provider::system_executable("docker");
        let output = docker.as_ref().and_then(|program| {
            crate::process::background(&mut Command::new(program))
                .args(["image", "ls", "--format", "{{.Repository}}:{{.Tag}}"])
                .stderr(Stdio::null())
                .output()
                .ok()
        });
        let available = output
            .as_ref()
            .is_some_and(|output| output.status.success());
        let images: Vec<String> = output
            .filter(|output| output.status.success())
            .map(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .filter(|line| !line.contains("<none>"))
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        if !available {
            terminal.message(Tone::Warning, "Containers", "Docker is missing or its daemon is unavailable. File edits remain enabled; process tools stay disabled.")?;
        } else if images.is_empty() {
            profile.image = download_image(terminal, docker.as_ref().unwrap())?;
        } else if let Some(index) = terminal.select("Choose a local container image", &images)? {
            profile.image = Some(images[index].clone());
        }
    }
    Ok(true)
}

fn configure_acceptance(terminal: &Terminal, profile: &mut Profile) -> Result<bool> {
    let choices = [
        "Successful-operation evidence",
        "Independent container check from a JSON file",
    ]
    .map(str::to_owned);
    let Some(choice) = terminal.select("Completion checks", &choices)? else {
        return Ok(false);
    };
    profile.acceptance_check = None;
    if choice == 1 {
        let Some(path) = field(terminal, "  Acceptance JSON file › ", false)? else {
            return Ok(false);
        };
        match crate::acceptance::Check::from_file(Path::new(&path)) {
            Ok(check) => {
                terminal.message(Tone::Accent, "Acceptance", &format!("{} · {} · read-only workspace, no network. The image must already be available locally.", check.name, check.image))?;
                profile.acceptance_check = Some(check);
            }
            Err(error) => {
                terminal.message(Tone::Warning, "Invalid check", &format!("{error:#}"))?;
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn settings(
    root: &Path,
    terminal: &mut Terminal,
    profile: &mut Profile,
    secret: &mut Option<String>,
) -> Result<()> {
    let choices = [
        "Workspace permissions and command environment",
        "Task budgets",
        "Completion checks",
        "Exact command scopes",
        "Appearance · mascot, colors and motion",
        "Project memory · remember what matters",
        "File access scopes · optional exact files / folders",
        "Web access · optional approved HTTPS domains",
        "Project instructions · pinned, scoped rules",
        "Automatic provider fallback · optional",
        "Back",
    ]
    .map(str::to_owned);
    let mut selected = profile.clone();
    let mut selected_secret = secret.clone();
    let changed = match terminal.select("Settings · no provider reset", &choices)? {
        Some(0) => configure_environment(terminal, &mut selected)?,
        Some(1) => {
            if let Some(limits) = configure_limits(terminal)? {
                selected.limits = limits;
                true
            } else {
                false
            }
        }
        Some(2) => configure_acceptance(terminal, &mut selected)?,
        Some(3) => configure_command_scopes(terminal, &mut selected)?,
        Some(4) => return configure_appearance(root, terminal, profile),
        Some(5) => return memory_menu(root, terminal),
        Some(6) => configure_file_scopes(terminal, &mut selected)?,
        Some(7) => configure_network_scopes(terminal, &mut selected)?,
        Some(8) => return instruction_menu(root, terminal),
        Some(9) => configure_fallback(terminal, &mut selected, &mut selected_secret)?,
        _ => false,
    };
    if changed {
        save(root, &selected)?;
        if secret_reference(&selected).is_none() {
            selected_secret = None;
        }
        *profile = selected;
        *secret = selected_secret;
        terminal.message(
            Tone::Success,
            "Settings saved",
            "Applies to new tasks. Existing tasks retain their approved permissions and budgets.",
        )?;
    }
    Ok(())
}

fn configure_fallback(
    terminal: &Terminal,
    profile: &mut Profile,
    secret: &mut Option<String>,
) -> Result<bool> {
    if !matches!(
        profile.provider.as_str(),
        "codex" | "grok" | "custom" | "claude-api"
    ) {
        terminal.message(
            Tone::Quiet,
            "Fallback",
            "Claude subscription fallback is pending. ChatGPT, Grok, Claude API and custom endpoint routes can be reviewed here.",
        )?;
        return Ok(false);
    }
    let available: Vec<_> = ["codex", "grok", "custom"]
        .into_iter()
        .filter(|provider| {
            *provider != profile.provider
                && !(profile.provider == "claude-api" && *provider == "custom")
                && !profile
                    .fallback_routes
                    .iter()
                    .any(|route| route.provider == *provider)
        })
        .collect();
    let current = if profile.fallback_routes.is_empty() {
        "off".into()
    } else {
        profile
            .fallback_routes
            .iter()
            .map(|route| format!("{} / {}", name(&route.provider), route.model))
            .collect::<Vec<_>>()
            .join(" → ")
    };
    let mut choices = vec![format!("Keep current · {}", current)];
    choices.extend(available.iter().map(|provider| {
        if profile.fallback_routes.is_empty() {
            format!("Use {} as fallback", name(provider))
        } else {
            format!("Add {} after current fallback", name(provider))
        }
    }));
    let swap = profile.fallback_routes.len() > 1;
    if swap {
        choices.push("Swap fallback order".into());
    }
    choices.extend(["Turn automatic fallback off".into(), "Back".into()]);
    let selected = terminal.select("Switch on quota, outage or removed model", &choices)?;
    if swap && selected == Some(available.len() + 1) {
        profile.fallback_routes.swap(0, 1);
        return Ok(true);
    }
    if selected == Some(available.len() + 1 + usize::from(swap)) {
        profile.fallback_routes.clear();
        return Ok(true);
    }
    let Some(other) = selected
        .and_then(|index| index.checked_sub(1))
        .and_then(|index| available.get(index))
    else {
        return Ok(false);
    };
    let route = if *other == "custom" {
        let Some(base_url) = field(terminal, "OpenAI-compatible endpoint URL", false)? else {
            return Ok(false);
        };
        let formats = [
            "Strict JSON schema",
            "JSON object",
            "Prompt-only JSON",
            "Back",
        ]
        .map(str::to_owned);
        let Some(format) = terminal.select("Endpoint response format", &formats)? else {
            return Ok(false);
        };
        let response_format = match format {
            0 => ResponseFormat::Schema,
            1 => ResponseFormat::Json,
            2 => ResponseFormat::None,
            _ => return Ok(false),
        };
        let Some(key) = field(
            terminal,
            "Fallback API key (blank for keyless endpoint)",
            true,
        )?
        else {
            return Ok(false);
        };
        let key = (!key.trim().is_empty()).then_some(key);
        let endpoint = Endpoint {
            base_url,
            api_key_env: key.as_ref().map(|_| "ARUN_SESSION_API_KEY".into()),
            response_format,
            allow_insecure: false,
        };
        if let Err(error) = endpoint.url() {
            terminal.message(Tone::Warning, "Invalid endpoint", &error.to_string())?;
            return Ok(false);
        }
        let discovery = endpoint.clone();
        let discovery_key = key.clone();
        let catalog =
            crate::background::run(terminal, "Verifying endpoint models", move |cancelled| {
                discovery.models_with_cancel(discovery_key.as_deref(), || {
                    cancelled.load(std::sync::atomic::Ordering::Acquire)
                })
            });
        let models = match catalog {
            Ok(Some(models)) if !models.is_empty() => models,
            Ok(Some(_)) => {
                terminal.message(
                    Tone::Warning,
                    "Fallback unavailable",
                    "The endpoint advertised no selectable models.",
                )?;
                return Ok(false);
            }
            Ok(None) => return Ok(false),
            Err(error) => {
                terminal.message(Tone::Warning, "Fallback unavailable", &error.to_string())?;
                return Ok(false);
            }
        };
        let mut model_choices: Vec<_> = models.iter().map(|model| model.id.clone()).collect();
        model_choices.push("Back".into());
        let Some(index) = terminal.select("Choose verified endpoint model", &model_choices)? else {
            return Ok(false);
        };
        let Some(model) = models.get(index) else {
            return Ok(false);
        };
        *secret = key;
        crate::routing::Route {
            provider: "custom".into(),
            model: model.id.clone(),
            reasoning_effort: None,
            endpoint: Some(endpoint),
            api_key_env: None,
        }
    } else {
        if !ensure_provider(terminal, other)? {
            return Ok(false);
        }
        let candidate = blank_profile((*other).into());
        let Some(Some(model)) = choose_model(terminal, &candidate, None)? else {
            return Ok(false);
        };
        let provider = crate::direct::provider(other)?;
        let catalog = crate::background::run(
            terminal,
            "Verifying fallback account and model",
            move |cancelled| {
                crate::provider_catalog::refresh(
                    &crate::auth_store::Vault::user()?,
                    provider,
                    || cancelled.load(std::sync::atomic::Ordering::Acquire),
                )
            },
        );
        let models = match catalog {
            Ok(Some(models)) => models,
            Ok(None) => return Ok(false),
            Err(error) => {
                terminal.message(Tone::Warning, "Fallback unavailable", &error.to_string())?;
                return Ok(false);
            }
        };
        if !models.iter().any(|entry| entry.id == model) {
            terminal.message(
                Tone::Warning,
                "Fallback unavailable",
                "The selected model is not in this account's live catalog. No fallback was saved.",
            )?;
            return Ok(false);
        }
        crate::routing::Route {
            provider: (*other).into(),
            model,
            reasoning_effort: None,
            endpoint: None,
            api_key_env: None,
        }
    };
    route.validate()?;
    let destination = match route.endpoint.as_ref() {
        Some(endpoint) => endpoint.url()?.to_string(),
        None => "Aegis signed-in account".into(),
    };
    terminal.message(
        Tone::Accent,
        "Review fallback",
        &format!(
            "{} / {} · {} · same task, permissions, evidence, and budget",
            name(&route.provider),
            route.model,
            destination
        ),
    )?;
    terminal.message(Tone::Warning, "Context sharing", "Aegis sends the task and bounded workspace context to this fallback if the primary fails. No account credential is copied.")?;
    let confirmation = ["Keep current fallback", "Approve this fallback"].map(str::to_owned);
    if terminal.select("Approve automatic switch?", &confirmation)? != Some(1) {
        return Ok(false);
    }
    profile.fallback_routes.push(route);
    Ok(true)
}

fn configure_network_scopes(terminal: &Terminal, profile: &mut Profile) -> Result<bool> {
    terminal.message(Tone::Quiet,"Web access","Off by default. Optionally allow read-only HTTPS from exact public domains; this does not open container networking or share login credentials. Bodies have a durable byte budget.")?;
    if let Some(scopes) = &profile.network_scopes {
        terminal.message(Tone::Accent, "Domains", &scopes.domains.join(", "))?;
        terminal.message(
            Tone::Quiet,
            "Body budget",
            &format!(
                "{} bytes for new tasks; at most 1 MiB per response",
                scopes.body_bytes
            ),
        )?;
    }
    let choices = [
        "Keep current web access",
        "Approve exact domains here",
        "Disable web access",
        "Load reviewed network scopes JSON",
        "Back",
    ]
    .map(str::to_owned);
    profile.network_scopes = match terminal.select("Optional web access", &choices)? {
        Some(1) => {
            let mut domains = Vec::new();
            loop {
                let Some(domain) =
                    field(terminal, "  Domain (example.com; empty finishes) › ", false)?
                else {
                    return Ok(false);
                };
                if domain.is_empty() {
                    break;
                }
                if domains.len() >= 32 {
                    terminal.message(
                        Tone::Warning,
                        "Domain limit",
                        "At most 32 exact domains are supported. Existing settings are unchanged.",
                    )?;
                    return Ok(false);
                }
                domains.push(domain.trim().to_ascii_lowercase());
            }
            let scopes = crate::network::NetworkScopes {
                domains,
                body_bytes: 8 * 1024 * 1024,
            };
            if let Err(error) = scopes.validate() {
                terminal.message(Tone::Warning, "Invalid domains", &error.to_string())?;
                return Ok(false);
            }
            Some(scopes)
        }
        Some(2) => None,
        Some(3) => {
            let Some(path) = field(terminal, "  Reviewed JSON file › ", false)? else {
                return Ok(false);
            };
            match crate::network::NetworkScopes::from_file(Path::new(&path)) {
                Ok(scopes) => Some(scopes),
                Err(error) => {
                    terminal.message(
                        Tone::Warning,
                        "Invalid network scopes",
                        &error.to_string(),
                    )?;
                    return Ok(false);
                }
            }
        }
        _ => return Ok(false),
    };
    Ok(true)
}

fn configure_file_scopes(terminal: &Terminal, profile: &mut Profile) -> Result<bool> {
    terminal.message(Tone::Quiet,"File access","Optional restrictions for new tasks. Exact files or src/** folder subtrees; ** means the workspace. Empty lists deny host access. Existing permission grants still apply.")?;
    if let Some(scopes) = &profile.filesystem_scopes {
        terminal.message(Tone::Accent, "Read", &scopes.read.join(", "))?;
        terminal.message(Tone::Accent, "Write", &scopes.write.join(", "))?;
    }
    let choices = [
        "Keep current scopes",
        "Choose files / folders here",
        "Load a reviewed JSON file",
        "Deny all file access",
        "Remove file restrictions",
        "Back",
    ]
    .map(str::to_owned);
    let scopes = match terminal.select("File access scopes", &choices)? {
        Some(1) => {
            let mut lists = [Vec::new(), Vec::new()];
            for (list, label) in lists.iter_mut().zip(["Read", "Write"]) {
                terminal.message(Tone::Quiet,label,"Enter one relative file or folder/** per line. Enter an empty line when finished.")?;
                loop {
                    let Some(path) = field(terminal, "  Path › ", false)? else {
                        return Ok(false);
                    };
                    if path.is_empty() {
                        break;
                    }
                    if list.len() >= 64 {
                        terminal.message(
                            Tone::Warning,
                            "Scope limit",
                            "At most 64 paths are allowed. Existing settings are unchanged.",
                        )?;
                        return Ok(false);
                    }
                    list.push(path);
                }
            }
            let [read, write] = lists;
            let scopes = crate::filesystem::FileScopes { read, write };
            if let Err(error) = scopes.validate() {
                terminal.message(Tone::Warning, "Invalid scopes", &error.to_string())?;
                return Ok(false);
            }
            Some(scopes)
        }
        Some(2) => {
            let Some(path) = field(terminal, "  Reviewed JSON file › ", false)? else {
                return Ok(false);
            };
            match crate::filesystem::FileScopes::from_file(Path::new(&path)) {
                Ok(scopes) => Some(scopes),
                Err(error) => {
                    terminal.message(Tone::Warning, "Invalid scopes", &error.to_string())?;
                    return Ok(false);
                }
            }
        }
        Some(3) => Some(crate::filesystem::FileScopes {
            read: Vec::new(),
            write: Vec::new(),
        }),
        Some(4) => {
            let confirmation = [
                "Keep file restrictions",
                "Allow the workspace under existing permissions",
            ]
            .map(str::to_owned);
            if terminal.select(
                "Confirm broader file access for future tasks",
                &confirmation,
            )? != Some(1)
            {
                return Ok(false);
            }
            None
        }
        _ => return Ok(false),
    };
    profile.filesystem_scopes = scopes;
    Ok(true)
}

fn configure_command_scopes(terminal: &Terminal, profile: &mut Profile) -> Result<bool> {
    terminal.message(Tone::Quiet, "Command scopes", "Limit commands to exact program/argument pairs. This narrows existing permissions; it does not enable commands or change the approved image. Do not include secrets: this policy is saved in task contracts.")?;
    let choices = [
        "Keep current scopes",
        "Choose exact commands here",
        "Load a reviewed command scopes JSON file",
        "Deny every command",
        "Remove argument limits (existing program grants still apply)",
    ]
    .map(str::to_owned);
    let scopes = match terminal.select("Command authorization", &choices)? {
        Some(1) => {
            let mut commands = Vec::new();
            loop {
                let Some(program) = field(terminal, "  Program name › ", false)? else {
                    return Ok(false);
                };
                terminal.message(Tone::Quiet, "Arguments", "Enter one exact argument at a time. Empty input ends the list. No shell parsing, wildcards or implicit flags.")?;
                let mut args = Vec::new();
                loop {
                    let Some(argument) = field(terminal, "  Argument (Enter to finish) › ", false)?
                    else {
                        return Ok(false);
                    };
                    if argument.is_empty() {
                        break;
                    }
                    args.push(argument);
                    if args.len() > 128 {
                        terminal.message(
                            Tone::Warning,
                            "Scope limit",
                            "At most 128 arguments are allowed. Existing settings are unchanged.",
                        )?;
                        return Ok(false);
                    }
                }
                commands.push(crate::policy::CommandScope { program, args });
                let candidate = crate::policy::CommandScopes {
                    commands: commands.clone(),
                };
                if let Err(error) = candidate.validate() {
                    terminal.message(Tone::Warning, "Invalid scope", &error.to_string())?;
                    return Ok(false);
                }
                let next = ["Add another exact command", "Save these scopes"].map(str::to_owned);
                match terminal.select("Command scopes", &next)? {
                    Some(0) => {}
                    Some(1) => break Some(candidate),
                    _ => return Ok(false),
                }
            }
        }
        Some(2) => {
            let Some(path) = field(terminal, "  Reviewed JSON file › ", false)? else {
                return Ok(false);
            };
            match crate::policy::CommandScopes::from_file(Path::new(&path)) {
                Ok(scopes) => Some(scopes),
                Err(error) => {
                    terminal.message(Tone::Warning, "Invalid scopes", &error.to_string())?;
                    return Ok(false);
                }
            }
        }
        Some(3) => Some(crate::policy::CommandScopes {
            commands: Vec::new(),
        }),
        Some(4) => {
            let confirm = [
                "Keep argument restrictions",
                "Remove argument restrictions for new tasks",
            ]
            .map(str::to_owned);
            if terminal.select("Confirm broader command access", &confirm)? != Some(1) {
                return Ok(false);
            }
            None
        }
        _ => return Ok(false),
    };
    terminal.message(Tone::Accent, "Command scopes", &match &scopes {
        Some(scopes) => format!("{} exact commands approved for new tasks; other arguments are rejected before execution.", scopes.commands.len()),
        None => "Argument restrictions removed for new tasks; approved program grants and container isolation still apply.".into(),
    })?;
    profile.command_scopes = scopes;
    Ok(true)
}

fn configure_appearance(root: &Path, terminal: &mut Terminal, profile: &Profile) -> Result<()> {
    let choices = [
        "Mint + Pip · tiny shield sidekick",
        "Midnight + Byte · little robot",
        "Solar + Orbit · pocket star",
        "Calm · no mascot or motion",
        "Load your own style file",
        "Back",
    ]
    .map(str::to_owned);
    let style = match terminal.select("Make Aegis yours", &choices)? {
        Some(index @ 0..=3) => crate::ui::UiOptions::preset(index),
        Some(4) => {
            let Some(path) = field(terminal, "  Style file › ", false)? else {
                return Ok(());
            };
            match crate::ui::UiOptions::from_file(Path::new(&path)) {
                Ok(style) => style,
                Err(error) => {
                    return terminal.message(
                        Tone::Quiet,
                        "Style kept",
                        &format!(
                            "Couldn't load that style; your current look is unchanged. {}",
                            error
                        ),
                    );
                }
            }
        }
        _ => return Ok(()),
    };
    style.save(root)?;
    terminal.apply_ui(style)?;
    terminal.welcome(
        name(&profile.provider),
        &std::env::current_dir()?.display().to_string(),
    )?;
    terminal.message(
        Tone::Success,
        "Looking good",
        "Saved for this workspace. Permissions, models and task history are unchanged.",
    )
}

fn remember(root: &Path, terminal: &Terminal, text: &str, replace: Option<&str>) -> Result<()> {
    match Store::open(root)?.remember(&std::env::current_dir()?, text, replace) {
        Ok(_) => terminal.message(Tone::Success, "Remembered", "Saved for future tasks in this workspace. No model call needed; existing tasks keep their original memory snapshot."),
        Err(error) => terminal.message(Tone::Quiet, "Memory unchanged", &error.to_string()),
    }
}

fn instruction_menu(root: &Path, terminal: &Terminal) -> Result<()> {
    let workspace = std::env::current_dir()?;
    let mut store = Store::open(root)?;
    let rules = store.project_instructions(&workspace)?;
    terminal.message(Tone::Quiet, "Pinned instructions", "Explicit project rules, not inferred memories. Frozen into new tasks and retained across recovery. Eight rules / 3 KiB. Current requests and permissions take precedence; no rule can grant access.")?;
    let mut choices = vec!["Add an instruction".to_owned()];
    choices.extend(rules.iter().map(|rule| {
        format!(
            "{} · v{} · {}",
            rule.scope,
            rule.revision,
            crate::terminal::fit(&rule.text, 70)
        )
    }));
    choices.push("Review repository guidance · AGENTS.md / CLAUDE.md".into());
    choices.push("Back".into());
    let Some(selected) = terminal.select("Project instructions", &choices)? else {
        return Ok(());
    };
    if selected == rules.len() + 1 {
        return repository_menu(root, terminal);
    }
    let existing = selected.checked_sub(1).and_then(|index| rules.get(index));
    if selected != 0 && existing.is_none() {
        return Ok(());
    }
    if let Some(rule) = existing {
        terminal.message(
            Tone::Accent,
            &format!("{} · v{}", rule.scope, rule.revision),
            &rule.text,
        )?;
        let actions = ["Keep", "Edit instruction", "Remove instruction"].map(str::to_owned);
        match terminal.select("Explicit user rule", &actions)? {
            Some(1) => {}
            Some(2) => {
                store.unpin_instruction(&workspace, &rule.id)?;
                return terminal.message(Tone::Quiet, "Instruction removed", "Future tasks use the updated ledger; saved tasks retain their frozen instructions.");
            }
            _ => return Ok(()),
        }
    }
    let Some(text) = field(terminal, "  Instruction › ", false)? else {
        return Ok(());
    };
    let scopes = ["Whole workspace", "Choose relative file or folder"].map(str::to_owned);
    let scope = match terminal.select("Where does it apply?", &scopes)? {
        Some(0) => "**".to_owned(),
        Some(1) => {
            terminal.message(Tone::Quiet, "Scope", "Use an exact relative file or folder/**. This is guidance, not a permission grant.")?;
            let Some(scope) = field(terminal, "  Scope › ", false)? else {
                return Ok(());
            };
            scope
        }
        _ => return Ok(()),
    };
    match store.pin_instruction(
        &workspace,
        &scope,
        &text,
        existing.map(|rule| rule.id.as_str()),
    ) {
        Ok(_) => terminal.message(
            Tone::Success,
            "Instruction pinned",
            "Saved without a model call. Existing tasks retain their original revisions.",
        ),
        Err(error) => terminal.message(Tone::Warning, "Instruction unchanged", &error.to_string()),
    }
}

fn review_repository_file(
    store: &mut Store,
    workspace: &Path,
    terminal: &Terminal,
    path: &str,
) -> Result<()> {
    let candidate = match crate::repository_rules::read(workspace, path) {
        Ok(candidate) => candidate,
        Err(error) => {
            return terminal.message(Tone::Warning, "Guidance not imported", &error.to_string());
        }
    };
    terminal.message(
        Tone::Accent,
        &format!("{} · scope {}", candidate.path, candidate.scope),
        &candidate.text,
    )?;
    terminal.message(Tone::Quiet, "Review before trusting", "This full file will become task guidance, not a permission grant. Changed content needs another review. Eight files / 16 KiB total; no silent truncation.")?;
    let choices = [
        "Ignore this version",
        "Approve this exact content",
        "Back without saving",
    ]
    .map(str::to_owned);
    match terminal.select("Trust repository guidance?", &choices)? {
        Some(choice @ (0 | 1)) => {
            match store.review_repository_rule(workspace, &candidate, choice == 1) {
                Ok(()) => terminal.message(Tone::Success, if choice == 1 { "Guidance approved" } else { "Guidance ignored" }, "Saved locally without a model call. Existing tasks keep their original guidance."),
                Err(error) => terminal.message(Tone::Warning, "Review not saved", &error.to_string()),
            }
        }
        _ => Ok(()),
    }
}

fn repository_menu(root: &Path, terminal: &Terminal) -> Result<()> {
    let workspace = std::env::current_dir()?;
    let mut store = Store::open(root)?;
    let reviews = store.repository_reviews(&workspace)?;
    let mut paths: Vec<String> = reviews.iter().map(|rule| rule.path.clone()).collect();
    for path in ["AGENTS.md", "CLAUDE.md"] {
        if workspace.join(path).exists() && !paths.iter().any(|existing| existing == path) {
            paths.push(path.into());
        }
    }
    let mut choices: Vec<_> = paths
        .iter()
        .map(|path| {
            let status =
                reviews
                    .iter()
                    .find(|rule| &rule.path == path)
                    .map_or("not reviewed", |rule| {
                        if !rule.approved {
                            "ignored"
                        } else if crate::repository_rules::read(&workspace, path)
                            .is_ok_and(|current| current.sha256 == rule.sha256)
                        {
                            "approved"
                        } else {
                            "needs review"
                        }
                    });
            format!("{path} · {status}")
        })
        .collect();
    choices.extend(["Choose a nested guidance file".into(), "Back".into()]);
    let Some(selected) = terminal.select(
        "Repository guidance · full content, explicit trust",
        &choices,
    )?
    else {
        return Ok(());
    };
    if selected == paths.len() {
        let Some(path) = field(terminal, "  Relative AGENTS.md / CLAUDE.md path › ", false)?
        else {
            return Ok(());
        };
        return review_repository_file(&mut store, &workspace, terminal, &path);
    }
    let Some(path) = paths.get(selected) else {
        return Ok(());
    };
    if reviews.iter().any(|rule| &rule.path == path) {
        let actions = ["Review current content", "Remove saved review", "Back"].map(str::to_owned);
        match terminal.select(path, &actions)? {
            Some(0) => {}
            Some(1) => {
                store.remove_repository_review(&workspace, path)?;
                return terminal.message(Tone::Quiet, "Review removed", "Saved tasks are unchanged; this source is no longer approved for future tasks.");
            }
            _ => return Ok(()),
        }
    }
    review_repository_file(&mut store, &workspace, terminal, path)
}

fn review_new_root_guidance(
    root: &Path,
    terminal: &Terminal,
    scopes: Option<&crate::filesystem::FileScopes>,
) -> Result<()> {
    let workspace = std::env::current_dir()?;
    let mut store = Store::open(root)?;
    let reviews = store.repository_reviews(&workspace)?;
    for path in ["AGENTS.md", "CLAUDE.md"] {
        if scopes.is_some_and(|scopes| !scopes.permits(path, false)) {
            continue;
        }
        let candidate = match crate::repository_rules::read(&workspace, path) {
            Ok(candidate) => candidate,
            Err(error) => {
                if workspace.join(path).exists() {
                    terminal.message(
                        Tone::Warning,
                        "Guidance not imported",
                        &format!("{path}: {error}"),
                    )?;
                }
                continue;
            }
        };
        if !reviews
            .iter()
            .any(|rule| rule.path == path && rule.sha256 == candidate.sha256)
        {
            review_repository_file(&mut store, &workspace, terminal, path)?;
        }
    }
    Ok(())
}

fn memory_menu(root: &Path, terminal: &Terminal) -> Result<()> {
    let workspace = std::env::current_dir()?;
    let mut store = Store::open(root)?;
    let notes = store.project_memory(&workspace)?;
    terminal.message(Tone::Quiet, "Project memory", "Small, durable notes you control. Say 'Remember: use pnpm for this project' to save one directly. Up to 16 notes / 4 KiB; never store credentials.")?;
    let mut choices = vec!["Add a memory".to_owned()];
    choices.extend(
        notes
            .iter()
            .map(|note| crate::terminal::fit(&note.text, 100)),
    );
    choices.push("Habit and workflow learning · inspect / pause / reset".into());
    choices.push("Back".into());
    let Some(choice) = terminal.select("What should Aegis remember?", &choices)? else {
        return Ok(());
    };
    if choice == 0 {
        if let Some(text) = field(terminal, "  Remember › ", false)? {
            remember(root, terminal, &text, None)?;
        }
    } else if choice == notes.len() + 1 {
        terminal.message(Tone::Accent, "Habit and workflow learning", &format!("{} verified experiences saved. Learning is {}. Repeated user preferences teach tentative habits; independently accepted tasks teach tool paths. No extra model calls, transcripts or command arguments.", store.learning_count(&workspace)?, if store.learning_enabled(&workspace)? {"on"} else {"paused"}))?;
        for habit in store.habits(&workspace)? {
            terminal.message(
                Tone::Quiet,
                &format!(
                    "{} · {}",
                    habit.category,
                    if habit.confirmed {
                        "confirmed"
                    } else if habit.observations >= 2 {
                        "learned"
                    } else {
                        "tentative"
                    }
                ),
                &habit.preference,
            )?;
        }
        for (steps, count) in store.learning_paths(&workspace)? {
            terminal.message(
                Tone::Quiet,
                &format!("{count} accepted"),
                &steps
                    .iter()
                    .map(|step| format!("{} v{}", step.capability, step.version))
                    .collect::<Vec<_>>()
                    .join(" → "),
            )?;
        }
        let actions = [
            "Keep",
            "Pause learning",
            "Enable learning",
            "Reset learned experiences",
            "Review or correct a habit",
        ]
        .map(str::to_owned);
        match terminal.select("Learning controls", &actions)? {
            Some(1) => store.set_learning(&workspace, false)?,
            Some(2) => store.set_learning(&workspace, true)?,
            Some(3) => {
                let confirmation = ["Keep experiences", "Reset future learning"].map(str::to_owned);
                if terminal.select(
                    "Existing task snapshots and audit records remain saved",
                    &confirmation,
                )? == Some(1)
                {
                    store.reset_learning(&workspace)?;
                }
            }
            Some(4) => review_habits(&mut store, &workspace, terminal)?,
            _ => {}
        }
    } else if let Some(note) = notes.get(choice - 1) {
        terminal.message(Tone::Accent, "Memory", &note.text)?;
        let actions = ["Keep", "Edit this memory", "Forget this memory"].map(str::to_owned);
        match terminal.select("This memory", &actions)? {
            Some(1) => {
                if let Some(text) = field(terminal, "  Replacement › ", false)? {
                    remember(root, terminal, &text, Some(&note.id))?;
                }
            }
            Some(2) => {
                store.forget(&workspace, &note.id)?;
                terminal.message(Tone::Quiet, "Forgotten", "Removed from future tasks. Existing task contracts retain their original snapshots.")?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn review_habits(store: &mut Store, workspace: &Path, terminal: &Terminal) -> Result<()> {
    let habits = store.habits(workspace)?;
    if habits.is_empty() {
        return terminal.message(Tone::Quiet,"Habits","Nothing inferred yet. Repeated preferences in your normal requests can teach Aegis; you do not need to configure them first.");
    }
    let mut choices: Vec<_> = habits
        .iter()
        .map(|habit| format!("{} · {}", habit.category, habit.choice))
        .collect();
    choices.push("Back".into());
    let Some(selected) = terminal.select("Which habit?", &choices)? else {
        return Ok(());
    };
    let Some(habit) = habits.get(selected) else {
        return Ok(());
    };
    let actions = [
        "Keep",
        "Confirm this preference",
        "Change this preference",
        "Forget this preference",
    ]
    .map(str::to_owned);
    match terminal.select("You control what Aegis learns", &actions)? {
        Some(1) => store.confirm_habit(workspace, &habit.category, &habit.choice)?,
        Some(2) => {
            let alternatives = crate::habits::choices(&habit.category);
            let mut labels: Vec<_> = alternatives
                .iter()
                .map(|(_, description)| description.clone())
                .collect();
            labels.push("Back".into());
            if let Some(index) = terminal.select("Preferred habit", &labels)? {
                if let Some((choice, _)) = alternatives.get(index) {
                    store.confirm_habit(workspace, &habit.category, choice)?;
                }
            }
        }
        Some(3) => store.forget_habit(workspace, &habit.category)?,
        _ => {}
    }
    terminal.message(
        Tone::Quiet,
        "Habit updated",
        "Applies to future tasks; current instructions always take priority.",
    )
}

fn help(terminal: &Terminal) -> Result<()> {
    terminal.message(Tone::Accent, "How to use Aegis", "Describe an outcome in plain language. The agent discovers tools, edits permitted files, runs approved isolated commands and saves evidence automatically.")?;
    terminal.message(
        Tone::Quiet,
        "Choose",
        "F2 provider · F4 sign in · F6 searchable model picker · F7 permissions and budgets",
    )?;
    terminal.message(
        Tone::Quiet,
        "Continue",
        "F3 saved tasks and recovery · F5 new conversation · F8 checkpoint · Up recalls previous requests",
    )?;
    terminal.message(
        Tone::Quiet,
        "Task contract",
        "/goal then pasted task text starts a task · /goal (/contract) views it · /goal add · /goal replace O2 · /goal history",
    )?;
    terminal.message(
        Tone::Quiet,
        "Inspect",
        "/status · /why · /evidence O3 · /verify · /provider history · /budget · /handoff",
    )?;
    terminal.message(Tone::Quiet, "Pause", "Type / while following a task to inspect it or request /pause. /resume continues the selected task. F2 or /providers selects a provider for new tasks.")?;
    terminal.message(Tone::Quiet,"Commands","Type / to open all shortcuts; search or scroll, select to fill the prompt, then Enter to run.")?;
    terminal.message(Tone::Quiet, "Control", "Ctrl+C interrupts an operation; twice quickly interrupts the model turn. Ctrl+D detaches. F9 asks before cancelling the entire task. No shell commands needed.")
}

fn checkpoint_view(root: &Path, id: Option<&str>, terminal: &Terminal) -> Result<()> {
    let Some(id) = id else {
        return terminal.message(Tone::Quiet, "Checkpoint", "No current task yet.");
    };
    let store = Store::open(root)?;
    let Some(checkpoint) = store.last_checkpoint(id)? else {
        return terminal.message(Tone::Quiet, "Checkpoint", "No structured handoff saved yet. Runtime events and completed tool evidence are still durable.");
    };
    terminal.message(Tone::Accent, "Checkpoint", &checkpoint.next_action)?;
    for decision in checkpoint.decisions {
        terminal.message(Tone::Quiet, "Decision", &decision)?;
    }
    for question in checkpoint.unresolved {
        terminal.message(Tone::Warning, "Unresolved", &question)?;
    }
    for milestone in store.milestones(id)? {
        terminal.message(Tone::Quiet, &milestone.state, &milestone.title)?;
    }
    Ok(())
}

fn cancel_task(root: &Path, id: Option<&str>, terminal: &Terminal) -> Result<()> {
    let Some(id) = id else {
        return terminal.message(Tone::Quiet, "Cancel", "No current task yet.");
    };
    let mut store = Store::open(root)?;
    if store.run(id)?.is_terminal() {
        return terminal.message(Tone::Quiet, "Cancel", "This task has already ended.");
    }
    terminal.message(Tone::Warning, "Cancel whole task?", "The worker/model will stop. Completed edits are not undone; uncertain effects still require reconciliation.")?;
    let choices = ["Keep the task", "Cancel the entire task"].map(str::to_owned);
    if terminal.select("Confirm cancellation", &choices)? == Some(1) {
        store.state(id, "cancelled", json!({"source":"terminal_cancel"}))?;
        terminal.message(
            Tone::Warning,
            "Cancelled",
            "Task stopped. Its events, evidence and any uncertain operation outcomes remain saved.",
        )?;
    }
    Ok(())
}

fn download_image(terminal: &Terminal, docker: &Path) -> Result<Option<String>> {
    terminal.message(Tone::Warning, "Container setup", "No local images are available. Downloading an image uses your network and disk space; task commands themselves remain network-disabled.")?;
    let choices = [
        "Download node:22-alpine for Node/npm tasks",
        "Download a different image",
        "Continue without command execution",
    ]
    .map(str::to_owned);
    let Some(choice) = terminal.select("Prepare an isolated command environment", &choices)? else {
        return Ok(None);
    };
    let image = match choice {
        0 => "node:22-alpine".to_owned(),
        1 => {
            let Some(image) = field(terminal, "  Container image reference › ", false)? else {
                return Ok(None);
            };
            if !valid_image_reference(&image) {
                terminal.message(Tone::Warning, "!", "Use a nonempty container image reference, not command options or shell syntax.")?;
                return Ok(None);
            }
            image
        }
        _ => return Ok(None),
    };
    let status = Command::new(docker)
        .args(["pull", "--"])
        .arg(&image)
        .status()?;
    if !status.success() {
        terminal.message(
            Tone::Warning,
            "Download failed",
            "File edits remain enabled. Reopen provider setup to try again.",
        )?;
        return Ok(None);
    }
    terminal.message(Tone::Success, "Container ready", &image)?;
    Ok(Some(image))
}

fn configure_limits(terminal: &Terminal) -> Result<Option<crate::budget::Limits>> {
    let choices = [
        "Standard — 4 hours, 200 turns, 800k tokens, 10-minute commands",
        "Quick — 1 hour, 80 turns, 800k tokens, 1-minute commands",
        "Custom limits",
    ]
    .map(str::to_owned);
    let Some(choice) = terminal.select("Task budget", &choices)? else {
        return Ok(None);
    };
    let mut limits = if choice == 1 {
        crate::budget::Limits::quick()
    } else {
        crate::budget::Limits::default()
    };
    if choice == 2 {
        terminal.message(Tone::Quiet, "Custom budget", "Deadlines apply. Model tokens are counted after each turn; tool context is reserved before calls. Press Enter to keep a default.")?;
        for (label, value) in [
            ("Action limit", &mut limits.actions),
            ("Model token limit", &mut limits.model_tokens),
            (
                "Tool-context token limit (o200k_base)",
                &mut limits.tool_result_tokens,
            ),
            ("Task duration in seconds", &mut limits.wall_seconds),
            ("Command deadline in seconds", &mut limits.process_seconds),
            ("Model-turn deadline in seconds", &mut limits.model_seconds),
            ("Context characters", &mut limits.context_chars),
            (
                "Model response capture bytes",
                &mut limits.model_response_bytes,
            ),
        ] {
            let Some(input) = field(terminal, &format!("  {label} [{}] › ", *value), false)?
            else {
                return Ok(None);
            };
            if !input.trim().is_empty() {
                match input.trim().parse() {
                    Ok(parsed) => *value = parsed,
                    Err(_) => {
                        terminal.message(
                            Tone::Warning,
                            "Invalid budget",
                            "Use a positive whole number.",
                        )?;
                        return Ok(None);
                    }
                }
            }
        }
    }
    if let Err(error) = limits.validate() {
        terminal.message(Tone::Warning, "Invalid budget", &error.to_string())?;
        return Ok(None);
    }
    Ok(Some(limits))
}

fn valid_image_reference(image: &str) -> bool {
    !image.is_empty()
        && image.len() <= 256
        && !image.starts_with('-')
        && image
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._/:@-".contains(&byte))
}

fn ensure_provider(terminal: &Terminal, name: &str) -> Result<bool> {
    crate::signin::ensure(name, terminal)
}
fn save(root: &Path, profile: &Profile) -> Result<()> {
    let path = root.join("profile.json");
    let mut temporary = tempfile::NamedTempFile::new_in(root)?;
    temporary.write_all(&serde_json::to_vec_pretty(profile)?)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn remember_selection(terminal: &Terminal, profile: &Profile) -> Result<()> {
    if !terminal.interactive {
        return Ok(());
    }
    let Some(model) = &profile.model else {
        return Ok(());
    };
    let selection = Selection {
        provider: profile.provider.clone(),
        model: model.clone(),
        reasoning_effort: profile.reasoning_effort.clone(),
    };
    if selection.validate().is_err() {
        return Ok(());
    }
    if SelectionStore::user()
        .and_then(|store| store.save(&selection))
        .is_err()
    {
        terminal.message(
            Tone::Quiet,
            "Preferences",
            "This account/model shortcut could not be saved. Workspace settings are unchanged.",
        )?;
    }
    Ok(())
}

fn secret_reference(profile: &Profile) -> Option<&str> {
    profile
        .api_key_env
        .as_deref()
        .or_else(|| {
            profile
                .endpoint
                .as_ref()
                .and_then(|endpoint| endpoint.api_key_env.as_deref())
        })
        .or_else(|| {
            profile
                .fallback_routes
                .iter()
                .find_map(|route| route.endpoint.as_ref()?.api_key_env.as_deref())
        })
}

fn sign_in_for_selection(
    root: &Path,
    terminal: &Terminal,
    profile: &mut Profile,
    secret: &mut Option<String>,
    task_secret: &mut Option<TaskSecret>,
) -> Result<()> {
    let current = if let Some(id) = profile.previous_run.as_deref() {
        let store = Store::open(root)?;
        match store.run(id) {
            Ok(run) if !run.is_terminal() => Some((id.to_owned(), store.current_route(id)?)),
            _ => None,
        }
    } else {
        None
    };
    let current_route = current.as_ref().map(|(_, route)| route);
    let current_selected = if let Some(route) = current_route.as_ref().filter(|route| {
        route.provider != profile.provider
            || route.provider == "claude-api"
            || (route.provider == "custom" && route.endpoint != profile.endpoint)
    }) {
        let choices = [
            format!("Current task · {} / {}", name(&route.provider), route.model),
            format!("New tasks · {}", name(&profile.provider)),
            "Back".into(),
        ];
        match terminal.select("Sign in for which task?", &choices)? {
            Some(0) => true,
            Some(1) => false,
            _ => return Ok(()),
        }
    } else {
        false
    };
    let selected = if current_selected {
        &current_route.unwrap().provider
    } else {
        &profile.provider
    };
    if selected == "claude-api" {
        let Some(key) = field(terminal, "  Claude API key (hidden) › ", true)? else {
            return Ok(());
        };
        if let Err(error) = crate::claude_api::validate_key(&key) {
            terminal.message(Tone::Warning, "Key unchanged", &error.to_string())?;
            return Ok(());
        }
        if current_selected {
            let (run_id, route) = current.as_ref().unwrap();
            let run = Store::open(root)?.run(run_id)?;
            let Some(target) = route_secret_target(&run, route) else {
                return Ok(());
            };
            *task_secret = Some(TaskSecret {
                run_id: run_id.clone(),
                target,
                value: key,
            });
        } else {
            profile.api_key_env = Some("ARUN_SESSION_API_KEY".into());
            *secret = Some(key);
            save(root, profile)?;
        }
    } else if selected == "custom" {
        if current_selected {
            let (run_id, route) = current.as_ref().unwrap();
            let endpoint = route.endpoint.as_ref().unwrap();
            if endpoint.api_key_env.is_none() {
                terminal.message(Tone::Quiet, "Endpoint", "This task's custom route was approved without a key. New tasks can review a different endpoint in F7.")?;
                return Ok(());
            }
            if let Some(key) = field(terminal, "  Key for this task (hidden) › ", true)? {
                if key.trim().is_empty() {
                    terminal.message(
                        Tone::Warning,
                        "Key unchanged",
                        "This task's fallback needs a nonempty API key.",
                    )?;
                } else {
                    *task_secret = Some(TaskSecret {
                        run_id: run_id.clone(),
                        target: endpoint.base_url.clone(),
                        value: key,
                    });
                }
            }
        } else {
            let Some(key) = field(terminal, "  API key (hidden) › ", true)? else {
                return Ok(());
            };
            *secret = (!key.trim().is_empty()).then_some(key);
            if let Some(endpoint) = &mut profile.endpoint {
                endpoint.api_key_env = secret
                    .as_ref()
                    .filter(|key| !key.trim().is_empty())
                    .map(|_| "ARUN_SESSION_API_KEY".into());
            }
            save(root, profile)?;
        }
    } else if let Err(error) = crate::signin::run(selected, terminal) {
        terminal.message(Tone::Warning, "!", &error.to_string())?;
    }
    Ok(())
}

fn run_secret_reference<'a>(profile: &Profile, run: &'a crate::storage::Run) -> Option<&'a str> {
    let endpoint = profile.endpoint.as_ref().or_else(|| {
        profile
            .fallback_routes
            .iter()
            .find_map(|route| route.endpoint.as_ref())
    })?;
    let matches =
        |value: &serde_json::Value| value["base_url"].as_str() == Some(endpoint.base_url.as_str());
    let approved = if matches(&run.budgets["endpoint"]) {
        Some(&run.budgets["endpoint"])
    } else {
        run.budgets["fallback_routes"]
            .as_array()?
            .iter()
            .map(|route| &route["endpoint"])
            .find(|route| matches(route))
    }?;
    approved["api_key_env"].as_str()
}

#[derive(Default)]
struct InterruptKeys {
    previous: Option<Instant>,
}

impl InterruptKeys {
    fn next(&mut self, now: Instant) -> crate::interrupt::Scope {
        if self.previous.is_some_and(|previous| {
            now.saturating_duration_since(previous) <= Duration::from_millis(900)
        }) {
            self.previous = None;
            crate::interrupt::Scope::Model
        } else {
            self.previous = Some(now);
            crate::interrupt::Scope::Operation
        }
    }
}

fn follow(root: &Path, id: &str, terminal: &mut Terminal) -> Result<()> {
    let _raw = RawMode::enter(terminal.interactive)?;
    let mut store = Store::open(root)?;
    let mut sequence = store
        .recent_events(id, 64)?
        .first()
        .map(|event| event.seq - 1)
        .unwrap_or(0);
    let mut interrupts = InterruptKeys::default();
    let started = Instant::now();
    let mut phase = "Starting task".to_owned();
    let mut context_status = crate::terminal::ContextStatus::default();
    loop {
        let events = store.events_since(id, sequence)?;
        if !events.is_empty() {
            terminal.clear_activity()?;
            context_status.operations = store.event_count(id, "operation.pending")? as u64;
            context_status.artifacts = store.evidence_artifacts(id)?.len();
        }
        for event in events {
            match event.kind.as_str() {
                "model.started" => {
                    phase = "Thinking".into();
                    context_status.prompt_chars = event.payload["prompt_chars"].as_u64();
                    context_status.normalized_tokens =
                        if event.payload["context_tokenizer"] == crate::tokenization::ENCODING {
                            event.payload["raw_prompt_tokens"].as_u64()
                        } else {
                            None
                        };
                    context_status.schema_count = event.payload["schema_count"].as_u64();
                }
                "checkpoint.created" => context_status.checkpoint_at = Some(event.created_at),
                "acceptance.started" => phase = "Verifying acceptance".into(),
                "operation.pending" => {
                    phase = format!(
                        "Running {}",
                        event.payload["capability"].as_str().unwrap_or("tool")
                    )
                }
                "operation.succeeded" => phase = "Reviewing evidence".into(),
                _ => {}
            }
            terminal.render_event(&event)?;
            sequence = event.seq;
        }
        let run = store.run(id)?;
        if !matches!(run.state.as_str(), "ready" | "running") {
            terminal.clear_activity()?;
            for event in store.events_since(id, sequence)? {
                terminal.render_event(&event)?;
            }
            terminal.message(
                Tone::Quiet,
                "",
                &format!(
                    "{} model tokens · {}",
                    store.model_tokens(id)?,
                    chat_state(&run.state)
                ),
            )?;
            return Ok(());
        }
        if started.elapsed() > Duration::from_secs(3) && !kernel::is_active(root, id)? {
            terminal.clear_activity()?;
            terminal.message(
                Tone::Warning,
                "Interrupted",
                "The runner stopped. Open sessions to resume safely.",
            )?;
            return Ok(());
        }
        terminal.activity(
            &phase,
            started.elapsed(),
            store.model_tokens(id)?,
            &context_status,
        )?;
        if terminal.interactive && event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Release {
                    continue;
                }
                if key.code == KeyCode::Char('/') && !key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    terminal.clear_activity()?;
                    let selected = terminal.command_menu()?;
                    if let Some(command) = selected {
                        let mut words = command.split_whitespace();
                        let command = words.next().unwrap_or_default().trim_start_matches('/');
                        if command == "pause" {
                            crate::pause::request(root, id)?;
                            terminal.message(
                                Tone::Quiet,
                                "Pause requested",
                                "Waiting for the current action's safe boundary.",
                            )?;
                        } else if crate::control::VIEWS.contains(&command) {
                            match crate::control::view(&store, id, command, words.next()) {
                                Ok(value) => terminal.message(
                                    Tone::Quiet,
                                    command,
                                    &crate::control::display(command, &value),
                                )?,
                                Err(error) => terminal.message(
                                    Tone::Warning,
                                    "View unavailable",
                                    &error.to_string(),
                                )?,
                            }
                        } else {
                            terminal.message(Tone::Quiet,"Task controls","/goal /status /why /evidence O3 /verify /provider /budget /handoff /pause")?;
                        }
                    }
                }
                if key.code == KeyCode::F(8) {
                    terminal.clear_activity()?;
                    checkpoint_view(root, Some(id), terminal)?;
                }
                if key.code == KeyCode::F(9) {
                    terminal.clear_activity()?;
                    cancel_task(root, Some(id), terminal)?;
                }
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    match key.code {
                        KeyCode::Char('d') => {
                            terminal.clear_activity()?;
                            terminal.message(
                                Tone::Quiet,
                                "Detached",
                                "Your task keeps running. Reopen sessions to follow it.",
                            )?;
                            return Ok(());
                        }
                        KeyCode::Char('c') => {
                            let scope = interrupts.next(Instant::now());
                            terminal.clear_activity()?;
                            match store.request_interrupt(id, scope) {
                                Ok(Some(_)) => terminal.message(Tone::Warning, "Interrupt", match scope {
                                    crate::interrupt::Scope::Operation => "Stopping this operation; the task itself remains available.",
                                    crate::interrupt::Scope::Model => "Interrupting this model turn; you can resume the task later.",
                                })?,
                                Ok(None) => terminal.message(Tone::Quiet, "Interrupt", match scope {
                                    crate::interrupt::Scope::Operation => "No active operation. Press Ctrl+C again quickly to interrupt the model turn.",
                                    crate::interrupt::Scope::Model => "No active model turn. Ctrl+D detaches; use the task menu to cancel the whole task.",
                                })?,
                                Err(error) => terminal.message(Tone::Warning, "Interrupt", &error.to_string())?,
                            }
                        }
                        _ => {}
                    }
                }
            }
        } else if !terminal.interactive {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn continue_legacy(
    root: &Path,
    id: &str,
    terminal: &mut Terminal,
    profile: &mut Profile,
) -> Result<()> {
    let mut store = Store::open(root)?;
    if kernel::is_active(root, id)? {
        return terminal.message(
            Tone::Warning,
            "Task still active",
            "Pause the original task before reviewing a new continuation.",
        );
    }
    let source = match crate::continuation::inspect_source(&store, id, &std::env::current_dir()?) {
        Ok(source) => source,
        Err(error) => {
            return terminal.message(Tone::Warning, "Original task kept", &error.to_string());
        }
    };
    terminal.message(Tone::Accent, "Continue directly", "This older task used a native provider transport. Aegis can create a reviewed new task without running that provider's CLI.")?;
    let choices = ["ChatGPT", "Grok", "Back · keep original task"].map(str::to_owned);
    let Some(choice @ (0 | 1)) = terminal.select_at(
        "Connection for the new task",
        &choices,
        usize::from(profile.provider == "grok"),
    )?
    else {
        return Ok(());
    };
    let mut draft = profile.clone();
    draft.provider = if choice == 0 { "codex" } else { "grok" }.into();
    draft.endpoint = None;
    draft.model = if crate::provider::canonical(&source.provider) == draft.provider {
        source.budgets["model"]
            .as_str()
            .filter(|id| crate::catalog::valid_id(id))
            .map(str::to_owned)
    } else if profile.provider == draft.provider {
        profile.model.clone()
    } else {
        None
    };
    draft.reasoning_effort = None;
    let Some(Some(model)) = choose_model(terminal, &draft, None)? else {
        return Ok(());
    };
    draft.model = Some(model.clone());
    let Some(reasoning) = choose_reasoning(terminal, &draft, None, false)? else {
        return Ok(());
    };
    draft.reasoning_effort = reasoning.clone();
    let review = match crate::continuation::prepare(
        &store,
        id,
        &std::env::current_dir()?,
        &draft.provider,
        &model,
        reasoning.as_deref(),
    ) {
        Ok(review) => review,
        Err(error) => {
            return terminal.message(Tone::Warning, "Original task kept", &error.to_string());
        }
    };
    terminal.message(
        Tone::Accent,
        "New connection",
        &format!(
            "{} · {} · reasoning {}",
            name(&draft.provider),
            model,
            reasoning.as_deref().unwrap_or("provider default")
        ),
    )?;
    terminal.message(Tone::Quiet, "Workspace", &source.workspace)?;
    let permissions = serde_json::from_value::<Vec<String>>(source.grants.clone())?;
    terminal.message(
        Tone::Quiet,
        "Same permissions",
        &if permissions.is_empty() {
            "Conversation only · no workspace tools".into()
        } else {
            permissions.join(" · ")
        },
    )?;
    terminal.message(Tone::Quiet, "Frozen guidance", "Original rules, memory, command/file/network scopes and acceptance checks are retained. No current settings or new permissions are substituted.")?;
    let limits = ["actions", "model_tokens", "wall_seconds"].map(|key| {
        source.budgets[key]
            .as_u64()
            .map(|value| value.to_string())
            .unwrap_or_else(|| "runtime default".into())
    });
    terminal.message(Tone::Warning, "Fresh task budget", &format!("Same limits: {} turns · {} tokens · {} seconds. Usage and time restart for this NEW task; the original accounting stays untouched.",limits[0],limits[1],limits[2]))?;
    terminal.message(Tone::Quiet, "Fresh evidence", "Old operations, evidence and completed milestones are not copied. Aegis will inspect current files and verify again, not replay old calls.")?;
    if let Some(next) = review.handoff()["next_action"].as_str() {
        terminal.message(Tone::Quiet, "Old next step · context only", next)?;
        if review.handoff()["clipped"] == true {
            terminal.message(
                Tone::Quiet,
                "Handoff preview",
                "Older notes are clipped; saved task details retain the full original handoff.",
            )?;
        }
    }
    let choices = [
        "Create new task and start",
        "Keep original · do not create a task",
        "Inspect original task details",
    ]
    .map(str::to_owned);
    loop {
        match terminal.select_at("Review continuation", &choices, 1)? {
            Some(0) => break,
            Some(2) => context_view(root, id, terminal)?,
            _ => return Ok(()),
        }
    }
    let child = match crate::continuation::commit(&mut store, root, &review) {
        Ok(child) => child,
        Err(error) => {
            return terminal.message(Tone::Warning, "Original task kept", &error.to_string());
        }
    };
    profile.provider = draft.provider;
    profile.model = draft.model;
    profile.reasoning_effort = draft.reasoning_effort;
    profile.endpoint = None;
    profile.previous_run = Some(child.id.clone());
    save(root, profile)?;
    terminal.message(Tone::Success, "Continuation saved", "The original task is unchanged. The new task is in saved chats even if sign-in or startup is cancelled.")?;
    drop(store);
    if !ensure_provider(terminal, &profile.provider)? {
        return Ok(());
    }
    kernel::spawn(root, &child.id, None)?;
    follow(root, &child.id, terminal)
}

fn resume(
    root: &Path,
    id: &str,
    terminal: &mut Terminal,
    secret: Option<(&str, &str)>,
) -> Result<()> {
    let mut store = Store::open(root)?;
    let run = store.run(id)?;
    if run.is_terminal() {
        terminal.message(
            Tone::Warning,
            "!",
            "This task has ended. Start a new task to continue the conversation.",
        )?;
        return Ok(());
    }
    if store.unknown_count(id)? > 0 {
        review_operations(root, id, terminal)?;
    }
    if store.unknown_count(id)? > 0 {
        terminal.message(
            Tone::Warning,
            "Paused",
            "Unverified operation outcomes remain. Aegis will not repeat them automatically.",
        )?;
        return Ok(());
    }
    store.resume_paused(id)?;
    drop(store);
    kernel::spawn(root, id, secret)?;
    follow(root, id, terminal)
}

fn resume_current(
    root: &Path,
    terminal: &mut Terminal,
    profile: &Profile,
    secret: Option<&str>,
    task_secret: &mut Option<TaskSecret>,
) -> Result<()> {
    let Some(id) = profile.previous_run.as_deref() else {
        return terminal.message(
            Tone::Quiet,
            "No current task",
            "Select a saved task with F3.",
        );
    };
    let store = Store::open(root)?;
    let run = store.run(id)?;
    if kernel::is_active(root, id)? {
        return follow(root, id, terminal);
    }
    let route = store.current_route(id)?;
    let profile_credentials = run_secret_reference(profile, &run).zip(secret);
    if route_key_reference(&run, &route).is_some()
        && profile_credentials.is_none()
        && task_secret
            .as_ref()
            .is_none_or(|saved| !saved.matches(&run, &route))
    {
        if let Some(key) = field(terminal, "Key for this saved task (hidden)", true)? {
            if !key.trim().is_empty()
                && (route.provider != "claude-api" || crate::claude_api::validate_key(&key).is_ok())
                && let Some(target) = route_secret_target(&run, &route)
            {
                *task_secret = Some(TaskSecret {
                    run_id: run.id.clone(),
                    target,
                    value: key,
                });
            }
        }
    }
    let credentials = task_secret
        .as_ref()
        .filter(|saved| saved.matches(&run, &route))
        .and_then(|saved| {
            route_key_reference(&run, &route).map(|reference| (reference, saved.value.as_str()))
        })
        .or(profile_credentials);
    if route_key_reference(&run, &route).is_some() && credentials.is_none() {
        return terminal.message(
            Tone::Warning,
            "Key needed",
            "The saved task remains paused.",
        );
    }
    resume(root, id, terminal, credentials)?;
    recover_auth_after_follow(root, id, terminal)
}

fn review_operations(root: &Path, id: &str, terminal: &Terminal) -> Result<()> {
    let mut store = Store::open(root)?;
    if kernel::is_active(root, id)? {
        terminal.message(
            Tone::Warning,
            "!",
            "Stop the active runner before reviewing interrupted operations.",
        )?;
        return Ok(());
    }
    let operations: Vec<_> = store
        .operations(id)?
        .into_iter()
        .filter(|operation| operation.state == "outcome_unknown")
        .collect();
    if operations.is_empty() {
        terminal.message(Tone::Quiet, "Recovery", "No unknown operation outcomes.")?;
        return Ok(());
    }
    let mut choices: Vec<_> = operations
        .iter()
        .map(|operation| {
            format!(
                "{} · {}",
                operation.capability,
                crate::terminal::fit(&operation.arguments.to_string(), 100)
            )
        })
        .collect();
    choices.push("Back — leave outcomes unresolved".into());
    let Some(index) = terminal.select("Choose an interrupted operation", &choices)? else {
        return Ok(());
    };
    let Some(operation) = operations.get(index) else {
        return Ok(());
    };
    terminal.message(Tone::Warning, "Unknown outcome", "The call may already have taken effect. Check the actual workspace or external system before recording a result. Aegis will not re-run it here.")?;
    terminal.message(
        Tone::Quiet,
        &operation.capability,
        &crate::terminal::fit(&operation.arguments.to_string(), 2000),
    )?;
    let choices = [
        "I verified the operation succeeded",
        "I verified the operation failed",
        "Leave unresolved",
    ]
    .map(str::to_owned);
    let Some(outcome) = terminal.select("Record an externally verified outcome", &choices)? else {
        return Ok(());
    };
    if outcome > 1 {
        return Ok(());
    }
    let Some(note) = field(
        terminal,
        "  Verification note or receipt (no credentials) › ",
        false,
    )?
    else {
        return Ok(());
    };
    if note.trim().is_empty() {
        terminal.message(
            Tone::Warning,
            "!",
            "A verification note is required; nothing changed.",
        )?;
        return Ok(());
    }
    store.resolve_unknown(id, &operation.id, outcome == 0, &note)?;
    terminal.message(
        Tone::Success,
        "Recorded",
        "Verification saved durably. No operation was repeated.",
    )
}

fn inspection_number(
    terminal: &Terminal,
    label: &str,
    default: usize,
    minimum: usize,
    maximum: usize,
) -> Result<Option<usize>> {
    loop {
        let Some(value) = field(terminal, label, false)? else {
            return Ok(None);
        };
        let parsed = if value.trim().is_empty() {
            Some(default)
        } else {
            value.trim().parse::<usize>().ok()
        };
        if let Some(number) = parsed.filter(|number| (minimum..=maximum).contains(number)) {
            return Ok(Some(number));
        }
        terminal.message(
            Tone::Warning,
            "Choose a number",
            &format!("Enter a whole number from {minimum} to {maximum}, or Enter for {default}."),
        )?;
    }
}

fn artifacts(root: &Path, id: &str, terminal: &Terminal) -> Result<()> {
    let store = Store::open(root)?;
    let artifacts = store.inspectable_artifacts(id)?;
    if artifacts.is_empty() {
        return terminal.message(
            Tone::Quiet,
            "Artifacts",
            "No recorded operation artifacts yet.",
        );
    }
    let mut choices: Vec<_> = artifacts
        .iter()
        .map(|(label, hash)| format!("{label} · {}", &hash[..12]))
        .collect();
    choices.push("Back".into());
    let Some(index) = terminal.select("Stored evidence", &choices)? else {
        return Ok(());
    };
    let Some((label, hash)) = artifacts.get(index) else {
        return Ok(());
    };
    let bytes = store.artifact(hash)?;
    let modes = [
        "Preview · first 4,000 characters",
        "Find text · literal search",
        "Read lines · choose start and count",
        "Read characters · choose offset and count",
        "Back",
    ]
    .map(str::to_owned);
    loop {
        let query = match terminal.select("Inspect evidence · no tools or model calls", &modes)? {
            Some(0) => String::new(),
            Some(1) => {
                let Some(text) = field(terminal, "  Find text › ", false)? else {
                    continue;
                };
                if text.trim().is_empty() {
                    continue;
                }
                format!("@find {text}")
            }
            Some(mode @ (2 | 3)) => {
                let lines = mode == 2;
                let Some(start) = inspection_number(
                    terminal,
                    if lines {
                        "  First line (Enter: 1) › "
                    } else {
                        "  Character offset (Enter: 0) › "
                    },
                    usize::from(lines),
                    usize::from(lines),
                    usize::MAX,
                )?
                else {
                    continue;
                };
                let Some(count) = inspection_number(
                    terminal,
                    if lines {
                        "  Line count (Enter: 40, max: 100) › "
                    } else {
                        "  Character count (Enter: 2,000, max: 4,000) › "
                    },
                    if lines { 40 } else { 2000 },
                    1,
                    if lines { 100 } else { 4000 },
                )?
                else {
                    continue;
                };
                format!(
                    "{} {start} {count}",
                    if lines { "@lines" } else { "@slice" }
                )
            }
            _ => return Ok(()),
        };
        terminal.message(Tone::Quiet, label, &kernel::inspect(&bytes, &query))?;
    }
}

fn context_view(root: &Path, id: &str, terminal: &Terminal) -> Result<()> {
    let store = Store::open(root)?;
    let run = store.run(id)?;
    terminal.message(Tone::Accent, "Task", &run.task)?;
    if let Ok(route) = store.current_route(id) {
        if route.provider != run.provider {
            terminal.message(
                Tone::Accent,
                "Current model",
                &format!(
                    "{} / {} · switched within this task",
                    name(&route.provider),
                    route.model
                ),
            )?;
        }
    }
    terminal.message(
        Tone::Quiet,
        "Context",
        &format!(
            "{} · {} · {} tokens · {} active capabilities · {} evidence artifacts",
            name(&run.provider),
            run.state,
            store.model_tokens(id)?,
            store.working_capabilities(id)?.len(),
            store.evidence_artifacts(id)?.len()
        ),
    )?;
    let usage = trace::metrics(&store.events(id)?);
    if usage.model_attempts > 0 {
        terminal.message(
            Tone::Quiet,
            "Model usage",
            &format!(
                "{} input ({} reported cached) + {} output tokens · calls {} · estimated {} · unaccounted {}",
                usage.input_tokens,
                usage.cached_input_tokens,
                usage.output_tokens,
                usage.model_attempts,
                usage.estimated_turns,
                usage.unaccounted_model_attempts
            ),
        )?;
        if usage.context_tokenizer.is_some() {
            terminal.message(
                Tone::Quiet,
                "Prompt exposure",
                &format!(
                    "{} normalized units · {} schema · {} tool results{}; not provider billing",
                    usage.raw_prompt_tokens,
                    usage.schema_tokens_total.unwrap_or(0),
                    usage.tool_result_tokens,
                    if usage.unaccounted_context_attempts > 0 {
                        " · partial history"
                    } else {
                        ""
                    }
                ),
            )?;
        }
    }
    let obligations = store.obligations(id)?;
    if obligations.len() > 1 {
        let active = obligations
            .iter()
            .filter(|item| item.id > 0 && item.state != "superseded")
            .count();
        terminal.message(
            Tone::Accent,
            "Obligations",
            &format!(
                "{} user {} · workspace revision {}",
                active,
                if active == 1 {
                    "requirement"
                } else {
                    "requirements"
                },
                store.workspace_revision(id)?.unwrap_or(0)
            ),
        )?;
        for obligation in obligations.iter().skip(1) {
            terminal.message(
                if obligation.state == "verified" {
                    Tone::Success
                } else {
                    Tone::Warning
                },
                &format!("O{} · {}", obligation.id, obligation.state),
                &if let Some(replacement) = obligation.superseded_by {
                    format!(
                        "{} → O{} · {}",
                        obligation.title,
                        replacement,
                        obligation.reason.as_deref().unwrap_or("no reason recorded")
                    )
                } else {
                    format!(
                        "{} · {} evidence",
                        obligation.title,
                        obligation.evidence.len()
                    )
                },
            )?;
        }
    }
    terminal.message(Tone::Quiet, "Acceptance", &run.acceptance)?;
    for rule in crate::instructions::frozen(&run.budgets)? {
        terminal.message(
            Tone::Accent,
            &format!("Rule · {} · v{}", rule.scope, rule.revision),
            &rule.text,
        )?;
    }
    for rule in crate::repository_rules::frozen(&run.budgets)? {
        terminal.message(
            Tone::Accent,
            &format!(
                "Repository · {} · {} · v{} · {}",
                rule.path,
                rule.scope,
                rule.revision,
                &rule.sha256[..12]
            ),
            &rule.text,
        )?;
    }
    if let Some(checkpoint) = store.last_checkpoint(id)? {
        terminal.message(Tone::Accent, "Next action", &checkpoint.next_action)?;
        for decision in checkpoint.decisions {
            terminal.message(Tone::Quiet, "Decision", &decision)?;
        }
        for unresolved in checkpoint.unresolved {
            terminal.message(Tone::Warning, "Unresolved", &unresolved)?;
        }
    }
    Ok(())
}

fn tools_view(root: &Path, id: &str, terminal: &Terminal) -> Result<()> {
    let store = Store::open(root)?;
    let active = store.working_capabilities(id)?;
    if active.is_empty() {
        terminal.message(
            Tone::Quiet,
            "Tools",
            "No capabilities activated yet. Discovery happens automatically when a task needs it.",
        )?;
    }
    for (capability, version) in active {
        terminal.message(Tone::Accent, &capability, &format!("version {version}"))?;
    }
    Ok(())
}

fn sessions(
    root: &Path,
    terminal: &mut Terminal,
    profile: &mut Profile,
    secret: Option<&str>,
    task_secret: &mut Option<TaskSecret>,
) -> Result<()> {
    let store = Store::open(root)?;
    let runs = chat_heads(store.runs()?);
    if runs.is_empty() {
        terminal.message(
            Tone::Quiet,
            "Chats",
            "Your conversations will be saved here automatically.",
        )?;
        return Ok(());
    }
    let selected: Vec<_> = runs.iter().take(100).collect();
    let mut choices: Vec<_> = selected
        .iter()
        .map(|run| {
            format!(
                "{}  ·  {} · {}",
                crate::terminal::fit(&run.task, 50),
                chat_state(&run.state),
                relative_age(run.created_at)
            )
        })
        .collect();
    choices.push("Back".into());
    let Some(index) = terminal.select("Saved chats · type to search", &choices)? else {
        return Ok(());
    };
    let Some(run) = selected.get(index) else {
        return Ok(());
    };
    terminal.message(Tone::Accent, "You", &crate::terminal::fit(&run.task, 2000))?;
    if let Some(summary) = store.run_summary(&run.id)? {
        terminal.message(Tone::Quiet, "Aegis", &crate::terminal::fit(&summary, 800))?;
    } else {
        terminal.message(Tone::Quiet, "Saved", chat_state(&run.state))?;
    }
    let mut choices = [
        "Continue this chat · return to typing",
        "Read saved messages",
        "Inspect evidence artifacts",
        "Task details and recovery",
    ]
    .map(str::to_owned)
    .to_vec();
    if !run.is_terminal() {
        choices.extend(
            [
                "Follow task",
                if crate::continuation::needed(run) {
                    "Continue work as new direct task"
                } else {
                    "Resume task"
                },
                "Cancel task",
            ]
            .map(str::to_owned),
        );
    } else if crate::continuation::needed(run) {
        choices.push("Continue work as new direct task".into());
    }
    choices.push("Back".into());
    match terminal.select("Chat actions", &choices)? {
        Some(0) => {
            profile.previous_run = Some(run.id.clone());
            save(root, profile)?;
            terminal.message(Tone::Accent, "Chat restored", "Your next message continues this conversation. Unfinished tool work is not restarted automatically.")?;
        }
        Some(1) => saved_messages(&store, &run.id, terminal)?,
        Some(2) => artifacts(root, &run.id, terminal)?,
        Some(3) => chat_details(root, run, &store, terminal)?,
        Some(4) if run.is_terminal() && crate::continuation::needed(run) => {
            continue_legacy(root, &run.id, terminal, profile)?;
        }
        Some(4) if !run.is_terminal() => {
            follow(root, &run.id, terminal)?;
            recover_auth_after_follow(root, &run.id, terminal)?;
        }
        Some(5) if !run.is_terminal() => {
            if crate::continuation::needed(run) {
                return continue_legacy(root, &run.id, terminal, profile);
            }
            let current_route = store.current_route(&run.id)?;
            let profile_credentials = run_secret_reference(profile, run).zip(secret);
            if task_secret
                .as_ref()
                .is_none_or(|saved| !saved.matches(run, &current_route))
                && profile_credentials.is_none()
            {
                if route_key_reference(run, &current_route).is_some() {
                    if let Some(key) =
                        field(terminal, "  Key for this saved task (hidden) › ", true)?
                    {
                        if (current_route.provider != "claude-api"
                            || crate::claude_api::validate_key(&key).is_ok())
                            && !key.trim().is_empty()
                            && let Some(target) = route_secret_target(run, &current_route)
                        {
                            *task_secret = Some(TaskSecret {
                                run_id: run.id.clone(),
                                target,
                                value: key,
                            });
                        }
                    }
                }
            }
            let credentials = task_secret
                .as_ref()
                .filter(|saved| saved.matches(run, &current_route))
                .and_then(|saved| {
                    route_key_reference(run, &current_route)
                        .map(|reference| (reference, saved.value.as_str()))
                })
                .or(profile_credentials);
            if route_key_reference(run, &current_route).is_some() && credentials.is_none() {
                terminal.message(Tone::Warning, "Key needed", "This saved task needs its provider key before it can resume. Open F3 again to continue.")?;
                return Ok(());
            }
            resume(root, &run.id, terminal, credentials)?;
            recover_auth_after_follow(root, &run.id, terminal)?;
        }
        Some(6) if !run.is_terminal() => {
            cancel_task(root, Some(&run.id), terminal)?;
        }
        _ => {}
    }
    Ok(())
}

fn chat_details(
    root: &Path,
    run: &crate::storage::Run,
    store: &Store,
    terminal: &mut Terminal,
) -> Result<()> {
    let choices = [
        "View milestones",
        "View trace",
        "View context and handoff",
        "View active tools",
        "Review interrupted operations",
        "Replace a requirement",
        "Back",
    ]
    .map(str::to_owned);
    match terminal.select("Task details", &choices)? {
        Some(0) => {
            for milestone in store.milestones(&run.id)? {
                terminal.message(Tone::Quiet, &milestone.state, &milestone.title)?;
            }
        }
        Some(1) => trace::display(run, &store.events(&run.id)?),
        Some(2) => context_view(root, &run.id, terminal)?,
        Some(3) => tools_view(root, &run.id, terminal)?,
        Some(4) => review_operations(root, &run.id, terminal)?,
        Some(5) => supersede_obligation(root, run, terminal, None)?,
        _ => {}
    }
    Ok(())
}

fn supersede_obligation(
    root: &Path,
    run: &crate::storage::Run,
    terminal: &mut Terminal,
    selected_id: Option<i64>,
) -> Result<()> {
    if run.is_terminal() {
        terminal.message(
            Tone::Warning,
            "Requirements",
            "This task has ended; its contract cannot change.",
        )?;
        return Ok(());
    }
    let mut store = Store::open(root)?;
    let obligations: Vec<_> = store
        .obligations(&run.id)?
        .into_iter()
        .filter(|item| item.id > 0 && item.state != "superseded")
        .collect();
    if obligations.is_empty() {
        terminal.message(
            Tone::Quiet,
            "Requirements",
            "No explicit requirements to replace.",
        )?;
        return Ok(());
    }
    let mut choices: Vec<_> = obligations
        .iter()
        .map(|item| format!("O{} · {}", item.id, item.title))
        .collect();
    choices.push("Back".into());
    let index = if let Some(id) = selected_id {
        let Some(index) = obligations.iter().position(|item| item.id == id) else {
            return terminal.message(
                Tone::Warning,
                "Requirement unavailable",
                "Choose an active explicit obligation such as O2.",
            );
        };
        index
    } else {
        let Some(index) = terminal.select("Replace a requirement", &choices)? else {
            return Ok(());
        };
        index
    };
    let Some(old) = obligations.get(index) else {
        return Ok(());
    };
    let Some(replacement) = field(terminal, "Replacement requirement", false)? else {
        return Ok(());
    };
    let Some(reason) = field(terminal, "Why are you changing the task contract?", false)? else {
        return Ok(());
    };
    let confirmation = [
        format!("Keep O{} unchanged", old.id),
        format!(
            "Approve replacement · {}",
            crate::terminal::fit(&replacement, 80)
        ),
    ];
    terminal.message(
        Tone::Warning,
        "Contract change",
        &format!(
            "O{}: {} → {}\nReason: {}",
            old.id, old.title, replacement, reason
        ),
    )?;
    if terminal.select("Approve this change?", &confirmation)? != Some(1) {
        return Ok(());
    }
    match store.supersede_obligation(&run.id, old.id, &replacement, &reason) {
        Ok(new_id) => terminal.message(
            Tone::Success,
            "Requirement updated",
            &format!("O{} retained; O{} is now open.", old.id, new_id),
        )?,
        Err(error) => {
            terminal.message(Tone::Warning, "Requirement unchanged", &error.to_string())?
        }
    }
    Ok(())
}

fn edit_goal(
    root: &Path,
    id: &str,
    command: &str,
    selected: Option<&str>,
    terminal: &mut Terminal,
) -> Result<()> {
    let mut store = Store::open(root)?;
    let run = store.run(id)?;
    if run.is_terminal() {
        return terminal.message(
            Tone::Warning,
            "Contract retained",
            "Ended tasks cannot change requirements.",
        );
    }
    if kernel::is_active(root, id)? {
        return terminal.message(
            Tone::Warning,
            "Pause first",
            "Use /pause and wait for its safe boundary before reviewing contract changes.",
        );
    }
    if command == "replace" {
        return supersede_obligation(
            root,
            &run,
            terminal,
            selected.map(crate::control::obligation_id).transpose()?,
        );
    }
    let Some(title) = field(terminal, "New requirement", false)? else {
        return Ok(());
    };
    let Some(reason) = field(terminal, "Reason for adding this requirement", false)? else {
        return Ok(());
    };
    terminal.message(
        Tone::Accent,
        "Add requirement",
        &format!("{title}\nReason: {reason}"),
    )?;
    if terminal.select(
        "Approve this addition?",
        &[
            "Keep current contract".into(),
            "Add this requirement".into(),
        ],
    )? != Some(1)
    {
        return Ok(());
    }
    let id = store.add_obligation(id, &title, &reason)?;
    terminal.message(
        Tone::Success,
        "Requirement added",
        &format!("O{id} is open. The original task is retained."),
    )
}

fn review_start_contract(root: &Path, id: &str, terminal: &mut Terminal) -> Result<bool> {
    loop {
        let store = Store::open(root)?;
        let value = crate::control::view(&store, id, "goal", None)?;
        terminal.message(
            Tone::Accent,
            "Detected requirements",
            &crate::control::display("goal", &value),
        )?;
        match terminal.select(
            "Review this task before starting",
            &[
                "Start task".into(),
                "Edit requirements".into(),
                "Add requirement".into(),
                "Leave task paused".into(),
            ],
        )? {
            Some(0) => return Ok(true),
            Some(1) => edit_goal(root, id, "replace", None, terminal)?,
            Some(2) => edit_goal(root, id, "add", None, terminal)?,
            _ => {
                crate::pause::request(root, id)?;
                return Ok(false);
            }
        }
    }
}

fn chat_heads(runs: Vec<crate::storage::Run>) -> Vec<crate::storage::Run> {
    let parents: std::collections::HashSet<_> = runs
        .iter()
        .filter_map(|run| {
            run.budgets["previous_run"]
                .as_str()
                .filter(|parent| *parent != run.id)
                .map(str::to_owned)
        })
        .collect();
    runs.into_iter()
        .filter(|run| !parents.contains(&run.id))
        .collect()
}

fn chat_state(state: &str) -> &str {
    match state {
        "answered" => "replied",
        "completed" => "done",
        "running" => "working",
        "ready" => "ready",
        "waiting_recovery" => "paused",
        "cancelled" => "stopped",
        "failed" => "needs attention",
        _ => state,
    }
}

fn relative_age(created: i64) -> String {
    let seconds = crate::storage::now().saturating_sub(created).max(0) as u64;
    match seconds {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", seconds / 60),
        3600..=86399 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86400),
    }
}

fn saved_messages(store: &Store, id: &str, terminal: &Terminal) -> Result<()> {
    let workspace = store.run(id)?.workspace;
    let mut next = Some(id.to_owned());
    let mut seen = std::collections::HashSet::new();
    while let Some(first) = next.take() {
        let mut turns = Vec::new();
        let mut cursor = Some(first);
        while turns.len() < 50 {
            let Some(id) = cursor.take() else {
                break;
            };
            if !seen.insert(id.clone()) {
                break;
            }
            let run = store.run(&id)?;
            if run.workspace != workspace {
                break;
            }
            cursor = run.budgets["previous_run"].as_str().map(str::to_owned);
            turns.push(run);
        }
        let mut choices: Vec<_> = turns
            .iter()
            .map(|run| {
                format!(
                    "{} · {}",
                    crate::terminal::fit(&run.task, 70),
                    relative_age(run.created_at)
                )
            })
            .collect();
        let older = choices.len();
        if cursor.is_some() {
            choices.push("Earlier messages…".into());
        }
        choices.push("Back".into());
        loop {
            let Some(index) = terminal.select("Saved messages · newest first", &choices)? else {
                return Ok(());
            };
            if let Some(run) = turns.get(index) {
                terminal.message(Tone::Accent, "You", &run.task)?;
                let events = store.events(&run.id)?;
                if let Some(reply) = events
                    .iter()
                    .rev()
                    .find(|event| matches!(event.kind.as_str(), "run.completed" | "run.answered"))
                {
                    terminal.message(
                        Tone::Quiet,
                        "Aegis",
                        reply.payload["summary"].as_str().unwrap_or_default(),
                    )?;
                } else {
                    terminal.message(Tone::Quiet, "Saved", chat_state(&run.state))?;
                }
            } else if index == older && cursor.is_some() {
                next = cursor;
                break;
            } else {
                return Ok(());
            }
        }
    }
    Ok(())
}

fn task(
    root: &Path,
    terminal: &mut Terminal,
    profile: &mut Profile,
    secret: Option<&str>,
    request: &str,
) -> Result<()> {
    profile.limits.validate()?;
    if profile.model.is_none() {
        let Some(model) = choose_model(terminal, profile, secret)? else {
            return Ok(());
        };
        profile.model = model;
        save(root, profile)?;
    }
    review_new_root_guidance(root, terminal, profile.filesystem_scopes.as_ref())?;
    let mut store = Store::open(root)?;
    let mut grants = vec!["workspace.read".to_owned()];
    if profile.write {
        grants.push("workspace.write".into());
    }
    if profile.image.is_some() {
        grants.extend(["process.run".into(), "process:*".into()]);
    }
    if profile
        .network_scopes
        .as_ref()
        .is_some_and(|scopes| !scopes.domains.is_empty())
    {
        grants.push("network.fetch".into());
    }
    let acceptance = profile
        .acceptance_check
        .as_ref()
        .map(|check| check.name.as_str())
        .unwrap_or("Reply directly to conversation; verify requested tool work with successful-operation evidence");
    let mut budgets = serde_json::to_value(profile.limits)?;
    budgets["provider_transport"] = json!("aegis-direct-v1");
    if profile.provider == "claude-api" {
        budgets["provider_transport"] = json!("aegis-claude-api-v1");
    }
    budgets["model"] = json!(profile.model);
    budgets["api_key_env"] = json!(profile.api_key_env);
    budgets["reasoning_effort"] = json!(profile.reasoning_effort);
    budgets["fallback_routes"] = json!(profile.fallback_routes);
    budgets["endpoint"] = json!(profile.endpoint);
    budgets["container_image"] = json!(profile.image);
    budgets["previous_run"] = json!(profile.previous_run);
    budgets["acceptance_check"] = json!(profile.acceptance_check);
    budgets["command_scopes"] = json!(profile.command_scopes);
    budgets["filesystem_scopes"] = json!(profile.filesystem_scopes);
    budgets["network_scopes"] = json!(profile.network_scopes);
    let run = store.create_run(
        request,
        &std::env::current_dir()?,
        &profile.provider,
        json!(grants),
        budgets,
        acceptance,
    )?;
    let obligations = store.obligations(&run.id)?;
    if obligations.len() > 1 {
        terminal.message(
            Tone::Accent,
            "Tracking requirements",
            &format!(
                "{} user-written items are frozen for this task; F3 shows what remains open.",
                obligations.len() - 1
            ),
        )?;
    }
    drop(store);
    profile.previous_run = Some(run.id.clone());
    save(root, profile)?;
    if obligations.len() > 1 && !review_start_contract(root, &run.id, terminal)? {
        return Ok(());
    }
    kernel::spawn(root, &run.id, secret_reference(profile).zip(secret))?;
    follow(root, &run.id, terminal)?;
    recover_auth_after_follow(root, &run.id, terminal)
}

fn recover_auth_after_follow(root: &Path, id: &str, terminal: &mut Terminal) -> Result<()> {
    let store = Store::open(root)?;
    let Some(route) = auth_recovery_route(&store, id)? else {
        return Ok(());
    };
    let run = store.run(id)?;
    let reference = route_key_reference(&run, &route).map(str::to_owned);
    drop(store);
    if route.provider == "claude-api" {
        let Some(reference) = reference else {
            return Ok(());
        };
        let choices = [
            "Enter a current Claude API key and continue this task",
            "Leave task paused",
        ]
        .map(str::to_owned);
        if terminal.select("This task's Claude API key was rejected", &choices)? == Some(0)
            && let Some(key) = field(terminal, "  Replacement key (hidden) › ", true)?
        {
            match crate::claude_api::validate_key(&key) {
                Ok(()) => resume(root, id, terminal, Some((&reference, &key)))?,
                Err(error) => {
                    terminal.message(Tone::Warning, "Key unchanged", &error.to_string())?
                }
            }
        }
    } else if route.provider == "custom" {
        let Some(reference) = route
            .endpoint
            .as_ref()
            .and_then(|endpoint| endpoint.api_key_env.as_deref())
        else {
            return Ok(());
        };
        let choices = [
            "Enter a new key and continue this task",
            "Leave task paused",
        ]
        .map(str::to_owned);
        if terminal.select("This endpoint rejected its key", &choices)? == Some(0) {
            if let Some(key) = field(terminal, "  Replacement key (hidden) › ", true)? {
                if !key.trim().is_empty() {
                    resume(root, id, terminal, Some((reference, &key)))?;
                }
            }
        }
    } else {
        let choices = [
            format!(
                "Sign in to {} and continue this task",
                name(&route.provider)
            ),
            "Leave task paused".into(),
        ];
        if terminal.select("This task's provider needs sign-in", &choices)? == Some(0)
            && crate::signin::run(&route.provider, terminal)?
        {
            resume(root, id, terminal, None)?;
        }
    }
    Ok(())
}

fn auth_recovery_route(store: &Store, id: &str) -> Result<Option<crate::routing::Route>> {
    if store.run(id)?.state != "waiting_recovery" {
        return Ok(None);
    }
    let latest_model = store
        .recent_events(id, 8)?
        .into_iter()
        .rev()
        .find(|event| matches!(event.kind.as_str(), "model.failed" | "model.response"));
    if !latest_model.is_some_and(|event| {
        event.kind == "model.failed" && event.payload["error"].as_str().is_some_and(is_auth_error)
    }) {
        return Ok(None);
    }
    Ok(Some(store.current_route(id)?))
}

fn new_conversation(root: &Path, terminal: &Terminal, profile: &mut Profile) -> Result<()> {
    profile.previous_run = None;
    save(root, profile)?;
    terminal.message(
        Tone::Accent,
        "New conversation",
        "Previous tasks remain in sessions. What would you like to do?",
    )
}

fn is_auth_error(error: &str) -> bool {
    let error = error.to_lowercase();
    [
        "401",
        "oauth access token has expired",
        "not logged in",
        "not authenticated",
        "not signed in",
        "sign-in expired",
        "sign-in was rejected",
        "choose sign in",
        "please log in",
    ]
    .iter()
    .any(|needle| error.contains(needle))
}

pub fn interactive(root: &Path) -> Result<()> {
    Store::open(root)?;
    let mut terminal = Terminal::default();
    terminal.load_ui(root)?;
    let saved = fs::read(root.join("profile.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Profile>(&bytes).ok());
    let returning = saved.is_some();
    let (mut profile, mut secret) = match saved {
        Some(profile) => (profile, None),
        None => {
            terminal.welcome(
                "Set up this workspace",
                &std::env::current_dir()?.display().to_string(),
            )?;
            let Some(configured) = configure(&terminal)? else {
                return Ok(());
            };
            save(root, &configured.0)?;
            configured
        }
    };
    let mut task_secret = None;
    remember_selection(&terminal, &profile)?;
    if secret_reference(&profile).is_some() && secret.is_none() {
        secret = field(&terminal, "  API key for this session (hidden) › ", true)?;
    }
    if returning {
        let recent: Vec<_> = chat_heads(Store::open(root)?.runs()?)
            .iter()
            .take(3)
            .flat_map(|run| {
                [
                    format!("• {}", crate::terminal::fit(&run.task, 60)),
                    format!(
                        "  {} · {}",
                        chat_state(&run.state),
                        relative_age(run.created_at)
                    ),
                ]
            })
            .collect();
        terminal.home(
            name(&profile.provider),
            &std::env::current_dir()?.display().to_string(),
            &format!(
                "{} · {}",
                profile.model.as_deref().unwrap_or("provider default"),
                profile
                    .reasoning_effort
                    .as_deref()
                    .unwrap_or("default effort")
            ),
            &recent,
        )?;
    }
    let mut history: Vec<_> = Store::open(root)?
        .runs()?
        .iter()
        .take(50)
        .map(|run| run.task.clone())
        .collect();
    history.reverse();
    loop {
        terminal.set_input_status(&format!(
            "{} · {} · {}",
            profile.model.as_deref().unwrap_or(name(&profile.provider)),
            profile
                .reasoning_effort
                .as_deref()
                .unwrap_or("default effort"),
            if profile.write {
                "edit files"
            } else {
                "review only"
            }
        ));
        match terminal.input(&terminal.input_prefix(), false, &history)? {
            Input::Exit => break,
            Input::Providers => {
                switch_provider(root, &terminal, &mut profile, &mut secret)?;
            }
            Input::Models => switch_model(root, &terminal, &mut profile, secret.as_deref())?,
            Input::Settings => settings(root, &mut terminal, &mut profile, &mut secret)?,
            Input::Help => help(&terminal)?,
            Input::Checkpoint => checkpoint_view(root, profile.previous_run.as_deref(), &terminal)?,
            Input::CancelTask => cancel_task(root, profile.previous_run.as_deref(), &terminal)?,
            Input::Sessions => sessions(
                root,
                &mut terminal,
                &mut profile,
                secret.as_deref(),
                &mut task_secret,
            )?,
            Input::NewConversation => {
                new_conversation(root, &terminal, &mut profile)?;
                history.clear();
            }
            Input::Login => {
                sign_in_for_selection(
                    root,
                    &terminal,
                    &mut profile,
                    &mut secret,
                    &mut task_secret,
                )?;
            }
            Input::Submit(request) => {
                let request = request.trim();
                if request.is_empty() {
                    continue;
                }
                let request = goal_task_submission(request).unwrap_or(request);
                if let Some((prefix, text)) = request.split_once(':') {
                    if prefix.eq_ignore_ascii_case("remember") {
                        remember(root, &terminal, text, None)?;
                        continue;
                    }
                }
                let mut words = request.split_whitespace();
                let command = words.next().unwrap_or_default().trim_start_matches('/');
                if request.starts_with('/') && crate::control::VIEWS.contains(&command) {
                    if let Some(id) = profile.previous_run.as_deref() {
                        let argument = words.next();
                        if matches!(command, "goal" | "contract")
                            && matches!(argument, Some("add" | "replace"))
                        {
                            if let Err(error) =
                                edit_goal(root, id, argument.unwrap(), words.next(), &mut terminal)
                            {
                                terminal.message(
                                    Tone::Warning,
                                    "Contract retained",
                                    &error.to_string(),
                                )?;
                            }
                            continue;
                        }
                        let result =
                            crate::control::view(&Store::open(root)?, id, command, argument);
                        match result {
                            Ok(value) => terminal.message(
                                Tone::Quiet,
                                command,
                                &crate::control::display(command, &value),
                            )?,
                            Err(error) => terminal.message(
                                Tone::Warning,
                                "View unavailable",
                                &error.to_string(),
                            )?,
                        }
                    } else {
                        terminal.message(
                            Tone::Quiet,
                            "No current task",
                            "Describe a task or open F3 to select a saved task.",
                        )?;
                    }
                    continue;
                }
                match request {
                    "/" => {
                        if let Some(command) = terminal.command_menu()? {
                            terminal.set_command_draft(&command);
                            if !terminal.interactive { terminal.message(Tone::Quiet,"Command selected",&command)?; }
                        }
                    }
                    "/providers" => {
                        switch_provider(root, &terminal, &mut profile, &mut secret)?;
                    }
                    "/model" | "/models" => {
                        switch_model(root, &terminal, &mut profile, secret.as_deref())?
                    }
                    "/reasoning" => switch_reasoning(root, &terminal, &mut profile, secret.as_deref())?,
                    "/login" => {
                        sign_in_for_selection(
                            root,
                            &terminal,
                            &mut profile,
                            &mut secret,
                            &mut task_secret,
                        )?
                    }
                    "/settings" => {
                        settings(root, &mut terminal, &mut profile, &mut secret)?;
                    }
                    "/memory" => memory_menu(root, &terminal)?,
                    "/instructions" => instruction_menu(root, &terminal)?,
                    "/new" => {
                        new_conversation(root, &terminal, &mut profile)?;
                        history.clear();
                    }
                    "/pause" => {
                        if let Some(id)=profile.previous_run.as_deref() {
                            match crate::pause::request(root,id) {
                                Ok(())=>terminal.message(Tone::Quiet,"Pause requested","Inference stops after the current action records its outcome. /resume continues the same task.")?,
                                Err(error)=>terminal.message(Tone::Warning,"Pause unavailable",&error.to_string())?,
                            }
                        } else { terminal.message(Tone::Quiet,"No current task","Select a saved task with F3.")?; }
                    }
                    "/resume" => {
                        if let Err(error)=resume_current(root,&mut terminal,&profile,secret.as_deref(),&mut task_secret) { terminal.message(Tone::Warning,"Resume unavailable",&error.to_string())?; }
                    }
                    "/sessions" | "/chats" => {
                        sessions(
                            root,
                            &mut terminal,
                            &mut profile,
                            secret.as_deref(),
                            &mut task_secret,
                        )?
                    }
                    "/context" | "/tools" | "/artifacts" | "/trace" | "/tasks" => {
                        if let Some(id) = &profile.previous_run {
                            match request {
                                "/context" => context_view(root, id, &terminal)?,
                                "/tools" => tools_view(root, id, &terminal)?,
                                "/artifacts" => artifacts(root, id, &terminal)?,
                                "/trace" => {
                                    let store = Store::open(root)?;
                                    trace::display(&store.run(id)?, &store.events(id)?);
                                }
                                _ => {
                                    for milestone in Store::open(root)?.milestones(id)? {
                                        terminal.message(
                                            Tone::Quiet,
                                            &milestone.state,
                                            &milestone.title,
                                        )?;
                                    }
                                }
                            }
                        } else {
                            terminal.message(
                                Tone::Quiet,
                                "",
                                "No current task. Describe one to get started.",
                            )?;
                        }
                    }
                    "/exit" | "/quit" => break,
                    "/checkpoint" => {
                        checkpoint_view(root, profile.previous_run.as_deref(), &terminal)?
                    }
                    "/cancel" => cancel_task(root, profile.previous_run.as_deref(), &terminal)?,
                    "/help" => help(&terminal)?,
                    _ if request.starts_with('/') => terminal.message(Tone::Quiet, "Unknown shortcut", "Press F1 for help, F3 for chats, F6 for model/reasoning, or describe your task without a slash.")?,
                    _ => {
                        history.push(request.to_owned());
                        if !matches!(profile.provider.as_str(), "custom" | "claude-api")
                            && !ensure_provider(&terminal, &profile.provider)?
                        {
                            continue;
                        }
                        if let Err(error) = task(
                            root,
                            &mut terminal,
                            &mut profile,
                            secret.as_deref(),
                            request,
                        ) {
                            terminal.clear_activity()?;
                            terminal.message(Tone::Warning, "!", &format!("{error:#}"))?;
                        }
                    }
                }
            }
        }
    }
    terminal.message(
        Tone::Quiet,
        "Session saved",
        "Active tasks remain available the next time you open Aegis.",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_prefix_preserves_pasted_task_text_and_control_commands() {
        assert_eq!(
            goal_task_submission("/goal Repair the model picker"),
            Some("Repair the model picker")
        );
        assert_eq!(
            goal_task_submission(
                "/contract\nRepair the picker\nRequirements:\n- show current models"
            ),
            Some("Repair the picker\nRequirements:\n- show current models")
        );
        for command in [
            "/goal",
            "/goal ",
            "/goal add",
            "/goal add tests",
            "/goal replace O2",
            "/goal history",
            "/contract history",
            "/goalkeeper repair the picker",
        ] {
            assert_eq!(goal_task_submission(command), None, "{command}");
        }
    }

    #[test]
    fn auth_recovery_targets_the_switched_route_not_the_primary_profile() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "Repair parser",
            directory.path(),
            "codex",
            json!([]),
            json!({"provider_transport":"aegis-direct-v1","model":"primary","fallback_routes":[{"provider":"grok","model":"fallback"}]}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        store.event(&run.id, "model.started", json!({"turn":1}))?;
        store.event(
            &run.id,
            "model.failed",
            json!({"recoverable_reason":"usage_limit"}),
        )?;
        store.transition_provider(&run.id, crate::routing::Reason::UsageLimit)?;
        store.event(&run.id, "model.started", json!({"turn":2}))?;
        store.event(&run.id, "model.failed", json!({"error":"HTTP 401"}))?;
        store.state(
            &run.id,
            "waiting_recovery",
            json!({"reason":"model unavailable"}),
        )?;
        assert_eq!(
            auth_recovery_route(&store, &run.id)?.unwrap().provider,
            "grok"
        );

        store.state(&run.id, "ready", json!({}))?;
        assert!(auth_recovery_route(&store, &run.id)?.is_none());
        store.state(&run.id, "running", json!({}))?;
        store.event(&run.id, "model.response", json!({"action":"blocked"}))?;
        store.state(&run.id, "waiting_recovery", json!({"reason":"other"}))?;
        assert!(auth_recovery_route(&store, &run.id)?.is_none());
        Ok(())
    }

    #[test]
    fn saved_chat_ages_use_storage_seconds() {
        let now = crate::storage::now();
        assert_eq!(relative_age(now), "just now");
        assert_eq!(relative_age(now - 120), "2m ago");
        assert_eq!(relative_age(now - 7200), "2h ago");
        assert_eq!(relative_age(now - 3 * 86400), "3d ago");
        assert_eq!(relative_age(now + 60), "just now");
    }

    #[test]
    fn saved_chat_heads_hide_parent_turns_without_merging_new_conversations() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let first =
            store.create_run("first", directory.path(), "codex", json!([]), json!({}), "")?;
        let second = store.create_run(
            "follow up",
            directory.path(),
            "codex",
            json!([]),
            json!({"previous_run":first.id}),
            "",
        )?;
        let separate = store.create_run(
            "new chat",
            directory.path(),
            "codex",
            json!([]),
            json!({}),
            "",
        )?;
        let heads = chat_heads(vec![separate.clone(), second.clone(), first]);
        assert_eq!(
            heads.iter().map(|run| run.id.as_str()).collect::<Vec<_>>(),
            [separate.id, second.id]
        );
        Ok(())
    }

    #[test]
    fn only_a_quick_second_press_interrupts_the_model_turn() {
        let mut keys = InterruptKeys::default();
        let now = Instant::now();
        assert!(matches!(keys.next(now), crate::interrupt::Scope::Operation));
        assert!(matches!(
            keys.next(now + Duration::from_millis(100)),
            crate::interrupt::Scope::Model
        ));
        assert!(matches!(
            keys.next(now + Duration::from_millis(200)),
            crate::interrupt::Scope::Operation
        ));
        assert!(matches!(
            keys.next(now + Duration::from_secs(2)),
            crate::interrupt::Scope::Operation
        ));
    }

    #[test]
    fn image_references_cannot_be_command_options_or_shell_expressions() {
        assert!(valid_image_reference("node:22-alpine"));
        assert!(valid_image_reference(
            "localhost:5000/org/image@sha256:abcdef"
        ));
        for invalid in [
            "",
            "--all-tags",
            "node;echo bad",
            "node\nother",
            "node image",
        ] {
            assert!(!valid_image_reference(invalid));
        }
    }

    #[test]
    fn profile_persists_only_secret_references() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut profile = Profile {
            provider: "custom".into(),
            model: Some("model".into()),
            reasoning_effort: None,
            fallback_routes: Vec::new(),
            endpoint: Some(Endpoint {
                base_url: "https://example.test/v1".into(),
                api_key_env: Some("ARUN_SESSION_API_KEY".into()),
                response_format: ResponseFormat::Schema,
                allow_insecure: false,
            }),
            api_key_env: None,
            write: false,
            image: None,
            limits: crate::budget::Limits::default(),
            acceptance_check: None,
            command_scopes: None,
            filesystem_scopes: None,
            network_scopes: None,
            previous_run: None,
        };
        save(directory.path(), &profile)?;
        profile.write = true;
        save(directory.path(), &profile)?;
        let saved: Profile =
            serde_json::from_slice(&fs::read(directory.path().join("profile.json"))?)?;
        assert_eq!(secret_reference(&saved), Some("ARUN_SESSION_API_KEY"));
        let mut direct = saved.clone();
        direct.provider = "codex".into();
        direct.endpoint = None;
        direct.fallback_routes = vec![crate::routing::Route {
            provider: "custom".into(),
            model: "model".into(),
            reasoning_effort: None,
            endpoint: saved.endpoint.clone(),
            api_key_env: None,
        }];
        assert_eq!(secret_reference(&direct), Some("ARUN_SESSION_API_KEY"));
        assert!(saved.write);
        assert_eq!(fs::read_dir(directory.path())?.count(), 1);
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "read fixture",
            directory.path(),
            "codex",
            json!([]),
            json!({"provider_transport":"aegis-direct-v1","model":"primary","fallback_routes":[{"provider":"custom","model":"model","endpoint":{"base_url":"https://example.test/v1","api_key_env":"FROZEN_RUN_KEY"}}]}),
            "",
        )?;
        assert_eq!(run_secret_reference(&direct, &run), Some("FROZEN_RUN_KEY"));
        let other = store.create_run(
            "read fixture",
            directory.path(),
            "codex",
            json!([]),
            json!({"provider_transport":"aegis-direct-v1","model":"primary","fallback_routes":[{"provider":"custom","model":"model","endpoint":{"base_url":"https://other.test/v1","api_key_env":"OTHER_KEY"}}]}),
            "",
        )?;
        assert_eq!(run_secret_reference(&direct, &other), None);
        assert!(is_auth_error("OAuth access token has expired. HTTP 401"));
        assert!(!is_auth_error("usage balance exhausted"));
        let mut aliased = serde_json::to_value(&saved)?;
        aliased["provider"] = json!("chatgpt");
        assert_eq!(
            serde_json::from_value::<Profile>(aliased.clone())?.provider,
            "codex"
        );
        aliased["provider"] = json!("claude-code");
        assert_eq!(
            serde_json::from_value::<Profile>(aliased)?.provider,
            "claude"
        );
        Ok(())
    }

    #[test]
    fn claude_api_secret_is_bound_to_its_saved_run_and_never_persisted() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut profile = blank_profile("claude-api".into());
        profile.model = Some("account-model".into());
        profile.api_key_env = Some("ARUN_SESSION_API_KEY".into());
        save(directory.path(), &profile)?;
        let saved = fs::read_to_string(directory.path().join("profile.json"))?;
        assert!(!saved.contains("secret-example"));
        assert_eq!(secret_reference(&profile), Some("ARUN_SESSION_API_KEY"));

        let mut store = Store::open(directory.path())?;
        let budgets = json!({"provider_transport":"aegis-claude-api-v1","model":"account-model","api_key_env":"ARUN_SESSION_API_KEY"});
        let first = store.create_run(
            "first",
            directory.path(),
            "claude-api",
            json!([]),
            budgets.clone(),
            "",
        )?;
        let second = store.create_run(
            "second",
            directory.path(),
            "claude-api",
            json!([]),
            budgets,
            "",
        )?;
        let route = store.current_route(&first.id)?;
        assert_eq!(
            route_key_reference(&first, &route),
            Some("ARUN_SESSION_API_KEY")
        );
        assert_eq!(
            route_secret_target(&first, &route).as_deref(),
            Some("claude-api")
        );
        assert_eq!(run_secret_reference(&profile, &first), None);
        let key = TaskSecret {
            run_id: first.id.clone(),
            target: "claude-api".into(),
            value: "secret-example".into(),
        };
        assert!(key.matches(&first, &route));
        assert!(!key.matches(&second, &route));
        assert!(
            !fs::read_to_string(directory.path().join("profile.json"))?.contains("secret-example")
        );
        Ok(())
    }

    #[test]
    fn saved_account_shortcut_never_imports_workspace_permissions() {
        let profile = profile_from_selection(Selection {
            provider: "codex".into(),
            model: "advertised-model".into(),
            reasoning_effort: Some("low".into()),
        });
        assert_eq!(profile.model.as_deref(), Some("advertised-model"));
        assert_eq!(profile.reasoning_effort.as_deref(), Some("low"));
        assert!(!profile.write);
        assert!(profile.image.is_none());
        assert!(profile.endpoint.is_none());
        assert!(profile.acceptance_check.is_none());
        assert!(profile.command_scopes.is_none());
        assert!(profile.filesystem_scopes.is_none());
        assert!(profile.network_scopes.is_none());
        assert!(profile.previous_run.is_none());
    }
}
