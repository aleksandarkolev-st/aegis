use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use arun::{capability, mcp, storage::Store, worker};
use serde_json::json;
use sha2::{Digest, Sha256};

#[test]
fn arc_bridge_discovers_and_observes_cached_frames_without_network_or_model_calls() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let session = directory.path().join("session");
    fs::create_dir(&session)?;
    let bridge = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benchmarks/arc/bridge.mjs");
    let digest = hex::encode(Sha256::digest(fs::read(&bridge)?));
    fs::write(
        session.join("authorized.json"),
        json!({"bridge_sha256":digest}).to_string(),
    )?;
    fs::write(session.join("state.json"), json!({"calls":0,"moves":0,"move_limit":3,"pending":null,"closed":false,"game_id":"fixture-012345678abc","frame":{"state":"NOT_FINISHED","levels_completed":0,"win_levels":1,"available_actions":[1],"frame":[vec![vec![2;64];64]]}}).to_string())?;
    let server = mcp::Server {
        name: "arc".into(),
        command: "node".into(),
        args: vec![
            bridge.to_string_lossy().into_owned(),
            session.to_string_lossy().into_owned(),
        ],
        policy: mcp::Policy {
            trusted_host: true,
            ..Default::default()
        },
    };
    let tools = mcp::discover(&server, directory.path())?;
    assert_eq!(tools.len(), 2);
    let root = directory.path().join(".arun");
    let mut store = Store::open(&root)?;
    store.register_mcp(&server, &tools)?;
    let grants = vec!["mcp:arc:observe".to_owned()];
    assert!(capability::permitted(&store, "workspace.read", &grants)?.is_none());
    assert!(capability::permitted(&store, "mcp.arc.act", &grants)?.is_none());
    let run = store.create_run(
        "Observe fixture",
        directory.path(),
        "fixture",
        json!(grants),
        json!({}),
        "",
    )?;
    store.state(&run.id, "running", json!({}))?;
    let version = tools
        .iter()
        .find(|tool| tool.name == "observe")
        .unwrap()
        .version;
    let operation =
        store.begin_operation_versioned(&run.id, "mcp.arc.observe", version, json!({}), false)?;
    store.operation_state(&operation, "dispatched", None, json!({}))?;
    let result = worker::execute(&root, &operation.id)?;
    assert_eq!(result["isError"], false);
    let observation: serde_json::Value =
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap())?;
    assert_eq!(observation["rows"].as_array().unwrap().len(), 64);
    assert_eq!(observation["rows"][0], "2".repeat(64));
    assert_eq!(observation["uncertain"], false);
    let state: serde_json::Value = serde_json::from_slice(&fs::read(session.join("state.json"))?)?;
    assert_eq!(state["calls"], 0);
    assert!(!session.join("requests.jsonl").exists());
    assert!(!session.join("credentials.json").exists());
    Ok(())
}
