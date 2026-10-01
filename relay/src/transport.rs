use std::{
    future::Future,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

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
const COMMAND_MAX_STREAM_AGE: Duration = Duration::from_secs(15 * 60);
const EVENT_MAX_STREAM_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_EVENT_ENVELOPE_BYTES: usize = 16 * 1024;
const EVENT_RETRY_INITIAL: Duration = Duration::from_secs(1);
const EVENT_RETRY_MAX: Duration = Duration::from_secs(30);
const EVENT_CONSUMER_STABLE: Duration = Duration::from_secs(60);

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
            .create_or_update_stream(stream_config(
                COMMAND_STREAM,
                "aegis.commands.*",
                COMMAND_MAX_STREAM_AGE,
            ))
            .await
            .map_err(|_| TransportError)?;
        context
            .create_or_update_stream(stream_config(
                EVENT_STREAM,
                "aegis.events.*",
                EVENT_MAX_STREAM_AGE,
            ))
            .await
            .map_err(|_| TransportError)?;
        Ok(Self { context })
    }

    pub async fn supervise_events<S>(self: Arc<Self>, service: Arc<RelayService>, shutdown: S)
    where
        S: Future<Output = ()>,
    {
        let transport = self.clone();
        let mut consume = move || {
            let transport = transport.clone();
            let service = service.clone();
            async move { transport.consume_events(service).await }
        };
        supervise_consumer(
            &mut consume,
            shutdown,
            EVENT_RETRY_INITIAL,
            EVENT_RETRY_MAX,
            EVENT_CONSUMER_STABLE,
        )
        .await;
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
            let message = message.map_err(|_| TransportError)?;
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

async fn supervise_consumer<F, Fut, S>(
    consume: &mut F,
    shutdown: S,
    initial_backoff: Duration,
    max_backoff: Duration,
    stable_after: Duration,
) where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<(), TransportError>>,
    S: Future<Output = ()>,
{
    tokio::pin!(shutdown);
    let mut backoff = initial_backoff;
    loop {
        let started = Instant::now();
        let result = tokio::select! {
            _ = &mut shutdown => return,
            result = consume() => result,
        };

        match result {
            Ok(()) => {
                tracing::warn!("JetStream event consumer reached EOF; reattaching durable consumer")
            }
            Err(_) => {
                tracing::warn!("JetStream event consumer failed; reattaching durable consumer")
            }
        }

        let stable = started.elapsed() >= stable_after;
        let wait = if stable { initial_backoff } else { backoff };
        backoff = next_backoff(backoff, stable, initial_backoff, max_backoff);
        tokio::select! {
            _ = &mut shutdown => return,
            _ = tokio::time::sleep(wait) => {}
        }
    }
}

fn next_backoff(current: Duration, stable: bool, initial: Duration, maximum: Duration) -> Duration {
    if stable {
        initial
    } else {
        current.saturating_mul(2).min(maximum)
    }
}

fn stream_config(name: &str, subject: &str, max_age: Duration) -> jetstream::stream::Config {
    jetstream::stream::Config {
        name: name.to_owned(),
        subjects: vec![subject.to_owned()],
        retention: jetstream::stream::RetentionPolicy::WorkQueue,
        storage: jetstream::stream::StorageType::File,
        discard: jetstream::stream::DiscardPolicy::Old,
        max_age,
        max_bytes: MAX_STREAM_BYTES,
        duplicate_window: Duration::from_secs(15 * 60),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use super::*;

    #[test]
    fn event_stream_retains_for_a_day_with_a_hard_byte_cap() {
        let config = stream_config("AEGIS_EVENTS", "aegis.events.*", EVENT_MAX_STREAM_AGE);
        assert_eq!(config.max_age, Duration::from_secs(24 * 60 * 60));
        assert_eq!(config.max_bytes, MAX_STREAM_BYTES);
        assert_eq!(MAX_STREAM_BYTES, 64 * 1024 * 1024);
        assert_eq!(config.discard, jetstream::stream::DiscardPolicy::Old);
    }

    #[test]
    fn command_stream_keeps_its_short_expiry_window() {
        let config = stream_config("AEGIS_COMMANDS", "aegis.commands.*", COMMAND_MAX_STREAM_AGE);
        assert_eq!(config.max_age, Duration::from_secs(15 * 60));
    }

    #[test]
    fn retry_backoff_doubles_to_its_cap_and_resets_after_a_stable_consumer() {
        let initial = Duration::from_millis(1);
        let maximum = Duration::from_millis(4);
        let second = next_backoff(initial, false, initial, maximum);
        let third = next_backoff(second, false, initial, maximum);
        let capped = next_backoff(third, false, initial, maximum);
        assert_eq!(
            [initial, second, third, capped],
            [
                Duration::from_millis(1),
                Duration::from_millis(2),
                Duration::from_millis(4),
                Duration::from_millis(4),
            ]
        );
        assert_eq!(next_backoff(capped, true, initial, maximum), initial);
    }

    #[tokio::test]
    async fn consumer_errors_and_eof_are_retried_until_shutdown() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let mut stop_tx = Some(stop_tx);
        let attempts_for_consumer = attempts.clone();
        let mut consume = move || {
            let attempt = attempts_for_consumer.fetch_add(1, Ordering::SeqCst);
            let should_stop = if attempt == 2 { stop_tx.take() } else { None };
            async move {
                if let Some(sender) = should_stop {
                    let _ = sender.send(());
                }
                if attempt == 1 {
                    Ok(()) // A clean EOF also requires reattachment.
                } else {
                    Err(TransportError)
                }
            }
        };

        supervise_consumer(
            &mut consume,
            async move {
                let _ = stop_rx.await;
            },
            Duration::from_millis(1),
            Duration::from_millis(4),
            Duration::from_secs(60),
        )
        .await;

        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }
}
