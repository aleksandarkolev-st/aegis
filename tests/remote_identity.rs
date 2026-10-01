use std::process::Command;

#[test]
fn remote_identity_is_stable_and_needs_no_relay_or_credentials() {
    let workspace = tempfile::tempdir().unwrap();

    let identity = || {
        let output = Command::new(env!("CARGO_BIN_EXE_arun"))
            .args(["remote", "identity"])
            .current_dir(workspace.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value = String::from_utf8(output.stdout).unwrap();
        let value = value.trim();
        assert_eq!(uuid::Uuid::parse_str(value).unwrap().to_string(), value);
        value.to_owned()
    };

    let first = identity();
    assert_eq!(identity(), first);
    assert!(!workspace.path().join(".arun/remote.json").exists());
}
