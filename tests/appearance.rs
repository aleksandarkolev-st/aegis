use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::Result;
use serde_json::json;

#[test]
fn optional_appearance_settings_preserve_runtime_profile_and_reload_without_setup() -> Result<()> {
    for preset in 1..=4 {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join(".arun");
        fs::create_dir(&root)?;
        let profile = serde_json::to_vec(
            &json!({"provider":"custom","model":"fixture","endpoint":{"base_url":"http://127.0.0.1:9/v1","api_key_env":null},"write":false,"image":null}),
        )?;
        fs::write(root.join("profile.json"), &profile)?;
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
            .write_all(format!("/settings\n5\n{preset}\n/quit\n").as_bytes())?;
        let output = child.wait_with_output()?;
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("Looking good"));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("Your look, your workflow"));
        assert_eq!(fs::read(root.join("profile.json"))?, profile);
        let style = arun::ui::UiOptions::from_file(&root.join("ui.json"))?;
        assert_eq!(
            serde_json::to_value(style)?,
            serde_json::to_value(arun::ui::UiOptions::preset(preset - 1))?
        );
        let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
            .current_dir(directory.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        child.stdin.take().unwrap().write_all(b"/quit\n")?;
        let output = child.wait_with_output()?;
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success());
        assert!(!text.contains("Choose your provider") && !text.contains("Make Aegis yours"));
        assert_eq!(text.contains("| o o |"), preset == 1);
        assert_eq!(text.contains("[ o o ]"), preset == 2);
    }
    Ok(())
}

#[test]
fn invalid_optional_style_falls_back_without_blocking_or_running_code() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    fs::create_dir(&root)?;
    fs::write(
        root.join("profile.json"),
        serde_json::to_vec(
            &json!({"provider":"claude","model":null,"endpoint":null,"write":false,"image":null}),
        )?,
    )?;
    fs::write(
        root.join("ui.json"),
        r#"{"script":"write a file","frames":["\u001b[2J"]}"#,
    )?;
    let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    child.stdin.take().unwrap().write_all(b"/quit\n")?;
    let output = child.wait_with_output()?;
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("Style fallback") && text.contains("| o o |"));
    assert!(!text.contains('\u{1b}'));
    assert_eq!(fs::read_dir(directory.path())?.count(), 1);
    Ok(())
}
