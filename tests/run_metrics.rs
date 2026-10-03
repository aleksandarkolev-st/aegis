use anyhow::Result;
use arun::{
    run_metrics::{self, LiveMetrics},
    storage::Store,
};
use serde_json::json;

#[path = "support/operation.rs"]
mod operation_fixture;

#[test]
fn metrics_preserve_cached_failed_and_pending_usage_through_archival() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut store = Store::open(directory.path())?;
    let run = store.create_run(
        "Count observed usage",
        directory.path(),
        "custom",
        json!([]),
        json!({}),
        "",
    )?;
    store.state(&run.id, "running", json!({}))?;
    store.event(&run.id, "model.started", json!({}))?;
    store.event(&run.id, "model.response", json!({"usage":{"input_tokens":100,"output_tokens":20,"cached_input_tokens":60,"source":"provider"},"elapsed_ms":1000}))?;
    store.event(&run.id, "model.started", json!({}))?;
    store.event(&run.id, "model.failed", json!({"usage":{"input_tokens":40,"output_tokens":10,"cached_input_tokens":0,"cached_input_reported":true,"source":"provider"},"elapsed_ms":500}))?;
    let measured = run_metrics::report(&store, &run.id)?;
    assert_eq!(measured["tokens"]["reported_total"], 170);
    assert_eq!(measured["tokens"]["input"], 140);
    assert_eq!(measured["tokens"]["output"], 30);
    assert_eq!(measured["tokens"]["cached_input"], 60);
    assert_eq!(
        measured["efficiency"]["output_tokens_per_model_second"],
        20.0
    );
    assert!(measured["efficiency"]["tokens_per_completed_task"].is_null());
    store.event(&run.id, "model.started", json!({}))?;
    let pending = run_metrics::report(&store, &run.id)?;
    assert_eq!(pending["tokens"]["unreported_attempts"], 1);
    assert_eq!(pending["tokens"]["current_turn_pending"], true);
    assert!(pending["efficiency"]["output_tokens_per_model_second"].is_null());
    store.event(
        &run.id,
        "model.failed",
        json!({"error":"unreported connection failure"}),
    )?;
    for _ in 0..180 {
        store.event(&run.id, "telemetry", json!({}))?;
    }
    store.save_snapshot(&run.id)?;
    assert!(store.archive_history(&run.id)? > 0);
    drop(store);
    let store = Store::open(directory.path())?;
    let archived = run_metrics::report(&store, &run.id)?;
    assert_eq!(archived["tokens"]["reported_total"], 170);
    assert_eq!(archived["tokens"]["cached_input"], 60);
    assert_eq!(archived["tokens"]["unreported_attempts"], 1);
    assert_eq!(archived["tokens"]["usage_complete"], false);
    assert_eq!(archived["tokens"]["current_turn_pending"], false);
    assert!(archived["efficiency"]["output_tokens_per_model_second"].is_null());
    assert!(run_metrics::display(&archived).contains("1 unreported"));
    assert_eq!(LiveMetrics::load(&store, &run.id)?.reported_tokens, 170);
    Ok(())
}

#[test]
fn missing_measurements_are_unknown_and_coverage_is_not_confidence() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut store = Store::open(directory.path())?;
    let run = store.create_run(
        "Inspect\nRequirements:\n- preserve API\n- verify behavior",
        directory.path(),
        "custom",
        json!([]),
        json!({}),
        "",
    )?;
    let empty = run_metrics::report(&store, &run.id)?;
    assert!(empty["tokens"]["input"].is_null());
    assert!(empty["tokens"]["cached_input"].is_null());
    assert!(empty["efficiency"]["output_tokens_per_model_second"].is_null());
    assert_eq!(empty["verification"]["requirements"], 2);
    assert_eq!(empty["verification"]["verified"], 0);
    assert_eq!(empty["verification"]["coverage_percent"], 0.0);
    assert!(empty["operations"]["success_percent"].is_null());
    assert!(run_metrics::display(&empty).contains("not model confidence"));
    let live = LiveMetrics::load(&store, &run.id)?;
    assert_eq!(live.requirements, 2);
    assert_eq!(live.verified, 0);
    Ok(())
}

#[test]
fn verification_metrics_drop_stale_workspace_evidence() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut store = Store::open(directory.path())?;
    let run = store.create_run(
        "Repair\nRequirements:\n- preserve API\n- test behavior",
        directory.path(),
        "custom",
        json!(["workspace.read", "workspace.write"]),
        json!({}),
        "",
    )?;
    store.state(&run.id, "running", json!({}))?;
    let read = store.begin_operation(&run.id, "workspace.read", json!({"path":"api.rs"}), true)?;
    let proof = store.put_artifact(b"passing API check")?;
    operation_fixture::claim_fixture_operation(&mut store, &read.id)?;
    store.operation_state(&read, "succeeded", Some(&proof), json!({"exit_code":0}))?;
    store.verify_obligation(&run.id, 1, &[proof])?;
    let verified = run_metrics::report(&store, &run.id)?;
    assert_eq!(verified["verification"]["verified"], 1);
    assert_eq!(verified["verification"]["coverage_percent"], 50.0);
    let write = store.begin_operation(&run.id, "workspace.write", json!({}), false)?;
    operation_fixture::claim_fixture_operation(&mut store, &write.id)?;
    assert_eq!(
        LiveMetrics::load(&store, &run.id)?.verified,
        0,
        "proof is uncertain during a workspace mutation"
    );
    let receipt = store.put_artifact(b"workspace changed")?;
    store.operation_state(&write, "succeeded", Some(&receipt), json!({}))?;
    let stale = run_metrics::report(&store, &run.id)?;
    assert_eq!(stale["verification"]["verified"], 0);
    assert_eq!(stale["verification"]["requirements"], 2);
    assert_eq!(stale["verification"]["coverage_percent"], 0.0);
    assert_eq!(LiveMetrics::load(&store, &run.id)?.verified, 0);
    Ok(())
}

#[test]
fn estimated_and_legacy_tokens_remain_separate_after_projection_migration() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut store = Store::open(directory.path())?;
    let run = store.create_run(
        "Classify token receipts",
        directory.path(),
        "custom",
        json!([]),
        json!({}),
        "",
    )?;
    for usage in [
        json!({"input_tokens":30,"output_tokens":10,"source":"provider","cached_input_tokens":20}),
        json!({"input_tokens":70,"output_tokens":10,"source":"estimated"}),
        json!({"input_tokens":8,"output_tokens":2}),
    ] {
        store.event(&run.id, "model.started", json!({}))?;
        store.event(
            &run.id,
            "model.response",
            json!({"usage":usage,"elapsed_ms":100}),
        )?;
    }
    for _ in 0..180 {
        store.event(&run.id, "telemetry", json!({}))?;
    }
    store.save_snapshot(&run.id)?;
    assert!(store.archive_history(&run.id)? > 0);
    drop(store);
    // Simulate upgrading an older database whose receipts are already archived.
    rusqlite::Connection::open(directory.path().join("runs.sqlite"))?
        .execute_batch("DROP TABLE usage_source_projection")?;
    let store = Store::open(directory.path())?;
    let live = LiveMetrics::load(&store, &run.id)?;
    assert_eq!(live.reported_tokens, 40);
    assert_eq!(live.estimated_tokens, 80);
    assert_eq!(live.unclassified_tokens, 10);
    let metrics = run_metrics::report(&store, &run.id)?;
    assert_eq!(metrics["tokens"]["tracked_total"], 130);
    assert_eq!(metrics["tokens"]["reported_total"], 40);
    assert_eq!(metrics["tokens"]["measured_attempts"], 1);
    assert_eq!(metrics["tokens"]["unreported_attempts"], 2);
    assert_eq!(metrics["tokens"]["usage_complete"], false);
    assert!(metrics["efficiency"]["output_tokens_per_model_second"].is_null());
    assert_eq!(metrics["tokens"]["input"], 30);
    Ok(())
}
