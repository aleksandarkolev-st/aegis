use anyhow::{Result, ensure};
use arun::{kernel, storage::Store};
use serde_json::json;
use std::sync::{Arc, Mutex};

#[path = "support/http.rs"]
mod http;

#[test]
fn large_answers_are_read_once_compacted_and_retained_after_restart() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let mut store = Store::open(&root)?;
    let calls = Arc::new(Mutex::new(0usize));
    let observed = calls.clone();
    let handles = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured = handles.clone();
    let endpoint = http::Endpoint::start(move |body| {
        let state = http::state(body)?;
        ensure!(
            body.to_string().chars().count() < 180_000,
            "Prompt grew beyond bounded working context"
        );
        let mut call = observed.lock().unwrap();
        let mut handles = captured.lock().unwrap();
        if *call == 0 {
            ensure!(state["working_memory"]["owner_compaction_required"] == true);
            ensure!(state["user_answers"].to_string().chars().count() < 5000);
        }
        if *call < 10 && *call % 2 == 1 {
            ensure!(
                state["recent_events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|event| event["kind"] == "owner.source_read"
                        && event["payload"]["text"]
                            .as_str()
                            .is_some_and(|text| text.ends_with(&format!("PRESERVE_API_AUDIT_TAIL_{}",*call/2)))),
                "Full answer tail was lost"
            );
        }
        let action = match *call {
            index @ 0..=9 if index % 2 == 0 => {
                let handle=format!("user:{}",state["working_memory"]["owner_batch_through"].as_i64().unwrap());
                ensure!(!handles.contains(&handle),"Source was reread instead of advancing memory");
                handles.push(handle.clone());
                json!({"kind":"inspect_result","artifact":handle,"query":"@full"})
            }
            0..=9 => {
                json!({"kind":"remember","summary":format!("Preserve public APIs {} throughout this task; continue exploring.",(0..=*call/2).map(|index|format!("PRESERVE_API_AUDIT_TAIL_{index}")).collect::<Vec<_>>().join(", ")),"artifact":format!("user:{}",state["working_memory"]["owner_batch_through"].as_i64().unwrap())})
            }
            10 => {
                ensure!(state["working_memory"]["owner_compaction_required"] == false);
                ensure!(
                    state["working_memory"]["summary"]
                        .as_str()
                        .unwrap()
                        .contains("PRESERVE_API_AUDIT_TAIL")
                );
                json!({"kind":"search_capabilities","query":"workspace.read"})
            }
            11 => {
                ensure!(
                    state["working_memory"]["summary"]
                        .as_str()
                        .unwrap()
                        .contains("PRESERVE_API_AUDIT_TAIL")
                );
                json!({"kind":"blocked","reason":"Fixture complete"})
            }
            _ => anyhow::bail!("Unexpected extra model request"),
        };
        *call += 1;
        Ok((
            200,
            json!({"choices":[{"message":{"content":action.to_string()}}],"usage":{"prompt_tokens":10,"completion_tokens":2}}),
        ))
    })?;
    let run=store.create_run("Explore the loop",directory.path(),"custom",json!(["workspace.read"]),json!({"mode":"durable","model":"fixture","endpoint":{"base_url":endpoint.url,"api_key_env":null},"wall_seconds":30}),"")?;
    store.state(&run.id, "running", json!({}))?;
    for index in 0..5 {
        store.ask_user(&run.id, &format!("Constraint {index}?"))?;
        let marker=format!("PRESERVE_API_AUDIT_TAIL_{index}");
        store.steer(
            &run.id,
            &format!(
                "{}{marker}",
                "X".repeat(65_536 - marker.len())
            ),
        )?;
    }
    kernel::drive(&root, &run.id)?;
    endpoint.finish()?;
    assert_eq!(*calls.lock().unwrap(), 12);
    assert_eq!(handles.lock().unwrap().len(),5);
    assert_eq!(store.event_count(&run.id, "context.over_limit")?, 0);
    assert_eq!(store.event_count(&run.id, "action.rejected")?, 0);
    assert_eq!(store.event_count(&run.id, "memory.saved")?, 5);
    assert_eq!(store.question_answers(&run.id)?.len(), 5);
    for _ in 0..1100 {
        store.event(&run.id, "telemetry", json!({}))?;
    }
    store.maintain_history(&run.id)?;
    drop(store);
    let store = Store::open(&root)?;
    let state = kernel::normalized_handoff(&store, &store.run(&run.id)?)?;
    assert!(
        state["working_memory"]["summary"]
            .as_str()
            .unwrap()
            .contains("PRESERVE_API_AUDIT_TAIL")
    );
    assert!(store.pending_steering(&run.id)?.is_empty());
    for index in 0..5 {assert!(state["working_memory"]["summary"].as_str().unwrap().contains(&format!("PRESERVE_API_AUDIT_TAIL_{index}")));}
    Ok(())
}
