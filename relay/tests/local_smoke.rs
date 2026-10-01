use std::{
    collections::HashSet,
    process::{Child, Command, Stdio},
    sync::Mutex,
    time::Duration,
};

use aegis_relay::{
    AegisEvent, AgentCommand, CommandEnvelope, InboundDisposition, RelayService, RemoteEventKind,
    channel::{ChannelCapabilities, ChannelError, ChannelId, ChannelMessage, MessagingChannel},
    http::{HttpState, router},
    repository::{
        ChannelBinding, DeliveryTarget, InboundReceiptClaim, PgRepository, RelayRepository,
        RepositoryError,
    },
    service::{CommandTransport, TransportError},
};
use arun::remote::{
    Authority, Command as AegisCommand, CommandEnvelope as AegisCommandEnvelope,
    DispatchAuthorization, config::Config as RemoteConfig, ensure_remote_approval_gate,
};
use arun::storage::Store;
use async_trait::async_trait;
use axum::{
    http::{HeaderMap, HeaderValue},
    serve,
};
use uuid::Uuid;

#[derive(Default)]
struct FakeProvider {
    sends: Mutex<Vec<(String, String, String)>>,
    ambiguous_first_send: Mutex<bool>,
}

#[derive(Default)]
struct MockIMessageChannel {
    sends: Mutex<Vec<(String, String)>>,
}

#[async_trait]
impl MessagingChannel for MockIMessageChannel {
    fn id(&self) -> ChannelId {
        ChannelId::IMessage
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            text: true,
            attachments: false,
            idempotency_keys: false,
        }
    }

    fn parse_inbound(
        &self,
        _headers: &HeaderMap,
        _raw_body: &[u8],
    ) -> Result<Option<ChannelMessage>, ChannelError> {
        Ok(None)
    }

    async fn send_text(
        &self,
        destination: &str,
        text: &str,
        _idempotency_key: &str,
    ) -> Result<Option<String>, ChannelError> {
        self.sends
            .lock()
            .unwrap()
            .push((destination.to_owned(), text.to_owned()));
        Ok(None)
    }
}

#[async_trait]
impl MessagingChannel for FakeProvider {
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

    fn parse_inbound(
        &self,
        headers: &HeaderMap,
        raw_body: &[u8],
    ) -> Result<Option<ChannelMessage>, ChannelError> {
        if headers
            .get("x-fake-auth")
            .and_then(|value| value.to_str().ok())
            != Some("local-fixture")
        {
            return Err(ChannelError::Authentication);
        }
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fixture {
            sender_id: String,
            external_message_id: String,
            text: String,
        }
        let fixture: Fixture =
            serde_json::from_slice(raw_body).map_err(|_| ChannelError::InvalidRequest)?;
        Ok(Some(ChannelMessage {
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
    ) -> Result<Option<String>, ChannelError> {
        self.sends.lock().unwrap().push((
            destination.to_owned(),
            text.to_owned(),
            idempotency_key.to_owned(),
        ));
        if std::mem::take(&mut *self.ambiguous_first_send.lock().unwrap()) {
            // Model the provider accepting the send while the response is lost.
            return Err(ChannelError::Delivery);
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
        expires_at: None,
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

#[tokio::test]
async fn generic_core_routes_state_notifications_through_a_mock_channel_contract() {
    let installation_id = Uuid::new_v4();
    let binding = ChannelBinding {
        id: Uuid::new_v4(),
        user_id: Uuid::new_v4(),
        installation_id,
        channel: "imessage".into(),
        actor_id: "owner-1".into(),
        sender_id: "person@example.test".into(),
        destination_id: "person@example.test".into(),
    };
    let repository = std::sync::Arc::new(FakeRepository::default());
    *repository.binding.lock().unwrap() = Some(binding);
    *repository.notifications.lock().unwrap() = true;
    let channel = std::sync::Arc::new(MockIMessageChannel::default());
    let service = RelayService::new(
        repository,
        std::sync::Arc::new(FakeTransport::default()),
        channel.clone(),
    );
    let event = AegisEvent {
        event_id: Uuid::new_v4(),
        installation_id,
        actor_id: "owner-1".into(),
        kind: RemoteEventKind::Completed,
        task_id: Some("task-8".into()),
        challenge_id: None,
        expires_at: None,
        display_detail: None,
        reply_text: None,
    };

    service.deliver_event(&event).await.unwrap();

    assert_eq!(
        channel.sends.lock().unwrap().as_slice(),
        &[(
            "person@example.test".into(),
            "Aegis task task-8 completed.".into()
        )]
    );
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
        channel: &str,
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
                binding.installation_id == installation_id
                    && binding.actor_id == actor_id
                    && binding.channel == channel
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
        _channel: &str,
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
        _channel: &str,
        event_id: Uuid,
        binding_id: Uuid,
    ) -> Result<(), RepositoryError> {
        self.attempts.lock().unwrap().push((event_id, binding_id));
        Ok(())
    }

    async fn mark_delivered(
        &self,
        _channel: &str,
        event_id: Uuid,
        binding_id: Uuid,
        _provider_message_id: Option<&str>,
    ) -> Result<(), RepositoryError> {
        self.receipts.lock().unwrap().insert((event_id, binding_id));
        Ok(())
    }

    async fn mark_delivery_failed(
        &self,
        _channel: &str,
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
        expires_at: None,
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
        expires_at: None,
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
        Err(aegis_relay::service::RelayError::Channel(
            ChannelError::Authentication
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
        AgentCommand::ApproveOnce { challenge_id } => AegisCommand::ApproveOnce {
            challenge_id: challenge_id.clone(),
        },
        AgentCommand::Deny { challenge_id } => AegisCommand::Deny {
            challenge_id: challenge_id.clone(),
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

    let now = chrono::Utc::now().timestamp();
    let approved_operation = store.begin_operation(
        &selected_run.id,
        "workspace.write",
        serde_json::json!({"path":"src/approved.txt"}),
        true,
    )?;
    let approved_challenge =
        ensure_remote_approval_gate(&mut store, &selected_run.id, &approved_operation.id, now)?;
    let approval_source = store
        .events(&selected_run.id)?
        .into_iter()
        .find(|event| {
            event.kind == "approval.required"
                && event.payload["challenge_id"] == approved_challenge.challenge_id
        })
        .expect("local approval gate records its challenge event");
    let approval_notification = AegisEvent {
        event_id: Uuid::new_v4(),
        installation_id,
        actor_id: actor_id.into(),
        kind: RemoteEventKind::ApprovalRequired,
        task_id: Some(selected_run.id.clone()),
        challenge_id: Some(approved_challenge.challenge_id.clone()),
        expires_at: Some(approval_source.payload["expires_at"].as_i64().unwrap()),
        display_detail: Some(
            approval_source.payload["descriptor"]
                .as_str()
                .unwrap()
                .into(),
        ),
        reply_text: None,
    };
    service.deliver_event(&approval_notification).await?;
    {
        let sends = provider.sends.lock().unwrap();
        assert_eq!(sends.len(), 1);
        assert!(sends[0].1.contains(&format!(
            "/approve_once {}",
            approved_challenge.challenge_id
        )));
        assert!(sends[0].1.contains("expires at"));
        assert!(!sends[0].1.contains("model-authored fixture secret"));
    }
    provider.sends.lock().unwrap().clear();

    let approve_inbound = serde_json::to_vec(&serde_json::json!({
        "sender_id":binding.sender_id.clone(),
        "external_message_id":"wamid.approve-exact-challenge",
        "text":format!("/approve_once {}", approved_challenge.challenge_id)
    }))?;
    assert_eq!(
        service.handle_webhook(&headers, &approve_inbound).await?,
        InboundDisposition::Published
    );
    let approve_envelope = transport.commands.lock().unwrap()[2].clone();
    assert_eq!(
        approve_envelope.command,
        AgentCommand::ApproveOnce {
            challenge_id: approved_challenge.challenge_id.clone()
        }
    );
    let approved = authority.apply_from_authenticated_relay(
        actor_id,
        &aegis_request(&approve_envelope),
        approve_envelope.issued_at.timestamp(),
    )?;
    assert_eq!(approved.result["decision"], "approved_once");
    assert_eq!(
        authority.authorize_operation_dispatch(&selected_run.id, &approved_operation.id)?,
        DispatchAuthorization::ApprovedOnce
    );

    let denied_operation = store.begin_operation(
        &selected_run.id,
        "workspace.write",
        serde_json::json!({"path":"src/denied.txt"}),
        true,
    )?;
    let denied_challenge = ensure_remote_approval_gate(
        &mut store,
        &selected_run.id,
        &denied_operation.id,
        chrono::Utc::now().timestamp(),
    )?;
    let deny_inbound = serde_json::to_vec(&serde_json::json!({
        "sender_id":binding.sender_id.clone(),
        "external_message_id":"wamid.deny-exact-challenge",
        "text":format!("/deny {}", denied_challenge.challenge_id)
    }))?;
    assert_eq!(
        service.handle_webhook(&headers, &deny_inbound).await?,
        InboundDisposition::Published
    );
    let deny_envelope = transport.commands.lock().unwrap()[3].clone();
    assert_eq!(
        deny_envelope.command,
        AgentCommand::Deny {
            challenge_id: denied_challenge.challenge_id.clone()
        }
    );
    let denied = authority.apply_from_authenticated_relay(
        actor_id,
        &aegis_request(&deny_envelope),
        deny_envelope.issued_at.timestamp(),
    )?;
    assert_eq!(denied.result["decision"], "denied");
    assert_eq!(
        authority.authorize_operation_dispatch(&selected_run.id, &denied_operation.id)?,
        DispatchAuthorization::Denied
    );

    let expired_operation = store.begin_operation(
        &selected_run.id,
        "workspace.write",
        serde_json::json!({"path":"src/expired.txt"}),
        true,
    )?;
    let expired_challenge = ensure_remote_approval_gate(
        &mut store,
        &selected_run.id,
        &expired_operation.id,
        now.saturating_sub(600),
    )?;
    let expired_source = store
        .events(&selected_run.id)?
        .into_iter()
        .find(|event| {
            event.kind == "approval.required"
                && event.payload["challenge_id"] == expired_challenge.challenge_id
        })
        .expect("expired local challenge retains its event for relay validation");
    let expired_notification = AegisEvent {
        event_id: Uuid::new_v4(),
        installation_id,
        actor_id: actor_id.into(),
        kind: RemoteEventKind::ApprovalRequired,
        task_id: Some(selected_run.id.clone()),
        challenge_id: Some(expired_challenge.challenge_id.clone()),
        expires_at: Some(expired_source.payload["expires_at"].as_i64().unwrap()),
        display_detail: Some(
            expired_source.payload["descriptor"]
                .as_str()
                .unwrap()
                .into(),
        ),
        reply_text: None,
    };
    service.deliver_event(&expired_notification).await?;
    assert!(provider.sends.lock().unwrap().is_empty());
    let expired_inbound = serde_json::to_vec(&serde_json::json!({
        "sender_id":binding.sender_id.clone(),
        "external_message_id":"wamid.expired-challenge",
        "text":format!("/approve_once {}", expired_challenge.challenge_id)
    }))?;
    assert_eq!(
        service.handle_webhook(&headers, &expired_inbound).await?,
        InboundDisposition::Published
    );
    let expired_envelope = transport.commands.lock().unwrap()[4].clone();
    let expired = authority
        .apply_from_authenticated_relay(
            actor_id,
            &aegis_request(&expired_envelope),
            expired_envelope.issued_at.timestamp(),
        )
        .unwrap_err();
    assert!(expired.to_string().contains("expired"));

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
        expires_at: None,
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

struct RemoteDaemonChild(Child);

impl RemoteDaemonChild {
    fn stop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for RemoteDaemonChild {
    fn drop(&mut self) {
        self.stop();
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL, local TLS JetStream, and AEGIS_E2E_BINARY; run relay/scripts/e2e-smoke.ps1"]
async fn jetstream_remote_command_and_event_round_trip_uses_local_authority()
-> Result<(), Box<dyn std::error::Error>> {
    let database_url = std::env::var("RELAY_E2E_DATABASE_URL")?;
    let nats_url = std::env::var("RELAY_E2E_NATS_URL")?;
    let nats_token = std::env::var("RELAY_E2E_NATS_TOKEN")?;
    let root_certificate = std::path::PathBuf::from(std::env::var("RELAY_E2E_NATS_ROOT_CERT")?);
    let aegis_binary = std::path::PathBuf::from(std::env::var("AEGIS_E2E_BINARY")?);
    if !aegis_binary.is_file() {
        return Err(std::io::Error::other("AEGIS_E2E_BINARY does not name a file").into());
    }

    let transport = std::sync::Arc::new(
        aegis_relay::transport::JetStreamTransport::connect(
            &nats_url,
            &nats_token,
            Some(&root_certificate),
        )
        .await?,
    );
    let repository = PgRepository::connect(&database_url).await?;
    let workspace = tempfile::tempdir()?;
    let data_dir = workspace.path().join(".arun");
    std::fs::create_dir_all(&data_dir)?;
    let sender_id = format!(
        "+4915{}",
        Uuid::new_v4().simple().to_string()[..10].to_owned()
    );
    let mut store = Store::open(&data_dir)?;
    let selected_run = store.create_run(
        "Review the parser through JetStream",
        workspace.path(),
        "codex",
        serde_json::json!(["workspace.read"]),
        serde_json::json!({}),
        "",
    )?;
    store.state(&selected_run.id, "completed", serde_json::json!({}))?;
    drop(store);

    let authority = Authority::open(&data_dir)?;
    let installation_id = Uuid::parse_str(authority.installation_id())?;
    drop(authority);

    let provider = std::sync::Arc::new(FakeProvider::default());
    let service = std::sync::Arc::new(RelayService::new(
        std::sync::Arc::new(repository.clone()),
        transport.clone(),
        provider.clone(),
    ));
    let admin_token = "local-e2e-admin-token";
    let http_router = router(HttpState {
        service: service.clone(),
        repository: repository.clone(),
        admin_token: std::sync::Arc::new(admin_token.to_owned()),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let (server_shutdown_tx, server_shutdown_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        serve(listener, http_router)
            .with_graceful_shutdown(async move {
                let _ = server_shutdown_rx.await;
            })
            .await
    });
    let (events_shutdown_tx, events_shutdown_rx) = tokio::sync::oneshot::channel();
    let event_consumer = tokio::spawn(transport.clone().supervise_events(
        service.clone(),
        async move {
            let _ = events_shutdown_rx.await;
        },
    ));

    let base_url = format!("http://{address}");
    let http = reqwest::Client::new();
    let pair_output = Command::new(&aegis_binary)
        .args([
            "remote",
            "pair",
            "--relay-admin-url",
            &base_url,
            "--admin-token-env",
            "AEGIS_E2E_RELAY_ADMIN_TOKEN",
            "--nats-url",
            &nats_url,
            "--nats-token-env",
            "AEGIS_E2E_NATS_TOKEN",
            "--nats-root-cert",
            root_certificate
                .to_str()
                .ok_or_else(|| std::io::Error::other("NATS certificate path is not valid UTF-8"))?,
        ])
        .current_dir(workspace.path())
        .env("AEGIS_E2E_RELAY_ADMIN_TOKEN", admin_token)
        .env("AEGIS_E2E_NATS_TOKEN", &nats_token)
        .output()?;
    if !pair_output.status.success() {
        return Err(std::io::Error::other(format!(
            "Aegis remote pair failed: {}",
            String::from_utf8_lossy(&pair_output.stderr)
        ))
        .into());
    }
    let pair_output = String::from_utf8(pair_output.stdout)?;
    let pairing_code = pair_output
        .split_whitespace()
        .find(|value| {
            value.len() == 43
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        })
        .ok_or_else(|| std::io::Error::other("Aegis remote pair omitted its one-time code"))?
        .to_owned();
    let remote_config = RemoteConfig::load(&data_dir)?;
    assert_eq!(remote_config.installation_id, installation_id.to_string());
    let actor_id = remote_config.actor_id;
    let mut authority = Authority::open(&data_dir)?;
    authority.pair_actor(&actor_id, std::slice::from_ref(&selected_run.id))?;
    authority.bind_selected_task(&actor_id, &selected_run.id)?;
    drop(authority);
    let user_id = sqlx::query_scalar::<_, Uuid>("SELECT user_id FROM installations WHERE id = $1")
        .bind(installation_id)
        .fetch_one(repository.pool())
        .await?;

    let pair_response = http
        .post(format!("{base_url}/v1/webhooks/whatsapp/evolution"))
        .header("x-fake-auth", "local-fixture")
        .json(&serde_json::json!({
            "sender_id": sender_id,
            "external_message_id": format!("pair-{}", Uuid::new_v4()),
            "text": format!("AEGIS {pairing_code}")
        }))
        .send()
        .await?;
    assert_eq!(pair_response.status(), axum::http::StatusCode::ACCEPTED);

    let mut daemon = RemoteDaemonChild(
        Command::new(aegis_binary)
            .args(["remote", "run"])
            .current_dir(workspace.path())
            .env("AEGIS_E2E_RELAY_ADMIN_TOKEN", admin_token)
            .env("AEGIS_E2E_NATS_TOKEN", &nats_token)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
    );

    let status_response = http
        .post(format!("{base_url}/v1/webhooks/whatsapp/evolution"))
        .header("x-fake-auth", "local-fixture")
        .json(&serde_json::json!({
            "sender_id": sender_id,
            "external_message_id": format!("status-{}", Uuid::new_v4()),
            "text": "/status"
        }))
        .send()
        .await?;
    assert_eq!(status_response.status(), axum::http::StatusCode::ACCEPTED);

    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(status) = daemon.0.try_wait()? {
                return Err(std::io::Error::other(format!(
                    "Aegis remote daemon exited before replying ({status})"
                )));
            }
            if provider.sends.lock().unwrap().len() >= 2 {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await??;

    {
        let sends = provider.sends.lock().unwrap();
        assert_eq!(sends.len(), 2);
        assert_eq!(sends[0].0, sender_id);
        assert_eq!(sends[0].1, "This account is paired with Aegis.");
        assert_eq!(sends[1].0, sender_id);
        assert!(sends[1].1.contains("[completed]"));
        assert!(sends[1].1.contains("Review the parser through JetStream"));
    }

    daemon.stop();
    let _ = events_shutdown_tx.send(());
    event_consumer.await?;
    let _ = server_shutdown_tx.send(());
    server.await??;

    let cleanup_client = async_nats::ConnectOptions::new()
        .token(nats_token)
        .require_tls(true)
        .add_root_certificates(root_certificate)
        .connect(nats_url)
        .await?;
    let context = async_nats::jetstream::new(cleanup_client);
    context
        .get_stream("AEGIS_COMMANDS")
        .await?
        .delete_consumer(&format!("aegis-{installation_id}"))
        .await?;
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(repository.pool())
        .await?;
    Ok(())
}
