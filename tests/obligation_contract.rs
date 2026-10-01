use anyhow::Result;
use arun::model::{Checkpoint, Milestone};
use arun::routing::Reason;
use arun::storage::Store;
use serde_json::json;
#[path = "support/operation.rs"]
mod operation_fixture;

fn successful_operation(
    store: &mut Store,
    run_id: &str,
    capability: &str,
    result: &[u8],
) -> Result<String> {
    let operation = store.begin_operation(
        run_id,
        capability,
        json!({}),
        capability != "workspace.write",
    )?;
    let artifact = store.put_artifact(result)?;
    operation_fixture::claim_fixture_operation(store, &operation.id)?;
    store.operation_state(&operation, "succeeded", Some(&artifact), json!({}))?;
    Ok(artifact)
}

#[test]
fn parser_obligations_survive_replanning_provider_change_and_workspace_edit() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut store = Store::open(directory.path())?;
    let run = store.create_run(
        "Refactor the parser.\n\nRequirements:\n- support nested expressions\n- preserve public API compatibility\n- add regression tests\n- full test suite must pass",
        directory.path(),
        "codex",
        json!(["workspace.read", "workspace.write"]),
        json!({"provider_transport":"aegis-direct-v1","model":"primary-model","fallback_routes":[{"provider":"grok","model":"fallback-model"}]}),
        "",
    )?;
    store.state(&run.id, "running", json!({}))?;
    let titles = store
        .obligations(&run.id)?
        .into_iter()
        .map(|obligation| obligation.title)
        .collect::<Vec<_>>();
    assert_eq!(
        titles,
        [
            "Task request",
            "support nested expressions",
            "preserve public API compatibility",
            "add regression tests",
            "full test suite must pass",
        ]
    );

    let nesting = successful_operation(
        &mut store,
        &run.id,
        "workspace.read",
        b"nested parser proof",
    )?;
    let compatibility =
        successful_operation(&mut store, &run.id, "workspace.read", b"public API proof")?;
    store.verify_obligation(&run.id, 1, &[nesting.clone()])?;
    store.verify_obligation(&run.id, 2, &[compatibility.clone()])?;
    store.save_checkpoint(
        &run.id,
        &Checkpoint {
            decisions: vec!["Keep the public API".into()],
            unresolved: vec!["Regression suite".into()],
            next_action: "Run parser tests".into(),
            milestones: vec![Milestone {
                title: "Implementation compiles".into(),
                state: "completed".into(),
                evidence: vec![nesting.clone()],
            }],
        },
    )?;
    assert_eq!(store.obligations(&run.id)?.len(), 5);
    assert!(
        store
            .complete_run(&run.id, "done", &[nesting.clone()])
            .is_err()
    );

    store.event(&run.id, "model.started", json!({"turn":1}))?;
    store.event(
        &run.id,
        "model.failed",
        json!({"recoverable_reason":"usage_limit"}),
    )?;
    let route = store
        .transition_provider(&run.id, Reason::UsageLimit)?
        .unwrap();
    assert_eq!(route.provider, "grok");
    assert_eq!(store.run(&run.id)?.id, run.id);
    assert_eq!(store.obligations(&run.id)?[1].state, "verified");
    assert_eq!(store.obligations(&run.id)?[2].state, "verified");
    assert_eq!(store.obligations(&run.id)?[3].state, "open");
    assert_eq!(store.obligations(&run.id)?[4].state, "open");
    drop(store);

    let mut store = Store::open(directory.path())?;
    assert_eq!(store.current_route(&run.id)?, route);
    let edit = successful_operation(&mut store, &run.id, "workspace.write", b"parser edit")?;
    assert_eq!(store.workspace_revision(&run.id)?, Some(1));
    assert_eq!(store.obligations(&run.id)?[1].state, "stale");
    assert_eq!(store.obligations(&run.id)?[2].state, "stale");
    assert!(store.verify_obligation(&run.id, 1, &[nesting]).is_err());
    assert!(store.complete_run(&run.id, "done", &[edit]).is_err());

    let mut current_evidence = Vec::new();
    for (id, result) in [
        (1, b"nested parser test".as_slice()),
        (2, b"public API check".as_slice()),
        (3, b"regression test".as_slice()),
        (4, b"full test suite".as_slice()),
    ] {
        let artifact = successful_operation(&mut store, &run.id, "workspace.read", result)?;
        store.verify_obligation(&run.id, id, &[artifact.clone()])?;
        current_evidence.push(artifact);
    }
    store.save_checkpoint(
        &run.id,
        &Checkpoint {
            decisions: vec!["Keep the public API".into()],
            unresolved: vec![],
            next_action: "Review the refreshed proof".into(),
            milestones: vec![Milestone {
                title: "Implementation compiles".into(),
                state: "completed".into(),
                evidence: vec![current_evidence[0].clone()],
            }],
        },
    )?;
    store.complete_run(&run.id, "done", &current_evidence)?;
    assert_eq!(store.run(&run.id)?.state, "completed");
    assert_eq!(store.event_count(&run.id, "provider.transition")?, 1);
    assert!(
        store
            .obligations(&run.id)?
            .into_iter()
            .skip(1)
            .all(|obligation| obligation.state == "verified"
                && obligation.verified_revision == Some(1))
    );
    Ok(())
}
