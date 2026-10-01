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
#[path = "support/operation.rs"]
mod operation_fixture;

#[test]
fn failed_logs_survive_context_rotation_and_are_inspectable_but_cannot_prove_completion()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let mut store = Store::open(&root)?;
    let log = store.put_artifact(b"FAILED_TEST_DETAIL: expected SyntaxError")?;
    let failed = store.put_artifact(&serde_json::to_vec(
        &json!({"exit_code":1,"output_artifact":log,"bytes":38}),
    )?)?;
    let foreign_hash = store.put_artifact(b"PRIVATE_OTHER_RUN_OUTPUT")?;
    let count = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&count);
    let proof = failed.clone();
    let inspection = log.clone();
    let foreign = foreign_hash.clone();
    let endpoint = http::Endpoint::start(move |body| {
        let state = http::state(body)?;
        let process = state["recent_operation_outcomes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["capability"] == "process.run")
            .unwrap();
        assert_eq!(process["state"], "failed");
        assert_eq!(process["receipt"]["exit_code"], 1);
        assert_eq!(process["successful_current_evidence"], false);
        let action = match observed.fetch_add(1, Ordering::SeqCst) {
            0 => json!({"kind":"inspect_result","artifact":inspection,"query":""}),
            1 => {
                assert!(
                    state["recent_events"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|event| event["kind"] == "artifact.inspected"
                            && event["payload"]["excerpt"]
                                .as_str()
                                .is_some_and(|text| text.contains("FAILED_TEST_DETAIL")))
                );
                json!({"kind":"finish","summary":"Wrongly complete from failure","evidence":[proof],"obligations":[{"id":1,"evidence":[proof]}]})
            }
            2 => json!({"kind":"inspect_result","artifact":foreign,"query":""}),
            3 => {
                assert!(!state.to_string().contains("PRIVATE_OTHER_RUN_OUTPUT"));
                json!({"kind":"blocked","reason":"diagnostic fixture done"})
            }
            _ => panic!("Unexpected model request"),
        };
        Ok((
            200,
            json!({"choices":[{"message":{"content":action.to_string()}}]}),
        ))
    })?;
    let run = store.create_run(
        "Repair tests\nRequirements:\n- full suite passes",
        directory.path(),
        "custom",
        json!([]),
        json!({"model":"fixture","endpoint":{"base_url":endpoint.url,"api_key_env":null}}),
        "",
    )?;
    store.state(&run.id, "running", json!({}))?;
    let operation = store.begin_operation(
        &run.id,
        "process.run",
        json!({"program":"node","args":["--test"]}),
        true,
    )?;
    store.link_artifact(&operation.id, &log, "output")?;
    store.operation_state(&operation, "failed", Some(&failed), json!({"exit_code":1}))?;
    let other = store.create_run(
        "Other task",
        directory.path(),
        "custom",
        json!([]),
        json!({}),
        "",
    )?;
    let other_op = store.begin_operation(&other.id, "workspace.read", json!({}), true)?;
    operation_fixture::claim_fixture_operation(&mut store, &other_op.id)?;
    store.operation_state(&other_op, "succeeded", Some(&foreign_hash), json!({}))?;
    for index in 0..12 {
        let read = store.begin_operation(
            &run.id,
            "workspace.read",
            json!({"path":format!("read-{index}")}),
            true,
        )?;
        let hash = store.put_artifact(b"read fixture")?;
        operation_fixture::claim_fixture_operation(&mut store, &read.id)?;
        store.operation_state(&read, "succeeded", Some(&hash), json!({}))?;
    }
    for _ in 0..180 {
        store.event(&run.id, "telemetry", json!({}))?;
    }
    store.save_snapshot(&run.id)?;
    assert!(store.archive_history(&run.id)? > 0);
    assert!(store.has_operation_artifact(&run.id, &log)?);
    assert!(!store.has_evidence(&run.id, &failed)?);
    assert!(!store.has_operation_artifact(&run.id, &foreign_hash)?);
    assert!(
        store
            .inspectable_artifacts(&run.id)?
            .iter()
            .any(|(label, hash)| label.contains("failed") && hash == &log)
    );
    drop(store);
    let output = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .args(["serve", root.to_str().unwrap(), &run.id])
        .output()?;
    endpoint.finish()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let store = Store::open(&root)?;
    assert_eq!(count.load(Ordering::SeqCst), 4);
    assert_eq!(store.run(&run.id)?.state, "waiting_recovery");
    assert_eq!(store.obligations(&run.id)?[1].state, "open");
    assert_eq!(store.event_count(&run.id, "action.rejected")?, 2);
    Ok(())
}
