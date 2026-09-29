use anyhow::Result;
use arun::{control, storage::Store};
use serde_json::json;
use std::{
    fs,
    io::Write,
    process::{Command, Stdio},
};

#[test]
fn views_inspect_open_verified_and_stale_obligations_without_inference() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let mut store = Store::open(&root)?;
    let run = store.create_run(
        "Repair parser\nRequirements:\n- preserve API\n- add regression tests",
        directory.path(),
        "custom",
        json!(["workspace.read", "workspace.write"]),
        json!({"model":"fixture","endpoint":null}),
        "",
    )?;
    store.state(&run.id, "running", json!({}))?;
    let operation =
        store.begin_operation(&run.id, "workspace.read", json!({"path":"api.rs"}), true)?;
    let hash = store.put_artifact(b"API test receipt")?;
    store.operation_state(&operation, "succeeded", Some(&hash), json!({"exit_code":0}))?;
    store.verify_obligation(&run.id, 1, &[hash.clone()])?;
    store.save_checkpoint(&run.id,&serde_json::from_value(json!({"decisions":["Preserve v1"],"unresolved":["Missing tests"],"next_action":"Add regression tests","milestones":[]}))?)?;
    let events = store.events(&run.id)?.len();
    let handoff = control::view(&store, &run.id, "handoff", None)?;
    assert_eq!(handoff["task"], run.task);
    assert_eq!(handoff["obligations"][1]["state"], "verified");
    assert_eq!(handoff["handoff"]["next_action"], "Add regression tests");
    assert_eq!(
        control::view(&store, &run.id, "status", None)?["obligations"]["open"],
        1
    );
    let proof = control::view(&store, &run.id, "evidence", Some("O1"))?;
    assert_eq!(proof["artifacts"][0]["artifact"], hash);
    assert_eq!(
        proof["artifacts"][0]["operations"][0]["receipt"]["exit_code"],
        0
    );
    assert_eq!(proof["artifacts"][0]["operations"][0]["current"], true);
    assert_eq!(store.events(&run.id)?.len(), events);
    fs::write(
        root.join("profile.json"),
        serde_json::to_vec(
            &json!({"provider":"custom","model":"fixture","write":true,"image":null,"previous_run":run.id,"endpoint":{"base_url":"http://127.0.0.1:9/v1","api_key_env":null}}),
        )?,
    )?;
    let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child.stdin.take().unwrap().write_all(b"/goal\n/status\n/why\n/evidence O1\n/verify\n/provider history\n/budget\n/handoff\n/quit\n")?;
    let output = child.wait_with_output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("preserve API") && text.contains("[verified]"),
        "{text}"
    );
    assert!(text.contains("O2 remains open"));
    assert!(text.contains(&hash));
    assert_eq!(store.event_count(&run.id, "model.started")?, 0);
    let edit = store.begin_operation(&run.id, "workspace.write", json!({}), false)?;
    let edit_hash = store.put_artifact(b"edit")?;
    store.operation_state(&edit, "succeeded", Some(&edit_hash), json!({}))?;
    assert_eq!(
        control::view(&store, &run.id, "status", None)?["obligations"]["stale"],
        1
    );
    assert_eq!(
        control::view(&store, &run.id, "evidence", Some("O1"))?["artifacts"][0]["operations"][0]["current"],
        false
    );
    assert!(
        !control::view(&store, &run.id, "verify", None)?["blockers"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(control::view(&store, &run.id, "evidence", Some("O99")).is_err());
    Ok(())
}

#[test]
fn completion_explanation_is_saved_with_the_terminal_event() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut store = Store::open(directory.path())?;
    let run = store.create_run(
        "work",
        directory.path(),
        "custom",
        json!([]),
        json!({"obligations":["Read API"]}),
        "",
    )?;
    store.state(&run.id, "running", json!({}))?;
    let op = store.begin_operation(&run.id, "workspace.read", json!({}), true)?;
    let hash = store.put_artifact(b"proof")?;
    store.operation_state(&op, "succeeded", Some(&hash), json!({}))?;
    store.verify_obligation(&run.id, 1, &[hash.clone()])?;
    store.complete_run(&run.id, "API inspected", &[hash.clone()])?;
    let completed = store
        .events(&run.id)?
        .into_iter()
        .find(|event| event.kind == "run.completed")
        .unwrap();
    assert_eq!(
        completed.payload["completion"]["requirements"][1]["evidence"],
        json!([hash])
    );
    assert_eq!(
        completed.payload["completion"]["requirements"][1]["state"],
        "verified"
    );
    assert!(
        control::display_completion(&completed.payload["completion"])
            .contains("O1 Read API · verified · revision 0")
    );
    Ok(())
}

#[test]
fn goal_add_and_targeted_replace_require_review_and_retain_original_contract() -> Result<()> {
    for approval in [1, 2] {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join(".arun");
        let mut store = Store::open(&root)?;
        let run = store.create_run(
            "Repair parser\nRequirements:\n- preserve API",
            directory.path(),
            "custom",
            json!([]),
            json!({}),
            "",
        )?;
        fs::write(
            root.join("profile.json"),
            serde_json::to_vec(
                &json!({"provider":"custom","model":"fixture","write":false,"image":null,"previous_run":run.id,"endpoint":{"base_url":"http://127.0.0.1:9/v1","api_key_env":null}}),
            )?,
        )?;
        let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
            .current_dir(directory.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child.stdin.take().unwrap().write_all(format!("/goal add\nRegression tests\nUser requested coverage\n{approval}\n/goal replace O1\nAllow v2 API\nUser approved v2\n{approval}\n/goal history\n/quit\n").as_bytes())?;
        let output = child.wait_with_output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(store.run(&run.id)?.task, run.task);
        assert_eq!(store.run(&run.id)?.budgets, run.budgets);
        assert_eq!(store.event_count(&run.id, "model.started")?, 0);
        let ledger = store.obligations(&run.id)?;
        if approval == 1 {
            assert_eq!(ledger.len(), 2);
            assert_eq!(ledger[1].state, "open");
        } else {
            assert_eq!(ledger.len(), 4);
            assert_eq!(ledger[1].state, "superseded");
            assert_eq!(ledger[1].superseded_by, Some(3));
            assert_eq!(ledger[2].title, "Regression tests");
            assert_eq!(ledger[3].title, "Allow v2 API");
            assert!(
                store
                    .add_obligation(&run.id, "Regression tests", "Repeated")
                    .is_err()
            );
            let text = String::from_utf8_lossy(&output.stdout);
            assert!(text.contains("obligation.added"));
            assert!(text.contains("obligation.superseded"));
        }
    }
    Ok(())
}
