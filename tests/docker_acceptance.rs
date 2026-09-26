use std::fs;
use std::process::Command;

use anyhow::Result;
use arun::{acceptance::Check, storage::Store, worker};
use serde_json::json;

#[test]
#[ignore = "requires Docker and the local node:22-alpine image"]
fn independent_acceptance_is_read_only_frozen_and_recoverable() -> Result<()> {
    for correct in [false, true] {
        let directory = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR"))?;
        let root = directory.path().join(".arun");
        let expression = if correct {
            "left + right"
        } else {
            "left - right"
        };
        fs::write(
            directory.path().join("math.mjs"),
            format!("export function sum(left,right) {{ return {expression}; }}"),
        )?;
        let script = "const fs=await import('node:fs'); const assert=(await import('node:assert/strict')).default; assert.equal(fs.existsSync('/workspace/.arun/runs.sqlite'),false); assert.throws(()=>fs.writeFileSync('./math.mjs','tampered')); const {sum}=await import('./math.mjs'); assert.equal(sum(2,3),5); assert.equal(sum(-2,3),1); assert.equal(sum(0,0),0);";
        let check = Check {
            name: "independent addition assertions".into(),
            program: "node".into(),
            args: vec!["--input-type=module".into(), "-e".into(), script.into()],
            image: "node:22-alpine".into(),
            seconds: 10,
        };
        let criteria = directory.path().join("acceptance.json");
        fs::write(&criteria, serde_json::to_vec(&check)?)?;
        let check = Check::from_file(&criteria)?;
        let mut store = Store::open(&root)?;
        let run = store.create_run(
            "repair addition",
            directory.path(),
            "intentionally-unavailable",
            json!(["workspace.read", "workspace.write"]),
            json!({"acceptance_check":check}),
            "addition assertions",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let read =
            store.begin_operation(&run.id, "workspace.read", json!({"path":"math.mjs"}), true)?;
        store.operation_state(&read, "dispatched", None, json!({}))?;
        let result = worker::execute(&root, &read.id)?;
        let evidence = store.put_artifact(&serde_json::to_vec(&result)?)?;
        store.operation_state(&read, "succeeded", Some(&evidence), json!({}))?;
        let proposal = store.put_artifact(&serde_json::to_vec(
            &json!({"summary":"repair complete","evidence":[evidence]}),
        )?)?;
        store.event(&run.id, "completion.proposed", json!({"artifact":proposal}))?;
        fs::write(
            &criteria,
            r#"{"name":"always pass","program":"node","args":["-e","process.exit(0)"],"image":"node:22-alpine"}"#,
        )?;
        let output = Command::new(env!("CARGO_BIN_EXE_arun"))
            .args(["serve", root.to_str().unwrap(), &run.id])
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(store.run(&run.id)?.state == "completed", correct);
        assert!(fs::read_to_string(directory.path().join("math.mjs"))?.contains(expression));
        assert_eq!(store.event_count(&run.id, "acceptance.started")?, 1);
        assert_eq!(
            store.event_count(&run.id, "acceptance.passed")?,
            i64::from(correct)
        );
        assert_eq!(
            store.event_count(&run.id, "acceptance.failed")?,
            i64::from(!correct)
        );
        if correct {
            assert_eq!(store.event_count(&run.id, "model.started")?, 0);
            let completion = store
                .events(&run.id)?
                .into_iter()
                .find(|event| event.kind == "run.completed")
                .unwrap();
            assert!(completion.payload["acceptance"].as_str().is_some());
        }
    }
    Ok(())
}
