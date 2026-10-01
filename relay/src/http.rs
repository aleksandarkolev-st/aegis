use std::sync::Arc;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::IntoResponse,
    routing::{get, post},
};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::{
    channel::ChannelId,
    repository::PgRepository,
    service::{InboundDisposition, RelayError, RelayService},
};

#[derive(Clone)]
pub struct HttpState {
    pub service: Arc<RelayService>,
    pub repository: PgRepository,
    pub admin_token: Arc<String>,
}

pub fn router(state: HttpState) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/v1/webhooks/whatsapp/evolution", post(evolution_webhook))
        .route("/admin/v1/installations", post(provision_installation))
        .route(
            "/admin/v1/installations/{installation_id}/notifications/{event_kind}",
            post(set_whatsapp_notification_preference),
        )
        .route(
            "/admin/v1/installations/{installation_id}/channels/{channel}/notifications/{event_kind}",
            post(set_channel_notification_preference),
        )
        .route(
            "/admin/v1/installations/{installation_id}/bindings/revoke",
            post(revoke_channel_binding),
        )
        .route(
            "/admin/v1/installations/{installation_id}/channels/{channel}/bindings/revoke",
            post(revoke_scoped_channel_binding),
        )
        .route(
            "/admin/v1/installations/{installation_id}/actors/{actor_id}/bindings/revoke",
            post(revoke_actor_bindings),
        )
        .layer(DefaultBodyLimit::max(64 * 1024))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

async fn evolution_webhook(
    State(state): State<HttpState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    match state.service.handle_webhook(&headers, &body).await {
        Ok(
            InboundDisposition::Ignored
            | InboundDisposition::PairingPromptSent
            | InboundDisposition::Paired
            | InboundDisposition::Published,
        ) => (StatusCode::ACCEPTED, "accepted"),
        Err(RelayError::Authentication) => (StatusCode::UNAUTHORIZED, "unauthorized"),
        Err(RelayError::InvalidMessage) => (StatusCode::BAD_REQUEST, "invalid message"),
        Err(RelayError::InProgress) => (StatusCode::SERVICE_UNAVAILABLE, "retry later"),
        Err(RelayError::Channel(crate::channel::ChannelError::Authentication)) => {
            (StatusCode::UNAUTHORIZED, "unauthorized")
        }
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "temporarily unavailable"),
    }
}

async fn provision_installation(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(request): Json<ProvisionRequest>,
) -> impl IntoResponse {
    if !authorized(&headers, &state.admin_token) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error":"unauthorized"})),
        );
    }
    let installation_id = match request.installation_id.as_deref() {
        Some(value) => match canonical_uuid(value) {
            Some(value) => Some(value),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error":"installation_id must be a canonical UUID"})),
                );
            }
        },
        None => None,
    };
    match state
        .repository
        .provision_installation(&request.actor_id, installation_id)
        .await
    {
        Ok(provisioned) => (StatusCode::CREATED, Json(serde_json::json!(provisioned))),
        Err(crate::repository::RepositoryError::ActorCollision) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error":"actor id already belongs to another installation"})),
        ),
        Err(crate::repository::RepositoryError::InvalidActor) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"invalid actor id"})),
        ),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error":"temporarily unavailable"})),
        ),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProvisionRequest {
    actor_id: String,
    #[serde(default)]
    installation_id: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PreferenceUpdate {
    enabled: bool,
}

async fn set_whatsapp_notification_preference(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path((installation_id, event_kind)): Path<(Uuid, String)>,
    Json(update): Json<PreferenceUpdate>,
) -> impl IntoResponse {
    set_channel_preference(
        state,
        &headers,
        installation_id,
        "whatsapp",
        &event_kind,
        update.enabled,
    )
    .await
}

async fn set_channel_notification_preference(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path((installation_id, channel, event_kind)): Path<(Uuid, String, String)>,
    Json(update): Json<PreferenceUpdate>,
) -> impl IntoResponse {
    set_channel_preference(
        state,
        &headers,
        installation_id,
        &channel,
        &event_kind,
        update.enabled,
    )
    .await
}

async fn set_channel_preference(
    state: HttpState,
    headers: &HeaderMap,
    installation_id: Uuid,
    channel: &str,
    event_kind: &str,
    enabled: bool,
) -> (StatusCode, &'static str) {
    if !authorized(headers, &state.admin_token) {
        return (StatusCode::UNAUTHORIZED, "unauthorized");
    }
    if ChannelId::parse(channel).is_none() {
        return (StatusCode::BAD_REQUEST, "invalid channel");
    }
    if !matches!(
        event_kind,
        "task_started"
            | "progress"
            | "approval_required"
            | "blocked"
            | "completed"
            | "failed"
            | "reply"
    ) {
        return (StatusCode::BAD_REQUEST, "invalid event kind");
    }
    match state
        .repository
        .set_notification_preference(installation_id, channel, event_kind, enabled)
        .await
    {
        Ok(()) => (StatusCode::NO_CONTENT, ""),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "temporarily unavailable"),
    }
}

async fn revoke_channel_binding(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(installation_id): Path<Uuid>,
    Json(request): Json<RevokeBindingRequest>,
) -> impl IntoResponse {
    revoke_binding(
        state,
        &headers,
        installation_id,
        ChannelId::WhatsApp,
        request,
    )
    .await
}

async fn revoke_scoped_channel_binding(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path((installation_id, channel)): Path<(Uuid, String)>,
    Json(request): Json<RevokeBindingRequest>,
) -> impl IntoResponse {
    let Some(channel) = ChannelId::parse(&channel) else {
        return (StatusCode::BAD_REQUEST, "invalid channel");
    };
    revoke_binding(state, &headers, installation_id, channel, request).await
}

async fn revoke_actor_bindings(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path((installation_id, actor_id)): Path<(Uuid, String)>,
) -> impl IntoResponse {
    if !authorized(&headers, &state.admin_token) {
        return (StatusCode::UNAUTHORIZED, "unauthorized");
    }
    if !crate::repository::valid_actor_id(&actor_id) {
        return (StatusCode::BAD_REQUEST, "invalid actor id");
    }
    match state
        .repository
        .revoke_actor_bindings(installation_id, &actor_id)
        .await
    {
        Ok(_) => (StatusCode::NO_CONTENT, ""),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "temporarily unavailable"),
    }
}

async fn revoke_binding(
    state: HttpState,
    headers: &HeaderMap,
    installation_id: Uuid,
    channel: ChannelId,
    request: RevokeBindingRequest,
) -> (StatusCode, &'static str) {
    if !authorized(headers, &state.admin_token) {
        return (StatusCode::UNAUTHORIZED, "unauthorized");
    }
    if !crate::repository::valid_actor_id(&request.actor_id) {
        return (StatusCode::BAD_REQUEST, "invalid actor id");
    }
    let Some(sender_id) = normalize_channel_sender(channel, &request.sender_id) else {
        return (StatusCode::BAD_REQUEST, "invalid sender id");
    };
    match state
        .repository
        .revoke_channel_binding(
            installation_id,
            channel.as_str(),
            &request.actor_id,
            &sender_id,
        )
        .await
    {
        Ok(true) => (StatusCode::NO_CONTENT, ""),
        Ok(false) => (StatusCode::NOT_FOUND, "binding not found"),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "temporarily unavailable"),
    }
}

fn normalize_channel_sender(channel: ChannelId, sender_id: &str) -> Option<String> {
    match channel {
        ChannelId::WhatsApp => crate::provider::normalize_phone(sender_id),
        ChannelId::IMessage
            if !sender_id.is_empty()
                && sender_id.len() <= 200
                && sender_id.bytes().all(|byte| byte.is_ascii_graphic()) =>
        {
            Some(sender_id.to_owned())
        }
        ChannelId::IMessage => None,
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RevokeBindingRequest {
    actor_id: String,
    sender_id: String,
}

fn authorized(headers: &HeaderMap, expected: &str) -> bool {
    let Some(supplied) = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    else {
        return false;
    };
    supplied.len() == expected.len() && bool::from(supplied.as_bytes().ct_eq(expected.as_bytes()))
}

fn canonical_uuid(value: &str) -> Option<Uuid> {
    let parsed = Uuid::parse_str(value).ok()?;
    (parsed.hyphenated().to_string() == value).then_some(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installation_id_must_use_canonical_lowercase_hyphenated_format() {
        let uuid = Uuid::new_v4();
        let canonical = uuid.hyphenated().to_string();
        assert_eq!(canonical_uuid(&canonical), Some(uuid));
        assert!(canonical_uuid(&canonical.to_uppercase()).is_none());
        assert!(canonical_uuid(&uuid.simple().to_string()).is_none());
    }

    #[test]
    fn sender_identity_validation_is_channel_specific() {
        assert_eq!(
            normalize_channel_sender(ChannelId::WhatsApp, "4915112345678"),
            Some("+4915112345678".into())
        );
        assert_eq!(
            normalize_channel_sender(ChannelId::IMessage, "person@example.test"),
            Some("person@example.test".into())
        );
        assert!(normalize_channel_sender(ChannelId::IMessage, "bad identity\n").is_none());
    }
}
