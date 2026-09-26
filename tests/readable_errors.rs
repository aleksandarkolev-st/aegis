use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use arun::storage::Store;
use serde_json::json;

#[test]
fn guided_and_advanced_native_failures_show_recovery_hints_not_provider_json() -> Result<()> {
    let node = arun::provider::system_executable("node")
        .context("Node is required for the native provider fixture")?;
    for guided in [false, true] {
        let directory = tempfile::tempdir()?;
        let fixture = directory.path().join("error.cjs");
        fs::write(
            &fixture,
            r#"console.log(JSON.stringify({error:{type:'authentication_error',message:'OAuth access token has expired'},privateDiagnostic:'fixture-private-diagnostic'})); process.exit(1);"#,
        )?;
        let wrapper = if cfg!(windows) {
            format!("@\"{}\" \"{}\" %*\r\n", node.display(), fixture.display())
        } else {
            format!(
                "#!/bin/sh\nexec \"{}\" \"{}\" \"$@\"\n",
                node.display(),
                fixture.display()
            )
        };
        let cli = directory.path().join(if cfg!(windows) {
            "claude.cmd"
        } else {
            "claude"
        });
        fs::write(&cli, wrapper)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(cli, fs::Permissions::from_mode(0o755))?;
        }
        let root = directory.path().join(".arun");
        fs::create_dir(&root)?;
        fs::write(
            root.join("profile.json"),
            serde_json::to_vec(
                &json!({"provider":"claude","model":"sonnet","endpoint":null,"write":false,"image":null}),
            )?,
        )?;
        let mut command = Command::new(env!("CARGO_BIN_EXE_arun"));
        command
            .current_dir(directory.path())
            .env("PATH", directory.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = if guided {
            let mut child = command.stdin(Stdio::piped()).spawn()?;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(b"Read the workspace\n2\n/quit\n")?;
            child.wait_with_output()?
        } else {
            command
                .args([
                    "run",
                    "Read the workspace",
                    "--provider",
                    "claude",
                    "--model",
                    "sonnet",
                    "--foreground",
                ])
                .output()?
        };
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("F4"));
        assert!(!text.contains("privateDiagnostic"));
        assert!(!text.contains("fixture-private-diagnostic"));
        assert!(!text.contains("\"type\""));
        let store = Store::open(&root)?;
        let run = &store.runs()?[0];
        assert_eq!(run.state, "waiting_recovery");
        assert_eq!(store.event_count(&run.id, "model.failed")?, 1);
        assert!(store.operations(&run.id)?.is_empty());
        assert!(store.events(&run.id)?.iter().any(|event| {
            event.kind == "model.failed"
                && event.payload["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("fixture-private-diagnostic"))
        }));
    }
    Ok(())
}
