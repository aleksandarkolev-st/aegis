use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{Command as LocalCommand, CommandEnvelope as LocalEnvelope};

const MAX_WIRE_BYTES: usize = 128 * 1024;
const MAX_TEXT_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RelayCommand {
    Message { text: String },
    ListTasks,
    Status { task_id: Option<String> },
    Details { task_id: Option<String> },
    Pause { task_id: Option<String> },
    Resume { task_id: Option<String> },
    Cancel { task_id: Option<String> },
    SelectTask { task_id: String },
    ApproveOnce { challenge_id: String },
    Deny { challenge_id: String },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayCommandEnvelope {
    pub envelope_id: String,
    pub request_id: String,
    pub installation_id: String,
    pub actor_id: String,
    pub channel: String,
    pub sender_id: String,
    pub external_message_id: String,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub command: RelayCommand,
}

impl RelayCommandEnvelope {
    pub fn parse(subject: &str, bytes: &[u8], expected_installation: &str) -> Result<Self> {
        if bytes.is_empty() || bytes.len() > MAX_WIRE_BYTES {
            bail!("remote command envelope has an invalid size");
        }
        let envelope: Self =
            serde_json::from_slice(bytes).context("invalid remote command envelope")?;
        if subject != format!("aegis.commands.{expected_installation}")
            || envelope.installation_id != expected_installation
        {
            bail!("remote command targets a different Aegis installation");
        }
        uuid::Uuid::parse_str(&envelope.envelope_id).context("invalid relay envelope ID")?;
        uuid::Uuid::parse_str(&envelope.request_id).context("invalid relay request ID")?;
        uuid::Uuid::parse_str(&envelope.installation_id).context("invalid installation ID")?;
        if envelope.channel != "whatsapp"
            || !valid_actor(&envelope.actor_id)
            || envelope.sender_id.is_empty()
            || envelope.sender_id.len() > 256
            || envelope.external_message_id.is_empty()
            || envelope.external_message_id.len() > 256
        {
            bail!("remote command metadata is invalid");
        }
        if envelope.expires_at <= envelope.issued_at
            || envelope.expires_at.timestamp() - envelope.issued_at.timestamp() > 5 * 60
        {
            bail!("remote command expiry window is invalid");
        }
        if let RelayCommand::Message { text } = &envelope.command
            && (text.trim().is_empty()
                || text.len() > MAX_TEXT_BYTES
                || text.chars().any(|character| {
                    character.is_control() && character != '\n' && character != '\t'
                }))
        {
            bail!("remote task text is invalid");
        }
        Ok(envelope)
    }

    pub fn local_envelope(&self) -> LocalEnvelope {
        let command = match &self.command {
            RelayCommand::Message { text } => LocalCommand::Message {
                text: text.clone(),
                task_id: None,
            },
            RelayCommand::ListTasks => LocalCommand::ListTasks,
            RelayCommand::Status { task_id } => LocalCommand::Status {
                task_id: task_id.clone(),
            },
            RelayCommand::Details { task_id } => LocalCommand::Details {
                task_id: task_id.clone(),
            },
            RelayCommand::Pause { task_id } => LocalCommand::Pause {
                task_id: task_id.clone(),
            },
            RelayCommand::Resume { task_id } => LocalCommand::Resume {
                task_id: task_id.clone(),
            },
            RelayCommand::Cancel { task_id } => LocalCommand::Cancel {
                task_id: task_id.clone(),
            },
            RelayCommand::SelectTask { task_id } => LocalCommand::SelectTask {
                task_id: task_id.clone(),
            },
            RelayCommand::ApproveOnce { challenge_id } => LocalCommand::ApproveOnce {
                challenge_id: challenge_id.clone(),
            },
            RelayCommand::Deny { challenge_id } => LocalCommand::Deny {
                challenge_id: challenge_id.clone(),
            },
        };
        LocalEnvelope {
            version: 1,
            installation_id: self.installation_id.clone(),
            actor_id: self.actor_id.clone(),
            request_id: self.request_id.clone(),
            issued_at: self.issued_at.timestamp(),
            expires_at: self.expires_at.timestamp(),
            command,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    TaskStarted,
    Progress,
    ApprovalRequired,
    Blocked,
    Completed,
    Failed,
    Reply,
}

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AegisEvent {
    pub event_id: String,
    pub installation_id: String,
    pub actor_id: String,
    pub kind: EventKind,
    pub task_id: Option<String>,
    pub challenge_id: Option<String>,
    /// Unix timestamp at which an approval challenge stops accepting decisions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    pub display_detail: Option<String>,
    pub reply_text: Option<String>,
}

impl AegisEvent {
    pub fn to_json(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }

    pub fn reply(
        installation_id: &str,
        actor_id: &str,
        event_id: &str,
        reply: &str,
    ) -> Result<Self> {
        let sanitized = reply
            .chars()
            .map(|character| match character {
                '\r' => '\n',
                '\n' | '\t' => character,
                value if value.is_control() => ' ',
                value => value,
            })
            .collect::<String>();
        let reply = truncate_utf8(&sanitized, 4 * 1024);
        if reply.trim().is_empty() {
            bail!("remote reply cannot be empty");
        }
        Ok(Self {
            event_id: event_id.to_owned(),
            installation_id: installation_id.to_owned(),
            actor_id: actor_id.to_owned(),
            kind: EventKind::Reply,
            task_id: None,
            challenge_id: None,
            expires_at: None,
            display_detail: None,
            reply_text: Some(reply),
        })
    }
}

pub fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn valid_actor(actor: &str) -> bool {
    !actor.is_empty()
        && actor.len() <= 128
        && actor
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accepts_only_installation_scoped_whatsapp_envelopes() -> Result<()> {
        let installation_id = uuid::Uuid::new_v4().to_string();
        let envelope_id = uuid::Uuid::new_v4().to_string();
        let request_id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now();
        let input = json!({
            "envelope_id":envelope_id,
            "request_id":request_id,
            "installation_id":installation_id,
            "actor_id":"actor-1",
            "channel":"whatsapp",
            "sender_id":"+15551234567",
            "external_message_id":"wamid.1",
            "issued_at":now,
            "expires_at":now + chrono::Duration::minutes(5),
            "command":{"type":"message","text":"inspect the parser"}
        });
        let bytes = serde_json::to_vec(&input)?;
        let subject = format!(
            "aegis.commands.{}",
            input["installation_id"].as_str().unwrap()
        );
        let parsed = RelayCommandEnvelope::parse(
            &subject,
            &bytes,
            input["installation_id"].as_str().unwrap(),
        )?;
        assert!(matches!(parsed.command, RelayCommand::Message { .. }));
        let local = parsed.local_envelope();
        assert_eq!(local.actor_id, "actor-1");
        assert!(matches!(
            local.command,
            LocalCommand::Message { task_id: None, .. }
        ));
        Ok(())
    }

    #[test]
    fn rejects_other_installations_unknown_fields_and_oversized_replies() -> Result<()> {
        let installation_id = uuid::Uuid::new_v4().to_string();
        let mut input = json!({
            "envelope_id":uuid::Uuid::new_v4().to_string(),
            "request_id":uuid::Uuid::new_v4().to_string(),
            "installation_id":installation_id,
            "actor_id":"actor-1",
            "channel":"whatsapp",
            "sender_id":"+15551234567",
            "external_message_id":"wamid.1",
            "issued_at":Utc::now(),
            "expires_at":Utc::now() + chrono::Duration::minutes(1),
            "command":{"type":"status"}
        });
        let subject = format!("aegis.commands.{installation_id}");
        let valid_bytes = serde_json::to_vec(&input)?;
        assert!(RelayCommandEnvelope::parse(&subject, &valid_bytes, &installation_id).is_ok());
        assert!(
            RelayCommandEnvelope::parse("aegis.commands.other", &valid_bytes, &installation_id)
                .is_err()
        );
        assert!(
            RelayCommandEnvelope::parse(
                &format!("{subject}.other"),
                &valid_bytes,
                &installation_id
            )
            .is_err()
        );
        let other_installation = uuid::Uuid::new_v4().to_string();
        let other_subject = format!("aegis.commands.{other_installation}");
        assert!(
            RelayCommandEnvelope::parse(&other_subject, &valid_bytes, &other_installation).is_err()
        );
        input["extra"] = json!(true);
        let bytes = serde_json::to_vec(&input)?;
        assert!(RelayCommandEnvelope::parse(&subject, &bytes, &installation_id).is_err());
        assert_eq!(truncate_utf8(&"я".repeat(10), 7).len(), 6);
        Ok(())
    }

    #[test]
    fn replies_strip_unsupported_controls_and_stay_within_relay_limits() -> Result<()> {
        let installation_id = uuid::Uuid::new_v4().to_string();
        let event = AegisEvent::reply(
            &installation_id,
            "actor-1",
            &uuid::Uuid::new_v4().to_string(),
            "ok\u{0000}\rnext",
        )?;
        let text = event.reply_text.as_deref().unwrap();
        assert_eq!(text, "ok \nnext");
        assert!(text.bytes().all(|byte| byte != 0));

        let oversized = AegisEvent::reply(
            &installation_id,
            "actor-1",
            &uuid::Uuid::new_v4().to_string(),
            &"x".repeat(5 * 1024),
        )?;
        assert_eq!(oversized.reply_text.as_deref().unwrap().len(), 4 * 1024);
        Ok(())
    }

    #[test]
    fn parses_details_and_preserves_an_optional_task_alias() -> Result<()> {
        let installation_id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now();
        let envelope = serde_json::json!({
            "envelope_id":uuid::Uuid::new_v4().to_string(),
            "request_id":uuid::Uuid::new_v4().to_string(),
            "installation_id":installation_id,
            "actor_id":"actor-1",
            "channel":"whatsapp",
            "sender_id":"+15551234567",
            "external_message_id":"wamid.details",
            "issued_at":now,
            "expires_at":now + chrono::Duration::minutes(1),
            "command":{"type":"details","task_id":"t-abcdef"}
        });
        let bytes = serde_json::to_vec(&envelope)?;
        let subject = format!(
            "aegis.commands.{}",
            envelope["installation_id"].as_str().unwrap()
        );
        let parsed = RelayCommandEnvelope::parse(
            &subject,
            &bytes,
            envelope["installation_id"].as_str().unwrap(),
        )?;
        assert!(matches!(
            parsed.local_envelope().command,
            LocalCommand::Details { task_id: Some(id) } if id == "t-abcdef"
        ));
        Ok(())
    }
}
