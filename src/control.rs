//! Deterministic user views of kernel-owned state. No inference or hidden reasoning.
use anyhow::{Context, Result, bail};
use rusqlite::params;
use serde_json::{Value, json};

use crate::storage::{Run, Store};

pub const VIEWS: &[&str] = &[
    "goal", "contract", "status", "why", "evidence", "verify", "provider", "budget", "handoff", "metrics",
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

pub fn recent_operation_outcomes(store: &Store, run_id: &str) -> Result<Value> {
    let current = store.workspace_revision(run_id)?;
    let mut outcomes = std::collections::BTreeMap::new();
    // Keep eight latest receipts plus four process receipts, so reads cannot hide a test failure.
    for process_only in [false, true] {
        let mut query = store.connection.prepare("SELECT operation.rowid,operation.id,operation.capability,operation.arguments,operation.state,operation.artifact,revision.revision FROM operations AS operation LEFT JOIN operation_revisions AS revision ON revision.operation_id=operation.id WHERE operation.run_id=?1 AND operation.artifact IS NOT NULL AND (?2=0 OR operation.capability='process.run') ORDER BY operation.rowid DESC LIMIT ?3")?;
        let rows = query
            .query_map(
                params![run_id, process_only, if process_only { 4 } else { 8 }],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, Option<i64>>(6)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (position, id, capability, args, state, hash, revision) in rows {
            let args: Value = serde_json::from_str(&args)?;
            let target = args["path"]
                .as_str()
                .or_else(|| args["program"].as_str())
                .map(|value| value.chars().take(200).collect::<String>());
            let receipt = if capability == "process.run" {
                let value = store
                    .artifact(&hash)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
                value.map(|value|json!({"exit_code":value["exit_code"],"output_artifact":value["output_artifact"],"bytes":value["bytes"]}))
            } else {
                None
            };
            outcomes.insert(position,json!({"operation":id,"capability":capability,"target":target,"state":state,"artifact":hash,"revision":revision,"current":revision.is_some() && revision==current,"successful_current_evidence":state=="succeeded" && revision.is_some() && revision==current,"receipt":receipt}));
        }
    }
    Ok(json!(outcomes.into_values().rev().collect::<Vec<_>>()))
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
            None,
        ),
        ("model_tokens", Some(store.model_tokens(&run.id)?), None),
        (
            "tool_result_tokens",
            Some(store.tool_result_tokens(&run.id)?),
            None,
        ),
        (
            "wall_seconds",
            elapsed,
            run.budgets["wall_seconds"].as_u64().or(Some(3600)),
        ),
    ] {
        result[name] = json!({"used":used,"limit":limit,"remaining":used.zip(limit).map(|(used,limit)|limit.saturating_sub(used))});
    }
    let metrics=crate::run_metrics::report(store,&run.id)?;
    let tokens=&metrics["tokens"];
    result["provider_usage"] = json!({"input":tokens["input"],"output":tokens["output"],"cached_input":tokens["cached_input"],"estimated":tokens["estimated_total"],"unclassified":tokens["unclassified_total"],"usage_complete":tokens["usage_complete"],"note":"Cached input is included in input. Missing provider receipts remain unmeasured; estimates and legacy totals are separate."});
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
    let events = store.events(run_id)?;
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
            let detail = events
                .iter()
                .rev()
                .find(|event| {
                    event.kind.starts_with("operation.")
                        && event.payload["id"] == op
                        && event.payload["detail"].is_object()
                })
                .map(|event| event.payload["detail"].clone());
            operations.push(json!({"operation":op,"capability":capability,"arguments":safe_arguments(&capability,serde_json::from_str::<Value>(&arguments)?),"state":state,"revision":recorded_revision,"current":state == "succeeded" && recorded_revision.is_some() && recorded_revision == revision,"receipt":detail}));
        }
        artifacts.push(json!({"artifact":hash,"operations":operations,"integrity":match store.artifact(hash) {Ok(_)=>"verified".to_owned(),Err(error)=>error.to_string()}}));
    }
    Ok(
        json!({"obligation":obligation,"workspace_revision":revision,"artifacts":artifacts,"verification_scope":"Provenance and revision checks; semantic coverage is not independently inferred."}),
    )
}

use rusqlite::OptionalExtension;

fn safe_arguments(capability: &str, mut arguments: Value) -> Value {
    if matches!(capability, "workspace.write" | "workspace.patch") {
        if let Some(object) = arguments.as_object_mut() {
            object.remove("content");
            object.remove("edits");
        }
    }
    arguments
}

pub fn view(store: &Store, run_id: &str, command: &str, argument: Option<&str>) -> Result<Value> {
    if argument.is_some() && !matches!(command, "goal" | "contract" | "provider" | "evidence") {
        bail!("This view takes no additional arguments");
    }
    if matches!(command, "goal" | "contract") && argument.is_some_and(|value| value != "history") {
        bail!("Use /goal, /goal history, /goal add, or /goal replace O2");
    }
    let run = store.run(run_id)?;
    let obligations = store.obligations(run_id)?;
    let revision = store.workspace_revision(run_id)?;
    let milestones = store.milestones(run_id)?;
    let checkpoint = store.last_checkpoint(run_id)?;
    let route = store.current_route(run_id)?;
    match command {
        "goal" | "contract" if argument == Some("history") => Ok(
            json!({"goal":run.task,"requirements":obligations,"history":store.events(run_id)?.into_iter().filter(|event|event.kind.starts_with("obligation.") || event.kind == "workspace.revision").collect::<Vec<_>>()}),
        ),
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
            for (path, _, _) in store.changed_observed_files(run_id)? {
                blockers.push(format!(
                    "Observed file changed since its recorded receipt: {path}"
                ));
            }
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
        "metrics" => crate::run_metrics::report(store, run_id),
        "handoff" => crate::kernel::normalized_handoff(store, &run),
        _ => bail!("Unknown control view"),
    }
}

/// Captured in run.completed so the explanation survives subsequent inspection.
pub(crate) fn completion_report(
    connection: &rusqlite::Connection,
    run_id: &str,
    acceptance: Option<&str>,
    prior_transitions: &[(i64, Value)],
    receipts: &std::collections::BTreeMap<String, Value>,
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
    let mut transitions: std::collections::BTreeMap<i64, Value> =
        prior_transitions.iter().cloned().collect();
    // Preserve archived history and include transitions committed since the snapshot.
    let mut statement=connection.prepare("SELECT seq,payload FROM events WHERE run_id=?1 AND kind='provider.transition' ORDER BY seq")?;
    for (seq, text) in statement
        .query_map([run_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    {
        transitions.insert(seq, serde_json::from_str(&text)?);
    }
    let mut artifacts = Vec::new();
    for (hash, receipt) in receipts {
        let mut statement=connection.prepare("SELECT operation.id,operation.capability,operation.arguments,operation.state,revision.revision FROM operations AS operation LEFT JOIN operation_revisions AS revision ON revision.operation_id=operation.id WHERE operation.run_id=?1 AND (operation.artifact=?2 OR EXISTS(SELECT 1 FROM operation_artifacts AS linked WHERE linked.operation_id=operation.id AND linked.hash=?2)) ORDER BY operation.rowid")?;
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
        let operations=sources.into_iter().map(|(id,capability,args,state,revision)|Ok(json!({"id":id,"capability":capability,"arguments":safe_arguments(&capability,serde_json::from_str::<Value>(&args)?),"state":state,"revision":revision}))).collect::<Result<Vec<Value>>>()?;
        artifacts.push(json!({"artifact":hash,"operations":operations,"receipt":receipt}));
    }
    Ok(
        json!({"requirements":items,"workspace_revision":revision,"acceptance":acceptance,"provider_transitions":transitions.into_values().collect::<Vec<_>>(),"artifacts":artifacts,"verification_scope":"Successful operation provenance at the recorded revision; configured independent acceptance when present."}),
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
    if command == "metrics" { return crate::run_metrics::display(value); }
    if matches!(command, "goal" | "contract") && value["history"].is_array() {
        return display("goal_history", value);
    }
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
            for route in value["fallbacks"].as_array().into_iter().flatten() {
                lines.push(format!("Fallback: {}", route_label(route)));
            }
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
        "goal_history" => {
            for event in value["history"].as_array().into_iter().flatten() {
                lines.push(format!(
                    "event {} · {} · {}",
                    event["seq"],
                    event["kind"].as_str().unwrap_or_default(),
                    event["payload"]
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
            for route in value["fallbacks"].as_array().into_iter().flatten() {
                lines.push(format!("Fallback: {}", route_label(route)));
            }
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
            let turns = &value["actions"];
            let model_tokens = &value["model_tokens"];
            let tool_tokens = &value["tool_result_tokens"];
            let wall = &value["wall_seconds"];
            lines.push(format!(
                "Model responses: {} recorded · no turn cap",
                turns["used"]
            ));
            lines.push(format!(
                "Model tokens: {} recorded · no token cap",
                model_tokens["used"]
            ));
            lines.push(format!(
                "Tool-result tokens: {} recorded · no token cap",
                tool_tokens["used"]
            ));
            lines.push(format!(
                "Wall time: {}s elapsed / {}s limit / {}s remaining",
                wall["used"], wall["limit"], wall["remaining"]
            ));
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
                    "{} -> {} · {} · {}",
                    route_label(&transition["from"]),
                    route_label(&transition["to"]),
                    transition["reason"].as_str().unwrap_or_default(),
                    transition_position(transition)
                ));
            }
        }
        _ => return serde_json::to_string_pretty(value).unwrap_or_default(),
    }
    lines.join("\n")
}

fn transition_position(value: &Value) -> String {
    value["turn"]
        .as_u64()
        .map(|turn| format!("turn {turn}"))
        .unwrap_or_else(|| format!("attempt event {}", value["attempt_seq"]))
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
                if let Some(artifact) = value["artifacts"]
                    .as_array()
                    .and_then(|items| items.iter().find(|item| item["artifact"] == hash))
                {
                    for source in artifact["operations"].as_array().into_iter().flatten() {
                        lines.push(format!(
                            "  {} {} · {} · exit {}",
                            source["capability"].as_str().unwrap_or("operation"),
                            source["arguments"],
                            source["state"].as_str().unwrap_or("unknown"),
                            artifact["receipt"]["exit_code"]
                        ));
                    }
                }
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
            "{} -> {} · {} · {}",
            route_label(&transition["from"]),
            route_label(&transition["to"]),
            transition["reason"].as_str().unwrap_or_default(),
            transition_position(transition)
        ));
    }
    lines.join("\n")
}
