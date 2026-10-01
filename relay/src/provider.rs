use std::time::Duration;

use async_trait::async_trait;
use axum::http::HeaderMap;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use hmac::{Hmac, Mac};
use reqwest::{Client, Url};
use serde::Serialize;
use serde_json::Value;
use sha2::Sha256;
use subtle::ConstantTimeEq;
use thiserror::Error;

type HmacSha256 = Hmac<Sha256>;
const MAX_PROVIDER_RESPONSE_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundWhatsAppMessage {
    pub sender_id: String,
    pub external_message_id: String,
    pub text: String,
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("provider authentication failed")]
    Authentication,
    #[error("provider request is invalid")]
    InvalidRequest,
    #[error("provider delivery failed")]
    Delivery,
    #[error("provider configuration is invalid")]
    Configuration,
}

#[async_trait]
pub trait WhatsAppProvider: Send + Sync {
    fn parse_inbound(
        &self,
        headers: &HeaderMap,
        raw_body: &[u8],
    ) -> Result<Option<InboundWhatsAppMessage>, ProviderError>;

    async fn send_text(
        &self,
        destination: &str,
        text: &str,
        idempotency_key: &str,
    ) -> Result<Option<String>, ProviderError>;
}

#[derive(Clone)]
pub struct EvolutionAdapter {
    client: Client,
    base_url: Url,
    instance: String,
    api_key: String,
    webhook_secret: Vec<u8>,
}

impl EvolutionAdapter {
    pub fn new(
        base_url: &str,
        instance: impl Into<String>,
        api_key: impl Into<String>,
        webhook_secret: impl Into<Vec<u8>>,
    ) -> Result<Self, ProviderError> {
        let base_url = Url::parse(base_url).map_err(|_| ProviderError::Configuration)?;
        let local_http = base_url.scheme() == "http"
            && matches!(base_url.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
        if base_url.scheme() != "https" && !local_http {
            return Err(ProviderError::Configuration);
        }
        if base_url.host_str().is_none() {
            return Err(ProviderError::Configuration);
        }
        let instance = instance.into();
        let api_key = api_key.into();
        let webhook_secret = webhook_secret.into();
        if instance.is_empty() || api_key.is_empty() || webhook_secret.len() < 32 {
            return Err(ProviderError::Configuration);
        }
        Ok(Self {
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(15))
                .build()
                .map_err(|_| ProviderError::Configuration)?,
            base_url,
            instance,
            api_key,
            webhook_secret,
        })
    }

    fn verify_signature(&self, headers: &HeaderMap, raw_body: &[u8]) -> bool {
        if headers
            .get("x-aegis-webhook-token")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| constant_time_match(value.as_bytes(), &self.webhook_secret))
        {
            return true;
        }
        let Some(timestamp) = headers
            .get("x-aegis-timestamp")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<i64>().ok())
        else {
            return false;
        };
        if Utc::now().timestamp().abs_diff(timestamp) > 300 {
            return false;
        }
        let Some(signature) = headers
            .get("x-aegis-signature")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("v1="))
            .and_then(|value| URL_SAFE_NO_PAD.decode(value).ok())
        else {
            return false;
        };
        let mut mac = match HmacSha256::new_from_slice(&self.webhook_secret) {
            Ok(mac) => mac,
            Err(_) => return false,
        };
        mac.update(timestamp.to_string().as_bytes());
        mac.update(b".");
        mac.update(raw_body);
        mac.verify_slice(&signature).is_ok()
    }
}

#[async_trait]
impl WhatsAppProvider for EvolutionAdapter {
    fn parse_inbound(
        &self,
        headers: &HeaderMap,
        raw_body: &[u8],
    ) -> Result<Option<InboundWhatsAppMessage>, ProviderError> {
        if !self.verify_signature(headers, raw_body) {
            return Err(ProviderError::Authentication);
        }
        let value: Value =
            serde_json::from_slice(raw_body).map_err(|_| ProviderError::InvalidRequest)?;
        if value.get("event").and_then(Value::as_str) != Some("messages.upsert") {
            return Ok(None);
        }
        let key = value
            .pointer("/data/key")
            .ok_or(ProviderError::InvalidRequest)?;
        if key.get("fromMe").and_then(Value::as_bool) != Some(false) {
            return Ok(None);
        }
        let external_message_id = key
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| {
                !value.is_empty()
                    && value.len() <= 200
                    && value.bytes().all(|byte| byte.is_ascii_graphic())
            })
            .ok_or(ProviderError::InvalidRequest)?
            .to_owned();
        let remote_jid = key
            .get("remoteJid")
            .and_then(Value::as_str)
            .ok_or(ProviderError::InvalidRequest)?;
        if !remote_jid.ends_with("@s.whatsapp.net") {
            return Ok(None);
        }
        let sender_jid = key
            .get("remoteJidAlt")
            .and_then(Value::as_str)
            .filter(|jid| jid.ends_with("@s.whatsapp.net"))
            .unwrap_or(remote_jid);
        let digits = sender_jid
            .strip_suffix("@s.whatsapp.net")
            .ok_or(ProviderError::InvalidRequest)?;
        let sender_id = normalize_phone(digits).ok_or(ProviderError::InvalidRequest)?;
        let message = value
            .pointer("/data/message/conversation")
            .and_then(Value::as_str)
            .or_else(|| {
                value
                    .pointer("/data/message/extendedTextMessage/text")
                    .and_then(Value::as_str)
            });
        let Some(text) = message else {
            return Ok(None);
        };
        Ok(Some(InboundWhatsAppMessage {
            sender_id,
            external_message_id,
            text: text.to_owned(),
        }))
    }

    async fn send_text(
        &self,
        destination: &str,
        text: &str,
        _idempotency_key: &str,
    ) -> Result<Option<String>, ProviderError> {
        // Evolution's current sendText DTO has no idempotency-key field. The key is
        // therefore only useful to provider adapters whose APIs support deduplication.
        let digits = normalize_phone(destination).ok_or(ProviderError::InvalidRequest)?;
        let endpoint = self
            .base_url
            .join(&format!("message/sendText/{}", self.instance))
            .map_err(|_| ProviderError::Configuration)?;
        let body = EvolutionSendRequest {
            number: digits.trim_start_matches('+'),
            text,
        };
        let mut response = self
            .client
            .post(endpoint)
            .header("apikey", &self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|_| ProviderError::Delivery)?;
        if !response.status().is_success() {
            return Err(ProviderError::Delivery);
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_PROVIDER_RESPONSE_BYTES as u64)
        {
            return Err(ProviderError::Delivery);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| ProviderError::Delivery)?
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_PROVIDER_RESPONSE_BYTES {
                return Err(ProviderError::Delivery);
            }
            bytes.extend_from_slice(&chunk);
        }
        let result: Value = serde_json::from_slice(&bytes).map_err(|_| ProviderError::Delivery)?;
        Ok(result
            .pointer("/key/id")
            .and_then(Value::as_str)
            .filter(|id| {
                !id.is_empty() && id.len() <= 200 && id.bytes().all(|byte| byte.is_ascii_graphic())
            })
            .map(str::to_owned))
    }
}

#[derive(Serialize)]
struct EvolutionSendRequest<'a> {
    number: &'a str,
    text: &'a str,
}

pub fn normalize_phone(input: &str) -> Option<String> {
    let digits = input.strip_prefix('+').unwrap_or(input);
    if !(8..=15).contains(&digits.len()) || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(format!("+{digits}"))
}

/// Build the signature Evolution's configured webhook proxy sends to this service.
/// The timestamp is part of the MAC to limit replay to the five-minute acceptance window.
pub fn sign_webhook(secret: &[u8], timestamp: i64, raw_body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts arbitrary key lengths");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(raw_body);
    format!("v1={}", URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
}

fn constant_time_match(supplied: &[u8], expected: &[u8]) -> bool {
    supplied.len() == expected.len() && bool::from(supplied.ct_eq(expected))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use tokio::{io::AsyncWriteExt, net::TcpListener, time::timeout};

    const FIXTURE: &[u8] = include_bytes!("../tests/fixtures/evolution-inbound.json");
    const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";

    fn signed_headers(body: &[u8], timestamp: i64) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-aegis-timestamp",
            HeaderValue::from_str(&timestamp.to_string()).unwrap(),
        );
        headers.insert(
            "x-aegis-signature",
            HeaderValue::from_str(&sign_webhook(SECRET, timestamp, body)).unwrap(),
        );
        headers
    }

    #[test]
    fn evolution_fixture_authenticates_and_normalizes_sender() {
        let adapter = EvolutionAdapter::new(
            "https://evolution.example",
            "instance-a",
            "not-a-real-provider-key",
            SECRET.to_vec(),
        )
        .unwrap();
        let message = adapter
            .parse_inbound(&signed_headers(FIXTURE, Utc::now().timestamp()), FIXTURE)
            .unwrap()
            .unwrap();
        assert_eq!(message.sender_id, "+4915112345678");
        assert_eq!(message.external_message_id, "wamid.fixture.001");
        assert_eq!(message.text, "/approve_once request-123");
    }

    #[test]
    fn evolution_fixture_rejects_forged_or_replayed_webhooks() {
        let adapter = EvolutionAdapter::new(
            "https://evolution.example",
            "instance-a",
            "not-a-real-provider-key",
            SECRET.to_vec(),
        )
        .unwrap();
        let forged = HeaderMap::new();
        assert!(matches!(
            adapter.parse_inbound(&forged, FIXTURE),
            Err(ProviderError::Authentication)
        ));
        let stale = signed_headers(FIXTURE, Utc::now().timestamp() - 301);
        assert!(matches!(
            adapter.parse_inbound(&stale, FIXTURE),
            Err(ProviderError::Authentication)
        ));
    }

    #[test]
    fn rejects_groups_and_invalid_sender_addresses() {
        assert_eq!(
            normalize_phone("4915112345678"),
            Some("+4915112345678".into())
        );
        assert_eq!(normalize_phone("123@g.us"), None);
        assert_eq!(normalize_phone("123"), None);
    }

    #[test]
    fn inbound_parser_rejects_groups_even_with_a_direct_sender_alias() {
        let adapter = EvolutionAdapter::new(
            "https://evolution.example",
            "instance-a",
            "not-a-real-provider-key",
            SECRET.to_vec(),
        )
        .unwrap();
        let group = br#"{"event":"messages.upsert","data":{"key":{"id":"group-1","remoteJid":"12345-678@g.us","remoteJidAlt":"4915112345678@s.whatsapp.net","fromMe":false},"message":{"conversation":"do not route this"}}}"#;
        let headers = signed_headers(group, Utc::now().timestamp());
        assert!(adapter.parse_inbound(&headers, group).unwrap().is_none());

        let missing_from_me = br#"{"event":"messages.upsert","data":{"key":{"id":"missing-flag","remoteJid":"4915112345678@s.whatsapp.net"},"message":{"conversation":"do not route this"}}}"#;
        let headers = signed_headers(missing_from_me, Utc::now().timestamp());
        assert!(
            adapter
                .parse_inbound(&headers, missing_from_me)
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn outbound_send_does_not_follow_redirects_with_the_provider_key() {
        let relay_target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay_address = relay_target.local_addr().unwrap();
        let second_target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let second_address = second_target.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = relay_target.accept().await.unwrap();
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://{second_address}/steal\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let adapter = EvolutionAdapter::new(
            &format!("http://{relay_address}"),
            "instance-a",
            "provider-secret-that-must-not-follow-redirects",
            SECRET.to_vec(),
        )
        .unwrap();
        assert!(
            adapter
                .send_text("+15551234567", "reply", "stable-event-id")
                .await
                .is_err()
        );
        assert!(
            timeout(Duration::from_millis(150), second_target.accept())
                .await
                .is_err()
        );
        server.await.unwrap();
    }
}
