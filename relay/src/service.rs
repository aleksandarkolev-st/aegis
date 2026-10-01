use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    channel::{ChannelError, MessagingChannel},
    model::{AegisEvent, COMMAND_TTL_SECONDS, CommandEnvelope, normalize_text, parse_command},
    repository::{DeliveryTarget, RelayRepository, RepositoryError},
};

#[async_trait]
pub trait CommandTransport: Send + Sync {
    async fn publish(&self, envelope: &CommandEnvelope) -> Result<(), TransportError>;
}

#[derive(Debug, Error)]
#[error("message transport failed")]
pub struct TransportError;

#[derive(Debug, Error)]
pub enum RelayError {
    #[error("channel authentication failed")]
    Authentication,
    #[error("channel message is invalid")]
    InvalidMessage,
    #[error("webhook message is already being processed")]
    InProgress,
    #[error("channel operation failed")]
    Channel(#[from] ChannelError),
    #[error("metadata operation failed")]
    Repository(#[from] RepositoryError),
    #[error("message transport failed")]
    Transport(#[from] TransportError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboundDisposition {
    Ignored,
    PairingPromptSent,
    Paired,
    Published,
}

impl InboundDisposition {
    fn receipt_value(self) -> &'static str {
        match self {
            Self::Ignored => "ignored",
            Self::PairingPromptSent => "pairing_prompt_sent",
            Self::Paired => "paired",
            Self::Published => "published",
        }
    }
}

pub struct RelayService {
    repository: Arc<dyn RelayRepository>,
    transport: Arc<dyn CommandTransport>,
    channel: Arc<dyn MessagingChannel>,
}

impl RelayService {
    pub fn new(
        repository: Arc<dyn RelayRepository>,
        transport: Arc<dyn CommandTransport>,
        channel: Arc<dyn MessagingChannel>,
    ) -> Self {
        Self {
            repository,
            transport,
            channel,
        }
    }

    pub async fn handle_webhook(
        &self,
        headers: &axum::http::HeaderMap,
        raw_body: &[u8],
    ) -> Result<InboundDisposition, RelayError> {
        let Some(message) = self.channel.parse_inbound(headers, raw_body)? else {
            return Ok(InboundDisposition::Ignored);
        };
        let channel_id = self.channel.id().as_str();
        match self
            .repository
            .claim_inbound_receipt(channel_id, &message.sender_id, &message.external_message_id)
            .await?
        {
            crate::repository::InboundReceiptClaim::Duplicate => {
                return Ok(InboundDisposition::Ignored);
            }
            crate::repository::InboundReceiptClaim::InProgress => {
                return Err(RelayError::InProgress);
            }
            crate::repository::InboundReceiptClaim::Claimed => {}
        }
        let sender_id = message.sender_id.clone();
        let external_message_id = message.external_message_id.clone();
        match self.handle_message(message).await {
            Ok(disposition) => {
                let binding = self
                    .repository
                    .lookup_binding(channel_id, &sender_id)
                    .await?;
                self.repository
                    .complete_inbound_receipt(
                        channel_id,
                        &sender_id,
                        &external_message_id,
                        disposition.receipt_value(),
                        binding.as_ref().map(|value| value.installation_id),
                        binding.as_ref().map(|value| value.id),
                    )
                    .await?;
                Ok(disposition)
            }
            Err(error) => {
                self.repository
                    .fail_inbound_receipt(channel_id, &sender_id, &external_message_id)
                    .await?;
                Err(error)
            }
        }
    }

    async fn handle_message(
        &self,
        mut message: crate::channel::ChannelMessage,
    ) -> Result<InboundDisposition, RelayError> {
        message.text = normalize_text(&message.text).map_err(|_| RelayError::InvalidMessage)?;
        let channel_id = self.channel.id().as_str();
        let existing = self
            .repository
            .lookup_binding(channel_id, &message.sender_id)
            .await?;
        if existing.is_none() {
            if let Some(code) = pairing_code(&message.text) {
                if let Some(binding) = self
                    .repository
                    .redeem_pairing(channel_id, &message.sender_id, code)
                    .await?
                {
                    let text = "This account is paired with Aegis.";
                    self.channel
                        .send_text(&binding.destination_id, text, &message.external_message_id)
                        .await?;
                    return Ok(InboundDisposition::Paired);
                }
            }
            self.channel
                .send_text(
                    &message.sender_id,
                    "Pair this account with Aegis using AEGIS <one-time-code> or /pair <one-time-code>.",
                    &message.external_message_id,
                )
                .await?;
            return Ok(InboundDisposition::PairingPromptSent);
        }

        let binding = existing.expect("checked as present");
        if is_pairing_attempt(&message.text) {
            self.channel
                .send_text(
                    &binding.destination_id,
                    "This account is already paired with Aegis.",
                    &message.external_message_id,
                )
                .await?;
            return Ok(InboundDisposition::Paired);
        }
        let command = parse_command(&message.text).map_err(|_| RelayError::InvalidMessage)?;
        let now = Utc::now();
        let envelope_id =
            stable_envelope_id(channel_id, &message.sender_id, &message.external_message_id);
        let envelope = CommandEnvelope {
            envelope_id,
            request_id: stable_request_id(
                channel_id,
                &message.sender_id,
                &message.external_message_id,
            ),
            installation_id: binding.installation_id,
            actor_id: binding.actor_id,
            channel: channel_id.to_owned(),
            sender_id: message.sender_id,
            external_message_id: message.external_message_id,
            issued_at: now,
            expires_at: now + chrono::Duration::seconds(COMMAND_TTL_SECONDS),
            command,
        };
        self.transport.publish(&envelope).await?;
        Ok(InboundDisposition::Published)
    }

    pub async fn deliver_event(&self, event: &AegisEvent) -> Result<(), RelayError> {
        event.validate().map_err(|_| RelayError::InvalidMessage)?;
        if event
            .expires_at
            .is_some_and(|expires_at| expires_at <= Utc::now().timestamp())
        {
            return Ok(());
        }
        let event_kind = event.kind.as_str();
        if !self
            .repository
            .notifications_enabled(
                event.installation_id,
                self.channel.id().as_str(),
                event_kind,
            )
            .await?
        {
            return Ok(());
        }
        let text = self.channel.render_event(event)?;
        let targets = self
            .repository
            .targets_for_installation(
                event.installation_id,
                &event.actor_id,
                self.channel.id().as_str(),
            )
            .await?;
        for target in targets {
            self.deliver_to_target(event, &text, target).await?;
        }
        Ok(())
    }

    async fn deliver_to_target(
        &self,
        event: &AegisEvent,
        text: &str,
        target: DeliveryTarget,
    ) -> Result<(), RelayError> {
        if self
            .repository
            .was_delivered(
                self.channel.id().as_str(),
                event.event_id,
                target.binding_id,
            )
            .await?
        {
            return Ok(());
        }
        self.repository
            .record_delivery_attempt(
                self.channel.id().as_str(),
                event.event_id,
                target.binding_id,
            )
            .await?;
        match self
            .channel
            .send_text(&target.destination_id, text, &event.event_id.to_string())
            .await
        {
            Ok(provider_message_id) => {
                self.repository
                    .mark_delivered(
                        self.channel.id().as_str(),
                        event.event_id,
                        target.binding_id,
                        provider_message_id.as_deref(),
                    )
                    .await?;
                Ok(())
            }
            Err(error) => {
                self.repository
                    .mark_delivery_failed(
                        self.channel.id().as_str(),
                        event.event_id,
                        target.binding_id,
                    )
                    .await?;
                Err(error.into())
            }
        }
    }
}

fn pairing_code(text: &str) -> Option<&str> {
    let mut pieces = text.split_whitespace();
    let command = pieces.next()?;
    if command != "/pair" && !command.eq_ignore_ascii_case("AEGIS") {
        return None;
    }
    let code = pieces.next()?;
    if pieces.next().is_some()
        || code.len() != 43
        || !code
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return None;
    }
    Some(code)
}

fn is_pairing_attempt(text: &str) -> bool {
    text.split_whitespace()
        .next()
        .is_some_and(|command| command == "/pair" || command.eq_ignore_ascii_case("AEGIS"))
}

fn stable_envelope_id(channel: &str, sender: &str, external_message_id: &str) -> Uuid {
    stable_id(
        b"aegis-relay-envelope-v1",
        channel,
        sender,
        external_message_id,
    )
}

fn stable_request_id(channel: &str, sender: &str, external_message_id: &str) -> Uuid {
    stable_id(
        b"aegis-relay-request-v1",
        channel,
        sender,
        external_message_id,
    )
}

fn stable_id(domain: &[u8], channel: &str, sender: &str, external_message_id: &str) -> Uuid {
    let mut digest = Sha256::new();
    digest.update(domain);
    digest.update(b"\0");
    digest.update(channel.as_bytes());
    digest.update(b"\0");
    digest.update(sender.as_bytes());
    digest.update(b"\0");
    digest.update(external_message_id.as_bytes());
    let bytes = digest.finalize();
    let mut uuid_bytes = [0; 16];
    uuid_bytes.copy_from_slice(&bytes[..16]);
    uuid_bytes[6] = (uuid_bytes[6] & 0x0f) | 0x40;
    uuid_bytes[8] = (uuid_bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(uuid_bytes)
}

impl crate::model::RemoteEventKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::TaskStarted => "task_started",
            Self::Progress => "progress",
            Self::ApprovalRequired => "approval_required",
            Self::Blocked => "blocked",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Reply => "reply",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_codes_must_be_one_exact_token() {
        let code = "a".repeat(43);
        assert_eq!(pairing_code(&format!("/pair {code}")), Some(code.as_str()));
        assert_eq!(pairing_code(&format!("AEGIS {code}")), Some(code.as_str()));
        assert_eq!(pairing_code(&format!("aegis {code}")), Some(code.as_str()));
        assert_eq!(pairing_code(&format!("/pair {code} extra")), None);
        assert_eq!(pairing_code(&format!("AEGIS {code} extra")), None);
        assert_eq!(pairing_code("/pair short"), None);
        assert_eq!(pairing_code("hello"), None);
    }

    #[test]
    fn pairing_attempt_detection_reserves_only_the_exact_command_token() {
        assert!(is_pairing_attempt("/pair anything"));
        assert!(is_pairing_attempt("AEGIS anything"));
        assert!(is_pairing_attempt("aegis anything"));
        assert!(!is_pairing_attempt("/pairing anything"));
        assert!(!is_pairing_attempt("please use AEGIS"));
    }

    #[test]
    fn envelope_id_is_stable_per_sender_and_external_message() {
        let first = stable_envelope_id("whatsapp", "+4915112345678", "msg-2");
        assert_eq!(
            first,
            stable_envelope_id("whatsapp", "+4915112345678", "msg-2")
        );
        assert_ne!(
            first,
            stable_envelope_id("whatsapp", "+4915112345678", "msg-3")
        );
        assert_ne!(
            first,
            stable_envelope_id("whatsapp", "+4915110000000", "msg-2")
        );
        assert_ne!(
            first,
            stable_envelope_id("imessage", "+4915112345678", "msg-2")
        );
    }

    #[test]
    fn request_and_envelope_ids_are_distinct_stable_correlation_ids() {
        let envelope_id = stable_envelope_id("whatsapp", "+4915112345678", "msg-2");
        let request_id = stable_request_id("whatsapp", "+4915112345678", "msg-2");
        assert_ne!(envelope_id, request_id);
        assert_eq!(
            request_id,
            stable_request_id("whatsapp", "+4915112345678", "msg-2")
        );
    }
}
