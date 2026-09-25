use std::path::PathBuf;

use anyhow::Result;
use arun::{capability, mcp, storage::Store, worker};
use serde_json::json;

#[test]
fn discovers_and_invokes_only_locally_granted_mcp_tool() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp.mjs");
    let server = mcp::Server {
        name: "fixture".into(),
        command: "node".into(),
        args: vec![fixture.to_string_lossy().into_owned()],
    };
    let tools = mcp::discover(&server, directory.path())?;
    assert_eq!(tools.len(), 2);
    let mut store = Store::open(&root)?;
    store.register_mcp(&server, &tools)?;
    let grants = vec!["mcp:fixture:echo".to_owned()];
    assert_eq!(
        capability::resolve(&store, "echo text", &grants, 2)?[0].id,
        "mcp.fixture.echo"
    );
    assert!(capability::permitted(&store, "mcp.fixture.ungranted", &grants)?.is_none());
    let run = store.create_run(
        "echo",
        directory.path(),
        "codex",
        json!(grants),
        json!({}),
        "",
    )?;
    store.state(&run.id, "running", json!({}))?;
    let version = tools
        .iter()
        .find(|tool| tool.name == "echo")
        .unwrap()
        .version;
    let operation = store.begin_operation_versioned(
        &run.id,
        "mcp.fixture.echo",
        version,
        json!({"text":"hello"}),
        false,
    )?;
    store.operation_state(&operation, "dispatched", None, json!({}))?;
    let result = worker::execute(&root, &operation.id)?;
    assert!(result.to_string().contains("hello"));
    let mut changed = tools.clone();
    changed
        .iter_mut()
        .find(|tool| tool.name == "echo")
        .unwrap()
        .version = version.wrapping_add(1);
    store.register_mcp(&server, &changed)?;
    assert!(worker::execute(&root, &operation.id).is_err());
    Ok(())
}
