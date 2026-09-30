use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::storage::{Run, Store, append_event};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<crate::endpoint::Endpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
}

pub(crate) fn valid_key_reference(reference: &str) -> bool {
    !reference.is_empty()
        && reference.len() <= 128
        && !reference.as_bytes()[0].is_ascii_digit()
        && reference
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    UsageLimit,
    Outage,
    ModelRemoved,
}

impl Reason {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::UsageLimit => "usage_limit",
            Self::Outage => "provider_outage",
            Self::ModelRemoved => "model_removed",
        }
    }
}

impl Route {
    pub fn validate(&self) -> Result<()> {
        match self.provider.as_str() {
            "codex" | "grok" if self.endpoint.is_none() && self.api_key_env.is_none() => {}
            "claude-api" if self.endpoint.is_none() => {
                if !self.api_key_env.as_deref().is_some_and(valid_key_reference) {
                    bail!("Claude API fallback requires a session-only API-key reference");
                }
            }
            "custom" if self.api_key_env.is_none() => {
                self.endpoint
                    .as_ref()
                    .context("custom fallback requires an OpenAI-compatible endpoint")?
                    .url()?;
            }
            _ => bail!(
                "fallback route must be direct ChatGPT/Grok, Claude API, or a validated custom endpoint"
            ),
        }
        if !crate::catalog::valid_id(&self.model)
            || self
                .reasoning_effort
                .as_deref()
                .is_some_and(|effort| !crate::catalog::valid_effort(effort))
        {
            bail!("fallback model or reasoning level is invalid");
        }
        Ok(())
    }

    pub fn configuration(&self, run: &Run) -> Value {
        if self.provider == run.provider {
            return run.budgets.clone();
        }
        let mut configuration = run.budgets.clone();
        configuration["provider_transport"] = json!(if self.provider == "claude-api" {
            "aegis-claude-api-v1"
        } else {
            "aegis-direct-v1"
        });
        configuration["api_key_env"] = json!(self.api_key_env);
        configuration["model"] = json!(self.model);
        configuration["reasoning_effort"] = json!(self.reasoning_effort);
        configuration["endpoint"] = json!(self.endpoint);
        configuration
    }
}

pub fn approved(run: &Run) -> Result<Vec<Route>> {
    let Some(value) = run.budgets.get("fallback_routes") else {
        return Ok(Vec::new());
    };
    let routes: Vec<Route> = serde_json::from_value(value.clone())
        .context("fallback_routes must be a reviewed list of provider routes")?;
    if routes.len() > 3 {
        bail!("at most three fallback routes may be approved");
    }
    if !routes.is_empty()
        && !matches!(
            run.provider.as_str(),
            "codex" | "grok" | "custom" | "claude-api"
        )
    {
        bail!(
            "automatic fallback requires an Aegis direct, Claude API, or custom primary provider"
        );
    }
    let mut providers = std::collections::HashSet::new();
    providers.insert(run.provider.as_str());
    let mut key_references = std::collections::HashSet::new();
    for reference in [
        run.budgets["api_key_env"].as_str(),
        run.budgets
            .pointer("/endpoint/api_key_env")
            .and_then(Value::as_str),
    ]
    .into_iter()
    .flatten()
    {
        if !valid_key_reference(reference) || !key_references.insert(reference) {
            bail!("provider key references must be valid and distinct");
        }
    }
    for route in &routes {
        route.validate()?;
        if !providers.insert(&route.provider) {
            bail!("fallback providers must be distinct from the primary and each other");
        }
        for reference in [
            route.api_key_env.as_deref(),
            route
                .endpoint
                .as_ref()
                .and_then(|endpoint| endpoint.api_key_env.as_deref()),
        ]
        .into_iter()
        .flatten()
        {
            if !valid_key_reference(reference) || !key_references.insert(reference) {
                bail!("provider key references must be valid and distinct");
            }
        }
    }
    if !routes.is_empty() {
        let expected = match run.provider.as_str() {
            "custom" => None,
            "claude-api" => Some("aegis-claude-api-v1"),
            _ => Some("aegis-direct-v1"),
        };
        if expected.is_some_and(|transport| run.budgets["provider_transport"] != transport) {
            bail!("automatic fallback requires a verified primary transport");
        }
    }
    Ok(routes)
}

impl Store {
    pub fn current_route(&self, run_id: &str) -> Result<Route> {
        let run = self.run(run_id)?;
        if let Some(route) = self
            .connection
            .query_row(
                "SELECT route FROM provider_routes WHERE run_id = ?1",
                [run_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            let route: Route = serde_json::from_str(&route)?;
            if !approved(&run)?.contains(&route) {
                bail!("stored provider route is not approved by this task contract");
            }
            return Ok(route);
        }
        Ok(Route {
            provider: run.provider,
            model: run.budgets["model"].as_str().unwrap_or_default().into(),
            reasoning_effort: run.budgets["reasoning_effort"].as_str().map(str::to_owned),
            endpoint: serde_json::from_value(run.budgets["endpoint"].clone())?,
            api_key_env: None,
        })
    }

    pub fn transition_provider(&mut self, run_id: &str, reason: Reason) -> Result<Option<Route>> {
        let run = self.run(run_id)?;
        if run.state != "running" {
            bail!("only a running task may transition providers");
        }
        let routes = approved(&run)?;
        if routes.is_empty() || self.unknown_count(run_id)? > 0 {
            return Ok(None);
        }
        let active: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM operations WHERE run_id = ?1 AND state IN ('pending','dispatched','executing','unknown')",
            [run_id],
            |row| row.get(0),
        )?;
        if active > 0 {
            return Ok(None);
        }
        let current = self.current_route(run_id)?;
        let latest = self.recent_events(run_id, 1)?;
        if latest.first().is_none_or(|event| {
            event.kind != "model.failed" || event.payload["recoverable_reason"] != reason.name()
        }) {
            return Ok(None);
        }
        let next = if current.provider == run.provider {
            routes.first()
        } else {
            routes
                .iter()
                .position(|route| route == &current)
                .and_then(|index| routes.get(index + 1))
        };
        let Some(next) = next.cloned() else {
            return Ok(None);
        };
        let revision = self.workspace_revision(run_id)?;
        let tokens = self.model_tokens(run_id)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state: String =
            transaction.query_row("SELECT state FROM runs WHERE id = ?1", [run_id], |row| {
                row.get(0)
            })?;
        if state != "running" {
            bail!("task ended before provider transition");
        }
        let attempt: i64 = transaction.query_row(
            "SELECT COALESCE(MAX(seq),0) FROM events WHERE run_id = ?1 AND kind = 'model.started'",
            [run_id],
            |row| row.get(0),
        )?;
        let turn = if attempt > 0 {
            let payload: String = transaction.query_row(
                "SELECT payload FROM events WHERE run_id = ?1 AND seq = ?2",
                params![run_id, attempt],
                |row| row.get(0),
            )?;
            serde_json::from_str::<Value>(&payload)?["turn"].as_u64()
        } else {
            None
        };
        transaction.execute(
            "INSERT INTO provider_routes(run_id, route) VALUES (?1, ?2) ON CONFLICT(run_id) DO UPDATE SET route = excluded.route",
            params![run_id, serde_json::to_string(&next)?],
        )?;
        append_event(
            &transaction,
            run_id,
            "provider.transition",
            json!({"from":current,"to":next,"reason":reason.name(),"attempt_seq":attempt,"turn":turn,"workspace_revision":revision,"model_tokens":tokens}),
        )?;
        transaction.commit()?;
        Ok(Some(next))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approved_transition_keeps_run_and_ledger_and_survives_restart() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "Refactor parser\nRequirements:\n- preserve API",
            directory.path(),
            "codex",
            json!([]),
            json!({"provider_transport":"aegis-direct-v1","model":"primary","fallback_routes":[{"provider":"grok","model":"fallback"}]}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let operation = store.begin_operation(&run.id, "workspace.read", json!({}), true)?;
        let evidence = store.put_artifact(b"compatibility proof")?;
        store.operation_state(&operation, "succeeded", Some(&evidence), json!({}))?;
        store.verify_obligation(&run.id, 1, &[evidence.clone()])?;
        let checkpoint = crate::model::Checkpoint {
            decisions: vec!["keep API".into()],
            unresolved: Vec::new(),
            next_action: "run tests".into(),
            milestones: Vec::new(),
        };
        store.save_checkpoint(&run.id, &checkpoint)?;
        store.event(&run.id, "model.started", json!({"turn":1}))?;
        store.event(&run.id, "model.failed", json!({"error":"bad JSON"}))?;
        assert!(
            store
                .transition_provider(&run.id, Reason::UsageLimit)?
                .is_none()
        );
        store.event(&run.id, "model.started", json!({"turn":2}))?;
        store.event(
            &run.id,
            "model.failed",
            json!({"error":"quota","recoverable_reason":"usage_limit","usage":{"input_tokens":7,"output_tokens":3}}),
        )?;
        let before = store.obligations(&run.id)?;
        let fallback = store
            .transition_provider(&run.id, Reason::UsageLimit)?
            .unwrap();
        assert_eq!(fallback.provider, "grok");
        assert_eq!(store.run(&run.id)?.provider, "codex");
        assert_eq!(store.model_tokens(&run.id)?, 10);
        assert_eq!(store.obligations(&run.id)?, before);
        assert_eq!(store.last_checkpoint(&run.id)?, Some(checkpoint.clone()));
        assert!(store.has_evidence(&run.id, &evidence)?);
        assert_eq!(store.operations(&run.id)?.len(), 1);
        assert_eq!(store.run(&run.id)?.budgets, run.budgets);
        assert_eq!(store.run(&run.id)?.grants, run.grants);
        assert_eq!(store.event_count(&run.id, "provider.transition")?, 1);
        assert!(
            store
                .transition_provider(&run.id, Reason::Outage)?
                .is_none()
        );
        drop(store);
        let store = Store::open(directory.path())?;
        assert_eq!(store.current_route(&run.id)?, fallback);
        assert_eq!(store.run(&run.id)?.id, run.id);
        assert_eq!(store.last_checkpoint(&run.id)?, Some(checkpoint));
        assert!(store.has_evidence(&run.id, &evidence)?);
        Ok(())
    }

    #[test]
    fn claude_api_quota_failover_keeps_contract_and_drops_primary_key_reference() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "Refactor parser\nRequirements:\n- preserve API",
            directory.path(),
            "claude-api",
            json!(["workspace.read"]),
            json!({"provider_transport":"aegis-claude-api-v1","model":"claude-account-model","api_key_env":"CLAUDE_SESSION_KEY","fallback_routes":[{"provider":"codex","model":"account-model"},{"provider":"grok","model":"other-model"}]}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let obligation = store.obligations(&run.id)?;
        store.event(&run.id, "model.started", json!({"turn":1}))?;
        store.event(
            &run.id,
            "model.failed",
            json!({"recoverable_reason":"usage_limit"}),
        )?;
        let first = store
            .transition_provider(&run.id, Reason::UsageLimit)?
            .unwrap();
        assert_eq!(first.provider, "codex");
        assert_eq!(
            first.configuration(&run)["provider_transport"],
            "aegis-direct-v1"
        );
        assert_eq!(first.configuration(&run)["api_key_env"], Value::Null);
        assert_eq!(
            store.run(&run.id)?.budgets["api_key_env"],
            "CLAUDE_SESSION_KEY"
        );
        assert_eq!(store.obligations(&run.id)?, obligation);
        store.event(&run.id, "model.started", json!({"turn":2}))?;
        store.event(
            &run.id,
            "model.failed",
            json!({"recoverable_reason":"provider_outage"}),
        )?;
        let second = store.transition_provider(&run.id, Reason::Outage)?.unwrap();
        assert_eq!(second.provider, "grok");
        assert_eq!(store.run(&run.id)?.provider, "claude-api");
        assert_eq!(store.obligations(&run.id)?, obligation);
        assert_eq!(store.event_count(&run.id, "provider.transition")?, 2);
        drop(store);
        let store = Store::open(directory.path())?;
        assert_eq!(store.current_route(&run.id)?, second);

        let mut store = Store::open(directory.path())?;
        assert!(store.create_run(
            "invalid transport",
            directory.path(),
            "claude-api",
            json!([]),
            json!({"provider_transport":"aegis-direct-v1","model":"claude-account-model","fallback_routes":[{"provider":"codex","model":"account-model"}]}),
            "",
        ).is_err());
        Ok(())
    }

    #[test]
    fn claude_api_fallback_uses_a_distinct_reviewed_key_reference() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "Repair parser",
            directory.path(),
            "codex",
            json!([]),
            json!({"provider_transport":"aegis-direct-v1","model":"account-model","fallback_routes":[{"provider":"claude-api","model":"claude-account-model","api_key_env":"CLAUDE_FALLBACK_KEY"}]}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        store.event(&run.id, "model.started", json!({"turn":1}))?;
        store.event(
            &run.id,
            "model.failed",
            json!({"recoverable_reason":"usage_limit"}),
        )?;
        let fallback = store
            .transition_provider(&run.id, Reason::UsageLimit)?
            .unwrap();
        assert_eq!(fallback.provider, "claude-api");
        assert_eq!(
            fallback.configuration(&run)["provider_transport"],
            "aegis-claude-api-v1"
        );
        assert_eq!(
            fallback.configuration(&run)["api_key_env"],
            "CLAUDE_FALLBACK_KEY"
        );
        assert_eq!(store.current_route(&run.id)?, fallback);

        for invalid in [
            json!({"provider":"claude-api","model":"claude-account-model"}),
            json!({"provider":"claude-api","model":"claude-account-model","api_key_env":"BAD-NAME"}),
            json!({"provider":"grok","model":"other-model","api_key_env":"CLAUDE_FALLBACK_KEY"}),
        ] {
            assert!(store.create_run(
                "invalid route",
                directory.path(),
                "codex",
                json!([]),
                json!({"provider_transport":"aegis-direct-v1","model":"account-model","fallback_routes":[invalid]}),
                "",
            ).is_err());
        }
        assert!(store.create_run(
            "colliding keys",
            directory.path(),
            "custom",
            json!([]),
            json!({"model":"local-model","endpoint":{"base_url":"https://example.test/v1","api_key_env":"SHARED_KEY"},"fallback_routes":[{"provider":"claude-api","model":"claude-account-model","api_key_env":"SHARED_KEY"}]}),
            "",
        ).is_err());
        Ok(())
    }

    #[test]
    fn unapproved_routes_and_uncertain_operations_cannot_transition() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        for routes in [
            json!([{"provider":"claude","model":"sonnet"}]),
            json!([{"provider":"codex","model":"same"}]),
            json!([{"provider":"grok","model":""}]),
        ] {
            assert!(store.create_run("work", directory.path(), "codex", json!([]), json!({"provider_transport":"aegis-direct-v1","model":"primary","fallback_routes":routes}), "").is_err());
        }
        let run = store.create_run("work", directory.path(), "codex", json!([]), json!({"provider_transport":"aegis-direct-v1","model":"primary","fallback_routes":[{"provider":"grok","model":"fallback"}]}), "")?;
        store.state(&run.id, "running", json!({}))?;
        store.begin_operation(&run.id, "workspace.read", json!({}), true)?;
        assert!(
            store
                .transition_provider(&run.id, Reason::UsageLimit)?
                .is_none()
        );
        assert_eq!(store.event_count(&run.id, "provider.transition")?, 0);
        Ok(())
    }

    #[test]
    fn primary_route_does_not_reinterpret_legacy_transport_as_direct() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "old task",
            directory.path(),
            "codex",
            json!([]),
            json!({"model":"legacy-model"}),
            "",
        )?;
        let route = store.current_route(&run.id)?;
        assert_eq!(route.provider, "codex");
        assert_eq!(route.configuration(&run)["provider_transport"], Value::Null);
        Ok(())
    }

    #[test]
    fn approved_custom_route_uses_only_its_reviewed_endpoint_after_restart() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let endpoint = json!({"base_url":"http://127.0.0.1:1234/v1","api_key_env":null,"response_format":"schema","allow_insecure":false});
        let run = store.create_run(
            "Repair parser",
            directory.path(),
            "codex",
            json!([]),
            json!({"provider_transport":"aegis-direct-v1","model":"primary","fallback_routes":[{"provider":"custom","model":"qwen-local","endpoint":endpoint}]}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        store.event(&run.id, "model.started", json!({"turn":1}))?;
        store.event(
            &run.id,
            "model.failed",
            json!({"recoverable_reason":"provider_outage"}),
        )?;
        let route = store.transition_provider(&run.id, Reason::Outage)?.unwrap();
        assert_eq!(route.provider, "custom");
        assert_eq!(route.configuration(&run)["model"], "qwen-local");
        assert_eq!(route.configuration(&run)["endpoint"], endpoint);
        assert_eq!(store.run(&run.id)?.provider, "codex");
        drop(store);
        assert_eq!(
            Store::open(directory.path())?.current_route(&run.id)?,
            route
        );
        Ok(())
    }

    #[test]
    fn custom_primary_can_transition_without_replacing_its_contract() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let endpoint = json!({"base_url":"http://127.0.0.1:1234/v1","api_key_env":null,"response_format":"schema","allow_insecure":false});
        let run = store.create_run(
            "Refactor parser\nRequirements:\n- preserve API",
            directory.path(),
            "custom",
            json!([]),
            json!({"model":"qwen-local","endpoint":endpoint,"fallback_routes":[{"provider":"codex","model":"gpt-fallback"}]}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let operation = store.begin_operation(&run.id, "workspace.read", json!({}), true)?;
        let evidence = store.put_artifact(b"API compatibility proof")?;
        store.operation_state(&operation, "succeeded", Some(&evidence), json!({}))?;
        store.verify_obligation(&run.id, 1, &[evidence.clone()])?;
        store.event(&run.id, "model.started", json!({"turn":1}))?;
        store.event(
            &run.id,
            "model.failed",
            json!({"recoverable_reason":"provider_outage"}),
        )?;
        let obligations = store.obligations(&run.id)?;
        let route = store.transition_provider(&run.id, Reason::Outage)?.unwrap();
        assert_eq!(route.provider, "codex");
        assert_eq!(
            route.configuration(&run)["provider_transport"],
            "aegis-direct-v1"
        );
        assert_eq!(route.configuration(&run)["model"], "gpt-fallback");
        assert_eq!(route.configuration(&run)["endpoint"], Value::Null);
        assert_eq!(store.run(&run.id)?.budgets, run.budgets);
        assert_eq!(store.obligations(&run.id)?, obligations);
        assert!(store.has_evidence(&run.id, &evidence)?);
        assert_eq!(store.event_count(&run.id, "provider.transition")?, 1);
        drop(store);
        assert_eq!(
            Store::open(directory.path())?.current_route(&run.id)?,
            route
        );
        Ok(())
    }

    #[test]
    fn resumed_route_cannot_change_endpoint_outside_the_frozen_contract() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let endpoint = json!({"base_url":"http://127.0.0.1:1234/v1","api_key_env":"FALLBACK_KEY","response_format":"schema","allow_insecure":false});
        let run = store.create_run(
            "Repair parser",
            directory.path(),
            "codex",
            json!([]),
            json!({"provider_transport":"aegis-direct-v1","model":"primary","fallback_routes":[{"provider":"custom","model":"reviewed-model","endpoint":endpoint}]}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        store.event(&run.id, "model.started", json!({"turn":1}))?;
        store.event(
            &run.id,
            "model.failed",
            json!({"recoverable_reason":"provider_outage"}),
        )?;
        let approved = store.transition_provider(&run.id, Reason::Outage)?.unwrap();
        assert_eq!(store.current_route(&run.id)?, approved);
        let mut changed = serde_json::to_value(&approved)?;
        changed["endpoint"]["base_url"] = json!("https://unreviewed.example/v1");
        store.connection.execute(
            "UPDATE provider_routes SET route = ?2 WHERE run_id = ?1",
            params![run.id, serde_json::to_string(&changed)?],
        )?;
        assert!(store.current_route(&run.id).is_err());
        drop(store);
        assert!(
            Store::open(directory.path())?
                .current_route(&run.id)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn classified_failures_follow_reviewed_fallback_order_once_each() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "Repair parser",
            directory.path(),
            "codex",
            json!([]),
            json!({"provider_transport":"aegis-direct-v1","model":"primary","fallback_routes":[{"provider":"grok","model":"grok-model"},{"provider":"custom","model":"local-model","endpoint":{"base_url":"http://127.0.0.1:1234/v1","api_key_env":null}}]}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        store.event(&run.id, "model.started", json!({"turn":1}))?;
        store.event(
            &run.id,
            "model.failed",
            json!({"recoverable_reason":"usage_limit"}),
        )?;
        assert_eq!(
            store
                .transition_provider(&run.id, Reason::UsageLimit)?
                .unwrap()
                .provider,
            "grok"
        );
        store.event(&run.id, "model.started", json!({"turn":2}))?;
        store.event(
            &run.id,
            "model.failed",
            json!({"recoverable_reason":"provider_outage"}),
        )?;
        let second = store.transition_provider(&run.id, Reason::Outage)?.unwrap();
        assert_eq!(second.provider, "custom");
        assert_eq!(second.model, "local-model");
        assert_eq!(store.event_count(&run.id, "provider.transition")?, 2);
        assert!(
            store
                .transition_provider(&run.id, Reason::Outage)?
                .is_none()
        );
        drop(store);
        assert_eq!(
            Store::open(directory.path())?.current_route(&run.id)?,
            second
        );
        Ok(())
    }
}
