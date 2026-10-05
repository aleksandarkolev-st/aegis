//! Detect unchanged outcomes, while treating new exploratory findings as progress.
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    model::Action,
    storage::{Run, Store, append_event},
};

pub(crate) fn ensure_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS loop_progress (
        run_id TEXT PRIMARY KEY REFERENCES runs(id), epoch TEXT NOT NULL,
        unchanged INTEGER NOT NULL, last_error TEXT
    );
    CREATE TABLE IF NOT EXISTS loop_observations (
        run_id TEXT NOT NULL REFERENCES runs(id), signature TEXT NOT NULL,
        PRIMARY KEY(run_id,signature)
    );
    CREATE TABLE IF NOT EXISTS loop_read_ranges (
        run_id TEXT NOT NULL REFERENCES runs(id), source TEXT NOT NULL,
        start INTEGER NOT NULL, end INTEGER NOT NULL,
        PRIMARY KEY(run_id,source,start,end)
    );",
    )?;
    Ok(())
}

fn digest(value: &Value) -> String {
    hex::encode(Sha256::digest(value.to_string().as_bytes()))
}

fn stable(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            for field in ["elapsed_ms", "duration_ms", "elapsed_seconds"] {
                fields.remove(field);
            }
            for value in fields.values_mut() {
                stable(value);
            }
        }
        Value::Array(items) => {
            for value in items {
                stable(value);
            }
        }
        Value::String(text) => {
            if let Ok(mut nested) = serde_json::from_str::<Value>(text) {
                if nested.is_object() || nested.is_array() {
                    stable(&mut nested);
                    *text = nested.to_string();
                }
            }
        }
        _ => {}
    }
}

#[derive(Default)]
struct Evidence {
    facts: Vec<Value>,
    ranges: Vec<(String, usize, String)>,
}

impl Evidence {
    fn results(&mut self, scope: &str, value: &Value) {
        // Count returned items, never the query, ordering, truncation marker or
        // grouping of an already-known set of search results.
        if let Some(items) = value
            .get("matches")
            .and_then(Value::as_array)
            .or_else(|| value.as_array())
        {
            for item in items {
                self.facts.push(json!({"scope":scope,"item":item}));
            }
        } else if !value.is_null() {
            self.facts.push(json!({"scope":scope,"result":value}));
        }
    }

    fn excerpt(&mut self, source: &str, query: &str, text: &str) {
        if text.is_empty()
            || text == "No matching lines"
            || text == "Range is past the end of the artifact."
            || text.starts_with("Invalid inspection range;")
        {
            return;
        }
        if let Ok(value) = serde_json::from_str::<Value>(text) {
            self.results(source, &value);
            return;
        }
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            let line = if !query.is_empty() && !query.starts_with("@slice ") {
                line.split_once(": ")
                    .filter(|(prefix, _)| prefix.parse::<usize>().is_ok())
                    .map_or(line, |(_, text)| text)
            } else {
                line
            };
            self.facts.push(json!({"scope":source,"text":line}));
        }
    }

    fn record(&self, transaction: &Transaction<'_>, run_id: &str) -> Result<bool> {
        let mut novel = false;
        for fact in &self.facts {
            let signature = digest(&json!({"evidence_v2":fact}));
            novel |= transaction.execute(
                "INSERT OR IGNORE INTO loop_observations(run_id,signature) VALUES (?1,?2)",
                params![run_id, signature],
            )? == 1;
        }
        for (source, start, text) in &self.ranges {
            let end = start + text.chars().count();
            if end == *start {
                continue;
            }
            let mut statement = transaction.prepare("SELECT start,end FROM loop_read_ranges WHERE run_id=?1 AND source=?2 AND end>?3 AND start<?4 ORDER BY start")?;
            let known = statement
                .query_map(params![run_id, source, *start as i64, end as i64], |row| {
                    Ok((
                        row.get::<_, i64>(0)? as usize,
                        row.get::<_, i64>(1)? as usize,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut cursor = *start;
            let mut gaps = Vec::new();
            for (first, last) in known {
                if first > cursor {
                    gaps.push((cursor, first.min(end)));
                }
                cursor = cursor.max(last).min(end);
            }
            if cursor < end {
                gaps.push((cursor, end));
            }
            // Empty/whitespace-only ranges are not substantive information.
            let characters: Vec<_> = text.chars().collect();
            novel |= gaps.iter().any(|(first, last)| {
                characters[first - start..last - start]
                    .iter()
                    .any(|c| !c.is_whitespace())
            });
            transaction.execute("DELETE FROM loop_read_ranges WHERE run_id=?1 AND source=?2 AND start>=?3 AND end<=?4",params![run_id,source,*start as i64,end as i64])?;
            transaction.execute("INSERT OR IGNORE INTO loop_read_ranges(run_id,source,start,end) VALUES (?1,?2,?3,?4)",params![run_id,source,*start as i64,end as i64])?;
        }
        Ok(novel)
    }
}

pub(crate) fn observe(
    store: &mut Store,
    run: &Run,
    action: &Action,
    error: Option<&str>,
) -> Result<()> {
    if store.run(&run.id)?.state != "running" {
        return Ok(());
    }
    let mut evidence = Evidence::default();
    let outcome = if let Some(error) = error {
        json!({"error":error})
    } else {
        match action {
            Action::Invoke { capability, args } => {
                let receipt:Option<(String,Option<String>)>=store.connection.query_row("SELECT state,artifact FROM operations WHERE run_id=?1 ORDER BY rowid DESC LIMIT 1",[&run.id],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
                let Some((state, Some(hash))) = receipt else {
                    return Ok(());
                };
                let bytes = store.artifact(&hash)?;
                let mut value: Value =
                    serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({"artifact":hash}));
                // Hash the full receipt locally, including image bytes: a new
                // screenshot is new information even when its size is unchanged.
                // Only the digest is persisted; receipt data never enters policy.
                stable(&mut value);
                if state == "succeeded" {
                    match capability.as_str() {
                        "workspace.search" => evidence.results("workspace.search", &value),
                        "workspace.read" => {
                            if let Some(text) = value["content"].as_str() {
                                evidence.ranges.push((
                                    digest(&json!({"path":args["path"],"sha256":value["sha256"]})),
                                    0,
                                    text.into(),
                                ));
                            }
                        }
                        "workspace.read_batch" => {
                            for entry in value["selected"].as_array().into_iter().flatten() {
                                if let (Some(offset), Some(text)) =
                                    (entry["offset"].as_u64(), entry["text"].as_str())
                                {
                                    evidence.ranges.push((
                                        digest(
                                            &json!({"path":entry["path"],"sha256":entry["sha256"]}),
                                        ),
                                        offset as usize,
                                        text.into(),
                                    ));
                                }
                            }
                        }
                        _ => evidence.results("tool", &value),
                    }
                }
                json!({"state":state,"result":value})
            }
            Action::SearchCapabilities { .. } => {
                let event = store.recent_context_events(&run.id, 1)?;
                if let Some(event) = event
                    .first()
                    .filter(|event| event.kind == "capability.search")
                {
                    evidence.results("capability", &event.payload["matches"]);
                }
                json!({"matches":event.first().map(|event|&event.payload["matches"])})
            }
            Action::InspectResult { query, .. } => {
                let events = store.recent_context_events(&run.id, 1)?;
                if let Some(event) = events.first().filter(|event| {
                    matches!(
                        event.kind.as_str(),
                        "artifact.inspected"
                            | "memory.inspected"
                            | "conversation.inspected"
                            | "owner.source_read"
                    )
                }) {
                    let excerpt = event.payload["excerpt"]
                        .as_str()
                        .or_else(|| event.payload["text"].as_str())
                        .unwrap_or_default();
                    // Handles can change when receipts differ only in wrapper
                    // metadata. The returned excerpt is the evidence.
                    evidence.excerpt("inspection", query, excerpt);
                }
                json!({"inspection":true})
            }
            Action::Checkpoint { .. } | Action::Remember { .. } => json!({"maintenance":true}),
            Action::AskUser { .. } => json!({"question":true}),
            Action::VerifyObligations { obligations } => {
                for proof in obligations {
                    evidence
                        .facts
                        .push(json!({"verified":proof.id,"evidence":proof.evidence}));
                }
                json!({"verified":obligations})
            }
            Action::Finish { .. } | Action::Blocked { .. } => return Ok(()),
        }
    };
    let owner: i64 = store.connection.query_row(
        "SELECT COALESCE(MAX(seq),0) FROM task_owner_messages WHERE run_id=?1",
        [&run.id],
        |row| row.get(0),
    )?;
    let observed: i64 = store.connection.query_row(
        "SELECT COALESCE(MAX(changed_seq),0) FROM observed_files WHERE run_id=?1",
        [&run.id],
        |row| row.get(0),
    )?;
    let source: Option<String> = store
        .connection
        .query_row(
            "SELECT fingerprint FROM workspace_fingerprints WHERE run_id=?1",
            [&run.id],
            |row| row.get(0),
        )
        .optional()?;
    let epoch = digest(&json!({"source":source,"observed":observed,"owner":owner}));
    let mut key_action = serde_json::to_value(action)?;
    // Model-authored prose is not new environmental evidence. A different
    // checkpoint wording must not disguise a loop with no new observations.
    let maintenance =
        matches!(action, Action::Remember { .. } | Action::Checkpoint { .. }) && error.is_none();
    if maintenance {
        key_action = json!({"maintenance":true});
    }
    let failed = error.is_some()
        || outcome["state"] == "failed"
        || outcome["result"]["exit_code"]
            .as_i64()
            .is_some_and(|code| code != 0)
        || outcome["result"]["isError"] == true;
    // Equivalent failures are the same blocker even when retry arguments change.
    // Successful exploration depends on returned evidence, never action novelty.
    if failed {
        evidence = Evidence {
            facts: vec![json!({"failure":outcome,"epoch":epoch})],
            ranges: vec![],
        };
    }
    let transaction = store.connection.unchecked_transaction()?;
    let previous: Option<(String, i64)> = transaction
        .query_row(
            "SELECT epoch,unchanged FROM loop_progress WHERE run_id=?1",
            [&run.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let novel = evidence.record(&transaction, &run.id)?;
    let unchanged = if previous.as_ref().is_some_and(|(old, _)| old != &epoch) {
        0
    } else if novel && !maintenance {
        0
    } else {
        previous.map(|(_, count)| count).unwrap_or(0) + 1
    };
    transaction.execute("INSERT INTO loop_progress(run_id,epoch,unchanged,last_error) VALUES (?1,?2,?3,?4) ON CONFLICT(run_id) DO UPDATE SET epoch=excluded.epoch,unchanged=excluded.unchanged,last_error=excluded.last_error",params![run.id,epoch,unchanged,error])?;
    let limit = if failed { 2 } else { 4 };
    if unchanged >= limit {
        append_event(
            &transaction,
            &run.id,
            "loop.stalled",
            json!({"unchanged":unchanged,"error":error,"action":key_action,"reason":"repeated actions produced no new information or verified workspace progress"}),
        )?;
        transaction.execute(
            "UPDATE runs SET state='waiting_recovery' WHERE id=?1 AND state='running'",
            [&run.id],
        )?;
        append_event(
            &transaction,
            &run.id,
            "run.waiting_recovery",
            json!({"reason":"repeated unchanged actions; resume with a different approach or new task-owner input"}),
        )?;
    }
    transaction.commit()?;
    Ok(())
}

pub(crate) fn add_context(store: &Store, run: &Run, context: &mut Value) -> Result<()> {
    let state:Option<(i64,Option<String>)>=store.connection.query_row("SELECT unchanged,last_error FROM loop_progress WHERE run_id=?1 AND (unchanged>=2 OR (unchanged>0 AND last_error IS NOT NULL))",[&run.id],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
    if let Some((unchanged, error)) = state {
        context["progress_guard"] = json!({"unchanged":unchanged,"last_error":error,"policy":"Recent actions added no new evidence. Reuse mapped information and choose a different approach. Different queries, arguments, or reordered/subset results are not progress. Exploration must return previously unseen evidence or unread source content; model-authored prose is not evidence. Do not invent edits for an exploratory task. If waiting for external state, use a bounded granted wait/poll tool or blocked instead of busy repetition. Unchanged retries will pause durably."});
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_retry_arguments_cannot_disguise_the_same_permission_blocker() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "Explore",
            directory.path(),
            "custom",
            json!([]),
            json!({}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        for retry in 0..8 {
            observe(
                &mut store,
                &run,
                &Action::Invoke {
                    capability: format!("unavailable.{retry}"),
                    args: json!({"retry":retry}),
                },
                Some("capability not granted"),
            )?;
        }
        assert_eq!(store.run(&run.id)?.state, "waiting_recovery");
        assert_eq!(store.event_count(&run.id, "loop.stalled")?, 1);
        store.state(&run.id, "running", json!({}))?;
        store.steer(&run.id, "Use the newly granted alternate approach")?;
        observe(
            &mut store,
            &run,
            &Action::Invoke {
                capability: "alternate".into(),
                args: json!({}),
            },
            Some("capability not granted"),
        )?;
        assert_eq!(store.run(&run.id)?.state, "running");
        Ok(())
    }

    #[test]
    fn unchanged_rejections_pause_and_the_guard_survives_restart() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "Explore",
            directory.path(),
            "custom",
            json!([]),
            json!({}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let action = Action::Invoke {
            capability: "unavailable".into(),
            args: json!({}),
        };
        observe(&mut store, &run, &action, Some("capability not granted"))?;
        observe(&mut store, &run, &action, Some("capability not granted"))?;
        drop(store);
        let mut store = Store::open(directory.path())?;
        observe(&mut store, &run, &action, Some("capability not granted"))?;
        assert_eq!(store.run(&run.id)?.state, "waiting_recovery");
        assert_eq!(store.event_count(&run.id, "loop.stalled")?, 1);
        store.state(&run.id, "running", json!({}))?;
        store.steer(&run.id, "Use a different granted tool")?;
        observe(&mut store, &run, &action, Some("capability not granted"))?;
        assert_eq!(store.run(&run.id)?.state, "running");
        Ok(())
    }

    #[test]
    fn returned_inspection_evidence_progresses_but_changing_queries_does_not() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "Explore",
            directory.path(),
            "custom",
            json!([]),
            json!({}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        for index in 0..10 {
            let action = Action::InspectResult {
                artifact: "fixture".into(),
                query: format!("@slice {} 4000", index * 4000),
            };
            store.event(
                &run.id,
                "artifact.inspected",
                json!({"hash":"fixture","excerpt":format!("Finding {index}")}),
            )?;
            observe(&mut store, &run, &action, None)?;
        }
        assert_eq!(store.run(&run.id)?.state, "running");
        for index in 0..4 {
            let action = Action::InspectResult {
                artifact: "fixture".into(),
                query: format!("different query {index}"),
            };
            store.event(
                &run.id,
                "artifact.inspected",
                json!({"hash":"fixture","excerpt":format!("1: Finding {}",index % 2)}),
            )?;
            observe(&mut store, &run, &action, None)?;
        }
        assert_eq!(store.run(&run.id)?.state, "waiting_recovery");
        Ok(())
    }

    fn invoke_result(
        store: &mut Store,
        run: &Run,
        capability: &str,
        args: Value,
        result: Value,
    ) -> Result<String> {
        let operation = store.begin_operation(&run.id, capability, args.clone(), true)?;
        crate::storage::claim_test_operation(store, &operation)?;
        let artifact = store.put_artifact(&serde_json::to_vec(&result)?)?;
        store.operation_state(&operation, "succeeded", Some(&artifact), json!({}))?;
        observe(
            store,
            run,
            &Action::Invoke {
                capability: capability.into(),
                args,
            },
            None,
        )?;
        Ok(artifact)
    }

    #[test]
    fn changed_search_queries_and_reordered_subsets_do_not_add_evidence() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "Find weaknesses",
            directory.path(),
            "custom",
            json!([]),
            json!({}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let first = json!({"path":"src/a.rs","line":1,"text":"cancellation"});
        let second = json!({"path":"src/b.rs","line":2,"text":"cancellation"});
        invoke_result(
            &mut store,
            &run,
            "workspace.search",
            json!({"query":"cancel"}),
            json!({"matches":[first,second]}),
        )?;
        drop(store);
        let mut store = Store::open(directory.path())?;
        for index in 0..4 {
            let matches = if index % 2 == 0 {
                json!([second, first])
            } else {
                json!([first])
            };
            invoke_result(
                &mut store,
                &run,
                "workspace.search",
                json!({"query":format!("cancel {index}")}),
                json!({"matches":matches,"truncated":index%2==0}),
            )?;
        }
        assert_eq!(store.run(&run.id)?.state, "waiting_recovery");
        assert_eq!(store.event_count(&run.id, "loop.stalled")?, 1);
        Ok(())
    }

    #[test]
    fn a_new_search_location_resets_unchanged_and_empty_searches_do_not() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "Explore",
            directory.path(),
            "custom",
            json!([]),
            json!({}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        for index in 0..3 {
            invoke_result(
                &mut store,
                &run,
                "workspace.search",
                json!({"query":index.to_string()}),
                json!({"matches":[]}),
            )?;
        }
        invoke_result(
            &mut store,
            &run,
            "workspace.search",
            json!({"query":"new"}),
            json!({"matches":[{"path":"a","line":1,"text":"new evidence"}]}),
        )?;
        let count: i64 = store.connection.query_row(
            "SELECT unchanged FROM loop_progress WHERE run_id=?1",
            [&run.id],
            |row| row.get(0),
        )?;
        assert_eq!(count, 0);
        assert_eq!(store.run(&run.id)?.state, "running");
        Ok(())
    }

    #[test]
    fn changing_capability_queries_cannot_cycle_the_same_matches() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "Explore",
            directory.path(),
            "custom",
            json!([]),
            json!({}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        for index in 0..5 {
            store.event(
                &run.id,
                "capability.search",
                json!({"matches":["workspace.read"]}),
            )?;
            observe(
                &mut store,
                &run,
                &Action::SearchCapabilities {
                    query: index.to_string(),
                },
                None,
            )?;
        }
        assert_eq!(store.run(&run.id)?.state, "waiting_recovery");
        Ok(())
    }

    #[test]
    fn overlapping_read_ranges_only_progress_when_they_expose_unread_text() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let source = "Unicode 🦀 evidence with more unread source";
        std::fs::write(directory.path().join("a"), source)?;
        let mut store = Store::open(&directory.path().join(".arun"))?;
        let run = store.create_run(
            "Explore",
            directory.path(),
            "custom",
            json!([]),
            json!({}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let hash = hex::encode(Sha256::digest(source.as_bytes()));
        for (offset, length) in [(0, 15), (10, 15), (0, 20), (1, 20), (2, 20), (3, 20)] {
            let text: String = source.chars().skip(offset).take(length).collect();
            invoke_result(
                &mut store,
                &run,
                "workspace.read_batch",
                json!({"files":[{"path":"a","offset":offset,"length":length}]}),
                json!({"selected":[{"path":"a","sha256":hash,"offset":offset,"next_offset":offset+text.chars().count(),"total_characters":source.chars().count(),"text":text}]}),
            )?;
        }
        assert_eq!(store.run(&run.id)?.state, "waiting_recovery");
        Ok(())
    }

    #[test]
    fn empty_or_invalid_inspection_ranges_cannot_bypass_the_guard() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "Explore",
            directory.path(),
            "custom",
            json!([]),
            json!({}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        for index in 0..40 {
            let query = format!("@slice {} 4000", index * 4000);
            let excerpt = crate::kernel::inspect(b"tiny", &query);
            store.event(
                &run.id,
                "artifact.inspected",
                json!({"hash":"fixture","excerpt":excerpt}),
            )?;
            observe(
                &mut store,
                &run,
                &Action::InspectResult {
                    artifact: "fixture".into(),
                    query,
                },
                None,
            )?;
        }
        assert_eq!(store.run(&run.id)?.state, "waiting_recovery");
        Ok(())
    }

    #[test]
    fn unchanged_failed_commands_stall_despite_conservative_revision_bumps() -> Result<()> {
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join("source.txt"), "original")?;
        let mut store = Store::open(&directory.path().join(".arun"))?;
        let run = store.create_run(
            "Diagnose a failing command",
            directory.path(),
            "custom",
            json!(["workspace.read", "workspace.write", "process.run"]),
            json!({}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let action = Action::Invoke {
            capability: "process.run".into(),
            args: json!({"command":"failing fixture"}),
        };
        for _ in 0..3 {
            let operation = store.begin_operation(
                &run.id,
                "process.run",
                json!({"command":"failing fixture"}),
                true,
            )?;
            crate::storage::claim_test_operation(&mut store, &operation)?;
            let artifact = store.put_artifact(br#"{"exit_code":1,"stderr":"same failure"}"#)?;
            store.operation_state(&operation, "failed", Some(&artifact), json!({}))?;
            observe(&mut store, &run, &action, None)?;
        }
        assert_eq!(store.run(&run.id)?.state, "waiting_recovery");
        assert!(store.workspace_revision(&run.id)?.unwrap() > 0);
        store.state(&run.id, "running", json!({}))?;
        std::fs::write(directory.path().join("source.txt"), "fixed source")?;
        let operation = store.begin_operation(
            &run.id,
            "process.run",
            json!({"command":"failing fixture"}),
            true,
        )?;
        crate::storage::claim_test_operation(&mut store, &operation)?;
        let artifact = store.put_artifact(br#"{"exit_code":1,"stderr":"same failure"}"#)?;
        store.operation_state(&operation, "failed", Some(&artifact), json!({}))?;
        observe(&mut store, &run, &action, None)?;
        assert_eq!(
            store.run(&run.id)?.state,
            "running",
            "An actual source change permits a fresh attempt"
        );
        Ok(())
    }
}
