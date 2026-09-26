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
    kernel, model, provider,
    storage::Store,
    terminal::{Input, RawMode, Terminal, Tone},
    trace,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Profile {
    provider: String,
    model: Option<String>,
    endpoint: Option<Endpoint>,
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

fn name(provider: &str) -> &str {
    match provider {
        "codex" => "ChatGPT / Codex",
        "claude" => "Claude Code",
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

fn choose_model(
    terminal: &Terminal,
    profile: &Profile,
    secret: Option<&str>,
) -> Result<Option<Option<String>>> {
    terminal.message(Tone::Quiet, "Models", "Loading the provider catalog…")?;
    let catalog = if let Some(endpoint) = &profile.endpoint {
        endpoint
            .models(secret)
            .map(|models| crate::catalog::Catalog {
                models,
                source: "Your endpoint's /models catalog".into(),
            })
    } else {
        crate::catalog::native(&profile.provider)
    };
    let models = match catalog {
        Ok(catalog) => {
            terminal.message(Tone::Quiet, "Catalog", &catalog.source)?;
            catalog.models
        }
        Err(error) => {
            terminal.message(
                Tone::Quiet,
                "Catalog",
                &format!("Model list unavailable · {}. Default/manual selection is still available; F4 refreshes sign-in.", if error.to_string().to_lowercase().contains("timed out") { "provider didn't respond in time" } else { "listing is unsupported or not ready" }),
            )?;
            Vec::new()
        }
    };
    let native = profile.provider != "custom";
    let mut choices = Vec::new();
    let mut values = Vec::new();
    if native {
        choices.push("Provider default · let your CLI choose".into());
        values.push(None);
    }
    for model in models {
        let current = if profile.model.as_deref() == Some(model.id.as_str()) {
            " · current"
        } else {
            ""
        };
        choices.push(format!("{}  [{}]{current}", model.label, model.id));
        values.push(Some(model.id));
    }
    if let Some(current) = &profile.model {
        if !values.iter().any(|value| value.as_ref() == Some(current)) {
            choices.push(format!("{current} · current (not in catalog)"));
            values.push(Some(current.clone()));
        }
    }
    let manual = choices.len();
    choices.push("Enter another model ID…".into());
    let selected = if choices.len() == 1 {
        Some(0)
    } else {
        terminal.select(
            &format!("{} · choose a model", name(&profile.provider)),
            &choices,
        )?
    };
    let Some(selected) = selected else {
        return Ok(None);
    };
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
    Ok(Some(values[selected].clone()))
}

fn show_selection(terminal: &Terminal, profile: &Profile) -> Result<()> {
    terminal.message(
        Tone::Accent,
        "Connected",
        &format!(
            "{} · {}",
            name(&profile.provider),
            profile.model.as_deref().unwrap_or("provider default")
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
        profile.endpoint = selected.endpoint;
        *secret = key;
        save(root, profile)?;
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
        profile.model = model;
        save(root, profile)?;
        show_selection(terminal, profile)?;
        terminal.message(
            Tone::Quiet,
            "",
            "New tasks use this model; saved tasks keep their original selection.",
        )?;
    }
    Ok(())
}

fn configure_provider(terminal: &Terminal) -> Result<Option<(Profile, Option<String>)>> {
    let choices = [
        "ChatGPT — use your Codex login",
        "Claude Code — use your Claude login",
        "Grok — use your Grok login",
        "Custom OpenAI-compatible endpoint",
    ]
    .map(str::to_owned);
    let Some(choice) = terminal.select("Choose your provider", &choices)? else {
        return Ok(None);
    };
    let provider = ["codex", "claude", "grok", "custom"][choice].to_owned();
    let mut profile = Profile {
        provider,
        model: None,
        endpoint: None,
        write: false,
        image: None,
        limits: crate::budget::Limits::default(),
        acceptance_check: None,
        command_scopes: None,
        filesystem_scopes: None,
        network_scopes: None,
        previous_run: None,
    };
    let mut secret = None;
    if profile.provider == "custom" {
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
        terminal.message(Tone::Quiet, "Ready", "Using your existing provider login and default model. F4 signs in if needed; F6 lets you pick another model.")?;
    }
    if profile.provider == "custom" {
        let Some(model) = choose_model(terminal, &profile, secret.as_deref())? else {
            return Ok(None);
        };
        profile.model = model;
    }
    Ok(Some((profile, secret)))
}

fn configure(terminal: &Terminal) -> Result<Option<(Profile, Option<String>)>> {
    let Some((mut profile, secret)) = configure_provider(terminal)? else {
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

fn settings(root: &Path, terminal: &mut Terminal, profile: &mut Profile) -> Result<()> {
    let choices = [
        "Workspace permissions and command environment",
        "Task budgets",
        "Completion checks",
        "Exact command scopes",
        "Appearance · mascot, colors and motion",
        "Project memory · remember what matters",
        "File access scopes · optional exact files / folders",
        "Web access · optional approved HTTPS domains",
        "Back",
    ]
    .map(str::to_owned);
    let mut selected = profile.clone();
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
        _ => false,
    };
    if changed {
        save(root, &selected)?;
        *profile = selected;
        terminal.message(
            Tone::Success,
            "Settings saved",
            "Applies to new tasks. Existing tasks retain their approved permissions and budgets.",
        )?;
    }
    Ok(())
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
    if matches!(
        store.run(id)?.state.as_str(),
        "completed" | "cancelled" | "failed"
    ) {
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
        terminal.message(Tone::Warning, "Custom budget", "Task and command deadlines are enforced. Model usage is checked between turns; tool-context exposure is reserved before requests in o200k_base units, not provider billing tokens. Provider usage allowances still apply. Press Enter to keep each default.")?;
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
    if provider::find(name)?.is_some() {
        return Ok(true);
    }
    let package = provider::specification(name)?.package;
    terminal.message(Tone::Warning, "Provider setup", &format!("This provider is not installed. Aegis can download {package} into your private provider directory; it will not modify this workspace or your global npm installation."))?;
    let choices = [format!("Install {package} and continue"), "Back".into()];
    if terminal.select("Install the official provider CLI?", &choices)? != Some(0) {
        return Ok(false);
    }
    if let Err(error) = provider::install(name) {
        terminal.message(Tone::Warning, "Installation failed", &format!("{error:#}"))?;
        return Ok(false);
    }
    Ok(true)
}

fn save(root: &Path, profile: &Profile) -> Result<()> {
    let path = root.join("profile.json");
    let mut temporary = tempfile::NamedTempFile::new_in(root)?;
    temporary.write_all(&serde_json::to_vec_pretty(profile)?)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn secret_reference(profile: &Profile) -> Option<&str> {
    profile
        .endpoint
        .as_ref()
        .and_then(|endpoint| endpoint.api_key_env.as_deref())
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
            terminal.message(
                Tone::Quiet,
                "",
                &format!(
                    "{} model tokens · state {}",
                    store.model_tokens(id)?,
                    run.state
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

fn resume(
    root: &Path,
    id: &str,
    terminal: &mut Terminal,
    secret: Option<(&str, &str)>,
) -> Result<()> {
    let mut store = Store::open(root)?;
    let run = store.run(id)?;
    if matches!(run.state.as_str(), "completed" | "cancelled" | "failed") {
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
    if run.state == "waiting_recovery" {
        store.state(id, "ready", json!({"source":"terminal_resume"}))?;
    }
    drop(store);
    kernel::spawn(root, id, secret)?;
    follow(root, id, terminal)
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

fn artifacts(root: &Path, id: &str, terminal: &Terminal) -> Result<()> {
    let store = Store::open(root)?;
    let artifacts = store.evidence_artifacts(id)?;
    if artifacts.is_empty() {
        return terminal.message(Tone::Quiet, "Artifacts", "No successful tool evidence yet.");
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
    let Some(query) = field(terminal, "  Find text (Enter for preview) › ", false)? else {
        return Ok(());
    };
    terminal.message(
        Tone::Quiet,
        label,
        &kernel::inspect(&store.artifact(hash)?, &query),
    )
}

fn context_view(root: &Path, id: &str, terminal: &Terminal) -> Result<()> {
    let store = Store::open(root)?;
    let run = store.run(id)?;
    terminal.message(Tone::Accent, "Task", &run.task)?;
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
    terminal.message(Tone::Quiet, "Acceptance", &run.acceptance)?;
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
    profile: &Profile,
    secret: Option<&str>,
) -> Result<()> {
    let store = Store::open(root)?;
    let runs = store.runs()?;
    if runs.is_empty() {
        terminal.message(Tone::Quiet, "Sessions", "No tasks yet.")?;
        return Ok(());
    }
    let selected: Vec<_> = runs.iter().take(100).collect();
    let mut choices: Vec<_> = selected
        .iter()
        .map(|run| {
            format!(
                "{} · {} · {}",
                run.state,
                name(&run.provider),
                crate::terminal::fit(&run.task, 70)
            )
        })
        .collect();
    choices.push("Back".into());
    let Some(index) = terminal.select("Recent tasks", &choices)? else {
        return Ok(());
    };
    let Some(run) = selected.get(index) else {
        return Ok(());
    };
    let choices = [
        "Follow task",
        "Resume task",
        "Cancel task",
        "View milestones",
        "View trace",
        "View context and handoff",
        "View active tools",
        "Inspect evidence artifacts",
        "Review interrupted operations",
        "Back",
    ]
    .map(str::to_owned);
    match terminal.select("Task controls", &choices)? {
        Some(0) => follow(root, &run.id, terminal)?,
        Some(1) => {
            let matching_endpoint = profile.endpoint.as_ref().is_some_and(|endpoint| {
                run.budgets
                    .pointer("/endpoint/base_url")
                    .and_then(|url| url.as_str())
                    == Some(endpoint.base_url.as_str())
            });
            let credentials = if matching_endpoint {
                secret_reference(profile).zip(secret)
            } else {
                None
            };
            resume(root, &run.id, terminal, credentials)?;
        }
        Some(2) if !matches!(run.state.as_str(), "completed" | "cancelled" | "failed") => {
            cancel_task(root, Some(&run.id), terminal)?;
        }
        Some(3) => {
            for milestone in store.milestones(&run.id)? {
                terminal.message(Tone::Quiet, &milestone.state, &milestone.title)?;
            }
        }
        Some(4) => trace::display(run, &store.events(&run.id)?),
        Some(5) => context_view(root, &run.id, terminal)?,
        Some(6) => tools_view(root, &run.id, terminal)?,
        Some(7) => artifacts(root, &run.id, terminal)?,
        Some(8) => review_operations(root, &run.id, terminal)?,
        _ => {}
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
        .unwrap_or("Complete the requested task using successful-operation evidence");
    let mut budgets = serde_json::to_value(profile.limits)?;
    budgets["model"] = json!(profile.model);
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
    terminal.message(
        Tone::Quiet,
        "Budget",
        &format!(
            "{} hours · {} turns · {} tokens · commands up to {}s",
            profile.limits.wall_seconds as f64 / 3600.0,
            profile.limits.actions,
            profile.limits.model_tokens,
            profile.limits.process_seconds
        ),
    )?;
    drop(store);
    profile.previous_run = Some(run.id.clone());
    save(root, profile)?;
    kernel::spawn(root, &run.id, secret_reference(profile).zip(secret))?;
    follow(root, &run.id, terminal)?;
    let store = Store::open(root)?;
    let authentication_failed = store.events(&run.id)?.iter().any(|event| {
        event.kind == "model.failed"
            && event.payload["error"]
                .as_str()
                .is_some_and(|error| is_auth_error(error))
    });
    if authentication_failed && profile.provider != "custom" {
        let choices = ["Sign in and continue this task", "Leave task paused"].map(str::to_owned);
        if terminal.select("Your provider sign-in needs refreshing", &choices)? == Some(0) {
            model::login(&profile.provider)?;
            resume(root, &run.id, terminal, None)?;
        }
    }
    Ok(())
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
                "Choose a provider to get started",
                &std::env::current_dir()?.display().to_string(),
            )?;
            let Some(configured) = configure(&terminal)? else {
                return Ok(());
            };
            save(root, &configured.0)?;
            configured
        }
    };
    if secret_reference(&profile).is_some() && secret.is_none() {
        secret = field(&terminal, "  API key for this session (hidden) › ", true)?;
    }
    if returning {
        terminal.welcome(
            name(&profile.provider),
            &std::env::current_dir()?.display().to_string(),
        )?;
    }
    show_selection(&terminal, &profile)?;
    if terminal.interactive {
        if let Some(id) = &profile.previous_run {
            let store = Store::open(root)?;
            if let Ok(run) = store.run(id) {
                if !matches!(run.state.as_str(), "completed" | "cancelled" | "failed") {
                    let choices = [
                        "Continue my last task",
                        "Start a new task",
                        "Open saved tasks",
                    ]
                    .map(str::to_owned);
                    terminal.message(Tone::Quiet, "Last task", &run.task)?;
                    match terminal.select("Welcome back", &choices)? {
                        Some(0) => resume(
                            root,
                            id,
                            &mut terminal,
                            secret_reference(&profile).zip(secret.as_deref()),
                        )?,
                        Some(2) => sessions(root, &mut terminal, &profile, secret.as_deref())?,
                        _ => {}
                    }
                }
            }
        }
    }
    let mut history: Vec<_> = Store::open(root)?
        .runs()?
        .iter()
        .take(50)
        .map(|run| run.task.clone())
        .collect();
    history.reverse();
    loop {
        match terminal.input(&terminal.input_prefix(), false, &history)? {
            Input::Exit => break,
            Input::Providers => {
                switch_provider(root, &terminal, &mut profile, &mut secret)?;
            }
            Input::Models => switch_model(root, &terminal, &mut profile, secret.as_deref())?,
            Input::Settings => settings(root, &mut terminal, &mut profile)?,
            Input::Help => help(&terminal)?,
            Input::Checkpoint => checkpoint_view(root, profile.previous_run.as_deref(), &terminal)?,
            Input::CancelTask => cancel_task(root, profile.previous_run.as_deref(), &terminal)?,
            Input::Sessions => sessions(root, &mut terminal, &profile, secret.as_deref())?,
            Input::NewConversation => {
                new_conversation(root, &terminal, &mut profile)?;
                history.clear();
            }
            Input::Login => {
                if profile.provider == "custom" {
                    secret = field(&terminal, "  API key (hidden) › ", true)?;
                    if let Some(endpoint) = &mut profile.endpoint {
                        endpoint.api_key_env = secret
                            .as_ref()
                            .filter(|key| !key.trim().is_empty())
                            .map(|_| "ARUN_SESSION_API_KEY".into());
                    }
                    save(root, &profile)?;
                } else if ensure_provider(&terminal, &profile.provider)? {
                    if let Err(error) = model::login(&profile.provider) {
                        terminal.message(Tone::Warning, "!", &error.to_string())?;
                    }
                }
            }
            Input::Submit(request) => {
                let request = request.trim();
                if request.is_empty() {
                    continue;
                }
                if let Some((prefix, text)) = request.split_once(':') {
                    if prefix.eq_ignore_ascii_case("remember") {
                        remember(root, &terminal, text, None)?;
                        continue;
                    }
                }
                match request {
                    "/provider" => {
                        switch_provider(root, &terminal, &mut profile, &mut secret)?;
                    }
                    "/model" | "/models" => {
                        switch_model(root, &terminal, &mut profile, secret.as_deref())?
                    }
                    "/settings" => {
                        settings(root, &mut terminal, &mut profile)?;
                    }
                    "/memory" => memory_menu(root, &terminal)?,
                    "/new" => {
                        new_conversation(root, &terminal, &mut profile)?;
                        history.clear();
                    }
                    "/sessions" | "/status" => {
                        sessions(root, &mut terminal, &profile, secret.as_deref())?
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
                    _ => {
                        history.push(request.to_owned());
                        if profile.provider != "custom"
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
            endpoint: Some(Endpoint {
                base_url: "https://example.test/v1".into(),
                api_key_env: Some("ARUN_SESSION_API_KEY".into()),
                response_format: ResponseFormat::Schema,
                allow_insecure: false,
            }),
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
        assert!(saved.write);
        assert_eq!(fs::read_dir(directory.path())?.count(), 1);
        assert!(is_auth_error("OAuth access token has expired. HTTP 401"));
        assert!(!is_auth_error("usage balance exhausted"));
        Ok(())
    }
}
