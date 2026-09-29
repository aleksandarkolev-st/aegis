//! Deterministic user views of kernel-owned state. No inference or hidden reasoning.
use anyhow::{Context, Result, bail};
use rusqlite::params;
use serde_json::{Value, json};

use crate::storage::{Run, Store};

pub const VIEWS: &[&str] = &[
    "goal", "contract", "status", "why", "evidence", "verify", "provider", "budget", "handoff",
];

pub fn obligation_id(text: &str) -> Result<i64> {
    let id: i64 = text
        .trim_start_matches(['O', 'o'])
        .parse()
        .context("Use an obligation ID such as O3")?;
    if id < 0 {
        bail!("Obligation ID must be nonnegative");
    }
    Ok(id)
}

fn usage(store: &Store, run: &Run) -> Result<Value> {
    let elapsed = store.run_started_at(&run.id)?.map(|start| {
        let end: Option<i64> = store.connection.query_row(
            "SELECT MAX(created_at) FROM events WHERE run_id = ?1 AND kind IN ('run.completed','run.answered','run.cancelled','run.failed')", [&run.id], |row| row.get(0),
        ).unwrap_or(None);
        end.unwrap_or(crate::storage::unix_time()).saturating_sub(start).max(0) as u64
    });
    let mut result = json!({});
    for (name, used, limit) in [
        (
            "actions",
            Some(store.event_count(&run.id, "model.response")? as u64),
            run.budgets["actions"].as_u64().or(Some(40)),
        ),
        (
            "model_tokens",
            Some(store.model_tokens(&run.id)?),
            run.budgets["model_tokens"].as_u64().or(Some(400_000)),
        ),
        (
            "tool_result_tokens",
            Some(store.tool_result_tokens(&run.id)?),
            crate::tokenization::limit(&run.budgets)?,
        ),
        (
            "wall_seconds",
            elapsed,
            run.budgets["wall_seconds"].as_u64().or(Some(3600)),
        ),
    ] {
        result[name] = json!({"used":used,"limit":limit,"remaining":used.zip(limit).map(|(used,limit)|limit.saturating_sub(used))});
    }
    let events = store.events(&run.id)?;
    let mut input = 0u64;
    let mut output = 0u64;
    let mut cached = 0u64;
    let mut measured = false;
    let mut measured_cache = false;
    for event in events
        .iter()
        .filter(|event| matches!(event.kind.as_str(), "model.response" | "model.failed"))
    {
        if let Some(tokens) = event.payload["usage"]["input_tokens"].as_u64() {
            input = input.saturating_add(tokens);
            measured = true;
        }
        if let Some(tokens) = event.payload["usage"]["output_tokens"].as_u64() {
            output = output.saturating_add(tokens);
        }
        if let Some(tokens) = event.payload["usage"]["cached_input_tokens"].as_u64() {
            cached = cached.saturating_add(tokens);
            measured_cache = true;
        }
    }
    result["provider_usage"] = json!({"input":measured.then_some(input),"output":measured.then_some(output),"cached_input":measured_cache.then_some(cached),"note":"Cached input is included in input. Missing provider receipts remain unmeasured."});
    result["tool_result_metric"] = json!(crate::tokenization::ENCODING);
    Ok(result)
}

pub fn evidence(store: &Store, run_id: &str, id: i64) -> Result<Value> {
    let obligation = store
        .obligations(run_id)?
        .into_iter()
        .find(|item| item.id == id)
        .context("Obligation not found")?;
    let revision = store.workspace_revision(run_id)?;
    let mut artifacts = Vec::new();
    for hash in &obligation.evidence {
        let mut statement = store.connection.prepare(
            "SELECT operation.id, operation.capability, operation.arguments, operation.state, revision.revision FROM operations AS operation LEFT JOIN operation_revisions AS revision ON revision.operation_id = operation.id WHERE operation.run_id = ?1 AND (operation.artifact = ?2 OR EXISTS(SELECT 1 FROM operation_artifacts AS linked WHERE linked.operation_id = operation.id AND linked.hash = ?2)) ORDER BY operation.rowid",
        )?;
        let sources = statement
            .query_map(params![run_id, hash], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut operations = Vec::new();
        for (op, capability, arguments, state, recorded_revision) in sources {
            let detail: Option<String> = store.connection.query_row(
                "SELECT json_extract(payload,'$.detail') FROM events WHERE run_id = ?1 AND kind LIKE 'operation.%' AND json_extract(payload,'$.id') = ?2 AND json_extract(payload,'$.detail') IS NOT NULL ORDER BY seq DESC LIMIT 1", params![run_id,op], |row| row.get(0),
            ).optional()?;
            operations.push(json!({"operation":op,"capability":capability,"arguments":serde_json::from_str::<Value>(&arguments)?,"state":state,"revision":recorded_revision,"current":state == "succeeded" && recorded_revision.is_some() && recorded_revision == revision,"receipt":detail.map(|text|serde_json::from_str::<Value>(&text)).transpose()?}));
        }
        artifacts.push(json!({"artifact":hash,"operations":operations,"integrity":match store.artifact(hash) {Ok(_)=>"verified".to_owned(),Err(error)=>error.to_string()}}));
    }
    Ok(
        json!({"obligation":obligation,"workspace_revision":revision,"artifacts":artifacts,"verification_scope":"Provenance and revision checks; semantic coverage is not independently inferred."}),
    )
}

use rusqlite::OptionalExtension;

pub fn view(store: &Store, run_id: &str, command: &str, argument: Option<&str>) -> Result<Value> {
    let run = store.run(run_id)?;
    let obligations = store.obligations(run_id)?;
    let revision = store.workspace_revision(run_id)?;
    let milestones = store.milestones(run_id)?;
    let checkpoint = store.last_checkpoint(run_id)?;
    let route = store.current_route(run_id)?;
    match command {
        "goal" | "contract" => Ok(
            json!({"run":run.id,"goal":run.task,"requirements":obligations,"workspace_revision":revision,"current_plan":milestones,"checkpoint":checkpoint,"current_provider":route,"fallbacks":crate::routing::approved(&run)?}),
        ),
        "status" => Ok(
            json!({"run":run.id,"state":run.state,"current_provider":route,"workspace_revision":revision,
            "obligations":{"open":obligations.iter().filter(|item|item.id>0 && item.state=="open").count(),"verified":obligations.iter().filter(|item|item.id>0 && item.state=="verified").count(),"stale":obligations.iter().filter(|item|item.id>0 && item.state=="stale").count()},
            "active_milestone":milestones.iter().find(|item|item.state=="active"),"last_operation":store.operations(run_id)?.last(),"fallbacks":crate::routing::approved(&run)?,"budget":usage(store,&run)?}),
        ),
        "why" => Ok(
            json!({"source":"Persisted kernel state and checkpoint; no hidden model reasoning",
            "state":run.state,"next_action":checkpoint.as_ref().map(|item|&item.next_action),"unresolved":checkpoint.as_ref().map(|item|&item.unresolved),
            "requirements_remaining":obligations.iter().filter(|item|item.id>0 && item.state!="verified" && item.state!="superseded").collect::<Vec<_>>(),
            "last_rejection":store.events(run_id)?.into_iter().rev().find(|event|event.kind=="action.rejected").map(|event|event.payload)}),
        ),
        "evidence" => match argument {
            Some(id) => evidence(store, run_id, obligation_id(id)?),
            None => Ok(
                json!({"requirements":obligations,"artifacts":store.evidence_artifacts(run_id)?}),
            ),
        },
        "verify" => {
            let mut blockers = Vec::new();
            for item in obligations
                .iter()
                .filter(|item| item.id > 0 && item.state != "superseded")
            {
                if item.state != "verified" || item.verified_revision != revision {
                    blockers.push(format!(
                        "O{} {}: {} (proof revision {:?}, current {:?})",
                        item.id, item.title, item.state, item.verified_revision, revision
                    ));
                }
            }
            if let Err(error) = crate::obligations::validate_completion(store, run_id) {
                blockers.push(error.to_string());
            }
            for operation in store
                .operations(run_id)?
                .iter()
                .filter(|op| !matches!(op.state.as_str(), "succeeded" | "failed" | "cancelled"))
            {
                blockers.push(format!("Operation {}: {}", operation.id, operation.state));
            }
            if store.evidence_artifacts(run_id)?.is_empty() {
                blockers.push("No successful-operation evidence".into());
            }
            for milestone in milestones
                .iter()
                .filter(|item| item.state != "completed" && item.title != "Task request")
            {
                blockers.push(format!(
                    "Milestone {}: {}",
                    milestone.title, milestone.state
                ));
            }
            let acceptance_configured = crate::acceptance::Check::from_run(&run)?.is_some();
            if acceptance_configured && run.state != "completed" {
                if let Some(proposal) = store.completion_proposal(run_id)? {
                    let proposal: Value = serde_json::from_slice(&store.artifact(&proposal)?)?;
                    let evidence: Vec<String> =
                        serde_json::from_value(proposal["evidence"].clone())?;
                    if let Err(error) = crate::acceptance::verified_result(
                        store,
                        &run,
                        proposal["summary"].as_str().unwrap_or_default(),
                        &evidence,
                    ) {
                        blockers.push(error.to_string());
                    }
                } else {
                    blockers.push(
                        "Independent acceptance requires a current completion proposal".into(),
                    );
                }
            }
            if !run.is_terminal() && run.state != "running" {
                blockers.push(format!("Task is {}", run.state));
            }
            Ok(
                json!({"state":run.state,"workspace_revision":revision,"blockers":blockers,"acceptance_configured":acceptance_configured,"note":"Read-only inspection. Finish still validates the actual proposal and acceptance transaction."}),
            )
        }
        "provider" => {
            if argument.is_some_and(|arg| arg != "history") {
                bail!("Use provider or provider history");
            }
            Ok(
                json!({"primary":{"provider":run.provider,"model":run.budgets["model"],"reasoning_effort":run.budgets["reasoning_effort"]},"current":route,"fallbacks":crate::routing::approved(&run)?,"history":store.events(run_id)?.into_iter().filter(|event|event.kind=="provider.transition").collect::<Vec<_>>()}),
            )
        }
        "budget" => usage(store, &run),
        "handoff" => crate::kernel::normalized_handoff(store, &run),
        _ => bail!("Unknown control view"),
    }
}

/// Captured in run.completed so the explanation survives subsequent inspection.
pub(crate) fn completion_report(
    connection: &rusqlite::Connection,
    run_id: &str,
    acceptance: Option<&str>,
) -> Result<Value> {
    let mut statement = connection.prepare("SELECT id,title,state,evidence,verified_revision,superseded_by,reason FROM obligations WHERE run_id=?1 ORDER BY id")?;
    let items = statement.query_map([run_id],|row| Ok(json!({"id":row.get::<_,i64>(0)?,"title":row.get::<_,String>(1)?,"state":row.get::<_,String>(2)?,"evidence":serde_json::from_str::<Value>(&row.get::<_,String>(3)?).unwrap_or(Value::Null),"revision":row.get::<_,Option<i64>>(4)?,"superseded_by":row.get::<_,Option<i64>>(5)?,"reason":row.get::<_,Option<String>>(6)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let revision: Option<i64> = connection
        .query_row(
            "SELECT revision FROM workspace_revisions WHERE run_id=?1",
            [run_id],
            |row| row.get(0),
        )
        .optional()?;
    let mut statement = connection.prepare(
        "SELECT payload FROM events WHERE run_id=?1 AND kind='provider.transition' ORDER BY seq",
    )?;
    let transitions = statement
        .query_map([run_id], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|text| serde_json::from_str::<Value>(&text))
        .collect::<serde_json::Result<Vec<_>>>()?;
    Ok(
        json!({"requirements":items,"workspace_revision":revision,"acceptance":acceptance,"provider_transitions":transitions,"verification_scope":"Successful operation provenance at the recorded revision; configured independent acceptance when present."}),
    )
}

fn route_label(route: &Value) -> String {
    format!(
        "{} / {} / {}",
        route["provider"].as_str().unwrap_or("unknown"),
        route["model"].as_str().unwrap_or("default"),
        route["reasoning_effort"]
            .as_str()
            .unwrap_or("default effort")
    )
}

pub fn display(command: &str, value: &Value) -> String {
    let mut lines = Vec::new();
    match command {
        "goal" | "contract" => {
            lines.push(value["goal"].as_str().unwrap_or_default().to_owned());
            for item in value["requirements"].as_array().into_iter().flatten() {
                lines.push(format!(
                    "O{}  {}  [{}]{}",
                    item["id"],
                    item["title"].as_str().unwrap_or_default(),
                    item["state"].as_str().unwrap_or_default(),
                    item["superseded_by"]
                        .as_i64()
                        .map(|id| format!(
                            " -> O{id}; {}",
                            item["reason"].as_str().unwrap_or_default()
                        ))
                        .unwrap_or_default()
                ));
            }
            lines.push(format!(
                "Workspace revision: {}",
                value["workspace_revision"]
            ));
            lines.push(format!(
                "Provider: {}",
                route_label(&value["current_provider"])
            ));
            if let Some(next) = value["checkpoint"]["next_action"].as_str() {
                lines.push(format!("Next action: {next}"));
            }
            for item in value["current_plan"].as_array().into_iter().flatten() {
                lines.push(format!(
                    "[{}] {}",
                    item["state"].as_str().unwrap_or_default(),
                    item["title"].as_str().unwrap_or_default()
                ));
            }
        }
        "status" => {
            lines.push(format!(
                "{}  {}",
                value["run"].as_str().unwrap_or_default(),
                value["state"].as_str().unwrap_or_default()
            ));
            lines.push(format!(
                "Provider: {}",
                route_label(&value["current_provider"])
            ));
            lines.push(format!(
                "Revision {} · requirements {} open / {} verified / {} stale",
                value["workspace_revision"],
                value["obligations"]["open"],
                value["obligations"]["verified"],
                value["obligations"]["stale"]
            ));
            lines.push(display("budget", &value["budget"]));
            if let Some(title) = value["active_milestone"]["title"].as_str() {
                lines.push(format!("Active: {title}"));
            }
            if let Some(capability) = value["last_operation"]["capability"].as_str() {
                lines.push(format!(
                    "Last operation: {} · {}",
                    capability,
                    value["last_operation"]["state"]
                        .as_str()
                        .unwrap_or_default()
                ));
            }
        }
        "budget" => {
            for name in [
                "actions",
                "model_tokens",
                "tool_result_tokens",
                "wall_seconds",
            ] {
                let metric = &value[name];
                lines.push(format!(
                    "{name}: {} used / {} limit / {} remaining",
                    metric["used"], metric["limit"], metric["remaining"]
                ));
            }
            lines.push(format!(
                "Provider input {} · cached {} · output {}",
                value["provider_usage"]["input"],
                value["provider_usage"]["cached_input"],
                value["provider_usage"]["output"]
            ));
        }
        "verify" => {
            lines.push(format!(
                "State {} · revision {}",
                value["state"].as_str().unwrap_or_default(),
                value["workspace_revision"]
            ));
            for blocker in value["blockers"].as_array().into_iter().flatten() {
                lines.push(format!("- {}", blocker.as_str().unwrap_or_default()));
            }
            if value["blockers"].as_array().is_some_and(Vec::is_empty) {
                lines.push("No persisted blockers; Finish must validate the actual evidence and acceptance proposal.".into());
            }
        }
        "why" => {
            lines.push(value["source"].as_str().unwrap_or_default().into());
            lines.push(format!(
                "Next action: {}",
                value["next_action"]
                    .as_str()
                    .unwrap_or("No checkpointed next action")
            ));
            for item in value["requirements_remaining"]
                .as_array()
                .into_iter()
                .flatten()
            {
                lines.push(format!(
                    "O{} remains {}: {}",
                    item["id"],
                    item["state"].as_str().unwrap_or_default(),
                    item["title"].as_str().unwrap_or_default()
                ));
            }
            for item in value["unresolved"].as_array().into_iter().flatten() {
                if let Some(text) = item.as_str() {
                    lines.push(format!("Unresolved: {text}"));
                }
            }
        }
        "provider" => {
            lines.push(format!("Primary: {}", route_label(&value["primary"])));
            lines.push(format!("Current: {}", route_label(&value["current"])));
            for route in value["fallbacks"].as_array().into_iter().flatten() {
                lines.push(format!("Fallback: {}", route_label(route)));
            }
            for event in value["history"].as_array().into_iter().flatten() {
                let transition = &event["payload"];
                lines.push(format!(
                    "{} -> {} · {} · attempt event {}",
                    route_label(&transition["from"]),
                    route_label(&transition["to"]),
                    transition["reason"].as_str().unwrap_or_default(),
                    transition["attempt_seq"]
                ));
            }
        }
        _ => return serde_json::to_string_pretty(value).unwrap_or_default(),
    }
    lines.join("\n")
}

pub fn display_completion(value: &Value) -> String {
    let mut lines = Vec::new();
    for item in value["requirements"].as_array().into_iter().flatten() {
        lines.push(format!(
            "O{} {} · {} · revision {}",
            item["id"],
            item["title"].as_str().unwrap_or_default(),
            item["state"].as_str().unwrap_or_default(),
            item["revision"]
        ));
        for hash in item["evidence"].as_array().into_iter().flatten() {
            if let Some(hash) = hash.as_str() {
                lines.push(format!("  artifact {hash}"));
            }
        }
        if let Some(next) = item["superseded_by"].as_i64() {
            lines.push(format!(
                "  replaced by O{next}: {}",
                item["reason"].as_str().unwrap_or_default()
            ));
        }
    }
    lines.push(format!(
        "Workspace revision: {}",
        value["workspace_revision"]
    ));
    lines.push(
        value["acceptance"]
            .as_str()
            .map(|hash| format!("Acceptance: verified artifact {hash}"))
            .unwrap_or("Acceptance: no independent check configured".into()),
    );
    for transition in value["provider_transitions"]
        .as_array()
        .into_iter()
        .flatten()
    {
        lines.push(format!(
            "{} -> {} · {}",
            route_label(&transition["from"]),
            route_label(&transition["to"]),
            transition["reason"].as_str().unwrap_or_default()
        ));
    }
    lines.join("\n")
}
