use std::{path::Path, sync::Arc, time::Duration};

use async_nats::{HeaderMap, jetstream};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::from_slice;
use uuid::Uuid;

use crate::{
    model::{AegisEvent, CommandEnvelope},
    service::{CommandTransport, RelayService, TransportError},
};

const COMMAND_STREAM: &str = "AEGIS_COMMANDS";
const EVENT_STREAM: &str = "AEGIS_EVENTS";
const MAX_STREAM_BYTES: i64 = 64 * 1024 * 1024;
const MAX_STREAM_AGE: Duration = Duration::from_secs(15 * 60);
const MAX_EVENT_ENVELOPE_BYTES: usize = 16 * 1024;

#[derive(Clone)]
pub struct JetStreamTransport {
    context: jetstream::Context,
}

impl JetStreamTransport {
    pub async fn connect(
        url: &str,
        auth_token: &str,
        root_certificate: Option<&Path>,
    ) -> Result<Self, TransportError> {
        let mut options = async_nats::ConnectOptions::new()
            .token(auth_token.to_owned())
            .require_tls(true);
        if let Some(path) = root_certificate {
            options = options.add_root_certificates(path.to_path_buf());
        }
        let client = options.connect(url).await.map_err(|_| TransportError)?;
        let context = jetstream::new(client);
        context
            .get_or_create_stream(stream_config(COMMAND_STREAM, "aegis.commands.*"))
            .await
            .map_err(|_| TransportError)?;
        context
            .get_or_create_stream(stream_config(EVENT_STREAM, "aegis.events.*"))
            .await
            .map_err(|_| TransportError)?;
        Ok(Self { context })
    }

    pub async fn consume_events(
        self: Arc<Self>,
        service: Arc<RelayService>,
    ) -> Result<(), TransportError> {
        let stream = self
            .context
            .get_stream(EVENT_STREAM)
            .await
            .map_err(|_| TransportError)?;
        let consumer = stream
            .get_or_create_consumer(
                "relay-whatsapp-delivery",
                jetstream::consumer::pull::Config {
                    durable_name: Some("relay-whatsapp-delivery".to_owned()),
                    filter_subject: "aegis.events.*".to_owned(),
                    ack_wait: Duration::from_secs(45),
                    ..Default::default()
                },
            )
            .await
            .map_err(|_| TransportError)?;
        let mut messages = consumer.messages().await.map_err(|_| TransportError)?;
        while let Some(message) = messages.next().await {
            let Ok(message) = message else {
                continue;
            };
            if message.payload.len() > MAX_EVENT_ENVELOPE_BYTES {
                tracing::warn!("discarding oversized Aegis event envelope");
                let _ = message.ack().await;
                continue;
            }
            let subject_installation = message
                .subject
                .as_str()
                .strip_prefix("aegis.events.")
                .and_then(|value| Uuid::parse_str(value).ok());
            let event = from_slice::<AegisEvent>(&message.payload).ok();
            let matching_installation = event
                .as_ref()
                .zip(subject_installation)
                .is_some_and(|(event, subject_id)| event.installation_id == subject_id);
            if !matching_installation {
                tracing::warn!("discarding invalid Aegis event envelope");
                let _ = message.ack().await;
                continue;
            }
            let event = event.expect("validated above");
            if let Err(_error) = service.deliver_event(&event).await {
                // Dropping without ack lets JetStream redeliver; successful per-destination
                // receipts prevent repeats after a partial fan-out.
                tracing::warn!(event_id = %event.event_id, "WhatsApp event delivery will retry");
                continue;
            }
            if message.ack().await.is_err() {
                tracing::warn!(event_id = %event.event_id, "could not acknowledge delivered event");
            }
        }
        Ok(())
    }
}

#[async_trait]
impl CommandTransport for JetStreamTransport {
    async fn publish(&self, envelope: &CommandEnvelope) -> Result<(), TransportError> {
        let subject = format!("aegis.commands.{}", envelope.installation_id);
        let mut headers = HeaderMap::new();
        headers.insert("Nats-Msg-Id", envelope.envelope_id.to_string());
        let payload = serde_json::to_vec(envelope).map_err(|_| TransportError)?;
        self.context
            .publish_with_headers(subject, headers, payload.into())
            .await
            .map_err(|_| TransportError)?
            .await
            .map_err(|_| TransportError)?;
        Ok(())
    }
}

fn stream_config(name: &str, subject: &str) -> jetstream::stream::Config {
    jetstream::stream::Config {
        name: name.to_owned(),
        subjects: vec![subject.to_owned()],
        retention: jetstream::stream::RetentionPolicy::WorkQueue,
        storage: jetstream::stream::StorageType::File,
        max_age: MAX_STREAM_AGE,
        max_bytes: MAX_STREAM_BYTES,
        duplicate_window: Duration::from_secs(15 * 60),
        ..Default::default()
    }
}
