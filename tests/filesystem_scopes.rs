use anyhow::Result;
use arun::{storage::Store, worker};
use serde_json::json;

#[test]
fn narrowed_files_are_enforced_before_claim_and_search_never_returns_other_files() -> Result<()> {
    let directory = tempfile::tempdir()?;
    std::fs::create_dir(directory.path().join("src"))?;
    std::fs::write(directory.path().join("src/allowed.txt"), "marker approved")?;
    std::fs::write(directory.path().join("secret.txt"), "marker secret")?;
    let root = directory.path().join(".arun");
    let mut store = Store::open(&root)?;
    let run = store.create_run(
        "read",
        directory.path(),
        "fixture",
        json!([
            "workspace.read",
            "workspace.write",
            "process.run",
            "process:node"
        ]),
        json!({"filesystem_scopes":{"read":["src/**"],"write":[]}}),
        "",
    )?;
    store.state(&run.id, "running", json!({}))?;
    for (capability, arguments) in [
        ("workspace.read", json!({"path":"secret.txt"})),
        (
            "workspace.write",
            json!({"path":"src/allowed.txt","content":"modified"}),
        ),
        ("process.run", json!({"program":"node","args":[]})),
    ] {
        let operation = store.begin_operation(&run.id, capability, arguments, false)?;
        store.operation_state(&operation, "dispatched", None, json!({}))?;
        assert!(worker::execute(&root, &operation.id).is_err());
        assert_eq!(store.operation(&operation.id)?.state, "dispatched");
    }
    let search =
        store.begin_operation(&run.id, "workspace.search", json!({"query":"marker"}), true)?;
    store.operation_state(&search, "dispatched", None, json!({}))?;
    let result = worker::execute(&root, &search.id)?;
    assert_eq!(result["matches"].as_array().unwrap().len(), 1);
    assert!(!result.to_string().contains("secret"));
    assert_eq!(
        std::fs::read_to_string(directory.path().join("src/allowed.txt"))?,
        "marker approved"
    );
    assert_eq!(store.event_count(&run.id, "operation.executing")?, 1);
    Ok(())
}
