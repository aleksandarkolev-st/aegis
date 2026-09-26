#![cfg(windows)]

use std::fs;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

#[test]
fn forced_parent_termination_closes_the_owned_job() -> Result<()> {
    if let Some(directory) = std::env::var_os("AEGIS_TEST_CRASH_PARENT") {
        let directory = std::path::PathBuf::from(directory);
        let mut command = Command::new("node");
        command.args(["-e", "const fs=require('fs');fs.writeFileSync(process.argv[1],String(process.pid));let count=0;setInterval(()=>fs.writeFileSync(process.argv[2],String(++count)),25)"])
            .arg(directory.join("pid"))
            .arg(directory.join("heartbeat"))
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        let _child = arun::process::spawn(command)?;
        loop {
            thread::sleep(Duration::from_secs(1));
        }
    }
    let directory = tempfile::tempdir()?;
    let mut parent = Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "forced_parent_termination_closes_the_owned_job",
            "--nocapture",
        ])
        .env("AEGIS_TEST_CRASH_PARENT", directory.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?;
    let heartbeat = directory.path().join("heartbeat");
    let started = Instant::now();
    while !heartbeat.exists() && started.elapsed() < Duration::from_secs(5) {
        thread::sleep(Duration::from_millis(25));
    }
    parent.kill()?;
    parent.wait()?;
    thread::sleep(Duration::from_millis(150));
    let stopped = fs::read_to_string(&heartbeat)?;
    thread::sleep(Duration::from_millis(250));
    let current = fs::read_to_string(&heartbeat)?;
    if current != stopped {
        let pid: u32 = fs::read_to_string(directory.path().join("pid"))?.parse()?;
        Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status()?;
        bail!("managed child survived forced parent termination");
    }
    Ok(())
}
