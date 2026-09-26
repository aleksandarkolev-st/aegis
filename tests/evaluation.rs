use std::fs;
use std::process::Command;

use anyhow::Result;
use arun::{capability, storage::Store};
use serde_json::Value;

#[test]
fn prepares_paired_saved_cases_without_running_models() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let output = Command::new(env!("CARGO_BIN_EXE_arun"))
        .args([
            "eval",
            "--prepare-only",
            "--sizes",
            "50,500",
            "--tasks",
            "read",
            "--modes",
            "eager,durable",
            "--restart-at",
            "operation.executing",
        ])
        .current_dir(directory.path())
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let evaluations = directory.path().join(".arun/evaluations");
    let experiment = fs::read_dir(evaluations)?.next().unwrap()?.path();
    let cases: Vec<Value> = serde_json::from_slice(&fs::read(experiment.join("cases.json"))?)?;
    assert_eq!(cases.len(), 4);
    assert!(experiment.join("experiment.json").exists());
    assert!(!experiment.join("results.jsonl").exists());
    for case in cases {
        let store = Store::open(std::path::Path::new(case["root"].as_str().unwrap()))?;
        let id = case["run_id"].as_str().unwrap();
        let run = store.run(id)?;
        assert_eq!(run.state, "ready");
        assert_eq!(case["restart_at"], "operation.executing");
        assert_eq!(store.event_count(id, "model.started")?, 0);
        assert_eq!(
            capability::all(&store)?.len() as u64,
            case["size"].as_u64().unwrap()
        );
        assert!(
            std::path::Path::new(&run.workspace)
                .join("fixture.txt")
                .exists()
        );
    }
    Ok(())
}
