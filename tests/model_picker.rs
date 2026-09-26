use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde_json::{Value, json};

fn profile() -> Value {
    json!({"provider":"codex", "model":"old-model", "endpoint":null, "write":true, "image":"approved-image", "previous_run":"saved-task", "limits":{"actions":99,"wall_seconds":10800,"model_tokens":123456,"model_seconds":180,"process_seconds":900,"context_chars":256000,"model_response_bytes":8388608}})
}

#[test]
fn settings_change_only_the_selected_section_and_remove_unapproved_commands() -> Result<()> {
    for environment in [false, true] {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join(".arun");
        fs::create_dir(&root)?;
        let original = profile();
        fs::write(root.join("profile.json"), serde_json::to_vec(&original)?)?;
        let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
            .current_dir(directory.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child.stdin.take().unwrap().write_all(if environment {
            b"/settings\n1\n2\n/quit\n"
        } else {
            b"/settings\n2\n2\n/quit\n"
        })?;
        let output = child.wait_with_output()?;
        assert!(output.status.success());
        let saved: Value = serde_json::from_slice(&fs::read(root.join("profile.json"))?)?;
        for key in ["provider", "model", "endpoint", "previous_run"] {
            assert_eq!(saved[key], original[key]);
        }
        if environment {
            assert_eq!(saved["write"], false);
            assert!(saved["image"].is_null());
            assert_eq!(saved["limits"], original["limits"]);
        } else {
            assert_eq!(saved["limits"]["wall_seconds"], 3600);
            assert_eq!(saved["write"], original["write"]);
            assert_eq!(saved["image"], original["image"]);
        }
    }
    Ok(())
}

#[test]
fn model_and_provider_switches_keep_permissions_budgets_and_task_history() -> Result<()> {
    for provider_switch in [false, true] {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join(".arun");
        fs::create_dir(&root)?;
        let original = profile();
        fs::write(root.join("profile.json"), serde_json::to_vec(&original)?)?;
        let codex_home = directory.path().join("codex-home");
        fs::create_dir(&codex_home)?;
        fs::write(
            codex_home.join("models_cache.json"),
            serde_json::to_vec(
                &json!({"models":[{"slug":"catalog-model","display_name":"Friendly model","visibility":"list"},{"slug":"internal-hidden","visibility":"hide"}]}),
            )?,
        )?;
        let claude = directory.path().join(if cfg!(windows) {
            "claude.cmd"
        } else {
            "claude"
        });
        fs::write(&claude, "")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(claude, fs::Permissions::from_mode(0o755))?;
        }
        let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
            .current_dir(directory.path())
            .env("CODEX_HOME", codex_home)
            .env("PATH", directory.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child.stdin.take().unwrap().write_all(if provider_switch {
            b"/provider\n2\n/models\n2\n/quit\n"
        } else {
            b"/models\n2\n/quit\n"
        })?;
        let output = child.wait_with_output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let saved: Value = serde_json::from_slice(&fs::read(root.join("profile.json"))?)?;
        for key in ["write", "image", "limits", "previous_run"] {
            assert_eq!(saved[key], original[key]);
        }
        assert_eq!(
            saved["model"],
            if provider_switch {
                "sonnet"
            } else {
                "catalog-model"
            }
        );
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(!text.contains("Allow workspace edits"));
        assert!(!text.contains("internal-hidden"));
        assert!(text.contains(if provider_switch {
            "Claude Code model aliases"
        } else {
            "Friendly model"
        }));
    }
    Ok(())
}

#[test]
fn custom_setup_lists_authenticated_endpoint_models_without_saving_the_key() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    listener.set_nonblocking(true)?;
    let server = std::thread::spawn(move || -> Result<()> {
        let started = Instant::now();
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if started.elapsed() > Duration::from_secs(15) {
                        bail!("Model catalog was not requested");
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => return Err(error.into()),
            }
        };
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        let mut request = Vec::new();
        let mut buffer = [0; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let count = stream.read(&mut buffer)?;
            if count == 0 || request.len() > 16 * 1024 {
                bail!("Incomplete catalog request");
            }
            request.extend_from_slice(&buffer[..count]);
        }
        let headers = String::from_utf8(request)?.to_lowercase();
        assert!(headers.starts_with("get /v1/models "));
        assert!(headers.contains("authorization: bearer fixture-catalog-key"));
        let body = json!({"data":[{"id":"z-model"},{"id":"a-model"}]}).to_string();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )?;
        Ok(())
    });
    let directory = tempfile::tempdir()?;
    let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child.stdin.take().unwrap().write_all(
        format!("4\nhttp://{address}/v1\n1\nfixture-catalog-key\n2\n2\n/quit\n").as_bytes(),
    )?;
    let output = child.wait_with_output()?;
    server.join().unwrap()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("a-model") && text.contains("z-model"));
    assert!(!text.contains("fixture-catalog-key"));
    let saved = fs::read_to_string(directory.path().join(".arun/profile.json"))?;
    assert!(!saved.contains("fixture-catalog-key"));
    let saved: Value = serde_json::from_str(&saved)?;
    assert_eq!(saved["model"], "z-model");
    Ok(())
}
