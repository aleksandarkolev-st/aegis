use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use arun::{interrupt::Scope, mcp, process, provider, storage::Store};
use serde_json::json;

fn wait_until(mut condition: impl FnMut() -> Result<bool>) -> Result<()> {
    let started = Instant::now();
    while !condition()? {
        if started.elapsed() > Duration::from_secs(5) {
            bail!("interrupt fixture timed out");
        }
        thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

fn runner(root: &Path, id: &str, key: bool) -> Result<process::Child> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_arun"));
    command
        .args(["serve", root.to_str().unwrap(), id])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if key {
        command.env("AEGIS_INTERRUPT_FIXTURE_KEY", "fixture");
    } else {
        command.env_remove("AEGIS_INTERRUPT_FIXTURE_KEY");
    }
    Ok(process::spawn(command)?)
}

#[test]
fn interrupting_http_model_work_pauses_one_turn_without_cancelling_or_poisoning_the_task()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    let accepted = Arc::new(AtomicBool::new(false));
    let received = accepted.clone();
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
                received.store(true, Ordering::SeqCst);
                thread::sleep(Duration::from_secs(3));
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                );
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
    });
    let mut store = Store::open(&root)?;
    let run = store.create_run("Slow decision", directory.path(), "custom", json!([]),
        json!({"model":"fixture","wall_seconds":30,"endpoint":{"base_url":format!("http://{address}/v1"),"api_key_env":"AEGIS_INTERRUPT_FIXTURE_KEY","response_format":"schema","allow_insecure":false}}), "")?;
    let mut child = runner(&root, &run.id, true)?;
    wait_until(|| Ok(accepted.load(Ordering::SeqCst)))?;
    let target = store
        .request_interrupt(&run.id, Scope::Model)?
        .context("model turn should be active")?;
    wait_until(|| Ok(child.try_wait()?.is_some()))?;
    assert_eq!(store.run(&run.id)?.state, "waiting_recovery");
    assert!(!store.interrupt_requested(&run.id, Scope::Model, &target)?);
    let events = store.events(&run.id)?;
    assert!(
        events
            .iter()
            .any(|event| event.kind == "model.failed" && event.payload["interrupted"] == true)
    );
    assert_eq!(store.event_count(&run.id, "run.cancelled")?, 0);
    store.state(&run.id, "ready", json!({"source":"resume fixture"}))?;
    let mut resumed = runner(&root, &run.id, false)?;
    wait_until(|| Ok(resumed.try_wait()?.is_some()))?;
    let events = store.events(&run.id)?;
    let failure = events
        .iter()
        .rev()
        .find(|event| event.kind == "model.failed")
        .unwrap();
    assert_eq!(failure.payload["interrupted"], false);
    assert!(
        failure.payload["error"]
            .as_str()
            .unwrap()
            .contains("environment variable")
    );
    assert_eq!(store.event_count(&run.id, "model.started")?, 2);
    server.join().unwrap();
    Ok(())
}

#[test]
fn interrupting_an_effectful_operation_does_not_pretend_to_undo_or_repeat_it() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let script = directory.path().join("server.cjs");
    fs::write(
        &script,
        r#"
const fs = require('fs');
const send=(id,result)=>process.stdout.write(JSON.stringify({jsonrpc:'2.0',id,result})+'\n');
require('readline').createInterface({input:process.stdin}).on('line',line=>{
  const request=JSON.parse(line);
  if(request.method==='initialize') send(request.id,{protocolVersion:request.params.protocolVersion,capabilities:{tools:{}},serverInfo:{name:'effect',version:'1'}});
  if(request.method==='tools/list') send(request.id,{tools:[{name:'write',description:'External effect',inputSchema:{type:'object',properties:{}}}]});
  if(request.method==='tools/call') { fs.appendFileSync('effects.txt','effect\n'); setTimeout(()=>send(request.id,{content:[{type:'text',text:'done'}]}),10000); }
});
"#,
    )?;
    let server = mcp::Server {
        name: "effect".into(),
        command: provider::system_executable("node")
            .context("Node is required")?
            .to_string_lossy()
            .into_owned(),
        args: vec![script.to_string_lossy().into_owned()],
        policy: mcp::Policy {
            trusted_host: true,
            ..Default::default()
        },
    };
    let tools = mcp::discover(&server, directory.path())?;
    let mut store = Store::open(&root)?;
    store.register_mcp(&server, &tools)?;
    let run = store.create_run(
        "Effect",
        directory.path(),
        "unsupported",
        json!(["mcp:effect:write"]),
        json!({"wall_seconds":30}),
        "",
    )?;
    let operation = store.begin_operation_versioned(
        &run.id,
        "mcp.effect.write",
        tools[0].version,
        json!({}),
        false,
    )?;
    let mut child = runner(&root, &run.id, false)?;
    wait_until(|| Ok(directory.path().join("effects.txt").exists()))?;
    assert_eq!(
        store.request_interrupt(&run.id, Scope::Operation)?,
        Some(operation.id.clone())
    );
    wait_until(|| Ok(child.try_wait()?.is_some()))?;
    assert_eq!(store.run(&run.id)?.state, "waiting_recovery");
    assert_eq!(store.operation(&operation.id)?.state, "outcome_unknown");
    assert_eq!(store.event_count(&run.id, "run.cancelled")?, 0);
    assert!(!store.interrupt_requested(&run.id, Scope::Operation, &operation.id)?);
    let mut resumed = runner(&root, &run.id, false)?;
    wait_until(|| Ok(resumed.try_wait()?.is_some()))?;
    assert_eq!(
        fs::read_to_string(directory.path().join("effects.txt"))?,
        "effect\n"
    );
    assert_eq!(store.event_count(&run.id, "operation.executing")?, 1);
    assert_eq!(store.event_count(&run.id, "model.started")?, 0);
    Ok(())
}
