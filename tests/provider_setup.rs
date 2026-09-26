use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use arun::storage::Store;

fn node() -> Result<std::path::PathBuf> {
    let directories = std::env::var_os("PATH").context("PATH missing")?;
    std::env::split_paths(&directories)
        .map(|directory| directory.join(if cfg!(windows) { "node.exe" } else { "node" }))
        .find(|path| path.is_file())
        .context("Node is required for the provider setup fixture")
}

#[test]
fn declining_setup_does_not_install_or_create_a_task() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let providers = directory.path().join("managed");
    let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .env("PATH", directory.path())
        .env("AEGIS_PROVIDER_HOME", &providers)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child.stdin.take().unwrap().write_all(b"3\n2\n")?;
    let output = child.wait_with_output()?;
    assert!(output.status.success());
    assert!(!providers.exists());
    assert!(
        Store::open(&directory.path().join(".arun"))?
            .runs()?
            .is_empty()
    );
    Ok(())
}

#[test]
fn missing_provider_installs_with_consent_without_manual_commands() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let node = node()?;
    let fixture = directory.path().join("installer.mjs");
    fs::write(
        &fixture,
        r#"
import { mkdirSync, writeFileSync } from 'node:fs';
import path from 'node:path';
const args = process.argv.slice(2);
if (args[0] !== 'install' || args.at(-1) !== '@xai-official/grok') process.exit(9);
const prefix = args[args.indexOf('--prefix') + 1];
const bin = path.join(prefix, 'node_modules', '.bin');
mkdirSync(bin, { recursive: true });
const model = path.join(prefix, 'model.mjs');
writeFileSync(model, 'console.log(JSON.stringify({text:JSON.stringify({kind:"blocked",reason:"fixture provider ready"})}));');
const wrapper = process.platform === 'win32'
  ? '@"' + process.execPath + '" "' + model + '" %*\r\n'
  : '#!/bin/sh\nexec "' + process.execPath + '" "' + model + '" "$@"\n';
writeFileSync(path.join(bin, process.platform === 'win32' ? 'grok.cmd' : 'grok'), wrapper, {mode:0o755});
writeFileSync(path.join(prefix, 'consent-install.json'), JSON.stringify(args));
"#,
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
    let npm = directory
        .path()
        .join(if cfg!(windows) { "npm.cmd" } else { "npm" });
    fs::write(&npm, wrapper)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(npm, fs::Permissions::from_mode(0o755))?;
    }
    let providers = directory.path().join("managed");
    let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .env("PATH", directory.path())
        .env("AEGIS_PROVIDER_HOME", &providers)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"3\n1\n1\n2\nRead the workspace\n/quit\n")?;
    let output = child.wait_with_output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Install the official provider CLI?"));
    assert!(stdout.contains("fixture provider ready"));
    assert!(providers.join("grok/consent-install.json").is_file());
    assert!(!directory.path().join("node_modules").exists());
    let store = Store::open(&directory.path().join(".arun"))?;
    assert_eq!(store.runs()?.len(), 1);
    assert_eq!(store.runs()?[0].state, "waiting_recovery");
    Ok(())
}
