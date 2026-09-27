use std::fs;
use std::process::Command;

use anyhow::Result;
use arun::{capability, storage::Store};
use serde_json::Value;

#[test]
fn prepares_paired_saved_cases_without_running_models() -> Result<()> {
    let directory = tempfile::tempdir()?;
    for name in ["codex", "grok", "claude"] {
        let trap = directory.path().join(if cfg!(windows) {
            format!("{name}.cmd")
        } else {
            name.into()
        });
        fs::write(
            &trap,
            if cfg!(windows) {
                "@echo bad > native-started.txt\r\n@echo native-fixture-version\r\n"
            } else {
                "#!/bin/sh\nprintf bad > native-started.txt\nprintf native-fixture-version\n"
            },
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(trap, fs::Permissions::from_mode(0o755))?;
        }
    }
    let mut paths = vec![directory.path().to_owned()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
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
        .env("PATH", std::env::join_paths(paths)?)
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
    let manifest: Value = serde_json::from_slice(&fs::read(experiment.join("experiment.json"))?)?;
    assert_eq!(manifest["provider_transport"], "aegis-direct-v1");
    assert_eq!(manifest["native_provider_cli_started"], false);
    assert!(manifest["provider_cli_version"].is_null());
    assert!(!directory.path().join("native-started.txt").exists());
    assert!(!experiment.join("results.jsonl").exists());
    for case in cases {
        let store = Store::open(std::path::Path::new(case["root"].as_str().unwrap()))?;
        let id = case["run_id"].as_str().unwrap();
        let run = store.run(id)?;
        assert_eq!(run.state, "ready");
        assert_eq!(run.budgets["provider_transport"], "aegis-direct-v1");
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
