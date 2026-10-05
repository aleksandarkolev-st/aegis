use anyhow::Result;
use arun::storage::Store;
use serde_json::{Value, json};
use std::process::Command;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[path = "support/http.rs"]
mod http;

fn cli(workspace: &std::path::Path, args: &[&str]) -> Result<()> {
    let output = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(workspace)
        .args(args)
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn reply(action: Value) -> (u16, Value) {
    (
        200,
        json!({"choices":[{"message":{"content":action.to_string()}}],"usage":{"prompt_tokens":10,"completion_tokens":2}}),
    )
}

#[test]
fn different_queries_returning_the_same_location_pause_the_real_kernel() -> Result<()> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("source.txt"),
        "storage cancellation dispatch cancellation fresh query progress kernel cancellation completion cancellation",
    )?;
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let endpoint = http::Endpoint::start(move |_body| {
        let index = observed.fetch_add(1, Ordering::SeqCst);
        let queries = [
            "storage cancellation",
            "dispatch cancellation",
            "fresh query progress",
            "kernel cancellation",
            "completion cancellation",
        ];
        Ok(reply(if index < queries.len() {
            json!({"kind":"invoke","capability":"workspace.search","args":{"path":"source.txt","query":queries[index]}})
        } else {
            json!({"kind":"blocked","reason":"Fixture endpoint reached its bound"})
        }))
    })?;
    cli(
        directory.path(),
        &[
            "run",
            "Find weaknesses",
            "--provider",
            "custom",
            "--model",
            "fixture",
            "--endpoint",
            &endpoint.url,
            "--mode",
            "eager",
            "--foreground",
        ],
    )?;
    endpoint.finish()?;
    let store = Store::open(&directory.path().join(".arun"))?;
    let run = store.runs()?.remove(0);
    assert_eq!(run.state, "waiting_recovery");
    assert_eq!(calls.load(Ordering::SeqCst), 5);
    assert_eq!(store.operations(&run.id)?.len(), 5);
    assert_eq!(store.event_count(&run.id, "loop.stalled")?, 1);
    assert_eq!(store.event_count(&run.id, "loop.exploration_exhausted")?, 0);
    Ok(())
}

#[test]
fn fresh_results_are_bounded_across_resume_and_owner_input_can_finish_the_task() -> Result<()> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("source.txt"),
        (0..40)
            .map(|i| format!("match-{i:03}: finding {i}\n"))
            .collect::<String>(),
    )?;
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let endpoint = http::Endpoint::start(move |body| {
        let index = observed.fetch_add(1, Ordering::SeqCst);
        let state = http::state(body)?;
        if (12..24).contains(&index) {
            assert_eq!(state["exploration_guard"]["remaining"], 24 - index);
        }
        if index == 25 {
            let artifact = state["recent_operation_outcomes"]
                .as_array()
                .unwrap()
                .iter()
                .find_map(|item| item["artifact"].as_str())
                .unwrap();
            return Ok(reply(
                json!({"kind":"finish","summary":"Report the supported findings from source.txt","evidence":[artifact]}),
            ));
        }
        Ok(reply(if index < 40 {
            json!({"kind":"invoke","capability":"workspace.search","args":{"path":"source.txt","query":format!("match-{index:03}")}})
        } else {
            json!({"kind":"blocked","reason":"Fixture endpoint reached its bound"})
        }))
    })?;
    cli(
        directory.path(),
        &[
            "run",
            "Find weaknesses",
            "--provider",
            "custom",
            "--model",
            "fixture",
            "--endpoint",
            &endpoint.url,
            "--mode",
            "eager",
            "--foreground",
        ],
    )?;
    let store = Store::open(&directory.path().join(".arun"))?;
    let run = store.runs()?.remove(0);
    assert_eq!(run.state, "waiting_recovery");
    assert_eq!(calls.load(Ordering::SeqCst), 24);
    assert_eq!(store.event_count(&run.id, "loop.exploration_exhausted")?, 1);
    drop(store);
    // A new runner does not silently grant another exploration allowance.
    cli(directory.path(), &["resume", &run.id, "--foreground"])?;
    let mut store = Store::open(&directory.path().join(".arun"))?;
    assert_eq!(store.run(&run.id)?.state, "waiting_recovery");
    assert_eq!(calls.load(Ordering::SeqCst), 25);
    assert_eq!(store.event_count(&run.id, "loop.exploration_exhausted")?, 2);
    store.resume_paused(&run.id)?;
    store.steer(
        &run.id,
        "Finish now with the supported findings already collected",
    )?;
    drop(store);
    cli(directory.path(), &["resume", &run.id, "--foreground"])?;
    endpoint.finish()?;
    let store = Store::open(&directory.path().join(".arun"))?;
    assert_eq!(store.run(&run.id)?.state, "completed");
    assert_eq!(calls.load(Ordering::SeqCst), 26);
    assert_eq!(store.operations(&run.id)?.len(), 25);
    Ok(())
}
