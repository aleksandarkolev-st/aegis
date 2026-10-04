use anyhow::{Result, ensure};
use arun::{kernel, storage::Store};
use serde_json::json;
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
#[path = "support/http.rs"]
mod http;

#[test]
fn summary_omission_cannot_remove_constraints_from_model_prompts_after_restart() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let fixture_root = root.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let constraint = "Preserve public APIs and do not rewrite original tests.";
    let endpoint = http::Endpoint::start(move |body| {
        let state = http::state(body)?;
        let call = observed.fetch_add(1, Ordering::SeqCst);
        let action = match call {
            0 => {
                ensure!(state["pending_user_steering"][0]["text"] == constraint);
                let mut store = Store::open(&fixture_root)?;
                let run = store.runs()?.remove(0);
                store.request_pause(&run.id)?;
                json!({"kind":"remember","summary":"Inspected source; continue exploration","artifact":format!("user:{}",state["pending_user_steering"][0]["seq"])})
            }
            1 => {
                ensure!(state["compacted_task_owner_messages"][0]["text"] == constraint);
                ensure!(!state["working_memory"]["summary"].as_str().unwrap().contains("public APIs"));
                json!({"kind":"finish","summary":"Exploration reply with owner constraints still available","evidence":[]})
            }
            _ => anyhow::bail!("Unexpected model request"),
        };
        Ok((200,json!({"choices":[{"message":{"content":action.to_string()}}],"usage":{"prompt_tokens":10,"completion_tokens":2}})))
    })?;
    let mut store = Store::open(&root)?;
    let run = store.create_run("Explore", directory.path(), "custom", json!([]), json!({"mode":"durable","model":"fixture","endpoint":{"base_url":endpoint.url,"api_key_env":null}}), "")?;
    store.steer(&run.id, constraint)?;
    kernel::drive(&root, &run.id)?;
    ensure!(store.run(&run.id)?.state == "paused");
    for _ in 0..1100 { store.event(&run.id,"telemetry",json!({}))?; }
    store.maintain_history(&run.id)?;
    store.resume_paused(&run.id)?;
    drop(store);
    kernel::drive(&root, &run.id)?;
    endpoint.finish()?;
    let store = Store::open(&root)?;
    ensure!(store.run(&run.id)?.state == "answered");
    ensure!(calls.load(Ordering::SeqCst) == 2);
    Ok(())
}
