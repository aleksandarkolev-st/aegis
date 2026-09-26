use std::process::Command;

use anyhow::Result;

#[test]
fn version_flags_work_without_setup_profiles_or_provider_calls() -> Result<()> {
    let directory = tempfile::tempdir()?;
    for flag in ["--version", "-V"] {
        let output = Command::new(env!("CARGO_BIN_EXE_arun"))
            .current_dir(directory.path())
            .arg(flag)
            .output()?;
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout)?.trim(),
            format!("aegis {}", env!("CARGO_PKG_VERSION"))
        );
        assert!(output.stderr.is_empty());
    }
    assert!(!directory.path().join(".arun/profile.json").exists());
    assert!(!directory.path().join(".arun/runs.sqlite").exists());
    Ok(())
}
