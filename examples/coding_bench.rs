use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use arun::{process, storage::Store, trace, worker};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const GRADER: &str = include_str!("../benchmarks/coding/grade.mjs");
const TASKS: &[(&str, &str, &str)] = &[
    (
        "json-patch",
        include_str!("../benchmarks/coding/json-patch.mjs"),
        include_str!("../benchmarks/coding/json-patch.txt"),
    ),
    (
        "dag-scheduler",
        include_str!("../benchmarks/coding/dag-scheduler.mjs"),
        include_str!("../benchmarks/coding/dag-scheduler.txt"),
    ),
    (
        "sse-decoder",
        include_str!("../benchmarks/coding/sse-decoder.mjs"),
        include_str!("../benchmarks/coding/sse-decoder.txt"),
    ),
    (
        "interval-overlay",
        include_str!("../benchmarks/coding/interval-overlay.mjs"),
        include_str!("../benchmarks/coding/interval-overlay.txt"),
    ),
];

fn hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn reviewed_reference(path: &Path, attestation: &Value) -> Result<PathBuf> {
    let metadata =
        fs::symlink_metadata(path).context("Explicit Codex reference binary is unavailable")?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("Codex reference must be an explicitly reviewed regular native executable");
    }
    #[cfg(windows)]
    if !path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
    {
        bail!("Select the native Codex .exe, not an npm or shell shim");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            bail!("Codex reference binary is not executable");
        }
    }
    let path = dunce::canonicalize(path)?;
    if attestation["codex_binary_sha256"] != hash(&fs::read(&path)?) {
        bail!("Codex reference build does not match readiness record; no reference agent started");
    }
    Ok(path)
}

fn record(log: &mut File, event: Value) -> Result<()> {
    writeln!(log, "{event}")?;
    log.sync_all()?;
    Ok(())
}

fn capture(
    mut command: Command,
    directory: &Path,
    input: Option<&Path>,
    prefix: &Path,
    seconds: u64,
) -> Result<Value> {
    let stdout = prefix.with_extension("stdout.jsonl");
    let stderr = prefix.with_extension("stderr.txt");
    command
        .current_dir(directory)
        .stdin(match input {
            Some(input) => Stdio::from(File::open(input)?),
            None => Stdio::null(),
        })
        .stdout(Stdio::from(File::create(&stdout)?))
        .stderr(Stdio::from(File::create(&stderr)?));
    let mut child = process::spawn(command)?;
    let started = Instant::now();
    let mut stopped = None;
    let status = loop {
        if fs::metadata(&stdout)?
            .len()
            .saturating_add(fs::metadata(&stderr)?.len())
            > 32 * 1024 * 1024
        {
            stopped = Some("capture_limit");
        } else if started.elapsed() >= Duration::from_secs(seconds) {
            stopped = Some("deadline");
        }
        if stopped.is_some() {
            child.kill()?;
            break child.wait()?;
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    if fs::metadata(&stdout)?
        .len()
        .saturating_add(fs::metadata(&stderr)?.len())
        > 32 * 1024 * 1024
    {
        stopped = Some("capture_limit");
    }
    Ok(
        json!({"exit_code":status.code(),"stopped":stopped,"elapsed_ms":started.elapsed().as_millis(),"stdout":stdout,"stderr":stderr}),
    )
}

fn native_usage(text: &str, successful: bool) -> Value {
    let mut tokens = 0_u64;
    let mut turns = 0;
    let mut unknown = 0;
    let mut inputs=0_u64;
    let mut outputs=0_u64;
    let mut cached=0_u64;
    let mut measured=0_u64;
    let mut cached_measured=0_u64;
    for event in text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
    {
        if event["type"] == "turn.completed" || event["type"] == "turn.failed" {
            turns += usize::from(event["type"] == "turn.completed");
            match (
                event["usage"]["input_tokens"].as_u64(),
                event["usage"]["output_tokens"].as_u64(),
            ) {
                (Some(input), Some(output)) => {
                    tokens = tokens.saturating_add(input).saturating_add(output);
                    inputs=inputs.saturating_add(input);
                    outputs=outputs.saturating_add(output);
                    measured+=1;
                    if let Some(value)=event["usage"]["cached_input_tokens"].as_u64().filter(|value|*value<=input) {
                        cached=cached.saturating_add(value);
                        cached_measured+=1;
                    }
                }
                _ => unknown += 1,
            }
        }
    }
    json!({"recorded_model_tokens":tokens,"input_tokens":if measured>0{json!(inputs)}else{Value::Null},"output_tokens":if measured>0{json!(outputs)}else{Value::Null},"cached_input_tokens":if measured>0 && cached_measured==measured{json!(cached)}else{Value::Null},"model_turns":turns,"complete_usage":successful && turns > 0 && unknown == 0,"unaccounted_turns":unknown,"schema_tokens":null})
}

fn accepted_attempt(
    arm: &str,
    execution: &Value,
    accounting: &Value,
    grading: &Value,
    integrity: bool,
) -> bool {
    grading["passed"] == true
        && integrity
        && execution["exit_code"] == 0
        && execution["stopped"].is_null()
        && (arm != "aegis" || accounting["state"] == "completed")
}

fn read_capture(path: &Path) -> Result<String> {
    let mut text = String::new();
    File::open(path)?
        .take(32 * 1024 * 1024)
        .read_to_string(&mut text)?;
    Ok(text)
}

fn summarize(events: &[Value]) -> Value {
    let mut cases = std::collections::BTreeMap::new();
    for event in events {
        let key = format!(
            "{}:{}:{}",
            event["task"].as_str().unwrap_or("unknown"),
            event["repeat"],
            event["harness"].as_str().unwrap_or("unknown")
        );
        let entry = cases.entry(key).or_insert_with(|| json!({"task":event["task"],"repeat":event["repeat"],"harness":event["harness"],"started_attempts":0,"finished_attempts":0,"unaccounted_attempts":0,"accepted":false,"recorded_model_tokens":0,"recorded_elapsed_ms":0,"untimed_attempts":0,"latest_assertions":null}));
        if event["kind"] == "attempt.started" {
            entry["started_attempts"] = json!(entry["started_attempts"].as_u64().unwrap_or(0) + 1);
        }
        if event["kind"] == "attempt.finished" {
            if let Some(elapsed) = event["execution"]["elapsed_ms"].as_u64() {
                entry["recorded_elapsed_ms"] = json!(entry["recorded_elapsed_ms"].as_u64().unwrap_or(0).saturating_add(elapsed));
            } else {
                entry["untimed_attempts"] = json!(entry["untimed_attempts"].as_u64().unwrap_or(0) + 1);
            }
            // Report measured fixture accuracy separately from accepted task completion.
            entry["latest_assertions"] = match (event["grading"]["result"]["passed_cases"].as_u64(),event["grading"]["result"]["total_cases"].as_u64()) {
                (Some(passed),Some(total)) if total>0 && passed<=total && event["task_integrity"]==true => json!({"passed":passed,"total":total}),
                _ => Value::Null,
            };
            entry["finished_attempts"] =
                json!(entry["finished_attempts"].as_u64().unwrap_or(0) + 1);
            entry["recorded_model_tokens"] = json!(
                entry["recorded_model_tokens"]
                    .as_u64()
                    .unwrap_or(0)
                    .saturating_add(
                        event["accounting"]["recorded_model_tokens"]
                            .as_u64()
                            .unwrap_or(0)
                    )
            );
            if event["accounting"]["complete_usage"] != true {
                entry["unaccounted_attempts"] =
                    json!(entry["unaccounted_attempts"].as_u64().unwrap_or(0) + 1);
            }
            if event["accepted"] == true {
                entry["accepted"] = json!(true);
            }
        }
    }
    for entry in cases.values_mut() {
        let started = entry["started_attempts"].as_u64().unwrap_or(0);
        let finished = entry["finished_attempts"].as_u64().unwrap_or(0);
        entry["correction_rounds"] = json!(started.saturating_sub(1));
        entry["unfinished_attempts"] = json!(started.saturating_sub(finished));
        entry["tokens_per_accepted_task"] = if entry["accepted"] == true
            && started == finished
            && entry["unaccounted_attempts"] == 0
        {
            entry["recorded_model_tokens"].clone()
        } else {
            Value::Null
        };
        entry["ms_per_accepted_task"] = if entry["accepted"]==true && started==finished && entry["untimed_attempts"]==0 {entry["recorded_elapsed_ms"].clone()}else{Value::Null};
    }
    let mut arms = std::collections::BTreeMap::new();
    for entry in cases.values() {
        let arm = arms.entry(entry["harness"].as_str().unwrap_or("unknown").to_owned()).or_insert_with(||json!({"tasks":0,"accepted_tasks":0,"reported_tokens":0,"recorded_elapsed_ms":0,"incomplete_usage_tasks":0,"incomplete_timing_tasks":0,"graded_tasks":0,"passed_assertions":0,"total_assertions":0}));
        for (field, amount) in [
            ("tasks",1),("accepted_tasks",u64::from(entry["accepted"]==true)),
            ("reported_tokens",entry["recorded_model_tokens"].as_u64().unwrap_or(0)),
            ("recorded_elapsed_ms",entry["recorded_elapsed_ms"].as_u64().unwrap_or(0)),
            ("incomplete_usage_tasks",u64::from(entry["unfinished_attempts"]!=0 || entry["unaccounted_attempts"]!=0)),
            ("incomplete_timing_tasks",u64::from(entry["unfinished_attempts"]!=0 || entry["untimed_attempts"]!=0)),
            ("graded_tasks",u64::from(!entry["latest_assertions"].is_null())),
            ("passed_assertions",entry["latest_assertions"]["passed"].as_u64().unwrap_or(0)),
            ("total_assertions",entry["latest_assertions"]["total"].as_u64().unwrap_or(0)),
        ] { arm[field]=json!(arm[field].as_u64().unwrap_or(0).saturating_add(amount)); }
    }
    for arm in arms.values_mut() {
        let accepted=arm["accepted_tasks"].as_u64().unwrap_or(0);
        let total=arm["total_assertions"].as_u64().unwrap_or(0);
        arm["task_accuracy_percent"]=json!(100.0*accepted as f64/arm["tasks"].as_u64().unwrap() as f64);
        arm["measured_assertion_accuracy_percent"]=if total>0 {json!(100.0*arm["passed_assertions"].as_u64().unwrap() as f64/total as f64)}else{Value::Null};
        // Charge all unsuccessful tasks and correction attempts to the arm's successes.
        arm["tokens_per_accepted_task"]=if accepted>0 && arm["incomplete_usage_tasks"]==0 {json!(arm["reported_tokens"].as_u64().unwrap() as f64/accepted as f64)}else{Value::Null};
        arm["ms_per_accepted_task"]=if accepted>0 && arm["incomplete_timing_tasks"]==0 {json!(arm["recorded_elapsed_ms"].as_u64().unwrap() as f64/accepted as f64)}else{Value::Null};
    }
    json!({"cases":cases.values().collect::<Vec<_>>(),"harnesses":arms,"notes":"Accuracy describes this measured fixture sample, not general model confidence. Cumulative tokens and time include failed tasks and correction rounds. Unknown usage, timing or unfinished attempts prevents complete efficiency comparisons. Cached tokens are included in gross usage; no billed-cost estimate. Native schema exposure is unobservable, not zero."})
}

fn verify(workspace: &Path, state: &Path, task: &str, image: &str) -> Result<Value> {
    let script = format!(
        "const {{grade}} = await import('data:text/javascript,' + encodeURIComponent(process.argv.slice(1).join(''))); const result = await grade({}, await import('./index.mjs')); console.log(JSON.stringify(result)); if (!result.passed) process.exitCode = 1;",
        json!(task)
    );
    if script.len() + GRADER.len() > 24_000 {
        bail!("private grader exceeds portable command bound");
    }
    let mut arguments = vec!["--input-type=module".to_owned(), "-e".to_owned(), script];
    let mut chunk = String::new();
    for character in GRADER.chars() {
        if chunk.len() + character.len_utf8() > 4096 {
            arguments.push(std::mem::take(&mut chunk));
        }
        chunk.push(character);
    }
    if !chunk.is_empty() {
        arguments.push(chunk);
    }
    let mut store = Store::open(state)?;
    let run = store.create_run("Independent coding fixture grading",workspace,"verifier",json!(["process.run","process:node"]),json!({"container_image":image,"process_seconds":30,"wall_seconds":60,"command_scopes":{"commands":[{"program":"node","args":arguments}]}}),"Read-only independent assertion suite")?;
    store.state(&run.id, "running", json!({}))?;
    let operation = store.begin_operation(
        &run.id,
        "process.run",
        json!({"program":"node","args":arguments}),
        true,
    )?;
    store.operation_state(&operation, "dispatched", None, json!({}))?;
    match worker::execute(state, &operation.id) {
        Ok(result) => {
            let artifact = store.put_artifact(&serde_json::to_vec(&result)?)?;
            store.operation_state(
                &operation,
                "succeeded",
                Some(&artifact),
                json!({"exit_code":result["exit_code"]}),
            )?;
            let output = store.artifact(
                result["output_artifact"]
                    .as_str()
                    .context("grader output artifact missing")?,
            )?;
            let parsed: Option<Value> = String::from_utf8_lossy(&output)
                .lines()
                .find_map(|line| serde_json::from_str(line).ok());
            let passed = result["exit_code"] == 0
                && parsed.as_ref().is_some_and(|value| {
                    value["task"] == task
                        && value["passed"] == true
                        && value["total_cases"]
                            .as_u64()
                            .is_some_and(|count| count >= 5)
                });
            store.state(
                &run.id,
                if passed { "completed" } else { "failed" },
                json!({"reason":"external grading finished"}),
            )?;
            Ok(
                json!({"passed":passed,"run_id":run.id,"artifact":artifact,"result":parsed,"exit_code":result["exit_code"]}),
            )
        }
        Err(error) => {
            store.operation_state(
                &operation,
                "failed",
                None,
                json!({"error":error.to_string()}),
            )?;
            store.state(&run.id, "failed", json!({"reason":"grading unavailable"}))?;
            Ok(json!({"passed":false,"unavailable":true,"run_id":run.id,"error":error.to_string()}))
        }
    }
}

fn verify_native(workspace: &Path, records: &Path, task: &str, node: &Path) -> Result<Value> {
    fs::create_dir_all(records)?;
    let before = fs::read(workspace.join("index.mjs"))?;
    let script = format!(
        "const {{grade}} = await import('data:text/javascript,' + encodeURIComponent(process.argv.slice(1).join(''))); const result = await grade({}, await import('./index.mjs')); console.log(JSON.stringify(result)); if (!result.passed) process.exitCode = 1;",
        json!(task)
    );
    let mut command = Command::new(node);
    command.args(["--input-type=module", "-e", &script]);
    // Native Command argv preserves the grader without shell interpolation.
    let mut chunk = String::new();
    for character in GRADER.chars() {
        if chunk.len() + character.len_utf8() > 4096 {
            command.arg(std::mem::take(&mut chunk));
        }
        chunk.push(character);
    }
    if !chunk.is_empty() {
        command.arg(chunk);
    }
    let execution = capture(command, workspace, None, &records.join("grade"), 30)?;
    let output = read_capture(&records.join("grade.stdout.jsonl"))?;
    let parsed: Option<Value> = output
        .lines()
        .find_map(|line| serde_json::from_str(line).ok());
    let unchanged = fs::read(workspace.join("index.mjs"))? == before;
    let passed = unchanged
        && execution["exit_code"] == 0
        && execution["stopped"].is_null()
        && parsed.as_ref().is_some_and(|value| {
            value["task"] == task
                && value["passed"] == true
                && value["total_cases"]
                    .as_u64()
                    .is_some_and(|count| count >= 5)
        });
    let result = json!({"passed":passed,"result":parsed,"execution":execution,
        "source_unchanged":unchanged,"grader_sha256":hash(GRADER.as_bytes()),
        "execution_policy":"Native Windows Node, current-user privileges; not a container or read-only sandbox"});
    fs::write(
        records.join("receipt.json"),
        serde_json::to_vec_pretty(&result)?,
    )?;
    Ok(result)
}

fn register_windows_host(aegis: &Path, node: &Path, workspace: &Path) -> Result<()> {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/windows-host.mjs");
    let registration = process::background(&mut Command::new(aegis))
        .args(["mcp", "add", "windows-host", "--trusted-host"])
        .arg(node)
        .arg(script)
        .arg("--trusted-host")
        .current_dir(workspace)
        .output()?;
    if !registration.status.success() {
        bail!(
            "Windows host registration failed: {}",
            String::from_utf8_lossy(&registration.stderr)
        );
    }
    Ok(())
}

fn main() -> Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.first().is_some_and(|argument|argument=="--report") {
        ensure!(arguments.len()==2,"Usage: coding_bench --report <attempts.jsonl>");
        let events = fs::read_to_string(&arguments[1])?.lines().map(serde_json::from_str).collect::<std::result::Result<Vec<Value>,_>>()?;
        println!("{}",serde_json::to_string_pretty(&summarize(&events))?);
        return Ok(());
    }
    let mut run_agents = false;
    let mut preparation = false;
    let mut exploratory = false;
    let mut aegis_only = false;
    let mut windows_native = false;
    let mut node = None;
    let mut node_version = Value::Null;
    let mut ready = None;
    let mut aegis = None;
    let mut codex_reference = None;
    let mut model = "gpt-5.5".to_owned();
    let mut image = "node:22-alpine".to_owned();
    let mut repeats = 2_usize;
    let mut seconds = 600_u64;
    let mut reasoning = None;
    let mut selected_tasks: Option<Vec<String>> = None;
    let mut attempts = 3_usize;
    let mut index = 0;
    while index < arguments.len() {
        let flag = arguments[index].as_str();
        if flag == "--prepare-only" {
            preparation = true;
            index += 1;
            continue;
        }
        if flag == "--run" {
            run_agents = true;
            index += 1;
            continue;
        }
        if flag == "--exploratory" {
            exploratory = true;
            index += 1;
            continue;
        }
        if flag == "--aegis-only" {
            aegis_only = true;
            index += 1;
            continue;
        }
        if flag == "--windows-native" {
            windows_native = true;
            index += 1;
            continue;
        }
        index += 1;
        let value = arguments
            .get(index)
            .context("benchmark option needs a value")?;
        match flag {
            "--ready" => ready = Some(PathBuf::from(value)),
            "--aegis" => aegis = Some(dunce::canonicalize(value)?),
            "--codex-reference" => codex_reference = Some(PathBuf::from(value)),
            "--model" => model = value.clone(),
            "--node" => node = Some(dunce::canonicalize(value)?),
            "--image" => image = value.clone(),
            "--repeats" => repeats = value.parse()?,
            "--seconds" => seconds = value.parse()?,
            "--attempts" => attempts = value.parse()?,
            "--reasoning" => {
                if !arun::catalog::valid_effort(value) {
                    bail!("invalid reasoning effort");
                }
                reasoning = Some(value.clone());
            }
            "--tasks" => {
                let tasks: Vec<String> = value.split(',').map(str::to_owned).collect();
                if tasks.is_empty()
                    || tasks
                        .iter()
                        .any(|task| !TASKS.iter().any(|(name, _, _)| *name == task))
                {
                    bail!("select known coding tasks");
                }
                selected_tasks = Some(tasks);
            }
            _ => bail!("unknown benchmark option: {flag}"),
        }
        index += 1;
    }
    if (run_agents && preparation)
        || (exploratory && (!run_agents || ready.is_some()))
        || (aegis_only && (!exploratory || codex_reference.is_some()))
        || !(1..=3).contains(&attempts)
        || !(1..=3).contains(&repeats)
        || !(30..=1800).contains(&seconds)
        || model.is_empty()
        || model.len() > 128
        || !model
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._/".contains(&byte))
    {
        bail!("invalid bounded benchmark configuration");
    }
    if windows_native && !cfg!(windows) {
        bail!("--windows-native requires Windows");
    }
    if windows_native {
        let node = node
            .as_ref()
            .context("--windows-native requires an explicit --node executable")?;
        if !node.is_file()
            || !node
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
        {
            bail!("Select a regular native Node .exe");
        }
        let version = process::background(&mut Command::new(node))
            .arg("--version")
            .output()?;
        if !version.status.success() {
            bail!("Native Node version unavailable");
        }
        node_version = json!(String::from_utf8_lossy(&version.stdout).trim());
    } else if node.is_some() {
        bail!("--node requires --windows-native");
    }
    let binary = hash(&fs::read(std::env::current_exe()?)?);
    let plan = hash(include_bytes!("../plan.txt"));
    let mut attestation = Value::Null;
    let mut codex_version = Value::Null;
    if run_agents {
        if exploratory {
            let reference_hash = if aegis_only {
                Value::Null
            } else {
                json!(hash(&fs::read(codex_reference.as_ref().context(
                    "Exploratory comparison requires an explicit native --codex-reference"
                )?)?))
            };
            attestation = json!({"stage":"exploratory","production_readiness_verified":false,
                "aegis_binary_sha256":hash(&fs::read(aegis.as_ref().context("--aegis is required")?)?),
                "codex_binary_sha256":reference_hash,"pending":["Full production readiness audit"]});
        } else {
            let ready = ready.context("agent benchmarks stay gated; provide --ready only after implementation and functional verification")?;
            if fs::metadata(&ready)?.len() > 8192 {
                bail!("readiness record exceeds 8192 bytes");
            }
            attestation = serde_json::from_slice(&fs::read(ready)?)?;
            if attestation["implementation_complete"] != true
                || attestation["functional_verified"] != true
                || attestation["installed_ux_verified"] != true
                || attestation["benchmark_binary_sha256"] != binary
                || attestation["plan_sha256"] != plan
                || !attestation["pending"].as_array().is_some_and(Vec::is_empty)
            {
                bail!("readiness gate incomplete or binary/plan changed; no agent calls started");
            }
        }
        let aegis = aegis
            .as_ref()
            .context("--aegis must identify the verified native runtime")?;
        if attestation["aegis_binary_sha256"] != hash(&fs::read(aegis)?) {
            bail!("Aegis build does not match readiness record");
        }
        if !aegis_only {
            let reference = reviewed_reference(codex_reference.as_deref().context("--codex-reference must identify the reviewed external comparator; Aegis never resolves or installs it")?, &attestation)?;
            codex_reference = Some(reference);
        }
        if !windows_native {
            let resolved = process::background(&mut Command::new("docker"))
                .args(["image", "inspect", "--format", "{{.Id}}", &image])
                .output()?;
            if !resolved.status.success() {
                bail!("approved Node image is not locally available");
            }
            image = String::from_utf8(resolved.stdout)?.trim().to_owned();
            if image.len() != 71 || !image.starts_with("sha256:") {
                bail!("container image digest unavailable");
            }
        }
        if !aegis_only {
            let version = process::background(&mut Command::new(codex_reference.as_ref().unwrap()))
                .arg("--version")
                .output()?;
            if !version.status.success() {
                bail!("native Codex version unavailable");
            }
            codex_version = json!(String::from_utf8_lossy(&version.stdout).trim());
        }
    }
    let root = std::env::current_dir()?
        .join(".arun/coding-bench")
        .join(uuid::Uuid::new_v4().to_string());
    fs::create_dir_all(&root)?;
    let root = dunce::canonicalize(root)?;
    let node_hash = node
        .as_ref()
        .map(|path| fs::read(path).map(|bytes| hash(&bytes)))
        .transpose()?;
    let host_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts");
    let host_hashes = if windows_native {
        Some((
            hash(&fs::read(host_root.join("windows-host.mjs"))?),
            hash(&fs::read(host_root.join("windows-host.ps1"))?),
        ))
    } else {
        None
    };
    let manifest = json!({"kind":"public-handwritten-coding-fixtures-v1","exploratory":exploratory,"aegis_only":aegis_only,"windows_native":windows_native,"node":node,"node_sha256":node_hash,"node_version":node_version,"windows_host_script_sha256":if windows_native {json!(hash(&fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/windows-host.mjs"))?))}else{Value::Null},"tasks":selected_tasks,"reasoning_effort":reasoning,"benchmark_binary_sha256":binary,"plan_sha256":plan,"grader_sha256":hash(GRADER.as_bytes()),"model":model,"codex_version":codex_version,"codex_reference":codex_reference,"codex_binary_sha256":attestation["codex_binary_sha256"],"image":if windows_native {Value::Null}else{json!(image)},"repeats":repeats,"seconds_per_attempt":seconds,"max_attempts":attempts,"readiness":attestation,"prepare_only":!run_agents,"policy":if windows_native {"Both agents are native Windows processes using the same task and native Node grader. Codex retains user config/rules with automatic approval review and workspace-write isolation. Aegis uses direct HTTP and its explicitly granted trusted Windows PowerShell MCP tool. Native grading runs with current-user privileges and does not enforce read-only isolation. This is not identical tool isolation, a held-out dataset or a production attestation."}else{"Same task and independent grader. Explicitly reviewed external Codex uses its own workspace-write harness; Aegis uses direct HTTP, scoped file tools and exact containerized Node commands. This is not a native provider adapter, identical tool isolation, a held-out dataset, SWE-bench or an ARC score."}});
    fs::write(
        root.join("experiment.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    if let Some((script, helper)) = &host_hashes {
        fs::write(
            root.join("windows-host.json"),
            serde_json::to_vec_pretty(&json!({"script_sha256":script,"helper_sha256":helper}))?,
        )?;
    }
    fs::write(root.join("grader.mjs"), GRADER)?;
    let mut log = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(root.join("attempts.jsonl"))?;
    for repeat in 0..repeats {
        for (task, source, specification) in TASKS {
            if selected_tasks
                .as_ref()
                .is_some_and(|selected| !selected.iter().any(|name| name == task))
            {
                continue;
            }
            let arms = if repeat % 2 == 0 {
                ["aegis", "codex"]
            } else {
                ["codex", "aegis"]
            };
            for arm in arms {
                if aegis_only && arm != "aegis" {
                    continue;
                }
                let workspace = root.join(format!("{task}-{repeat}-{arm}"));
                fs::create_dir_all(&workspace)?;
                fs::write(workspace.join("index.mjs"), source)?;
                fs::write(workspace.join("TASK.txt"), specification)?;
                fs::write(
                    workspace.join("verify.mjs"),
                    "await import('./index.mjs');\n",
                )?;
                record(
                    &mut log,
                    json!({"kind":"case.prepared","task":task,"repeat":repeat,"harness":arm,"workspace":workspace,"source_sha256":hash(source.as_bytes()),"task_sha256":hash(specification.as_bytes())}),
                )?;
                if !run_agents {
                    continue;
                }
                let state = workspace.join(".arun");
                Store::open(&state)?.set_learning(&workspace, false)?;
                if windows_native && arm == "aegis" {
                    register_windows_host(
                        aegis.as_ref().unwrap(),
                        node.as_ref().unwrap(),
                        &workspace,
                    )?;
                }
                let command_scopes = workspace.join(".arun/commands.json");
                fs::write(
                    &command_scopes,
                    serde_json::to_vec(
                        &json!({"commands":[{"program":"node","args":["--check","index.mjs"]},{"program":"node","args":["verify.mjs"]}]}),
                    )?,
                )?;
                let file_scopes = workspace.join(".arun/files.json");
                fs::write(
                    &file_scopes,
                    serde_json::to_vec(&json!({"read":["**"],"write":["index.mjs","verify.mjs"]}))?,
                )?;
                let mut feedback = String::new();
                for attempt in 0..attempts {
                    if let Some((script, helper)) = &host_hashes {
                        if node_hash.as_ref() != Some(&hash(&fs::read(node.as_ref().unwrap())?))
                            || *script != hash(&fs::read(host_root.join("windows-host.mjs"))?)
                            || *helper != hash(&fs::read(host_root.join("windows-host.ps1"))?)
                        {
                            bail!(
                                "Native Windows execution dependencies changed; no attempt started"
                            );
                        }
                    }
                    let prefix = root.join(format!("{task}-{repeat}-{arm}-{attempt}"));
                    let native_guidance = if windows_native {
                        format!(
                            "\nRun native Windows commands. Node executable: {}. Use Node --check index.mjs and Node verify.mjs for local checks. Do not use Docker. In Aegis, Windows shell execution is mcp.windows-host.powershell; discover that capability when needed. Keep edits to index.mjs and verify.mjs; do not change TASK.txt.\n",
                            node.as_ref().unwrap().display()
                        )
                    } else {
                        String::new()
                    };
                    let prompt = format!("{specification}\n{native_guidance}{feedback}");
                    let prompt_file = prefix.with_extension("prompt.txt");
                    fs::write(&prompt_file, &prompt)?;
                    record(
                        &mut log,
                        json!({"kind":"attempt.started","task":task,"repeat":repeat,"harness":arm,"attempt":attempt,"created_at":arun::storage::unix_time(),"prompt_sha256":hash(prompt.as_bytes())}),
                    )?;
                    if arm == "aegis"
                        && attestation["aegis_binary_sha256"]
                            != hash(&fs::read(aegis.as_ref().unwrap())?)
                    {
                        bail!(
                            "Aegis binary changed after experiment preparation; no attempt started"
                        );
                    }
                    let mut command = Command::new(if arm == "codex" {
                        reviewed_reference(codex_reference.as_ref().unwrap(), &attestation)?
                    } else {
                        aegis.clone().unwrap()
                    });
                    if arm == "codex" {
                        if let Some(effort) = &reasoning {
                            command.args(["-c", &format!("model_reasoning_effort={effort:?}")]);
                        }
                        if windows_native {
                            command.args([
                                "exec",
                                "--approve-for-me",
                                "--ephemeral",
                                "--skip-git-repo-check",
                                "--json",
                                "-m",
                                &model,
                                "-",
                            ]);
                        } else {
                            command.args([
                                "exec",
                                "--ignore-user-config",
                                "--ignore-rules",
                                "--ephemeral",
                                "--skip-git-repo-check",
                                "--json",
                                "--sandbox",
                                "workspace-write",
                                "-c",
                                "approval_policy=\"never\"",
                                "-m",
                                &model,
                                "-",
                            ]);
                        }
                    } else {
                        command
                            .args([
                                "run",
                                &prompt,
                                "--provider",
                                "chatgpt",
                                "--model",
                                &model,
                                "--mode",
                                "durable",
                                "--allow-write",
                            ])
                            .args(["--wall-seconds", &seconds.to_string(), "--foreground"]);
                        if let Some(effort) = &reasoning {
                            command.args(["--reasoning", effort]);
                        }
                        if windows_native {
                            command.args(["--allow-mcp", "windows-host:powershell"]);
                        } else {
                            command
                                .args(["--image", &image, "--command-scopes"])
                                .arg(&command_scopes)
                                .arg("--filesystem-scopes")
                                .arg(&file_scopes);
                        }
                    }
                    let previous_runs = Store::open(&state)?
                        .runs()?
                        .into_iter()
                        .map(|run| run.id)
                        .collect::<Vec<_>>();
                    let execution = capture(
                        command,
                        &workspace,
                        Some(&prompt_file),
                        &prefix,
                        seconds.saturating_add(10),
                    )?;
                    let successful = execution["exit_code"] == 0 && execution["stopped"].is_null();
                    let accounting = if arm == "codex" {
                        native_usage(
                            &read_capture(&prefix.with_extension("stdout.jsonl"))?,
                            successful,
                        )
                    } else {
                        let store = Store::open(&state)?;
                        if let Some(run) = store
                            .runs()?
                            .into_iter()
                            .find(|run| !previous_runs.contains(&run.id))
                        {
                            let events = store.events(&run.id)?;
                            fs::write(
                                prefix.with_extension("events.json"),
                                serde_json::to_vec(&events)?,
                            )?;
                            let metrics = trace::metrics(&events);
                            json!({"run_id":run.id,"state":run.state,"recorded_model_tokens":metrics.model_tokens,"complete_usage":successful && run.state == "completed" && metrics.model_attempts > 0 && metrics.unaccounted_model_attempts == 0 && metrics.estimated_turns == 0,"observed":arun::run_metrics::report(&store,&run.id)?,"metrics":metrics})
                        } else {
                            json!({"complete_usage":false,"recorded_model_tokens":0,"error":"no runtime run was created"})
                        }
                    };
                    let grading = if windows_native {
                        if node_hash.as_ref() != Some(&hash(&fs::read(node.as_ref().unwrap())?)) {
                            bail!("Native Node changed during the experiment");
                        }
                        verify_native(
                            &workspace,
                            &root.join(format!("grade-{task}-{repeat}-{arm}-{attempt}")),
                            task,
                            node.as_ref().unwrap(),
                        )?
                    } else {
                        verify(
                            &workspace,
                            &root.join(format!("grade-{task}-{repeat}-{arm}-{attempt}")),
                            task,
                            &image,
                        )?
                    };
                    let integrity =
                        fs::read(workspace.join("TASK.txt"))? == specification.as_bytes();
                    let passed =
                        accepted_attempt(arm, &execution, &accounting, &grading, integrity);
                    record(
                        &mut log,
                        json!({"kind":"attempt.finished","task":task,"repeat":repeat,"harness":arm,"attempt":attempt,"finished_at":arun::storage::unix_time(),"execution":execution,"accounting":accounting,"grading":grading,"task_integrity":integrity,"accepted":passed,"solution_sha256":hash(&fs::read(workspace.join("index.mjs"))?)}),
                    )?;
                    println!("{task} repeat={repeat} {arm} attempt={attempt} accepted={passed}");
                    if passed || grading["unavailable"] == true {
                        break;
                    }
                    feedback = format!(
                        "Independent assertions failed. Repair the existing code; do not change TASK.txt. Feedback: {}",
                        grading["result"]
                            .to_string()
                            .chars()
                            .take(6000)
                            .collect::<String>()
                    );
                }
            }
        }
    }
    let events: Vec<Value> = fs::read_to_string(root.join("attempts.jsonl"))?
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?;
    fs::write(
        root.join("summary.json"),
        serde_json::to_vec_pretty(&summarize(&events))?,
    )?;
    println!("Records: {}", root.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn native_windows_graders_accept_references_and_reject_every_broken_starter() -> Result<()> {
        let resolved = Command::new("where.exe").arg("node.exe").output()?;
        let output = String::from_utf8(resolved.stdout)?;
        let node = dunce::canonicalize(
            output
                .lines()
                .next()
                .context("Node required for native fixture grading")?,
        )?;
        let directory = tempfile::tempdir()?;
        for (task, broken, _) in TASKS {
            let workspace = directory.path().join(task);
            fs::create_dir(&workspace)?;
            fs::write(workspace.join("index.mjs"), broken)?;
            let failed = verify_native(
                &workspace,
                &directory.path().join(format!("failed-{task}")),
                task,
                &node,
            )?;
            assert_eq!(failed["passed"], false, "{failed}");
            assert_eq!(failed["source_unchanged"], true);
            let reference = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("benchmarks/coding/references")
                .join(format!("{task}.mjs"));
            fs::write(workspace.join("index.mjs"), fs::read(reference)?)?;
            let passed = verify_native(
                &workspace,
                &directory.path().join(format!("passed-{task}")),
                task,
                &node,
            )?;
            assert_eq!(passed["passed"], true, "{passed}");
            assert_eq!(passed["source_unchanged"], true);
        }
        Ok(())
    }

    #[test]
    fn passing_code_and_zero_exit_do_not_hide_an_incomplete_aegis_task() {
        let execution = json!({"exit_code":0,"stopped":null});
        let grading = json!({"passed":true});
        for state in [
            "waiting_recovery",
            "paused",
            "running",
            "answered",
            "failed",
        ] {
            assert!(!accepted_attempt(
                "aegis",
                &execution,
                &json!({"state":state}),
                &grading,
                true
            ));
        }
        let completed = json!({"state":"completed"});
        assert!(accepted_attempt(
            "aegis", &execution, &completed, &grading, true
        ));
        assert!(!accepted_attempt(
            "aegis",
            &json!({"exit_code":0,"stopped":"deadline"}),
            &completed,
            &grading,
            true
        ));
        assert!(!accepted_attempt(
            "aegis", &execution, &completed, &grading, false
        ));
    }

    #[test]
    fn reference_requires_an_explicit_matching_native_build_without_execution() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join(if cfg!(windows) {
            "reference.exe"
        } else {
            "reference"
        });
        fs::write(&path, b"fixture-not-a-real-agent")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert!(
                reviewed_reference(
                    &path,
                    &json!({"codex_binary_sha256":hash(b"fixture-not-a-real-agent")})
                )
                .is_err()
            );
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
        }
        assert!(reviewed_reference(&path, &Value::Null).is_err());
        assert!(reviewed_reference(directory.path(), &Value::Null).is_err());
        let attestation = json!({"codex_binary_sha256":hash(b"fixture-not-a-real-agent")});
        assert_eq!(
            reviewed_reference(&path, &attestation)?,
            dunce::canonicalize(&path)?
        );
        fs::write(&path, b"changed-build")?;
        assert!(reviewed_reference(&path, &attestation).is_err());
        #[cfg(windows)]
        {
            let shim = directory.path().join("reference.cmd");
            fs::write(&shim, b"@echo unexpected-reference-started")?;
            assert!(
                reviewed_reference(
                    &shim,
                    &json!({"codex_binary_sha256":hash(b"@echo unexpected-reference-started")})
                )
                .is_err()
            );
        }
        Ok(())
    }

    #[test]
    #[ignore = "requires Docker and the local node:22-alpine image; no agent calls"]
    fn isolated_graders_accept_references_and_preserve_the_read_only_workspace() -> Result<()> {
        let directory = tempfile::tempdir()?;
        for task in [
            "json-patch",
            "dag-scheduler",
            "sse-decoder",
            "interval-overlay",
        ] {
            let workspace = directory.path().join(task);
            fs::create_dir(&workspace)?;
            let reference = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("benchmarks/coding/references")
                .join(format!("{task}.mjs"));
            let source = fs::read(reference)?;
            fs::write(workspace.join("index.mjs"), &source)?;
            let grading = verify(
                &workspace,
                &directory.path().join(format!("state-{task}")),
                task,
                "node:22-alpine",
            )?;
            assert_eq!(grading["passed"], true, "{grading}");
            assert_eq!(fs::read(workspace.join("index.mjs"))?, source);
        }
        Ok(())
    }

    #[test]
    fn unknown_native_usage_is_never_complete_or_inferred_from_text_length() {
        assert_eq!(native_usage("", true)["complete_usage"], false);
        assert_eq!(
            native_usage("{\"type\":\"turn.completed\",\"usage\":null}", true)["complete_usage"],
            false
        );
        let text =
            "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":10,\"output_tokens\":3}}";
        assert_eq!(native_usage(text, true)["recorded_model_tokens"], 13);
        assert_eq!(native_usage(text, false)["complete_usage"], false);
        assert_eq!(native_usage(text, true)["schema_tokens"], Value::Null);
    }

    #[test]
    fn corrections_and_partial_usage_remain_in_task_totals() {
        let events = [
            json!({"kind":"attempt.started","task":"fixture","repeat":0,"harness":"aegis"}),
            json!({"kind":"attempt.finished","task":"fixture","repeat":0,"harness":"aegis","accepted":false,"accounting":{"recorded_model_tokens":10,"complete_usage":true}}),
            json!({"kind":"attempt.started","task":"fixture","repeat":0,"harness":"aegis"}),
            json!({"kind":"attempt.finished","task":"fixture","repeat":0,"harness":"aegis","accepted":true,"accounting":{"recorded_model_tokens":20,"complete_usage":true}}),
        ];
        let result = summarize(&events);
        assert_eq!(result["cases"][0]["correction_rounds"], 1);
        assert_eq!(result["cases"][0]["tokens_per_accepted_task"], 30);
        let mut partial = events.to_vec();
        partial[1]["accounting"]["complete_usage"] = json!(false);
        assert_eq!(
            summarize(&partial)["cases"][0]["tokens_per_accepted_task"],
            Value::Null
        );
        assert_eq!(summarize(&partial)["cases"][0]["recorded_model_tokens"], 30);
        assert_eq!(
            summarize(&partial[..3])["cases"][0]["unfinished_attempts"],
            1
        );
    }

    #[test]
    fn accuracy_and_efficiency_charge_failed_tasks_without_hiding_missing_receipts() {
        let mut events = Vec::new();
        for (task, accepted, tokens, passed) in [("one",true,10,4),("two",true,20,4),("three",false,60,2)] {
            events.push(json!({"kind":"attempt.started","task":task,"repeat":0,"harness":"aegis"}));
            events.push(json!({"kind":"attempt.finished","task":task,"repeat":0,"harness":"aegis","accepted":accepted,"task_integrity":true,"accounting":{"recorded_model_tokens":tokens,"complete_usage":true},"execution":{"elapsed_ms":1000},"grading":{"result":{"passed_cases":passed,"total_cases":4}}}));
        }
        let summary = summarize(&events);
        let arm = &summary["harnesses"]["aegis"];
        assert_eq!(arm["accepted_tasks"],2);
        assert_eq!(arm["tasks"],3);
        assert_eq!(arm["tokens_per_accepted_task"],45.0);
        assert_eq!(arm["ms_per_accepted_task"],1500.0);
        assert_eq!(arm["passed_assertions"],10);
        assert_eq!(arm["total_assertions"],12);
        events[5]["accounting"]["complete_usage"]=json!(false);
        let partial = summarize(&events);
        assert!(partial["harnesses"]["aegis"]["tokens_per_accepted_task"].is_null());
        assert_eq!(partial["harnesses"]["aegis"]["reported_tokens"],90);
        events[5]["execution"]["elapsed_ms"]=Value::Null;
        assert!(summarize(&events)["harnesses"]["aegis"]["ms_per_accepted_task"].is_null());
    }
}
