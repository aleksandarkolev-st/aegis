use async_trait::async_trait;
use axum::http::HeaderMap;
use thiserror::Error;

use crate::model::AegisEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChannelId {
    WhatsApp,
    IMessage,
}

impl ChannelId {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "whatsapp" => Some(Self::WhatsApp),
            "imessage" => Some(Self::IMessage),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WhatsApp => "whatsapp",
            Self::IMessage => "imessage",
        }
    }

    pub const fn display_name(self) -> &'static str {
        match self {
            Self::WhatsApp => "WhatsApp",
            Self::IMessage => "iMessage",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelMessage {
    pub sender_id: String,
    pub external_message_id: String,
    pub text: String,
    pub conversation_id: Option<String>,
    pub from_me: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelAttachment {
    pub file_name: String,
    pub content_type: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelCapabilities {
    pub text: bool,
    pub attachments: bool,
    pub idempotency_keys: bool,
}

#[derive(Debug, Error)]
pub enum ChannelError {
    #[error("channel authentication failed")]
    Authentication,
    #[error("channel message is invalid")]
    InvalidRequest,
    #[error("channel delivery failed")]
    Delivery,
    #[error("channel configuration is invalid")]
    Configuration,
    #[error("channel does not support this operation")]
    Unsupported,
}

/// A provider-independent messaging surface. Provider webhooks are normalized at
/// this boundary; relay routing and event delivery only see a channel identity and
/// plain message data.
#[async_trait]
pub trait MessagingChannel: Send + Sync {
    fn id(&self) -> ChannelId;

    fn capabilities(&self) -> ChannelCapabilities;

    fn task_groups(&self) -> bool {
        false
    }

    async fn create_task_group(
        &self,
        _owner: &str,
        _task_id: &str,
    ) -> Result<String, ChannelError> {
        Err(ChannelError::Unsupported)
    }

    fn parse_inbound(
        &self,
        headers: &HeaderMap,
        raw_body: &[u8],
    ) -> Result<Option<ChannelMessage>, ChannelError>;

    fn render_event(&self, event: &AegisEvent) -> Result<String, ChannelError> {
        event
            .notification_text()
            .map_err(|_| ChannelError::InvalidRequest)
    }

    async fn send_text(
        &self,
        destination: &str,
        text: &str,
        idempotency_key: &str,
    ) -> Result<Option<String>, ChannelError>;

    async fn send_attachment(
        &self,
        _destination: &str,
        _attachment: &ChannelAttachment,
        _idempotency_key: &str,
    ) -> Result<Option<String>, ChannelError> {
        Err(ChannelError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::ChannelId;

    #[test]
    fn channel_metadata_uses_only_stable_channel_keys() {
        assert_eq!(ChannelId::parse("whatsapp"), Some(ChannelId::WhatsApp));
        assert_eq!(ChannelId::parse("imessage"), Some(ChannelId::IMessage));
        assert_eq!(ChannelId::parse("evolution"), None);
        assert_eq!(ChannelId::IMessage.as_str(), "imessage");
    }
}
