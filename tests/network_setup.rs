use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::Result;
use arun::storage::Store;
use serde_json::{Value, json};

#[test]
fn web_scopes_are_optional_inline_and_preserve_other_permissions() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    fs::create_dir(&root)?;
    let path = root.join("profile.json");
    fs::write(
        &path,
        serde_json::to_vec(
            &json!({"provider":"custom","model":"fixture","endpoint":{"base_url":"http://127.0.0.1:9/v1","api_key_env":null},"write":false,"image":null,"filesystem_scopes":{"read":["src/**"],"write":[]}}),
        )?,
    )?;
    for (input, enabled) in [
        (b"/settings\n8\n2\nExample.com\n\n/quit\n".as_slice(), true),
        (b"/settings\n8\n3\n/quit\n".as_slice(), false),
    ] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
            .current_dir(directory.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        child.stdin.take().unwrap().write_all(input)?;
        let output = child.wait_with_output()?;
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("Settings saved"));
        let profile: Value = serde_json::from_slice(&fs::read(&path)?)?;
        assert_eq!(profile["write"], false);
        assert_eq!(
            profile["filesystem_scopes"],
            json!({"read":["src/**"],"write":[]})
        );
        if enabled {
            assert_eq!(
                profile["network_scopes"],
                json!({"domains":["example.com"],"body_bytes":8388608})
            );
        } else {
            assert!(profile["network_scopes"].is_null());
        }
    }
    assert!(Store::open(&root)?.runs()?.is_empty());
    Ok(())
}
