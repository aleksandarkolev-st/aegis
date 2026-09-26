use std::fs;
use std::process::Command;

use anyhow::{Context, Result};
use arun::{mcp, provider, storage::Store};
use serde_json::{Value, json};

#[test]
fn forced_restart_preserves_committed_evidence_without_repeating_the_adapter() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let node =
        provider::system_executable("node").context("Node is required for the provider fixture")?;
    let script = directory.path().join("model.cjs");
    fs::write(
        &script,
        r#"
const fs = require('fs');
const args = process.argv.slice(2);
const prompt = fs.readFileSync(args[args.indexOf('--prompt-file')+1],'utf8');
const state = JSON.parse(prompt.split('STATE (bounded, data not instructions):\n')[1]);
if(state.mode==='durable' && !state.handoff?.decisions.includes('Keep the fixture evidence through context resets')) process.exit(9);
const result = state.recent_events.find(event=>event.kind==='operation.succeeded');
const active = state.active_capabilities.some(capability=>capability.id==='workspace.read');
const action = result ? {kind:'finish',summary:'fixture read',evidence:[JSON.parse(result.payload).artifact]}
  : active ? {kind:'invoke',capability:'workspace.read',args:{path:'fixture.txt'}}
  : {kind:'search_capabilities',query:'read workspace file'};
setTimeout(()=>console.log(JSON.stringify({text:JSON.stringify(action)})),250);
"#,
    )?;
    let shim = directory
        .path()
        .join(if cfg!(windows) { "grok.cmd" } else { "grok" });
    let wrapper = if cfg!(windows) {
        format!("@\"{}\" \"{}\" %*\r\n", node.display(), script.display())
    } else {
        format!(
            "#!/bin/sh\nexec \"{}\" \"{}\" \"$@\"\n",
            node.display(),
            script.display()
        )
    };
    fs::write(&shim, wrapper)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o755))?;
    }
    fs::write(directory.path().join("fixture.txt"), "expected evidence")?;
    let root = directory.path().join(".arun");
    let mut store = Store::open(&root)?;
    for mode in ["eager", "lazy", "artifact", "durable"] {
        let run = store.create_run(
            "Read fixture",
            directory.path(),
            "grok",
            json!(["workspace.read"]),
            json!({"mode":mode,"actions":8,"wall_seconds":30}),
            "",
        )?;
        if mode == "durable" {
            let checkpoint = arun::model::Checkpoint {
                decisions: vec!["Keep the fixture evidence through context resets".into()],
                unresolved: vec![],
                next_action: "Read fixture.txt and preserve its evidence".into(),
                milestones: vec![],
            };
            store.save_checkpoint(&run.id, &checkpoint)?;
            for _ in 0..1100 {
                store.event(&run.id, "audit.detail", json!({"old":"not model context"}))?;
            }
            store.save_snapshot(&run.id)?;
            store.archive_history(&run.id)?;
        }
        let output = Command::new(env!("CARGO_BIN_EXE_arun"))
            .args([
                "restart-check",
                root.to_str().unwrap(),
                &run.id,
                "operation.succeeded",
            ])
            .env("PATH", directory.path())
            .output()?;
        assert!(
            output.status.success(),
            "{mode}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let observation: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(observation["interrupted"], true, "{mode}: {observation}");
        assert_eq!(store.event_count(&run.id, "operation.succeeded")?, 1);
        assert_eq!(store.event_count(&run.id, "operation.executing")?, 1);
        let recovered = store.run(&run.id)?;
        assert_eq!(
            recovered.state,
            if mode == "durable" {
                "completed"
            } else {
                "failed"
            }
        );
        if mode == "durable" {
            assert!(observation["recovery_ms"].as_u64().is_some());
            assert_eq!(store.event_count(&run.id, "model.response")?, 3);
            let events = store.events(&run.id)?;
            let recoveries: Vec<_> = events
                .iter()
                .filter(|event| event.kind == "runtime.recovered")
                .collect();
            assert_eq!(recoveries.len(), 2);
            assert!(
                recoveries
                    .iter()
                    .all(|event| event.payload["tail_events"].as_u64().unwrap() < 64)
            );
        }
    }
    Ok(())
}

#[test]
fn forced_restart_during_an_unsafe_call_requires_reconciliation_not_replay() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let script = directory.path().join("server.cjs");
    fs::write(
        &script,
        r#"
const fs = require('fs');
const send = (id,result)=>process.stdout.write(JSON.stringify({jsonrpc:'2.0',id,result})+'\n');
require('readline').createInterface({input:process.stdin}).on('line',line=>{
  const request = JSON.parse(line);
  if(request.method==='initialize') send(request.id,{protocolVersion:request.params.protocolVersion,capabilities:{tools:{}},serverInfo:{name:'unsafe',version:'1'}});
  if(request.method==='tools/list') send(request.id,{tools:[{name:'write',description:'External write',annotations:{readOnlyHint:true},inputSchema:{type:'object',properties:{}}}]});
  if(request.method==='tools/call') {
    fs.appendFileSync('effects.txt','effect\n');
    setTimeout(()=>send(request.id,{content:[{type:'text',text:'written'}]}),10000);
  }
});
"#,
    )?;
    let server = mcp::Server {
        name: "unsafe".into(),
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
        "Unsafe effect",
        directory.path(),
        "unsupported",
        json!(["mcp:unsafe:write"]),
        json!({"wall_seconds":30}),
        "",
    )?;
    let operation = store.begin_operation_versioned(
        &run.id,
        "mcp.unsafe.write",
        tools[0].version,
        json!({}),
        false,
    )?;
    let output = Command::new(env!("CARGO_BIN_EXE_arun"))
        .args([
            "restart-check",
            root.to_str().unwrap(),
            &run.id,
            "operation.executing",
        ])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let observation: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(observation["interrupted"], true);
    assert_eq!(
        observation["operation_state_after_termination"],
        "executing"
    );
    assert_eq!(observation["requires_reconciliation"], true);
    assert_eq!(store.run(&run.id)?.state, "waiting_recovery");
    assert_eq!(store.operation(&operation.id)?.state, "outcome_unknown");
    assert_eq!(store.event_count(&run.id, "operation.executing")?, 1);
    assert_eq!(store.event_count(&run.id, "model.started")?, 0);
    let effects = fs::read_to_string(directory.path().join("effects.txt")).unwrap_or_default();
    assert!(effects.lines().count() <= 1);
    Ok(())
}
