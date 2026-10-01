use std::{collections::HashSet, sync::Mutex};

use aegis_relay::{
    AegisEvent, AgentCommand, CommandEnvelope, InboundDisposition, RelayService, RemoteEventKind,
    provider::{InboundWhatsAppMessage, ProviderError, WhatsAppProvider},
    repository::{
        ChannelBinding, DeliveryTarget, InboundReceiptClaim, RelayRepository, RepositoryError,
    },
    service::{CommandTransport, TransportError},
};
use arun::remote::{Authority, Command as AegisCommand, CommandEnvelope as AegisCommandEnvelope};
use arun::storage::Store;
use async_trait::async_trait;
use axum::http::{HeaderMap, HeaderValue};
use uuid::Uuid;

#[derive(Default)]
struct FakeProvider {
    sends: Mutex<Vec<(String, String, String)>>,
    ambiguous_first_send: Mutex<bool>,
}

#[async_trait]
impl WhatsAppProvider for FakeProvider {
    fn parse_inbound(
        &self,
        headers: &HeaderMap,
        raw_body: &[u8],
    ) -> Result<Option<InboundWhatsAppMessage>, ProviderError> {
        if headers
            .get("x-fake-auth")
            .and_then(|value| value.to_str().ok())
            != Some("local-fixture")
        {
            return Err(ProviderError::Authentication);
        }
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fixture {
            sender_id: String,
            external_message_id: String,
            text: String,
        }
        let fixture: Fixture =
            serde_json::from_slice(raw_body).map_err(|_| ProviderError::InvalidRequest)?;
        Ok(Some(InboundWhatsAppMessage {
            sender_id: fixture.sender_id,
            external_message_id: fixture.external_message_id,
            text: fixture.text,
        }))
    }

    async fn send_text(
        &self,
        destination: &str,
        text: &str,
        idempotency_key: &str,
    ) -> Result<Option<String>, ProviderError> {
        self.sends.lock().unwrap().push((
            destination.to_owned(),
            text.to_owned(),
            idempotency_key.to_owned(),
        ));
        if std::mem::take(&mut *self.ambiguous_first_send.lock().unwrap()) {
            // Model the provider accepting the send while the response is lost.
            return Err(ProviderError::Delivery);
        }
        Ok(Some(format!(
            "provider-{}",
            self.sends.lock().unwrap().len()
        )))
    }
}

#[tokio::test]
async fn ambiguous_provider_send_retries_at_least_once_then_receipt_deduplicates() {
    let installation_id = Uuid::new_v4();
    let binding = ChannelBinding {
        id: Uuid::new_v4(),
        user_id: Uuid::new_v4(),
        installation_id,
        channel: "whatsapp".into(),
        actor_id: "owner-1".into(),
        sender_id: "+4915112345678".into(),
        destination_id: "+4915112345678".into(),
    };
    let repository = std::sync::Arc::new(FakeRepository::default());
    *repository.binding.lock().unwrap() = Some(binding);
    *repository.notifications.lock().unwrap() = true;
    let provider = std::sync::Arc::new(FakeProvider::default());
    *provider.ambiguous_first_send.lock().unwrap() = true;
    let service = RelayService::new(
        repository,
        std::sync::Arc::new(FakeTransport::default()),
        provider.clone(),
    );
    let event = AegisEvent {
        event_id: Uuid::new_v4(),
        installation_id,
        actor_id: "owner-1".into(),
        kind: RemoteEventKind::Reply,
        task_id: None,
        challenge_id: None,
        display_detail: None,
        reply_text: Some("The task completed.".into()),
    };

    assert!(service.deliver_event(&event).await.is_err());
    service.deliver_event(&event).await.unwrap();
    service.deliver_event(&event).await.unwrap();

    let sends = provider.sends.lock().unwrap();
    assert_eq!(sends.len(), 2);
    assert_eq!(sends[0].2, event.event_id.to_string());
    assert_eq!(sends[1].2, event.event_id.to_string());
}

#[derive(Default)]
struct FakeTransport {
    commands: Mutex<Vec<CommandEnvelope>>,
}

#[async_trait]
impl CommandTransport for FakeTransport {
    async fn publish(&self, envelope: &CommandEnvelope) -> Result<(), TransportError> {
        self.commands.lock().unwrap().push(envelope.clone());
        Ok(())
    }
}

#[derive(Default)]
struct FakeRepository {
    binding: Mutex<Option<ChannelBinding>>,
    other_bindings: Mutex<Vec<ChannelBinding>>,
    pairing: Mutex<Option<(String, Uuid, String)>>,
    receipts: Mutex<HashSet<(Uuid, Uuid)>>,
    inbound_receipts: Mutex<HashSet<(String, String, String)>>,
    attempts: Mutex<Vec<(Uuid, Uuid)>>,
    notifications: Mutex<bool>,
}

#[async_trait]
impl RelayRepository for FakeRepository {
    async fn claim_inbound_receipt(
        &self,
        channel: &str,
        sender_id: &str,
        external_message_id: &str,
    ) -> Result<InboundReceiptClaim, RepositoryError> {
        let key = (
            channel.to_owned(),
            sender_id.to_owned(),
            external_message_id.to_owned(),
        );
        let is_new = self.inbound_receipts.lock().unwrap().insert(key);
        Ok(if is_new {
            InboundReceiptClaim::Claimed
        } else {
            InboundReceiptClaim::Duplicate
        })
    }

    async fn complete_inbound_receipt(
        &self,
        _channel: &str,
        _sender_id: &str,
        _external_message_id: &str,
        _disposition: &str,
        _installation_id: Option<Uuid>,
        _binding_id: Option<Uuid>,
    ) -> Result<(), RepositoryError> {
        Ok(())
    }

    async fn fail_inbound_receipt(
        &self,
        _channel: &str,
        _sender_id: &str,
        _external_message_id: &str,
    ) -> Result<(), RepositoryError> {
        Ok(())
    }

    async fn prune_expired_receipts(&self) -> Result<u64, RepositoryError> {
        Ok(0)
    }

    async fn lookup_binding(
        &self,
        channel: &str,
        sender_id: &str,
    ) -> Result<Option<ChannelBinding>, RepositoryError> {
        Ok(self
            .binding
            .lock()
            .unwrap()
            .clone()
            .filter(|binding| binding.channel == channel && binding.sender_id == sender_id))
    }

    async fn redeem_pairing(
        &self,
        channel: &str,
        sender_id: &str,
        code: &str,
    ) -> Result<Option<ChannelBinding>, RepositoryError> {
        if channel != "whatsapp" {
            return Ok(None);
        }
        let mut pairing = self.pairing.lock().unwrap();
        let Some((expected_code, installation_id, actor_id)) = pairing.as_ref() else {
            return Ok(None);
        };
        if expected_code != code {
            return Ok(None);
        }
        let binding = ChannelBinding {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            installation_id: *installation_id,
            channel: channel.to_owned(),
            actor_id: actor_id.clone(),
            sender_id: sender_id.to_owned(),
            destination_id: sender_id.to_owned(),
        };
        *self.binding.lock().unwrap() = Some(binding.clone());
        *pairing = None;
        Ok(Some(binding))
    }

    async fn targets_for_installation(
        &self,
        installation_id: Uuid,
        actor_id: &str,
    ) -> Result<Vec<DeliveryTarget>, RepositoryError> {
        let mut bindings = self
            .binding
            .lock()
            .unwrap()
            .clone()
            .into_iter()
            .collect::<Vec<_>>();
        bindings.extend(self.other_bindings.lock().unwrap().iter().cloned());
        Ok(bindings
            .into_iter()
            .filter(|binding| {
                binding.installation_id == installation_id && binding.actor_id == actor_id
            })
            .map(|binding| DeliveryTarget {
                binding_id: binding.id,
                destination_id: binding.destination_id,
            })
            .collect())
    }

    async fn notifications_enabled(
        &self,
        _installation_id: Uuid,
        _channel: &str,
        _event_kind: &str,
    ) -> Result<bool, RepositoryError> {
        Ok(*self.notifications.lock().unwrap())
    }

    async fn was_delivered(
        &self,
        event_id: Uuid,
        binding_id: Uuid,
    ) -> Result<bool, RepositoryError> {
        Ok(self
            .receipts
            .lock()
            .unwrap()
            .contains(&(event_id, binding_id)))
    }

    async fn record_delivery_attempt(
        &self,
        event_id: Uuid,
        binding_id: Uuid,
    ) -> Result<(), RepositoryError> {
        self.attempts.lock().unwrap().push((event_id, binding_id));
        Ok(())
    }

    async fn mark_delivered(
        &self,
        event_id: Uuid,
        binding_id: Uuid,
        _provider_message_id: Option<&str>,
    ) -> Result<(), RepositoryError> {
        self.receipts.lock().unwrap().insert((event_id, binding_id));
        Ok(())
    }

    async fn mark_delivery_failed(
        &self,
        _event_id: Uuid,
        _binding_id: Uuid,
    ) -> Result<(), RepositoryError> {
        Ok(())
    }
}

#[tokio::test]
async fn local_whatsapp_to_aegis_and_reply_delivery_smoke() {
    let installation_id = Uuid::new_v4();
    let binding = ChannelBinding {
        id: Uuid::new_v4(),
        user_id: Uuid::new_v4(),
        installation_id,
        channel: "whatsapp".into(),
        actor_id: "owner-1".into(),
        sender_id: "+4915112345678".into(),
        destination_id: "+4915112345678".into(),
    };
    let repository = std::sync::Arc::new(FakeRepository::default());
    *repository.binding.lock().unwrap() = Some(binding.clone());
    *repository.other_bindings.lock().unwrap() = vec![ChannelBinding {
        id: Uuid::new_v4(),
        user_id: binding.user_id,
        installation_id,
        channel: "whatsapp".into(),
        actor_id: "owner-2".into(),
        sender_id: "+4915119999999".into(),
        destination_id: "+4915119999999".into(),
    }];
    *repository.notifications.lock().unwrap() = true;
    let transport = std::sync::Arc::new(FakeTransport::default());
    let provider = std::sync::Arc::new(FakeProvider::default());
    let service = RelayService::new(repository.clone(), transport.clone(), provider.clone());

    let mut headers = HeaderMap::new();
    headers.insert("x-fake-auth", HeaderValue::from_static("local-fixture"));
    let inbound = br#"{"sender_id":"+4915112345678","external_message_id":"wamid.fixture.001","text":"/approve_once request-123"}"#;
    assert_eq!(
        service.handle_webhook(&headers, inbound).await.unwrap(),
        InboundDisposition::Published
    );
    assert_eq!(
        service.handle_webhook(&headers, inbound).await.unwrap(),
        InboundDisposition::Ignored
    );
    let command = transport.commands.lock().unwrap()[0].clone();
    assert_eq!(
        command.command,
        AgentCommand::ApproveOnce {
            challenge_id: "request-123".into()
        }
    );
    assert_eq!(command.installation_id, installation_id);
    assert_eq!(command.actor_id, "owner-1");
    assert_eq!(
        command.expires_at.timestamp() - command.issued_at.timestamp(),
        aegis_relay::model::COMMAND_TTL_SECONDS
    );

    let event = AegisEvent {
        event_id: Uuid::new_v4(),
        installation_id,
        actor_id: "owner-1".into(),
        kind: RemoteEventKind::Reply,
        task_id: None,
        challenge_id: None,
        display_detail: None,
        reply_text: Some("The task completed.".into()),
    };
    service.deliver_event(&event).await.unwrap();
    service.deliver_event(&event).await.unwrap();
    let sends = provider.sends.lock().unwrap();
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].0, "+4915112345678");
    assert_eq!(sends[0].1, "The task completed.");
    drop(sends);

    let other_actor_event = AegisEvent {
        event_id: Uuid::new_v4(),
        installation_id,
        actor_id: "owner-2".into(),
        kind: RemoteEventKind::Reply,
        task_id: None,
        challenge_id: None,
        display_detail: None,
        reply_text: Some("Private to the second actor.".into()),
    };
    service.deliver_event(&other_actor_event).await.unwrap();
    let sends = provider.sends.lock().unwrap();
    assert_eq!(sends.len(), 2);
    assert_eq!(sends[1].0, "+4915119999999");
    assert_eq!(sends[1].1, "Private to the second actor.");
}

#[tokio::test]
async fn local_fake_rejects_unauthenticated_sender_before_publish() {
    let repository = std::sync::Arc::new(FakeRepository::default());
    let transport = FakeTransport::default();
    let service = RelayService::new(
        repository,
        std::sync::Arc::new(transport),
        std::sync::Arc::new(FakeProvider::default()),
    );
    let body = br#"{"sender_id":"+4915112345678","external_message_id":"x","text":"/status"}"#;
    assert!(matches!(
        service.handle_webhook(&HeaderMap::new(), body).await,
        Err(aegis_relay::service::RelayError::Provider(
            ProviderError::Authentication
        ))
    ));
}

#[tokio::test]
async fn used_pair_code_retry_is_deduplicated_and_never_published_as_a_command() {
    let code = "a".repeat(43);
    let installation_id = Uuid::new_v4();
    let repository = std::sync::Arc::new(FakeRepository::default());
    *repository.pairing.lock().unwrap() = Some((code.clone(), installation_id, "owner-1".into()));
    let transport = std::sync::Arc::new(FakeTransport::default());
    let provider = std::sync::Arc::new(FakeProvider::default());
    let service = RelayService::new(repository.clone(), transport.clone(), provider.clone());
    let mut headers = HeaderMap::new();
    headers.insert("x-fake-auth", HeaderValue::from_static("local-fixture"));
    let first = format!(
        "{{\"sender_id\":\"+4915112345678\",\"external_message_id\":\"pair-1\",\"text\":\"AEGIS {code}\"}}"
    );
    assert_eq!(
        service
            .handle_webhook(&headers, first.as_bytes())
            .await
            .unwrap(),
        InboundDisposition::Paired
    );
    assert_eq!(
        service
            .handle_webhook(&headers, first.as_bytes())
            .await
            .unwrap(),
        InboundDisposition::Ignored
    );
    let second = format!(
        "{{\"sender_id\":\"+4915112345678\",\"external_message_id\":\"pair-2\",\"text\":\"/pair {code}\"}}"
    );
    assert_eq!(
        service
            .handle_webhook(&headers, second.as_bytes())
            .await
            .unwrap(),
        InboundDisposition::Paired
    );
    assert!(transport.commands.lock().unwrap().is_empty());
    assert_eq!(provider.sends.lock().unwrap().len(), 2);
}

fn aegis_request(envelope: &CommandEnvelope) -> AegisCommandEnvelope {
    let command = match &envelope.command {
        AgentCommand::Message { text } => AegisCommand::Message {
            text: text.clone(),
            task_id: None,
        },
        AgentCommand::Status { task_id } => AegisCommand::Status {
            task_id: task_id.clone(),
        },
        unsupported => panic!("unexpected fixture command: {unsupported:?}"),
    };
    AegisCommandEnvelope {
        version: 1,
        installation_id: envelope.installation_id.to_string(),
        actor_id: envelope.actor_id.clone(),
        request_id: envelope.request_id.to_string(),
        issued_at: envelope.issued_at.timestamp(),
        expires_at: envelope.expires_at.timestamp(),
        command,
    }
}

#[tokio::test]
async fn relay_messages_use_aegis_local_authority_and_safe_events_return_to_the_paired_actor()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = tempfile::tempdir()?;
    let data_dir = workspace.path().join(".arun");
    std::fs::create_dir_all(&data_dir)?;

    let actor_id = "paired-phone";
    let mut store = Store::open(&data_dir)?;
    let selected_run = store.create_run(
        "Review the parser",
        workspace.path(),
        "codex",
        serde_json::json!(["workspace.read"]),
        serde_json::json!({}),
        "",
    )?;
    store.state(&selected_run.id, "running", serde_json::json!({}))?;
    let private_run = store.create_run(
        "Unshared task",
        workspace.path(),
        "codex",
        serde_json::json!(["workspace.read"]),
        serde_json::json!({}),
        "",
    )?;
    store.state(&private_run.id, "running", serde_json::json!({}))?;
    drop(store);

    let mut authority = Authority::open(&data_dir)?;
    let installation_id = Uuid::parse_str(authority.installation_id())?;
    authority.pair_actor(actor_id, std::slice::from_ref(&selected_run.id))?;
    authority.bind_selected_task(actor_id, &selected_run.id)?;

    let binding = ChannelBinding {
        id: Uuid::new_v4(),
        user_id: Uuid::new_v4(),
        installation_id,
        channel: "whatsapp".into(),
        actor_id: actor_id.into(),
        sender_id: "+4915112345678".into(),
        destination_id: "+4915112345678".into(),
    };
    let repository = std::sync::Arc::new(FakeRepository::default());
    *repository.binding.lock().unwrap() = Some(binding.clone());
    *repository.other_bindings.lock().unwrap() = vec![ChannelBinding {
        id: Uuid::new_v4(),
        user_id: binding.user_id,
        installation_id,
        channel: "whatsapp".into(),
        actor_id: "different-phone".into(),
        sender_id: "+4915119999999".into(),
        destination_id: "+4915119999999".into(),
    }];
    *repository.notifications.lock().unwrap() = true;
    let transport = std::sync::Arc::new(FakeTransport::default());
    let provider = std::sync::Arc::new(FakeProvider::default());
    let service = RelayService::new(repository, transport.clone(), provider.clone());
    let mut headers = HeaderMap::new();
    headers.insert("x-fake-auth", HeaderValue::from_static("local-fixture"));

    let inbound = serde_json::to_vec(&serde_json::json!({
        "sender_id":binding.sender_id.clone(),
        "external_message_id":"wamid.aegis-message-1",
        "text":"/message Continue the parser review"
    }))?;
    assert_eq!(
        service.handle_webhook(&headers, &inbound).await?,
        InboundDisposition::Published
    );
    let relayed = transport.commands.lock().unwrap()[0].clone();
    assert_eq!(relayed.installation_id, installation_id);
    assert_eq!(relayed.actor_id, actor_id);
    assert_eq!(
        relayed.command,
        AgentCommand::Message {
            text: "Continue the parser review".into()
        }
    );
    let local = aegis_request(&relayed);
    let receipt = authority.apply_from_authenticated_relay(
        actor_id,
        &local,
        relayed.issued_at.timestamp(),
    )?;
    assert!(!receipt.duplicate);
    assert_eq!(receipt.result["task_id"], selected_run.id);
    assert_eq!(
        authority.selected_task(actor_id)?.as_deref(),
        Some(selected_run.id.as_str())
    );

    let retry = authority.apply_from_authenticated_relay(
        actor_id,
        &local,
        relayed.issued_at.timestamp(),
    )?;
    assert!(retry.duplicate);
    let mut store = Store::open(&data_dir)?;
    assert_eq!(store.event_count(&selected_run.id, "user.steering")?, 1);

    let denied_inbound = serde_json::to_vec(&serde_json::json!({
        "sender_id":binding.sender_id.clone(),
        "external_message_id":"wamid.aegis-message-2",
        "text":format!("/status {}", private_run.id)
    }))?;
    assert_eq!(
        service.handle_webhook(&headers, &denied_inbound).await?,
        InboundDisposition::Published
    );
    let unauthorized = transport.commands.lock().unwrap()[1].clone();
    let error = authority
        .apply_from_authenticated_relay(
            actor_id,
            &aegis_request(&unauthorized),
            unauthorized.issued_at.timestamp(),
        )
        .unwrap_err();
    assert!(error.to_string().contains("not authorized for this task"));
    assert_eq!(store.event_count(&private_run.id, "user.steering")?, 0);
    assert!(
        authority
            .authorized_runs(actor_id)?
            .iter()
            .all(|task| task["task_id"].as_str() != Some(private_run.id.as_str()))
    );

    store.state(
        &selected_run.id,
        "completed",
        serde_json::json!({"summary":"model-authored fixture secret"}),
    )?;
    let local_completion = store
        .events(&selected_run.id)?
        .into_iter()
        .find(|event| event.kind == "run.completed")
        .expect("Aegis recorded the local completion state");
    assert_eq!(
        local_completion.payload["summary"],
        "model-authored fixture secret"
    );
    let completion = AegisEvent {
        event_id: Uuid::new_v4(),
        installation_id,
        actor_id: actor_id.into(),
        kind: RemoteEventKind::Completed,
        task_id: Some(selected_run.id.clone()),
        challenge_id: None,
        display_detail: None,
        reply_text: None,
    };
    service.deliver_event(&completion).await?;
    service.deliver_event(&completion).await?;

    let sends = provider.sends.lock().unwrap();
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].0, binding.destination_id);
    assert_eq!(
        sends[0].1,
        format!("Aegis task {} completed.", selected_run.id)
    );
    assert!(!sends[0].1.contains("model-authored fixture secret"));
    assert_eq!(sends[0].2, completion.event_id.to_string());
    Ok(())
}
