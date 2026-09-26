use std::fs;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

#[test]
fn kill_and_drop_stop_descendants_not_only_the_launcher() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let fixture = directory.path().join("tree.cjs");
    fs::write(
        &fixture,
        r#"
const fs = require('fs');
const cp = require('child_process');
if (process.argv[2] === 'child') {
  let counter = 0;
  setInterval(() => fs.writeFileSync(process.argv[3], String(++counter)), 25);
} else {
  cp.spawn(process.execPath, [__filename, 'child', process.argv[3]], {stdio:'ignore'});
  setInterval(() => {}, 1000);
}
"#,
    )?;
    for explicit_kill in [false, true] {
        let heartbeat = directory.path().join(format!("heartbeat-{explicit_kill}"));
        let mut command = Command::new("node");
        command
            .arg(&fixture)
            .arg("parent")
            .arg(&heartbeat)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = arun::process::spawn(command)?;
        let started = Instant::now();
        while !heartbeat.exists() {
            if started.elapsed() > Duration::from_secs(5) {
                bail!("descendant did not start");
            }
            thread::sleep(Duration::from_millis(25));
        }
        assert!(child.try_wait()?.is_none());
        if explicit_kill {
            child.kill()?;
            child.wait()?;
        }
        drop(child);
        thread::sleep(Duration::from_millis(100));
        let stopped = fs::read_to_string(&heartbeat).context("heartbeat missing")?;
        thread::sleep(Duration::from_millis(200));
        assert_eq!(fs::read_to_string(&heartbeat)?, stopped);
    }
    Ok(())
}
