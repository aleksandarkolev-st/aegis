use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
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
    for event in text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
    {
        if event["type"] == "turn.completed" {
            turns += 1;
            match (
                event["usage"]["input_tokens"].as_u64(),
                event["usage"]["output_tokens"].as_u64(),
            ) {
                (Some(input), Some(output)) => {
                    tokens = tokens.saturating_add(input).saturating_add(output)
                }
                _ => unknown += 1,
            }
        }
        if event["type"] == "turn.failed" {
            unknown += 1;
        }
    }
    json!({"recorded_model_tokens":tokens,"model_turns":turns,"complete_usage":successful && turns > 0 && unknown == 0,"unaccounted_turns":unknown,"schema_tokens":null})
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
        let entry = cases.entry(key).or_insert_with(|| json!({"task":event["task"],"repeat":event["repeat"],"harness":event["harness"],"started_attempts":0,"finished_attempts":0,"unaccounted_attempts":0,"accepted":false,"recorded_model_tokens":0}));
        if event["kind"] == "attempt.started" {
            entry["started_attempts"] = json!(entry["started_attempts"].as_u64().unwrap_or(0) + 1);
        }
        if event["kind"] == "attempt.finished" {
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
    }
    json!({"cases":cases.values().collect::<Vec<_>>(),"notes":"Cumulative tokens include failed correction rounds. Unknown usage or unfinished attempts excludes a task from complete-token comparisons. Correction rounds are outer reattempts, not every model/tool decision. Native schema exposure is unobservable, not zero."})
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

fn main() -> Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let mut run_agents = false;
    let mut preparation = false;
    let mut ready = None;
    let mut aegis = None;
    let mut codex_reference = None;
    let mut model = "gpt-5.5".to_owned();
    let mut image = "node:22-alpine".to_owned();
    let mut repeats = 2_usize;
    let mut seconds = 600_u64;
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
        index += 1;
        let value = arguments
            .get(index)
            .context("benchmark option needs a value")?;
        match flag {
            "--ready" => ready = Some(PathBuf::from(value)),
            "--aegis" => aegis = Some(dunce::canonicalize(value)?),
            "--codex-reference" => codex_reference = Some(PathBuf::from(value)),
            "--model" => model = value.clone(),
            "--image" => image = value.clone(),
            "--repeats" => repeats = value.parse()?,
            "--seconds" => seconds = value.parse()?,
            _ => bail!("unknown benchmark option: {flag}"),
        }
        index += 1;
    }
    if (run_agents && preparation)
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
    let binary = hash(&fs::read(std::env::current_exe()?)?);
    let plan = hash(include_bytes!("../plan.txt"));
    let mut attestation = Value::Null;
    let mut codex_version = Value::Null;
    if run_agents {
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
        let aegis = aegis
            .as_ref()
            .context("--aegis must identify the verified native runtime")?;
        if attestation["aegis_binary_sha256"] != hash(&fs::read(aegis)?) {
            bail!("Aegis build does not match readiness record");
        }
        let reference = reviewed_reference(codex_reference.as_deref().context("--codex-reference must identify the reviewed external comparator; Aegis never resolves or installs it")?, &attestation)?;
        codex_reference = Some(reference);
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
        let version = process::background(&mut Command::new(codex_reference.as_ref().unwrap()))
            .arg("--version")
            .output()?;
        if !version.status.success() {
            bail!("native Codex version unavailable");
        }
        codex_version = json!(String::from_utf8_lossy(&version.stdout).trim());
    }
    let root = std::env::current_dir()?
        .join(".arun/coding-bench")
        .join(uuid::Uuid::new_v4().to_string());
    fs::create_dir_all(&root)?;
    let root = dunce::canonicalize(root)?;
    let manifest = json!({"kind":"public-handwritten-coding-fixtures-v1","benchmark_binary_sha256":binary,"plan_sha256":plan,"grader_sha256":hash(GRADER.as_bytes()),"model":model,"codex_version":codex_version,"codex_reference":codex_reference,"codex_binary_sha256":attestation["codex_binary_sha256"],"image":image,"repeats":repeats,"seconds_per_attempt":seconds,"max_attempts":3,"readiness":attestation,"prepare_only":!run_agents,"policy":"Same task and independent grader. Explicitly reviewed external Codex uses its own workspace-write harness; Aegis uses direct HTTP, scoped file tools and exact containerized Node commands. This is not a native provider adapter, identical tool isolation, a held-out dataset, SWE-bench or an ARC score."});
    fs::write(
        root.join("experiment.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    fs::write(root.join("grader.mjs"), GRADER)?;
    let mut log = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(root.join("attempts.jsonl"))?;
    for repeat in 0..repeats {
        for (task, source, specification) in TASKS {
            let arms = if repeat % 2 == 0 {
                ["aegis", "codex"]
            } else {
                ["codex", "aegis"]
            };
            for arm in arms {
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
                for attempt in 0..3 {
                    let prefix = root.join(format!("{task}-{repeat}-{arm}-{attempt}"));
                    let prompt = format!("{specification}\n{feedback}");
                    let prompt_file = prefix.with_extension("prompt.txt");
                    fs::write(&prompt_file, &prompt)?;
                    record(
                        &mut log,
                        json!({"kind":"attempt.started","task":task,"repeat":repeat,"harness":arm,"attempt":attempt,"created_at":arun::storage::unix_time(),"prompt_sha256":hash(prompt.as_bytes())}),
                    )?;
                    let mut command = Command::new(if arm == "codex" {
                        reviewed_reference(codex_reference.as_ref().unwrap(), &attestation)?
                    } else {
                        aegis.clone().unwrap()
                    });
                    if arm == "codex" {
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
                                "--image",
                                &image,
                                "--command-scopes",
                            ])
                            .arg(&command_scopes)
                            .arg("--filesystem-scopes")
                            .arg(&file_scopes)
                            .args(["--wall-seconds", &seconds.to_string(), "--foreground"]);
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
                            json!({"run_id":run.id,"state":run.state,"recorded_model_tokens":metrics.model_tokens,"complete_usage":successful && metrics.model_attempts > 0 && metrics.unaccounted_model_attempts == 0 && metrics.estimated_turns == 0,"metrics":metrics})
                        } else {
                            json!({"complete_usage":false,"recorded_model_tokens":0,"error":"no runtime run was created"})
                        }
                    };
                    let grading = verify(
                        &workspace,
                        &root.join(format!("grade-{task}-{repeat}-{arm}-{attempt}")),
                        task,
                        &image,
                    )?;
                    let integrity =
                        fs::read(workspace.join("TASK.txt"))? == specification.as_bytes();
                    let passed = grading["passed"] == true && integrity && successful;
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
}
