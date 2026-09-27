use anyhow::Result;
use arun::{
    model::{Checkpoint, Milestone},
    storage::Store,
};
use serde_json::json;

#[test]
fn conversational_replies_are_durable_but_not_verified_work() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut store = Store::open(directory.path())?;
    let run = store.create_run("hello", directory.path(), "codex", json!([]), json!({}), "")?;
    store.state(&run.id, "running", json!({}))?;
    store.save_snapshot(&run.id)?;
    store.answer_run(&run.id, "Hello! What would you like to build?")?;
    assert!(store.run(&run.id)?.is_terminal());
    assert_eq!(store.run(&run.id)?.state, "answered");
    assert_eq!(
        store.run_summary(&run.id)?.as_deref(),
        Some("Hello! What would you like to build?")
    );
    assert_eq!(store.event_count(&run.id, "run.completed")?, 0);
    assert!(store.operations(&run.id)?.is_empty());
    assert!(store.evidence_artifacts(&run.id)?.is_empty());
    assert!(store.state(&run.id, "ready", json!({})).is_err());
    drop(store);
    let store = Store::open(directory.path())?;
    assert!(store.load_recovery(&run.id)?.is_some());
    assert_eq!(store.run(&run.id)?.state, "answered");
    assert_eq!(
        store.run_summary(&run.id)?.as_deref(),
        Some("Hello! What would you like to build?")
    );
    Ok(())
}

#[test]
fn replies_cannot_bypass_operations_plans_or_acceptance() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut store = Store::open(directory.path())?;
    for kind in ["operation", "plan", "acceptance"] {
        let budgets = if kind == "acceptance" {
            json!({"acceptance_check":{"name":"verify", "program":"node", "args":["check.mjs"],"image":"node:22-alpine","seconds":10}})
        } else {
            json!({})
        };
        let run = store.create_run(kind, directory.path(), "codex", json!([]), budgets, "")?;
        store.state(&run.id, "running", json!({}))?;
        if kind == "operation" {
            store.begin_operation(
                &run.id,
                "workspace.write",
                json!({"path":"hello.txt","content":"hello"}),
                false,
            )?;
        }
        if kind == "plan" {
            store.save_checkpoint(
                &run.id,
                &Checkpoint {
                    decisions: vec![],
                    unresolved: vec![],
                    next_action: "repair".into(),
                    milestones: vec![Milestone {
                        title: "Repair code".into(),
                        state: "active".into(),
                        evidence: vec![],
                    }],
                },
            )?;
        }
        assert!(store.answer_run(&run.id, "Claimed done").is_err());
        assert_eq!(store.run(&run.id)?.state, "running");
        assert!(store.run_summary(&run.id)?.is_none());
    }
    Ok(())
}
