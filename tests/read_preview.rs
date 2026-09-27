use std::fs;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use arun::storage::Store;
use serde_json::json;

#[path = "support/http.rs"]
mod http;

#[test]
fn small_file_reads_reach_the_next_decision_without_an_inspection_turn() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let original = "日本語 🦊\n\"quoted\" \\ exact source";
    fs::write(directory.path().join("source.txt"), original)?;
    let requests = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&requests);
    let endpoint = http::Endpoint::start(move |body| {
        let state = http::state(body)?;
        let action = match observed.fetch_add(1, Ordering::SeqCst) {
            0 => json!({"kind":"search_capabilities","query":"read file"}),
            1 => {
                json!({"kind":"invoke","capability":"workspace.read","args":{"path":"source.txt"}})
            }
            2 => {
                assert!(
                    state["result_policy"]
                        .as_str()
                        .unwrap()
                        .contains("mapped read content is ready to use")
                );
                let result = state["recent_events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|event| event["kind"] == "operation.succeeded")
                    .unwrap();
                assert_eq!(result["payload"]["content"], original);
                assert_eq!(result["payload"]["complete"], true);
                assert_eq!(result["payload"]["sha256"].as_str().unwrap().len(), 64);
                json!({"kind":"finish","summary":"Exact source read","evidence":[result["payload"]["artifact"]]})
            }
            _ => anyhow::bail!("Unexpected extra model request"),
        };
        Ok((
            200,
            json!({"choices":[{"message":{"content":action.to_string()}}],
            "usage":{"prompt_tokens":10,"completion_tokens":2}}),
        ))
    })?;
    let output = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .env("PATH", directory.path())
        .args([
            "run",
            "Read the source",
            "--provider",
            "custom",
            "--endpoint",
            &endpoint.url,
            "--model",
            "fixture-model",
            "--foreground",
        ])
        .output()?;
    endpoint.finish()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(requests.load(Ordering::SeqCst), 3);
    let store = Store::open(&directory.path().join(".arun"))?;
    let run = store.runs()?.remove(0);
    assert_eq!(run.state, "completed");
    assert_eq!(store.event_count(&run.id, "model.started")?, 3);
    assert_eq!(store.event_count(&run.id, "artifact.inspected")?, 0);
    assert_eq!(store.operations(&run.id)?.len(), 1);
    assert!(store.tool_result_tokens(&run.id)? > 0);
    assert_eq!(store.model_tokens(&run.id)?, 36);
    assert_eq!(
        fs::read_to_string(directory.path().join("source.txt"))?,
        original
    );
    Ok(())
}
