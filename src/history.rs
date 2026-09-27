use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::model::{Checkpoint, Milestone};
use crate::storage::{Event, Operation, Run, Store, append_event};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    version: u32,
    run_id: String,
    contract: String,
    pub sequence: i64,
    pub state: String,
    counts: BTreeMap<String, i64>,
    model_tokens: u64,
    #[serde(default)]
    tool_result_tokens: u64,
    started_at: Option<i64>,
    checkpoint: Option<String>,
    proposal: Option<String>,
    summary: Option<String>,
    milestones: Vec<Milestone>,
    activated: Vec<(String, u32)>,
    pub unresolved: Vec<Operation>,
}

#[derive(Debug)]
pub struct Recovery {
    pub snapshot: Snapshot,
    pub base_sequence: i64,
    pub tail_events: usize,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Archive {
    run_id: String,
    first_seq: i64,
    last_seq: i64,
    events: Vec<Event>,
}

fn contract(run: &Run) -> Result<String> {
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(&json!({
        "id":run.id,"task":run.task,"workspace":run.workspace,"provider":run.provider,
        "grants":run.grants,"budgets":run.budgets,"acceptance":run.acceptance,"created_at":run.created_at
    }))?)))
}

impl Snapshot {
    fn apply(&mut self, store: &Store, event: &Event) -> Result<()> {
        if event.seq != self.sequence + 1 {
            bail!("recovery event sequence has a gap");
        }
        self.sequence = event.seq;
        *self.counts.entry(event.kind.clone()).or_default() += 1;
        match event.kind.as_str() {
            "model.started" => {
                if event.payload["context_tokenizer"] == crate::tokenization::ENCODING {
                    self.tool_result_tokens = self
                        .tool_result_tokens
                        .checked_add(
                            event.payload["tool_result_tokens"]
                                .as_u64()
                                .context("measured tool-result tokens missing")?,
                        )
                        .context("tool-result token accounting overflow")?;
                }
            }
            "model.response" => {
                self.model_tokens = self
                    .model_tokens
                    .saturating_add(event.payload["usage"]["input_tokens"].as_u64().unwrap_or(0))
                    .saturating_add(
                        event.payload["usage"]["output_tokens"]
                            .as_u64()
                            .unwrap_or(0),
                    )
                    .min(i64::MAX as u64);
            }
            "run.running" => {
                self.state = "running".into();
                self.started_at.get_or_insert(event.created_at);
            }
            "run.ready"
            | "run.waiting_recovery"
            | "run.failed"
            | "run.cancelled"
            | "run.answered"
            | "run.completed" => {
                self.state = event.kind.trim_start_matches("run.").into();
                if event.kind == "run.answered" {
                    self.summary = event.payload["summary"]
                        .as_str()
                        .map(|summary| summary.chars().take(4000).collect());
                }
                if event.kind == "run.completed" {
                    self.summary = event.payload["summary"]
                        .as_str()
                        .map(|summary| summary.chars().take(4000).collect());
                    if self.milestones.len() == 1 && self.milestones[0].title == "Task request" {
                        self.milestones[0].state = "completed".into();
                        self.milestones[0].evidence =
                            serde_json::from_value(event.payload["evidence"].clone())?;
                    }
                }
            }
            "checkpoint.created" => {
                let hash = event.payload["artifact"]
                    .as_str()
                    .context("checkpoint artifact missing")?;
                let checkpoint: Checkpoint = serde_json::from_slice(&store.artifact(hash)?)?;
                self.checkpoint = Some(hash.into());
                if !checkpoint.milestones.is_empty() {
                    self.milestones = checkpoint.milestones;
                }
            }
            "completion.proposed" => {
                self.proposal = Some(
                    event.payload["artifact"]
                        .as_str()
                        .context("proposal artifact missing")?
                        .into(),
                )
            }
            "completion.resolved" => self.proposal = None,
            "capability.deactivated" => {
                let capability = event.payload["id"]
                    .as_str()
                    .context("deactivated capability missing")?;
                self.activated.retain(|(id, _)| id != capability);
            }
            "capability.activated" => {
                let capability = event.payload["id"]
                    .as_str()
                    .context("activated capability missing")?;
                let version = event.payload["version"]
                    .as_u64()
                    .context("activated version missing")?
                    .try_into()?;
                self.activated.retain(|(id, _)| id != capability);
                self.activated.push((capability.into(), version));
                self.activated.sort();
            }
            "operation.pending" => {
                self.unresolved.push(Operation {
                    id: event.payload["id"]
                        .as_str()
                        .context("operation ID missing")?
                        .into(),
                    run_id: self.run_id.clone(),
                    capability: event.payload["capability"]
                        .as_str()
                        .context("operation capability missing")?
                        .into(),
                    capability_version: event.payload["version"]
                        .as_u64()
                        .context("operation version missing")?
                        .try_into()?,
                    arguments: event.payload["arguments"].clone(),
                    idempotency_key: event.payload["idempotency_key"]
                        .as_str()
                        .context("operation key missing")?
                        .into(),
                    retry_safe: event.payload["retry_safe"]
                        .as_bool()
                        .context("operation retry policy missing")?,
                    state: "pending".into(),
                    artifact: None,
                });
            }
            "operation.dispatched" | "operation.executing" => {
                let id = event.payload["id"]
                    .as_str()
                    .context("operation ID missing")?;
                if !self.unresolved.iter().any(|operation| operation.id == id) {
                    let operation = store.operation(id)?;
                    if operation.run_id != self.run_id || !operation.retry_safe {
                        bail!("unsafe operation cannot be redispatched without a new intent");
                    }
                    self.unresolved.push(operation);
                }
                let operation = self
                    .unresolved
                    .iter_mut()
                    .find(|operation| operation.id == id)
                    .context("operation intent missing from recovery")?;
                operation.state = event.kind.trim_start_matches("operation.").into();
            }
            "operation.succeeded"
            | "operation.failed"
            | "operation.outcome_unknown"
            | "operation.cancelled"
            | "operation.timed_out" => {
                let id = event.payload["id"]
                    .as_str()
                    .context("operation ID missing")?;
                self.unresolved.retain(|operation| operation.id != id);
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archived_history_replays_exactly_but_recovery_reads_only_the_snapshot_tail() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "long history",
            directory.path(),
            "codex",
            json!(["workspace.read", "workspace.write"]),
            json!({}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let operation = store.begin_operation(
            &run.id,
            "workspace.read",
            json!({"path":"fixture.txt"}),
            true,
        )?;
        let evidence = store.put_artifact(b"verified progress")?;
        store.operation_state(&operation, "succeeded", Some(&evidence), json!({}))?;
        let checkpoint = Checkpoint {
            decisions: vec!["preserve verified progress".into()],
            unresolved: vec![],
            next_action: "continue repair".into(),
            milestones: vec![Milestone {
                title: "Read fixture".into(),
                state: "completed".into(),
                evidence: vec![evidence],
            }],
        };
        store.save_checkpoint(&run.id, &checkpoint)?;
        store.activate(&run.id, "workspace.read", 1)?;
        for _ in 0..3000 {
            store.event(
                &run.id,
                "model.response",
                json!({"usage":{"input_tokens":20,"output_tokens":2}}),
            )?;
        }
        let original = store.events(&run.id)?;
        store.save_snapshot(&run.id)?;
        assert!(store.archive_history(&run.id)? > 2900);
        store.activate(&run.id, "workspace.write", 1)?;
        let write = store.begin_operation(
            &run.id,
            "workspace.write",
            json!({"path":"patch.txt","content":"pending"}),
            false,
        )?;
        store.operation_state(&write, "dispatched", None, json!({}))?;
        store.claim_operation(&write)?;
        drop(store);
        let store = Store::open(directory.path())?;
        let recovery = store.load_recovery(&run.id)?.unwrap();
        assert!(recovery.tail_events < 32, "{}", recovery.tail_events);
        assert_eq!(recovery.snapshot.model_tokens, 66_000);
        assert_eq!(recovery.snapshot.counts["model.response"], 3000);
        assert_eq!(recovery.snapshot.unresolved.len(), 1);
        assert_eq!(recovery.snapshot.unresolved[0].id, write.id);
        assert_eq!(recovery.snapshot.unresolved[0].state, "executing");
        assert_eq!(store.last_checkpoint(&run.id)?, Some(checkpoint));
        let all = store.events(&run.id)?;
        assert_eq!(
            serde_json::to_value(&all[..original.len()])?,
            serde_json::to_value(&original)?
        );
        assert!(all.windows(2).all(|pair| pair[1].seq == pair[0].seq + 1));
        let later = store.events_since(&run.id, 2500)?;
        assert_eq!(later.first().unwrap().seq, 2501);
        assert_eq!(later.last().unwrap().seq, all.last().unwrap().seq);
        Ok(())
    }

    #[test]
    fn corrupted_cold_history_does_not_enter_recovery_but_cannot_pass_an_audit() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "archive",
            directory.path(),
            "codex",
            json!([]),
            json!({}),
            "",
        )?;
        for _ in 0..100 {
            store.event(&run.id, "audit.detail", json!({}))?;
        }
        store.save_snapshot(&run.id)?;
        store.archive_history(&run.id)?;
        let hash: String = store.connection.query_row(
            "SELECT artifact FROM event_archives WHERE run_id = ?1",
            [&run.id],
            |row| row.get(0),
        )?;
        std::fs::write(directory.path().join("artifacts").join(hash), "corrupt")?;
        assert!(store.load_recovery(&run.id)?.is_some());
        assert!(store.events(&run.id).is_err());
        let snapshot: String = store.connection.query_row(
            "SELECT artifact FROM run_snapshots WHERE run_id = ?1",
            [&run.id],
            |row| row.get(0),
        )?;
        std::fs::write(directory.path().join("artifacts").join(snapshot), "corrupt")?;
        assert!(store.load_recovery(&run.id).is_err());
        Ok(())
    }

    #[test]
    fn a_safe_failed_operation_can_recover_its_original_intent_after_a_snapshot() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run("retry", directory.path(), "codex", json!([]), json!({}), "")?;
        store.state(&run.id, "running", json!({}))?;
        let operation = store.begin_operation(
            &run.id,
            "workspace.read",
            json!({"path":"fixture.txt"}),
            true,
        )?;
        store.operation_state(&operation, "failed", None, json!({}))?;
        store.save_snapshot(&run.id)?;
        store.operation_state(&operation, "dispatched", None, json!({}))?;
        store.claim_operation(&operation)?;
        assert_eq!(
            store.load_recovery(&run.id)?.unwrap().snapshot.unresolved[0].id,
            operation.id
        );
        Ok(())
    }
}

impl Store {
    fn capture(&self, run_id: &str) -> Result<Snapshot> {
        let run = self.run(run_id)?;
        let (sequence, checkpoint, proposal, summary) = self.connection.query_row(
            "SELECT last_seq, checkpoint, proposal, summary FROM run_projection WHERE run_id = ?1",
            [run_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        let mut statement = self
            .connection
            .prepare("SELECT kind, count FROM event_counts WHERE run_id = ?1 ORDER BY kind")?;
        let counts = statement
            .query_map([run_id], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<BTreeMap<String, i64>>>()?;
        Ok(Snapshot {
            version: 1,
            run_id: run_id.into(),
            contract: contract(&run)?,
            sequence,
            state: run.state,
            counts,
            model_tokens: self.model_tokens(run_id)?,
            tool_result_tokens: self.tool_result_tokens(run_id)?,
            started_at: self.run_started_at(run_id)?,
            checkpoint,
            proposal,
            summary,
            milestones: self.milestones(run_id)?,
            activated: self.active_capabilities(run_id)?,
            unresolved: self.unresolved(run_id)?,
        })
    }

    pub fn save_snapshot(&mut self, run_id: &str) -> Result<String> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let snapshot = self.capture(run_id)?;
        let hash = self.put_artifact(&serde_json::to_vec(&snapshot)?)?;
        transaction.execute("INSERT INTO run_snapshots(run_id, sequence, artifact) VALUES (?1, ?2, ?3)
            ON CONFLICT(run_id) DO UPDATE SET sequence=excluded.sequence, artifact=excluded.artifact", params![run_id,snapshot.sequence,hash])?;
        append_event(
            &transaction,
            run_id,
            "runtime.snapshot",
            json!({"artifact":hash,"sequence":snapshot.sequence}),
        )?;
        transaction.commit()?;
        Ok(hash)
    }

    pub fn load_recovery(&self, run_id: &str) -> Result<Option<Recovery>> {
        let transaction = self.connection.unchecked_transaction()?;
        let pointer: Option<(i64, String)> = transaction
            .query_row(
                "SELECT sequence, artifact FROM run_snapshots WHERE run_id = ?1",
                [run_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((base_sequence, hash)) = pointer else {
            return Ok(None);
        };
        let mut snapshot: Snapshot = serde_json::from_slice(&self.artifact(&hash)?)?;
        if snapshot.version != 1
            || snapshot.run_id != run_id
            || snapshot.sequence != base_sequence
            || snapshot.contract != contract(&self.run(run_id)?)?
        {
            bail!("snapshot does not match the immutable task contract");
        }
        let events = self.hot_events_since(run_id, base_sequence)?;
        for event in &events {
            snapshot.apply(self, event)?;
        }
        let mut current = self.capture(run_id)?;
        snapshot
            .unresolved
            .sort_by(|first, second| first.id.cmp(&second.id));
        current
            .unresolved
            .sort_by(|first, second| first.id.cmp(&second.id));
        if serde_json::to_value(&snapshot)? != serde_json::to_value(&current)? {
            bail!("snapshot recovery disagrees with committed projections");
        }
        transaction.commit()?;
        Ok(Some(Recovery {
            snapshot,
            base_sequence,
            tail_events: events.len(),
        }))
    }

    pub(crate) fn archived_events_since(&self, run_id: &str, sequence: i64) -> Result<Vec<Event>> {
        let mut statement = self.connection.prepare("SELECT first_seq, last_seq, artifact FROM event_archives WHERE run_id = ?1 AND last_seq > ?2 ORDER BY first_seq")?;
        let rows = statement.query_map(params![run_id, sequence], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut events = Vec::new();
        for row in rows {
            let (first, last, hash) = row?;
            let archive: Archive = serde_json::from_slice(&self.artifact(&hash)?)?;
            if archive.run_id != run_id
                || archive.first_seq != first
                || archive.last_seq != last
                || archive.events.first().map(|event| event.seq) != Some(first)
                || archive.events.last().map(|event| event.seq) != Some(last)
                || archive
                    .events
                    .windows(2)
                    .any(|pair| pair[1].seq != pair[0].seq + 1)
            {
                bail!("event archive range failed validation");
            }
            events.extend(
                archive
                    .events
                    .into_iter()
                    .filter(|event| event.seq > sequence),
            );
        }
        Ok(events)
    }

    pub fn archive_history(&mut self, run_id: &str) -> Result<usize> {
        let mut total = 0;
        loop {
            let transaction =
                Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
            let through: Option<i64> = transaction
                .query_row(
                    "SELECT sequence - 64 FROM run_snapshots WHERE run_id = ?1",
                    [run_id],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(through) = through.filter(|sequence| *sequence > 0) else {
                return Ok(total);
            };
            let events = {
                let mut statement = transaction.prepare("SELECT seq, kind, payload, created_at FROM events WHERE run_id = ?1 AND seq <= ?2 ORDER BY seq LIMIT 256")?;
                let mut rows = statement.query(params![run_id, through])?;
                let mut events = Vec::new();
                let mut bytes = 0;
                while let Some(row) = rows.next()? {
                    let payload: String = row.get(2)?;
                    if !events.is_empty() && bytes + payload.len() > 8 * 1024 * 1024 {
                        break;
                    }
                    bytes += payload.len();
                    events.push(Event {
                        seq: row.get(0)?,
                        kind: row.get(1)?,
                        payload: serde_json::from_str(&payload)?,
                        created_at: row.get(3)?,
                    });
                }
                events
            };
            if events.is_empty() {
                return Ok(total);
            }
            let first = events.first().unwrap().seq;
            let last = events.last().unwrap().seq;
            let count = events.len();
            let hash = self.put_artifact(&serde_json::to_vec(&Archive {
                run_id: run_id.into(),
                first_seq: first,
                last_seq: last,
                events,
            })?)?;
            transaction.execute("INSERT INTO event_archives(run_id, first_seq, last_seq, artifact) VALUES (?1, ?2, ?3, ?4)", params![run_id,first,last,hash])?;
            transaction.execute(
                "DELETE FROM events WHERE run_id = ?1 AND seq BETWEEN ?2 AND ?3",
                params![run_id, first, last],
            )?;
            append_event(
                &transaction,
                run_id,
                "history.archived",
                json!({"first_seq":first,"last_seq":last,"artifact":hash,"events":count}),
            )?;
            transaction.commit()?;
            total += count;
        }
    }

    pub fn maintain_history(&mut self, run_id: &str) -> Result<()> {
        let due: bool = self.connection.query_row("SELECT last_seq - COALESCE((SELECT sequence FROM run_snapshots WHERE run_id=?1), -256) >= 256 FROM run_projection WHERE run_id=?1", [run_id], |row| row.get(0))?;
        if due {
            self.save_snapshot(run_id)?;
        }
        let hot: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM events WHERE run_id = ?1",
            [run_id],
            |row| row.get(0),
        )?;
        if hot > 1024 {
            self.archive_history(run_id)?;
        }
        Ok(())
    }
}
