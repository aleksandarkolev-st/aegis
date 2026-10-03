use anyhow::{Result, ensure};
use arun::{run_metrics, storage::Store};
use serde_json::json;
use std::{
    net::TcpListener,
    process::Command,
    time::{Duration, Instant},
};

#[path = "support/http.rs"]
#[allow(dead_code)]
mod http;

#[test]
fn connection_returns_before_dispatch_and_same_route_completes_once() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let reserved = TcpListener::bind("127.0.0.1:0")?;
    let address = reserved.local_addr()?;
    drop(reserved);
    let mut store = Store::open(&root)?;
    let run = store.create_run("Write one receipt", directory.path(), "custom", json!(["workspace.write"]), json!({
        "mode":"eager","model":"fixture","endpoint":{"base_url":format!("http://{address}/v1"),"api_key_env":null},"model_seconds":5,"wall_seconds":30}), "")?;
    let watched_root = root.clone();
    let watched_id = run.id.clone();
    let endpoint = std::thread::spawn(move || -> Result<http::Endpoint> {
        let watched = Store::open(&watched_root)?;
        let deadline = Instant::now() + Duration::from_secs(25);
        while watched.event_count(&watched_id, "model.pre_dispatch_retry")? == 0 {
            ensure!(
                Instant::now() < deadline,
                "connection failure did not schedule a retry"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut requests = 0;
        http::Endpoint::start_at(address, move |body| {
            let state = http::state(body)?;
            requests += 1;
            let action = match requests {
                1 => {
                    json!({"kind":"invoke","capability":"workspace.write","args":{"path":"receipt.txt","content":"written once"}})
                }
                2 => {
                    json!({"kind":"finish","summary":"Receipt written","evidence":[state["recent_operation_outcomes"][0]["artifact"]]})
                }
                _ => anyhow::bail!("unexpected extra request"),
            };
            Ok((
                200,
                json!({"choices":[{"message":{"content":action.to_string()}}],"usage":{"prompt_tokens":10,"completion_tokens":2}}),
            ))
        })
    });
    let output = Command::new(env!("CARGO_BIN_EXE_arun"))
        .args(["serve", root.to_str().unwrap(), &run.id])
        .output()?;
    endpoint.join().unwrap()?.finish()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(store.run(&run.id)?.state, "completed");
    assert_eq!(
        std::fs::read_to_string(directory.path().join("receipt.txt"))?,
        "written once"
    );
    assert_eq!(store.operations(&run.id)?.len(), 1);
    assert_eq!(store.event_count(&run.id, "model.pre_dispatch_retry")?, 1);
    assert_eq!(store.model_tokens(&run.id)?, 24);
    let metrics = run_metrics::report(&store, &run.id)?;
    assert_eq!(metrics["tokens"]["not_dispatched_attempts"], 1);
    assert_eq!(metrics["tokens"]["unreported_attempts"], 0);
    assert_eq!(metrics["tokens"]["usage_complete"], true);
    let failure = store
        .events(&run.id)?
        .into_iter()
        .find(|event| event.kind == "model.failed")
        .unwrap();
    assert_eq!(failure.payload["request_dispatched"], false);
    assert!(failure.payload["usage"].is_null());
    Ok(())
}

#[test]
fn persistent_connection_failure_is_bounded_and_creates_no_usage_or_effect() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let reserved = TcpListener::bind("127.0.0.1:0")?;
    let address = reserved.local_addr()?;
    drop(reserved);
    let mut store = Store::open(&root)?;
    let run = store.create_run("Bound connection recovery", directory.path(), "custom", json!([]), json!({
        "model":"fixture","endpoint":{"base_url":format!("http://{address}/v1"),"api_key_env":null},"model_seconds":5,"wall_seconds":30}), "")?;
    let output = Command::new(env!("CARGO_BIN_EXE_arun"))
        .args(["serve", root.to_str().unwrap(), &run.id])
        .output()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(store.run(&run.id)?.state, "waiting_recovery");
    assert_eq!(store.event_count(&run.id, "model.pre_dispatch_retry")?, 3);
    assert_eq!(store.event_count(&run.id, "model.started")?, 4);
    assert_eq!(store.event_count(&run.id, "model.response")?, 0);
    assert_eq!(store.model_tokens(&run.id)?, 0);
    assert!(store.operations(&run.id)?.is_empty());
    let metrics = run_metrics::report(&store, &run.id)?;
    assert_eq!(metrics["tokens"]["not_dispatched_attempts"], 4);
    assert!(metrics["tokens"]["input"].is_null());
    assert_eq!(metrics["tokens"]["usage_complete"], false);
    Ok(())
}

#[test]
fn dispatched_http_outage_is_not_retried_without_a_usage_receipt() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let endpoint = http::Endpoint::start(|_| Ok((503, json!({"error":"unavailable"}))))?;
    let root = directory.path().join(".arun");
    let mut store = Store::open(&root)?;
    let run = store.create_run("Bound dispatched outage",directory.path(),"custom",json!([]),json!({"model":"fixture","endpoint":{"base_url":endpoint.url,"api_key_env":null},"wall_seconds":30}),"")?;
    let output = Command::new(env!("CARGO_BIN_EXE_arun"))
        .args(["serve", root.to_str().unwrap(), &run.id])
        .output()?;
    endpoint.finish()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(store.run(&run.id)?.state, "waiting_recovery");
    assert_eq!(store.event_count(&run.id, "model.started")?, 1);
    assert_eq!(store.event_count(&run.id, "model.pre_dispatch_retry")?, 0);
    let metrics = run_metrics::report(&store, &run.id)?;
    assert_eq!(metrics["tokens"]["not_dispatched_attempts"], 0);
    assert_eq!(metrics["tokens"]["unreported_attempts"], 1);
    Ok(())
}

#[test]
fn user_pause_during_connection_backoff_stops_before_the_next_request() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let reserved = TcpListener::bind("127.0.0.1:0")?;
    let address = reserved.local_addr()?;
    drop(reserved);
    let mut store = Store::open(&root)?;
    let run = store.create_run("Pause connection recovery",directory.path(),"custom",json!([]),json!({"model":"fixture","endpoint":{"base_url":format!("http://{address}/v1"),"api_key_env":null},"model_seconds":5,"wall_seconds":30}),"")?;
    let watched_root = root.clone();
    let watched_id = run.id.clone();
    let pause = std::thread::spawn(move || -> Result<()> {
        let mut watched = Store::open(&watched_root)?;
        let deadline = Instant::now() + Duration::from_secs(25);
        while watched.event_count(&watched_id, "model.pre_dispatch_retry")? == 0 {
            ensure!(Instant::now() < deadline, "retry was not scheduled");
            std::thread::sleep(Duration::from_millis(10));
        }
        watched.request_pause(&watched_id)
    });
    let output = Command::new(env!("CARGO_BIN_EXE_arun"))
        .args(["serve", root.to_str().unwrap(), &run.id])
        .output()?;
    pause.join().unwrap()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(store.run(&run.id)?.state, "paused");
    assert_eq!(store.event_count(&run.id, "model.started")?, 1);
    assert!(store.operations(&run.id)?.is_empty());
    Ok(())
}
