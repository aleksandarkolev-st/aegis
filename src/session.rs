use std::fs::{self, File};
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
    kernel, model,
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
        let Some(key) = field(terminal, "  API key (hidden; optional) › ", true)? else {
            return Ok(None);
        };
        if !key.trim().is_empty() {
            secret = Some(key);
        }
        let endpoint = Endpoint {
            base_url: url,
            api_key_env: secret.as_ref().map(|_| "ARUN_SESSION_API_KEY".into()),
            response_format: ResponseFormat::Schema,
            allow_insecure: false,
        };
        if let Err(error) = endpoint.url() {
            terminal.message(Tone::Warning, "!", &error.to_string())?;
            return Ok(None);
        }
        profile.model = Some(model);
        profile.endpoint = Some(endpoint);
    } else {
        let choices = ["Use my existing sign-in", "Sign in now"].map(str::to_owned);
        if terminal.select("Authentication", &choices)? == Some(1) {
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
        let output = Command::new("docker")
            .args(["image", "ls", "--format", "{{.Repository}}:{{.Tag}}"])
            .stderr(Stdio::null())
            .output();
        let images: Vec<String> = output
            .ok()
            .filter(|output| output.status.success())
            .map(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .filter(|line| !line.contains("<none>"))
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        if images.is_empty() {
            terminal.message(Tone::Warning, "Containers", "No local Docker image is available. File edits remain enabled; process tools stay disabled.")?;
        } else if let Some(index) = terminal.select("Choose a local container image", &images)? {
            profile.image = Some(images[index].clone());
        }
    }
    Ok(Some((profile, secret)))
}

fn save(root: &Path, profile: &Profile) -> Result<()> {
    let path = root.join("profile.json");
    let temporary = root.join(format!("profile-{}.tmp", uuid::Uuid::new_v4()));
    let mut file = File::create(&temporary)?;
    file.write_all(&serde_json::to_vec_pretty(profile)?)?;
    file.sync_all()?;
    drop(file);
    if path.exists() {
        fs::remove_file(&path)?;
    }
    fs::rename(temporary, path)?;
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
    if matches!(run.state.as_str(), "completed" | "cancelled" | "failed")
        || store.unknown_count(id)? > 0
    {
        terminal.message(Tone::Warning, "!", "This task cannot resume without resolving its terminal state or unknown operation outcomes.")?;
        return Ok(());
    }
    if run.state == "waiting_recovery" {
        store.state(id, "ready", json!({"source":"terminal_resume"}))?;
    }
    drop(store);
    kernel::spawn(root, id, secret)?;
    follow(root, id, terminal)
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
    let selected: Vec<_> = runs.iter().take(8).collect();
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
    let run = store.create_run(request, &std::env::current_dir()?, &profile.provider, json!(grants),
        json!({"model":profile.model, "endpoint":profile.endpoint, "container_image":profile.image, "previous_run":profile.previous_run,
            "actions":80,"model_tokens":800_000,"wall_seconds":3600,"context_chars":256_000,"model_seconds":180,"process_seconds":60}),
        "Complete the requested task using successful-operation evidence")?;
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
                } else if let Err(error) = model::login(&profile.provider) {
                    terminal.message(Tone::Warning, "!", &error.to_string())?;
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
                    "/exit" | "/quit" => break,
                    "/help" => terminal.message(Tone::Quiet, "Help", "Write a task in plain language. F2 changes provider, F3 opens tasks, F4 signs in, F5 starts a fresh conversation. Ctrl+C stops an active task; Ctrl+D detaches. Up recalls previous tasks.")?,
                    _ => {
                        history.push(request.to_owned());
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
    fn profile_persists_only_secret_references() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let profile = Profile {
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
            previous_run: None,
        };
        save(directory.path(), &profile)?;
        let saved: Profile =
            serde_json::from_slice(&fs::read(directory.path().join("profile.json"))?)?;
        assert_eq!(secret_reference(&saved), Some("ARUN_SESSION_API_KEY"));
        assert!(is_auth_error("OAuth access token has expired. HTTP 401"));
        assert!(!is_auth_error("usage balance exhausted"));
        Ok(())
    }
}
