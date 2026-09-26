use anyhow::Result;
use arun::{storage::Store, worker};
use serde_json::json;

#[test]
fn direct_workers_cannot_claim_ungranted_or_out_of_scope_network_requests() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let mut store = Store::open(&root)?;
    for grants in [json!([]), json!(["network.fetch"])] {
        let run = store.create_run(
            "network",
            directory.path(),
            "fixture",
            grants,
            json!({"network_scopes":{"domains":["example.com"],"body_bytes":2048}}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let operation = store.begin_operation(
            &run.id,
            "network.fetch",
            json!({"url":"https://127.0.0.1/"}),
            true,
        )?;
        store.operation_state(&operation, "dispatched", None, json!({}))?;
        assert!(worker::execute(&root, &operation.id).is_err());
        assert_eq!(store.operation(&operation.id)?.state, "dispatched");
        assert_eq!(store.network_body_charge(&run.id)?, 0);
        assert_eq!(store.event_count(&run.id, "operation.executing")?, 0);
    }
    Ok(())
}

#[test]
#[ignore = "requires public HTTPS access to example.com; no model calls"]
fn public_https_transport_records_bounded_artifact_and_durable_body_receipt() -> Result<()> {
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".arun")
        .join(format!("network-functional-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory)?;
    let root = directory.join(".arun");
    let mut store = Store::open(&root)?;
    let run = store.create_run("Functional public HTTPS adapter check; no model",&directory,"fixture",json!(["network.fetch"]),json!({"network_scopes":{"domains":["example.com"],"body_bytes":16384},"process_seconds":20}),"")?;
    eprintln!(
        "Functional network record: {} run {}",
        directory.display(),
        run.id
    );
    store.state(&run.id, "running", json!({}))?;
    let operation = store.begin_operation(
        &run.id,
        "network.fetch",
        json!({"url":"https://example.com/"}),
        true,
    )?;
    store.operation_state(&operation, "dispatched", None, json!({}))?;
    let result = worker::execute(&root, &operation.id)?;
    assert_eq!(result["status"], 200);
    let body = store.artifact(result["output_artifact"].as_str().unwrap())?;
    assert!(String::from_utf8_lossy(&body).contains("Example Domain"));
    assert_eq!(result["bytes"], body.len());
    assert_eq!(store.network_body_charge(&run.id)?, body.len() as u64);
    let hash = store.put_artifact(&serde_json::to_vec(&result)?)?;
    store.operation_state(
        &operation,
        "succeeded",
        Some(&hash),
        json!({"functional":true}),
    )?;
    store.complete_run(
        &run.id,
        "Public HTTPS transport and bounded body receipt verified",
        &[hash],
    )?;
    assert_eq!(store.event_count(&run.id, "model.started")?, 0);
    eprintln!(
        "HTTPS 200; body {} bytes; body artifact {}",
        body.len(),
        result["output_artifact"]
    );
    Ok(())
}
