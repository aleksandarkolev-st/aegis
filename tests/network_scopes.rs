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
