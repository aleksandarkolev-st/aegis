//! Pairing and the outbound-only NATS daemon for remote Aegis control.

use std::{path::Path, time::Duration};

use anyhow::{Context, Result, bail};
use async_nats::{HeaderMap, jetstream};
use chrono::Utc;
use futures_util::StreamExt;
use rusqlite::params;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{
    Authority, Command,
    config::Config,
    protocol::{AegisEvent, EventKind, RelayCommand, RelayCommandEnvelope},
};
use crate::storage::{Event, Run, Store};

const COMMAND_STREAM: &str = "AEGIS_COMMANDS";
const MAX_ADMIN_RESPONSE_BYTES: usize = 16 * 1024;

#[derive(Clone)]
struct Transport {
    context: jetstream::Context,
    installation_id: String,
}

/// Entry point for `aegis remote ...`.
pub fn command(root: &Path, args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("pair") => pair_command(root, &args[1..]),
        Some("status") if args.len() == 1 => status(root),
        Some("run") if args.len() == 1 => run_daemon(root),
        Some("help" | "--help") | None => {
            usage();
            Ok(())
        }
        Some(other) => bail!("unknown remote command: {other}"),
    }
}

fn usage() {
    println!(
        "aegis remote pair [--relay-admin-url <url> --admin-token-env <name> --nats-url <tls-url> --nats-token-env <name> [--nats-root-cert <path>]]"
    );
    println!("aegis remote status | run");
    println!(
        "Pair stores only endpoint settings and environment variable names in .arun/remote.json."
    );
}

fn pair_command(root: &Path, args: &[String]) -> Result<()> {
    if args
        .iter()
        .any(|argument| argument == "--help" || argument == "-h")
    {
        usage();
        return Ok(());
    }
    let existing = Config::load(root).ok();
    let mut relay_admin_url = existing.as_ref().map(|value| value.relay_admin_url.clone());
    let mut admin_token_env = existing.as_ref().map(|value| value.admin_token_env.clone());
    let mut nats_url = existing.as_ref().map(|value| value.nats_url.clone());
    let mut nats_token_env = existing.as_ref().map(|value| value.nats_token_env.clone());
    let mut nats_root_certificate = existing
        .as_ref()
        .and_then(|value| value.nats_root_certificate.clone());

    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        index += 1;
        let value = args
            .get(index)
            .context("remote pair option needs a value")?;
        match flag {
            "--relay-admin-url" => relay_admin_url = Some(value.clone()),
            "--admin-token-env" => admin_token_env = Some(value.clone()),
            "--nats-url" => nats_url = Some(value.clone()),
            "--nats-token-env" => nats_token_env = Some(value.clone()),
            "--nats-root-cert" => nats_root_certificate = Some(value.into()),
            _ => bail!("unknown remote pair option: {flag}"),
        }
        index += 1;
    }

    let relay_admin_url = relay_admin_url.context("remote pair needs --relay-admin-url")?;
    let admin_token_env = admin_token_env.context("remote pair needs --admin-token-env")?;
    let nats_url = nats_url.context("remote pair needs --nats-url")?;
    let nats_token_env = nats_token_env.context("remote pair needs --nats-token-env")?;

    let mut authority = Authority::open(root)?;
    let installation_id = authority.installation_id().to_owned();
    if let Some(previous) = &existing
        && previous.installation_id != installation_id
    {
        bail!("local remote configuration belongs to a different Aegis installation");
    }
    let actor_id = existing
        .map(|value| value.actor_id)
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let workspace = root
        .parent()
        .context("Aegis data directory has no workspace parent")?;
    let workspace = dunce::canonicalize(workspace).context("Aegis workspace is unavailable")?;
    let config = Config {
        installation_id: installation_id.clone(),
        actor_id: actor_id.clone(),
        workspace,
        relay_admin_url,
        admin_token_env,
        nats_url,
        nats_token_env,
        nats_root_certificate,
    };
    config.validate(root)?;

    // Persist the local identity before the request. If the network call fails,
    // rerunning `remote pair` rotates the relay code for the same identity.
    authority.register_actor(&actor_id)?;
    config.save(root)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let code = runtime.block_on(provision(&config))?;
    println!("Aegis remote pairing code (expires in 10 minutes):");
    println!("/pair {code}");
    println!("Send that command to the Aegis WhatsApp relay account.");
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProvisionResponse {
    user_id: String,
    installation_id: String,
    actor_id: String,
    pairing_code: String,
    pairing_expires_at: chrono::DateTime<Utc>,
}

async fn provision(config: &Config) -> Result<String> {
    let token = env_secret(&config.admin_token_env)?;
    let endpoint = admin_endpoint(&config.relay_admin_url)?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .build()?;
    let mut response = client
        .post(endpoint)
        .bearer_auth(token)
        .json(&json!({
            "installation_id":config.installation_id,
            "actor_id":config.actor_id,
        }))
        .send()
        .await
        .context("could not reach the remote relay admin endpoint")?;
    if !response.status().is_success() {
        bail!("relay rejected remote pairing (HTTP {})", response.status());
    }
    if response
        .content_length()
        .is_some_and(|length| length as usize > MAX_ADMIN_RESPONSE_BYTES)
    {
        bail!("relay pairing response exceeds its size limit");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("could not read relay pairing response")?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_ADMIN_RESPONSE_BYTES {
            bail!("relay pairing response exceeds its size limit");
        }
        bytes.extend_from_slice(&chunk);
    }
    let response: ProvisionResponse =
        serde_json::from_slice(&bytes).context("relay returned an invalid pairing response")?;
    if Uuid::parse_str(&response.user_id).is_err()
        || response.installation_id != config.installation_id
        || response.actor_id != config.actor_id
        || response.pairing_code.is_empty()
        || response.pairing_code.len() > 128
        || response.pairing_code.chars().any(char::is_whitespace)
        || response.pairing_code.chars().any(char::is_control)
        || response.pairing_expires_at <= Utc::now()
        || response.pairing_expires_at > Utc::now() + chrono::Duration::minutes(11)
    {
        bail!("relay pairing response does not match this local installation");
    }
    Ok(response.pairing_code)
}

fn admin_endpoint(base: &str) -> Result<reqwest::Url> {
    let mut endpoint = reqwest::Url::parse(base).context("relay admin URL is invalid")?;
    {
        let mut segments = endpoint
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("relay admin URL cannot contain an opaque path"))?;
        segments.pop_if_empty();
        segments.extend(["admin", "v1", "installations"]);
    }
    Ok(endpoint)
}

fn status(root: &Path) -> Result<()> {
    let config = Config::load(root)?;
    let authority = Authority::open(root)?;
    if authority.installation_id() != config.installation_id {
        bail!("remote configuration does not match the local installation identity");
    }
    let enabled: bool = authority.store.connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM remote_actors WHERE actor_id=?1 AND enabled=1)",
        [&config.actor_id],
        |row| row.get(0),
    )?;
    println!(
        "Remote control: {}",
        if enabled {
            "configured"
        } else {
            "revoked locally"
        }
    );
    println!("Installation: {}", config.installation_id);
    println!("Actor: {}", config.actor_id);
    println!(
        "Admin token environment variable: {}",
        env_presence(&config.admin_token_env)
    );
    println!(
        "NATS token environment variable: {}",
        env_presence(&config.nats_token_env)
    );
    println!("Relay: {}", config.relay_admin_url);
    println!("NATS: {}", config.nats_url);
    Ok(())
}

fn env_presence(name: &str) -> &'static str {
    if std::env::var(name).is_ok_and(|value| !value.is_empty()) {
        "set"
    } else {
        "missing"
    }
}

fn env_secret(name: &str) -> Result<String> {
    let value =
        std::env::var(name).with_context(|| format!("environment variable {name} is not set"))?;
    if value.is_empty() || value.len() > 16 * 1024 || value.chars().any(char::is_control) {
        bail!("environment variable {name} is empty or invalid");
    }
    Ok(value)
}

fn run_daemon(root: &Path) -> Result<()> {
    let config = Config::load(root)?;
    let authority = Authority::open(root)?;
    if authority.installation_id() != config.installation_id {
        bail!("remote configuration does not match the local installation identity");
    }
    let enabled: bool = authority.store.connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM remote_actors WHERE actor_id=?1 AND enabled=1)",
        [&config.actor_id],
        |row| row.get(0),
    )?;
    if !enabled {
        bail!("this local remote actor is revoked; run 'aegis remote pair' to rotate pairing");
    }
    let nats_token = env_secret(&config.nats_token_env)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run_loop(root.to_owned(), config, nats_token, authority))
}

async fn run_loop(
    root: std::path::PathBuf,
    config: Config,
    nats_token: String,
    mut authority: Authority,
) -> Result<()> {
    let mut options = async_nats::ConnectOptions::new()
        .token(nats_token)
        .require_tls(true);
    if let Some(root_certificate) = &config.nats_root_certificate {
        options = options.add_root_certificates(root_certificate.clone());
    }
    let client = options
        .connect(config.nats_url.as_str())
        .await
        .context("could not connect to the authenticated TLS NATS endpoint")?;
    let context = jetstream::new(client);
    let stream = context
        .get_stream(COMMAND_STREAM)
        .await
        .context("Aegis command stream is unavailable")?;
    let durable_name = format!("aegis-{}", config.installation_id);
    let consumer = stream
        .get_or_create_consumer(
            &durable_name,
            jetstream::consumer::pull::Config {
                durable_name: Some(durable_name.clone()),
                filter_subject: format!("aegis.commands.{}", config.installation_id),
                ack_wait: Duration::from_secs(45),
                ..Default::default()
            },
        )
        .await
        .context("could not create the installation-scoped command consumer")?;
    let transport = Transport {
        context,
        installation_id: config.installation_id.clone(),
    };
    let mut messages = consumer
        .messages()
        .await
        .context("could not pull remote commands")?;
    let mut maintenance = tokio::time::interval(Duration::from_secs(2));
    println!(
        "Aegis remote daemon connected for installation {}.",
        config.installation_id
    );

    loop {
        tokio::select! {
            signal = tokio::signal::ctrl_c() => {
                signal.context("could not listen for shutdown signal")?;
                break;
            }
            next = messages.next() => {
                let Some(message) = next else { break };
                let message = match message {
                    Ok(message) => message,
                    Err(_) => continue,
                };
                if let Err(error) = process_command(&root, &config, &transport, &mut authority, message).await {
                    let _ = error;
                    eprintln!("Aegis remote command was deferred for retry.");
                }
            }
            _ = maintenance.tick() => {
                if let Err(error) = maintain_remote_gates(&root, &mut authority) {
                    eprintln!("Aegis remote approval maintenance will retry: {error:#}");
                }
                if let Err(error) = publish_local_events(&mut authority, &transport).await {
                    eprintln!("Aegis remote event delivery will retry: {error:#}");
                }
            }
        }
    }
    Ok(())
}

async fn process_command(
    root: &Path,
    config: &Config,
    transport: &Transport,
    authority: &mut Authority,
    message: jetstream::Message,
) -> Result<()> {
    let subject = message.subject.as_str().to_owned();
    let envelope =
        match RelayCommandEnvelope::parse(&subject, &message.payload, &config.installation_id) {
            Ok(envelope) => envelope,
            Err(_) => {
                if message.ack().await.is_err() {
                    bail!("could not acknowledge invalid remote envelope");
                }
                return Ok(());
            }
        };
    if !actor_enabled(authority, &envelope.actor_id)? {
        if message.ack().await.is_err() {
            bail!("could not acknowledge unauthorized actor");
        }
        return Ok(());
    }

    let reply =
        match apply_relay_command(root, config, authority, &envelope, Utc::now().timestamp()) {
            Ok(receipt) => {
                apply_command_effects(root, authority, &envelope, &receipt)?;
                format_receipt(&envelope.command, &receipt)
            }
            Err(_) => rejection_reply(&envelope.command),
        };
    let event_id = stable_uuid(&format!(
        "reply\0{}\0{}\0{}",
        config.installation_id, envelope.actor_id, envelope.request_id
    ));
    let event = AegisEvent::reply(
        &config.installation_id,
        &envelope.actor_id,
        &event_id,
        &reply,
    )?;
    transport.publish(&event).await?;
    if message.ack().await.is_err() {
        bail!("could not acknowledge applied remote command");
    }
    Ok(())
}

fn apply_relay_command(
    root: &Path,
    config: &Config,
    authority: &mut Authority,
    relay: &RelayCommandEnvelope,
    now: i64,
) -> Result<super::Receipt> {
    let envelope = relay.local_envelope();
    let mut receipt = authority.apply_from_authenticated_relay(&relay.actor_id, &envelope, now)?;
    if receipt.result["needs_task_creation"] == true {
        let Command::Message {
            text,
            task_id: None,
        } = &envelope.command
        else {
            bail!("only a new message can create a remote task");
        };
        let run_id = stable_uuid(&format!(
            "run\0{}\0{}\0{}",
            config.installation_id, relay.actor_id, relay.request_id
        ));
        crate::session::create_remote_task(root, &config.workspace, text, &run_id)?;
        authority.grant_run(&relay.actor_id, &run_id)?;
        authority.bind_selected_task(&relay.actor_id, &run_id)?;
        initialize_event_cursor(authority, &relay.actor_id, &run_id)?;
        // Reuse the exact original envelope and stable ID so recovery after a
        // crash between task creation and receipt insertion stays idempotent.
        receipt = authority.apply_from_authenticated_relay(&relay.actor_id, &envelope, now)?;
        if receipt.result["needs_task_creation"] == true {
            bail!("new task could not be selected for its originating actor");
        }
    }
    if let Some(run_id) = receipt.result["task_id"].as_str() {
        initialize_event_cursor(authority, &relay.actor_id, run_id)?;
    }
    Ok(receipt)
}

fn apply_command_effects(
    root: &Path,
    _authority: &Authority,
    envelope: &RelayCommandEnvelope,
    receipt: &super::Receipt,
) -> Result<()> {
    let Some(run_id) = receipt.result["task_id"].as_str() else {
        return Ok(());
    };
    match &envelope.command {
        RelayCommand::Message { .. } => {
            let run = Store::open(root)?.run(run_id)?;
            if matches!(run.state.as_str(), "ready" | "running") {
                crate::kernel::spawn(root, run_id, None)?;
            }
            Ok(())
        }
        RelayCommand::Pause => {
            let run = Store::open(root)?.run(run_id)?;
            if !run.is_terminal() {
                crate::pause::request(root, run_id)?;
            }
            Ok(())
        }
        RelayCommand::Resume | RelayCommand::ApproveOnce { .. } | RelayCommand::Deny { .. } => {
            let mut store = Store::open(root)?;
            let run = store.run(run_id)?;
            if matches!(run.state.as_str(), "paused" | "waiting_recovery") {
                store.resume_paused(run_id)?;
            }
            if matches!(store.run(run_id)?.state.as_str(), "ready" | "running") {
                crate::kernel::spawn(root, run_id, None)?;
            }
            Ok(())
        }
        RelayCommand::ListTasks
        | RelayCommand::Status
        | RelayCommand::Cancel { .. }
        | RelayCommand::SelectTask { .. } => Ok(()),
    }
}

fn actor_enabled(authority: &Authority, actor_id: &str) -> Result<bool> {
    Ok(authority.store.connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM remote_actors WHERE actor_id=?1 AND enabled=1)",
        [actor_id],
        |row| row.get(0),
    )?)
}

fn format_receipt(command: &RelayCommand, receipt: &super::Receipt) -> String {
    let result = &receipt.result;
    match command {
        RelayCommand::Message { .. } => {
            let task = result["task_id"].as_str().unwrap_or("unknown");
            if result["interrupting_model"] == true {
                format!(
                    "Message added to task {task}; Aegis will apply it after the current response stops."
                )
            } else {
                format!("Aegis accepted your message for task {task}.")
            }
        }
        RelayCommand::ListTasks => {
            let Some(tasks) = result["tasks"].as_array() else {
                return "Aegis could not list tasks.".into();
            };
            if tasks.is_empty() {
                return "No tasks are currently shared with this phone.".into();
            }
            let mut lines = vec!["Tasks shared with this phone:".to_owned()];
            for task in tasks.iter().take(30) {
                let id = task["task_id"].as_str().unwrap_or("unknown");
                let state = task["state"].as_str().unwrap_or("unknown");
                let name = task["task"].as_str().unwrap_or("Task");
                lines.push(format!("{id} [{state}] {name}"));
            }
            lines.join("\n")
        }
        RelayCommand::Status => {
            let state = result["state"].as_str().unwrap_or("unknown");
            let id = result["task_id"].as_str().unwrap_or("unknown");
            let task = result["task"].as_str().unwrap_or("Task");
            let summary = result["summary"].as_str().unwrap_or("");
            if summary.trim().is_empty() {
                format!("Task {id} [{state}]: {task}")
            } else {
                format!("Task {id} [{state}]: {task}\n{summary}")
            }
        }
        RelayCommand::Pause => "Aegis will pause this task at its next safe boundary.".into(),
        RelayCommand::Resume => "Aegis has resumed this task.".into(),
        RelayCommand::Cancel { .. } => "Aegis marked this task cancelled.".into(),
        RelayCommand::SelectTask { task_id } => {
            format!("Selected task {task_id}. New messages will be sent to it.")
        }
        RelayCommand::ApproveOnce { .. } => {
            "Aegis recorded approval for that exact operation.".into()
        }
        RelayCommand::Deny { .. } => "Aegis recorded the denial for that exact operation.".into(),
    }
}

fn rejection_reply(command: &RelayCommand) -> String {
    match command {
        RelayCommand::Message { .. } => "Aegis could not start or update that task. Check the local Aegis setup, then send the request again.".into(),
        RelayCommand::ListTasks => "Aegis could not list tasks right now.".into(),
        RelayCommand::Status | RelayCommand::Pause | RelayCommand::Resume | RelayCommand::Cancel { .. } => {
            "Aegis could not apply that task command. Check /tasks and try again.".into()
        }
        RelayCommand::SelectTask { .. } => "Aegis could not select that task. Use /tasks to see tasks shared with this phone.".into(),
        RelayCommand::ApproveOnce { .. } | RelayCommand::Deny { .. } => "Aegis could not resolve that approval. It may have expired or already been decided.".into(),
    }
}

fn maintain_remote_gates(root: &Path, authority: &mut Authority) -> Result<()> {
    let now = Utc::now().timestamp();
    let mut statement = authority.store.connection.prepare(
        "SELECT DISTINCT run_id FROM remote_operation_gates WHERE state='pending' AND expires_at<=?1",
    )?;
    let due = statement
        .query_map([now], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    super::expire_remote_approval_gates(&mut authority.store, now)?;
    for run_id in due {
        let run = authority.store.run(&run_id)?;
        if run.budgets["remote_origin"] == true
            && matches!(run.state.as_str(), "paused" | "waiting_recovery")
        {
            authority.store.resume_paused(&run_id)?;
            crate::kernel::spawn(root, &run_id, None)?;
        }
    }
    Ok(())
}

async fn publish_local_events(authority: &mut Authority, transport: &Transport) -> Result<()> {
    let runs = authority.store.runs()?;
    for run in runs {
        for actor_id in authority.actors_for_run(&run.id)? {
            initialize_event_cursor(authority, &actor_id, &run.id)?;
            let cursor = event_cursor(authority, &actor_id, &run.id)?;
            for event in authority.store.events_since(&run.id, cursor)? {
                let outgoing = outbound_events(&run, &event, &transport.installation_id, &actor_id);
                for outgoing in outgoing {
                    transport.publish(&outgoing).await?;
                }
                advance_event_cursor(authority, &actor_id, &run.id, event.seq)?;
            }
        }
    }
    Ok(())
}

fn initialize_event_cursor(authority: &Authority, actor_id: &str, run_id: &str) -> Result<()> {
    ensure_cursor_schema(authority)?;
    let last_seq: i64 = authority.store.connection.query_row(
        "SELECT COALESCE((SELECT last_seq FROM run_projection WHERE run_id=?1),0)",
        [run_id],
        |row| row.get(0),
    )?;
    authority.store.connection.execute(
        "INSERT OR IGNORE INTO remote_event_cursors(actor_id,run_id,last_seq) VALUES (?1,?2,?3)",
        params![actor_id, run_id, last_seq],
    )?;
    Ok(())
}

fn event_cursor(authority: &Authority, actor_id: &str, run_id: &str) -> Result<i64> {
    Ok(authority.store.connection.query_row(
        "SELECT last_seq FROM remote_event_cursors WHERE actor_id=?1 AND run_id=?2",
        params![actor_id, run_id],
        |row| row.get(0),
    )?)
}

fn advance_event_cursor(
    authority: &Authority,
    actor_id: &str,
    run_id: &str,
    seq: i64,
) -> Result<()> {
    authority.store.connection.execute(
        "UPDATE remote_event_cursors SET last_seq=MAX(last_seq,?3) WHERE actor_id=?1 AND run_id=?2",
        params![actor_id, run_id, seq],
    )?;
    Ok(())
}

fn outbound_events(
    run: &Run,
    event: &Event,
    installation_id: &str,
    actor_id: &str,
) -> Vec<AegisEvent> {
    let task_id = Some(run.id.clone());
    let make = |suffix: &str, kind: EventKind| AegisEvent {
        event_id: stable_uuid(&format!(
            "local-event\0{}\0{}\0{}\0{}\0{}",
            installation_id, actor_id, run.id, event.seq, suffix
        )),
        installation_id: installation_id.to_owned(),
        actor_id: actor_id.to_owned(),
        kind,
        task_id: task_id.clone(),
        challenge_id: None,
        display_detail: None,
        reply_text: None,
    };
    match event.kind.as_str() {
        "run.running" => vec![make("started", EventKind::TaskStarted)],
        "run.ready" => vec![make("progress", EventKind::Progress)],
        "approval.required" => {
            let challenge_id = event.payload["challenge_id"].as_str();
            let descriptor = event.payload["descriptor"].as_str();
            let (Some(challenge_id), Some(descriptor)) = (challenge_id, descriptor) else {
                return Vec::new();
            };
            let mut event = make("approval", EventKind::ApprovalRequired);
            event.challenge_id = Some(challenge_id.to_owned());
            let descriptor = crate::text::clean(descriptor)
                .replace(['\r', '\n', '\t'], " ")
                .chars()
                .take(160)
                .collect::<String>();
            if descriptor.trim().is_empty() {
                return Vec::new();
            }
            // The payload is deliberately reduced to a short capability and
            // target descriptor; no arguments or tool output are transported.
            event.display_detail = Some(descriptor);
            vec![event]
        }
        "run.paused" => vec![make("blocked", EventKind::Blocked)],
        "run.waiting_recovery" if event.payload["reason"] != "remote_approval_required" => {
            vec![make("blocked", EventKind::Blocked)]
        }
        "run.waiting_recovery" => Vec::new(),
        "run.completed" | "run.answered" => {
            let mut events = vec![make("completed", EventKind::Completed)];
            let summary = event.payload["summary"].as_str().unwrap_or_default();
            let summary = crate::text::clean(summary);
            if !summary.trim().is_empty() {
                let reply_id = stable_uuid(&format!(
                    "local-event\0{}\0{}\0{}\0{}\0reply",
                    installation_id, actor_id, run.id, event.seq
                ));
                if let Ok(reply) = AegisEvent::reply(installation_id, actor_id, &reply_id, &summary)
                {
                    events.push(reply);
                }
            }
            events
        }
        "run.failed" => vec![make("failed", EventKind::Failed)],
        _ => Vec::new(),
    }
}

fn stable_uuid(name: &str) -> String {
    let digest = Sha256::digest(name.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes).to_string()
}

impl Transport {
    async fn publish(&self, event: &AegisEvent) -> Result<()> {
        let subject = format!("aegis.events.{}", self.installation_id);
        let mut headers = HeaderMap::new();
        headers.insert("Nats-Msg-Id", event.event_id.as_str());
        self.context
            .publish_with_headers(subject, headers, event.to_json()?.into())
            .await
            .context("could not publish remote Aegis event")?
            .await
            .context("NATS did not acknowledge remote Aegis event")?;
        Ok(())
    }
}

fn ensure_cursor_schema(authority: &Authority) -> Result<()> {
    authority.store.connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS remote_event_cursors (
            actor_id TEXT NOT NULL REFERENCES remote_actors(actor_id),
            run_id TEXT NOT NULL REFERENCES runs(id),
            last_seq INTEGER NOT NULL,
            PRIMARY KEY(actor_id,run_id)
        );",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;

    fn fixture(root: &Path) -> Result<(Config, Authority, RelayCommandEnvelope)> {
        std::fs::create_dir_all(root)?;
        std::fs::create_dir_all(root.join(".arun"))?;
        std::fs::write(
            root.join(".arun").join("profile.json"),
            br#"{"provider":"codex","model":"gpt-6.1-sol","write":false,"image":null,"endpoint":null}"#,
        )?;
        let mut authority = Authority::open(&root.join(".arun"))?;
        let actor_id = Uuid::new_v4().to_string();
        authority.register_actor(&actor_id)?;
        let config = Config {
            installation_id: authority.installation_id().to_owned(),
            actor_id: actor_id.clone(),
            workspace: dunce::canonicalize(root)?,
            relay_admin_url: "https://relay.example".into(),
            admin_token_env: "AEGIS_RELAY_ADMIN_TOKEN".into(),
            nats_url: "tls://nats.example:4222".into(),
            nats_token_env: "AEGIS_NATS_TOKEN".into(),
            nats_root_certificate: None,
        };
        let now = Utc::now();
        let relay = RelayCommandEnvelope {
            envelope_id: Uuid::new_v4().to_string(),
            request_id: Uuid::new_v4().to_string(),
            installation_id: config.installation_id.clone(),
            actor_id,
            channel: "whatsapp".into(),
            sender_id: "+15551234567".into(),
            external_message_id: "provider-message-id".into(),
            issued_at: now,
            expires_at: now + ChronoDuration::minutes(5),
            command: RelayCommand::Message {
                text: "Review the parser".into(),
            },
        };
        Ok((config, authority, relay))
    }

    #[test]
    fn admin_pairing_response_matches_the_relay_wire_contract() -> Result<()> {
        let installation_id = Uuid::new_v4().to_string();
        let actor_id = Uuid::new_v4().to_string();
        let value = json!({
            "user_id":Uuid::new_v4().to_string(),
            "installation_id":installation_id,
            "actor_id":actor_id,
            "pairing_code":"one-time-code",
            "pairing_expires_at":Utc::now() + chrono::Duration::minutes(10),
        });
        let response: ProvisionResponse = serde_json::from_value(value)?;
        assert_eq!(response.installation_id, installation_id);
        assert_eq!(response.actor_id, actor_id);
        assert_eq!(response.pairing_code, "one-time-code");
        Ok(())
    }

    #[test]
    fn first_message_creates_one_stable_local_task_and_receipt() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        let root = workspace.path().join(".arun");
        let (config, mut authority, relay) = fixture(workspace.path())?;
        let now = Utc::now().timestamp();

        let first = apply_relay_command(&root, &config, &mut authority, &relay, now)?;
        let second = apply_relay_command(&root, &config, &mut authority, &relay, now)?;
        assert!(!first.duplicate);
        assert!(second.duplicate);
        assert_eq!(first.result["task_id"], second.result["task_id"]);
        let task_id = first.result["task_id"].as_str().unwrap();
        assert_eq!(authority.store.runs()?.len(), 1);
        assert_eq!(authority.authorized_runs(&relay.actor_id)?.len(), 1);
        assert_eq!(
            authority.selected_task(&relay.actor_id)?.as_deref(),
            Some(task_id)
        );
        assert_eq!(authority.store.event_count(task_id, "user.steering")?, 1);
        assert_eq!(
            authority.store.run(task_id)?.budgets["model"],
            "gpt-6.1-sol"
        );
        Ok(())
    }

    #[test]
    fn task_cursors_start_at_current_history_and_actor_scopes_do_not_overlap() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        let root = workspace.path().join(".arun");
        let (config, mut authority, relay) = fixture(workspace.path())?;
        let now = Utc::now().timestamp();
        let receipt = apply_relay_command(&root, &config, &mut authority, &relay, now)?;
        let task_id = receipt.result["task_id"].as_str().unwrap();
        let cursor = event_cursor(&authority, &relay.actor_id, task_id)?;
        assert!(cursor < authority.store.events(task_id)?.last().unwrap().seq);

        let other_actor = Uuid::new_v4().to_string();
        authority.register_actor(&other_actor)?;
        let other_run = authority.store.create_run(
            "private local task",
            workspace.path(),
            "codex",
            json!([]),
            json!({}),
            "",
        )?;
        authority.grant_run(&other_actor, &other_run.id)?;
        assert!(authority.actors_for_run(task_id)?.contains(&relay.actor_id));
        assert!(!authority.actors_for_run(task_id)?.contains(&other_actor));
        assert!(
            authority
                .authorized_runs(&relay.actor_id)?
                .iter()
                .all(|task| { task["task_id"].as_str() != Some(other_run.id.as_str()) })
        );
        Ok(())
    }

    #[test]
    fn outbound_projection_uses_only_allowlisted_summaries_and_stable_ids() {
        let installation_id = Uuid::new_v4().to_string();
        let run = Run {
            id: Uuid::new_v4().to_string(),
            task: "task".into(),
            workspace: ".".into(),
            provider: "codex".into(),
            grants: json!([]),
            budgets: json!({}),
            acceptance: String::new(),
            state: "completed".into(),
            created_at: 1,
        };
        let event = Event {
            seq: 9,
            kind: "run.completed".into(),
            payload: json!({"summary":"done","raw_output":"not sent"}),
            created_at: 2,
        };
        let first = outbound_events(&run, &event, &installation_id, "actor-1");
        let second = outbound_events(&run, &event, &installation_id, "actor-1");
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].event_id, second[0].event_id);
        assert_eq!(first[1].reply_text.as_deref(), Some("done"));
        assert_eq!(first[0].actor_id, "actor-1");
        let approval = Event {
            seq: 10,
            kind: "approval.required".into(),
            payload: json!({"challenge_id":Uuid::new_v4().to_string(),"descriptor":"workspace.write · file.txt"}),
            created_at: 3,
        };
        let approval = outbound_events(&run, &approval, &installation_id, "actor-1");
        assert_eq!(approval.len(), 1);
        assert!(approval[0].challenge_id.is_some());
        assert!(
            approval[0]
                .display_detail
                .as_deref()
                .is_some_and(|detail| !detail.contains('\n'))
        );
        assert!(
            outbound_events(
                &run,
                &Event {
                    kind: "model.response".into(),
                    ..event
                },
                &installation_id,
                "actor-1"
            )
            .is_empty()
        );
        assert!(
            outbound_events(
                &run,
                &Event {
                    seq: 11,
                    kind: "run.waiting_recovery".into(),
                    payload: json!({"reason":"remote_approval_required"}),
                    created_at: 4,
                },
                &installation_id,
                "actor-1"
            )
            .is_empty()
        );
    }
}
