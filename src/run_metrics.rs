//! Read-only observed usage and verification. These metrics never authorize completion.
use crate::storage::Store;
use anyhow::Result;
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LiveMetrics {
    pub reported_tokens: u64,
    pub estimated_tokens: u64,
    pub unclassified_tokens: u64,
    pub verified: usize,
    pub requirements: usize,
    pub usage_pending: bool,
}

impl LiveMetrics {
    pub fn load(store: &Store, id: &str) -> Result<Self> {
        let revision = store.workspace_revision(id)?;
        let (estimated_tokens, unclassified_tokens) = store.usage_source_tokens(id)?;
        let obligations = store.obligations(id)?;
        let explicit = obligations
            .iter()
            .any(|item| item.id > 0 && item.state != "superseded");
        let active: Vec<_> = obligations
            .iter()
            .filter(|item| item.state != "superseded" && (!explicit || item.id > 0))
            .collect();
        let mut verified = 0;
        if let Some(revision) = revision {
            for item in &active {
                if item.state != "verified"
                    || item.verified_revision != Some(revision)
                    || item.evidence.is_empty()
                {
                    continue;
                }
                let mut current = true;
                for hash in &item.evidence {
                    if !crate::obligations::current_evidence(&store.connection, id, revision, hash)?
                    {
                        current = false;
                        break;
                    }
                }
                verified += usize::from(current);
            }
        }
        Ok(Self {
            reported_tokens: store
                .model_tokens(id)?
                .saturating_sub(estimated_tokens)
                .saturating_sub(unclassified_tokens),
            estimated_tokens,
            unclassified_tokens,
            verified,
            requirements: active.len(),
            usage_pending: store.event_count(id, "model.started")?
                > store.event_count(id, "model.response")?
                    + store.event_count(id, "model.failed")?,
        })
    }
}

pub fn report(store: &Store, id: &str) -> Result<Value> {
    let run = store.run(id)?;
    let live = LiveMetrics::load(store, id)?;
    let events = store.events(id)?;
    let mut input = 0_u64;
    let mut output = 0_u64;
    let mut cached = 0_u64;
    let mut measured = 0_u64;
    let mut cached_measured = 0_u64;
    let mut elapsed_ms = 0_u64;
    let mut timed = 0_u64;
    let mut closed = 0_u64;
    let mut not_dispatched = 0_u64;
    for event in events
        .iter()
        .filter(|event| matches!(event.kind.as_str(), "model.response" | "model.failed"))
    {
        closed += 1;
        if let Some(ms) = event.payload["elapsed_ms"].as_u64() {
            elapsed_ms = elapsed_ms.saturating_add(ms);
            timed += 1;
        }
        let usage = &event.payload["usage"];
        if event.kind == "model.failed"
            && event.payload["request_dispatched"] == false
            && usage.is_null()
        {
            not_dispatched += 1;
            continue;
        }
        if usage["source"] != "provider" {
            continue;
        }
        if let (Some(i), Some(o)) = (
            usage["input_tokens"].as_u64(),
            usage["output_tokens"].as_u64(),
        ) {
            input = input.saturating_add(i);
            output = output.saturating_add(o);
            measured += 1;
            if let Some(c) = usage["cached_input_tokens"].as_u64().filter(|cached| {
                *cached <= i && (usage["cached_input_reported"] == true || *cached > 0)
            }) {
                cached = cached.saturating_add(c);
                cached_measured += 1;
            }
        }
    }
    let attempts = (store.event_count(id, "model.started")? as u64).max(closed);
    let unreported = attempts.saturating_sub(measured + not_dispatched);
    let complete = measured > 0 && unreported == 0 && !live.usage_pending;
    let time_complete = complete && timed == closed && elapsed_ms > 0;
    let operations = store.operations(id)?;
    let succeeded = operations
        .iter()
        .filter(|operation| operation.state == "succeeded")
        .count();
    let failed = operations
        .iter()
        .filter(|operation| operation.state == "failed")
        .count();
    let acceptance = events.iter().rev().find(|event| {
        matches!(
            event.kind.as_str(),
            "acceptance.passed" | "acceptance.failed" | "acceptance.unavailable"
        )
    });
    let acceptance_current = match (acceptance, store.workspace_revision(id)?) {
        (Some(event), Some(revision)) if event.kind == "acceptance.passed" => {
            event.payload["artifact"]
                .as_str()
                .map(|hash| {
                    crate::obligations::current_evidence(&store.connection, id, revision, hash)
                })
                .transpose()?
                .unwrap_or(false)
        }
        _ => false,
    };
    let elapsed = store.run_started_at(id)?.map(|start| {
        let end = events
            .iter()
            .rev()
            .find(|event| {
                matches!(
                    event.kind.as_str(),
                    "run.completed" | "run.answered" | "run.failed" | "run.cancelled"
                )
            })
            .map(|event| event.created_at)
            .unwrap_or_else(crate::storage::unix_time);
        end.saturating_sub(start).max(0) as u64
    });
    Ok(json!({
        "tokens":{"reported_total":live.reported_tokens,"estimated_total":live.estimated_tokens,"unclassified_total":live.unclassified_tokens,"tracked_total":store.model_tokens(id)?,"input":if measured>0 {json!(input)}else{Value::Null},
            "output":if measured>0 {json!(output)}else{Value::Null},
            "cached_input":if measured>0 && cached_measured==measured {json!(cached)}else{Value::Null},
            "measured_attempts":measured,"not_dispatched_attempts":not_dispatched,"unreported_attempts":unreported,"usage_complete":complete,"current_turn_pending":live.usage_pending,
            "note":"Reported totals include known failed attempts. Cached input is part of input, not extra tokens. Missing receipts are unknown; these are not billed costs."},
        "verification":{"verified":live.verified,"requirements":live.requirements,
            "coverage_percent":if live.requirements>0 {json!(100.0*live.verified as f64/live.requirements as f64)}else{Value::Null},
            "independent_acceptance_configured":!run.budgets["acceptance_check"].is_null(),
            "latest_independent_acceptance":acceptance.map(|event|event.kind.as_str()),
            "independent_acceptance_current":acceptance_current,
            "note":"Current requirement evidence coverage, not model confidence or a general accuracy score."},
        "operations":{"succeeded":succeeded,"failed":failed,"unknown":store.unknown_count(id)?,
            "success_percent":if succeeded+failed>0 {json!(100.0*succeeded as f64/(succeeded+failed) as f64)}else{Value::Null}},
        "efficiency":{"wall_seconds":elapsed,"recorded_model_ms":elapsed_ms,
            "output_tokens_per_model_second":if time_complete {json!(output as f64*1000.0/elapsed_ms as f64)}else{Value::Null},
            "tokens_per_completed_task":if run.state=="completed" && complete {json!(live.reported_tokens)}else{Value::Null},
            "note":"Output rate includes request latency and uses complete reported usage and timing. Task token efficiency requires actual verified completion."},
        "state":run.state
    }))
}

pub fn display(value: &Value) -> String {
    let tokens = &value["tokens"];
    let verification = &value["verification"];
    let efficiency = &value["efficiency"];
    let shown = |value: &Value| {
        value
            .as_u64()
            .map(|number| number.to_string())
            .unwrap_or_else(|| "unknown".into())
    };
    let rate = efficiency["output_tokens_per_model_second"]
        .as_f64()
        .map(|value| format!("{value:.1}"))
        .unwrap_or_else(|| "unavailable".into());
    format!(
        "Tokens: {} reported · input {} · output {} · cached {} (included in input)\nEstimates: {} tokens · {} legacy tokens with unclassified source\nUsage: {} measured / {} unreported attempts{} · {} failed before dispatch\nVerified requirements: {}/{} · current evidence coverage\nIndependent acceptance: {}\nTools: {} succeeded / {} failed / {} uncertain\nEfficiency: {}s wall · {rate} output tokens/s of recorded model time\nTokens per completed task: {}\n{}",
        tokens["reported_total"],
        shown(&tokens["input"]),
        shown(&tokens["output"]),
        shown(&tokens["cached_input"]),
        tokens["estimated_total"],
        tokens["unclassified_total"],
        tokens["measured_attempts"],
        tokens["unreported_attempts"],
        if tokens["current_turn_pending"] == true {
            " · current turn pending"
        } else {
            ""
        },
        tokens["not_dispatched_attempts"],
        verification["verified"],
        verification["requirements"],
        if verification["latest_independent_acceptance"] == "acceptance.passed"
            && verification["independent_acceptance_current"] != true
        {
            "previous pass; evidence now stale"
        } else {
            verification["latest_independent_acceptance"]
                .as_str()
                .unwrap_or(
                    if verification["independent_acceptance_configured"] == true {
                        "pending"
                    } else {
                        "not configured"
                    },
                )
        },
        value["operations"]["succeeded"],
        value["operations"]["failed"],
        value["operations"]["unknown"],
        shown(&efficiency["wall_seconds"]),
        shown(&efficiency["tokens_per_completed_task"]),
        verification["note"].as_str().unwrap_or_default()
    )
}
