use std::time::Duration;

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use rand::{RngCore, rngs::OsRng};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, postgres::PgPoolOptions};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelBinding {
    pub id: Uuid,
    pub user_id: Uuid,
    pub installation_id: Uuid,
    pub channel: String,
    pub actor_id: String,
    pub sender_id: String,
    pub destination_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryTarget {
    pub binding_id: Uuid,
    pub destination_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboundReceiptClaim {
    Claimed,
    Duplicate,
    InProgress,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProvisionedInstallation {
    pub user_id: Uuid,
    pub installation_id: Uuid,
    pub actor_id: String,
    /// Returned once to the administrator; only the SHA-256 digest is persisted.
    pub pairing_code: String,
    pub pairing_expires_at: DateTime<Utc>,
}

#[derive(Debug, Error)]
pub enum RepositoryError {
    #[error("database operation failed")]
    Database(#[from] sqlx::Error),
    #[error("metadata schema initialization failed")]
    Migration(#[from] std::io::Error),
    #[error("actor id is already registered to another installation")]
    ActorCollision,
    #[error("actor id is invalid")]
    InvalidActor,
}

#[async_trait]
pub trait RelayRepository: Send + Sync {
    async fn claim_inbound_receipt(
        &self,
        channel: &str,
        sender_id: &str,
        external_message_id: &str,
    ) -> Result<InboundReceiptClaim, RepositoryError>;

    async fn complete_inbound_receipt(
        &self,
        channel: &str,
        sender_id: &str,
        external_message_id: &str,
        disposition: &str,
        installation_id: Option<Uuid>,
        binding_id: Option<Uuid>,
    ) -> Result<(), RepositoryError>;

    async fn fail_inbound_receipt(
        &self,
        channel: &str,
        sender_id: &str,
        external_message_id: &str,
    ) -> Result<(), RepositoryError>;

    async fn prune_expired_receipts(&self) -> Result<u64, RepositoryError>;

    async fn lookup_binding(
        &self,
        channel: &str,
        sender_id: &str,
    ) -> Result<Option<ChannelBinding>, RepositoryError>;

    async fn redeem_pairing(
        &self,
        channel: &str,
        sender_id: &str,
        code: &str,
    ) -> Result<Option<ChannelBinding>, RepositoryError>;

    async fn targets_for_installation(
        &self,
        installation_id: Uuid,
        actor_id: &str,
        channel: &str,
    ) -> Result<Vec<DeliveryTarget>, RepositoryError>;

    async fn notifications_enabled(
        &self,
        installation_id: Uuid,
        channel: &str,
        event_kind: &str,
    ) -> Result<bool, RepositoryError>;

    async fn was_delivered(
        &self,
        channel: &str,
        event_id: Uuid,
        binding_id: Uuid,
    ) -> Result<bool, RepositoryError>;

    async fn record_delivery_attempt(
        &self,
        channel: &str,
        event_id: Uuid,
        binding_id: Uuid,
    ) -> Result<(), RepositoryError>;

    async fn mark_delivered(
        &self,
        channel: &str,
        event_id: Uuid,
        binding_id: Uuid,
        provider_message_id: Option<&str>,
    ) -> Result<(), RepositoryError>;

    async fn mark_delivery_failed(
        &self,
        channel: &str,
        event_id: Uuid,
        binding_id: Uuid,
    ) -> Result<(), RepositoryError>;
}

#[derive(Clone)]
pub struct PgRepository {
    pool: PgPool,
}

impl PgRepository {
    pub async fn connect(database_url: &str) -> Result<Self, RepositoryError> {
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .acquire_timeout(Duration::from_secs(5))
            .connect(database_url)
            .await?;
        let repository = Self { pool };
        repository.initialize_schema().await?;
        Ok(repository)
    }

    async fn initialize_schema(&self) -> Result<(), RepositoryError> {
        let ddl = include_str!("../migrations/0001_metadata.sql");
        for statement in ddl.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            sqlx::query(statement).execute(&self.pool).await?;
        }
        let channel_constraints = [
            ("channel_bindings", "channel_bindings_channel_check"),
            ("delivery_receipts", "delivery_receipts_channel_check"),
            (
                "notification_preferences",
                "notification_preferences_channel_check",
            ),
        ];
        for (table, constraint) in channel_constraints {
            let definition = sqlx::query_scalar::<_, String>(
                "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = $1::regclass AND conname = $2",
            )
            .bind(table)
            .bind(constraint)
            .fetch_optional(&self.pool)
            .await?;
            if !definition
                .as_deref()
                .is_some_and(|definition| definition.contains("imessage"))
            {
                let migration = include_str!("../migrations/0002_channel_identity.sql");
                for statement in migration
                    .split(';')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    sqlx::query(statement).execute(&self.pool).await?;
                }
                break;
            }
        }
        Ok(())
    }

    pub async fn provision_installation(
        &self,
        actor_id: &str,
        requested_installation_id: Option<Uuid>,
    ) -> Result<ProvisionedInstallation, RepositoryError> {
        if !valid_actor_id(actor_id) {
            return Err(RepositoryError::InvalidActor);
        }
        let installation_id = requested_installation_id.unwrap_or_else(Uuid::new_v4);
        let mut tx = self.pool.begin().await?;
        let existing = sqlx::query_scalar::<_, Uuid>(
            "SELECT user_id FROM installations WHERE id = $1 FOR UPDATE",
        )
        .bind(installation_id)
        .fetch_optional(&mut *tx)
        .await?;
        let user_id = if let Some(user_id) = existing {
            user_id
        } else {
            let new_user_id = Uuid::new_v4();
            sqlx::query("INSERT INTO users (id) VALUES ($1)")
                .bind(new_user_id)
                .execute(&mut *tx)
                .await?;
            let inserted = sqlx::query_scalar::<_, Uuid>(
                "INSERT INTO installations (id, user_id) VALUES ($1, $2) ON CONFLICT (id) DO NOTHING RETURNING user_id",
            )
            .bind(installation_id)
            .bind(new_user_id)
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(existing_user_id) = inserted {
                existing_user_id
            } else {
                let existing_user_id = sqlx::query_scalar::<_, Uuid>(
                    "SELECT user_id FROM installations WHERE id = $1 FOR UPDATE",
                )
                .bind(installation_id)
                .fetch_optional(&mut *tx)
                .await?;
                let Some(existing_user_id) = existing_user_id else {
                    tx.rollback().await?;
                    return Err(RepositoryError::Database(sqlx::Error::RowNotFound));
                };
                sqlx::query("DELETE FROM users WHERE id = $1")
                    .bind(new_user_id)
                    .execute(&mut *tx)
                    .await?;
                existing_user_id
            }
        };
        let mut token_bytes = [0u8; 32];
        OsRng.fill_bytes(&mut token_bytes);
        let pairing_code = URL_SAFE_NO_PAD.encode(token_bytes);
        let token_hash = Sha256::digest(pairing_code.as_bytes()).to_vec();
        let pairing_expires_at = Utc::now() + chrono::Duration::minutes(5);
        let pairing_token_id = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO pairing_tokens (id, user_id, installation_id, actor_id, token_hash, expires_at) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (actor_id) DO UPDATE SET token_hash = EXCLUDED.token_hash, expires_at = EXCLUDED.expires_at, used_at = NULL, created_at = now() WHERE pairing_tokens.installation_id = EXCLUDED.installation_id AND pairing_tokens.user_id = EXCLUDED.user_id RETURNING id",
        )
        .bind(Uuid::new_v4())
        .bind(user_id)
        .bind(installation_id)
        .bind(actor_id)
        .bind(token_hash)
        .bind(pairing_expires_at)
        .fetch_optional(&mut *tx)
        .await?;
        if pairing_token_id.is_none() {
            tx.rollback().await?;
            return Err(RepositoryError::ActorCollision);
        }
        tx.commit().await?;
        Ok(ProvisionedInstallation {
            user_id,
            installation_id,
            actor_id: actor_id.to_owned(),
            pairing_code,
            pairing_expires_at,
        })
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn set_notification_preference(
        &self,
        installation_id: Uuid,
        channel: &str,
        event_kind: &str,
        enabled: bool,
    ) -> Result<(), RepositoryError> {
        sqlx::query(
            "INSERT INTO notification_preferences (installation_id, channel, event_kind, enabled) VALUES ($1, $2, $3, $4) ON CONFLICT (installation_id, channel, event_kind) DO UPDATE SET enabled = EXCLUDED.enabled, updated_at = now()",
        )
        .bind(installation_id)
        .bind(channel)
        .bind(event_kind)
        .bind(enabled)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn revoke_channel_binding(
        &self,
        installation_id: Uuid,
        channel: &str,
        actor_id: &str,
        sender_id: &str,
    ) -> Result<bool, RepositoryError> {
        let mut tx = self.pool.begin().await?;
        // Pair redemption locks its token before the binding. Keep that lock order
        // here so a simultaneous redemption and revocation cannot deadlock.
        sqlx::query(
            "UPDATE pairing_tokens SET used_at = now() WHERE installation_id = $1 AND actor_id = $2 AND used_at IS NULL AND EXISTS (SELECT 1 FROM channel_bindings WHERE installation_id = $1 AND actor_id = $2 AND channel = $3 AND sender_id = $4)",
        )
        .bind(installation_id)
        .bind(actor_id)
        .bind(channel)
        .bind(sender_id)
        .execute(&mut *tx)
        .await?;
        let binding_id = sqlx::query_scalar::<_, Uuid>(
            "UPDATE channel_bindings SET active = FALSE WHERE installation_id = $1 AND actor_id = $2 AND channel = $3 AND sender_id = $4 RETURNING id",
        )
        .bind(installation_id)
        .bind(actor_id)
        .bind(channel)
        .bind(sender_id)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(binding_id.is_some())
    }

    pub async fn revoke_actor_bindings(
        &self,
        installation_id: Uuid,
        actor_id: &str,
    ) -> Result<u64, RepositoryError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "UPDATE pairing_tokens SET used_at = now() WHERE installation_id = $1 AND actor_id = $2 AND used_at IS NULL",
        )
        .bind(installation_id)
        .bind(actor_id)
        .execute(&mut *tx)
        .await?;
        let revoked = sqlx::query(
            "UPDATE channel_bindings SET active = FALSE WHERE installation_id = $1 AND actor_id = $2 AND active = TRUE",
        )
        .bind(installation_id)
        .bind(actor_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        Ok(revoked)
    }
}

pub fn valid_actor_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn default_notification_enabled(event_kind: &str) -> bool {
    matches!(
        event_kind,
        "task_started" | "approval_required" | "blocked" | "completed" | "failed" | "reply"
    )
}

#[cfg(test)]
mod tests {
    use super::default_notification_enabled;

    #[test]
    fn notification_defaults_keep_state_changes_and_suppress_routine_progress() {
        for event in [
            "task_started",
            "approval_required",
            "blocked",
            "completed",
            "failed",
            "reply",
        ] {
            assert!(
                default_notification_enabled(event),
                "{event} should notify by default"
            );
        }
        assert!(!default_notification_enabled("progress"));
        assert!(!default_notification_enabled("unknown_future_event"));
    }
}

#[async_trait]
impl RelayRepository for PgRepository {
    async fn claim_inbound_receipt(
        &self,
        channel: &str,
        sender_id: &str,
        external_message_id: &str,
    ) -> Result<InboundReceiptClaim, RepositoryError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "DELETE FROM delivery_receipts WHERE direction = 'inbound' AND channel = $1 AND sender_id = $2 AND external_message_id = $3 AND expires_at < now()",
        )
            .bind(channel)
            .bind(sender_id)
            .bind(external_message_id)
            .execute(&mut *tx)
            .await?;
        let receipt_id = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO delivery_receipts (receipt_id, direction, channel, sender_id, external_message_id, state, attempts, expires_at) VALUES ($1, 'inbound', $2, $3, $4, 'processing', 1, now() + interval '30 days') ON CONFLICT (channel, sender_id, external_message_id) WHERE direction = 'inbound' DO NOTHING RETURNING receipt_id",
        )
        .bind(Uuid::new_v4())
        .bind(channel)
        .bind(sender_id)
        .bind(external_message_id)
        .fetch_optional(&mut *tx)
        .await?;
        if receipt_id.is_some() {
            tx.commit().await?;
            return Ok(InboundReceiptClaim::Claimed);
        }

        let existing = sqlx::query_as::<_, (String, DateTime<Utc>)>(
            "SELECT state, updated_at FROM delivery_receipts WHERE direction = 'inbound' AND channel = $1 AND sender_id = $2 AND external_message_id = $3 FOR UPDATE",
        )
        .bind(channel)
        .bind(sender_id)
        .bind(external_message_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((state, updated_at)) = existing else {
            tx.rollback().await?;
            return Ok(InboundReceiptClaim::Duplicate);
        };
        if state == "completed" {
            tx.commit().await?;
            return Ok(InboundReceiptClaim::Duplicate);
        }
        if state == "processing" && updated_at > Utc::now() - chrono::Duration::seconds(30) {
            tx.commit().await?;
            return Ok(InboundReceiptClaim::InProgress);
        }
        sqlx::query(
            "UPDATE delivery_receipts SET state = 'processing', disposition = NULL, installation_id = NULL, binding_id = NULL, attempts = attempts + 1, expires_at = now() + interval '30 days', updated_at = now() WHERE direction = 'inbound' AND channel = $1 AND sender_id = $2 AND external_message_id = $3",
        )
        .bind(channel)
        .bind(sender_id)
        .bind(external_message_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(InboundReceiptClaim::Claimed)
    }

    async fn complete_inbound_receipt(
        &self,
        channel: &str,
        sender_id: &str,
        external_message_id: &str,
        disposition: &str,
        installation_id: Option<Uuid>,
        binding_id: Option<Uuid>,
    ) -> Result<(), RepositoryError> {
        sqlx::query(
            "UPDATE delivery_receipts SET state = 'completed', disposition = $4, installation_id = $5, binding_id = $6, updated_at = now() WHERE direction = 'inbound' AND channel = $1 AND sender_id = $2 AND external_message_id = $3",
        )
        .bind(channel)
        .bind(sender_id)
        .bind(external_message_id)
        .bind(disposition)
        .bind(installation_id)
        .bind(binding_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn fail_inbound_receipt(
        &self,
        channel: &str,
        sender_id: &str,
        external_message_id: &str,
    ) -> Result<(), RepositoryError> {
        sqlx::query(
            "UPDATE delivery_receipts SET state = 'failed', updated_at = now() WHERE direction = 'inbound' AND channel = $1 AND sender_id = $2 AND external_message_id = $3",
        )
        .bind(channel)
        .bind(sender_id)
        .bind(external_message_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn prune_expired_receipts(&self) -> Result<u64, RepositoryError> {
        let mut total = 0;
        loop {
            let result = sqlx::query(
                "WITH expired AS (SELECT receipt_id FROM delivery_receipts WHERE expires_at < now() ORDER BY expires_at LIMIT 1000 FOR UPDATE SKIP LOCKED) DELETE FROM delivery_receipts WHERE receipt_id IN (SELECT receipt_id FROM expired)",
            )
            .execute(&self.pool)
            .await?;
            total += result.rows_affected();
            if result.rows_affected() < 1000 {
                break;
            }
        }
        Ok(total)
    }

    async fn lookup_binding(
        &self,
        channel: &str,
        sender_id: &str,
    ) -> Result<Option<ChannelBinding>, RepositoryError> {
        let row = sqlx::query_as::<_, (Uuid, Uuid, Uuid, String, String, String, String)>(
            "SELECT id, user_id, installation_id, channel, actor_id, sender_id, destination_id FROM channel_bindings WHERE channel = $1 AND sender_id = $2 AND active = TRUE",
        )
        .bind(channel)
        .bind(sender_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(
            |(id, user_id, installation_id, channel, actor_id, sender_id, destination_id)| {
                ChannelBinding {
                    id,
                    user_id,
                    installation_id,
                    channel,
                    actor_id,
                    sender_id,
                    destination_id,
                }
            },
        ))
    }

    async fn redeem_pairing(
        &self,
        channel: &str,
        sender_id: &str,
        code: &str,
    ) -> Result<Option<ChannelBinding>, RepositoryError> {
        if code.len() != 43 || !code.is_ascii() {
            return Ok(None);
        }
        let token_hash = Sha256::digest(code.as_bytes()).to_vec();
        let mut tx = self.pool.begin().await?;
        let token = sqlx::query_as::<_, (Uuid, Uuid, Uuid, String)>(
            "SELECT id, user_id, installation_id, actor_id FROM pairing_tokens WHERE token_hash = $1 AND used_at IS NULL AND expires_at > now() FOR UPDATE",
        )
        .bind(&token_hash)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((token_id, user_id, installation_id, actor_id)) = token else {
            tx.rollback().await?;
            return Ok(None);
        };

        let existing = sqlx::query_as::<_, (Uuid, Uuid, Uuid, String, String)>(
            "SELECT id, user_id, installation_id, actor_id, destination_id FROM channel_bindings WHERE channel = $1 AND sender_id = $2 FOR UPDATE",
        )
        .bind(channel)
        .bind(sender_id)
        .fetch_optional(&mut *tx)
        .await?;

        let binding = if let Some((
            id,
            existing_user,
            existing_installation,
            existing_actor,
            destination_id,
        )) = existing
        {
            if existing_installation != installation_id
                || existing_user != user_id
                || existing_actor != actor_id
            {
                tx.rollback().await?;
                return Ok(None);
            }
            sqlx::query("UPDATE channel_bindings SET active = TRUE WHERE id = $1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            ChannelBinding {
                id,
                user_id,
                installation_id,
                channel: channel.to_owned(),
                actor_id,
                sender_id: sender_id.to_owned(),
                destination_id,
            }
        } else {
            let id = Uuid::new_v4();
            sqlx::query(
                "INSERT INTO channel_bindings (id, user_id, installation_id, channel, actor_id, sender_id, destination_id) VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(id)
            .bind(user_id)
            .bind(installation_id)
            .bind(channel)
            .bind(&actor_id)
            .bind(sender_id)
            .bind(sender_id)
            .execute(&mut *tx)
            .await?;
            ChannelBinding {
                id,
                user_id,
                installation_id,
                channel: channel.to_owned(),
                actor_id,
                sender_id: sender_id.to_owned(),
                destination_id: sender_id.to_owned(),
            }
        };

        sqlx::query("UPDATE pairing_tokens SET used_at = now() WHERE id = $1")
            .bind(token_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(Some(binding))
    }

    async fn targets_for_installation(
        &self,
        installation_id: Uuid,
        actor_id: &str,
        channel: &str,
    ) -> Result<Vec<DeliveryTarget>, RepositoryError> {
        let rows = sqlx::query_as::<_, (Uuid, String)>(
            "SELECT id, destination_id FROM channel_bindings WHERE installation_id = $1 AND actor_id = $2 AND active = TRUE AND channel = $3",
        )
        .bind(installation_id)
        .bind(actor_id)
        .bind(channel)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(binding_id, destination_id)| DeliveryTarget {
                binding_id,
                destination_id,
            })
            .collect())
    }

    async fn notifications_enabled(
        &self,
        installation_id: Uuid,
        channel: &str,
        event_kind: &str,
    ) -> Result<bool, RepositoryError> {
        let enabled = sqlx::query_scalar::<_, bool>(
            "SELECT enabled FROM notification_preferences WHERE installation_id = $1 AND channel = $2 AND event_kind = $3",
        )
        .bind(installation_id)
        .bind(channel)
        .bind(event_kind)
        .fetch_optional(&self.pool)
        .await?;
        Ok(enabled.unwrap_or_else(|| default_notification_enabled(event_kind)))
    }

    async fn was_delivered(
        &self,
        channel: &str,
        event_id: Uuid,
        binding_id: Uuid,
    ) -> Result<bool, RepositoryError> {
        let state = sqlx::query_scalar::<_, String>(
            "SELECT state FROM delivery_receipts WHERE direction = 'outbound' AND channel = $1 AND event_id = $2 AND binding_id = $3",
        )
        .bind(channel)
        .bind(event_id)
        .bind(binding_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(state.as_deref() == Some("delivered"))
    }

    async fn record_delivery_attempt(
        &self,
        channel: &str,
        event_id: Uuid,
        binding_id: Uuid,
    ) -> Result<(), RepositoryError> {
        sqlx::query(
            "INSERT INTO delivery_receipts (receipt_id, direction, event_id, binding_id, channel, state, attempts, expires_at) VALUES ($1, 'outbound', $2, $3, $4, 'pending', 1, now() + interval '90 days') ON CONFLICT (event_id, binding_id) WHERE direction = 'outbound' DO UPDATE SET channel = EXCLUDED.channel, state = 'pending', attempts = delivery_receipts.attempts + 1, expires_at = now() + interval '90 days', updated_at = now()",
        )
        .bind(Uuid::new_v4())
        .bind(event_id)
        .bind(binding_id)
        .bind(channel)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn mark_delivered(
        &self,
        channel: &str,
        event_id: Uuid,
        binding_id: Uuid,
        provider_message_id: Option<&str>,
    ) -> Result<(), RepositoryError> {
        sqlx::query(
            "UPDATE delivery_receipts SET state = 'delivered', provider_message_id = $4, updated_at = now() WHERE direction = 'outbound' AND channel = $1 AND event_id = $2 AND binding_id = $3",
        )
        .bind(channel)
        .bind(event_id)
        .bind(binding_id)
        .bind(provider_message_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn mark_delivery_failed(
        &self,
        channel: &str,
        event_id: Uuid,
        binding_id: Uuid,
    ) -> Result<(), RepositoryError> {
        sqlx::query(
            "UPDATE delivery_receipts SET state = 'failed', updated_at = now() WHERE direction = 'outbound' AND channel = $1 AND event_id = $2 AND binding_id = $3",
        )
        .bind(channel)
        .bind(event_id)
        .bind(binding_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
