//! Pairing and the outbound-only NATS daemon for remote Aegis control.

use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    time::Duration,
};

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
const RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(1);
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(30);

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
        Some("share") if args.len() == 2 => share_task(root, &args[1]),
        Some("share") => bail!("usage: aegis remote share <run-id>"),
        Some("unshare") if args.len() == 2 => unshare_task(root, &args[1]),
        Some("unshare") => bail!("usage: aegis remote unshare <run-id>"),
        Some("revoke") if args.len() == 1 => revoke(root),
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
    println!("aegis remote share <run-id> | unshare <run-id>");
    println!("aegis remote status | revoke | run");
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
    let actor_id = actor_for_pair(&authority, existing.as_ref())?;
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

    // Drain relay bindings for every locally revoked identity before the new
    // pairing registers another actor. A failed cleanup leaves the old IDs in
    // SQLite, so both `remote revoke` and a later `remote pair` can retry.
    let revoked_actor_ids = authority.pending_relay_revocation_actor_ids()?;
    if !revoked_actor_ids.is_empty() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        runtime.block_on(revoke_relay_actors(
            &config,
            &revoked_actor_ids,
            &mut authority,
        ))?;
    }

    // Persist the local identity before the request. If the network call fails,
    // rerunning `remote pair` reuses this enabled identity; a revoked identity
    // is replaced above so a stale relay binding cannot become valid again.
    authority.register_actor(&actor_id)?;
    config.save(root)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let code = runtime.block_on(provision(&config))?;
    println!("Aegis remote pairing code (expires in 5 minutes):");
    println!("Send either: AEGIS {code}  or  /pair {code}");
    println!("to the Aegis WhatsApp relay account.");
    Ok(())
}

fn actor_for_pair(authority: &Authority, existing: Option<&Config>) -> Result<String> {
    if let Some(config) = existing
        && actor_enabled(authority, &config.actor_id)?
    {
        return Ok(config.actor_id.clone());
    }
    // A revoked actor may still have a stale relay binding if cloud cleanup
    // failed. Never re-enable that identity: leave it locally disabled and
    // make the new pairing code name a fresh actor with no inherited task grants.
    Ok(Uuid::new_v4().to_string())
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
        || response.pairing_expires_at > Utc::now() + chrono::Duration::minutes(6)
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

fn share_task(root: &Path, run_id: &str) -> Result<()> {
    let config = Config::load(root)?;
    let mut authority = Authority::open(root)?;
    if authority.installation_id() != config.installation_id {
        bail!("remote configuration does not match the local installation identity");
    }
    authority.grant_run(&config.actor_id, run_id)?;
    println!(
        "Shared task {run_id} with remote actor {}.",
        config.actor_id
    );
    Ok(())
}

fn unshare_task(root: &Path, run_id: &str) -> Result<()> {
    let config = Config::load(root)?;
    let mut authority = Authority::open(root)?;
    if authority.installation_id() != config.installation_id {
        bail!("remote configuration does not match the local installation identity");
    }
    if authority.revoke_run(&config.actor_id, run_id)? {
        println!(
            "Unshared task {run_id} from remote actor {}.",
            config.actor_id
        );
    } else {
        println!(
            "Task {run_id} was not shared with remote actor {}.",
            config.actor_id
        );
    }
    Ok(())
}

fn revoke(root: &Path) -> Result<()> {
    let (config, newly_revoked, revoked_actor_ids, mut authority) = revoke_locally(root)?;
    if newly_revoked {
        println!("Remote actor revoked locally.");
    } else {
        println!("Remote actor was already revoked locally.");
    }

    if revoked_actor_ids.is_empty() {
        println!("No locally revoked actor bindings are pending relay cleanup.");
        return Ok(());
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(revoke_relay_actors(
        &config,
        &revoked_actor_ids,
        &mut authority,
    ))?;
    println!("Pending remote channel bindings and pairing tokens revoked at relay.");
    Ok(())
}

fn revoke_locally(root: &Path) -> Result<(Config, bool, Vec<String>, Authority)> {
    let config = Config::load(root)?;
    let mut authority = Authority::open(root)?;
    if authority.installation_id() != config.installation_id {
        bail!("remote configuration does not match the local installation identity");
    }
    let newly_revoked = authority.revoke_actor(&config.actor_id)?;
    let revoked_actor_ids = authority.pending_relay_revocation_actor_ids()?;
    Ok((config, newly_revoked, revoked_actor_ids, authority))
}

fn actor_revoke_endpoint(config: &Config, actor_id: &str) -> Result<reqwest::Url> {
    let mut endpoint = admin_endpoint(&config.relay_admin_url)?;
    endpoint
        .path_segments_mut()
        .map_err(|_| anyhow::anyhow!("relay admin URL cannot contain an opaque path"))?
        .extend([
            config.installation_id.as_str(),
            "actors",
            actor_id,
            "bindings",
            "revoke",
        ]);
    Ok(endpoint)
}

async fn revoke_relay_actor(config: &Config, actor_id: &str) -> Result<()> {
    let token = env_secret(&config.admin_token_env)?;
    let endpoint = actor_revoke_endpoint(config, actor_id)?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .build()?;
    let response = client
        .post(endpoint)
        .bearer_auth(token)
        .send()
        .await
        .context("could not reach the remote relay admin endpoint")?;
    if response.status() != reqwest::StatusCode::NO_CONTENT {
        bail!(
            "relay rejected remote actor revocation (HTTP {})",
            response.status()
        );
    }
    Ok(())
}

async fn revoke_relay_actors(
    config: &Config,
    actor_ids: &[String],
    authority: &mut Authority,
) -> Result<()> {
    let mut failed = Vec::new();
    for actor_id in actor_ids {
        if revoke_relay_actor(config, actor_id).await.is_err() {
            failed.push(actor_id.as_str());
        } else if authority.mark_relay_actor_revoked(actor_id).is_err() {
            // The relay operation is idempotent. Leave this actor pending for
            // retry and continue cleaning up the remaining locally revoked IDs.
            failed.push(actor_id.as_str());
        }
    }
    if !failed.is_empty() {
        bail!(
            "local revocation remains active, but relay cleanup failed for actor IDs: {}; retry `aegis remote revoke` or `aegis remote pair` when the relay is reachable",
            failed.join(", ")
        );
    }
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
    authority: Authority,
) -> Result<()> {
    let mut session = DaemonSession {
        root,
        config,
        nats_token,
        authority,
    };
    supervise_sessions(
        &mut session,
        async {
            tokio::signal::ctrl_c()
                .await
                .context("could not listen for shutdown signal")
        },
        ReconnectBackoff::new(RECONNECT_INITIAL_DELAY, RECONNECT_MAX_DELAY),
    )
    .await
}

type SessionFuture<'a> = Pin<Box<dyn Future<Output = Result<()>> + 'a>>;

trait SessionRunner {
    fn run_once(&mut self) -> SessionFuture<'_>;
}

struct DaemonSession {
    root: PathBuf,
    config: Config,
    nats_token: String,
    authority: Authority,
}

impl SessionRunner for DaemonSession {
    fn run_once(&mut self) -> SessionFuture<'_> {
        Box::pin(run_nats_session(
            &self.root,
            &self.config,
            &self.nats_token,
            &mut self.authority,
        ))
    }
}

#[derive(Clone, Copy)]
struct ReconnectBackoff {
    next: Duration,
    max: Duration,
}

impl ReconnectBackoff {
    fn new(initial: Duration, max: Duration) -> Self {
        Self {
            next: initial,
            max: max.max(initial),
        }
    }

    fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = self.next.saturating_mul(2).min(self.max);
        delay
    }
}

async fn supervise_sessions<R, S>(
    session: &mut R,
    shutdown: S,
    mut backoff: ReconnectBackoff,
) -> Result<()>
where
    R: SessionRunner,
    S: Future<Output = Result<()>>,
{
    let mut shutdown = Box::pin(shutdown);
    loop {
        let result = tokio::select! {
            biased;
            signal = &mut shutdown => {
                signal?;
                return Ok(());
            }
            result = session.run_once() => result,
        };
        if result.is_ok() {
            return Ok(());
        }

        let delay = backoff.next_delay();
        eprintln!(
            "Aegis remote connection ended; retrying in {} seconds.",
            delay.as_secs_f64()
        );
        tokio::select! {
            biased;
            signal = &mut shutdown => {
                signal?;
                return Ok(());
            }
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

async fn run_nats_session(
    root: &Path,
    config: &Config,
    nats_token: &str,
    authority: &mut Authority,
) -> Result<()> {
    let mut options = async_nats::ConnectOptions::new()
        .user_and_password(config.nats_username()?, nats_token.to_owned())
        .custom_inbox_prefix(format!("_INBOX.aegis.device.{}", config.installation_id))
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
        .get_stream_no_info(COMMAND_STREAM)
        .await
        .context("Aegis command stream is unavailable")?;
    let durable_name = format!("aegis-{}", config.installation_id);
    let consumer = stream
        .get_or_create_consumer(
            &durable_name,
            jetstream::consumer::pull::Config {
                durable_name: Some(durable_name.clone()),
                name: Some(durable_name.clone()),
                filter_subject: format!("aegis.commands.{}", config.installation_id),
                ack_policy: jetstream::consumer::AckPolicy::Explicit,
                ack_wait: Duration::from_secs(45),
                ..Default::default()
            },
        )
        .await
        .context("could not create the installation-scoped command consumer")?;
    validate_command_consumer(consumer.cached_info(), &config.installation_id)?;
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
            next = messages.next() => {
                let Some(message) = next else {
                    bail!("NATS command consumer ended");
                };
                let message = message.map_err(|_| anyhow::anyhow!("NATS command consumer failed"))?;
                if let Err(error) = process_command(root, config, &transport, authority, message).await {
                    let _ = error;
                    eprintln!("Aegis remote command was deferred for retry.");
                }
            }
            _ = maintenance.tick() => {
                if maintain_remote_gates(root, authority).is_err() {
                    eprintln!("Aegis remote approval maintenance failed; retrying.");
                }
                if publish_local_events(authority, &transport).await.is_err() {
                    eprintln!("Aegis remote event delivery failed; retrying.");
                }
            }
        }
    }
}

fn validate_command_consumer(
    info: &jetstream::consumer::Info,
    installation_id: &str,
) -> Result<()> {
    let durable_name = format!("aegis-{installation_id}");
    let expected_subject = format!("aegis.commands.{installation_id}");
    let config = &info.config;
    if info.stream_name != COMMAND_STREAM
        || info.name != durable_name
        || config.durable_name.as_deref() != Some(durable_name.as_str())
        || config.name.as_deref() != Some(durable_name.as_str())
        || config.filter_subject != expected_subject
        || config.deliver_subject.is_some()
        || config.deliver_group.is_some()
        || config.ack_policy != jetstream::consumer::AckPolicy::Explicit
    {
        bail!("NATS command consumer does not match this installation's authority");
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
            Err(error) => command_failure_reply(&envelope.command, error)?,
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
        RelayCommand::Pause { .. } => {
            let run = Store::open(root)?.run(run_id)?;
            if !run.is_terminal() {
                crate::pause::request(root, run_id)?;
            }
            Ok(())
        }
        RelayCommand::Resume { .. }
        | RelayCommand::ApproveOnce { .. }
        | RelayCommand::Deny { .. } => {
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
        | RelayCommand::Status { .. }
        | RelayCommand::Result { .. }
        | RelayCommand::Evidence { .. }
        | RelayCommand::Details { .. }
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
            let mut body_bytes = lines[0].len();
            let mut listed = 0usize;
            for task in tasks.iter().take(30) {
                let alias = task["alias"].as_str().unwrap_or("unknown");
                let state = task["state"].as_str().unwrap_or("unknown");
                let name = task["task"]
                    .as_str()
                    .map(short_task_label)
                    .unwrap_or_else(|| "Task".into());
                let line = format!("{alias} [{state}] {name}");
                if body_bytes + line.len() + 1 + 160 > 3_600 {
                    break;
                }
                body_bytes += line.len() + 1;
                lines.push(line);
                listed += 1;
            }
            if tasks.len() > listed {
                lines.push(format!(
                    "{} more task(s) omitted to fit this message.",
                    tasks.len() - listed
                ));
            }
            lines.push(
                "Use /use <alias> to select a task; /status [alias], /result [alias], /evidence [alias] and /details [alias] inspect it.".into(),
            );
            lines.join("\n")
        }
        RelayCommand::Status { .. } => {
            let state = result["state"].as_str().unwrap_or("unknown");
            let id = result["alias"]
                .as_str()
                .or_else(|| result["task_id"].as_str())
                .unwrap_or("unknown");
            let task = result["task"].as_str().unwrap_or("Task");
            let summary = result["summary"].as_str().unwrap_or("");
            if summary.trim().is_empty() {
                format!("Task {id} [{state}]: {task}")
            } else {
                format!("Task {id} [{state}]: {task}\n{summary}")
            }
        }
        RelayCommand::Result { .. } => format_task_result(result),
        RelayCommand::Evidence { .. } => format_task_evidence(result),
        RelayCommand::Details { .. } => format_task_details(result),
        RelayCommand::Pause { .. } => {
            "Aegis will pause this task at its next safe boundary.".into()
        }
        RelayCommand::Resume { .. } => "Aegis has resumed this task.".into(),
        RelayCommand::Cancel { .. } => "Aegis marked this task cancelled.".into(),
        RelayCommand::SelectTask { task_id } => {
            let alias = result["alias"].as_str().unwrap_or(task_id);
            format!("Selected task {alias}. New messages will be sent to it.")
        }
        RelayCommand::ApproveOnce { .. } => {
            "Aegis recorded approval for that exact operation.".into()
        }
        RelayCommand::Deny { .. } => "Aegis recorded the denial for that exact operation.".into(),
    }
}

fn short_task_label(task: &str) -> String {
    crate::text::clean(task)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(48)
        .collect()
}

fn rejection_reply(command: &RelayCommand) -> String {
    match command {
        RelayCommand::Message { .. } => "Aegis could not start or update that task. Check the local Aegis setup, then send the request again.".into(),
        RelayCommand::ListTasks => "Aegis could not list tasks right now.".into(),
        RelayCommand::Status { .. }
        | RelayCommand::Result { .. }
        | RelayCommand::Evidence { .. }
        | RelayCommand::Details { .. }
        | RelayCommand::Pause { .. }
        | RelayCommand::Resume { .. }
        | RelayCommand::Cancel { .. } => {
            "Aegis could not apply that task command. Check /tasks and try again.".into()
        }
        RelayCommand::SelectTask { .. } => "Aegis could not select that task. Use /tasks to see tasks shared with this phone.".into(),
        RelayCommand::ApproveOnce { .. } | RelayCommand::Deny { .. } => "Aegis could not resolve that approval. It may have expired or already been decided.".into(),
    }
}

fn format_task_result(result: &serde_json::Value) -> String {
    let alias = result["alias"].as_str().unwrap_or("unknown");
    let state = result["state"].as_str().unwrap_or("unknown");
    let verification = result["verification"].as_str().unwrap_or("not verified");
    let Some(summary) = result["summary"]
        .as_str()
        .filter(|summary| !summary.trim().is_empty())
    else {
        return format!("Task {alias} [{state}; {verification}] has no saved final answer.");
    };
    let summary = crate::text::clean(summary);
    let summary = super::protocol::truncate_utf8(&summary, super::MAX_REMOTE_RESULT_BYTES);
    format!("Final answer for {alias} [{state}; {verification}]:\n{summary}")
}

fn format_task_evidence(result: &serde_json::Value) -> String {
    let alias = result["alias"].as_str().unwrap_or("unknown");
    let state = result["state"].as_str().unwrap_or("unknown");
    if state == "answered" {
        return format!(
            "Task {alias} [answered; unverified]. Conversational answers have no tool receipts."
        );
    }
    let Some(receipts) = result["receipts"].as_array() else {
        return format!("No verified current evidence is available for task {alias}.");
    };
    let mut lines = vec![format!("Verified current evidence for {alias} [{state}]:")];
    if receipts.is_empty() {
        lines.push("No verified current evidence receipts.".into());
    } else {
        for receipt in receipts {
            let capability = receipt["capability"].as_str().unwrap_or("operation");
            let hash = receipt["hash_prefix"].as_str().unwrap_or("unknown");
            let bytes = receipt["bytes"].as_i64().unwrap_or(0).max(0);
            lines.push(format!("{capability} · {hash} · {bytes} bytes"));
        }
    }
    let omitted = result["omitted"].as_u64().unwrap_or(0);
    if omitted > 0 {
        lines.push(format!("{omitted} more receipt(s) omitted."));
    }
    lines.join("\n")
}

fn format_task_details(result: &serde_json::Value) -> String {
    let alias = result["alias"].as_str().unwrap_or("unknown");
    let state = result["state"].as_str().unwrap_or("unknown");
    let task = result["task"].as_str().unwrap_or("Task");
    let provider = result["provider"].as_str().unwrap_or("unknown");
    let model = result["model"].as_str().unwrap_or("default");
    let turns = result["model_turns"].as_i64().unwrap_or(0);
    let successful = result["successful_actions"].as_i64().unwrap_or(0);
    let pending = result["pending_actions"].as_i64().unwrap_or(0);
    let uncertain = result["uncertain_actions"].as_i64().unwrap_or(0);
    let model_tokens = result["model_tokens"].as_i64().unwrap_or(0);
    let obligations = &result["obligations"];
    let open = obligations["open"].as_i64().unwrap_or(0);
    let verified = obligations["verified"].as_i64().unwrap_or(0);
    let stale = obligations["stale"].as_i64().unwrap_or(0);
    let superseded = obligations["superseded"].as_i64().unwrap_or(0);
    let selected = result["selected"].as_bool().unwrap_or(false);
    let created = result["created_at"]
        .as_i64()
        .map(format_utc_timestamp)
        .unwrap_or_else(|| "unknown".into());
    let started = result["started_at"]
        .as_i64()
        .map(format_utc_timestamp)
        .unwrap_or_else(|| "not started".into());
    let elapsed = result["elapsed_seconds"]
        .as_i64()
        .map(format_elapsed)
        .unwrap_or_else(|| "not started".into());
    format!(
        "Task {alias} [{state}]{}\n{task}\nProvider: {provider} · Model: {model}\nCreated: {created}\nStarted: {started} · Elapsed: {elapsed}\nEvidence: {turns} model turns, {successful} successful actions, {pending} pending, {uncertain} uncertain · {model_tokens} model tokens.\nExplicit requirements: {open} open, {verified} verified, {stale} stale, {superseded} superseded.",
        if selected { " · selected" } else { "" },
    )
}

fn format_utc_timestamp(timestamp: i64) -> String {
    chrono::DateTime::<Utc>::from_timestamp(timestamp, 0)
        .map(|value| value.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| "unknown".into())
}

fn format_elapsed(seconds: i64) -> String {
    let seconds = seconds.max(0) as u64;
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let seconds = seconds % 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

fn command_failure_reply(command: &RelayCommand, error: anyhow::Error) -> Result<String> {
    if super::is_command_rejection(&error) {
        Ok(rejection_reply(command))
    } else {
        // Internal SQLite, filesystem, profile, or process failures must leave
        // the JetStream message unacknowledged so the command can be retried.
        Err(error)
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
        expires_at: None,
        display_detail: None,
        reply_text: None,
    };
    match event.kind.as_str() {
        "run.running" => vec![make("started", EventKind::TaskStarted)],
        "run.ready" => vec![make("progress", EventKind::Progress)],
        "approval.required" => {
            let challenge_id = event.payload["challenge_id"].as_str();
            let descriptor = event.payload["descriptor"].as_str();
            let expires_at = event.payload["expires_at"].as_i64();
            let (Some(challenge_id), Some(descriptor), Some(expires_at)) =
                (challenge_id, descriptor, expires_at)
            else {
                return Vec::new();
            };
            if expires_at <= crate::storage::unix_time() {
                return Vec::new();
            }
            let mut event = make("approval", EventKind::ApprovalRequired);
            event.challenge_id = Some(challenge_id.to_owned());
            event.expires_at = Some(expires_at);
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
        // Completion payload summaries are model-authored and may quote
        // workspace content. Push only the state transition; the actor can
        // request bounded task details separately.
        "run.completed" | "run.answered" => vec![make("completed", EventKind::Completed)],
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
    use std::collections::VecDeque;

    #[test]
    fn nats_command_consumer_rejects_existing_authority_mismatches() -> Result<()> {
        let installation_id = "12345678-abcd-4321-8765-123456789abc";
        let name = format!("aegis-{installation_id}");
        let valid = json!({
            "stream_name": COMMAND_STREAM,
            "name": name,
            "created": "2026-10-01T00:00:00Z",
            "config": jetstream::consumer::Config {
                name: Some(name.clone()),
                durable_name: Some(name.clone()),
                filter_subject: format!("aegis.commands.{installation_id}"),
                ack_policy: jetstream::consumer::AckPolicy::Explicit,
                ..Default::default()
            },
            "delivered": {"consumer_seq": 0, "stream_seq": 0},
            "ack_floor": {"consumer_seq": 0, "stream_seq": 0},
            "num_ack_pending": 0, "num_redelivered": 0,
            "num_waiting": 0, "num_pending": 0
        });
        let info = serde_json::from_value(valid.clone())?;
        validate_command_consumer(&info, installation_id)?;
        for (field, value) in [
            ("name", json!("aegis-other")),
            ("durable_name", json!("aegis-other")),
            ("durable_name", json!(null)),
            ("filter_subject", json!("aegis.commands.*")),
            ("filter_subject", json!("")),
            ("deliver_subject", json!("_INBOX.aegis.relay.inject")),
            ("deliver_group", json!("other")),
            ("ack_policy", json!("all")),
            ("ack_policy", json!("none")),
        ] {
            let mut invalid = valid.clone();
            invalid["config"][field] = value;
            let info = serde_json::from_value(invalid)?;
            assert!(validate_command_consumer(&info, installation_id).is_err());
        }
        for (field, value) in [
            ("name", json!("aegis-other")),
            ("stream_name", json!("AEGIS_EVENTS")),
        ] {
            let mut invalid = valid.clone();
            invalid[field] = value;
            let info = serde_json::from_value(invalid)?;
            assert!(validate_command_consumer(&info, installation_id).is_err());
        }
        Ok(())
    }

    struct ScriptedSession {
        outcomes: VecDeque<bool>,
        attempts: usize,
    }

    impl ScriptedSession {
        fn new(outcomes: impl IntoIterator<Item = bool>) -> Self {
            Self {
                outcomes: outcomes.into_iter().collect(),
                attempts: 0,
            }
        }
    }

    impl SessionRunner for ScriptedSession {
        fn run_once(&mut self) -> SessionFuture<'_> {
            self.attempts += 1;
            let succeeds = self.outcomes.pop_front().unwrap_or(false);
            Box::pin(async move {
                if succeeds {
                    Ok(())
                } else {
                    anyhow::bail!("fixture failure token=must-not-be-logged");
                }
            })
        }
    }

    #[tokio::test]
    async fn reconnect_supervisor_recovers_after_a_failed_session() -> Result<()> {
        let mut session = ScriptedSession::new([false, true]);
        let result = supervise_sessions(
            &mut session,
            std::future::pending::<Result<()>>(),
            ReconnectBackoff::new(Duration::from_millis(1), Duration::from_millis(4)),
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(session.attempts, 2);
        Ok(())
    }

    #[tokio::test]
    async fn reconnect_backoff_is_capped_and_shutdown_interrupts_the_wait() -> Result<()> {
        let mut backoff = ReconnectBackoff::new(Duration::from_millis(1), Duration::from_millis(4));
        assert_eq!(backoff.next_delay(), Duration::from_millis(1));
        assert_eq!(backoff.next_delay(), Duration::from_millis(2));
        assert_eq!(backoff.next_delay(), Duration::from_millis(4));
        assert_eq!(backoff.next_delay(), Duration::from_millis(4));

        let mut session = ScriptedSession::new([false]);
        let shutdown = async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            Ok(())
        };
        supervise_sessions(
            &mut session,
            shutdown,
            ReconnectBackoff::new(Duration::from_secs(30), Duration::from_secs(30)),
        )
        .await?;
        assert_eq!(session.attempts, 1);
        Ok(())
    }

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
            "pairing_expires_at":Utc::now() + chrono::Duration::minutes(5),
        });
        let response: ProvisionResponse = serde_json::from_value(value)?;
        assert_eq!(response.installation_id, installation_id);
        assert_eq!(response.actor_id, actor_id);
        assert_eq!(response.pairing_code, "one-time-code");
        Ok(())
    }

    #[test]
    fn local_revoke_disables_command_and_notification_access() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        let root = workspace.path().join(".arun");
        let (config, mut authority, _) = fixture(workspace.path())?;
        let run = authority.store.create_run(
            "authorized remote task",
            workspace.path(),
            "codex",
            json!(["workspace.read"]),
            json!({}),
            "",
        )?;
        authority.grant_run(&config.actor_id, &run.id)?;
        config.save(&root)?;
        drop(authority);

        let (revoked_config, newly_revoked, revoked_actor_ids, authority) = revoke_locally(&root)?;
        assert!(newly_revoked);
        assert_eq!(revoked_config.actor_id, config.actor_id);
        assert_eq!(revoked_actor_ids, vec![config.actor_id.clone()]);
        drop(authority);

        let mut authority = Authority::open(&root)?;
        assert!(!actor_enabled(&authority, &config.actor_id)?);
        assert!(authority.actors_for_run(&run.id)?.is_empty());
        assert_eq!(
            authority.pending_relay_revocation_actor_ids()?,
            vec![config.actor_id.clone()]
        );
        assert!(!authority.revoke_actor(&config.actor_id)?);
        authority.mark_relay_actor_revoked(&config.actor_id)?;
        drop(authority);
        let authority = Authority::open(&root)?;
        assert!(authority.pending_relay_revocation_actor_ids()?.is_empty());
        assert!(!revoke_locally(&root)?.1);
        Ok(())
    }

    #[test]
    fn pairing_after_revoke_rotates_identity_without_restoring_old_task_access() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        let (old_config, mut authority, _) = fixture(workspace.path())?;
        let run = authority.store.create_run(
            "previously shared task",
            workspace.path(),
            "codex",
            json!(["workspace.read"]),
            json!({}),
            "",
        )?;
        authority.grant_run(&old_config.actor_id, &run.id)?;
        authority.bind_selected_task(&old_config.actor_id, &run.id)?;
        assert!(authority.revoke_actor(&old_config.actor_id)?);

        let new_actor_id = actor_for_pair(&authority, Some(&old_config))?;
        assert_ne!(new_actor_id, old_config.actor_id);
        authority.register_actor(&new_actor_id)?;
        assert!(!actor_enabled(&authority, &old_config.actor_id)?);
        assert!(authority.selected_task(&old_config.actor_id)?.is_none());
        assert!(authority.authorized_runs(&old_config.actor_id)?.is_empty());
        assert!(authority.authorized_runs(&new_actor_id)?.is_empty());
        assert_eq!(
            authority.pending_relay_revocation_actor_ids()?,
            vec![old_config.actor_id.clone()]
        );
        assert!(authority.register_actor(&old_config.actor_id).is_err());
        assert_eq!(
            authority.pending_relay_revocation_actor_ids()?,
            vec![old_config.actor_id.clone()]
        );

        let now = Utc::now().timestamp();
        let stale_phone_request = super::super::CommandEnvelope {
            version: 1,
            installation_id: authority.installation_id().to_owned(),
            actor_id: old_config.actor_id.clone(),
            request_id: Uuid::new_v4().to_string(),
            issued_at: now,
            expires_at: now + 60,
            command: Command::Status {
                task_id: Some(run.id),
            },
        };
        assert!(
            authority
                .apply_from_authenticated_relay(&old_config.actor_id, &stale_phone_request, now,)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn relay_revoke_endpoint_is_scoped_to_this_installation_and_actor() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        let (config, _, _) = fixture(workspace.path())?;
        assert_eq!(
            actor_revoke_endpoint(&config, &config.actor_id)?.as_str(),
            format!(
                "https://relay.example/admin/v1/installations/{}/actors/{}/bindings/revoke",
                config.installation_id, config.actor_id
            )
        );
        Ok(())
    }

    #[test]
    fn local_share_and_unshare_scope_the_configured_actor() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        let root = workspace.path().join(".arun");
        let (config, mut authority, _) = fixture(workspace.path())?;
        let run = authority.store.create_run(
            "task available only after explicit sharing",
            workspace.path(),
            "codex",
            json!(["workspace.read"]),
            json!({}),
            "",
        )?;
        authority.store.state(&run.id, "running", json!({}))?;
        let other_actor = Uuid::new_v4().to_string();
        authority.register_actor(&other_actor)?;
        authority.grant_run(&other_actor, &run.id)?;
        config.save(&root)?;
        drop(authority);

        let share_args = vec!["share".to_owned(), run.id.clone()];
        command(&root, &share_args)?;
        let mut authority = Authority::open(&root)?;
        assert_eq!(authority.authorized_runs(&config.actor_id)?.len(), 1);
        assert_eq!(authority.authorized_runs(&other_actor)?.len(), 1);

        let now = Utc::now().timestamp();
        let list_request = super::super::CommandEnvelope {
            version: 1,
            installation_id: config.installation_id.clone(),
            actor_id: config.actor_id.clone(),
            request_id: Uuid::new_v4().to_string(),
            issued_at: now,
            expires_at: now + 60,
            command: Command::ListTasks,
        };
        let listed =
            authority.apply_from_authenticated_relay(&config.actor_id, &list_request, now)?;
        let tasks = listed.result["tasks"]
            .as_array()
            .context("missing task list")?;
        assert_eq!(tasks.len(), 1);
        let alias = tasks[0]["alias"].as_str().context("missing task alias")?;
        assert!(format_receipt(&RelayCommand::ListTasks, &listed).contains(alias));
        authority.bind_selected_task(&config.actor_id, &run.id)?;
        assert_eq!(
            authority.selected_task(&config.actor_id)?.as_deref(),
            Some(run.id.as_str())
        );
        drop(authority);

        let unshare_args = vec!["unshare".to_owned(), run.id.clone()];
        command(&root, &unshare_args)?;
        let mut authority = Authority::open(&root)?;
        assert!(authority.authorized_runs(&config.actor_id)?.is_empty());
        assert_eq!(authority.selected_task(&config.actor_id)?, None);
        assert_eq!(authority.resolve_task(&config.actor_id, None)?, None);
        assert!(
            authority
                .resolve_task(&config.actor_id, Some(&run.id))
                .is_err()
        );
        assert_eq!(authority.authorized_runs(&other_actor)?.len(), 1);

        // An idempotent retry of an earlier /tasks request must not replay its
        // now-stale task snapshot after unsharing.
        let refreshed =
            authority.apply_from_authenticated_relay(&config.actor_id, &list_request, now + 1)?;
        assert!(!refreshed.duplicate);
        assert!(refreshed.result["tasks"].as_array().unwrap().is_empty());
        assert!(
            format_receipt(&RelayCommand::ListTasks, &refreshed)
                .contains("No tasks are currently shared")
        );

        let status_request = super::super::CommandEnvelope {
            version: 1,
            installation_id: config.installation_id,
            actor_id: config.actor_id.clone(),
            request_id: Uuid::new_v4().to_string(),
            issued_at: now + 1,
            expires_at: now + 61,
            command: Command::Status {
                task_id: Some(run.id),
            },
        };
        assert!(
            authority
                .apply_from_authenticated_relay(&config.actor_id, &status_request, now + 1)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn only_expected_rejections_become_replies_while_internal_errors_retry() {
        let rejection = super::super::reject_command("task is no longer active");
        let reply =
            command_failure_reply(&RelayCommand::Status { task_id: None }, rejection).unwrap();
        assert!(reply.contains("could not apply"));

        let transient = anyhow::anyhow!("temporary SQLite failure");
        let error =
            command_failure_reply(&RelayCommand::Status { task_id: None }, transient).unwrap_err();
        assert!(error.to_string().contains("temporary SQLite failure"));
    }

    #[test]
    fn phone_replies_show_aliases_and_keep_task_details_bounded() -> Result<()> {
        let list = format_receipt(
            &RelayCommand::ListTasks,
            &super::super::Receipt {
                request_id: Uuid::new_v4().to_string(),
                duplicate: false,
                result: json!({"tasks":[
                    {"alias":"t-abcdef","task":"Inspect parser","state":"running"},
                    {"alias":"t-123456","task":"Review tests","state":"completed"}
                ]}),
            },
        );
        assert!(list.contains("t-abcdef [running] Inspect parser"));
        assert!(list.contains("/use <alias>"));
        assert!(list.contains("/details [alias]"));
        let many_tasks = (0..30)
            .map(|index| {
                json!({
                    "alias":format!("t-{index:06}"),
                    "task":format!("{} more task name", "\u{1f6e1}".repeat(48)),
                    "state":"running"
                })
            })
            .collect::<Vec<_>>();
        let bounded_list = format_receipt(
            &RelayCommand::ListTasks,
            &super::super::Receipt {
                request_id: Uuid::new_v4().to_string(),
                duplicate: false,
                result: json!({"tasks":many_tasks}),
            },
        );
        assert!(bounded_list.contains("more task(s) omitted"));
        assert!(bounded_list.len() < 3_700);
        assert!(
            AegisEvent::reply(
                &Uuid::new_v4().to_string(),
                "actor-1",
                &Uuid::new_v4().to_string(),
                &bounded_list,
            )?
            .reply_text
            .unwrap()
            .len()
                <= 4 * 1024
        );

        let details = format_task_details(&json!({
            "alias":"t-abcdef",
            "task":"x".repeat(240),
            "state":"completed",
            "provider":"codex",
            "model":"gpt-6.1-sol",
            "created_at":1_800_000_000_i64,
            "started_at":1_800_000_060_i64,
            "elapsed_seconds":121_i64,
            "model_turns":4,
            "successful_actions":3,
            "pending_actions":0,
            "uncertain_actions":0,
            "model_tokens":512,
            "obligations":{"open":1,"verified":2,"stale":3,"superseded":4},
            "selected":true,
            "tool_output":"must never appear"
        }));
        assert!(details.contains("gpt-6.1-sol"));
        assert!(details.contains("4 model turns"));
        assert!(details.contains("2m 1s"));
        assert!(details.contains("1 open, 2 verified, 3 stale, 4 superseded"));
        assert!(!details.contains("must never appear"));
        assert!(details.len() < 4 * 1024);
        assert!(
            AegisEvent::reply(
                &Uuid::new_v4().to_string(),
                "actor-1",
                &Uuid::new_v4().to_string(),
                &details,
            )?
            .reply_text
            .unwrap()
            .len()
                <= 4 * 1024
        );
        Ok(())
    }

    #[test]
    fn phone_result_and_evidence_replies_are_bounded_and_allowlisted() -> Result<()> {
        let result = format_task_result(&json!({
            "alias":"t-abcdef",
            "state":"answered",
            "verification":"unverified",
            "summary":"🛡".repeat(900),
            "stdout":"TOP_SECRET_TOOL_OUTPUT"
        }));
        assert!(result.contains("answered; unverified"));
        assert!(result.len() <= super::super::MAX_REMOTE_RESULT_BYTES + 128);
        let reply = AegisEvent::reply(
            &Uuid::new_v4().to_string(),
            "actor-1",
            &Uuid::new_v4().to_string(),
            &result,
        )?;
        assert!(reply.reply_text.unwrap().len() <= 4 * 1024);

        let evidence = format_task_evidence(&json!({
            "alias":"t-abcdef",
            "state":"completed",
            "receipts":[{
                "capability":"workspace.read",
                "hash_prefix":"0123456789ab",
                "bytes":123,
                "path":"TOP_SECRET_PATH",
                "stdout":"TOP_SECRET_TOOL_OUTPUT",
                "arguments":"TOP_SECRET_ARGUMENTS"
            }],
            "omitted":3
        }));
        assert!(evidence.contains("workspace.read · 0123456789ab · 123 bytes"));
        assert!(evidence.contains("3 more receipt(s) omitted"));
        assert!(!evidence.contains("TOP_SECRET"));

        let answered = format_task_evidence(&json!({
            "alias":"t-abcdef",
            "state":"answered",
            "receipts":[{"capability":"workspace.read","hash_prefix":"0123456789ab","bytes":1}],
            "omitted":1
        }));
        assert!(answered.contains("unverified"));
        assert!(answered.contains("no tool receipts"));
        assert!(!answered.contains("workspace.read"));
        Ok(())
    }

    #[test]
    fn first_message_creates_one_stable_local_task_and_receipt() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        let root = workspace.path().join(".arun");
        let (config, mut authority, relay) = fixture(workspace.path())?;
        let now = Utc::now().timestamp();

        let first = apply_relay_command(&root, &config, &mut authority, &relay, now)?;
        let retry_now = Utc::now();
        let retried_envelope = RelayCommandEnvelope {
            issued_at: retry_now,
            expires_at: retry_now + ChronoDuration::minutes(5),
            ..relay.clone()
        };
        let second = apply_relay_command(
            &root,
            &config,
            &mut authority,
            &retried_envelope,
            retry_now.timestamp(),
        )?;
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

        authority
            .store
            .connection
            .execute("UPDATE runs SET state='completed' WHERE id=?1", [task_id])?;
        let next_now = Utc::now();
        let next_task_envelope = RelayCommandEnvelope {
            envelope_id: Uuid::new_v4().to_string(),
            request_id: Uuid::new_v4().to_string(),
            external_message_id: "provider-message-id-2".into(),
            issued_at: next_now,
            expires_at: next_now + ChronoDuration::minutes(5),
            command: RelayCommand::Message {
                text: "Start another task".into(),
            },
            ..relay.clone()
        };
        let next_task = apply_relay_command(
            &root,
            &config,
            &mut authority,
            &next_task_envelope,
            next_now.timestamp(),
        )?;
        assert!(!next_task.duplicate);
        let next_task_id = next_task.result["task_id"].as_str().unwrap();
        assert_ne!(next_task_id, task_id);
        assert_eq!(authority.store.runs()?.len(), 2);
        assert_eq!(
            authority.selected_task(&relay.actor_id)?.as_deref(),
            Some(next_task_id)
        );
        assert_eq!(
            authority.store.event_count(next_task_id, "user.steering")?,
            1
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
            payload: json!({
                "summary":"fixture-secret-token api_key=private-value",
                "raw_output":"not sent"
            }),
            created_at: 2,
        };
        let first = outbound_events(&run, &event, &installation_id, "actor-1");
        let second = outbound_events(&run, &event, &installation_id, "actor-1");
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].event_id, second[0].event_id);
        assert!(first[0].reply_text.is_none());
        assert_eq!(first[0].actor_id, "actor-1");
        assert!(
            !first[0]
                .to_json()
                .unwrap()
                .windows(b"fixture-secret-token".len())
                .any(|window| window == b"fixture-secret-token")
        );
        let answered = outbound_events(
            &run,
            &Event {
                kind: "run.answered".into(),
                ..event.clone()
            },
            &installation_id,
            "actor-1",
        );
        assert_eq!(answered.len(), 1);
        assert!(answered[0].reply_text.is_none());
        let mut approval = Event {
            seq: 10,
            kind: "approval.required".into(),
            payload: json!({"challenge_id":Uuid::new_v4().to_string(),"descriptor":"workspace.write · file.txt"}),
            created_at: 3,
        };
        let challenge_expires_at = crate::storage::unix_time() + 300;
        approval.payload["expires_at"] = json!(challenge_expires_at);
        let approval_source = approval.clone();
        let approval = outbound_events(&run, &approval, &installation_id, "actor-1");
        assert_eq!(approval.len(), 1);
        assert!(approval[0].challenge_id.is_some());
        assert_eq!(approval[0].expires_at, Some(challenge_expires_at));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&approval[0].to_json().unwrap()).unwrap()["expires_at"],
            challenge_expires_at
        );
        assert!(
            approval[0]
                .display_detail
                .as_deref()
                .is_some_and(|detail| !detail.contains('\n'))
        );
        let expired_approval = Event {
            payload: json!({
                "challenge_id":approval_source.payload["challenge_id"],
                "descriptor":"workspace.write · file.txt",
                "expires_at":crate::storage::unix_time() - 1,
            }),
            ..approval_source
        };
        assert!(outbound_events(&run, &expired_approval, &installation_id, "actor-1").is_empty());
        assert!(
            outbound_events(
                &run,
                &Event {
                    kind: "model.response".into(),
                    ..event.clone()
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
