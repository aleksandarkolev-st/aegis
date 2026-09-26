use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use arun::storage::Store;

#[test]
fn native_capture_limits_stop_stdout_stderr_and_reply_before_parsing_actions() -> Result<()> {
    let node = arun::provider::system_executable("node")
        .context("Node is required for the provider capture fixture")?;
    for channel in ["stdout", "stderr", "combined", "reply", "fast"] {
        let directory = tempfile::tempdir()?;
        let fixture = directory.path().join("oversized.cjs");
        let script = match channel {
            "stdout" | "fast" => "process.stdout.write('x'.repeat(2048));",
            "stderr" => "process.stderr.write('x'.repeat(2048));",
            "combined" => {
                "process.stdout.write('x'.repeat(700)); process.stderr.write('x'.repeat(700));"
            }
            _ => {
                "const args = process.argv.slice(2); require('fs').writeFileSync(args[args.indexOf('-o')+1], 'x'.repeat(2048));"
            }
        };
        fs::write(
            &fixture,
            if channel == "fast" {
                script.to_owned()
            } else {
                format!("{script}\nsetInterval(() => {{}}, 1000);")
            },
        )?;
        let provider = if channel == "reply" {
            "codex"
        } else {
            "claude"
        };
        let wrapper = if cfg!(windows) {
            format!("@\"{}\" \"{}\" %*\r\n", node.display(), fixture.display())
        } else {
            format!(
                "#!/bin/sh\nexec \"{}\" \"{}\" \"$@\"\n",
                node.display(),
                fixture.display()
            )
        };
        let cli = directory.path().join(format!(
            "{provider}{}",
            if cfg!(windows) { ".cmd" } else { "" }
        ));
        fs::write(&cli, wrapper)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(cli, fs::Permissions::from_mode(0o755))?;
        }
        let started = Instant::now();
        let output = Command::new(env!("CARGO_BIN_EXE_arun"))
            .current_dir(directory.path())
            .env("PATH", directory.path())
            .args([
                "run",
                "Capture fixture",
                "--provider",
                provider,
                "--model-response-bytes",
                "1024",
                "--foreground",
            ])
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "capture did not stop promptly: {channel}"
        );
        let store = Store::open(&directory.path().join(".arun"))?;
        let run = &store.runs()?[0];
        assert_eq!(run.budgets["model_response_bytes"], 1024);
        assert_eq!(run.state, "waiting_recovery");
        assert_eq!(store.event_count(&run.id, "model.response")?, 0);
        assert!(store.operations(&run.id)?.is_empty());
        assert!(store.events(&run.id)?.iter().any(|event| {
            event.kind == "model.failed"
                && event.payload["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("response capture limit"))
        }));
    }
    Ok(())
}

#[test]
fn custom_response_budget_rejects_advertised_and_chunked_oversized_bodies() -> Result<()> {
    for chunked in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let server = std::thread::spawn(move || -> Result<()> {
            let (mut stream, _) = listener.accept()?;
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            let mut bytes = Vec::new();
            let mut buffer = [0; 4096];
            while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = stream.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..count]);
            }
            if chunked {
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n800\r\n{}\r\n0\r\n\r\n",
                    "x".repeat(2048)
                )?;
            } else {
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: 2048\r\nConnection: close\r\n\r\n"
                )?;
            }
            Ok(())
        });
        let endpoint = arun::endpoint::Endpoint {
            base_url: format!("http://{address}/v1"),
            api_key_env: None,
            response_format: arun::endpoint::ResponseFormat::None,
            allow_insecure: false,
        };
        let error = endpoint
            .call_bounded("fixture", "prompt", Duration::from_secs(5), || false, 1024)
            .unwrap_err();
        assert!(
            error.to_string().contains("configured byte limit"),
            "{error:#}"
        );
        server.join().unwrap()?;
    }
    Ok(())
}
