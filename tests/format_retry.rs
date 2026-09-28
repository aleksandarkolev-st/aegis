use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use arun::storage::Store;
use serde_json::json;

#[path = "support/http.rs"]
mod http;

fn exercise(second_valid: bool) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let requests = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&requests);
    let endpoint = http::Endpoint::start(move |body| {
        let request = observed.fetch_add(1, Ordering::SeqCst);
        if request == 1 {
            let state = http::state(body)?;
            assert!(
                state["format_recovery_policy"]
                    .as_str()
                    .is_some_and(|policy| policy.contains("one valid JSON action"))
            );
            assert!(
                !body
                    .to_string()
                    .contains("SENSITIVE_PROVIDER_TEXT_FORMAT_RETRY")
            );
        }
        assert!(request < 2, "unexpected third model request");
        let content = if request == 0 {
            "SENSITIVE_PROVIDER_TEXT_FORMAT_RETRY".to_owned()
        } else if second_valid {
            json!({"kind":"blocked","reason":"fixture completed"}).to_string()
        } else {
            "still not JSON".to_owned()
        };
        Ok((
            200,
            json!({"choices":[{"message":{"content":content}}],
                "usage":{"prompt_tokens":10,"completion_tokens":2}}),
        ))
    })?;
    let output = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .env("PATH", directory.path())
        .args([
            "run",
            "Handle a bounded malformed provider reply",
            "--provider",
            "custom",
            "--endpoint",
            &endpoint.url,
            "--model",
            "fixture-model",
            "--actions",
            "2",
            "--model-tokens",
            "1000",
            "--wall-seconds",
            "30",
            "--foreground",
        ])
        .output()?;
    endpoint.finish()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    let store = Store::open(&directory.path().join(".arun"))?;
    let run = store.runs()?.remove(0);
    assert_eq!(run.state, "waiting_recovery");
    assert_eq!(store.event_count(&run.id, "model.started")?, 2);
    assert_eq!(store.event_count(&run.id, "model.format_retry")?, 1);
    assert_eq!(
        store.event_count(&run.id, "model.failed")?,
        if second_valid { 1 } else { 2 }
    );
    assert_eq!(
        store.event_count(&run.id, "model.response")?,
        usize::from(second_valid) as i64
    );
    assert_eq!(store.model_tokens(&run.id)?, 24);
    assert!(store.operations(&run.id)?.is_empty());
    assert!(!store.events(&run.id)?.iter().any(|event| {
        event
            .payload
            .to_string()
            .contains("SENSITIVE_PROVIDER_TEXT_FORMAT_RETRY")
    }));
    Ok(())
}

#[test]
fn one_invalid_format_is_retried_with_accounted_usage_and_no_raw_text() -> Result<()> {
    exercise(true)
}

#[test]
fn repeated_invalid_format_pauses_after_one_retry() -> Result<()> {
    exercise(false)
}

#[test]
fn unaccounted_invalid_format_does_not_trigger_an_extra_request() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let requests = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&requests);
    let endpoint = http::Endpoint::start(move |_| {
        observed.fetch_add(1, Ordering::SeqCst);
        Ok((200, json!({"choices":[{"message":{"content":"not JSON"}}]})))
    })?;
    let output = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .env("PATH", directory.path())
        .args([
            "run",
            "Reject unaccounted malformed provider replies",
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
    assert!(output.status.success());
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    let store = Store::open(&directory.path().join(".arun"))?;
    let run = store.runs()?.remove(0);
    assert_eq!(run.state, "waiting_recovery");
    assert_eq!(store.event_count(&run.id, "model.format_retry")?, 0);
    assert_eq!(store.event_count(&run.id, "model.failed")?, 1);
    assert_eq!(store.model_tokens(&run.id)?, 0);
    Ok(())
}
