use anyhow::{Result, bail};
use arun::storage::Store;
use serde_json::json;

pub fn claim_fixture_operation(store: &mut Store, operation_id: &str) -> Result<()> {
    let operation = store.operation(operation_id)?;
    let run = store.run(&operation.run_id)?;
    if run.is_terminal() {
        bail!("cannot claim an operation for a terminal fixture run");
    }
    if run.state != "running" {
        store.state(&operation.run_id, "running", json!({"test_fixture":true}))?;
    }

    let mut operation = store.operation(operation_id)?;
    if operation.state == "pending" {
        store.operation_state(&operation, "dispatched", None, json!({"test_fixture":true}))?;
        operation = store.operation(operation_id)?;
    }
    if operation.state == "dispatched" {
        store.claim_operation(&operation)?;
    } else if operation.state != "executing" {
        bail!("fixture operation is not unresolved");
    }
    Ok(())
}
