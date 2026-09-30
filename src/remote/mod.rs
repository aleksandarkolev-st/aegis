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
             ON CONFLICT(actor_id) DO UPDATE SET enabled=1,paired_at=excluded.paired_at",
            params![actor_id, crate::storage::unix_time()],
        )?;
        transaction.commit()?;
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
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn grant_run(&mut self, actor_id: &str, run_id: &str) -> Result<()> {
        validate_actor_id(actor_id)?;
        validate_uuid(run_id, "run ID")?;
        self.store.run(run_id)?;
        let changed = self.store.connection.execute(
            "INSERT INTO remote_actor_runs(actor_id,run_id,enabled)
             SELECT actor_id,?2,1 FROM remote_actors WHERE actor_id=?1 AND enabled=1
             ON CONFLICT(actor_id,run_id) DO UPDATE SET enabled=1",
            params![actor_id, run_id],
        )?;
        if changed != 1 {
            bail!("remote actor is not paired or has been revoked");
        }
        Ok(())
    }

    pub fn revoke_actor(&mut self, actor_id: &str) -> Result<bool> {
        let changed = self.store.connection.execute(
            "UPDATE remote_actors SET enabled=0 WHERE actor_id=?1 AND enabled=1",
            [actor_id],
        )?;
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
        validate_actor_id(authenticated_actor_id)?;
        if request.actor_id != authenticated_actor_id {
            bail!("relay-authenticated actor does not match the command actor");
        }
        validate_request(request, now)?;
        if request.installation_id != self.installation_id {
            bail!("remote command targets a different Aegis installation");
        }
        let canonical = request.canonical_bytes()?;
        let request_hash = hex::encode(Sha256::digest(&canonical));

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
            bail!("remote actor is not paired");
        };
        if !enabled {
            bail!("remote actor is revoked");
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
                bail!("remote request ID was already used for different content");
            }
            if let Some(task_id) = task_id.as_deref() {
                require_actor_run(&transaction, &request.actor_id, task_id)?;
            }
            let result: Value = serde_json::from_str(&prior_result)?;
            transaction.commit()?;
            return Ok(Receipt {
                request_id: request.request_id.clone(),
                duplicate: true,
                result,
            });
        }

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
             EXISTS(SELECT 1 FROM remote_selected_tasks AS selected WHERE selected.actor_id=?1 AND selected.run_id=run.id)
             FROM remote_actor_runs AS access JOIN runs AS run ON run.id=access.run_id
             JOIN remote_actors AS actor ON actor.actor_id=access.actor_id
             WHERE access.actor_id=?1 AND access.enabled=1 AND actor.enabled=1 ORDER BY run.created_at DESC LIMIT 100",
        )?;
        let rows = statement.query_map([actor_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, bool>(5)?,
            ))
        })?;
        let mut tasks = Vec::new();
        for row in rows {
            let (id, task, state, provider, created_at, selected) = row?;
            tasks.push(json!({
                "task_id":id,
                "task":crate::text::clean(&task).chars().take(300).collect::<String>(),
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
    let target = ["path", "program", "url", "host"]
        .iter()
        .find_map(|key| operation.arguments.get(*key).and_then(Value::as_str))
        .unwrap_or("(target not available)");
    format!(
        "{} · {}",
        operation.capability,
        crate::text::clean(target)
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
         CREATE TABLE IF NOT EXISTS remote_actor_runs (
            actor_id TEXT NOT NULL REFERENCES remote_actors(actor_id),
            run_id TEXT NOT NULL REFERENCES runs(id),
            enabled INTEGER NOT NULL,
            PRIMARY KEY(actor_id,run_id)
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
    Ok(())
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
            let text = text.replace("\r\n", "\n").replace('\r', "\n");
            let text = crate::text::clean(&text).trim().to_owned();
            if text.is_empty() || text.len() > MAX_MESSAGE_BYTES {
                bail!("remote message must be nonempty and at most 65536 UTF-8 bytes");
            }
            let state = run_state(transaction, &run_id)?;
            if !matches!(state.as_str(), "ready" | "running") {
                bail!("task is no longer active; message was not queued");
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
            let task = crate::text::clean(&task)
                .chars()
                .take(300)
                .collect::<String>();
            let summary = summary.map(|value| {
                crate::text::clean(&value)
                    .chars()
                    .take(300)
                    .collect::<String>()
            });
            Ok(applied_for_task(
                &run_id,
                json!({"task":task,"provider":provider,"state":state,"created_at":created_at,"started_at":started_at,"summary":summary,"selected":selected}),
            ))
        }
        Command::Pause { task_id } => {
            let run_id = require_resolved_task(transaction, actor_id, task_id.as_deref())?;
            let state = run_state(transaction, &run_id)?;
            if is_terminal(&state) {
                bail!("ended tasks cannot pause");
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
                bail!("ended tasks cannot resume");
            }
            let unknown: i64 = transaction.query_row(
                "SELECT COUNT(*) FROM operations WHERE run_id=?1 AND state='outcome_unknown'",
                [&run_id],
                |row| row.get(0),
            )?;
            if unknown > 0 {
                bail!("uncertain operation outcomes need local reconciliation before resume");
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
                bail!("ended tasks cannot be cancelled");
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
            require_actor_run(transaction, actor_id, task_id)?;
            transaction.execute(
                "INSERT INTO remote_selected_tasks(actor_id,run_id,selected_at) VALUES (?1,?2,?3)
                 ON CONFLICT(actor_id) DO UPDATE SET run_id=excluded.run_id,selected_at=excluded.selected_at",
                params![actor_id, task_id, crate::storage::unix_time()],
            )?;
            Ok(applied_for_task(task_id, json!({"selected":true})))
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
                bail!("approval challenge is unknown");
            };
            require_actor_run(transaction, actor_id, &run_id)?;
            if expires_at <= now {
                bail!("approval challenge has expired");
            }
            if gate_state != "pending" {
                bail!("approval challenge has already been resolved");
            }
            let operation = operation_in_transaction(transaction, &run_id, &operation_id)?;
            if operation.state != "pending" {
                bail!("approval challenge no longer refers to a pending operation");
            }
            if operation_intent_hash(&operation)? != intent_hash {
                bail!("the pending operation changed after the local approval gate was created");
            }
            let changed = transaction.execute(
                "UPDATE remote_operation_gates SET state=?2,decision_actor=?3,decision_request=?4,decided_at=?5 WHERE challenge_id=?1 AND state='pending'",
                params![challenge_id, wanted, actor_id, request_id, now],
            )?;
            if changed != 1 {
                bail!("approval challenge was concurrently resolved");
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
    resolve_actor_task(transaction, actor_id, requested)?
        .ok_or_else(|| anyhow::anyhow!("select or specify a task first"))
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

fn require_actor_run(transaction: &Transaction<'_>, actor_id: &str, run_id: &str) -> Result<()> {
    let allowed: bool = transaction.query_row(
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
        "SELECT run.id,run.task,run.state,run.provider,run.created_at,
         EXISTS(SELECT 1 FROM remote_selected_tasks AS selected WHERE selected.actor_id=?1 AND selected.run_id=run.id)
         FROM remote_actor_runs AS access JOIN runs AS run ON run.id=access.run_id
         JOIN remote_actors AS actor ON actor.actor_id=access.actor_id
         WHERE access.actor_id=?1 AND access.enabled=1 AND actor.enabled=1 ORDER BY run.created_at DESC LIMIT 100",
    )?;
    let rows = statement.query_map([actor_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, bool>(5)?,
        ))
    })?;
    let mut tasks = Vec::new();
    for row in rows {
        let (id, task, state, provider, created_at, selected) = row?;
        tasks.push(json!({"task_id":id,"task":crate::text::clean(&task).chars().take(300).collect::<String>(),"state":state,"provider":provider,"created_at":created_at,"selected":selected}));
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

fn validate_request(request: &CommandEnvelope, now: i64) -> Result<()> {
    if request.version != PROTOCOL_VERSION {
        bail!("unsupported remote command protocol version");
    }
    validate_uuid(&request.installation_id, "installation ID")?;
    validate_actor_id(&request.actor_id)?;
    validate_uuid(&request.request_id, "request ID")?;
    if request.issued_at <= 0
        || request.expires_at <= now
        || request.issued_at > now.saturating_add(CLOCK_SKEW_SECONDS)
        || request.expires_at <= request.issued_at
        || request.expires_at.saturating_sub(request.issued_at) > MAX_COMMAND_TTL_SECONDS
    {
        bail!("remote command is expired or has an invalid time window");
    }
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
        | Command::Pause { task_id }
        | Command::Resume { task_id }
        | Command::Cancel { task_id } => task_id.as_deref(),
        _ => None,
    } {
        validate_uuid(task_id, "task ID")?;
    }
    if let Command::SelectTask { task_id } = &request.command {
        validate_uuid(task_id, "task ID")?;
    }
    if let Command::ApproveOnce { challenge_id } | Command::Deny { challenge_id } = &request.command
    {
        validate_uuid(challenge_id, "approval challenge ID")?;
    }
    request.canonical_bytes()?;
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
