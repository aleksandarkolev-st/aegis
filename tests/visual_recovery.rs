use anyhow::{Result, ensure};
use arun::{kernel, storage::Store};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
#[path = "support/http.rs"]
mod http;

#[test]
fn malformed_screenshot_allows_replacement_question_on_repeated_resume() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let endpoint = http::Endpoint::start(move |body| {
        let state = http::state(body)?;
        ensure!(
            state["visual_input_error"]["error"]
                .as_str()
                .unwrap()
                .contains("base64")
        );
        ensure!(state.get("visual_inputs").is_none());
        let call = observed.fetch_add(1, Ordering::SeqCst);
        let action = if call == 0 {
            json!({"kind":"ask_user","query":"Please provide a replacement screenshot; the capture is malformed."})
        } else {
            ensure!(
                !state["pending_user_questions"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            json!({"kind":"blocked","reason":"awaiting replacement screenshot"})
        };
        Ok((
            200,
            json!({"choices":[{"message":{"content":action.to_string()}}],"usage":{"prompt_tokens":10,"completion_tokens":2}}),
        ))
    })?;
    let mut store = Store::open(&root)?;
    let run = store.create_run("Inspect screenshot", directory.path(), "custom", json!([]), json!({"mode":"durable","model":"fixture","endpoint":{"base_url":endpoint.url,"api_key_env":null}}), "")?;
    store.state(&run.id, "running", json!({}))?;
    let operation = store.begin_operation(&run.id, "mcp.fixture.capture", json!({}), true)?;
    store.operation_state(&operation, "dispatched", None, json!({}))?;
    store.claim_operation(&operation)?;
    let result = json!({"content":[{"type":"image","mimeType":"image/png","data":"not base64!"}]});
    let artifact = store.put_artifact(&serde_json::to_vec(&result)?)?;
    store.operation_state(
        &operation,
        "succeeded",
        Some(&artifact),
        json!({"output_preview":{"image_count":1}}),
    )?;
    kernel::drive(&root, &run.id)?;
    ensure!(calls.load(Ordering::SeqCst) == 2);
    ensure!(store.run(&run.id)?.state == "waiting_recovery");
    store.resume_paused(&run.id)?;
    drop(store);
    kernel::drive(&root, &run.id)?;
    endpoint.finish()?;
    let store = Store::open(&root)?;
    ensure!(calls.load(Ordering::SeqCst) == 3);
    ensure!(store.event_count(&run.id, "visual_input.rejected")? == 1);
    ensure!(store.pending_questions(&run.id)?.len() == 1);
    ensure!(store.run(&run.id)?.state == "waiting_recovery");
    Ok(())
}
