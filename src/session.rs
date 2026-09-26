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
        let choices = ["Use my existing sign-in", "Sign in now"].map(str::to_owned);
        let Some(authentication) = terminal.select("Authentication", &choices)? else {
            return Ok(None);
        };
        if authentication == 1 {
            terminal.message(
                Tone::Accent,
                "Sign in",
                "Opening the provider's native login flow…",
            )?;
            if let Err(error) = model::login(&profile.provider) {
                terminal.message(Tone::Warning, "!", &error.to_string())?;
            }
        }
    }
    let Some(model) = choose_model(terminal, &profile, secret.as_deref())? else {
        return Ok(None);
    };
    profile.model = model;
    Ok(Some((profile, secret)))
}

fn configure(terminal: &Terminal) -> Result<Option<(Profile, Option<String>)>> {
    let Some((mut profile, secret)) = configure_provider(terminal)? else {
        return Ok(None);
    };
    if !configure_environment(terminal, &mut profile)? {
        return Ok(None);
    }
    let Some(limits) = configure_limits(terminal)? else {
        return Ok(None);
    };
    profile.limits = limits;
    if !configure_acceptance(terminal, &mut profile)? {
        return Ok(None);
    }
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

fn settings(root: &Path, terminal: &Terminal, profile: &mut Profile) -> Result<()> {
    let choices = [
        "Workspace permissions and command environment",
        "Task budgets",
        "Completion checks",
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
        "F3 saved tasks and recovery · F5 new conversation · Up recalls previous requests",
    )?;
    terminal.message(Tone::Quiet, "Control", "Ctrl+C interrupts an operation; twice quickly interrupts the model turn. Ctrl+D detaches. Cancel the entire task from F3. No shell commands needed.")
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
        terminal.message(Tone::Warning, "Custom budget", "Task and command deadlines are enforced. Token limits are checked after each model turn, not a price estimate. Provider usage allowances still apply. Press Enter to keep each default.")?;
        for (label, value) in [
            ("Action limit", &mut limits.actions),
            ("Model token limit", &mut limits.model_tokens),
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
    loop {
        let events = store.events_since(id, sequence)?;
        if !events.is_empty() {
            terminal.clear_activity()?;
        }
        for event in events {
            match event.kind.as_str() {
                "model.started" => phase = "Thinking".into(),
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
        terminal.activity(&phase, started.elapsed(), store.model_tokens(id)?)?;
        if terminal.interactive && event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Release {
                    continue;
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
            Store::open(root)?.state(&run.id, "cancelled", json!({"source":"terminal"}))?
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
    let saved = fs::read(root.join("profile.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Profile>(&bytes).ok());
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
    terminal.welcome(
        name(&profile.provider),
        &std::env::current_dir()?.display().to_string(),
    )?;
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
        match terminal.input("  › ", false, &history)? {
            Input::Exit => break,
            Input::Providers => {
                switch_provider(root, &terminal, &mut profile, &mut secret)?;
            }
            Input::Models => switch_model(root, &terminal, &mut profile, secret.as_deref())?,
            Input::Settings => settings(root, &terminal, &mut profile)?,
            Input::Help => help(&terminal)?,
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
                match request {
                    "/provider" => {
                        switch_provider(root, &terminal, &mut profile, &mut secret)?;
                    }
                    "/model" | "/models" => {
                        switch_model(root, &terminal, &mut profile, secret.as_deref())?
                    }
                    "/settings" => {
                        settings(root, &terminal, &mut profile)?;
                    }
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
