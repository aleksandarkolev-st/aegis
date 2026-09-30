use anyhow::Result;
use arun::{control, storage::Store};
use serde_json::json;
use std::{
    fs,
    io::Write,
    process::{Command, Stdio},
};

#[test]
fn explicit_requirements_preview_can_pause_before_any_inference() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    fs::create_dir(&root)?;
    fs::write(
        root.join("profile.json"),
        serde_json::to_vec(
            &json!({"provider":"custom","model":"fixture","endpoint":{"base_url":"http://127.0.0.1:9/v1","api_key_env":null},"write":false,"image":null,"previous_run":null}),
        )?,
    )?;
    let task =
        "Refactor parser.\nRequirements:\n- support nested expressions\n- preserve public API";
    let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("\u{1b}[200~{task}\u{1b}[201~\n4\n/goal\n/quit\n").as_bytes())?;
    let output = child.wait_with_output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    for label in [
        "Detected requirements",
        "Start task",
        "Edit requirements",
        "Add requirement",
        "Leave task paused",
    ] {
        assert!(text.contains(label), "{text}");
    }
    let store = Store::open(&root)?;
    let run = store.runs()?.remove(0);
    assert_eq!(run.task, task);
    assert_eq!(run.state, "paused");
    assert_eq!(store.obligations(&run.id)?.len(), 3);
    assert_eq!(store.event_count(&run.id, "model.started")?, 0);
    assert!(store.last_checkpoint(&run.id)?.is_some());
    Ok(())
}

#[test]
fn goal_prefix_preserves_a_multiline_paste_and_opens_the_task_review() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    fs::create_dir(&root)?;
    fs::write(
        root.join("profile.json"),
        serde_json::to_vec(
            &json!({"provider":"custom","model":"fixture","endpoint":{"base_url":"http://127.0.0.1:9/v1","api_key_env":null},"write":false,"image":null,"previous_run":null}),
        )?,
    )?;
    let task =
        "Repair the command picker.\nRequirements:\n- preserve pasted text\n- keep goal history";
    let paste = format!("/goal\n{task}");
    let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("\u{1b}[200~{paste}\u{1b}[201~\n4\n/goal\n/quit\n").as_bytes())?;
    let output = child.wait_with_output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    for label in [
        "Detected requirements",
        "Start task",
        "Edit requirements",
        "Leave task paused",
    ] {
        assert!(text.contains(label), "{text}");
    }
    let store = Store::open(&root)?;
    let run = store.runs()?.remove(0);
    assert_eq!(run.task, task);
    assert_eq!(run.state, "paused");
    assert_eq!(store.obligations(&run.id)?.len(), 3);
    assert_eq!(store.event_count(&run.id, "model.started")?, 0);
    Ok(())
}

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

#[test]
fn archived_receipts_and_provider_turns_survive_completion() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut store = Store::open(directory.path())?;
    let run = store.create_run("Check API\nRequirements:\n- preserve API",directory.path(),"codex",json!([]),json!({"provider_transport":"aegis-direct-v1","model":"primary","fallback_routes":[{"provider":"grok","model":"fallback"}]}),"")?;
    store.state(&run.id, "running", json!({}))?;
    let op = store.begin_operation(
        &run.id,
        "process.run",
        json!({"program":"node","args":["--test"]}),
        true,
    )?;
    let hash = store.put_artifact(&serde_json::to_vec(&json!({"exit_code":0}))?)?;
    store.operation_state(&op, "succeeded", Some(&hash), json!({"exit_code":0}))?;
    store.verify_obligation(&run.id, 1, &[hash.clone()])?;
    store.event(&run.id, "model.started", json!({"turn":84}))?;
    store.event(
        &run.id,
        "model.failed",
        json!({"recoverable_reason":"usage_limit"}),
    )?;
    assert!(
        store
            .transition_provider(&run.id, arun::routing::Reason::UsageLimit)?
            .is_some()
    );
    for _ in 0..180 {
        store.event(&run.id, "telemetry", json!({}))?;
    }
    store.save_snapshot(&run.id)?;
    assert!(store.archive_history(&run.id)? > 0);
    drop(store);
    let mut store = Store::open(directory.path())?;
    let proof = control::view(&store, &run.id, "evidence", Some("O1"))?;
    assert_eq!(
        proof["artifacts"][0]["operations"][0]["receipt"]["exit_code"],
        0
    );
    let providers = control::view(&store, &run.id, "provider", Some("history"))?;
    assert!(control::display("provider", &providers).contains("turn 84"));
    store.complete_run(&run.id, "API checked", &[hash])?;
    let completed = store
        .events(&run.id)?
        .into_iter()
        .find(|event| event.kind == "run.completed")
        .unwrap();
    let report = &completed.payload["completion"];
    assert_eq!(report["provider_transitions"].as_array().unwrap().len(), 1);
    assert_eq!(report["artifacts"][0]["receipt"]["exit_code"], 0);
    let text = control::display_completion(report);
    assert!(
        text.contains("turn 84") && text.contains("node") && text.contains("exit 0"),
        "{text}"
    );
    Ok(())
}
