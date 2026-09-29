use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::Result;
use serde_json::Value;

#[test]
fn guided_image_download_requires_a_choice_and_records_the_environment() -> Result<()> {
    for download in [false, true] {
        let directory = tempfile::tempdir()?;
        let docker = directory.path().join(if cfg!(windows) {
            "docker.cmd"
        } else {
            "docker"
        });
        let fixture = if cfg!(windows) {
            "@echo off\r\nif \"%~1\"==\"pull\" echo %~3>\"%~dp0download.txt\"\r\nexit /b 0\r\n"
        } else {
            "#!/bin/sh\nif [ \"$1\" = pull ]; then printf '%s' \"$3\" > \"${0%/*}/download.txt\"; fi\nexit 0\n"
        };
        fs::write(&docker, fixture)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(docker, fs::Permissions::from_mode(0o755))?;
        }
        let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
            .current_dir(directory.path())
            .env("PATH", directory.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let choice = if download { 1 } else { 3 };
        child.stdin.take().unwrap().write_all(
            format!("5\nhttp://127.0.0.1:9/v1\n1\n\nfixture\n3\n{choice}\n/quit\n").as_bytes(),
        )?;
        let output = child.wait_with_output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout)
                .contains("Prepare an isolated command environment")
        );
        let profile: Value =
            serde_json::from_slice(&fs::read(directory.path().join(".arun/profile.json"))?)?;
        assert_eq!(profile["write"], true);
        assert_eq!(
            profile["image"],
            if download {
                Value::from("node:22-alpine")
            } else {
                Value::Null
            }
        );
        assert_eq!(directory.path().join("download.txt").exists(), download);
        if download {
            assert_eq!(
                fs::read_to_string(directory.path().join("download.txt"))?.trim(),
                "node:22-alpine"
            );
        }
    }
    Ok(())
}
