use anyhow::Result;
use arun::storage::Store;
use serde_json::json;
use std::process::Command;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[path = "support/http.rs"]
mod http;

#[test]
fn question_allows_independent_work_and_resumes_the_original_task_with_its_answer() -> Result<()> {
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("input.txt"), "independent inspection")?;
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let endpoint = http::Endpoint::start(move |body| {
        let state = http::state(body)?;
        let action = match observed.fetch_add(1, Ordering::SeqCst) {
            0 => json!({"kind":"ask_user","query":"Which output format?"}),
            1 => {
                assert_eq!(
                    state["pending_user_questions"][0]["text"],
                    "Which output format?"
                );
                json!({"kind":"invoke","capability":"workspace.read","args":{"path":"input.txt"}})
            }
            2 => json!({"kind":"blocked","reason":"Waiting for the output format"}),
            3 => {
                assert_eq!(
                    state["task"],
                    "Inspect input.txt and report it in my preferred format"
                );
                assert_eq!(state["user_answers"][0]["answer"], "JSON please");
                let hash = state["recent_operation_outcomes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find_map(|item| item["artifact"].as_str())
                    .unwrap();
                json!({"kind":"finish","summary":"{\"content\":\"independent inspection\"}","evidence":[hash]})
            }
            _ => panic!("Unexpected extra model request"),
        };
        Ok((
            200,
            json!({"choices":[{"message":{"content":action.to_string()}}],"usage":{"prompt_tokens":10,"completion_tokens":2}}),
        ))
    })?;
    let first = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .args([
            "run",
            "Inspect input.txt and report it in my preferred format",
            "--provider",
            "custom",
            "--model",
            "fixture",
            "--endpoint",
            &endpoint.url,
            "--mode",
            "eager",
            "--foreground",
        ])
        .output()?;
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let mut store = Store::open(&directory.path().join(".arun"))?;
    let run = store.runs()?.remove(0);
    assert!(store.waiting_for_answer(&run.id)?);
    assert_eq!(store.operations(&run.id)?.len(), 1);
    assert_eq!(store.operations(&run.id)?[0].state, "succeeded");
    store.steer(&run.id, "JSON please")?;
    drop(store);
    let second = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .args(["resume", &run.id, "--foreground"])
        .output()?;
    endpoint.finish()?;
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let store = Store::open(&directory.path().join(".arun"))?;
    assert_eq!(store.runs()?.len(), 1);
    assert_eq!(store.run(&run.id)?.state, "completed");
    assert_eq!(
        store.operations(&run.id)?.len(),
        1,
        "Independent inspection must not be repeated after answering"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    Ok(())
}
