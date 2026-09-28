use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::storage::{Run, Store, append_event};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    UsageLimit,
    Outage,
    ModelRemoved,
}

impl Reason {
    fn name(self) -> &'static str {
        match self {
            Self::UsageLimit => "usage_limit",
            Self::Outage => "provider_outage",
            Self::ModelRemoved => "model_removed",
        }
    }
}

impl Route {
    pub fn validate(&self) -> Result<()> {
        if !matches!(self.provider.as_str(), "codex" | "grok") {
            bail!("automatic fallback supports only direct ChatGPT or Grok sign-in");
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
        let mut configuration = run.budgets.clone();
        configuration["provider_transport"] = json!("aegis-direct-v1");
        configuration["model"] = json!(self.model);
        configuration["reasoning_effort"] = json!(self.reasoning_effort);
        configuration
    }
}

pub fn approved(run: &Run) -> Result<Vec<Route>> {
    let Some(value) = run.budgets.get("fallback_routes") else {
        return Ok(Vec::new());
    };
    let routes: Vec<Route> = serde_json::from_value(value.clone())
        .context("fallback_routes must be a reviewed list of direct routes")?;
    if routes.len() > 3 {
        bail!("at most three fallback routes may be approved");
    }
    let mut providers = std::collections::HashSet::new();
    providers.insert(run.provider.as_str());
    for route in &routes {
        route.validate()?;
        if !providers.insert(&route.provider) {
            bail!("fallback providers must be distinct from the primary and each other");
        }
    }
    if !routes.is_empty() && run.budgets["provider_transport"] != "aegis-direct-v1" {
        bail!("automatic fallback requires a direct primary provider");
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
            return Ok(serde_json::from_str(&route)?);
        }
        Ok(Route {
            provider: run.provider,
            model: run.budgets["model"].as_str().unwrap_or_default().into(),
            reasoning_effort: run.budgets["reasoning_effort"].as_str().map(str::to_owned),
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
        let attempt: i64 = transaction.query_row(
            "SELECT COALESCE(MAX(seq),0) FROM events WHERE run_id = ?1 AND kind = 'model.started'",
            [run_id],
            |row| row.get(0),
        )?;
        transaction.execute(
            "INSERT INTO provider_routes(run_id, route) VALUES (?1, ?2) ON CONFLICT(run_id) DO UPDATE SET route = excluded.route",
            params![run_id, serde_json::to_string(&next)?],
        )?;
        append_event(
            &transaction,
            run_id,
            "provider.transition",
            json!({"from":current,"to":next,"reason":reason.name(),"attempt_seq":attempt,"workspace_revision":revision,"model_tokens":tokens}),
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
        store.event(&run.id, "model.started", json!({"turn":1}))?;
        store.event(
            &run.id,
            "model.failed",
            json!({"error":"quota","usage":{"input_tokens":7,"output_tokens":3}}),
        )?;
        let before = store.obligations(&run.id)?;
        let fallback = store
            .transition_provider(&run.id, Reason::UsageLimit)?
            .unwrap();
        assert_eq!(fallback.provider, "grok");
        assert_eq!(store.run(&run.id)?.provider, "codex");
        assert_eq!(store.model_tokens(&run.id)?, 10);
        assert_eq!(store.obligations(&run.id)?, before);
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
}
