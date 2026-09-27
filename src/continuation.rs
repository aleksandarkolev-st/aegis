use std::fs::File;
use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::TransactionBehavior;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::storage::{Run, Store, append_event, insert_run};

pub struct Review {
    child_id: String,
    source: Run,
    sequence: i64,
    provider: String,
    model: String,
    reasoning: Option<String>,
    handoff: Value,
}

impl Review {
    pub fn source(&self) -> &Run {
        &self.source
    }

    pub fn handoff(&self) -> &Value {
        &self.handoff
    }
}

pub fn needed(run: &Run) -> bool {
    matches!(
        crate::provider::canonical(&run.provider),
        "codex" | "grok" | "claude"
    ) && run.budgets["provider_transport"] != "aegis-direct-v1"
}

fn eligible(store: &Store, source: &Run, workspace: &Path) -> Result<()> {
    if !needed(source) {
        bail!("This task does not require a direct-provider continuation");
    }
    if dunce::canonicalize(workspace)? != dunce::canonicalize(&source.workspace)? {
        bail!("Open the original workspace before continuing this task");
    }
    let unfinished = store.unresolved(&source.id)?;
    if store.unknown_count(&source.id)? > 0
        || unfinished
            .iter()
            .any(|operation| operation.state != "pending" || !source.is_terminal())
    {
        bail!(
            "Resolve unfinished operations or cancel the original task; uncertain outcomes still need review. No operation will be repeated"
        );
    }
    let _: Vec<String> = serde_json::from_value(source.grants.clone())?;
    crate::instructions::frozen(&source.budgets)?;
    crate::repository_rules::frozen(&source.budgets)?;
    crate::tokenization::validate(&source.budgets)?;
    crate::budget::response_bytes(&source.budgets)?;
    crate::policy::CommandScopes::from_configuration(&source.budgets)?;
    crate::filesystem::FileScopes::from_configuration(&source.budgets)?;
    crate::network::NetworkScopes::from_configuration(&source.budgets)?;
    crate::acceptance::Check::from_run(source)?;
    Ok(())
}

pub fn prepare(
    store: &Store,
    source_id: &str,
    workspace: &Path,
    provider: &str,
    model: &str,
    reasoning: Option<&str>,
) -> Result<Review> {
    crate::direct::provider(provider)?;
    if !crate::catalog::valid_id(model) {
        bail!("Choose a valid explicit model before continuing");
    }
    if reasoning.is_some_and(|effort| !crate::catalog::valid_effort(effort)) {
        bail!("Choose a valid reasoning effort before continuing");
    }
    let source = store.run(source_id)?;
    eligible(store, &source, workspace)?;
    let sequence = store.connection.query_row(
        "SELECT last_seq FROM run_projection WHERE run_id=?1",
        [source_id],
        |row| row.get(0),
    )?;
    let handoff = store.last_checkpoint(source_id)?.map(|checkpoint| {
        let preview = |text: &str, count| crate::text::clean(text).chars().take(count).collect::<String>();
        let clipped = checkpoint.decisions.len() > 4
            || checkpoint.unresolved.len() > 4
            || checkpoint.decisions.iter().chain(&checkpoint.unresolved).any(|text| text.chars().count() > 256);
        json!({
            "next_action":preview(&checkpoint.next_action, 1000),
            "decisions":checkpoint.decisions.iter().take(4).map(|text| preview(text, 256)).collect::<Vec<_>>(),
            "unresolved":checkpoint.unresolved.iter().take(4).map(|text| preview(text, 256)).collect::<Vec<_>>(),
            "clipped":clipped,
            "context_only":true,
        })
    }).unwrap_or(Value::Null);
    Ok(Review {
        child_id: uuid::Uuid::new_v4().to_string(),
        source,
        sequence,
        provider: crate::provider::canonical(provider).into(),
        model: model.into(),
        reasoning: reasoning.map(str::to_owned),
        handoff,
    })
}

pub fn commit(store: &mut Store, root: &Path, review: &Review) -> Result<Run> {
    let database = Path::new(
        store
            .connection
            .path()
            .context("Task database path missing")?,
    );
    if dunce::canonicalize(database.parent().context("Task database root missing")?)?
        != dunce::canonicalize(root)?
    {
        bail!("Continuation must use the source task database");
    }
    uuid::Uuid::parse_str(&review.source.id)?;
    let lock = File::options()
        .create(true)
        .write(true)
        .open(root.join(format!("run-{}.lock", review.source.id)))?;
    fs2::FileExt::try_lock_exclusive(&lock).context("Pause the active task before continuing")?;
    let transaction =
        rusqlite::Transaction::new_unchecked(&store.connection, TransactionBehavior::Immediate)?;
    let already_created: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM runs WHERE id=?1)",
        [&review.child_id],
        |row| row.get(0),
    )?;
    if already_created {
        bail!("This continuation was already created; open it from saved chats");
    }
    let current = store.run(&review.source.id)?;
    let sequence: i64 = transaction.query_row(
        "SELECT last_seq FROM run_projection WHERE run_id=?1",
        [&current.id],
        |row| row.get(0),
    )?;
    if sequence != review.sequence
        || serde_json::to_value(&current)? != serde_json::to_value(&review.source)?
    {
        bail!("This task changed during review; review its latest state before continuing");
    }
    eligible(store, &current, Path::new(&current.workspace))?;
    let mut budgets = current.budgets.clone();
    budgets["provider_transport"] = json!("aegis-direct-v1");
    budgets["model"] = json!(review.model);
    budgets["reasoning_effort"] = json!(review.reasoning);
    budgets["previous_run"] = json!(current.id);
    budgets["continuation_handoff"] = review.handoff.clone();
    budgets
        .as_object_mut()
        .context("Invalid source task configuration")?
        .remove("endpoint");
    let child = Run {
        id: review.child_id.clone(),
        task: current.task.clone(),
        workspace: current.workspace.clone(),
        provider: review.provider.clone(),
        grants: current.grants.clone(),
        budgets,
        acceptance: current.acceptance.clone(),
        state: "ready".into(),
        created_at: crate::storage::now(),
    };
    crate::acceptance::Check::from_run(&child)?;
    insert_run(&transaction, &child)?;
    append_event(
        &transaction,
        &child.id,
        "continuation.created",
        json!({
            "source_run":current.id,"source_sequence":sequence,
            "source_contract":hex::encode(Sha256::digest(serde_json::to_vec(&current)?)),
            "policy":"Reviewed new task; original contract and evidence are unchanged. Old operations are not replayed. Frozen guidance and permissions are preserved. Budgets restart only for this new task.",
        }),
    )?;
    transaction.commit()?;
    Ok(child)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(store: &mut Store, workspace: &Path) -> Result<Run> {
        store.remember(workspace, "Prefer focused changes", None)?;
        store.create_run(
            "Finish the parser",
            workspace,
            "codex",
            json!([
                "workspace.read",
                "workspace.write",
                "process.run",
                "process:node"
            ]),
            json!({"model":"old-model","reasoning_effort":"low","actions":5,"model_tokens":5000,
                "wall_seconds":120,"process_seconds":30,"container_image":"node:22-alpine",
                "command_scopes":{"commands":[{"program":"node","args":["--version"]}]},
                "filesystem_scopes":{"read":["src/**"],"write":["src/parser.rs"]}}),
            "Verify parser behavior",
        )
    }

    #[test]
    fn continuation_preserves_the_contract_without_importing_evidence_or_current_guidance()
    -> Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join("state");
        let mut store = Store::open(&root)?;
        let source = source(&mut store, directory.path())?;
        let operation = store.begin_operation(
            &source.id,
            "workspace.read",
            json!({"path":"src/parser.rs"}),
            true,
        )?;
        let evidence = store.put_artifact(b"old successful read")?;
        store.operation_state(&operation, "succeeded", Some(&evidence), json!({}))?;
        store.save_checkpoint(
            &source.id,
            &crate::model::Checkpoint {
                decisions: vec!["Use a bounded parser".into()],
                unresolved: vec!["Retest after editing".into()],
                next_action: "Inspect the current parser".into(),
                milestones: vec![crate::model::Milestone {
                    title: "Inspect parser".into(),
                    state: "completed".into(),
                    evidence: vec![evidence.clone()],
                }],
            },
        )?;
        let before = serde_json::to_value(store.events(&source.id)?)?;
        let review = prepare(
            &store,
            &source.id,
            directory.path(),
            "grok",
            "new-model",
            Some("high"),
        )?;
        store.remember(
            directory.path(),
            "New guidance must not silently replace old guidance",
            None,
        )?;
        let child = commit(&mut store, &root, &review)?;
        assert_eq!(
            serde_json::to_value(store.run(&source.id)?)?,
            serde_json::to_value(&source)?
        );
        assert_eq!(serde_json::to_value(store.events(&source.id)?)?, before);
        assert_eq!(child.grants, source.grants);
        assert_eq!(child.acceptance, source.acceptance);
        let mut expected = source.budgets.clone();
        expected["provider_transport"] = json!("aegis-direct-v1");
        expected["model"] = json!("new-model");
        expected["reasoning_effort"] = json!("high");
        expected["previous_run"] = json!(source.id);
        expected["continuation_handoff"] = review.handoff().clone();
        assert_eq!(child.budgets, expected);
        assert_eq!(child.provider, "grok");
        assert!(store.operations(&child.id)?.is_empty());
        assert!(store.active_capabilities(&child.id)?.is_empty());
        assert!(!store.has_evidence(&child.id, &evidence)?);
        assert!(store.last_checkpoint(&child.id)?.is_none());
        assert_eq!(store.milestones(&child.id)?[0].state, "active");
        assert_eq!(store.model_tokens(&child.id)?, 0);
        assert!(commit(&mut store, &root, &review).is_err());
        assert_eq!(store.runs()?.len(), 2);
        Ok(())
    }

    #[test]
    fn cancelled_review_and_stale_review_never_create_a_task() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join("state");
        let mut store = Store::open(&root)?;
        let source = source(&mut store, directory.path())?;
        let before = serde_json::to_value(store.events(&source.id)?)?;
        drop(prepare(
            &store,
            &source.id,
            directory.path(),
            "chatgpt",
            "new-model",
            None,
        )?);
        assert_eq!(serde_json::to_value(store.events(&source.id)?)?, before);
        let review = prepare(
            &store,
            &source.id,
            directory.path(),
            "chatgpt",
            "new-model",
            None,
        )?;
        store.event(
            &source.id,
            "action.rejected",
            json!({"error":"changed during review"}),
        )?;
        assert!(commit(&mut store, &root, &review).is_err());
        assert_eq!(store.runs()?.len(), 1);
        Ok(())
    }

    #[test]
    fn active_runner_and_unresolved_operations_block_continuation() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join("state");
        let mut store = Store::open(&root)?;
        let source = source(&mut store, directory.path())?;
        let review = prepare(
            &store,
            &source.id,
            directory.path(),
            "grok",
            "new-model",
            None,
        )?;
        let lock = File::options()
            .create(true)
            .write(true)
            .open(root.join(format!("run-{}.lock", source.id)))?;
        fs2::FileExt::try_lock_exclusive(&lock)?;
        assert!(commit(&mut store, &root, &review).is_err());
        drop(lock);
        let operation = store.begin_operation(
            &source.id,
            "workspace.read",
            json!({"path":"src/parser.rs"}),
            true,
        )?;
        assert!(
            prepare(
                &store,
                &source.id,
                directory.path(),
                "grok",
                "new-model",
                None
            )
            .is_err()
        );
        store.operation_state(&operation, "outcome_unknown", None, json!({}))?;
        assert!(
            prepare(
                &store,
                &source.id,
                directory.path(),
                "grok",
                "new-model",
                None
            )
            .is_err()
        );
        assert_eq!(store.runs()?.len(), 1);
        Ok(())
    }

    #[test]
    fn wrong_workspaces_providers_models_and_reasoning_fail_before_creation() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let other = tempfile::tempdir()?;
        let mut store = Store::open(&directory.path().join("state"))?;
        let source = source(&mut store, directory.path())?;
        assert!(prepare(&store, &source.id, other.path(), "grok", "model", None).is_err());
        for (provider, model, reasoning) in [
            ("claude", "model", None),
            ("custom", "model", None),
            ("grok", "invalid model", None),
            ("grok", "model", Some("arbitrary")),
        ] {
            assert!(
                prepare(
                    &store,
                    &source.id,
                    directory.path(),
                    provider,
                    model,
                    reasoning
                )
                .is_err()
            );
        }
        let review = prepare(&store, &source.id, directory.path(), "grok", "model", None)?;
        assert!(commit(&mut store, other.path(), &review).is_err());
        assert_eq!(store.runs()?.len(), 1);
        Ok(())
    }

    #[test]
    fn cancelled_unexecuted_work_can_continue_but_uncertain_outcomes_still_block() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join("state");
        let mut store = Store::open(&root)?;
        let source = source(&mut store, directory.path())?;
        let operation = store.begin_operation(
            &source.id,
            "workspace.read",
            json!({"path":"src/parser.rs"}),
            true,
        )?;
        store.state(
            &source.id,
            "cancelled",
            json!({"source":"explicit_user_cancel"}),
        )?;
        let review = prepare(&store, &source.id, directory.path(), "grok", "model", None)?;
        let child = commit(&mut store, &root, &review)?;
        assert!(store.operations(&child.id)?.is_empty());
        assert_eq!(store.run(&source.id)?.state, "cancelled");
        store.operation_state(&operation, "outcome_unknown", None, json!({}))?;
        assert!(prepare(&store, &source.id, directory.path(), "grok", "model", None).is_err());
        Ok(())
    }
}
