use std::{
    env,
    sync::{Arc, Mutex},
};

use aegis_relay::{
    CommandEnvelope, InboundDisposition, RelayService,
    http::{HttpState, router},
    provider::{InboundWhatsAppMessage, ProviderError, WhatsAppProvider},
    repository::{PgRepository, RelayRepository},
    service::{CommandTransport, TransportError},
};
use async_trait::async_trait;
use axum::{body::Body, http::Request};
use serde::Deserialize;
use tower::ServiceExt;
use uuid::Uuid;

#[derive(Default)]
struct TestProvider {
    sends: Mutex<Vec<(String, String)>>,
}

#[async_trait]
impl WhatsAppProvider for TestProvider {
    fn parse_inbound(
        &self,
        headers: &axum::http::HeaderMap,
        raw_body: &[u8],
    ) -> Result<Option<InboundWhatsAppMessage>, ProviderError> {
        if headers
            .get("x-test-auth")
            .and_then(|value| value.to_str().ok())
            != Some("fixture")
        {
            return Err(ProviderError::Authentication);
        }
        #[derive(Deserialize)]
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
        _idempotency_key: &str,
    ) -> Result<Option<String>, ProviderError> {
        self.sends
            .lock()
            .unwrap()
            .push((destination.to_owned(), text.to_owned()));
        Ok(None)
    }
}

#[derive(Default)]
struct TestTransport {
    commands: Mutex<Vec<CommandEnvelope>>,
}

#[async_trait]
impl CommandTransport for TestTransport {
    async fn publish(&self, envelope: &CommandEnvelope) -> Result<(), TransportError> {
        self.commands.lock().unwrap().push(envelope.clone());
        Ok(())
    }
}

#[tokio::test]
async fn admin_revoke_is_scoped_and_blocks_commands_from_the_revoked_phone() {
    let Ok(database_url) = env::var("RELAY_TEST_DATABASE_URL") else {
        eprintln!("skipping admin revoke integration test: RELAY_TEST_DATABASE_URL is unset");
        return;
    };
    let repository = PgRepository::connect(&database_url).await.unwrap();
    let installation_id = Uuid::new_v4();
    let other_installation_id = Uuid::new_v4();
    let actor_id = format!("owner-{}", Uuid::new_v4().simple());
    let other_actor_id = format!("owner-{}", Uuid::new_v4().simple());
    let other_installation_actor_id = format!("owner-{}", Uuid::new_v4().simple());
    let sender_id = format!("+4915{}", Uuid::new_v4().as_u128() % 10_000_000_000);
    let other_sender_id = format!("+4916{}", Uuid::new_v4().as_u128() % 10_000_000_000);
    let remote_sender_id = format!("+4917{}", Uuid::new_v4().as_u128() % 10_000_000_000);

    let actor_provision = repository
        .provision_installation(&actor_id, Some(installation_id))
        .await
        .unwrap();
    let other_actor_provision = repository
        .provision_installation(&other_actor_id, Some(installation_id))
        .await
        .unwrap();
    let other_installation_provision = repository
        .provision_installation(&other_installation_actor_id, Some(other_installation_id))
        .await
        .unwrap();
    let binding = repository
        .redeem_pairing("whatsapp", &sender_id, &actor_provision.pairing_code)
        .await
        .unwrap()
        .unwrap();
    let other_actor_binding = repository
        .redeem_pairing(
            "whatsapp",
            &other_sender_id,
            &other_actor_provision.pairing_code,
        )
        .await
        .unwrap()
        .unwrap();
    let other_installation_binding = repository
        .redeem_pairing(
            "whatsapp",
            &remote_sender_id,
            &other_installation_provision.pairing_code,
        )
        .await
        .unwrap()
        .unwrap();
    let outstanding_pairing = repository
        .provision_installation(&actor_id, Some(installation_id))
        .await
        .unwrap();

    let provider = Arc::new(TestProvider::default());
    let transport = Arc::new(TestTransport::default());
    let service = Arc::new(RelayService::new(
        Arc::new(repository.clone()),
        transport.clone(),
        provider.clone(),
    ));
    let admin_token = "test-admin-token-long-enough-for-route-tests".to_owned();
    let app = router(HttpState {
        service: service.clone(),
        repository: repository.clone(),
        admin_token: Arc::new(admin_token.clone()),
    });
    let body = serde_json::json!({
        "actor_id": actor_id,
        "sender_id": sender_id,
    })
    .to_string();

    let unauthorized = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/admin/v1/installations/{installation_id}/bindings/revoke"
                ))
                .header("content-type", "application/json")
                .body(Body::from(body.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), axum::http::StatusCode::UNAUTHORIZED);

    for (target_installation, target_actor) in [
        (other_installation_id, actor_id.as_str()),
        (installation_id, other_actor_id.as_str()),
    ] {
        let wrong_scope_body = serde_json::json!({
            "actor_id": target_actor,
            "sender_id": sender_id,
        })
        .to_string();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/admin/v1/installations/{target_installation}/bindings/revoke"
                    ))
                    .header("authorization", format!("Bearer {admin_token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(wrong_scope_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
    }

    for invalid_body in [
        serde_json::json!({ "actor_id": "invalid/actor", "sender_id": sender_id }),
        serde_json::json!({ "actor_id": actor_id, "sender_id": "not-a-phone" }),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/admin/v1/installations/{installation_id}/bindings/revoke"
                    ))
                    .header("authorization", format!("Bearer {admin_token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(invalid_body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    assert!(
        repository
            .lookup_binding("whatsapp", &sender_id)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        repository
            .lookup_binding("whatsapp", &other_sender_id)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        repository
            .lookup_binding("whatsapp", &remote_sender_id)
            .await
            .unwrap()
            .is_some()
    );

    let revoked = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/admin/v1/installations/{installation_id}/bindings/revoke"
                ))
                .header("authorization", format!("Bearer {admin_token}"))
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(revoked.status(), axum::http::StatusCode::NO_CONTENT);
    assert!(
        repository
            .lookup_binding("whatsapp", &sender_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        repository
            .lookup_binding("whatsapp", &other_sender_id)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        repository
            .lookup_binding("whatsapp", &remote_sender_id)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        repository
            .redeem_pairing("whatsapp", &sender_id, &outstanding_pairing.pairing_code)
            .await
            .unwrap()
            .is_none()
    );

    let mut headers = axum::http::HeaderMap::new();
    headers.insert("x-test-auth", "fixture".parse().unwrap());
    let message = serde_json::json!({
        "sender_id": sender_id,
        "external_message_id": format!("revoked-{}", Uuid::new_v4()),
        "text": "/status",
    })
    .to_string();
    assert_eq!(
        service
            .handle_webhook(&headers, message.as_bytes())
            .await
            .unwrap(),
        InboundDisposition::PairingPromptSent
    );
    assert!(transport.commands.lock().unwrap().is_empty());
    assert_eq!(
        provider.sends.lock().unwrap().as_slice(),
        &[(
            sender_id.clone(),
            "Pair this number with Aegis using /pair <one-time-code>.".into()
        )]
    );

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(actor_provision.user_id)
        .execute(repository.pool())
        .await
        .unwrap();
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(other_installation_provision.user_id)
        .execute(repository.pool())
        .await
        .unwrap();
    let _ = (
        binding.id,
        other_actor_binding.id,
        other_installation_binding.id,
    );
}
