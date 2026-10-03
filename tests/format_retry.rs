use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use arun::storage::Store;
use serde_json::json;

#[path = "support/http.rs"]
mod http;

#[test]
fn model_response_deadline_pauses_without_retry_or_unaccounted_success() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let requests = Arc::new(AtomicUsize::new(0));
    let observed = requests.clone();
    let endpoint = http::Endpoint::start(move |_| {
        observed.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(1500));
        Ok((200, json!({"choices":[{"message":{"content":"{}"}}]})))
    })?;
    let mut store = Store::open(&root)?;
    let run = store.create_run(
        "Bound a stalled model response",
        directory.path(),
        "custom",
        json!([]),
        json!({"model":"fixture", "endpoint":{"base_url":endpoint.url,"api_key_env":null},
            "model_seconds":1,"wall_seconds":20}),
        "",
    )?;
    let output = Command::new(env!("CARGO_BIN_EXE_arun"))
        .args(["serve", root.to_str().unwrap(), &run.id])
        .output()?;
    endpoint.finish()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(store.run(&run.id)?.state, "waiting_recovery");
    let events = store.events(&run.id)?;
    assert!(
        events
            .iter()
            .any(|event| event.kind == "run.waiting_recovery"
                && event.payload["reason"] == "model response deadline reached"),
        "{events:?}"
    );
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert_eq!(store.event_count(&run.id, "model.failed")?, 1);
    assert_eq!(store.event_count(&run.id, "model.response")?, 0);
    assert_eq!(store.event_count(&run.id, "model.format_retry")?, 0);
    assert_eq!(store.operations(&run.id)?.len(), 0);
    assert_eq!(store.model_tokens(&run.id)?, 0);
    Ok(())
}

#[test]
fn malformed_nested_content_recovers_with_separated_text_and_one_write() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let content = "export const quoted = '\"';\n// Київ \\ path\n".repeat(100);
    let expected = content.clone();
    let observed = Arc::new(AtomicUsize::new(0));
    let requests = observed.clone();
    let endpoint = http::Endpoint::start(move |body| {
        let state = http::state(body)?;
        let action = match requests.fetch_add(1, Ordering::SeqCst) {
            0 => {
                json!({"kind":"invoke","capability":"workspace.write","args":"SENSITIVE_BROKEN_NESTED_CONTENT"})
            }
            1 => {
                let hint = state["format_recovery_policy"].as_str().unwrap();
                assert!(hint.contains("args_text_field") && hint.contains("escaping once"));
                assert!(!body.to_string().contains("SENSITIVE_BROKEN_NESTED_CONTENT"));
                json!({"kind":"invoke","capability":"workspace.write","args":"{\"path\":\"index.mjs\"}","args_text_field":"content","args_text":content})
            }
            2 => {
                json!({"kind":"finish","summary":"Repaired text payload","evidence":[state["recent_operation_outcomes"][0]["artifact"]]})
            }
            _ => anyhow::bail!("unexpected retry after repaired action"),
        };
        Ok((
            200,
            json!({"choices":[{"message":{"content":action.to_string()}}],"usage":{"prompt_tokens":10,"completion_tokens":2}}),
        ))
    })?;
    let output = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .args([
            "run",
            "Recover a quoted source payload",
            "--provider",
            "custom",
            "--endpoint",
            &endpoint.url,
            "--model",
            "fixture",
            "--mode",
            "eager",
            "--allow-write",
            "--foreground",
        ])
        .output()?;
    endpoint.finish()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("index.mjs"))?,
        expected
    );
    let store = Store::open(&directory.path().join(".arun"))?;
    let run = store.runs()?.remove(0);
    assert_eq!(run.state, "completed");
    assert_eq!(store.operations(&run.id)?.len(), 1);
    assert_eq!(store.event_count(&run.id, "model.format_retry")?, 1);
    assert_eq!(store.model_tokens(&run.id)?, 36);
    assert_eq!(observed.load(Ordering::SeqCst), 3);
    Ok(())
}

#[test]
fn separated_large_file_text_executes_exactly_once_through_the_normal_kernel() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let content =
        "C:\\Windows\\path\nconst value = {\"quoted\": '\"', city: 'Київ 🦀'};\n".repeat(1000);
    let expected = content.clone();
    let observed = Arc::new(AtomicUsize::new(0));
    let requests = observed.clone();
    let endpoint = http::Endpoint::start(move |body| {
        let state = http::state(body)?;
        let action = match requests.fetch_add(1, Ordering::SeqCst) {
            0 => {
                json!({"kind":"invoke","capability":"workspace.write","args":"{\"path\":\"code.txt\"}","args_text_field":"content","args_text":content})
            }
            1 => {
                json!({"kind":"finish","summary":"Exact text written","evidence":[state["recent_operation_outcomes"][0]["artifact"]]})
            }
            _ => anyhow::bail!("unexpected model retry"),
        };
        Ok((
            200,
            json!({"choices":[{"message":{"content":action.to_string()}}],"usage":{"prompt_tokens":10,"completion_tokens":2}}),
        ))
    })?;
    let output = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .args([
            "run",
            "Write exact Unicode source text",
            "--provider",
            "custom",
            "--endpoint",
            &endpoint.url,
            "--model",
            "fixture",
            "--mode",
            "eager",
            "--allow-write",
            "--foreground",
        ])
        .output()?;
    endpoint.finish()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("code.txt"))?,
        expected
    );
    let store = Store::open(&directory.path().join(".arun"))?;
    let run = store.runs()?.remove(0);
    assert_eq!(run.state, "completed");
    assert_eq!(store.operations(&run.id)?.len(), 1);
    assert_eq!(store.event_count(&run.id, "model.format_retry")?, 0);
    assert_eq!(observed.load(Ordering::SeqCst), 2);
    Ok(())
}

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
fn malformed_json_action_arguments_get_one_accounted_retry_without_tool_dispatch() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let requests = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&requests);
    let endpoint = http::Endpoint::start(move |body| {
        let request = observed.fetch_add(1, Ordering::SeqCst);
        assert!(request < 2);
        if request == 1 {
            let state = http::state(body)?;
            assert!(
                state["format_recovery_policy"]
                    .as_str()
                    .unwrap()
                    .contains("both JSON layers")
            );
            assert!(!body.to_string().contains("not JSON"));
        }
        let content = if request == 0 {
            json!({"kind":"invoke","capability":"workspace.read","args":"not JSON"}).to_string()
        } else {
            json!({"kind":"blocked","reason":"fixture finished"}).to_string()
        };
        Ok((
            200,
            json!({"choices":[{"message":{"content":content}}],"usage":{"prompt_tokens":10,"completion_tokens":2}}),
        ))
    })?;
    let output = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .args([
            "run",
            "Check bounded format recovery",
            "--provider",
            "custom",
            "--endpoint",
            &endpoint.url,
            "--model",
            "fixture",
            "--foreground",
        ])
        .output()?;
    endpoint.finish()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let store = Store::open(&directory.path().join(".arun"))?;
    let run = store.runs()?.remove(0);
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    assert_eq!(store.event_count(&run.id, "model.format_retry")?, 1);
    assert_eq!(store.model_tokens(&run.id)?, 24);
    assert!(store.operations(&run.id)?.is_empty());
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
