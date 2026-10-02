//! Authenticated, installation-local remote control for Aegis runs.
//!
//! This module is the local authority boundary, not a transport. The daemon
//! passes commands only after TLS-authenticated NATS credentials and the
//! per-install subject ACL establish the relay principal. This module then
//! checks that principal against the actor, installation, request window, local
//! task allowlist and SQLite idempotency state. No shared signing key is used.

pub mod config;
pub mod daemon;
pub(crate) mod protocol;

use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::Path;

use crate::storage::{Store, append_event};

const PROTOCOL_VERSION: u32 = 1;
const MAX_ENVELOPE_BYTES: usize = 128 * 1024;
const MAX_COMMAND_TTL_SECONDS: i64 = 5 * 60;
const CLOCK_SKEW_SECONDS: i64 = 30;
const MAX_MESSAGE_BYTES: usize = 65_536;
const MAX_REMOTE_RESULT_BYTES: usize = 2 * 1024;
const MAX_REMOTE_EVIDENCE_RECEIPTS: usize = 8;

#[derive(Debug)]
struct CommandRejected(String);

impl std::fmt::Display for CommandRejected {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for CommandRejected {}

fn reject_command(message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(CommandRejected(message.into()))
}

fn reject_command_error(error: anyhow::Error) -> anyhow::Error {
    reject_command(error.to_string())
}

pub(crate) fn is_command_rejection(error: &anyhow::Error) -> bool {
    error.downcast_ref::<CommandRejected>().is_some()
}

/// Fixed, closed set of commands accepted from remote actors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Message {
        text: String,
        task_id: Option<String>,
    },
    ListTasks,
    Status {
        task_id: Option<String>,
    },
    Result {
        task_id: Option<String>,
    },
    Evidence {
        task_id: Option<String>,
    },
    Details {
        task_id: Option<String>,
    },
    Pause {
        task_id: Option<String>,
    },
    Resume {
        task_id: Option<String>,
    },
    Cancel {
        task_id: Option<String>,
    },
    SelectTask {
        task_id: String,
    },
    ApproveOnce {
        challenge_id: String,
    },
    Deny {
        challenge_id: String,
    },
}

/// The unsigned fields that the local daemon signs after authenticating the
/// transport principal. Task IDs appear only in command variants that need
/// them; challenge IDs map to their run locally.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandEnvelope {
    pub version: u32,
    pub installation_id: String,
    pub actor_id: String,
    pub request_id: String,
    pub issued_at: i64,
    pub expires_at: i64,
    pub command: Command,
}

impl CommandEnvelope {
    /// Stable serialization used for local idempotency hashing after transport
    /// authentication has established the principal.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() > MAX_ENVELOPE_BYTES {
            bail!("remote command exceeds the protocol size limit");
        }
        Ok(bytes)
    }

    /// Hashes the immutable request identity and command payload. Freshness
    /// timestamps are checked for first acceptance but excluded so relay retries
    /// can refresh their delivery window without changing command meaning.
    fn idempotency_hash(&self) -> Result<String> {
        #[derive(Serialize)]
        struct RequestIdentity<'a> {
            version: u32,
            installation_id: &'a str,
            actor_id: &'a str,
            request_id: &'a str,
            command: &'a Command,
        }

        let material = RequestIdentity {
            version: self.version,
            installation_id: &self.installation_id,
            actor_id: &self.actor_id,
            request_id: &self.request_id,
            command: &self.command,
        };
        let bytes = serde_json::to_vec(&material)?;
        if bytes.len() > MAX_ENVELOPE_BYTES {
            bail!("remote command exceeds the protocol size limit");
        }
        Ok(hex::encode(Sha256::digest(bytes)))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Receipt {
    pub request_id: String,
    pub duplicate: bool,
    pub result: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalChallenge {
    pub challenge_id: String,
    pub run_id: String,
    pub operation_id: String,
    pub capability: String,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchAuthorization {
    /// This operation has no remote decision gate; normal local policy applies.
    Ungated,
    /// The kernel must stop before dispatch and wait for a local/remote decision.
    AwaitingDecision,
    /// A local gate approved this exact operation once; this call atomically
    /// consumed that decision and marked the operation dispatched.
    ApprovedOnce,
    /// The exact operation was denied and atomically marked cancelled.
    Denied,
    /// Its local approval challenge expired and it was cancelled.
    Expired,
}

enum CommandApply {
    Applied {
        result: Value,
        task_id: Option<String>,
    },
    NeedsTaskCreation,
}

/// Local authority over a single Aegis installation's remote actors.
pub struct Authority {
    store: Store,
    installation_id: String,
}

impl Authority {
    /// Opens the canonical local Aegis database and creates the installation
    /// identity once. No network or provider credentials are loaded.
    pub fn open(root: &Path) -> Result<Self> {
        Self::from_store(Store::open(root)?)
    }

    pub fn from_store(store: Store) -> Result<Self> {
        ensure_schema(&store.connection)?;

        let transaction = store.connection.unchecked_transaction()?;
        let existing: Option<String> = transaction
            .query_row(
                "SELECT installation_id FROM remote_installation WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let installation_id = match existing {
            Some(id) => id,
            None => {
                let id = uuid::Uuid::new_v4().to_string();
                transaction.execute(
                    "INSERT INTO remote_installation(singleton,installation_id) VALUES (1,?1)",
                    [&id],
                )?;
                id
            }
        };
        transaction.commit()?;
        uuid::Uuid::parse_str(&installation_id)
            .context("stored Aegis remote installation identity is invalid")?;
        Ok(Self {
            store,
            installation_id,
        })
    }

    pub fn installation_id(&self) -> &str {
        &self.installation_id
    }

    /// Register a relay-authenticated actor without granting access to any run.
    /// The transport's TLS identity and subject ACL are the actor trust boundary.
    pub fn register_actor(&mut self, actor_id: &str) -> Result<()> {
        validate_actor_id(actor_id)?;
        let transaction = self
            .store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO remote_actors(actor_id,enabled,paired_at)
             VALUES (?1,1,?2)
             ON CONFLICT(actor_id) DO NOTHING",
            params![actor_id, crate::storage::unix_time()],
        )?;
        let enabled: bool = transaction.query_row(
            "SELECT enabled FROM remote_actors WHERE actor_id=?1",
            [actor_id],
            |row| row.get(0),
        )?;
        if !enabled {
            bail!("a revoked remote actor identity cannot be re-enabled; pair a new identity");
        }
        transaction.execute(
            "UPDATE remote_actors SET paired_at=?2 WHERE actor_id=?1 AND enabled=1",
            params![actor_id, crate::storage::unix_time()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Lists locally revoked actor identities whose relay cleanup is pending.
    /// The SQLite database is installation-local, so this cannot return IDs
    /// belonging to another installation.
    pub fn pending_relay_revocation_actor_ids(&self) -> Result<Vec<String>> {
        let mut statement = self.store.connection.prepare(
            "SELECT actor.actor_id FROM remote_actors AS actor
                 JOIN remote_actor_revocations AS revocation USING(actor_id)
                 WHERE actor.enabled=0 AND revocation.relay_revoked_at IS NULL
                 ORDER BY actor.actor_id",
        )?;
        let actors = statement
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(actors)
    }

    /// Records successful cleanup at the authenticated relay. Failed requests
    /// stay pending and can be retried by `remote revoke` or the next pair.
    pub fn mark_relay_actor_revoked(&mut self, actor_id: &str) -> Result<()> {
        validate_actor_id(actor_id)?;
        self.store.connection.execute(
            "UPDATE remote_actor_revocations
             SET relay_revoked_at=?2
             WHERE actor_id=?1
               AND relay_revoked_at IS NULL
               AND EXISTS (SELECT 1 FROM remote_actors WHERE actor_id=?1 AND enabled=0)",
            params![actor_id, crate::storage::unix_time()],
        )?;
        Ok(())
    }

    /// Register one actor and add only the explicitly supplied run scopes.
    pub fn pair_actor(&mut self, actor_id: &str, run_ids: &[String]) -> Result<()> {
        self.register_actor(actor_id)?;
        let mut unique = std::collections::BTreeSet::new();
        for run_id in run_ids {
            validate_uuid(run_id, "run ID")?;
            self.store.run(run_id)?;
            if !unique.insert(run_id) {
                bail!("remote actor task scopes must be unique");
            }
        }
        let transaction = self
            .store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "DELETE FROM remote_actor_runs WHERE actor_id=?1",
            [actor_id],
        )?;
        for run_id in run_ids {
            transaction.execute(
                "INSERT INTO remote_actor_runs(actor_id,run_id,enabled) VALUES (?1,?2,1)",
                params![actor_id, run_id],
            )?;
            ensure_task_alias(&transaction, actor_id, run_id)?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn grant_run(&mut self, actor_id: &str, run_id: &str) -> Result<()> {
        validate_actor_id(actor_id)?;
        validate_uuid(run_id, "run ID")?;
        self.store.run(run_id)?;
        let transaction = self
            .store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "INSERT INTO remote_actor_runs(actor_id,run_id,enabled)
             SELECT actor_id,?2,1 FROM remote_actors WHERE actor_id=?1 AND enabled=1
             ON CONFLICT(actor_id,run_id) DO UPDATE SET enabled=1",
            params![actor_id, run_id],
        )?;
        if changed != 1 {
            bail!("remote actor is not paired or has been revoked");
        }
        ensure_task_alias(&transaction, actor_id, run_id)?;
        transaction.commit()?;
        Ok(())
    }

    /// Removes one actor's access to a run and clears its selection when it
    /// points at that run. A later share must be explicit again.
    pub fn revoke_run(&mut self, actor_id: &str, run_id: &str) -> Result<bool> {
        validate_actor_id(actor_id)?;
        validate_uuid(run_id, "run ID")?;
        self.store.run(run_id)?;
        let transaction = self
            .store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE remote_actor_runs SET enabled=0
             WHERE actor_id=?1 AND run_id=?2 AND enabled=1",
            params![actor_id, run_id],
        )?;
        transaction.execute(
            "DELETE FROM remote_selected_tasks WHERE actor_id=?1 AND run_id=?2",
            params![actor_id, run_id],
        )?;
        // A ListTasks receipt has no task_id. Invalidate these read-only
        // snapshots so a retried request cannot return a task after unsharing.
        if changed != 0 {
            transaction.execute(
                "DELETE FROM remote_requests WHERE actor_id=?1 AND task_id IS NULL",
                [actor_id],
            )?;
        }
        transaction.commit()?;
        Ok(changed == 1)
    }

    /// Revokes a principal and clears its active run scopes and task selection.
    /// A later pairing must explicitly grant tasks again.
    pub fn revoke_actor(&mut self, actor_id: &str) -> Result<bool> {
        validate_actor_id(actor_id)?;
        let transaction = self
            .store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE remote_actors SET enabled=0 WHERE actor_id=?1 AND enabled=1",
            [actor_id],
        )?;
        transaction.execute(
            "UPDATE remote_actor_runs SET enabled=0 WHERE actor_id=?1",
            [actor_id],
        )?;
        transaction.execute(
            "DELETE FROM remote_selected_tasks WHERE actor_id=?1",
            [actor_id],
        )?;
        transaction.execute(
            "INSERT OR IGNORE INTO remote_actor_revocations(actor_id,relay_revoked_at)
             SELECT actor_id,NULL FROM remote_actors WHERE actor_id=?1 AND enabled=0",
            [actor_id],
        )?;
        transaction.commit()?;
        Ok(changed == 1)
    }

    /// Prompts the local user to put this immutable operation intent behind a
    /// remote decision gate. The first/default choice leaves existing behavior
    /// unchanged; only an explicit selection creates the gate.
    pub fn request_operation_decision_locally(
        &mut self,
        terminal: &crate::terminal::Terminal,
        run_id: &str,
        operation_id: &str,
    ) -> Result<Option<ApprovalChallenge>> {
        validate_uuid(run_id, "run ID")?;
        validate_uuid(operation_id, "operation ID")?;
        let operation = self.store.operation(operation_id)?;
        if operation.run_id != run_id || operation.state != "pending" {
            bail!("only this run's exact pending operation can enter remote approval");
        }
        let target = operation.arguments["path"]
            .as_str()
            .or_else(|| operation.arguments["program"].as_str())
            .unwrap_or("(no file or program target)");
        terminal.message(
            crate::terminal::Tone::Warning,
            "Remote operation decision",
            &format!(
                "{} · {} · {} · operation {}",
                operation.capability,
                target.chars().take(120).collect::<String>(),
                "the remote actor may approve this exact operation once or deny it",
                operation.id
            ),
        )?;
        let choices = vec![
            "Keep this operation local; do not create a remote approval request".to_owned(),
            "Require a remote approve-once or deny decision for this exact operation".to_owned(),
        ];
        if terminal.select("Confirm remote approval requirement", &choices)? != Some(1) {
            return Ok(None);
        }
        Ok(Some(ensure_remote_approval_gate(
            &mut self.store,
            run_id,
            operation_id,
            crate::storage::unix_time(),
        )?))
    }

    /// Called by the local kernel immediately before dispatch. It fails closed
    /// for pending gates and consumes an approval only in the same transaction
    /// that moves the exact operation to `dispatched`.
    pub fn authorize_operation_dispatch(
        &mut self,
        run_id: &str,
        operation_id: &str,
    ) -> Result<DispatchAuthorization> {
        authorize_operation_dispatch(&mut self.store, run_id, operation_id)
    }

    /// Accepts a command only after TLS and per-installation NATS subject ACLs
    /// authenticate the transport principal. It then checks that principal
    /// against the actor in the request and applies local identity/run scopes,
    /// expiry, schema and idempotency checks in SQLite.
    pub fn apply_from_authenticated_relay(
        &mut self,
        authenticated_actor_id: &str,
        request: &CommandEnvelope,
        now: i64,
    ) -> Result<Receipt> {
        validate_actor_id(authenticated_actor_id).map_err(reject_command_error)?;
        if request.actor_id != authenticated_actor_id {
            return Err(reject_command(
                "relay-authenticated actor does not match the command actor",
            ));
        }
        validate_request_shape(request).map_err(reject_command_error)?;
        if request.installation_id != self.installation_id {
            return Err(reject_command(
                "remote command targets a different Aegis installation",
            ));
        }
        request.canonical_bytes()?;
        let request_hash = request.idempotency_hash()?;

        let transaction = self
            .store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let enabled: Option<bool> = transaction
            .query_row(
                "SELECT enabled FROM remote_actors WHERE actor_id=?1",
                [&request.actor_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(enabled) = enabled else {
            return Err(reject_command("remote actor is not paired"));
        };
        if !enabled {
            return Err(reject_command("remote actor is revoked"));
        }
        let prior: Option<(String, String, Option<String>)> = transaction
            .query_row(
                "SELECT request_hash,result,task_id FROM remote_requests WHERE actor_id=?1 AND request_id=?2",
                params![request.actor_id, request.request_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((prior_hash, prior_result, task_id)) = prior {
            if prior_hash != request_hash {
                return Err(reject_command(
                    "remote request ID was already used for different content",
                ));
            }
            if let Some(task_id) = task_id.as_deref() {
                require_actor_run(&transaction, &request.actor_id, task_id)?;
            }
            let result: Value = if matches!(&request.command, Command::Evidence { .. }) {
                let refresh_command = match &request.command {
                    Command::Evidence { task_id: None } => Command::Evidence {
                        task_id: task_id.clone(),
                    },
                    command => command.clone(),
                };
                let CommandApply::Applied { result, .. } = apply_command(
                    &transaction,
                    &request.actor_id,
                    &request.request_id,
                    now,
                    &refresh_command,
                )?
                else {
                    bail!("evidence command unexpectedly requested task creation");
                };
                transaction.execute(
                    "UPDATE remote_requests SET result=?3 WHERE actor_id=?1 AND request_id=?2",
                    params![request.actor_id, request.request_id, result.to_string()],
                )?;
                result
            } else {
                let mut result = serde_json::from_str(&prior_result)?;
                redact_cached_remote_text(&mut result, &request.command);
                result
            };
            transaction.commit()?;
            return Ok(Receipt {
                request_id: request.request_id.clone(),
                duplicate: true,
                result,
            });
        }

        validate_request_time(request, now).map_err(reject_command_error)?;
        let applied = apply_command(
            &transaction,
            &request.actor_id,
            &request.request_id,
            now,
            &request.command,
        )?;
        let CommandApply::Applied { result, task_id } = applied else {
            // The local daemon may create a task only through its separately
            // authorized, workspace-bound task-creation path. Do not claim or
            // deduplicate this message until it is rebound to that task.
            transaction.commit()?;
            return Ok(Receipt {
                request_id: request.request_id.clone(),
                duplicate: false,
                result: json!({"needs_task_creation":true}),
            });
        };
        transaction.execute(
            "INSERT INTO remote_requests(actor_id,request_id,task_id,request_hash,result,accepted_at,expires_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                request.actor_id,
                request.request_id,
                task_id,
                request_hash,
                result.to_string(),
                now,
                request.expires_at
            ],
        )?;
        transaction.commit()?;
        Ok(Receipt {
            request_id: request.request_id.clone(),
            duplicate: false,
            result,
        })
    }

    pub fn selected_task(&self, actor_id: &str) -> Result<Option<String>> {
        validate_actor_id(actor_id)?;
        let selected: Option<String> = self
            .store
            .connection
            .query_row(
                "SELECT run_id FROM remote_selected_tasks WHERE actor_id=?1",
                [actor_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(run_id) = selected else {
            return Ok(None);
        };
        let enabled: bool = self.store.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM remote_actor_runs AS access
             JOIN remote_actors AS actor ON actor.actor_id=access.actor_id
             WHERE access.actor_id=?1 AND access.run_id=?2 AND access.enabled=1 AND actor.enabled=1)",
            params![actor_id, run_id],
            |row| row.get(0),
        )?;
        Ok(enabled.then_some(run_id))
    }

    /// Returns only tasks explicitly authorized for this actor.
    pub fn authorized_runs(&self, actor_id: &str) -> Result<Vec<Value>> {
        validate_actor_id(actor_id)?;
        let mut statement = self.store.connection.prepare(
            "SELECT run.id,run.task,run.state,run.provider,run.created_at,
             aliases.alias,
             EXISTS(SELECT 1 FROM remote_selected_tasks AS selected WHERE selected.actor_id=?1 AND selected.run_id=run.id)
             FROM remote_actor_runs AS access JOIN runs AS run ON run.id=access.run_id
             JOIN remote_actors AS actor ON actor.actor_id=access.actor_id
             JOIN remote_task_aliases AS aliases ON aliases.actor_id=access.actor_id AND aliases.run_id=run.id
             WHERE access.actor_id=?1 AND access.enabled=1 AND actor.enabled=1 ORDER BY run.created_at DESC LIMIT 100",
        )?;
        let rows = statement.query_map([actor_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, bool>(6)?,
            ))
        })?;
        let mut tasks = Vec::new();
        for row in rows {
            let (id, task, state, provider, created_at, alias, selected) = row?;
            tasks.push(json!({
                "task_id":id,
                "alias":alias,
                "task":redact_remote_text(&task).chars().take(300).collect::<String>(),
                "state":state,
                "provider":provider,
                "created_at":created_at,
                "selected":selected,
            }));
        }
        Ok(tasks)
    }

    /// Lists only enabled actors that may receive events for this run.
    pub fn actors_for_run(&self, run_id: &str) -> Result<Vec<String>> {
        validate_uuid(run_id, "run ID")?;
        let mut statement = self.store.connection.prepare(
            "SELECT access.actor_id FROM remote_actor_runs AS access
             JOIN remote_actors AS actor ON actor.actor_id=access.actor_id
             WHERE access.run_id=?1 AND access.enabled=1 AND actor.enabled=1
             ORDER BY access.actor_id",
        )?;
        let rows = statement.query_map([run_id], |row| row.get(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .context("could not list event-authorized remote actors")
    }

    /// Resolves an explicit task or the actor's current selection and enforces
    /// the local actor→run allowlist. `None` means no run is selected.
    pub fn resolve_task(&self, actor_id: &str, task_id: Option<&str>) -> Result<Option<String>> {
        validate_actor_id(actor_id)?;
        if let Some(task_id) = task_id {
            validate_uuid(task_id, "task ID")?;
            require_actor_run_connection(&self.store.connection, actor_id, task_id)?;
            return Ok(Some(task_id.to_owned()));
        }
        self.selected_task(actor_id)
    }

    pub fn bind_selected_task(&mut self, actor_id: &str, task_id: &str) -> Result<()> {
        validate_actor_id(actor_id)?;
        validate_uuid(task_id, "task ID")?;
        require_actor_run_connection(&self.store.connection, actor_id, task_id)?;
        self.store.connection.execute(
            "INSERT INTO remote_selected_tasks(actor_id,run_id,selected_at) VALUES (?1,?2,?3)
             ON CONFLICT(actor_id) DO UPDATE SET run_id=excluded.run_id,selected_at=excluded.selected_at",
            params![actor_id, task_id, crate::storage::unix_time()],
        )?;
        Ok(())
    }
}

/// Creates or returns the challenge for one exact pending operation. The
/// local kernel calls this only when its local policy requires remote review;
/// an inbound relay command cannot create or retarget a gate.
pub fn ensure_remote_approval_gate(
    store: &mut Store,
    run_id: &str,
    operation_id: &str,
    now: i64,
) -> Result<ApprovalChallenge> {
    validate_uuid(run_id, "run ID")?;
    validate_uuid(operation_id, "operation ID")?;
    if now <= 0 {
        bail!("approval gate time must be positive");
    }
    ensure_schema(&store.connection)?;
    let transaction = store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    let operation = operation_in_transaction(&transaction, run_id, operation_id)?;
    if operation.state != "pending" {
        bail!("only this run's exact pending operation can enter remote approval");
    }
    let intent_hash = operation_intent_hash(&operation)?;
    let existing: Option<(String, String, String, i64)> = transaction
        .query_row(
            "SELECT intent_hash,state,challenge_id,expires_at FROM remote_operation_gates WHERE run_id=?1 AND operation_id=?2",
            params![run_id, operation_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    if let Some((existing_hash, state, challenge_id, expires_at)) = existing {
        if existing_hash != intent_hash || state != "pending" {
            bail!("this operation already has a different or resolved approval gate");
        }
        if expires_at <= now {
            bail!("this operation's remote approval challenge has expired");
        }
        transaction.commit()?;
        return Ok(ApprovalChallenge {
            challenge_id,
            run_id: run_id.to_owned(),
            operation_id: operation_id.to_owned(),
            capability: operation.capability,
            expires_at,
        });
    }
    let challenge_id = uuid::Uuid::new_v4().to_string();
    let expires_at = now.saturating_add(MAX_COMMAND_TTL_SECONDS);
    transaction.execute(
        "INSERT INTO remote_operation_gates(run_id,operation_id,challenge_id,intent_hash,state,requested_at,expires_at)
         VALUES (?1,?2,?3,?4,'pending',?5,?6)",
        params![run_id, operation_id, challenge_id, intent_hash, now, expires_at],
    )?;
    append_event(
        &transaction,
        run_id,
        "approval.required",
        json!({"challenge_id":challenge_id,"operation_id":operation_id,"capability":operation.capability,"descriptor":operation_display_descriptor(&operation),"expires_at":expires_at,"source":"local_kernel_policy"}),
    )?;
    transaction.commit()?;
    Ok(ApprovalChallenge {
        challenge_id,
        run_id: run_id.to_owned(),
        operation_id: operation_id.to_owned(),
        capability: operation.capability,
        expires_at,
    })
}

/// Expires all overdue pending gates and cancels the corresponding operations
/// transactionally. The kernel may call this during wakeup before resuming.
pub fn expire_remote_approval_gates(store: &mut Store, now: i64) -> Result<usize> {
    if now <= 0 {
        bail!("approval gate time must be positive");
    }
    ensure_schema(&store.connection)?;
    let transaction = store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut statement = transaction.prepare(
        "SELECT run_id,operation_id FROM remote_operation_gates WHERE state='pending' AND expires_at<=?1 ORDER BY expires_at",
    )?;
    let rows = statement.query_map([now], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let overdue = rows.collect::<std::result::Result<Vec<_>, _>>()?;
    drop(statement);
    let mut expired = 0;
    for (run_id, operation_id) in overdue {
        let changed = transaction.execute(
            "UPDATE remote_operation_gates SET state='expired' WHERE run_id=?1 AND operation_id=?2 AND state='pending' AND expires_at<=?3",
            params![run_id, operation_id, now],
        )?;
        if changed == 0 {
            continue;
        }
        let cancelled = transaction.execute(
            "UPDATE operations SET state='cancelled' WHERE id=?1 AND run_id=?2 AND state='pending'",
            params![operation_id, run_id],
        )?;
        if cancelled == 1 {
            append_event(
                &transaction,
                &run_id,
                "operation.cancelled",
                json!({"id":operation_id,"detail":{"reason":"remote approval challenge expired"}}),
            )?;
        }
        append_event(
            &transaction,
            &run_id,
            "approval.expired",
            json!({"operation_id":operation_id}),
        )?;
        expired += 1;
    }
    transaction.commit()?;
    Ok(expired)
}

fn operation_display_descriptor(operation: &crate::storage::Operation) -> String {
    // A URL path or query may contain a bearer token even when its parameter
    // name is unfamiliar. Approval notifications cross the relay, so show
    // only the public authority for URL operations.
    let target = if let Some(address) = operation.arguments["url"].as_str() {
        reqwest::Url::parse(address)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
            .unwrap_or_else(|| "(URL target hidden)".to_owned())
    } else {
        ["path", "program", "host"]
            .iter()
            .find_map(|key| operation.arguments.get(*key).and_then(Value::as_str))
            .unwrap_or("(target not available)")
            .to_owned()
    };
    format!(
        "{} · {}",
        operation.capability,
        crate::text::clean(&target)
            .chars()
            .take(120)
            .collect::<String>()
    )
}

/// Kernel-facing gate check. A remote decision cannot grant tools, change
/// arguments, or broaden a task; it can authorize or deny only this exact
/// pending operation ID and immutable intent.
pub fn authorize_operation_dispatch(
    store: &mut Store,
    run_id: &str,
    operation_id: &str,
) -> Result<DispatchAuthorization> {
    validate_uuid(run_id, "run ID")?;
    validate_uuid(operation_id, "operation ID")?;
    ensure_schema(&store.connection)?;
    let transaction = store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    let gate: Option<(String, String, i64)> = transaction
        .query_row(
            "SELECT intent_hash,state,expires_at FROM remote_operation_gates WHERE run_id=?1 AND operation_id=?2",
            params![run_id, operation_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((intent_hash, state, expires_at)) = gate else {
        transaction.commit()?;
        return Ok(DispatchAuthorization::Ungated);
    };
    let operation = operation_in_transaction(&transaction, run_id, operation_id)?;
    if operation.state != "pending" {
        if state == "consumed" && operation.state == "cancelled" {
            transaction.commit()?;
            return Ok(DispatchAuthorization::Denied);
        }
        if state == "expired" && operation.state == "cancelled" {
            transaction.commit()?;
            return Ok(DispatchAuthorization::Expired);
        }
        bail!("gated operation is no longer pending");
    }
    if operation_intent_hash(&operation)? != intent_hash {
        bail!("gated operation changed before kernel dispatch");
    }
    if state == "pending" && expires_at <= crate::storage::unix_time() {
        transaction.execute(
            "UPDATE operations SET state='cancelled' WHERE id=?1 AND run_id=?2 AND state='pending'",
            params![operation_id, run_id],
        )?;
        transaction.execute(
            "UPDATE remote_operation_gates SET state='expired' WHERE run_id=?1 AND operation_id=?2 AND state='pending'",
            params![run_id, operation_id],
        )?;
        append_event(
            &transaction,
            run_id,
            "operation.cancelled",
            json!({"id":operation_id,"detail":{"reason":"remote approval challenge expired"}}),
        )?;
        append_event(
            &transaction,
            run_id,
            "approval.expired",
            json!({"operation_id":operation_id}),
        )?;
        transaction.commit()?;
        return Ok(DispatchAuthorization::Expired);
    }
    match state.as_str() {
        "pending" => {
            transaction.commit()?;
            Ok(DispatchAuthorization::AwaitingDecision)
        }
        "denied" => {
            let changed = transaction.execute(
                "UPDATE operations SET state='cancelled' WHERE id=?1 AND run_id=?2 AND state='pending'",
                params![operation_id, run_id],
            )?;
            if changed != 1 {
                bail!("denied operation changed before cancellation");
            }
            transaction.execute(
                "UPDATE remote_operation_gates SET state='consumed' WHERE run_id=?1 AND operation_id=?2 AND state='denied'",
                params![run_id, operation_id],
            )?;
            append_event(
                &transaction,
                run_id,
                "operation.cancelled",
                json!({"id":operation_id,"detail":{"reason":"remote actor denied the exact pending operation"}}),
            )?;
            append_event(
                &transaction,
                run_id,
                "remote.operation_decision_consumed",
                json!({"operation_id":operation_id,"decision":"denied"}),
            )?;
            transaction.commit()?;
            Ok(DispatchAuthorization::Denied)
        }
        "approved_once" => {
            let changed = transaction.execute(
                "UPDATE operations SET state='dispatched' WHERE id=?1 AND run_id=?2 AND state='pending'",
                params![operation_id, run_id],
            )?;
            if changed != 1 {
                bail!("approved operation changed before dispatch");
            }
            transaction.execute(
                "UPDATE remote_operation_gates SET state='consumed' WHERE run_id=?1 AND operation_id=?2 AND state='approved_once'",
                params![run_id, operation_id],
            )?;
            append_event(
                &transaction,
                run_id,
                "operation.dispatched",
                json!({"id":operation_id,"idempotency_key":operation.idempotency_key,"remote_approval":true}),
            )?;
            append_event(
                &transaction,
                run_id,
                "remote.operation_decision_consumed",
                json!({"operation_id":operation_id,"decision":"approved_once"}),
            )?;
            transaction.commit()?;
            Ok(DispatchAuthorization::ApprovedOnce)
        }
        "consumed" => {
            transaction.commit()?;
            bail!("one-time remote operation decision was already consumed")
        }
        "expired" => {
            transaction.commit()?;
            Ok(DispatchAuthorization::Expired)
        }
        _ => bail!("remote operation gate has an invalid state"),
    }
}

fn operation_in_transaction(
    transaction: &Transaction<'_>,
    run_id: &str,
    operation_id: &str,
) -> Result<crate::storage::Operation> {
    transaction.query_row(
        "SELECT capability,capability_version,arguments,idempotency_key,retry_safe,state,artifact FROM operations WHERE id=?1 AND run_id=?2",
        params![operation_id,run_id],
        |row| {
            let arguments: String = row.get(2)?;
            Ok(crate::storage::Operation {
                id: operation_id.to_owned(), run_id: run_id.to_owned(),
                capability: row.get(0)?, capability_version: row.get(1)?,
                arguments: serde_json::from_str(&arguments).map_err(|error| rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(error)))?,
                idempotency_key: row.get(3)?, retry_safe: row.get(4)?, state: row.get(5)?, artifact: row.get(6)?,
            })
        },
    ).context("remote operation gate does not match an operation in this run")
}

fn ensure_schema(connection: &rusqlite::Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS remote_installation (
            singleton INTEGER PRIMARY KEY CHECK(singleton=1),
            installation_id TEXT NOT NULL UNIQUE
         );
         CREATE TABLE IF NOT EXISTS remote_actors (
            actor_id TEXT PRIMARY KEY,
            enabled INTEGER NOT NULL,
            paired_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS remote_actor_revocations (
            actor_id TEXT PRIMARY KEY REFERENCES remote_actors(actor_id),
            relay_revoked_at INTEGER
         );
         CREATE TABLE IF NOT EXISTS remote_actor_runs (
            actor_id TEXT NOT NULL REFERENCES remote_actors(actor_id),
            run_id TEXT NOT NULL REFERENCES runs(id),
            enabled INTEGER NOT NULL,
            PRIMARY KEY(actor_id,run_id)
         );
         CREATE TABLE IF NOT EXISTS remote_task_aliases (
            actor_id TEXT NOT NULL REFERENCES remote_actors(actor_id),
            run_id TEXT NOT NULL REFERENCES runs(id),
            alias TEXT NOT NULL,
            PRIMARY KEY(actor_id,run_id),
            UNIQUE(actor_id,alias)
         );
         CREATE TABLE IF NOT EXISTS remote_requests (
            actor_id TEXT NOT NULL REFERENCES remote_actors(actor_id),
            request_id TEXT NOT NULL,
            task_id TEXT REFERENCES runs(id),
            request_hash TEXT NOT NULL,
            result TEXT NOT NULL,
            accepted_at INTEGER NOT NULL,
            expires_at INTEGER NOT NULL,
            PRIMARY KEY(actor_id,request_id)
         );
         CREATE TABLE IF NOT EXISTS remote_selected_tasks (
            actor_id TEXT PRIMARY KEY REFERENCES remote_actors(actor_id),
            run_id TEXT NOT NULL REFERENCES runs(id),
            selected_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS remote_operation_gates (
            run_id TEXT NOT NULL REFERENCES runs(id),
            operation_id TEXT NOT NULL REFERENCES operations(id),
            challenge_id TEXT NOT NULL UNIQUE,
            intent_hash TEXT NOT NULL,
            state TEXT NOT NULL,
            requested_at INTEGER NOT NULL,
            expires_at INTEGER NOT NULL,
            decision_actor TEXT REFERENCES remote_actors(actor_id),
            decision_request TEXT,
            decided_at INTEGER,
            PRIMARY KEY(run_id,operation_id)
         );",
    )?;
    // Treat historical local revocations as pending until the authenticated
    // relay endpoint confirms each cleanup. This preserves retry on upgrades.
    connection.execute(
        "INSERT OR IGNORE INTO remote_actor_revocations(actor_id,relay_revoked_at)
         SELECT actor_id,NULL FROM remote_actors WHERE enabled=0",
        [],
    )?;
    // Backfill aliases for installations that already have authorized remote
    // tasks. Ordering makes first-time allocation deterministic; thereafter
    // persisted aliases do not change when the actor receives more tasks.
    let mut statement = connection.prepare(
        "SELECT access.actor_id,access.run_id FROM remote_actor_runs AS access
         LEFT JOIN remote_task_aliases AS aliases
           ON aliases.actor_id=access.actor_id AND aliases.run_id=access.run_id
         WHERE aliases.run_id IS NULL ORDER BY access.actor_id,access.run_id",
    )?;
    let missing = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    for (actor_id, run_id) in missing {
        ensure_task_alias(connection, &actor_id, &run_id)?;
    }
    Ok(())
}

fn ensure_task_alias(
    connection: &rusqlite::Connection,
    actor_id: &str,
    run_id: &str,
) -> Result<String> {
    if let Some(alias) = connection
        .query_row(
            "SELECT alias FROM remote_task_aliases WHERE actor_id=?1 AND run_id=?2",
            params![actor_id, run_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    {
        return Ok(alias);
    }
    let compact = uuid::Uuid::parse_str(run_id)?.simple().to_string();
    for prefix_length in 6..=compact.len() {
        let alias = format!("t-{}", &compact[..prefix_length]);
        let used: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM remote_task_aliases WHERE actor_id=?1 AND alias=?2)",
            params![actor_id, alias],
            |row| row.get(0),
        )?;
        if !used {
            connection.execute(
                "INSERT INTO remote_task_aliases(actor_id,run_id,alias) VALUES (?1,?2,?3)",
                params![actor_id, run_id, alias],
            )?;
            return Ok(alias);
        }
    }
    bail!("could not allocate a unique short alias for authorized task")
}

fn apply_command(
    transaction: &Transaction<'_>,
    actor_id: &str,
    request_id: &str,
    now: i64,
    command: &Command,
) -> Result<CommandApply> {
    match command {
        Command::ListTasks => {
            let tasks = authorized_runs_in_transaction(transaction, actor_id)?;
            Ok(CommandApply::Applied {
                result: json!({"tasks":tasks}),
                task_id: None,
            })
        }
        Command::Message { text, task_id } => {
            let Some(run_id) = resolve_actor_task(transaction, actor_id, task_id.as_deref())?
            else {
                return Ok(CommandApply::NeedsTaskCreation);
            };
            let state = run_state(transaction, &run_id)?;
            if task_id.is_none() && is_terminal(&state) {
                // A chat bound to a finished task starts a fresh local task on
                // the next ordinary message. Explicit task targets still fail
                // below, and status can continue to inspect the old task.
                return Ok(CommandApply::NeedsTaskCreation);
            }
            let text = text.replace("\r\n", "\n").replace('\r', "\n");
            let text = crate::text::clean(&text).trim().to_owned();
            if text.is_empty() || text.len() > MAX_MESSAGE_BYTES {
                return Err(reject_command(
                    "remote message must be nonempty and at most 65536 UTF-8 bytes",
                ));
            }
            if !matches!(state.as_str(), "ready" | "running") {
                return Err(reject_command(
                    "task is no longer active; message was not queued",
                ));
            }
            append_event(
                transaction,
                &run_id,
                "user.steering",
                json!({"text":text,"source":"remote","actor_id":actor_id,"request_id":request_id}),
            )?;
            let target: Option<String> = transaction
                .query_row(
                    "SELECT CAST(seq AS TEXT) FROM events WHERE run_id=?1 AND kind='model.started' AND seq > COALESCE((SELECT MAX(seq) FROM events WHERE run_id=?1 AND kind IN ('model.response','model.failed')),0) ORDER BY seq DESC LIMIT 1",
                    [&run_id],
                    |row| row.get(0),
                )
                .optional()?;
            let interrupted = if let Some(target) = target {
                let changed = transaction.execute(
                    "INSERT OR IGNORE INTO interrupts(run_id,scope,target) VALUES (?1,'model',?2)",
                    params![run_id, target],
                )?;
                if changed == 1 {
                    append_event(
                        transaction,
                        &run_id,
                        "interrupt.requested",
                        json!({"scope":"model","target":target,"source":"remote_steering","actor_id":actor_id}),
                    )?;
                }
                true
            } else {
                false
            };
            Ok(applied_for_task(
                &run_id,
                json!({"accepted":true,"interrupting_model":interrupted}),
            ))
        }
        Command::Status { task_id } => {
            let run_id = require_resolved_task(transaction, actor_id, task_id.as_deref())?;
            let (task, provider, state, created_at): (String, String, String, i64) = transaction
                .query_row(
                    "SELECT task,provider,state,created_at FROM runs WHERE id=?1",
                    [&run_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )?;
            let (started_at, summary): (Option<i64>, Option<String>) = transaction.query_row(
                "SELECT started_at,summary FROM run_projection WHERE run_id=?1",
                [&run_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let selected: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM remote_selected_tasks WHERE actor_id=?1 AND run_id=?2)",
                params![actor_id, run_id],
                |row| row.get(0),
            )?;
            let task = redact_remote_text(&task)
                .chars()
                .take(300)
                .collect::<String>();
            let summary = summary.map(|value| {
                redact_remote_text(&value)
                    .chars()
                    .take(300)
                    .collect::<String>()
            });
            let alias = task_alias(transaction, actor_id, &run_id)?;
            Ok(applied_for_task(
                &run_id,
                json!({"alias":alias,"task":task,"provider":provider,"state":state,"created_at":created_at,"started_at":started_at,"summary":summary,"selected":selected}),
            ))
        }
        Command::Result { task_id } => {
            let run_id = require_resolved_task(transaction, actor_id, task_id.as_deref())?;
            let (state, summary): (String, Option<String>) = transaction.query_row(
                "SELECT run.state,projection.summary FROM runs AS run
                 LEFT JOIN run_projection AS projection ON projection.run_id=run.id
                 WHERE run.id=?1",
                [&run_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if !is_terminal(&state) {
                return Err(reject_command("task has no final result yet"));
            }
            let summary = summary
                .map(|value| redact_remote_text(&value))
                .map(|value| {
                    crate::remote::protocol::truncate_utf8(&value, MAX_REMOTE_RESULT_BYTES)
                })
                .filter(|value| !value.trim().is_empty());
            let verification = match state.as_str() {
                "completed" => "verified",
                "answered" => "unverified",
                _ => "not_verified",
            };
            let alias = task_alias(transaction, actor_id, &run_id)?;
            Ok(applied_for_task(
                &run_id,
                json!({"alias":alias,"state":state,"summary":summary,"verification":verification}),
            ))
        }
        Command::Evidence { task_id } => {
            let run_id = require_resolved_task(transaction, actor_id, task_id.as_deref())?;
            let state = run_state(transaction, &run_id)?;
            let alias = task_alias(transaction, actor_id, &run_id)?;
            if state == "answered" {
                return Ok(applied_for_task(
                    &run_id,
                    json!({"alias":alias,"state":state,"task_completed":false,"receipts":[],"omitted":0}),
                ));
            }
            let revision: Option<i64> = transaction
                .query_row(
                    "SELECT revision FROM workspace_revisions WHERE run_id=?1",
                    [&run_id],
                    |row| row.get(0),
                )
                .optional()?;
            let mut receipts = Vec::new();
            let mut omitted = 0usize;
            if let Some(revision) = revision {
                let evidence = {
                    let mut statement = transaction.prepare(
                        "SELECT DISTINCT evidence.value FROM obligations AS obligation,
                         json_each(obligation.evidence) AS evidence
                         WHERE obligation.run_id=?1 AND obligation.state='verified'
                         AND obligation.verified_revision=?2
                         ORDER BY evidence.value",
                    )?;
                    statement
                        .query_map(params![run_id, revision], |row| row.get::<_, String>(0))?
                        .collect::<rusqlite::Result<Vec<_>>>()?
                };
                for hash in evidence {
                    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                        continue;
                    }
                    let source: Option<(String, i64)> = transaction
                        .query_row(
                            "SELECT operation.capability, artifact.bytes
                             FROM operations AS operation
                             JOIN operation_revisions AS operation_revision
                               ON operation_revision.operation_id=operation.id
                              AND operation_revision.run_id=operation.run_id
                             JOIN artifacts AS artifact ON artifact.hash=?3
                             WHERE operation.run_id=?1 AND operation.state='succeeded'
                               AND operation_revision.revision=?2
                               AND (operation.artifact=?3 OR EXISTS(
                                 SELECT 1 FROM operation_artifacts AS linked
                                 WHERE linked.operation_id=operation.id AND linked.hash=?3
                               ))
                             ORDER BY operation.rowid LIMIT 1",
                            params![run_id, revision, hash],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )
                        .optional()?;
                    let Some((capability, bytes)) = source else {
                        continue;
                    };
                    if receipts.len() == MAX_REMOTE_EVIDENCE_RECEIPTS {
                        omitted = omitted.saturating_add(1);
                        continue;
                    }
                    receipts.push(json!({
                        "capability":safe_remote_capability(&capability),
                        "hash_prefix":hash.chars().take(12).collect::<String>(),
                        "bytes":bytes.max(0),
                    }));
                }
            }
            Ok(applied_for_task(
                &run_id,
                json!({"alias":alias,"state":state,"task_completed":state == "completed","receipts":receipts,"omitted":omitted}),
            ))
        }
        Command::Details { task_id } => {
            let run_id = if let Some(reference) = task_id.as_deref() {
                resolve_actor_task_reference(transaction, actor_id, reference)?
            } else {
                resolve_actor_task(transaction, actor_id, None)?
                    .ok_or_else(|| reject_command("select or specify a task first"))?
            };
            let details = task_details(transaction, actor_id, &run_id, now)?;
            Ok(applied_for_task(&run_id, details))
        }
        Command::Pause { task_id } => {
            let run_id = require_resolved_task(transaction, actor_id, task_id.as_deref())?;
            let state = run_state(transaction, &run_id)?;
            if is_terminal(&state) {
                return Err(reject_command("ended tasks cannot pause"));
            }
            let pending: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM pause_requests WHERE run_id=?1 AND pending=1)",
                [&run_id],
                |row| row.get(0),
            )?;
            if !pending {
                transaction.execute(
                    "INSERT INTO pause_requests(run_id,pending) VALUES (?1,1) ON CONFLICT(run_id) DO UPDATE SET pending=1",
                    [&run_id],
                )?;
                append_event(
                    transaction,
                    &run_id,
                    "pause.requested",
                    json!({"source":"remote","actor_id":actor_id,"request_id":request_id,"boundary":"after the current action records its outcome"}),
                )?;
            }
            Ok(applied_for_task(
                &run_id,
                json!({"requested":true,"state":state}),
            ))
        }
        Command::Resume { task_id } => {
            let run_id = require_resolved_task(transaction, actor_id, task_id.as_deref())?;
            let state = run_state(transaction, &run_id)?;
            if is_terminal(&state) {
                return Err(reject_command("ended tasks cannot resume"));
            }
            crate::obligations::ensure_reviewed_contract(transaction, &run_id)
                .map_err(|error| reject_command(error.to_string()))?;
            let unknown: i64 = transaction.query_row(
                "SELECT COUNT(*) FROM operations WHERE run_id=?1 AND state='outcome_unknown'",
                [&run_id],
                |row| row.get(0),
            )?;
            if unknown > 0 {
                return Err(reject_command(
                    "uncertain operation outcomes need local reconciliation before resume",
                ));
            }
            append_event(
                transaction,
                &run_id,
                "pause.resumed",
                json!({"source":"remote","actor_id":actor_id,"request_id":request_id}),
            )?;
            transaction.execute(
                "UPDATE pause_requests SET pending=0 WHERE run_id=?1",
                [&run_id],
            )?;
            let resumed = matches!(state.as_str(), "paused" | "waiting_recovery");
            if resumed {
                transaction.execute("UPDATE runs SET state='ready' WHERE id=?1", [&run_id])?;
                append_event(
                    transaction,
                    &run_id,
                    "run.ready",
                    json!({"source":"remote_resume"}),
                )?;
            }
            Ok(applied_for_task(
                &run_id,
                json!({"resumed":true,"state":if resumed {"ready"} else {state.as_str()},"kernel_restart_required":resumed}),
            ))
        }
        Command::Cancel { task_id } => {
            let run_id = require_resolved_task(transaction, actor_id, task_id.as_deref())?;
            let state = run_state(transaction, &run_id)?;
            if is_terminal(&state) {
                return Err(reject_command("ended tasks cannot be cancelled"));
            }
            transaction.execute("UPDATE runs SET state='cancelled' WHERE id=?1", [&run_id])?;
            transaction.execute(
                "UPDATE pause_requests SET pending=0 WHERE run_id=?1",
                [&run_id],
            )?;
            append_event(
                transaction,
                &run_id,
                "run.cancelled",
                json!({"source":"remote","actor_id":actor_id,"request_id":request_id}),
            )?;
            Ok(applied_for_task(&run_id, json!({"cancelled":true})))
        }
        Command::SelectTask { task_id } => {
            let run_id = resolve_actor_task_reference(transaction, actor_id, task_id)?;
            let alias = task_alias(transaction, actor_id, &run_id)?;
            transaction.execute(
                "INSERT INTO remote_selected_tasks(actor_id,run_id,selected_at) VALUES (?1,?2,?3)
                 ON CONFLICT(actor_id) DO UPDATE SET run_id=excluded.run_id,selected_at=excluded.selected_at",
                params![actor_id, run_id, crate::storage::unix_time()],
            )?;
            Ok(applied_for_task(
                &run_id,
                json!({"selected":true,"alias":alias}),
            ))
        }
        Command::ApproveOnce { challenge_id } | Command::Deny { challenge_id } => {
            validate_uuid(challenge_id, "approval challenge ID")?;
            let wanted = if matches!(command, Command::ApproveOnce { .. }) {
                "approved_once"
            } else {
                "denied"
            };
            let current: Option<(String, String, String, String, i64)> = transaction.query_row(
                "SELECT run_id,operation_id,intent_hash,state,expires_at FROM remote_operation_gates WHERE challenge_id=?1",
                [challenge_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            ).optional()?;
            let Some((run_id, operation_id, intent_hash, gate_state, expires_at)) = current else {
                return Err(reject_command("approval challenge is unknown"));
            };
            require_actor_run(transaction, actor_id, &run_id)?;
            if matches!(command, Command::ApproveOnce { .. }) {
                crate::obligations::ensure_reviewed_contract(transaction, &run_id)
                    .map_err(|error| reject_command(error.to_string()))?;
            }
            if expires_at <= now {
                return Err(reject_command("approval challenge has expired"));
            }
            if gate_state != "pending" {
                return Err(reject_command(
                    "approval challenge has already been resolved",
                ));
            }
            let operation = operation_in_transaction(transaction, &run_id, &operation_id)?;
            if operation.state != "pending" {
                return Err(reject_command(
                    "approval challenge no longer refers to a pending operation",
                ));
            }
            if operation_intent_hash(&operation)? != intent_hash {
                return Err(reject_command(
                    "the pending operation changed after the local approval gate was created",
                ));
            }
            let changed = transaction.execute(
                "UPDATE remote_operation_gates SET state=?2,decision_actor=?3,decision_request=?4,decided_at=?5 WHERE challenge_id=?1 AND state='pending'",
                params![challenge_id, wanted, actor_id, request_id, now],
            )?;
            if changed != 1 {
                return Err(reject_command(
                    "approval challenge was concurrently resolved",
                ));
            }
            append_event(
                transaction,
                &run_id,
                if wanted == "approved_once" {
                    "approval.approved_once"
                } else {
                    "approval.denied"
                },
                json!({"challenge_id":challenge_id,"operation_id":operation_id,"actor_id":actor_id,"request_id":request_id,"capability":operation.capability,"decision":wanted}),
            )?;
            Ok(applied_for_task(
                &run_id,
                json!({"recorded":true,"challenge_id":challenge_id,"decision":wanted}),
            ))
        }
    }
}

fn applied_for_task(task_id: &str, mut result: Value) -> CommandApply {
    if let Value::Object(object) = &mut result {
        object.insert("task_id".to_owned(), Value::String(task_id.to_owned()));
    }
    CommandApply::Applied {
        result,
        task_id: Some(task_id.to_owned()),
    }
}

fn require_resolved_task(
    transaction: &Transaction<'_>,
    actor_id: &str,
    requested: Option<&str>,
) -> Result<String> {
    if let Some(reference) = requested {
        return resolve_actor_task_reference(transaction, actor_id, reference);
    }
    resolve_actor_task(transaction, actor_id, None)?
        .ok_or_else(|| reject_command("select or specify a task first"))
}

fn resolve_actor_task(
    transaction: &Transaction<'_>,
    actor_id: &str,
    requested: Option<&str>,
) -> Result<Option<String>> {
    let task_id = if let Some(task_id) = requested {
        Some(task_id.to_owned())
    } else {
        transaction
            .query_row(
                "SELECT run_id FROM remote_selected_tasks WHERE actor_id=?1",
                [actor_id],
                |row| row.get(0),
            )
            .optional()?
    };
    if let Some(task_id) = task_id {
        require_actor_run(transaction, actor_id, &task_id)?;
        Ok(Some(task_id))
    } else {
        Ok(None)
    }
}

/// Resolve a phone-supplied UUID or short alias only against the authenticated
/// actor's currently enabled task grants. Aliases are never searched globally.
fn resolve_actor_task_reference(
    transaction: &Transaction<'_>,
    actor_id: &str,
    reference: &str,
) -> Result<String> {
    if let Ok(parsed) = uuid::Uuid::parse_str(reference) {
        let run_id = parsed.to_string();
        require_actor_run(transaction, actor_id, &run_id)?;
        return Ok(run_id);
    }
    if reference.is_empty()
        || reference.len() > 64
        || !reference
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(reject_command(
            "remote actor is not authorized for this task",
        ));
    }
    let run_id: Option<String> = transaction
        .query_row(
            "SELECT aliases.run_id FROM remote_task_aliases AS aliases
             JOIN remote_actor_runs AS access
               ON access.actor_id=aliases.actor_id AND access.run_id=aliases.run_id
             JOIN remote_actors AS actor ON actor.actor_id=access.actor_id
             WHERE aliases.actor_id=?1 AND aliases.alias=?2
               AND access.enabled=1 AND actor.enabled=1",
            params![actor_id, reference.to_ascii_lowercase()],
            |row| row.get(0),
        )
        .optional()?;
    let Some(run_id) = run_id else {
        return Err(reject_command(
            "remote actor is not authorized for this task",
        ));
    };
    // Re-check the grant before returning the resolved ID so a malformed or
    // stale alias record cannot bypass the ordinary actor/run guard.
    require_actor_run(transaction, actor_id, &run_id)?;
    Ok(run_id)
}

fn task_alias(transaction: &Transaction<'_>, actor_id: &str, run_id: &str) -> Result<String> {
    require_actor_run(transaction, actor_id, run_id)?;
    transaction
        .query_row(
            "SELECT alias FROM remote_task_aliases WHERE actor_id=?1 AND run_id=?2",
            params![actor_id, run_id],
            |row| row.get(0),
        )
        .context("authorized task has no short alias")
}

fn safe_remote_capability(capability: &str) -> String {
    let value = crate::text::clean(capability)
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
        })
        .take(64)
        .collect::<String>();
    if value.is_empty() {
        "operation".into()
    } else {
        value
    }
}

/// Defense in depth for free-form remote exports. Recognizes common credential
/// formats and explicitly labeled secret assignments; it cannot identify an
/// arbitrary unnamed secret, encoded credentials, or every provider format.
/// Always redact the complete text before applying the caller's export limit.
fn redact_remote_text(text: &str) -> String {
    let text = crate::text::clean(
        &text
            .replace("\r\n", "\n")
            .replace('\t', " ")
            .replace('\r', "\n"),
    );
    let bytes = text.as_bytes();
    let mut output = String::with_capacity(text.len());
    let mut copied = 0;
    let mut index = 0;
    while index < bytes.len() {
        let mut secret = None;
        if text[index..].starts_with("-----BEGIN ") {
            let label_start = index + "-----BEGIN ".len();
            if let Some(length) = text[label_start..].find("-----") {
                let label = &text[label_start..label_start + length];
                if label.ends_with("PRIVATE KEY")
                    && label
                        .bytes()
                        .all(|byte| byte.is_ascii_uppercase() || byte == b' ')
                {
                    let end_marker = format!("-----END {label}-----");
                    let body_start = label_start + length + 5;
                    let end = text[body_start..]
                        .find(&end_marker)
                        .map_or(text.len(), |offset| body_start + offset + end_marker.len());
                    // An unterminated private-key block also hides its remaining body.
                    secret = Some((index, end));
                }
            }
        }
        if secret.is_none() && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_') {
            let mut word_end = index
                + bytes[index..]
                    .iter()
                    .take_while(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                    .count();
            let word = &text[index..word_end];
            let mut key = word.to_ascii_lowercase().replace('-', "_");
            for (first, second) in [
                ("api", "key"),
                ("access", "token"),
                ("refresh", "token"),
                ("client", "secret"),
                ("private", "key"),
            ] {
                if key == first {
                    let next = text[word_end..].trim_start_matches(' ');
                    if next
                        .get(..second.len())
                        .is_some_and(|word| word.eq_ignore_ascii_case(second))
                        && !next
                            .as_bytes()
                            .get(second.len())
                            .is_some_and(u8::is_ascii_alphanumeric)
                    {
                        word_end = text.len() - next.len() + second.len();
                        key = format!("{first}_{second}");
                        break;
                    }
                }
            }
            let mut value_start = word_end;
            if matches!(bytes.get(value_start), Some(b'\'' | b'"' | b'`')) {
                value_start += 1;
            }
            while matches!(bytes.get(value_start), Some(b' ' | b'\t')) {
                value_start += 1;
            }
            let secret_key = matches!(
                key.as_str(),
                "api_key"
                    | "apikey"
                    | "token"
                    | "access_token"
                    | "accesstoken"
                    | "refresh_token"
                    | "refreshtoken"
                    | "secret"
                    | "secret_key"
                    | "password"
                    | "passwd"
                    | "passphrase"
                    | "pwd"
                    | "authorization"
                    | "credential"
                    | "credentials"
                    | "client_secret"
                    | "clientsecret"
                    | "private_key"
                    | "privatekey"
            ) || [
                "_api_key",
                "_token",
                "_secret",
                "_secret_key",
                "_password",
                "_access_key",
                "_private_key",
            ]
            .iter()
            .any(|suffix| key.ends_with(suffix));
            if secret_key && matches!(bytes.get(value_start), Some(b'=' | b':')) {
                value_start += 1;
                while matches!(bytes.get(value_start), Some(b' ' | b'\t')) {
                    value_start += 1;
                }
                let (start, mut end) = remote_secret_value(&text, value_start);
                if key == "authorization"
                    && matches!(
                        text[start..end].to_ascii_lowercase().as_str(),
                        "bearer" | "basic"
                    )
                {
                    let mut next = end;
                    while matches!(bytes.get(next), Some(b' ' | b'\t')) {
                        next += 1;
                    }
                    end = remote_secret_value(&text, next).1;
                }
                if end > start {
                    secret = Some((start, end));
                }
            } else if key == "bearer" && bytes.get(word_end) == Some(&b' ') {
                let mut start = word_end;
                while bytes.get(start) == Some(&b' ') {
                    start += 1;
                }
                let (start, end) = remote_secret_value(&text, start);
                let token = &text[start..end];
                // Avoid treating ordinary phrases such as "bearer bonds" or
                // "Bearer authentication" as a credential.
                if token.len() >= 20
                    || token.bytes().any(|byte| {
                        byte.is_ascii_digit()
                            || matches!(byte, b'_' | b'-' | b'.' | b'+' | b'/' | b'=')
                    })
                {
                    secret = Some((start, end));
                }
            }
            if secret.is_none() {
                let token_end = index
                    + bytes[index..]
                        .iter()
                        .take_while(|byte| {
                            byte.is_ascii_alphanumeric()
                                || matches!(byte, b'_' | b'-' | b'.' | b'+' | b'/' | b'=')
                        })
                        .count();
                let token = text[index..token_end].trim_end_matches('.');
                if remote_credential_token(token) {
                    secret = Some((index, index + token.len()));
                } else {
                    index = token_end;
                    continue;
                }
            }
        }
        if let Some((start, end)) = secret {
            output.push_str(&text[copied..start]);
            output.push_str("[redacted]");
            copied = end;
            index = end;
        } else {
            index += text[index..].chars().next().unwrap().len_utf8();
        }
    }
    output.push_str(&text[copied..]);
    output
}

fn remote_secret_value(text: &str, start: usize) -> (usize, usize) {
    let bytes = text.as_bytes();
    if let Some(quote @ (b'\'' | b'"' | b'`')) = bytes.get(start) {
        let mut end = start + 1;
        while end < bytes.len() && bytes[end] != *quote {
            if bytes[end] == b'\\' && end + 1 < bytes.len() {
                end += 1;
            }
            end += 1;
        }
        (start + 1, end)
    } else {
        let length = text[start..]
            .find(|character: char| {
                character.is_whitespace()
                    || matches!(character, ',' | ';' | '"' | '\'' | '`' | '}' | ')')
            })
            .unwrap_or(text.len() - start);
        (start, start + length)
    }
}

fn remote_credential_token(token: &str) -> bool {
    let prefixed = [
        "sk-",
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "glpat-",
        "xoxb-",
        "xoxp-",
        "xoxa-",
        "xoxr-",
        "xoxs-",
        "npm_",
    ]
    .iter()
    .any(|prefix| {
        token
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.len() >= 8)
    });
    let aws = (token.starts_with("AKIA") || token.starts_with("ASIA"))
        && token.len() == 20
        && token
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit());
    let google = token.starts_with("AIza") && token.len() >= 35;
    let parts = token.split('.').collect::<Vec<_>>();
    let jwt = parts.len() == 3
        && parts[0].starts_with("eyJ")
        && parts[0].len() >= 8
        && parts[1].len() >= 4
        && parts[2].len() >= 8
        && parts.iter().all(|part| {
            part.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        });
    prefixed || aws || google || jwt
}

fn redact_cached_remote_text(result: &mut Value, command: &Command) {
    fn field(result: &mut Value, key: &str, characters: usize) {
        if let Some(value) = result.get_mut(key).filter(|value| value.is_string()) {
            *value = Value::String(
                redact_remote_text(value.as_str().unwrap())
                    .chars()
                    .take(characters)
                    .collect(),
            );
        }
    }
    // Replies persisted by older versions must cross the same export boundary.
    match command {
        Command::Result { .. } => {
            if let Some(value) = result.get_mut("summary").filter(|value| value.is_string()) {
                *value = Value::String(protocol::truncate_utf8(
                    &redact_remote_text(value.as_str().unwrap()),
                    MAX_REMOTE_RESULT_BYTES,
                ));
            }
        }
        Command::Status { .. } => {
            field(result, "summary", 300);
            field(result, "task", 300);
        }
        Command::Details { .. } => {
            field(result, "task", 240);
            field(result, "model", 100);
        }
        Command::ListTasks => {
            if let Some(tasks) = result.get_mut("tasks").and_then(Value::as_array_mut) {
                for task in tasks {
                    field(task, "task", 300);
                }
            }
        }
        _ => {}
    }
}

/// Build a bounded status report from task metadata and aggregate counters.
/// Event payloads, artifacts, operation arguments and tool output are excluded.
fn task_details(
    transaction: &Transaction<'_>,
    actor_id: &str,
    run_id: &str,
    now: i64,
) -> Result<Value> {
    require_actor_run(transaction, actor_id, run_id)?;
    let row: (
        String,
        String,
        String,
        i64,
        Option<String>,
        Option<i64>,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
    ) = transaction.query_row(
        "SELECT run.task,run.state,run.provider,run.created_at,
         json_extract(run.budgets,'$.model'),projection.started_at,
         COALESCE(projection.model_tokens,0),
         (SELECT COUNT(*) FROM events WHERE run_id=run.id AND kind='model.response'),
         (SELECT COUNT(*) FROM operations WHERE run_id=run.id AND state='succeeded'),
         (SELECT COUNT(*) FROM operations WHERE run_id=run.id AND state IN ('pending','dispatched','executing')),
         (SELECT COUNT(*) FROM operations WHERE run_id=run.id AND state='outcome_unknown'),
         (SELECT COUNT(*) FROM obligations WHERE run_id=run.id AND id>0 AND state='open'),
         (SELECT COUNT(*) FROM obligations WHERE run_id=run.id AND id>0 AND state='verified'),
         (SELECT COUNT(*) FROM obligations WHERE run_id=run.id AND id>0 AND state='stale'),
         (SELECT COUNT(*) FROM obligations WHERE run_id=run.id AND id>0 AND state='superseded')
         FROM runs AS run LEFT JOIN run_projection AS projection ON projection.run_id=run.id
         WHERE run.id=?1",
        [run_id],
        |row| {
            Ok((
                row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?,
                row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?, row.get(9)?,
                row.get(10)?,
                row.get(11)?, row.get(12)?, row.get(13)?, row.get(14)?,
            ))
        },
    )?;
    let selected: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM remote_selected_tasks WHERE actor_id=?1 AND run_id=?2)",
        params![actor_id, run_id],
        |row| row.get(0),
    )?;
    let terminal_at: Option<i64> = transaction.query_row(
        "SELECT MAX(created_at) FROM events WHERE run_id=?1 AND kind IN ('run.completed','run.answered','run.failed','run.cancelled')",
        [run_id],
        |row| row.get(0),
    )?;
    let elapsed_seconds = row
        .5
        .map(|started| terminal_at.unwrap_or(now).saturating_sub(started).max(0));
    let task = redact_remote_text(&row.0)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(240)
        .collect::<String>();
    let model = row.4.filter(|value| !value.trim().is_empty()).map(|value| {
        redact_remote_text(&value)
            .chars()
            .take(100)
            .collect::<String>()
    });
    let alias = task_alias(transaction, actor_id, run_id)?;
    Ok(json!({
        "alias":alias,
        "task":task,
        "state":row.1,
        "provider":row.2,
        "model":model,
        "created_at":row.3,
        "started_at":row.5,
        "elapsed_seconds":elapsed_seconds,
        "model_tokens":row.6,
        "model_turns":row.7,
        "successful_actions":row.8,
        "pending_actions":row.9,
        "uncertain_actions":row.10,
        "obligations":{
            "open":row.11,
            "verified":row.12,
            "stale":row.13,
            "superseded":row.14,
        },
        "selected":selected,
    }))
}

fn require_actor_run(transaction: &Transaction<'_>, actor_id: &str, run_id: &str) -> Result<()> {
    let allowed: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM remote_actor_runs AS access
         JOIN remote_actors AS actor ON actor.actor_id=access.actor_id
         WHERE access.actor_id=?1 AND access.run_id=?2 AND access.enabled=1 AND actor.enabled=1)",
        params![actor_id, run_id],
        |row| row.get(0),
    )?;
    if !allowed {
        return Err(reject_command(
            "remote actor is not authorized for this task",
        ));
    }
    Ok(())
}

fn require_actor_run_connection(
    connection: &rusqlite::Connection,
    actor_id: &str,
    run_id: &str,
) -> Result<()> {
    let allowed: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM remote_actor_runs AS access
         JOIN remote_actors AS actor ON actor.actor_id=access.actor_id
         WHERE access.actor_id=?1 AND access.run_id=?2 AND access.enabled=1 AND actor.enabled=1)",
        params![actor_id, run_id],
        |row| row.get(0),
    )?;
    if !allowed {
        bail!("remote actor is not authorized for this task");
    }
    Ok(())
}

fn authorized_runs_in_transaction(
    transaction: &Transaction<'_>,
    actor_id: &str,
) -> Result<Vec<Value>> {
    let mut statement = transaction.prepare(
        "SELECT run.id,run.task,run.state,run.provider,run.created_at,aliases.alias,
         EXISTS(SELECT 1 FROM remote_selected_tasks AS selected WHERE selected.actor_id=?1 AND selected.run_id=run.id)
         FROM remote_actor_runs AS access JOIN runs AS run ON run.id=access.run_id
         JOIN remote_actors AS actor ON actor.actor_id=access.actor_id
         JOIN remote_task_aliases AS aliases ON aliases.actor_id=access.actor_id AND aliases.run_id=run.id
         WHERE access.actor_id=?1 AND access.enabled=1 AND actor.enabled=1 ORDER BY run.created_at DESC LIMIT 100",
    )?;
    let rows = statement.query_map([actor_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, bool>(6)?,
        ))
    })?;
    let mut tasks = Vec::new();
    for row in rows {
        let (id, task, state, provider, created_at, alias, selected) = row?;
        tasks.push(json!({"task_id":id,"alias":alias,"task":redact_remote_text(&task).chars().take(300).collect::<String>(),"state":state,"provider":provider,"created_at":created_at,"selected":selected}));
    }
    Ok(tasks)
}

fn run_state(transaction: &Transaction<'_>, run_id: &str) -> Result<String> {
    Ok(
        transaction.query_row("SELECT state FROM runs WHERE id=?1", [run_id], |row| {
            row.get(0)
        })?,
    )
}

fn is_terminal(state: &str) -> bool {
    matches!(state, "completed" | "answered" | "failed" | "cancelled")
}

fn operation_intent_hash(operation: &crate::storage::Operation) -> Result<String> {
    let material = json!({
        "id":operation.id,
        "run_id":operation.run_id,
        "capability":operation.capability,
        "capability_version":operation.capability_version,
        "arguments":operation.arguments,
        "idempotency_key":operation.idempotency_key,
        "retry_safe":operation.retry_safe,
    });
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(&material)?)))
}

fn validate_request_shape(request: &CommandEnvelope) -> Result<()> {
    if request.version != PROTOCOL_VERSION {
        bail!("unsupported remote command protocol version");
    }
    validate_uuid(&request.installation_id, "installation ID")?;
    validate_actor_id(&request.actor_id)?;
    validate_uuid(&request.request_id, "request ID")?;
    if let Command::Message { text, task_id } = &request.command {
        if let Some(task_id) = task_id {
            validate_uuid(task_id, "task ID")?;
        }
        if text.is_empty() || text.len() > MAX_MESSAGE_BYTES {
            bail!("remote message must be nonempty and at most 65536 UTF-8 bytes");
        }
    }
    if let Some(task_id) = match &request.command {
        Command::Status { task_id }
        | Command::Result { task_id }
        | Command::Evidence { task_id }
        | Command::Details { task_id }
        | Command::Pause { task_id }
        | Command::Resume { task_id }
        | Command::Cancel { task_id } => task_id.as_deref(),
        _ => None,
    } {
        validate_task_reference(task_id)?;
    }
    if let Command::SelectTask { task_id } = &request.command {
        validate_task_reference(task_id)?;
    }
    if let Command::ApproveOnce { challenge_id } | Command::Deny { challenge_id } = &request.command
    {
        validate_uuid(challenge_id, "approval challenge ID")?;
    }
    request.canonical_bytes()?;
    Ok(())
}

fn validate_request_time(request: &CommandEnvelope, now: i64) -> Result<()> {
    if request.issued_at <= 0
        || request.expires_at <= now
        || request.issued_at > now.saturating_add(CLOCK_SKEW_SECONDS)
        || request.expires_at <= request.issued_at
        || request.expires_at.saturating_sub(request.issued_at) > MAX_COMMAND_TTL_SECONDS
    {
        bail!("remote command is expired or has an invalid time window");
    }
    Ok(())
}

fn validate_actor_id(actor_id: &str) -> Result<()> {
    if actor_id.is_empty()
        || actor_id.len() > 128
        || !actor_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        bail!("remote actor ID must be 1–128 ASCII letters, digits, '.', '_' or '-'");
    }
    Ok(())
}

fn validate_uuid(value: &str, label: &str) -> Result<()> {
    uuid::Uuid::parse_str(value).with_context(|| format!("invalid remote {label}"))?;
    Ok(())
}

fn validate_task_reference(reference: &str) -> Result<()> {
    if uuid::Uuid::parse_str(reference).is_ok() {
        return Ok(());
    }
    if reference.is_empty()
        || reference.len() > 64
        || !reference
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        bail!("invalid remote task ID or short alias");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_now() -> i64 {
        crate::storage::unix_time()
    }

    struct Fixture {
        directory: tempfile::TempDir,
        authority: Authority,
        run_id: String,
        actor_id: String,
    }

    impl Fixture {
        fn new() -> Result<Self> {
            let directory = tempfile::tempdir()?;
            let mut store = Store::open(directory.path())?;
            let run = store.create_run(
                "repair a parser",
                directory.path(),
                "codex",
                json!(["workspace.read"]),
                json!({}),
                "tests pass",
            )?;
            let mut authority = Authority::from_store(store)?;
            let actor_id = "phone-1".to_owned();
            authority.pair_actor(&actor_id, std::slice::from_ref(&run.id))?;
            Ok(Self {
                directory,
                authority,
                run_id: run.id,
                actor_id,
            })
        }

        fn request(&self, command: Command) -> CommandEnvelope {
            let now = test_now();
            CommandEnvelope {
                version: PROTOCOL_VERSION,
                installation_id: self.authority.installation_id().to_owned(),
                actor_id: self.actor_id.clone(),
                request_id: uuid::Uuid::new_v4().to_string(),
                issued_at: now,
                expires_at: now + 60,
                command,
            }
        }

        fn apply(&mut self, command: Command) -> Result<Receipt> {
            let request = self.request(command);
            self.authority
                .apply_from_authenticated_relay(&self.actor_id, &request, test_now())
        }
    }

    fn complete_with_verified_evidence(
        fixture: &mut Fixture,
        summary: &str,
        referenced_count: usize,
    ) -> Result<Vec<String>> {
        let run_id = fixture.run_id.clone();
        fixture
            .authority
            .store
            .state(&run_id, "paused", json!({}))?;
        let obligation = fixture.authority.store.add_obligation(
            &run_id,
            "review evidence",
            "verify current local receipts",
        )?;
        fixture
            .authority
            .store
            .state(&run_id, "running", json!({}))?;
        let mut evidence = Vec::new();
        for index in 0..referenced_count {
            let relative_path = format!("proof-{index}.txt");
            let source = format!("current evidence source {index}");
            std::fs::write(fixture.directory.path().join(&relative_path), &source)?;
            let source_hash = hex::encode(Sha256::digest(source.as_bytes()));
            let receipt_bytes = serde_json::to_vec(&json!({
                "path":relative_path,
                "sha256":source_hash,
                "bytes":source.len(),
                "stdout":"TOP_SECRET_TOOL_OUTPUT",
                "search_preview":"TOP_SECRET_SEARCH_PREVIEW",
                "arguments":"TOP_SECRET_ARGUMENTS"
            }))?;
            let receipt_hash = fixture.authority.store.put_artifact(&receipt_bytes)?;
            let operation = fixture.authority.store.begin_operation(
                &run_id,
                "workspace.read",
                json!({"path":relative_path,"private_argument":"TOP_SECRET_ARGUMENTS"}),
                false,
            )?;
            crate::storage::claim_test_operation(&mut fixture.authority.store, &operation)?;
            fixture.authority.store.operation_state(
                &operation,
                "succeeded",
                Some(&receipt_hash),
                json!({"output_preview":"TOP_SECRET_TOOL_OUTPUT"}),
            )?;
            evidence.push(receipt_hash);
        }

        // Successful but unreferenced evidence must not be returned to the phone.
        let unreferenced_path = "unreferenced.txt";
        let unreferenced_source = b"unreferenced source";
        std::fs::write(
            fixture.directory.path().join(unreferenced_path),
            unreferenced_source,
        )?;
        let unreferenced_bytes = serde_json::to_vec(&json!({
            "path":unreferenced_path,
            "sha256":hex::encode(Sha256::digest(unreferenced_source)),
            "bytes":unreferenced_source.len(),
            "stdout":"UNREFERENCED_SECRET"
        }))?;
        let unreferenced_hash = fixture.authority.store.put_artifact(&unreferenced_bytes)?;
        let unreferenced_operation = fixture.authority.store.begin_operation(
            &run_id,
            "workspace.read",
            json!({"path":unreferenced_path}),
            false,
        )?;
        crate::storage::claim_test_operation(
            &mut fixture.authority.store,
            &unreferenced_operation,
        )?;
        fixture.authority.store.operation_state(
            &unreferenced_operation,
            "succeeded",
            Some(&unreferenced_hash),
            json!({}),
        )?;

        // Failed artifacts are also excluded, even if still stored locally.
        let failed_hash = fixture
            .authority
            .store
            .put_artifact(b"TOP_SECRET_FAILED_ARTIFACT")?;
        let failed_operation = fixture.authority.store.begin_operation(
            &run_id,
            "workspace.read",
            json!({"path":"failed.txt"}),
            false,
        )?;
        fixture.authority.store.operation_state(
            &failed_operation,
            "failed",
            Some(&failed_hash),
            json!({"output_preview":"TOP_SECRET_FAILED_OUTPUT"}),
        )?;

        fixture
            .authority
            .store
            .verify_obligation(&run_id, obligation, &evidence)?;
        fixture
            .authority
            .store
            .complete_run(&run_id, summary, &evidence)?;
        Ok(evidence)
    }

    #[test]
    fn authenticated_command_is_expiry_checked_scoped_and_idempotent() -> Result<()> {
        let mut fixture = Fixture::new()?;
        let request = fixture.request(Command::Message {
            text: "Inspect parser first".into(),
            task_id: Some(fixture.run_id.clone()),
        });
        let first = fixture.authority.apply_from_authenticated_relay(
            &fixture.actor_id,
            &request,
            test_now(),
        )?;
        assert!(!first.duplicate);
        let second = fixture.authority.apply_from_authenticated_relay(
            &fixture.actor_id,
            &request,
            test_now(),
        )?;
        assert!(second.duplicate);
        assert_eq!(first.result, second.result);
        let retry_now = test_now();
        let retried_after_delivery_window = CommandEnvelope {
            issued_at: retry_now - 600,
            expires_at: retry_now - 1,
            ..request.clone()
        };
        let stale_duplicate = fixture.authority.apply_from_authenticated_relay(
            &fixture.actor_id,
            &retried_after_delivery_window,
            retry_now,
        )?;
        assert!(stale_duplicate.duplicate);
        assert_eq!(first.result, stale_duplicate.result);
        let events = fixture
            .authority
            .store
            .events(&fixture.run_id)?
            .into_iter()
            .filter(|event| event.kind == "user.steering")
            .count();
        assert_eq!(events, 1);

        let expired = CommandEnvelope {
            request_id: uuid::Uuid::new_v4().to_string(),
            issued_at: test_now() - 600,
            expires_at: test_now() - 1,
            ..request.clone()
        };
        assert!(
            fixture
                .authority
                .apply_from_authenticated_relay(&fixture.actor_id, &expired, test_now(),)
                .is_err()
        );

        let mut wrong_actor = request.clone();
        wrong_actor.request_id = uuid::Uuid::new_v4().to_string();
        wrong_actor.actor_id = "forged-actor".into();
        assert!(
            fixture
                .authority
                .apply_from_authenticated_relay(&fixture.actor_id, &wrong_actor, test_now(),)
                .is_err()
        );

        let mut wrong_install = request.clone();
        wrong_install.request_id = uuid::Uuid::new_v4().to_string();
        wrong_install.installation_id = uuid::Uuid::new_v4().to_string();
        assert!(
            fixture
                .authority
                .apply_from_authenticated_relay(&fixture.actor_id, &wrong_install, test_now(),)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn remote_text_redacts_recognized_credentials_and_preserves_prose() {
        let cases = [
            (
                "Authorization: Bearer abc123SECRET",
                "Authorization: [redacted]",
            ),
            ("bearer\tabc123SECRET", "bearer [redacted]"),
            ("Bearer \"abc123SECRET\"", "Bearer \"[redacted]\""),
            (
                "Authorization: Basic dXNlcjpwYXNz",
                "Authorization: [redacted]",
            ),
            ("API Key: private-value, next", "API Key: [redacted], next"),
            ("API_KEY=short; done", "API_KEY=[redacted]; done"),
            ("SECRET_KEY=opaque-value", "SECRET_KEY=[redacted]"),
            (
                r#"{"access_token":"value with spaces", "password":"a\"b"}"#,
                r#"{"access_token":"[redacted]", "password":"[redacted]"}"#,
            ),
            (
                "AWS_SECRET_ACCESS_KEY=arbitrary-value",
                "AWS_SECRET_ACCESS_KEY=[redacted]",
            ),
            ("clientSecret='short secret'", "clientSecret='[redacted]'"),
            (
                "JWT: eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMifQ.c2lnbmF0dXJl",
                "JWT: [redacted]",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(redact_remote_text(input), expected, "case: {input}");
            assert_eq!(
                redact_remote_text(expected),
                expected,
                "redaction is idempotent"
            );
        }
        for token in [
            "sk-proj-0123456789abcdef",
            "sk-ant-api03-0123456789abcdef",
            "ghp_0123456789abcdef",
            "gho_0123456789abcdef",
            "ghu_0123456789abcdef",
            "ghs_0123456789abcdef",
            "ghr_0123456789abcdef",
            "github_pat_0123456789abcdef",
            "glpat-0123456789abcdef",
            "xoxb-12345678-abcdefgh",
            "xoxp-12345678-abcdefgh",
            "npm_0123456789abcdef",
            "AKIA0123456789ABCDEF",
            "ASIA0123456789ABCDEF",
            "AIza0123456789abcdefghijklmnopqrstuvwxyz",
        ] {
            assert_eq!(
                redact_remote_text(&format!("Found `{token}`. Done.")),
                "Found `[redacted]`. Done."
            );
        }
        for label in [
            "PRIVATE KEY",
            "RSA PRIVATE KEY",
            "EC PRIVATE KEY",
            "OPENSSH PRIVATE KEY",
            "ENCRYPTED PRIVATE KEY",
        ] {
            let block = format!(
                "Before\n-----BEGIN {label}-----\nPRIVATEBODY\n-----END {label}-----\nAfter"
            );
            assert_eq!(redact_remote_text(&block), "Before\n[redacted]\nAfter");
            assert_eq!(
                redact_remote_text(&format!("Before\n-----BEGIN {label}-----\nPRIVATEBODY")),
                "Before\n[redacted]"
            );
        }
        let prose = "Parser fixed. 12 tests pass. Use Bearer authentication and rotate the API key.\nThe bearer bonds mature in 2028. 日本語 🛡 version 1.2.3.";
        assert_eq!(redact_remote_text(prose), prose);
        assert_eq!(redact_remote_text("Done.\r\nNext."), "Done.\nNext.");
        // Unlabeled arbitrary strings are deliberately outside this heuristic.
        assert_eq!(
            redact_remote_text("unnamed-private-value"),
            "unnamed-private-value"
        );
    }

    #[test]
    fn result_and_status_redact_before_limits_and_legacy_retries() -> Result<()> {
        let mut fixture = Fixture::new()?;
        let summary = format!(
            "API_KEY=\"{}\"\nAll tests pass. {}",
            "PRIVATEVALUE".repeat(200),
            "🛡".repeat(900)
        );
        fixture
            .authority
            .store
            .state(&fixture.run_id, "running", json!({}))?;
        fixture
            .authority
            .store
            .answer_run(&fixture.run_id, &summary)?;
        for command in [
            Command::Result {
                task_id: Some(fixture.run_id.clone()),
            },
            Command::Status {
                task_id: Some(fixture.run_id.clone()),
            },
        ] {
            let request = fixture.request(command.clone());
            let result = fixture.authority.apply_from_authenticated_relay(
                &fixture.actor_id,
                &request,
                test_now(),
            )?;
            let returned = result.result["summary"].as_str().unwrap();
            assert!(returned.starts_with("API_KEY=\"[redacted]\"\nAll tests pass."));
            assert!(!returned.contains("PRIVATEVALUE"));
            if matches!(command, Command::Result { .. }) {
                assert!(returned.len() <= MAX_REMOTE_RESULT_BYTES);
                assert!(returned.is_char_boundary(returned.len()));
            } else {
                assert!(returned.chars().count() <= 300);
            }
            let expected = result.result["summary"].clone();
            // Simulate a receipt cached before this export hardening existed.
            let mut legacy = result.result;
            legacy["summary"] = Value::String(summary.clone());
            fixture.authority.store.connection.execute(
                "UPDATE remote_requests SET result=?1 WHERE request_id=?2",
                params![legacy.to_string(), request.request_id],
            )?;
            let retry = fixture.authority.apply_from_authenticated_relay(
                &fixture.actor_id,
                &request,
                test_now(),
            )?;
            assert!(retry.duplicate);
            assert!(
                !retry.result["summary"]
                    .as_str()
                    .unwrap()
                    .contains("PRIVATEVALUE")
            );
            assert_eq!(retry.result["summary"], expected);
        }
        Ok(())
    }

    #[test]
    fn task_labels_are_redacted_in_remote_views() -> Result<()> {
        let mut fixture = Fixture::new()?;
        let task = format!(
            "Inspect API_KEY=\"{}\" then repair the parser.",
            "PRIVATEVALUE".repeat(100)
        );
        fixture.authority.store.connection.execute(
            "UPDATE runs SET task=?1 WHERE id=?2",
            params![task, fixture.run_id],
        )?;
        let expected = "Inspect API_KEY=\"[redacted]\" then repair the parser.";
        let listed = fixture.authority.authorized_runs(&fixture.actor_id)?;
        assert_eq!(listed[0]["task"], expected);
        let list = fixture.apply(Command::ListTasks)?;
        assert_eq!(list.result["tasks"][0]["task"], expected);
        for command in [
            Command::Status {
                task_id: Some(fixture.run_id.clone()),
            },
            Command::Details {
                task_id: Some(fixture.run_id.clone()),
            },
        ] {
            let result = fixture.apply(command)?;
            assert_eq!(result.result["task"], expected);
        }
        Ok(())
    }

    #[test]
    fn result_is_terminal_authorized_and_utf8_byte_bounded() -> Result<()> {
        let mut fixture = Fixture::new()?;
        let not_ready = fixture
            .apply(Command::Result {
                task_id: Some(fixture.run_id.clone()),
            })
            .unwrap_err();
        assert!(not_ready.to_string().contains("no final result"));

        let summary = "🛡".repeat(900);
        complete_with_verified_evidence(&mut fixture, &summary, 1)?;
        let result = fixture.apply(Command::Result {
            task_id: Some(fixture.run_id.clone()),
        })?;
        assert_eq!(result.result["state"], "completed");
        assert_eq!(result.result["verification"], "verified");
        let returned = result.result["summary"].as_str().unwrap();
        assert!(!returned.is_empty());
        assert!(returned.len() <= MAX_REMOTE_RESULT_BYTES);
        assert!(returned.is_char_boundary(returned.len()));
        assert!(summary.starts_with(returned));

        let other_actor = "other-phone";
        fixture.authority.register_actor(other_actor)?;
        for command in [
            Command::Result {
                task_id: Some(fixture.run_id.clone()),
            },
            Command::Evidence {
                task_id: Some(fixture.run_id.clone()),
            },
        ] {
            let request = CommandEnvelope {
                actor_id: other_actor.into(),
                request_id: uuid::Uuid::new_v4().to_string(),
                command,
                ..fixture.request(Command::ListTasks)
            };
            assert!(
                fixture
                    .authority
                    .apply_from_authenticated_relay(other_actor, &request, test_now())
                    .is_err()
            );
        }
        Ok(())
    }

    #[test]
    fn answered_result_is_unverified_and_has_no_tool_receipts() -> Result<()> {
        let mut fixture = Fixture::new()?;
        fixture
            .authority
            .store
            .state(&fixture.run_id, "running", json!({}))?;
        fixture
            .authority
            .store
            .answer_run(&fixture.run_id, "A concise conversational answer.")?;

        let result = fixture.apply(Command::Result {
            task_id: Some(fixture.run_id.clone()),
        })?;
        assert_eq!(result.result["state"], "answered");
        assert_eq!(result.result["verification"], "unverified");
        assert_eq!(result.result["summary"], "A concise conversational answer.");

        let evidence = fixture.apply(Command::Evidence {
            task_id: Some(fixture.run_id.clone()),
        })?;
        assert_eq!(evidence.result["state"], "answered");
        assert_eq!(evidence.result["task_completed"], false);
        assert_eq!(evidence.result["receipts"], json!([]));
        assert_eq!(evidence.result["omitted"], 0);
        Ok(())
    }

    #[test]
    fn evidence_returns_only_bounded_verified_current_receipts_without_contents() -> Result<()> {
        let mut fixture = Fixture::new()?;
        complete_with_verified_evidence(&mut fixture, "Completed safely.", 10)?;
        let request = fixture.request(Command::Evidence {
            task_id: Some(fixture.run_id.clone()),
        });
        let evidence = fixture.authority.apply_from_authenticated_relay(
            &fixture.actor_id,
            &request,
            test_now(),
        )?;
        assert_eq!(evidence.result["state"], "completed");
        assert_eq!(evidence.result["task_completed"], true);
        assert_eq!(evidence.result["receipts"].as_array().unwrap().len(), 8);
        assert_eq!(evidence.result["omitted"], 2);
        for receipt in evidence.result["receipts"].as_array().unwrap() {
            assert_eq!(receipt["capability"], "workspace.read");
            assert_eq!(receipt["hash_prefix"].as_str().unwrap().len(), 12);
            assert!(receipt["bytes"].as_i64().unwrap() > 0);
        }
        let serialized = evidence.result.to_string();
        for private_value in [
            "TOP_SECRET_TOOL_OUTPUT",
            "TOP_SECRET_SEARCH_PREVIEW",
            "TOP_SECRET_ARGUMENTS",
            "UNREFERENCED_SECRET",
            "TOP_SECRET_FAILED_ARTIFACT",
            "TOP_SECRET_FAILED_OUTPUT",
            "proof-0.txt",
            "unreferenced.txt",
        ] {
            assert!(
                !serialized.contains(private_value),
                "leaked {private_value}"
            );
        }

        let transaction = fixture.authority.store.connection.unchecked_transaction()?;
        crate::obligations::invalidate_workspace(
            &transaction,
            &fixture.run_id,
            json!({"reason":"test workspace change"}),
        )?;
        transaction.commit()?;
        let stale = fixture.authority.apply_from_authenticated_relay(
            &fixture.actor_id,
            &request,
            test_now(),
        )?;
        assert!(stale.duplicate);
        assert_eq!(stale.result["receipts"], json!([]));
        assert_eq!(stale.result["omitted"], 0);
        Ok(())
    }

    #[test]
    fn actors_start_unscoped_and_only_see_locally_granted_tasks() -> Result<()> {
        let mut fixture = Fixture::new()?;
        let other = fixture.authority.store.create_run(
            "private task",
            fixture.directory.path(),
            "codex",
            json!([]),
            json!({}),
            "",
        )?;
        let empty_actor = "new-phone";
        fixture.authority.register_actor(empty_actor)?;
        assert!(fixture.authority.resolve_task(empty_actor, None)?.is_none());
        assert!(fixture.authority.authorized_runs(empty_actor)?.is_empty());
        assert!(
            fixture
                .authority
                .resolve_task(empty_actor, Some(&other.id))
                .is_err()
        );

        fixture.authority.grant_run(empty_actor, &other.id)?;
        assert_eq!(
            fixture
                .authority
                .resolve_task(empty_actor, Some(&other.id))?,
            Some(other.id.clone())
        );
        assert_eq!(fixture.authority.authorized_runs(empty_actor)?.len(), 1);
        fixture
            .authority
            .bind_selected_task(empty_actor, &other.id)?;
        assert_eq!(
            fixture.authority.selected_task(empty_actor)?,
            Some(other.id.clone())
        );
        assert!(fixture.authority.authorized_runs(&fixture.actor_id)?.len() == 1);
        assert_eq!(
            fixture.authority.actors_for_run(&fixture.run_id)?,
            vec![fixture.actor_id.clone()]
        );
        assert_eq!(
            fixture.authority.actors_for_run(&other.id)?,
            vec![empty_actor]
        );
        fixture.authority.revoke_actor(empty_actor)?;
        assert!(fixture.authority.authorized_runs(empty_actor)?.is_empty());
        assert!(fixture.authority.actors_for_run(&other.id)?.is_empty());
        let revoked_request = CommandEnvelope {
            actor_id: empty_actor.to_owned(),
            command: Command::Status {
                task_id: Some(other.id.clone()),
            },
            ..fixture.request(Command::ListTasks)
        };
        assert!(
            fixture
                .authority
                .apply_from_authenticated_relay(empty_actor, &revoked_request, test_now())
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn message_without_selected_task_hands_off_without_consuming_request_id() -> Result<()> {
        let mut fixture = Fixture::new()?;
        fixture.authority.register_actor("new-phone")?;
        let request = CommandEnvelope {
            actor_id: "new-phone".into(),
            request_id: uuid::Uuid::new_v4().to_string(),
            command: Command::Message {
                text: "Create a small task".into(),
                task_id: None,
            },
            ..fixture.request(Command::ListTasks)
        };
        let handoff =
            fixture
                .authority
                .apply_from_authenticated_relay("new-phone", &request, test_now())?;
        assert_eq!(handoff.result["needs_task_creation"], true);

        let task = fixture.authority.store.create_run(
            "Create a small task",
            fixture.directory.path(),
            "codex",
            json!([]),
            json!({}),
            "",
        )?;
        fixture.authority.grant_run("new-phone", &task.id)?;
        let rebound = CommandEnvelope {
            command: Command::Message {
                text: "Create a small task".into(),
                task_id: Some(task.id.clone()),
            },
            ..request.clone()
        };
        let applied =
            fixture
                .authority
                .apply_from_authenticated_relay("new-phone", &rebound, test_now())?;
        assert_eq!(applied.result["task_id"], task.id);
        assert!(!applied.duplicate);
        Ok(())
    }

    #[test]
    fn message_after_selected_task_ends_hands_off_to_a_new_task() -> Result<()> {
        let mut fixture = Fixture::new()?;
        fixture
            .authority
            .bind_selected_task(&fixture.actor_id, &fixture.run_id)?;
        fixture.authority.store.connection.execute(
            "UPDATE runs SET state='completed' WHERE id=?1",
            [&fixture.run_id],
        )?;

        let status = fixture.apply(Command::Status { task_id: None })?;
        assert_eq!(status.result["state"], "completed");

        let message = fixture.apply(Command::Message {
            text: "Start another task".into(),
            task_id: None,
        })?;
        assert_eq!(message.result["needs_task_creation"], true);
        assert_eq!(
            fixture
                .authority
                .store
                .events(&fixture.run_id)?
                .iter()
                .filter(|event| event.kind == "user.steering")
                .count(),
            0
        );

        let explicit = fixture.request(Command::Message {
            text: "Try to steer the completed run".into(),
            task_id: Some(fixture.run_id.clone()),
        });
        assert!(
            fixture
                .authority
                .apply_from_authenticated_relay(&fixture.actor_id, &explicit, test_now())
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn supported_commands_are_local_and_do_not_expand_run_permissions() -> Result<()> {
        let mut fixture = Fixture::new()?;
        let original_grants = fixture.authority.store.run(&fixture.run_id)?.grants;
        fixture.apply(Command::SelectTask {
            task_id: fixture.run_id.clone(),
        })?;
        let tasks = fixture.apply(Command::ListTasks)?;
        assert_eq!(tasks.result["tasks"].as_array().unwrap().len(), 1);
        let status = fixture.apply(Command::Status { task_id: None })?;
        assert_eq!(status.result["state"], "ready");
        assert_eq!(status.result["selected"], true);
        fixture.apply(Command::Pause { task_id: None })?;
        assert!(fixture.authority.store.pause_requested(&fixture.run_id)?);
        fixture.apply(Command::Resume { task_id: None })?;
        assert!(!fixture.authority.store.pause_requested(&fixture.run_id)?);
        fixture.apply(Command::Message {
            text: "Keep permissions unchanged\r\nand run tests\u{1b}[2J".into(),
            task_id: None,
        })?;
        let steering = fixture
            .authority
            .store
            .events(&fixture.run_id)?
            .into_iter()
            .find(|event| event.kind == "user.steering")
            .unwrap();
        assert_eq!(
            steering.payload["text"],
            "Keep permissions unchanged\nand run tests[2J"
        );
        assert_eq!(
            fixture.authority.store.run(&fixture.run_id)?.grants,
            original_grants
        );
        fixture.apply(Command::Cancel { task_id: None })?;
        assert_eq!(
            fixture.authority.store.run(&fixture.run_id)?.state,
            "cancelled"
        );
        Ok(())
    }

    #[test]
    fn remote_resume_rejects_unreviewed_legacy_contract_before_ready_transition() -> Result<()> {
        let mut fixture = Fixture::new()?;
        fixture
            .authority
            .store
            .state(&fixture.run_id, "paused", json!({"fixture":true}))?;
        fixture
            .authority
            .store
            .connection
            .execute("DELETE FROM obligations WHERE run_id=?1", [&fixture.run_id])?;
        fixture.authority.store.connection.execute(
            "DELETE FROM workspace_revisions WHERE run_id=?1",
            [&fixture.run_id],
        )?;

        let error = fixture
            .apply(Command::Resume {
                task_id: Some(fixture.run_id.clone()),
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("reviewed contract"));
        assert!(error.contains("F3"));
        assert_eq!(
            fixture.authority.store.run(&fixture.run_id)?.state,
            "paused"
        );
        Ok(())
    }

    #[test]
    fn remote_approval_cannot_resume_an_unreviewed_contract_but_denial_is_recorded() -> Result<()> {
        let mut fixture = Fixture::new()?;
        let operation = fixture.authority.store.begin_operation(
            &fixture.run_id,
            "workspace.read",
            json!({"path":"src/lib.rs"}),
            true,
        )?;
        let gate = ensure_remote_approval_gate(
            &mut fixture.authority.store,
            &fixture.run_id,
            &operation.id,
            test_now(),
        )?;
        fixture
            .authority
            .store
            .connection
            .execute("DELETE FROM obligations WHERE run_id=?1", [&fixture.run_id])?;
        fixture.authority.store.connection.execute(
            "DELETE FROM workspace_revisions WHERE run_id=?1",
            [&fixture.run_id],
        )?;

        let error = fixture
            .apply(Command::ApproveOnce {
                challenge_id: gate.challenge_id.clone(),
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("Open F3"));
        assert_eq!(
            fixture.authority.store.operation(&operation.id)?.state,
            "pending"
        );
        assert_eq!(
            fixture.authority.store.connection.query_row(
                "SELECT state FROM remote_operation_gates WHERE challenge_id=?1",
                [&gate.challenge_id],
                |row| row.get::<_, String>(0),
            )?,
            "pending"
        );

        let denial = fixture.apply(Command::Deny {
            challenge_id: gate.challenge_id.clone(),
        })?;
        assert_eq!(denial.result["decision"], "denied");
        assert_eq!(
            fixture.authority.store.connection.query_row(
                "SELECT state FROM remote_operation_gates WHERE challenge_id=?1",
                [&gate.challenge_id],
                |row| row.get::<_, String>(0),
            )?,
            "denied"
        );
        assert_eq!(
            fixture.authority.store.operation(&operation.id)?.state,
            "pending"
        );
        Ok(())
    }

    #[test]
    fn phone_task_aliases_are_stable_unique_and_actor_scoped() -> Result<()> {
        let mut fixture = Fixture::new()?;
        let first_id = "aabbcc00-0000-4000-8000-000000000001";
        let second_id = "aabbcc00-0000-4000-8000-000000000002";
        for (id, title) in [
            (first_id, "first collision task"),
            (second_id, "second collision task"),
        ] {
            fixture.authority.store.create_run_with_id(
                id,
                title,
                fixture.directory.path(),
                "codex",
                json!(["workspace.read"]),
                json!({}),
                "complete",
            )?;
        }
        fixture.authority.grant_run(&fixture.actor_id, first_id)?;
        let first_alias = fixture
            .authority
            .authorized_runs(&fixture.actor_id)?
            .into_iter()
            .find(|task| task["task_id"] == first_id)
            .unwrap()["alias"]
            .as_str()
            .unwrap()
            .to_owned();
        fixture.authority.grant_run(&fixture.actor_id, second_id)?;
        let tasks = fixture.authority.authorized_runs(&fixture.actor_id)?;
        let first_after_collision = tasks
            .iter()
            .find(|task| task["task_id"] == first_id)
            .unwrap()["alias"]
            .as_str()
            .unwrap();
        let second_alias = tasks
            .iter()
            .find(|task| task["task_id"] == second_id)
            .unwrap()["alias"]
            .as_str()
            .unwrap();
        assert_eq!(first_after_collision, first_alias);
        assert_ne!(first_after_collision, second_alias);
        assert_eq!(first_alias, "t-aabbcc");

        let selected = fixture.apply(Command::SelectTask {
            task_id: second_alias.to_owned(),
        })?;
        assert_eq!(selected.result["task_id"], second_id);
        assert_eq!(selected.result["alias"], second_alias);
        assert_eq!(
            fixture
                .authority
                .selected_task(&fixture.actor_id)?
                .as_deref(),
            Some(second_id)
        );
        let explicit_status = fixture.apply(Command::Status {
            task_id: Some(first_alias.clone()),
        })?;
        assert_eq!(explicit_status.result["task_id"], first_id);
        fixture.apply(Command::Pause {
            task_id: Some(first_alias.clone()),
        })?;
        assert!(fixture.authority.store.pause_requested(first_id)?);
        fixture.apply(Command::Resume {
            task_id: Some(first_alias.clone()),
        })?;
        assert!(!fixture.authority.store.pause_requested(first_id)?);
        fixture.apply(Command::Cancel {
            task_id: Some(first_alias.clone()),
        })?;
        assert_eq!(fixture.authority.store.run(first_id)?.state, "cancelled");
        assert!(
            fixture
                .apply(Command::SelectTask {
                    task_id: "t-doesnotexist".into(),
                })
                .is_err()
        );
        assert!(
            fixture
                .apply(Command::Details {
                    task_id: Some("t-doesnotexist".into()),
                })
                .is_err()
        );
        assert_eq!(
            fixture
                .authority
                .selected_task(&fixture.actor_id)?
                .as_deref(),
            Some(second_id)
        );

        let other_actor = "phone-with-no-task";
        fixture.authority.register_actor(other_actor)?;
        let unauthorized = CommandEnvelope {
            actor_id: other_actor.to_owned(),
            request_id: uuid::Uuid::new_v4().to_string(),
            command: Command::SelectTask {
                task_id: first_alias.clone(),
            },
            ..fixture.request(Command::ListTasks)
        };
        assert!(
            fixture
                .authority
                .apply_from_authenticated_relay(other_actor, &unauthorized, test_now())
                .is_err()
        );
        let unauthorized_details = CommandEnvelope {
            actor_id: other_actor.to_owned(),
            request_id: uuid::Uuid::new_v4().to_string(),
            command: Command::Details {
                task_id: Some(first_alias.clone()),
            },
            ..fixture.request(Command::ListTasks)
        };
        assert!(
            fixture
                .authority
                .apply_from_authenticated_relay(other_actor, &unauthorized_details, test_now())
                .is_err()
        );

        fixture.authority = Authority::open(fixture.directory.path())?;
        assert_eq!(
            fixture
                .authority
                .authorized_runs(&fixture.actor_id)?
                .into_iter()
                .find(|task| task["task_id"] == first_id)
                .unwrap()["alias"],
            first_alias
        );
        Ok(())
    }

    #[test]
    fn phone_details_report_bounded_safe_evidence_for_selected_or_explicit_tasks() -> Result<()> {
        let mut fixture = Fixture::new()?;
        let alias = fixture.authority.authorized_runs(&fixture.actor_id)?[0]["alias"]
            .as_str()
            .unwrap()
            .to_owned();
        let huge_title = "task".repeat(500);
        fixture.authority.store.connection.execute(
            "UPDATE runs SET task=?1,budgets=json_set(budgets,'$.model','gpt-6.1-sol') WHERE id=?2",
            params![huge_title, fixture.run_id],
        )?;
        fixture.authority.store.event(
            &fixture.run_id,
            "run.running",
            json!({"message":"running"}),
        )?;
        fixture.authority.store.event(
            &fixture.run_id,
            "model.response",
            json!({"usage":{"input_tokens":8,"output_tokens":13},"raw_tool_dump":"SECRET TOOL OUTPUT"}),
        )?;
        let completed_operation = fixture.authority.store.begin_operation(
            &fixture.run_id,
            "workspace.read",
            json!({"path":"private-path"}),
            true,
        )?;
        fixture.authority.store.connection.execute(
            "UPDATE operations SET state='succeeded' WHERE id=?1",
            [&completed_operation.id],
        )?;
        let _pending_operation = fixture.authority.store.begin_operation(
            &fixture.run_id,
            "workspace.write",
            json!({"path":"private-path"}),
            true,
        )?;
        let uncertain_operation = fixture.authority.store.begin_operation(
            &fixture.run_id,
            "process.run",
            json!({"command":"private command"}),
            false,
        )?;
        fixture.authority.store.connection.execute(
            "UPDATE operations SET state='outcome_unknown' WHERE id=?1",
            [&uncertain_operation.id],
        )?;

        let details = fixture.apply(Command::Details {
            task_id: Some(alias.clone()),
        })?;
        assert_eq!(details.result["task_id"], fixture.run_id);
        assert_eq!(details.result["alias"], alias);
        assert_eq!(details.result["model"], "gpt-6.1-sol");
        assert_eq!(details.result["model_turns"], 1);
        assert_eq!(details.result["model_tokens"], 21);
        assert_eq!(details.result["successful_actions"], 1);
        assert_eq!(details.result["pending_actions"], 1);
        assert_eq!(details.result["uncertain_actions"], 1);
        assert_eq!(
            details.result["obligations"],
            json!({"open":0,"verified":0,"stale":0,"superseded":0})
        );
        assert!(details.result["task"].as_str().unwrap().chars().count() <= 240);
        let serialized = details.result.to_string();
        assert!(!serialized.contains("SECRET TOOL OUTPUT"));
        assert!(!serialized.contains("private command"));
        assert!(!serialized.contains("private-path"));

        fixture
            .authority
            .bind_selected_task(&fixture.actor_id, &fixture.run_id)?;
        let selected_details = fixture.apply(Command::Details { task_id: None })?;
        assert_eq!(selected_details.result["task_id"], fixture.run_id);
        assert_eq!(selected_details.result["obligations"]["open"], 0);

        let explicit_run = fixture.authority.store.create_run(
            "task with explicit requirements",
            fixture.directory.path(),
            "codex",
            json!(["workspace.read"]),
            json!({"obligations":["open requirement","verified requirement","stale requirement","superseded requirement"]}),
            "complete",
        )?;
        fixture
            .authority
            .grant_run(&fixture.actor_id, &explicit_run.id)?;
        for (id, state) in [
            (1_i64, "open"),
            (2, "verified"),
            (3, "stale"),
            (4, "superseded"),
        ] {
            fixture.authority.store.connection.execute(
                "UPDATE obligations SET state=?1 WHERE run_id=?2 AND id=?3",
                params![state, explicit_run.id, id],
            )?;
        }
        let explicit_alias = fixture
            .authority
            .authorized_runs(&fixture.actor_id)?
            .into_iter()
            .find(|task| task["task_id"] == explicit_run.id)
            .unwrap()["alias"]
            .as_str()
            .unwrap()
            .to_owned();
        let explicit_details = fixture.apply(Command::Details {
            task_id: Some(explicit_alias),
        })?;
        assert_eq!(
            explicit_details.result["obligations"],
            json!({"open":1,"verified":1,"stale":1,"superseded":1})
        );
        assert!(
            !explicit_details
                .result
                .to_string()
                .contains("open requirement")
        );
        Ok(())
    }

    #[test]
    fn operation_decisions_require_a_local_exact_pending_gate_and_are_one_shot() -> Result<()> {
        let mut fixture = Fixture::new()?;
        let operation = fixture.authority.store.begin_operation(
            &fixture.run_id,
            "workspace.read",
            json!({"path":"src/lib.rs"}),
            true,
        )?;
        let approval = ensure_remote_approval_gate(
            &mut fixture.authority.store,
            &fixture.run_id,
            &operation.id,
            test_now(),
        )?;
        let same_approval = ensure_remote_approval_gate(
            &mut fixture.authority.store,
            &fixture.run_id,
            &operation.id,
            test_now(),
        )?;
        assert_eq!(approval.challenge_id, same_approval.challenge_id);
        let required = fixture
            .authority
            .store
            .events(&fixture.run_id)?
            .into_iter()
            .find(|event| event.kind == "approval.required")
            .unwrap();
        assert_eq!(required.payload["operation_id"], operation.id);
        assert!(
            required.payload["descriptor"]
                .as_str()
                .unwrap()
                .contains("src/lib.rs")
        );
        assert_eq!(
            fixture
                .authority
                .authorize_operation_dispatch(&fixture.run_id, &operation.id)?,
            DispatchAuthorization::AwaitingDecision
        );
        fixture.apply(Command::ApproveOnce {
            challenge_id: approval.challenge_id,
        })?;
        assert_eq!(
            fixture
                .authority
                .authorize_operation_dispatch(&fixture.run_id, &operation.id)?,
            DispatchAuthorization::ApprovedOnce
        );
        assert_eq!(
            fixture.authority.store.operation(&operation.id)?.state,
            "dispatched"
        );
        assert!(
            fixture
                .authority
                .authorize_operation_dispatch(&fixture.run_id, &operation.id)
                .is_err()
        );

        let second = fixture.authority.store.begin_operation(
            &fixture.run_id,
            "workspace.read",
            json!({"path":"src/main.rs"}),
            true,
        )?;
        let denial = ensure_remote_approval_gate(
            &mut fixture.authority.store,
            &fixture.run_id,
            &second.id,
            test_now(),
        )?;
        fixture.apply(Command::Deny {
            challenge_id: denial.challenge_id,
        })?;
        assert_eq!(
            fixture
                .authority
                .authorize_operation_dispatch(&fixture.run_id, &second.id)?,
            DispatchAuthorization::Denied
        );
        assert_eq!(
            fixture.authority.store.operation(&second.id)?.state,
            "cancelled"
        );
        Ok(())
    }

    #[test]
    fn remote_approval_notification_hides_url_paths_and_queries() -> Result<()> {
        let mut fixture = Fixture::new()?;
        let operation = fixture.authority.store.begin_operation(
            &fixture.run_id,
            "network.fetch",
            json!({"url":"https://public.example/reset/private-token?state=secret-value"}),
            true,
        )?;
        ensure_remote_approval_gate(
            &mut fixture.authority.store,
            &fixture.run_id,
            &operation.id,
            test_now(),
        )?;
        let required = fixture
            .authority
            .store
            .events(&fixture.run_id)?
            .into_iter()
            .find(|event| event.kind == "approval.required")
            .expect("approval event is recorded");
        let descriptor = required.payload["descriptor"].as_str().unwrap();
        assert!(descriptor.contains("public.example"));
        assert!(!descriptor.contains("/reset/private-token"));
        assert!(!descriptor.contains("secret-value"));
        Ok(())
    }

    #[test]
    fn expired_approval_cannot_be_applied_or_dispatched() -> Result<()> {
        let mut fixture = Fixture::new()?;
        let operation = fixture.authority.store.begin_operation(
            &fixture.run_id,
            "workspace.read",
            json!({"path":"src/lib.rs"}),
            true,
        )?;
        let challenge = ensure_remote_approval_gate(
            &mut fixture.authority.store,
            &fixture.run_id,
            &operation.id,
            test_now(),
        )?;
        fixture.authority.store.connection.execute(
            "UPDATE remote_operation_gates SET expires_at=?1 WHERE challenge_id=?2",
            rusqlite::params![test_now() - 1, challenge.challenge_id],
        )?;
        let request = fixture.request(Command::ApproveOnce {
            challenge_id: challenge.challenge_id,
        });
        assert!(
            fixture
                .authority
                .apply_from_authenticated_relay(&fixture.actor_id, &request, test_now())
                .is_err()
        );
        assert_eq!(
            expire_remote_approval_gates(&mut fixture.authority.store, test_now())?,
            1
        );
        assert_eq!(
            expire_remote_approval_gates(&mut fixture.authority.store, test_now())?,
            0
        );
        assert_eq!(
            fixture
                .authority
                .authorize_operation_dispatch(&fixture.run_id, &operation.id)?,
            DispatchAuthorization::Expired
        );
        assert_eq!(
            fixture.authority.store.operation(&operation.id)?.state,
            "cancelled"
        );
        Ok(())
    }

    #[test]
    fn request_id_cannot_be_reused_for_a_different_command() -> Result<()> {
        let mut fixture = Fixture::new()?;
        let request = fixture.request(Command::Status {
            task_id: Some(fixture.run_id.clone()),
        });
        fixture.authority.apply_from_authenticated_relay(
            &fixture.actor_id,
            &request,
            test_now(),
        )?;
        let changed = CommandEnvelope {
            command: Command::Cancel {
                task_id: Some(fixture.run_id.clone()),
            },
            ..request.clone()
        };
        assert!(
            fixture
                .authority
                .apply_from_authenticated_relay(&fixture.actor_id, &changed, test_now())
                .is_err()
        );
        assert_eq!(fixture.authority.store.run(&fixture.run_id)?.state, "ready");
        Ok(())
    }

    #[test]
    fn command_schema_rejects_unlisted_privileged_operations_and_extra_fields() -> Result<()> {
        for raw in [
            r#"{"kind":"shell","command":"whoami"}"#,
            r#"{"kind":"set_grants","grants":["workspace.write"]}"#,
            r#"{"kind":"change_model","model":"new-model"}"#,
            r#"{"kind":"message","text":"hello","workspace":"C:/"}"#,
        ] {
            assert!(serde_json::from_str::<Command>(raw).is_err());
        }
        Ok(())
    }
}
