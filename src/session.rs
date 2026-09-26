use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    endpoint::{Endpoint, ResponseFormat},
    kernel, model, provider,
    storage::Store,
    terminal::{Input, RawMode, Terminal, Tone},
    trace,
};

#[derive(Debug, Serialize, Deserialize)]
struct Profile {
    provider: String,
    model: Option<String>,
    endpoint: Option<Endpoint>,
    write: bool,
    image: Option<String>,
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

fn configure(terminal: &Terminal) -> Result<Option<(Profile, Option<String>)>> {
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
        acceptance_check: None,
        previous_run: None,
    };
    let mut secret = None;
    if profile.provider == "custom" {
        let Some(url) = field(terminal, "  Endpoint URL › ", false)? else {
            return Ok(None);
        };
        let Some(model) = field(terminal, "  Model ID › ", false)? else {
            return Ok(None);
        };
        if model.trim().is_empty() {
            terminal.message(Tone::Warning, "!", "A custom endpoint needs a model ID.")?;
            return Ok(None);
        }
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
        profile.model = Some(model);
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
        let choices = ["Use the provider's default model", "Choose a model ID"].map(str::to_owned);
        let Some(model) = terminal.select("Model", &choices)? else {
            return Ok(None);
        };
        if model == 1 {
            let Some(id) = field(terminal, "  Model ID › ", false)? else {
                return Ok(None);
            };
            if id.trim().is_empty() {
                terminal.message(Tone::Warning, "!", "A model ID cannot be empty.")?;
                return Ok(None);
            }
            profile.model = Some(id);
        }
    }
    let permissions = [
        "Allow workspace edits",
        "Review only — no edits",
        "Workspace edits and isolated container commands",
    ]
    .map(str::to_owned);
    let Some(permission) = terminal.select("Workspace permissions", &permissions)? else {
        return Ok(None);
    };
    profile.write = permission != 1;
    if permission == 2 {
        let docker = provider::system_executable("docker");
        let output = docker.as_ref().and_then(|program| {
            Command::new(program)
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
    let choices = [
        "Successful-operation evidence",
        "Independent container check from a JSON file",
    ]
    .map(str::to_owned);
    let Some(choice) = terminal.select("Completion checks", &choices)? else {
        return Ok(None);
    };
    if choice == 1 {
        let Some(path) = field(terminal, "  Acceptance JSON file › ", false)? else {
            return Ok(None);
        };
        match crate::acceptance::Check::from_file(Path::new(&path)) {
            Ok(check) => {
                terminal.message(Tone::Accent, "Acceptance", &format!("{} · {} · read-only workspace, no network. The image must already be available locally.", check.name, check.image))?;
                profile.acceptance_check = Some(check);
            }
            Err(error) => {
                terminal.message(Tone::Warning, "Invalid check", &format!("{error:#}"))?;
                return Ok(None);
            }
        }
    }
    Ok(Some((profile, secret)))
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

fn follow(root: &Path, id: &str, terminal: &mut Terminal) -> Result<()> {
    let _raw = RawMode::enter(terminal.interactive)?;
    let mut store = Store::open(root)?;
    let mut sequence = 0;
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
                            if matches!(store.run(id)?.state.as_str(), "ready" | "running") {
                                store.state(id, "cancelled", json!({"source":"terminal"}))?;
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
            store.active_capabilities(id)?.len(),
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
    let active = store.active_capabilities(id)?;
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
    let run = store.create_run(request, &std::env::current_dir()?, &profile.provider, json!(grants),
        json!({"model":profile.model, "endpoint":profile.endpoint, "container_image":profile.image, "previous_run":profile.previous_run,
            "acceptance_check":profile.acceptance_check,
            "actions":80,"model_tokens":800_000,"wall_seconds":3600,"context_chars":256_000,"model_seconds":180,"process_seconds":60}),
        acceptance)?;
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
                if let Some(mut configured) = configure(&terminal)? {
                    configured.0.previous_run = profile.previous_run.clone();
                    profile = configured.0;
                    secret = configured.1;
                    save(root, &profile)?;
                    terminal.message(Tone::Accent, "Provider", name(&profile.provider))?;
                }
            }
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
                        if let Some(mut configured) = configure(&terminal)? { configured.0.previous_run = profile.previous_run.clone(); profile = configured.0; secret = configured.1; save(root, &profile)?; }
                    }
                    "/new" => { new_conversation(root, &terminal, &mut profile)?; history.clear(); }
                    "/sessions" | "/status" => sessions(root, &mut terminal, &profile, secret.as_deref())?,
                    "/context" | "/tools" | "/artifacts" | "/trace" | "/tasks" => {
                        if let Some(id) = &profile.previous_run {
                            match request {
                                "/context" => context_view(root, id, &terminal)?,
                                "/tools" => tools_view(root, id, &terminal)?,
                                "/artifacts" => artifacts(root, id, &terminal)?,
                                "/trace" => { let store = Store::open(root)?; trace::display(&store.run(id)?, &store.events(id)?); },
                                _ => { for milestone in Store::open(root)?.milestones(id)? { terminal.message(Tone::Quiet, &milestone.state, &milestone.title)?; } }
                            }
                        } else { terminal.message(Tone::Quiet, "", "No current task. Describe one to get started.")?; }
                    }
                    "/exit" | "/quit" => break,
                    "/help" => terminal.message(Tone::Quiet, "Help", "Write a task in plain language. F2 changes provider, F3 opens tasks, F4 signs in, F5 starts a fresh conversation. Ctrl+C stops an active task; Ctrl+D detaches. Up recalls previous tasks.")?,
                    _ => {
                        history.push(request.to_owned());
                        if profile.provider != "custom" && !ensure_provider(&terminal, &profile.provider)? {
                            continue;
                        }
                        if let Err(error) = task(root, &mut terminal, &mut profile, secret.as_deref(), request) {
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
