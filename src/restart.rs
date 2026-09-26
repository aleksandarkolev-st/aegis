use std::fs::File;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::{process, storage::Store};

fn spawn(root: &Path, id: &str, phase: &str) -> Result<process::Child> {
    let output = File::create(root.join(format!("restart-{id}-{phase}.log")))?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args([
            "serve",
            root.to_str()
                .ok_or_else(|| anyhow::anyhow!("invalid state path"))?,
            id,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::from(output.try_clone()?))
        .stderr(Stdio::from(output));
    Ok(process::spawn(command)?)
}

pub fn execute(
    root: &Path,
    id: &str,
    cut: &str,
    wall_seconds: u64,
) -> Result<(Option<String>, Value)> {
    uuid::Uuid::parse_str(id)?;
    if ![
        "operation.executing",
        "operation.succeeded",
        "checkpoint.created",
    ]
    .contains(&cut)
    {
        bail!("unsupported restart boundary");
    }
    let started = Instant::now();
    let deadline = Duration::from_secs(wall_seconds.saturating_add(10));
    let mut child = spawn(root, id, "initial")?;
    let mut sequence = 0;
    let interrupted = loop {
        let store = Store::open(root)?;
        let mut boundary = None;
        for event in store.events_since(id, sequence)? {
            sequence = event.seq;
            if event.kind == cut {
                boundary = Some(event);
                break;
            }
        }
        if let Some(event) = boundary {
            child.kill()?;
            child.wait()?;
            break Some(event);
        }
        if let Some(status) = child.try_wait()? {
            return Ok((
                (!status.success()).then(|| format!("initial runner exited {status}")),
                json!({"requested":cut,"interrupted":false,"reason":"boundary not reached"}),
            ));
        }
        if started.elapsed() >= deadline {
            child.kill()?;
            child.wait()?;
            break None;
        }
        thread::sleep(Duration::from_millis(10));
    };
    drop(child);
    let mut store = Store::open(root)?;
    let operation_state = interrupted
        .as_ref()
        .and_then(|event| event.payload["id"].as_str())
        .map(|operation| store.operation(operation).map(|operation| operation.state))
        .transpose()?;
    store.event(id, "evaluation.interrupted", json!({"requested":cut,"boundary_seq":interrupted.as_ref().map(|event| event.seq),"boundary_payload":interrupted.as_ref().map(|event| &event.payload),"operation_state_after_termination":operation_state,"method":"terminated supervised process tree"}))?;
    let sequence = store
        .recent_events(id, 1)?
        .last()
        .map(|event| event.seq)
        .unwrap_or(0);
    if matches!(
        store.run(id)?.state.as_str(),
        "completed" | "failed" | "cancelled"
    ) {
        return Ok((
            None,
            json!({"requested":cut,"interrupted":interrupted.is_some(),"recovery_ms":null,"reason":"run became terminal before restart"}),
        ));
    }
    drop(store);
    let recovery_started = Instant::now();
    let mut child = spawn(root, id, "recovered")?;
    let mut recovery_ms = None;
    let error = loop {
        let store = Store::open(root)?;
        if recovery_ms.is_none()
            && store.events_since(id, sequence)?.iter().any(|event| {
                event.kind == "model.started"
                    || event.kind.starts_with("run.") && event.kind != "run.running"
            })
        {
            recovery_ms = Some(recovery_started.elapsed().as_millis());
        }
        if let Some(status) = child.try_wait()? {
            break (!status.success()).then(|| format!("recovered runner exited {status}"));
        }
        if started.elapsed() >= deadline {
            child.kill()?;
            child.wait()?;
            break Some("supervisor deadline exceeded".into());
        }
        thread::sleep(Duration::from_millis(10));
    };
    let store = Store::open(root)?;
    let state = store.run(id)?.state;
    Ok((
        error,
        json!({"requested":cut,"interrupted":interrupted.is_some(),"boundary_seq":interrupted.map(|event| event.seq),"operation_state_after_termination":operation_state,"recovery_ms":recovery_ms,"state_after_recovery":state,"requires_reconciliation":store.unknown_count(id)? > 0,"recovery_metric":"time to first post-restart model turn or terminal/recovery state; not total completion time"}),
    ))
}
