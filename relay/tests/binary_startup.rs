//! Actual relay process + real local PostgreSQL/TLS NATS + Evolution HTTP fixture.
//! This verifies the gateway contract, not delivery through a live carrier.
use std::{
    collections::BTreeMap,
    process::{Child, Command, Stdio},
    time::Duration,
};

use aegis_relay::{
    AegisEvent, AgentCommand, CommandEnvelope, RemoteEventKind, provider::sign_webhook,
};
use async_nats::jetstream::{
    self,
    consumer::{AckPolicy, pull},
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::HeaderMap,
    routing::post,
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::{
    sync::mpsc,
    time::{Instant, timeout},
};
use uuid::Uuid;

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

struct RelayProcess(Child);
impl Drop for RelayProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct GatewayCall {
    instance: String,
    headers: HeaderMap,
    body: Value,
}
async fn send_text(
    State(sender): State<mpsc::UnboundedSender<GatewayCall>>,
    Path(instance): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    sender
        .send(GatewayCall {
            instance,
            headers,
            body,
        })
        .unwrap();
    Json(json!({"key":{"id":"local-evolution-message"}}))
}

fn required(name: &str) -> String {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("{name} is required; run relay/scripts/e2e-smoke.ps1"))
}

fn binary_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_aegis-relay"));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command
}

#[test]
fn actual_relay_binary_rejects_missing_configuration() {
    let output = binary_command().env_clear().output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("DATABASE_URL"),
        "unexpected startup error: {stderr}"
    );
}

fn evolution_message(id: &str, phone: &str, text: &str) -> Value {
    json!({"event":"messages.upsert", "data": {
        "key":{"id":id,"fromMe":false,"remoteJid":"123456789@s.whatsapp.net",
            "remoteJidAlt":format!("{phone}@s.whatsapp.net")},
        "message":{"conversation":text}
    }})
}

fn assert_send(call: GatewayCall, phone: &str, text: &str, api_key: &str) {
    assert_eq!(call.instance, "binary-e2e");
    assert_eq!(
        call.headers.get("apikey").unwrap().to_str().unwrap(),
        api_key
    );
    // Exact object comparison rejects unsupported idempotency fields and leaked context.
    assert_eq!(call.body, json!({"number":phone,"text":text}));
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and TLS NATS; run relay/scripts/e2e-smoke.ps1"]
async fn actual_relay_binary_uses_evolution_http_and_real_postgres_tls_nats() -> TestResult {
    let database_url = required("RELAY_E2E_DATABASE_URL");
    let nats_url = required("RELAY_E2E_NATS_URL");
    let nats_token = required("RELAY_E2E_NATS_TOKEN");
    let root_cert = required("RELAY_E2E_NATS_ROOT_CERT");
    let installation_id: Uuid = required("RELAY_E2E_INSTALLATION_ID").parse()?;
    let device_token = required("AEGIS_E2E_NATS_TOKEN");
    let unique = Uuid::new_v4();
    let actor = format!("binary-{unique}");
    let phone = format!("49151{:010}", unique.as_u128() % 10_000_000_000);
    let admin_token = format!("local-admin-{unique}");
    let webhook_secret = format!("local-webhook-{unique}");
    let api_key = format!("local-api-{unique}");
    let inbound_text = format!("command-sentinel-{unique}\r\nprivate local context");
    let reply_text = format!("output-sentinel-{unique}: local result only");

    let (sender, mut calls) = mpsc::unbounded_channel();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let gateway_url = format!("http://{}/", listener.local_addr()?);
    let gateway = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/message/sendText/{instance}", post(send_text))
                .with_state(sender),
        )
        .await
        .unwrap();
    });
    // Abort even if a failed assertion unwinds this test.
    struct GatewayGuard(tokio::task::JoinHandle<()>);
    impl Drop for GatewayGuard {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _gateway = GatewayGuard(gateway);

    let port_reservation = std::net::TcpListener::bind("127.0.0.1:0")?;
    let relay_address = port_reservation.local_addr()?;
    drop(port_reservation);
    let log = tempfile::NamedTempFile::new()?;
    let mut relay = RelayProcess(
        binary_command()
            .env("RELAY_BIND_ADDR", relay_address.to_string())
            .env("DATABASE_URL", &database_url)
            .env("RELAY_ALLOW_INSECURE_LOCAL_DATABASE", "true")
            .env("NATS_URL", &nats_url)
            .env("NATS_AUTH_TOKEN", &nats_token)
            .env("NATS_TLS_ROOT_CERT", &root_cert)
            .env("RELAY_ADMIN_TOKEN", &admin_token)
            .env("EVOLUTION_BASE_URL", &gateway_url)
            .env("EVOLUTION_INSTANCE", "binary-e2e")
            .env("EVOLUTION_API_KEY", &api_key)
            .env("EVOLUTION_WEBHOOK_SECRET", &webhook_secret)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.reopen()?))
            .stderr(Stdio::from(log.reopen()?))
            .spawn()?,
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let base = format!("http://{relay_address}");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = relay.0.try_wait()? {
            panic!(
                "relay exited at startup ({status}): {}",
                std::fs::read_to_string(log.path())?
            );
        }
        if let Ok(response) = client.get(format!("{base}/healthz")).send().await {
            assert_eq!(response.status(), 200);
            assert_eq!(response.text().await?, "ok");
            break;
        }
        assert!(Instant::now() < deadline, "relay health startup timed out");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let provision_url = format!("{base}/admin/v1/installations");
    let provision_body = json!({"actor_id":actor,"installation_id":installation_id});
    assert_eq!(
        client
            .post(&provision_url)
            .json(&provision_body)
            .send()
            .await?
            .status(),
        401
    );
    let response = client
        .post(&provision_url)
        .bearer_auth(&admin_token)
        .json(&provision_body)
        .send()
        .await?;
    assert_eq!(response.status(), 201);
    let provisioned: Value = response.json().await?;
    assert_eq!(provisioned["installation_id"], installation_id.to_string());
    assert_eq!(provisioned["actor_id"], actor);
    let pairing_code = provisioned["pairing_code"].as_str().unwrap();
    assert_eq!(pairing_code.len(), 43);
    let webhook_url = format!("{base}/v1/webhooks/whatsapp/evolution");
    let pair_id = format!("pair-{unique}");
    let pairing = evolution_message(&pair_id, &phone, &format!("AEGIS {pairing_code}"));
    assert_eq!(
        client
            .post(&webhook_url)
            .json(&pairing)
            .send()
            .await?
            .status(),
        401
    );
    assert_eq!(
        client
            .post(&webhook_url)
            .header("x-aegis-webhook-token", "invalid")
            .json(&pairing)
            .send()
            .await?
            .status(),
        401
    );
    assert!(
        calls.try_recv().is_err(),
        "unauthenticated webhook sent a provider request"
    );
    assert_eq!(
        client
            .post(&webhook_url)
            .header("x-aegis-webhook-token", &webhook_secret)
            .json(&pairing)
            .send()
            .await?
            .status(),
        202
    );
    assert_send(
        timeout(Duration::from_secs(5), calls.recv())
            .await?
            .unwrap(),
        &phone,
        "This account is paired with Aegis.",
        &api_key,
    );

    let device = async_nats::ConnectOptions::new()
        .user_and_password(format!("aegis-{installation_id}"), device_token)
        .custom_inbox_prefix(format!("_INBOX.aegis.device.{installation_id}"))
        .require_tls(true)
        .add_root_certificates(root_cert.into())
        .connect(nats_url)
        .await?;
    let context = jetstream::new(device);
    let stream = context.get_stream_no_info("AEGIS_COMMANDS").await?;
    let durable = format!("aegis-{installation_id}");
    let consumer = stream
        .get_or_create_consumer(
            &durable,
            pull::Config {
                durable_name: Some(durable.clone()),
                name: Some(durable.clone()),
                filter_subject: format!("aegis.commands.{installation_id}"),
                ack_policy: AckPolicy::Explicit,
                ack_wait: Duration::from_secs(45),
                ..Default::default()
            },
        )
        .await?;
    let command_id = format!("command-{unique}");
    let body = serde_json::to_vec(&evolution_message(&command_id, &phone, &inbound_text))?;
    let timestamp = chrono::Utc::now().timestamp();
    let signature = sign_webhook(webhook_secret.as_bytes(), timestamp, &body);
    assert_eq!(
        client
            .post(&webhook_url)
            .header("content-type", "application/json")
            .header("x-aegis-timestamp", timestamp.to_string())
            .header("x-aegis-signature", "v1=invalid")
            .body(body.clone())
            .send()
            .await?
            .status(),
        401
    );
    assert_eq!(
        client
            .post(&webhook_url)
            .header("content-type", "application/json")
            .header("x-aegis-timestamp", timestamp.to_string())
            .header("x-aegis-signature", &signature)
            .body(body.clone())
            .send()
            .await?
            .status(),
        202
    );
    let mut messages = consumer
        .fetch()
        .max_messages(1)
        .expires(Duration::from_secs(5))
        .messages()
        .await?;
    let message = timeout(Duration::from_secs(6), messages.next())
        .await?
        .expect("missing command")?;
    let envelope: CommandEnvelope = serde_json::from_slice(&message.payload)?;
    assert_eq!(envelope.installation_id, installation_id);
    assert_eq!(envelope.actor_id, actor);
    assert_eq!(envelope.channel, "whatsapp");
    assert_eq!(envelope.sender_id, format!("+{phone}"));
    assert_eq!(envelope.external_message_id, command_id);
    assert_eq!(
        envelope.command,
        AgentCommand::Message {
            text: inbound_text.replace("\r\n", "\n")
        }
    );
    message.ack().await?;
    // Identical authenticated webhook is acknowledged without a second command.
    assert_eq!(
        client
            .post(&webhook_url)
            .header("content-type", "application/json")
            .header("x-aegis-timestamp", timestamp.to_string())
            .header("x-aegis-signature", &signature)
            .body(body)
            .send()
            .await?
            .status(),
        202
    );
    let mut duplicate = consumer
        .fetch()
        .max_messages(1)
        .expires(Duration::from_secs(1))
        .messages()
        .await?;
    assert!(
        timeout(Duration::from_secs(2), duplicate.next())
            .await?
            .is_none(),
        "duplicate webhook published another command"
    );

    assert_eq!(
        client
            .post(format!(
                "{base}/admin/v1/installations/{installation_id}/notifications/reply"
            ))
            .bearer_auth(&admin_token)
            .json(&json!({"enabled":true}))
            .send()
            .await?
            .status(),
        204
    );
    let event_id = Uuid::new_v4();
    let event = AegisEvent {
        event_id,
        installation_id,
        actor_id: actor.clone(),
        kind: RemoteEventKind::Reply,
        task_id: None,
        challenge_id: None,
        expires_at: None,
        display_detail: None,
        reply_text: Some(reply_text.clone()),
    };
    context
        .publish(
            format!("aegis.events.{installation_id}"),
            serde_json::to_vec(&event)?.into(),
        )
        .await?
        .await?;
    assert_send(
        timeout(Duration::from_secs(10), calls.recv())
            .await?
            .unwrap(),
        &phone,
        &reply_text,
        &api_key,
    );
    let pool = sqlx::PgPool::connect(&database_url).await?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let receipt: Option<(String, Option<String>)> = sqlx::query_as(
            "SELECT state, provider_message_id FROM delivery_receipts WHERE event_id=$1 AND direction='outbound'")
            .bind(event_id).fetch_optional(&pool).await?;
        if receipt == Some(("delivered".into(), Some("local-evolution-message".into()))) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "provider response was not saved in delivered receipt"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_metadata_only(
        &pool,
        &[
            &format!("command-sentinel-{unique}"),
            &reply_text,
            pairing_code,
        ],
    )
    .await?;
    pool.close().await;
    println!(
        "Actual relay process, local Evolution contract, metadata-only PostgreSQL and TLS NATS verified; no live carrier delivery tested."
    );
    Ok(())
}

async fn assert_metadata_only(pool: &sqlx::PgPool, secrets: &[&str]) -> TestResult {
    let expected: BTreeMap<&str, Vec<&str>> = [
        ("users", vec!["created_at", "id"]),
        (
            "installations",
            vec!["created_at", "id", "last_seen_at", "user_id"],
        ),
        (
            "channel_bindings",
            vec![
                "active",
                "actor_id",
                "channel",
                "created_at",
                "destination_id",
                "id",
                "installation_id",
                "sender_id",
                "user_id",
            ],
        ),
        (
            "pairing_tokens",
            vec![
                "actor_id",
                "created_at",
                "expires_at",
                "id",
                "installation_id",
                "token_hash",
                "used_at",
                "user_id",
            ],
        ),
        (
            "delivery_receipts",
            vec![
                "attempts",
                "binding_id",
                "channel",
                "direction",
                "disposition",
                "event_id",
                "expires_at",
                "external_message_id",
                "installation_id",
                "provider_message_id",
                "receipt_id",
                "sender_id",
                "state",
                "updated_at",
            ],
        ),
        (
            "notification_preferences",
            vec![
                "channel",
                "enabled",
                "event_kind",
                "installation_id",
                "updated_at",
            ],
        ),
    ]
    .into_iter()
    .collect();
    let columns: Vec<(String, String)> = sqlx::query_as(
        "SELECT table_name,column_name FROM information_schema.columns WHERE table_schema='public' ORDER BY table_name,column_name")
        .fetch_all(pool).await?;
    let mut actual = BTreeMap::<String, Vec<String>>::new();
    for (table, column) in columns {
        actual.entry(table).or_default().push(column);
    }
    let expected_owned: BTreeMap<String, Vec<String>> = expected
        .iter()
        .map(|(table, columns)| {
            (
                table.to_string(),
                columns.iter().map(|column| column.to_string()).collect(),
            )
        })
        .collect();
    assert_eq!(
        actual, expected_owned,
        "relay schema must contain metadata tables and columns only"
    );
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT 'users', row_to_json(m)::text FROM users m
         UNION ALL SELECT 'installations', row_to_json(m)::text FROM installations m
         UNION ALL SELECT 'channel_bindings', row_to_json(m)::text FROM channel_bindings m
         UNION ALL SELECT 'pairing_tokens', row_to_json(m)::text FROM pairing_tokens m
         UNION ALL SELECT 'delivery_receipts', row_to_json(m)::text FROM delivery_receipts m
         UNION ALL SELECT 'notification_preferences', row_to_json(m)::text FROM notification_preferences m",
    ).fetch_all(pool).await?;
    for (table, row) in rows {
        for secret in secrets {
            assert!(
                !row.contains(secret),
                "transient content persisted in {table}"
            );
        }
    }
    Ok(())
}
