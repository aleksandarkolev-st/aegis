use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::Result;
use serde_json::Value;

#[test]
fn guided_presets_and_custom_limits_are_saved_without_terminal_commands() -> Result<()> {
    for (choice, fields, seconds, command_seconds) in [
        ("1", "", 14_400, 600),
        ("2", "", 3600, 60),
        ("3", "20\n400000\n10800\n7200\n180\n256000\n", 10_800, 7200),
    ] {
        let directory = tempfile::tempdir()?;
        let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
            .current_dir(directory.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child.stdin.take().unwrap().write_all(
            format!("4\nhttp://127.0.0.1:9/v1\n1\n\nfixture\n2\n{choice}\n{fields}1\n/quit\n")
                .as_bytes(),
        )?;
        let output = child.wait_with_output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let profile: Value =
            serde_json::from_slice(&fs::read(directory.path().join(".arun/profile.json"))?)?;
        assert_eq!(profile["limits"]["wall_seconds"], seconds);
        assert_eq!(profile["limits"]["process_seconds"], command_seconds);
        assert!(String::from_utf8_lossy(&output.stdout).contains("Task budget"));
    }
    Ok(())
}
