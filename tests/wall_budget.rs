use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use arun::storage::Store;

#[test]
fn in_flight_endpoint_call_respects_the_task_wall_budget() -> Result<()> {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("fixture.txt"), "value")?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    let server = thread::spawn(move || {
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(5) {
            if let Ok((mut stream, _)) = listener.accept() {
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = [0; 8192];
                let _ = stream.read(&mut request);
                thread::sleep(Duration::from_secs(3));
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                );
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
    });
    let started = Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .args([
            "run",
            "Read fixture.txt",
            "--provider",
            "custom",
            "--model",
            "fixture",
            "--endpoint",
            &format!("http://{address}/v1"),
            "--wall-seconds",
            "1",
            "--foreground",
        ])
        .output()?;
    let elapsed = started.elapsed();
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    let store = Store::open(&directory.path().join(".arun"))?;
    let run = store.runs()?.remove(0);
    assert_eq!(run.state, "waiting_recovery");
    assert_eq!(store.event_count(&run.id, "model.started")?, 1);
    assert_eq!(store.event_count(&run.id, "model.failed")?, 1);
    Ok(())
}
