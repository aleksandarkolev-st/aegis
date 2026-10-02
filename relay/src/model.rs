use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

const MAX_MESSAGE_BYTES: usize = 8 * 1024;
const MAX_REPLY_BYTES: usize = 4 * 1024;
const MAX_DISPLAY_DETAIL_BYTES: usize = 512;
pub const COMMAND_TTL_SECONDS: i64 = 5 * 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentCommand {
    Message { text: String },
    /// A slash command handled by the local Aegis authority. The relay does not
    /// reinterpret unknown slash input as a model task.
    Slash { text: String },
    ListTasks,
    Status { task_id: Option<String> },
    Result { task_id: Option<String> },
    Evidence { task_id: Option<String> },
    Details { task_id: Option<String> },
    Pause { task_id: Option<String> },
    Resume { task_id: Option<String> },
    Cancel { task_id: Option<String> },
    SelectTask { task_id: String },
    ApproveOnce { challenge_id: String },
    Deny { challenge_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandEnvelope {
    pub envelope_id: Uuid,
    pub request_id: Uuid,
    pub installation_id: Uuid,
    pub actor_id: String,
    pub channel: String,
    pub sender_id: String,
    pub external_message_id: String,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub command: AgentCommand,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteEventKind {
    TaskStarted,
    Progress,
    ApprovalRequired,
    Blocked,
    Completed,
    Failed,
    Reply,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AegisEvent {
    pub event_id: Uuid,
    pub installation_id: Uuid,
    pub actor_id: String,
    pub kind: RemoteEventKind,
    pub task_id: Option<String>,
    pub challenge_id: Option<String>,
    /// Unix deadline of an approval challenge. Present only for approval events.
    pub expires_at: Option<i64>,
    /// Short capability plus bounded target for approval prompts. This value is
    /// delivered transiently and is never written to PostgreSQL or service logs.
    pub display_detail: Option<String>,
    /// Present only for the user-facing `reply` event. It is transported transiently
    /// and is never written to PostgreSQL or service logs.
    pub reply_text: Option<String>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CommandParseError {
    #[error("message is empty")]
    Empty,
    #[error("message exceeds the supported size")]
    TooLong,
    #[error("message contains unsupported control characters")]
    ControlCharacter,
    #[error("invalid command syntax")]
    InvalidSyntax,
}

pub fn parse_command(input: &str) -> Result<AgentCommand, CommandParseError> {
    let normalized = normalize_text(input)?;
    let text = normalized.trim();
    if text.is_empty() {
        return Err(CommandParseError::Empty);
    }

    let (command, rest) = text
        .split_once(char::is_whitespace)
        .map_or((text, ""), |(command, rest)| (command, rest.trim()));

    match command {
        "tasks" | "/tasks" | "/list_tasks" if rest.is_empty() => Ok(AgentCommand::ListTasks),
        "/tasks" if rest == "plan" => Ok(AgentCommand::Slash {
            text: text.to_owned(),
        }),
        "/status" => Ok(AgentCommand::Status {
            task_id: optional_id(rest)?,
        }),
        "/result" => Ok(AgentCommand::Result {
            task_id: optional_id(rest)?,
        }),
        "/evidence"
            if rest
                .split_whitespace()
                .next()
                .is_some_and(obligation_reference) =>
        {
            Ok(AgentCommand::Slash {
                text: text.to_owned(),
            })
        }
        "/evidence" => Ok(AgentCommand::Evidence {
            task_id: optional_id(rest)?,
        }),
        "details" | "/details" => Ok(AgentCommand::Details {
            task_id: optional_id(rest)?,
        }),
        "/pause" => Ok(AgentCommand::Pause {
            task_id: optional_id(rest)?,
        }),
        "/resume" => Ok(AgentCommand::Resume {
            task_id: optional_id(rest)?,
        }),
        "tasks" | "/list_tasks" => Err(CommandParseError::InvalidSyntax),
        "/tasks" => Ok(AgentCommand::Slash {
            text: text.to_owned(),
        }),
        "/cancel" => Ok(AgentCommand::Cancel {
            task_id: optional_id(rest)?,
        }),
        "use" | "/select_task" | "/use" => Ok(AgentCommand::SelectTask {
            task_id: required_id(rest)?,
        }),
        "/approve_once" => Ok(AgentCommand::ApproveOnce {
            challenge_id: required_id(rest)?,
        }),
        "/deny" => Ok(AgentCommand::Deny {
            challenge_id: required_id(rest)?,
        }),
        "/message" => {
            let body = rest.trim();
            if body.is_empty() {
                return Err(CommandParseError::InvalidSyntax);
            }
            Ok(AgentCommand::Message {
                text: body.to_owned(),
            })
        }
        _ if text.starts_with('/') => Ok(AgentCommand::Slash {
            text: text.to_owned(),
        }),
        _ => Ok(AgentCommand::Message {
            text: text.to_owned(),
        }),
    }
}

fn obligation_reference(input: &str) -> bool {
    input
        .strip_prefix('O')
        .or_else(|| input.strip_prefix('o'))
        .is_some_and(|digits| {
            !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
        })
}

pub fn normalize_text(input: &str) -> Result<String, CommandParseError> {
    let normalized = input.replace("\r\n", "\n").replace('\r', "\n");
    if normalized.len() > MAX_MESSAGE_BYTES {
        return Err(CommandParseError::TooLong);
    }
    if normalized
        .chars()
        .any(|ch| ch.is_control() && ch != '\n' && ch != '\t')
    {
        return Err(CommandParseError::ControlCharacter);
    }
    Ok(normalized)
}

fn optional_id(input: &str) -> Result<Option<String>, CommandParseError> {
    if input.is_empty() {
        Ok(None)
    } else {
        required_id(input).map(Some)
    }
}

fn required_id(input: &str) -> Result<String, CommandParseError> {
    let mut parts = input.split_whitespace();
    let id = parts.next().ok_or(CommandParseError::InvalidSyntax)?;
    if parts.next().is_some() || !valid_identifier(id) {
        return Err(CommandParseError::InvalidSyntax);
    }
    Ok(id.to_owned())
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

impl AegisEvent {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.actor_id.is_empty()
            || self.actor_id.len() > 128
            || !self
                .actor_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        {
            return Err("invalid actor id");
        }
        let needs_task = !matches!(self.kind, RemoteEventKind::Reply);
        if needs_task != self.task_id.is_some() {
            return Err("event task_id does not match its type");
        }
        if self
            .task_id
            .as_deref()
            .is_some_and(|id| !valid_identifier(id))
        {
            return Err("invalid task id");
        }
        if matches!(self.kind, RemoteEventKind::ApprovalRequired) != self.challenge_id.is_some() {
            return Err("event challenge_id does not match its type");
        }
        if matches!(self.kind, RemoteEventKind::ApprovalRequired) != self.expires_at.is_some() {
            return Err("event expires_at does not match its type");
        }
        if self.expires_at.is_some_and(|timestamp| timestamp <= 0) {
            return Err("event approval expiry is invalid");
        }
        if self.expires_at.is_some_and(|timestamp| {
            chrono::DateTime::<Utc>::from_timestamp(timestamp, 0).is_none()
        }) {
            return Err("event approval expiry is outside the supported range");
        }
        if self
            .challenge_id
            .as_deref()
            .is_some_and(|id| !valid_identifier(id))
        {
            return Err("invalid challenge id");
        }
        if matches!(self.kind, RemoteEventKind::ApprovalRequired) != self.display_detail.is_some() {
            return Err("event display_detail does not match its type");
        }
        if let Some(detail) = &self.display_detail {
            if detail.trim().is_empty() || detail.len() > MAX_DISPLAY_DETAIL_BYTES {
                return Err("display detail is empty or exceeds the supported size");
            }
            if detail.chars().any(char::is_control) {
                return Err("display detail contains unsupported control characters");
            }
        }
        if matches!(self.kind, RemoteEventKind::Reply) != self.reply_text.is_some() {
            return Err("event reply_text does not match its type");
        }
        if let Some(text) = &self.reply_text {
            if text.trim().is_empty() || text.len() > MAX_REPLY_BYTES {
                return Err("reply text is empty or exceeds the supported size");
            }
            if text
                .chars()
                .any(|ch| ch.is_control() && ch != '\n' && ch != '\t')
            {
                return Err("reply text contains unsupported control characters");
            }
        }
        Ok(())
    }

    pub fn notification_text(&self) -> Result<String, &'static str> {
        self.validate()?;
        let task = self.task_id.as_deref().unwrap_or_default();
        Ok(match self.kind {
            RemoteEventKind::TaskStarted => format!("Aegis task {task} started."),
            RemoteEventKind::Progress => format!("Aegis task {task} is in progress."),
            RemoteEventKind::ApprovalRequired => {
                let challenge = self.challenge_id.as_deref().unwrap_or_default();
                let detail = self.display_detail.as_deref().unwrap_or_default();
                let expiry = self
                    .expires_at
                    .and_then(|timestamp| chrono::DateTime::<Utc>::from_timestamp(timestamp, 0))
                    .map(|timestamp| {
                        format!(" It expires at {} UTC.", timestamp.format("%Y-%m-%d %H:%M"))
                    })
                    .unwrap_or_default();
                format!(
                    "Aegis needs approval for task {task}: {detail}. Reply /approve_once {challenge} or /deny {challenge}.{expiry}"
                )
            }
            RemoteEventKind::Blocked => format!("Aegis task {task} is blocked."),
            RemoteEventKind::Completed => format!("Aegis task {task} completed."),
            RemoteEventKind::Failed => format!("Aegis task {task} failed."),
            RemoteEventKind::Reply => self.reply_text.clone().unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_remote_command_surface_without_aliasing_approval() {
        assert_eq!(
            parse_command("hello\r\nAegis").unwrap(),
            AgentCommand::Message {
                text: "hello\nAegis".to_owned()
            }
        );
        assert_eq!(
            parse_command("/message inspect this").unwrap(),
            AgentCommand::Message {
                text: "inspect this".to_owned()
            }
        );
        assert_eq!(
            parse_command("/status").unwrap(),
            AgentCommand::Status { task_id: None }
        );
        assert_eq!(
            parse_command("/status t-abcdef").unwrap(),
            AgentCommand::Status {
                task_id: Some("t-abcdef".into())
            }
        );
        assert_eq!(
            parse_command("/result").unwrap(),
            AgentCommand::Result { task_id: None }
        );
        assert_eq!(
            parse_command("/result t-abcdef").unwrap(),
            AgentCommand::Result {
                task_id: Some("t-abcdef".into())
            }
        );
        assert_eq!(
            parse_command("/evidence").unwrap(),
            AgentCommand::Evidence { task_id: None }
        );
        assert_eq!(
            parse_command("/evidence t-abcdef").unwrap(),
            AgentCommand::Evidence {
                task_id: Some("t-abcdef".into())
            }
        );
        assert_eq!(
            parse_command("/evidence O3").unwrap(),
            AgentCommand::Slash {
                text: "/evidence O3".into()
            }
        );
        assert_eq!(
            parse_command("/evidence o3").unwrap(),
            AgentCommand::Slash {
                text: "/evidence o3".into()
            }
        );
        assert_eq!(
            parse_command("/pause t-abcdef").unwrap(),
            AgentCommand::Pause {
                task_id: Some("t-abcdef".into())
            }
        );
        assert_eq!(
            parse_command("/resume t-abcdef").unwrap(),
            AgentCommand::Resume {
                task_id: Some("t-abcdef".into())
            }
        );
        assert_eq!(
            parse_command("/details").unwrap(),
            AgentCommand::Details { task_id: None }
        );
        assert_eq!(
            parse_command("/details t-abcdef").unwrap(),
            AgentCommand::Details {
                task_id: Some("t-abcdef".to_owned())
            }
        );
        assert_eq!(parse_command("/tasks").unwrap(), AgentCommand::ListTasks);
        assert_eq!(
            parse_command("/tasks plan").unwrap(),
            AgentCommand::Slash {
                text: "/tasks plan".into()
            }
        );
        assert_eq!(
            parse_command("/list_tasks").unwrap(),
            AgentCommand::ListTasks
        );
        assert_eq!(
            parse_command("/pause").unwrap(),
            AgentCommand::Pause { task_id: None }
        );
        assert_eq!(
            parse_command("/resume").unwrap(),
            AgentCommand::Resume { task_id: None }
        );
        assert_eq!(
            parse_command("/cancel t-abcdef").unwrap(),
            AgentCommand::Cancel {
                task_id: Some("t-abcdef".to_owned())
            }
        );
        assert_eq!(
            parse_command("/cancel task-4").unwrap(),
            AgentCommand::Cancel {
                task_id: Some("task-4".to_owned())
            }
        );
        assert_eq!(
            parse_command("/select_task task-4").unwrap(),
            AgentCommand::SelectTask {
                task_id: "task-4".to_owned()
            }
        );
        assert_eq!(
            parse_command("/use task-4").unwrap(),
            AgentCommand::SelectTask {
                task_id: "task-4".to_owned()
            }
        );
        assert_eq!(
            parse_command("/use t-abcdef").unwrap(),
            AgentCommand::SelectTask {
                task_id: "t-abcdef".to_owned()
            }
        );
        assert_eq!(
            parse_command("/approve_once challenge-9").unwrap(),
            AgentCommand::ApproveOnce {
                challenge_id: "challenge-9".to_owned()
            }
        );
        assert_eq!(
            parse_command("/deny challenge-9").unwrap(),
            AgentCommand::Deny {
                challenge_id: "challenge-9".to_owned()
            }
        );
        for text in [
            "/approve req-9",
            "/goal add Requirement text",
            "/help",
            "/settings",
            "/confirm O3",
            "/back",
            "/exit",
        ] {
            assert_eq!(
                parse_command(text).unwrap(),
                AgentCommand::Slash { text: text.into() },
                "{text:?} must remain a slash command"
            );
        }
        assert_eq!(
            parse_command("/goal add\r\nRequirements:\r\n- Preserve pasted text").unwrap(),
            AgentCommand::Slash {
                text: "/goal add\nRequirements:\n- Preserve pasted text".into()
            }
        );
        assert_eq!(
            serde_json::to_value(AgentCommand::Slash {
                text: "/goal add Requirements".into()
            })
            .unwrap(),
            serde_json::json!({"type":"slash","text":"/goal add Requirements"})
        );
    }

    #[test]
    fn rejects_malformed_commands_and_unsafe_text() {
        assert_eq!(
            parse_command("/approve_once"),
            Err(CommandParseError::InvalidSyntax)
        );
        assert_eq!(
            parse_command("/status extra"),
            Ok(AgentCommand::Status {
                task_id: Some("extra".to_owned())
            })
        );
        assert_eq!(
            parse_command("/status t-abcdef extra"),
            Err(CommandParseError::InvalidSyntax)
        );
        assert_eq!(
            parse_command("/details task-1 extra"),
            Err(CommandParseError::InvalidSyntax)
        );
        assert_eq!(
            parse_command("/result task-1 extra"),
            Err(CommandParseError::InvalidSyntax)
        );
        assert_eq!(
            parse_command("/evidence task-1 extra"),
            Err(CommandParseError::InvalidSyntax)
        );
        assert_eq!(
            parse_command("\u{0000}"),
            Err(CommandParseError::ControlCharacter)
        );
        assert_eq!(parse_command(" \n "), Err(CommandParseError::Empty));
    }

    #[test]
    fn remote_event_schema_has_a_small_allowlist_and_safe_templates() {
        let event = AegisEvent {
            event_id: Uuid::new_v4(),
            installation_id: Uuid::new_v4(),
            actor_id: "owner-1".into(),
            kind: RemoteEventKind::ApprovalRequired,
            task_id: Some("task-8".into()),
            challenge_id: Some("challenge-2".into()),
            expires_at: Some(1_800_000_000),
            display_detail: Some("shell: C:\\work\\repo".into()),
            reply_text: None,
        };
        assert_eq!(
            event.notification_text().unwrap(),
            "Aegis needs approval for task task-8: shell: C:\\work\\repo. Reply /approve_once challenge-2 or /deny challenge-2. It expires at 2027-01-15 08:00 UTC."
        );
        let mut missing_detail = event.clone();
        missing_detail.display_detail = None;
        assert!(missing_detail.validate().is_err());
        let mut missing_expiry = event.clone();
        missing_expiry.expires_at = None;
        assert!(missing_expiry.validate().is_err());
        let mut oversized_detail = event.clone();
        oversized_detail.display_detail = Some("x".repeat(MAX_DISPLAY_DETAIL_BYTES + 1));
        assert!(oversized_detail.validate().is_err());
        let mut multiline_detail = event.clone();
        multiline_detail.display_detail = Some("shell: target\nextra output".into());
        assert!(multiline_detail.validate().is_err());
        assert!(serde_json::from_str::<AegisEvent>(
            r#"{"event_id":"00000000-0000-0000-0000-000000000001","installation_id":"00000000-0000-0000-0000-000000000002","actor_id":"owner-1","kind":"progress","task_id":"task-1","challenge_id":null,"display_detail":null,"reply_text":null,"shell_output":"not allowed"}"#
        )
        .is_err());
    }

    #[test]
    fn reply_event_is_transient_payload_and_rejects_oversized_text() {
        let mut event = AegisEvent {
            event_id: Uuid::new_v4(),
            installation_id: Uuid::new_v4(),
            actor_id: "owner-1".into(),
            kind: RemoteEventKind::Reply,
            task_id: None,
            challenge_id: None,
            expires_at: None,
            display_detail: None,
            reply_text: Some("Ready".into()),
        };
        assert_eq!(event.notification_text().unwrap(), "Ready");
        event.reply_text = Some("x".repeat(MAX_REPLY_BYTES + 1));
        assert!(event.validate().is_err());
    }

    #[test]
    fn remote_events_require_a_valid_actor_id() {
        let mut event = AegisEvent {
            event_id: Uuid::new_v4(),
            installation_id: Uuid::new_v4(),
            actor_id: "owner-1".into(),
            kind: RemoteEventKind::Reply,
            task_id: None,
            challenge_id: None,
            expires_at: None,
            display_detail: None,
            reply_text: Some("Ready".into()),
        };
        assert!(event.validate().is_ok());
        event.actor_id = "owner/other".into();
        assert!(event.validate().is_err());
    }
}
