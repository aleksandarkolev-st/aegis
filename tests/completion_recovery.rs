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
fn rejected_finish_recovers_by_updating_the_plan_without_repeating_tools() -> Result<()> {
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("input.txt"), "inspected content")?;
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let endpoint = http::Endpoint::start(move |body| {
        let state = http::state(body)?;
        let proof = || {
            state["recent_operation_outcomes"][0]["artifact"]
                .as_str()
                .unwrap()
        };
        let action = match observed.fetch_add(1, Ordering::SeqCst) {
            0 => {
                assert!(state.get("plan_completion_policy").is_none());
                json!({"kind":"checkpoint","checkpoint":{"decisions":["Inspect the requested file"],"unresolved":[],"next_action":"Read input.txt","milestones":[{"title":"Inspect the file","state":"active","evidence":[]}]}})
            }
            1 => json!({"kind":"invoke","capability":"workspace.read","args":{"path":"input.txt"}}),
            2 => json!({"kind":"finish","summary":"Inspected the file","evidence":[proof()]}),
            3 => {
                assert_eq!(state["milestones"][0]["state"], "active");
                let policy = state["plan_completion_policy"].as_str().unwrap();
                assert!(policy.contains("save a checkpoint"));
                assert!(policy.contains("Historical inspection evidence"));
                assert!(
                    state["recent_events"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|event| event["kind"] == "action.rejected")
                );
                json!({"kind":"checkpoint","checkpoint":{"decisions":["Inspect the requested file"],"unresolved":[],"next_action":"Finish with existing proof","milestones":[{"title":"Inspect the file","state":"completed","evidence":[proof()]}]}})
            }
            4 => {
                assert_eq!(state["milestones"][0]["state"], "completed");
                assert_eq!(state["plan_completion_ready"], true);
                assert!(state.get("plan_completion_policy").is_none());
                json!({"kind":"finish","summary":"Inspected content verified","evidence":[proof()]})
            }
            _ => anyhow::bail!("Unexpected inference after completion"),
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
            "Inspect input.txt",
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
    endpoint.finish()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let store = Store::open(&directory.path().join(".arun"))?;
    let run = store.runs()?.remove(0);
    assert_eq!(run.state, "completed");
    assert_eq!(store.operations(&run.id)?.len(), 1);
    assert_eq!(store.event_count(&run.id, "action.rejected")?, 1);
    assert_eq!(calls.load(Ordering::SeqCst), 5);
    Ok(())
}

#[test]
fn original_inspection_stays_completed_after_an_edit_but_stale_finish_is_rejected() -> Result<()> {
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("input.txt"), "original")?;
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let endpoint = http::Endpoint::start(move |body| {
        let state = http::state(body)?;
        let latest = || {
            state["recent_operation_outcomes"][0]["artifact"]
                .as_str()
                .unwrap()
        };
        let original = || state["milestones"][0]["evidence"][0].as_str().unwrap();
        let checkpoint = |inspection: serde_json::Value, implementation: serde_json::Value| json!({"kind":"checkpoint","checkpoint":{"decisions":[],"unresolved":[],"next_action":"Continue repair","milestones":[inspection,implementation]}});
        let action = match observed.fetch_add(1, Ordering::SeqCst) {
            0 => checkpoint(
                json!({"title":"Inspect original","state":"active","evidence":[]}),
                json!({"title":"Implement","state":"pending","evidence":[]}),
            ),
            1 => json!({"kind":"invoke","capability":"workspace.read","args":{"path":"input.txt"}}),
            2 => checkpoint(
                json!({"title":"Inspect original","state":"completed","evidence":[latest()]}),
                json!({"title":"Implement","state":"active","evidence":[]}),
            ),
            3 => {
                json!({"kind":"invoke","capability":"workspace.write","args":{"path":"input.txt","content":"repaired"}})
            }
            4 => json!({"kind":"finish","summary":"Repaired","evidence":[original()]}),
            5 => {
                assert_eq!(state["workspace_revision"], 2);
                assert!(
                    state["recent_events"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|event| event["kind"] == "action.rejected")
                );
                checkpoint(
                    json!({"title":"Inspect original","state":"completed","evidence":[original()]}),
                    json!({"title":"Implement","state":"completed","evidence":[latest()]}),
                )
            }
            6 => {
                assert_eq!(state["plan_completion_ready"], true);
                assert!(state.get("plan_completion_policy").is_none());
                assert_ne!(original(), latest());
                json!({"kind":"finish","summary":"Repair read back successfully","evidence":[latest()]})
            }
            _ => anyhow::bail!("Unexpected inference after completion"),
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
            "Repair input.txt",
            "--allow-write",
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
    endpoint.finish()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let store = Store::open(&directory.path().join(".arun"))?;
    let run = store.runs()?.remove(0);
    assert_eq!(run.state, "completed");
    assert_eq!(store.operations(&run.id)?.len(), 2);
    assert_eq!(store.event_count(&run.id, "action.rejected")?, 1);
    assert_eq!(calls.load(Ordering::SeqCst), 7);
    assert_eq!(
        std::fs::read_to_string(directory.path().join("input.txt"))?,
        "repaired"
    );
    Ok(())
}
