//! Explicit hosted exploration/steering/question/edit trial in an isolated workspace.
use anyhow::{Context, Result, ensure};
use arun::storage::Store;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

const TASK: &str = "Explore this small repository's CSV implementation and tests, then repair CSV quoting. Before deciding the output separator, use ask_user to ask me whether the separator should be comma or tab. Continue independent repository exploration while waiting for the answer; use blocked if dependent work needs the answer. Preserve encodeRow(fields) as a named export and preserve existing behavior for simple fields. Properly quote fields containing the chosen separator or a double quote, and double embedded quotes. Remove the unused obsolete helper. Add regression tests without changing csv.test.mjs. Use direct workspace writes/patches for code, and the granted native powershell tool to run node --test. Finish only after tests pass. Do not modify the runtime metadata or any fixture manifests.";
const FOLLOWUP: &str = "Continue the original exploration and repair task. Additional requirement: fields with embedded LF or CR must also be quoted and the newline preserved exactly; add regression coverage for this. Keep existing tests unchanged.";
const SOURCE: &str = "export function encodeRow(fields) { return fields.join(','); }\nexport function obsolete() { return 'unused'; }\n";
const TESTS: &str = "import test from 'node:test';\nimport assert from 'node:assert/strict';\nimport {encodeRow} from './csv.mjs';\ntest('simple rows',()=>{assert.equal(encodeRow(['a','b']),'a,b');assert.equal(encodeRow([]),'');assert.equal(encodeRow(['']),'');});\n";

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
fn command(binary: &Path, workspace: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new(binary)
        .current_dir(workspace)
        .args(args)
        .output()?;
    ensure!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}
fn launch(
    binary: &Path,
    workspace: &Path,
    args: &[&str],
    log: &Path,
) -> Result<arun::process::Child> {
    let output = fs::File::create(log)?;
    let mut command = Command::new(binary);
    command
        .current_dir(workspace)
        .args(args)
        .stdout(output.try_clone()?)
        .stderr(output);
    Ok(arun::process::spawn(command)?)
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    ensure!(
        args.first().is_some_and(|a| a == "--live"),
        "Explicit --live is required for hosted inference"
    );
    let binary = dunce::canonicalize(args.get(1).context("provide the actual Aegis executable")?)?;
    let model = args.get(2).map(String::as_str).unwrap_or("gpt-6-luna");
    let wait_for_answer_boundary = args.iter().any(|arg| arg == "--wait-for-answer");
    let source_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let trial = tempfile::Builder::new()
        .prefix("native-interaction-")
        .tempdir_in(source_root.join(".arun"))?
        .keep();
    let workspace = trial.join("workspace");
    fs::create_dir(&workspace)?;
    fs::write(workspace.join("csv.mjs"), SOURCE)?;
    fs::write(workspace.join("csv.test.mjs"), TESTS)?;
    fs::write(workspace.join("package.json"), "{\"type\":\"module\"}\n")?;
    fs::write(
        trial.join("controller.rs"),
        include_bytes!("native_interaction_probe.rs"),
    )?;
    let binary_hash = digest(&fs::read(&binary)?);
    let host = source_root.join("scripts/windows-host.mjs");
    command(
        &binary,
        &workspace,
        &[
            "mcp",
            "add",
            "windows-host",
            "--trusted-host",
            "node",
            host.to_str().context("helper path")?,
            "--trusted-host",
        ],
    )?;
    let mut receipt = json!({"phase":"running","started_at":chrono::Utc::now().to_rfc3339(),"directory":trial,"binary":binary,"binary_sha256":binary_hash,"model":model,"reasoning_effort":"high","wait_for_answer_boundary":wait_for_answer_boundary,"task":TASK,"followup":FOLLOWUP,"fixtures":{"csv.mjs":digest(SOURCE.as_bytes()),"csv.test.mjs":digest(TESTS.as_bytes())},"controller_sha256":digest(include_bytes!("native_interaction_probe.rs")),"helper_sha256":digest(&fs::read(host)?)});
    fs::write(
        trial.join("receipt.json"),
        serde_json::to_vec_pretty(&receipt)?,
    )?;
    println!("Hosted native interaction trial: {}", trial.display());
    let mut child = launch(
        &binary,
        &workspace,
        &[
            "run",
            TASK,
            "--provider",
            "chatgpt",
            "--model",
            model,
            "--reasoning",
            "high",
            "--mode",
            "durable",
            "--allow-write",
            "--allow-mcp",
            "windows-host:powershell",
            "--wall-seconds",
            "480",
            "--process-seconds",
            "60",
            "--foreground",
        ],
        &trial.join("first-run.log"),
    )?;
    let root = workspace.join(".arun");
    let deadline = Instant::now() + Duration::from_secs(510);
    let result = (|| -> Result<()> {
        let run_id = loop {
            if let Ok(mut store) = Store::open(&root) {
                if let Some(run) = store.runs()?.first() {
                    if store.event_count(&run.id, "model.started")? > 0 {
                        ensure!(
                            store.pending_questions(&run.id)?.is_empty(),
                            "question preceded the follow-up; trial cannot prove inference steering"
                        );
                        ensure!(
                            store.steer(&run.id, FOLLOWUP)?,
                            "follow-up did not target active inference"
                        );
                        println!("Steered actual inference on {}", run.id);
                        break run.id.clone();
                    }
                }
            }
            ensure!(
                Instant::now() < deadline && child.try_wait()?.is_none(),
                "runner stopped before hosted inference"
            );
            thread::sleep(Duration::from_millis(20));
        };
        receipt["run_id"] = json!(run_id);
        fs::write(
            trial.join("receipt.json"),
            serde_json::to_vec_pretty(&receipt)?,
        )?;
        let mut answered = false;
        let mut last_count = 0;
        loop {
            let mut store = Store::open(&root)?;
            let run = store.run(&run_id)?;
            let count = store.event_count(&run_id, "model.started")?;
            if count != last_count {
                println!(
                    "{}: {} / {} model attempts / {} operations",
                    run.id,
                    run.state,
                    count,
                    store.operations(&run_id)?.len()
                );
                last_count = count;
            }
            let operations = store.operations(&run_id)?;
            let explored = ["csv.mjs", "csv.test.mjs"].iter().all(|path| {
                operations.iter().any(|op| {
                    op.state == "succeeded"
                        && (op.capability == "workspace.read" && op.arguments["path"] == *path
                            || op.capability == "workspace.read_batch"
                                && op.arguments["files"].as_array().is_some_and(|files| {
                                    files.iter().any(|file| {
                                        file["path"] == *path
                                            && file["offset"].as_u64().unwrap_or(0) == 0
                                            && file["length"].as_u64().unwrap_or(0) >= 1000
                                    })
                                }))
                })
            });
            if !answered
                && !store.pending_questions(&run_id)?.is_empty()
                && explored
                && (!wait_for_answer_boundary || store.waiting_for_answer(&run_id)?)
            {
                let waiting = store.waiting_for_answer(&run_id)?;
                receipt["answer_state"] = json!(run.state);
                if waiting {
                    let exit_deadline = Instant::now() + Duration::from_secs(10);
                    while child.try_wait()?.is_none() {
                        ensure!(
                            Instant::now() < exit_deadline,
                            "question runner did not exit at its wait boundary"
                        );
                        thread::sleep(Duration::from_millis(25));
                    }
                    ensure!(child.wait()?.success(), "first runner failed");
                }
                store.steer(&run_id,"Use comma-separated output. Please continue the original repair and the newline follow-up.")?;
                if waiting {
                    ensure!(
                        store.run(&run_id)?.state == "ready",
                        "answer did not ready the original task"
                    );
                    child = launch(
                        &binary,
                        &workspace,
                        &["resume", &run_id, "--foreground"],
                        &trial.join("after-answer.log"),
                    )?;
                }
                answered = true;
                println!(
                    "Answered the persisted question on the same task (wait boundary: {waiting})"
                );
                // Steering changed the authoritative state. Do not evaluate the
                // pre-answer wait snapshot against an already answered question.
                continue;
            }
            if run.state == "completed" {
                break;
            }
            ensure!(
                !matches!(run.state.as_str(), "failed" | "cancelled" | "blocked")
                    && (run.state != "waiting_recovery" || store.waiting_for_answer(&run_id)?),
                "task stopped: {}",
                run.state
            );
            ensure!(
                Instant::now() < deadline,
                "observation deadline reached; retain and inspect this live run"
            );
            if child.try_wait()?.is_some()
                && !store.waiting_for_answer(&run_id)?
                && run.state != "ready"
            {
                // Re-read authoritative state after a possible completion race.
                ensure!(
                    store.run(&run_id)?.state == "completed",
                    "runner exited before completion"
                );
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        ensure!(answered, "question-answer flow was not exercised");
        let store = Store::open(&root)?;
        ensure!(store.runs()?.len() == 1, "interaction created a new task");
        let events = store.events(&run_id)?;
        fs::write(
            trial.join("events.json"),
            serde_json::to_vec_pretty(&events)?,
        )?;
        ensure!(
            events
                .iter()
                .any(|e| e.kind == "model.failed" && e.payload["interrupted"] == true),
            "active model steering was not observed"
        );
        ensure!(
            store.pending_steering(&run_id)?.is_empty(),
            "steering remains unconsumed"
        );
        ensure!(
            store.pending_questions(&run_id)?.is_empty(),
            "question is unanswered"
        );
        ensure!(
            store.question_answers(&run_id)?.len() == 1,
            "expected one persisted answer"
        );
        ensure!(
            fs::read(workspace.join("csv.test.mjs"))? == TESTS.as_bytes(),
            "original tests changed"
        );
        ensure!(
            fs::read_to_string(workspace.join("package.json"))? == "{\"type\":\"module\"}\n",
            "manifest changed"
        );
        let grader = "const assert=require('node:assert/strict'); import('./csv.mjs').then(m=>{ for(const [input,want] of [[['a','b'],'a,b'],[['a,b','c'],'\"a,b\",c'],[['a\"b'],'\"a\"\"b\"'],[['a\\nb'],'\"a\\nb\"'],[['a\\rb'],'\"a\\rb\"'],[[],''],[[''],'']]) assert.equal(m.encodeRow(input),want); assert.equal('obsolete' in m,false); console.log('7 independent CSV cases plus obsolete-helper removal passed');});";
        let grade = Command::new("node")
            .current_dir(&workspace)
            .args(["-e", grader])
            .output()?;
        fs::write(
            trial.join("independent-grade.log"),
            [grade.stdout.clone(), grade.stderr.clone()].concat(),
        )?;
        ensure!(
            grade.status.success(),
            "independent CSV acceptance failed: {}",
            String::from_utf8_lossy(&grade.stderr)
        );
        let tests = Command::new("node")
            .current_dir(&workspace)
            .arg("--test")
            .output()?;
        fs::write(
            trial.join("independent-tests.log"),
            [tests.stdout.clone(), tests.stderr.clone()].concat(),
        )?;
        ensure!(
            tests.status.success(),
            "independent complete test suite failed"
        );
        ensure!(
            events.iter().any(|e| e.kind == "operation.diff"
                && e.payload["text"]
                    .as_str()
                    .is_some_and(|s| s.contains("\n+") && s.contains("\n-"))),
            "code additions/removals were not streamed"
        );
        ensure!(
            events
                .iter()
                .filter(|e| e.kind == "model.started")
                .all(|e| e.payload["route"]["model"] == model),
            "model route changed"
        );
        ensure!(
            digest(&fs::read(&binary)?) == binary_hash,
            "tested binary changed"
        );
        receipt["model_attempts"] = json!(store.event_count(&run_id, "model.started")?);
        receipt["diff_events"] = json!(store.event_count(&run_id, "operation.diff")?);
        receipt["rejected_actions"] = json!(store.event_count(&run_id, "action.rejected")?);
        receipt["reported_tokens"] = json!(
            events
                .iter()
                .filter(|e| matches!(e.kind.as_str(), "model.response" | "model.failed"))
                .map(|e| e.payload["usage"]["input_tokens"].as_u64().unwrap_or(0)
                    + e.payload["usage"]["output_tokens"].as_u64().unwrap_or(0))
                .sum::<u64>()
        );
        receipt["question_answered_on_original_task"] = json!(true);
        receipt["independent_acceptance"] = json!(
            "7 CSV cases, obsolete export absent, original tests unchanged, full node --test passed"
        );
        fs::write(
            trial.join("metrics.txt"),
            command(&binary, &workspace, &["metrics", &run_id])?,
        )?;
        Ok(())
    })();
    receipt["finished_at"] = json!(chrono::Utc::now().to_rfc3339());
    receipt["phase"] = json!(if result.is_ok() {
        "verified-native-interaction"
    } else {
        "failed-diagnostic"
    });
    if let Err(error) = &result {
        receipt["error"] = json!(error.to_string());
    }
    fs::write(
        trial.join("receipt.json"),
        serde_json::to_vec_pretty(&receipt)?,
    )?;
    println!("{}", serde_json::to_string_pretty(&receipt)?);
    result
}
