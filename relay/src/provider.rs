use std::time::Duration;

use crate::channel::{
    ChannelCapabilities, ChannelError, ChannelId, ChannelMessage, MessagingChannel,
};
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

type HmacSha256 = Hmac<Sha256>;
const MAX_PROVIDER_RESPONSE_BYTES: usize = 32 * 1024;
pub use crate::channel::ChannelError as ProviderError;

#[derive(Clone)]
pub struct EvolutionAdapter {
    client: Client,
    base_url: Url,
    instance: String,
    api_key: String,
    webhook_secret: Vec<u8>,
    self_owner: Option<String>,
    self_account_pending: bool,
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
            self_owner: None,
            self_account_pending: false,
        })
    }

    /// Explicitly opt into the linked owner's self-DM and own group messages.
    pub fn with_self_account(mut self, owner: &str) -> Result<Self, ProviderError> {
        self.self_owner = Some(normalize_phone(owner).ok_or(ProviderError::Configuration)?);
        self.self_account_pending = false;
        Ok(self)
    }

    /// Keep a healthy gateway inert while the linked account's owner is unknown.
    pub fn with_self_account_pending(mut self) -> Self {
        self.self_owner = None;
        self.self_account_pending = true;
        self
    }

    async fn response_json(mut response: reqwest::Response) -> Result<Value, ProviderError> {
        if !response.status().is_success()
            || response
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
        serde_json::from_slice(&bytes).map_err(|_| ProviderError::Delivery)
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
impl MessagingChannel for EvolutionAdapter {
    fn id(&self) -> ChannelId {
        ChannelId::WhatsApp
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            text: true,
            attachments: false,
            idempotency_keys: false,
        }
    }

    fn task_groups(&self) -> bool {
        true
    }

    async fn create_task_group(&self, owner: &str, task_id: &str) -> Result<String, ChannelError> {
        if self.self_account_pending {
            return Err(ProviderError::Configuration);
        }
        let owner = normalize_phone(owner).ok_or(ProviderError::InvalidRequest)?;
        let phone = owner.trim_start_matches('+');
        // Evolution 2.3.7 requires at least one participant and ten numeric characters.
        // Including the linked owner is the only self-account request permitted by that DTO.
        if phone.len() < 10
            || task_id.is_empty()
            || task_id.len() > 80
            || !task_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            || self
                .self_owner
                .as_ref()
                .is_some_and(|configured| configured != &owner)
        {
            return Err(ProviderError::InvalidRequest);
        }
        let endpoint = self
            .base_url
            .join(&format!("group/create/{}", self.instance))
            .map_err(|_| ProviderError::Configuration)?;
        let response = self
            .client
            .post(endpoint)
            .header("apikey", &self.api_key)
            .json(
                &serde_json::json!({"subject":format!("Aegis {task_id}"), "participants":[phone]}),
            )
            .send()
            .await
            .map_err(|_| ProviderError::Delivery)?;
        let result = Self::response_json(response).await?;
        result
            .get("id")
            .and_then(Value::as_str)
            .filter(|jid| valid_group_jid(jid))
            .map(str::to_owned)
            .ok_or(ProviderError::Delivery)
    }

    fn parse_inbound(
        &self,
        headers: &HeaderMap,
        raw_body: &[u8],
    ) -> Result<Option<ChannelMessage>, ChannelError> {
        if !self.verify_signature(headers, raw_body) {
            return Err(ProviderError::Authentication);
        }
        if self.self_account_pending {
            return Ok(None);
        }
        let value: Value =
            serde_json::from_slice(raw_body).map_err(|_| ProviderError::InvalidRequest)?;
        if value.get("event").and_then(Value::as_str) != Some("messages.upsert") {
            return Ok(None);
        }
        let key = value
            .pointer("/data/key")
            .ok_or(ProviderError::InvalidRequest)?;
        let Some(from_me) = key.get("fromMe").and_then(Value::as_bool) else {
            return Ok(None);
        };
        if from_me != self.self_owner.is_some() {
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
        let conversation_id = valid_group_jid(remote_jid).then(|| remote_jid.to_owned());
        let sender_id = if conversation_id.is_some() {
            let participant = key
                .get("participantAlt")
                .and_then(Value::as_str)
                .and_then(phone_from_jid)
                .or_else(|| {
                    key.get("participant")
                        .and_then(Value::as_str)
                        .and_then(phone_from_jid)
                });
            match (&self.self_owner, participant) {
                (Some(owner), Some(participant)) if &participant == owner => participant,
                // Baileys can omit participant on the linked account's own outgoing key.
                (Some(owner), None)
                    if key.get("participant").is_none() && key.get("participantAlt").is_none() =>
                {
                    owner.clone()
                }
                (None, Some(participant)) => participant,
                _ => return Ok(None),
            }
        } else {
            // A group-looking or broadcast JID must never be rescued by a direct sender alias.
            if phone_from_jid(remote_jid).is_none() && !valid_lid_jid(remote_jid) {
                return Ok(None);
            }
            let Some(sender) = key
                .get("remoteJidAlt")
                .and_then(Value::as_str)
                .and_then(phone_from_jid)
                .or_else(|| phone_from_jid(remote_jid))
            else {
                return Ok(None);
            };
            if self
                .self_owner
                .as_ref()
                .is_some_and(|owner| owner != &sender)
            {
                return Ok(None);
            }
            sender
        };
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
        Ok(Some(ChannelMessage {
            sender_id,
            external_message_id,
            text: text.to_owned(),
            conversation_id,
            from_me,
        }))
    }

    async fn send_text(
        &self,
        destination: &str,
        text: &str,
        _idempotency_key: &str,
    ) -> Result<Option<String>, ChannelError> {
        if self.self_account_pending {
            return Err(ProviderError::Configuration);
        }
        // Evolution's current sendText DTO has no idempotency-key field. The key is
        // therefore only useful to provider adapters whose APIs support deduplication.
        let receiver = if valid_group_jid(destination) {
            destination.to_owned()
        } else {
            normalize_phone(destination)
                .ok_or(ProviderError::InvalidRequest)?
                .trim_start_matches('+')
                .to_owned()
        };
        let endpoint = self
            .base_url
            .join(&format!("message/sendText/{}", self.instance))
            .map_err(|_| ProviderError::Configuration)?;
        let body = EvolutionSendRequest {
            number: &receiver,
            text,
        };
        let response = self
            .client
            .post(endpoint)
            .header("apikey", &self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|_| ProviderError::Delivery)?;
        let result = Self::response_json(response).await?;
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

fn phone_from_jid(jid: &str) -> Option<String> {
    let user = jid.strip_suffix("@s.whatsapp.net")?;
    let phone = if let Some((phone, device)) = user.split_once(':') {
        if device.is_empty() || !device.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        phone
    } else {
        user
    };
    normalize_phone(phone)
}

fn valid_group_jid(jid: &str) -> bool {
    jid.strip_suffix("@g.us").is_some_and(|id| {
        !id.is_empty()
            && id.len() <= 80
            && id.as_bytes().first().is_some_and(u8::is_ascii_digit)
            && id.as_bytes().last().is_some_and(u8::is_ascii_digit)
            && id.bytes().all(|b| b.is_ascii_digit() || b == b'-')
    })
}

fn valid_lid_jid(jid: &str) -> bool {
    jid.strip_suffix("@lid").is_some_and(|user| {
        let (id, device) = user
            .split_once(':')
            .map(|(id, device)| (id, Some(device)))
            .unwrap_or((user, None));
        !id.is_empty()
            && id.len() <= 30
            && id.bytes().all(|b| b.is_ascii_digit())
            && device.is_none_or(|value| {
                !value.is_empty() && value.len() <= 5 && value.bytes().all(|b| b.is_ascii_digit())
            })
    })
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

    fn self_adapter() -> EvolutionAdapter {
        EvolutionAdapter::new(
            "https://evolution.example",
            "instance-a",
            "fixture-key",
            SECRET.to_vec(),
        )
        .unwrap()
        .with_self_account("+4915112345678")
        .unwrap()
    }

    fn parse_key(adapter: &EvolutionAdapter, key: Value) -> Option<ChannelMessage> {
        let body = serde_json::to_vec(&serde_json::json!({"event":"messages.upsert", "data":{
            "key":key,"message":{"conversation":"/status"}}}))
        .unwrap();
        adapter
            .parse_inbound(&signed_headers(&body, Utc::now().timestamp()), &body)
            .unwrap()
    }

    #[test]
    fn self_account_only_accepts_linked_owner_self_dm() {
        let adapter = self_adapter();
        let own = serde_json::json!({"id":"self-1","fromMe":true,"remoteJid":"4915112345678@s.whatsapp.net"});
        let message = parse_key(&adapter, own.clone()).unwrap();
        assert!(message.from_me);
        assert_eq!(message.sender_id, "+4915112345678");
        assert_eq!(message.conversation_id, None);
        let mut foreign = own.clone();
        foreign["remoteJid"] = "4915198765432@s.whatsapp.net".into();
        assert!(parse_key(&adapter, foreign).is_none());
        let mut incoming = own;
        incoming["fromMe"] = false.into();
        assert!(parse_key(&adapter, incoming).is_none());
        let linked_alias = serde_json::json!({"id":"self-lid","fromMe":true,
            "remoteJid":"123456789012345@lid","remoteJidAlt":"4915112345678@s.whatsapp.net"});
        assert_eq!(
            parse_key(&adapter, linked_alias.clone()).unwrap().sender_id,
            "+4915112345678"
        );
        let mut unknown_alias = linked_alias.clone();
        unknown_alias
            .as_object_mut()
            .unwrap()
            .remove("remoteJidAlt");
        assert!(parse_key(&adapter, unknown_alias).is_none());
        let mut broadcast_alias = linked_alias;
        broadcast_alias["remoteJid"] = "status@broadcast".into();
        assert!(parse_key(&adapter, broadcast_alias).is_none());
        assert!(
            EvolutionAdapter::new("https://evolution.example", "i", "k", SECRET.to_vec())
                .unwrap()
                .with_self_account("not-a-phone")
                .is_err()
        );
    }

    #[tokio::test]
    async fn pending_self_account_ignores_contacts_and_cannot_send() {
        let adapter =
            EvolutionAdapter::new("http://127.0.0.1:1/", "fixture", "key", SECRET.to_vec())
                .unwrap()
                .with_self_account_pending();
        assert!(
            adapter
                .parse_inbound(&signed_headers(FIXTURE, Utc::now().timestamp()), FIXTURE)
                .unwrap()
                .is_none()
        );
        assert!(
            adapter
                .send_text("+4915112345678", "Never send", "fixture")
                .await
                .is_err()
        );
    }

    #[test]
    fn group_normalization_preserves_scope_and_checks_actual_participant() {
        let adapter = self_adapter();
        let own = serde_json::json!({"id":"group-self","fromMe":true,"remoteJid":"120363123456789@g.us",
            "participant":"4915112345678:2@s.whatsapp.net"});
        let message = parse_key(&adapter, own.clone()).unwrap();
        assert_eq!(
            message.conversation_id.as_deref(),
            Some("120363123456789@g.us")
        );
        assert_eq!(message.sender_id, "+4915112345678");
        let mut foreign = own.clone();
        foreign["participant"] = "4915198765432@s.whatsapp.net".into();
        assert!(parse_key(&adapter, foreign).is_none());
        let mut unsupported = own.clone();
        unsupported["participant"] = "123@lid".into();
        assert!(parse_key(&adapter, unsupported.clone()).is_none());
        let mut alias = unsupported;
        alias["participantAlt"] = "4915112345678@s.whatsapp.net".into();
        assert!(parse_key(&adapter, alias).is_some());
        let mut without_participant = own;
        without_participant
            .as_object_mut()
            .unwrap()
            .remove("participant");
        assert!(parse_key(&adapter, without_participant).is_some());
        let ordinary =
            EvolutionAdapter::new("https://evolution.example", "i", "k", SECRET.to_vec()).unwrap();
        assert!(
            parse_key(
                &ordinary,
                serde_json::json!({"id":"group-external","fromMe":false,
            "remoteJid":"120363123456789@g.us","participant":"4915112345678@s.whatsapp.net"})
            )
            .is_some()
        );
        assert!(!valid_group_jid("group@g.us"));
        assert!(!valid_group_jid("120363123456789@g.us/escape"));
    }

    #[tokio::test]
    async fn actual_group_gateway_contract_includes_owner_and_group_send_receiver() {
        use axum::{Json, Router, extract::State, routing::post};
        let (sender, mut calls) = tokio::sync::mpsc::unbounded_channel::<(HeaderMap, Value)>();
        async fn capture(
            State(sender): State<tokio::sync::mpsc::UnboundedSender<(HeaderMap, Value)>>,
            headers: HeaderMap,
            Json(body): Json<Value>,
        ) -> Json<Value> {
            let group = body.get("participants").is_some();
            sender.send((headers, body)).unwrap();
            Json(if group {
                serde_json::json!({"id":"120363123456789@g.us"})
            } else {
                serde_json::json!({"key":{"id":"reply-1"}})
            })
        }
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let gateway = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/group/create/instance-a", post(capture))
                    .route("/message/sendText/instance-a", post(capture))
                    .with_state(sender),
            )
            .await
            .unwrap();
        });
        let adapter = EvolutionAdapter::new(
            &format!("http://{address}/"),
            "instance-a",
            "fixture-key",
            SECRET.to_vec(),
        )
        .unwrap()
        .with_self_account("+4915112345678")
        .unwrap();
        assert!(adapter.task_groups());
        let task = "2e741498-03b4-43de-9011-92623129e9b1";
        let jid = adapter
            .create_task_group("+4915112345678", task)
            .await
            .unwrap();
        let (headers, body) = calls.recv().await.unwrap();
        assert_eq!(headers.get("apikey").unwrap(), "fixture-key");
        assert_eq!(
            body,
            serde_json::json!({"subject":format!("Aegis {task}"),"participants":["4915112345678"]})
        );
        assert_eq!(
            adapter
                .send_text(&jid, "group reply", "event-1")
                .await
                .unwrap(),
            Some("reply-1".into())
        );
        let (_, body) = calls.recv().await.unwrap();
        assert_eq!(body, serde_json::json!({"number":jid,"text":"group reply"}));
        assert!(
            adapter
                .create_task_group("+4915198765432", task)
                .await
                .is_err()
        );
        assert!(
            adapter
                .create_task_group("+4915112345678", "bad/task")
                .await
                .is_err()
        );
        assert!(calls.try_recv().is_err());
        gateway.abort();
    }

    #[tokio::test]
    async fn evolution_advertises_only_the_provider_capabilities_it_implements() {
        let adapter = EvolutionAdapter::new(
            "https://evolution.example",
            "instance-a",
            "not-a-real-provider-key",
            SECRET.to_vec(),
        )
        .unwrap();
        assert_eq!(adapter.id(), ChannelId::WhatsApp);
        assert_eq!(
            adapter.capabilities(),
            ChannelCapabilities {
                text: true,
                attachments: false,
                idempotency_keys: false,
            }
        );
        assert!(matches!(
            adapter
                .send_attachment(
                    "+4915112345678",
                    &crate::channel::ChannelAttachment {
                        file_name: "test.txt".into(),
                        content_type: "text/plain".into(),
                        data: b"fixture".to_vec(),
                    },
                    "event-1",
                )
                .await,
            Err(ChannelError::Unsupported)
        ));
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
