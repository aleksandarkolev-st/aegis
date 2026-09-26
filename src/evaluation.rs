use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    capability, kernel, mcp,
    storage::{Run, Store},
    trace,
};

const SERVER_SOURCE: &str = include_str!("../tests/fixtures/eval.mjs");
const MARKER: &str = "AEGIS_EVAL_MARKER_C94D73";
const LOG_ERROR: &str = "AEGIS_EVAL_LOG_FAILURE";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Options {
    pub provider: String,
    pub model: Option<String>,
    pub sizes: Vec<usize>,
    pub modes: Vec<String>,
    pub tasks: Vec<String>,
    pub repeats: usize,
    pub actions: u64,
    pub model_tokens: u64,
    pub context_chars: u64,
    pub wall_seconds: u64,
    pub image: String,
    pub prepare_only: bool,
    pub restart_at: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            provider: "codex".into(),
            model: None,
            sizes: vec![50, 100, 250, 500],
            modes: ["eager", "lazy", "artifact", "durable"]
                .map(str::to_owned)
                .to_vec(),
            tasks: ["read", "log", "repair"].map(str::to_owned).to_vec(),
            repeats: 1,
            actions: 12,
            model_tokens: 400_000,
            context_chars: 256_000,
            wall_seconds: 600,
            image: "node:22-alpine".into(),
            prepare_only: false,
            restart_at: None,
        }
    }
}

impl Options {
    pub fn parse(args: &[String]) -> Result<Self> {
        let mut options = Self::default();
        let mut index = 0;
        while index < args.len() {
            let flag = args[index].as_str();
            if flag == "--prepare-only" {
                options.prepare_only = true;
                index += 1;
                continue;
            }
            index += 1;
            let value = args
                .get(index)
                .with_context(|| format!("{flag} needs a value"))?;
            match flag {
                "--provider" => {
                    options.provider = match value.as_str() {
                        "chatgpt" | "codex" => "codex",
                        "claude" | "claude-code" => "claude",
                        "grok" => "grok",
                        _ => bail!("choose chatgpt, claude, or grok"),
                    }
                    .into()
                }
                "--sizes" => {
                    options.sizes = value
                        .split(',')
                        .map(str::parse)
                        .collect::<std::result::Result<_, _>>()?
                }
                "--model" => {
                    if value.trim().is_empty() {
                        bail!("model ID cannot be empty");
                    }
                    options.model = Some(value.clone());
                }
                "--modes" => options.modes = value.split(',').map(str::to_owned).collect(),
                "--tasks" => options.tasks = value.split(',').map(str::to_owned).collect(),
                "--repeats" => options.repeats = value.parse()?,
                "--actions" => options.actions = value.parse()?,
                "--model-tokens" => options.model_tokens = value.parse()?,
                "--context-chars" => options.context_chars = value.parse()?,
                "--wall-seconds" => options.wall_seconds = value.parse()?,
                "--image" => options.image = value.clone(),
                "--restart-at" => {
                    if ![
                        "operation.executing",
                        "operation.succeeded",
                        "checkpoint.created",
                    ]
                    .contains(&value.as_str())
                    {
                        bail!(
                            "restart boundary must be operation.executing, operation.succeeded, or checkpoint.created"
                        );
                    }
                    options.restart_at = Some(value.clone());
                }
                _ => bail!("unknown evaluation option: {flag}"),
            }
            index += 1;
        }
        if options.sizes.is_empty()
            || options
                .sizes
                .iter()
                .any(|size| ![50, 100, 250, 500].contains(size))
        {
            bail!("registry sizes must be drawn from 50,100,250,500");
        }
        if options.modes.is_empty()
            || options
                .modes
                .iter()
                .any(|mode| !["eager", "lazy", "artifact", "durable"].contains(&mode.as_str()))
        {
            bail!("modes must be drawn from eager,lazy,artifact,durable");
        }
        if options.tasks.is_empty()
            || options
                .tasks
                .iter()
                .any(|task| !["read", "log", "repair"].contains(&task.as_str()))
        {
            bail!("tasks must be drawn from read,log,repair");
        }
        if !(1..=100).contains(&options.repeats)
            || !(1..=1000).contains(&options.actions)
            || options.model_tokens == 0
            || options.context_chars == 0
            || options.wall_seconds == 0
        {
            bail!("evaluation budgets and repeat count must be positive and bounded");
        }
        if options
            .sizes
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len()
            != options.sizes.len()
            || options
                .modes
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
                != options.modes.len()
            || options
                .tasks
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
                != options.tasks.len()
        {
            bail!("duplicate evaluation conditions are not allowed");
        }
        Ok(options)
    }
}

#[derive(Debug, Serialize)]
struct Case {
    size: usize,
    mode: String,
    task: String,
    repeat: usize,
    root: PathBuf,
    run_id: String,
    restart_at: Option<String>,
}

#[derive(Debug, Serialize)]
struct Acceptance {
    passed: bool,
    reason: String,
    verifier_run: Option<String>,
}

fn prompt(task: &str) -> &str {
    match task {
        "read" => {
            "Read fixture.txt in this workspace. Report the exact marker string on its marker line, with successful operation evidence."
        }
        "log" => {
            "Read the latest compiler build log. Find and report the exact AEGIS error identifier and source location at the end, with successful operation evidence."
        }
        "repair" => {
            "Repair math.mjs so its exported sum(left, right) adds its two arguments. Preserve the export and report the patch with successful operation evidence. External tests will cover positive, negative, and zero values."
        }
        _ => unreachable!(),
    }
}

fn prepare(root: &Path, options: &Options) -> Result<Vec<Case>> {
    fs::create_dir_all(root)?;
    let source = root.join("eval-server.mjs");
    fs::write(&source, SERVER_SOURCE)?;
    let source = dunce::canonicalize(source)?;
    let mut cases = Vec::new();
    for size in &options.sizes {
        let state_root = root.join(format!("registry-{size}"));
        let server = mcp::Server {
            policy: mcp::Policy {
                trusted_host: true,
                ..Default::default()
            },
            name: "eval".into(),
            command: "node".into(),
            args: vec![
                source.to_string_lossy().into_owned(),
                (size - capability::registry().len()).to_string(),
            ],
        };
        let tools = mcp::discover(&server, root)?;
        let mut store = Store::open(&state_root)?;
        store.register_mcp(&server, &tools)?;
        let manifests = capability::all(&store)?;
        if manifests.len() != *size {
            bail!("evaluation registry has the wrong size");
        }
        fs::write(
            state_root.join("registry.json"),
            serde_json::to_vec_pretty(&manifests)?,
        )?;
        let mut grants: Vec<_> = vec![
            "workspace.read".into(),
            "workspace.write".into(),
            "process.run".into(),
        ];
        grants.extend(tools.iter().map(|tool| format!("mcp:eval:{}", tool.name)));
        for repeat in 0..options.repeats {
            for task in &options.tasks {
                for offset in 0..options.modes.len() {
                    let mode = &options.modes[(offset + repeat) % options.modes.len()];
                    let workspace = root.join(format!("fixture-{size}-{task}-{repeat}-{mode}"));
                    fs::create_dir_all(&workspace)?;
                    fs::write(
                        workspace.join("fixture.txt"),
                        format!("fixture version 1\nmarker: {MARKER}\n"),
                    )?;
                    fs::write(
                        workspace.join("math.mjs"),
                        "export function sum(left, right) { return left - right; }\n",
                    )?;
                    let run = store.create_run(prompt(task), &workspace, &options.provider, json!(grants),
                        json!({"model":options.model, "mode": mode, "actions": options.actions, "model_tokens":options.model_tokens,
                            "context_chars":options.context_chars, "wall_seconds":options.wall_seconds,
                            "model_seconds":180, "process_seconds":60}),
                        "Independent fixture evidence checks; repair uses containerized Node assertions")?;
                    cases.push(Case {
                        size: *size,
                        mode: mode.clone(),
                        task: task.clone(),
                        repeat,
                        root: state_root.clone(),
                        run_id: run.id,
                        restart_at: options.restart_at.clone(),
                    });
                }
            }
        }
    }
    Ok(cases)
}

fn textual_acceptance(store: &Store, run: &Run, expected: &[&str]) -> Result<Acceptance> {
    let events = store.events(&run.id)?;
    let completion = events
        .iter()
        .rev()
        .find(|event| event.kind == "run.completed");
    let mut passed = run.state == "completed";
    passed &= completion.is_some_and(|event| {
        event.payload["summary"]
            .as_str()
            .is_some_and(|summary| expected.iter().all(|answer| summary.contains(answer)))
    });
    let evidence = completion.and_then(|event| event.payload["evidence"].as_array());
    let mut grounded = false;
    for hash in evidence.into_iter().flatten().filter_map(Value::as_str) {
        if store.has_evidence(&run.id, hash)? {
            let bytes = store.artifact(hash)?;
            let text = String::from_utf8_lossy(&bytes);
            if expected.iter().all(|answer| text.contains(answer)) {
                grounded = true;
            }
        }
    }
    passed &= grounded;
    Ok(Acceptance {
        passed,
        reason: if passed {
            "completed summary and successful evidence contain the expected fixture answer"
        } else {
            "missing completed answer or matching successful-operation evidence"
        }
        .into(),
        verifier_run: None,
    })
}

fn repair_acceptance(store: &mut Store, root: &Path, run: &Run, image: &str) -> Result<Acceptance> {
    let verifier = store.create_run(
        "External repair acceptance",
        Path::new(&run.workspace),
        "verifier",
        json!(["process.run", "process:node"]),
        json!({"container_image":image, "process_seconds":30}),
        "Node addition assertions",
    )?;
    store.state(&verifier.id, "running", json!({}))?;
    let script = "const assert = await import('node:assert/strict'); const {sum} = await import('./math.mjs'); assert.equal(sum(2,3),5); assert.equal(sum(-2,3),1); assert.equal(sum(0,0),0); assert.equal(sum(2.5,0.5),3);";
    let operation = store.begin_operation(
        &verifier.id,
        "process.run",
        json!({"program":"node", "args":["--input-type=module", "-e", script]}),
        false,
    )?;
    kernel::perform(store, root, &verifier, &operation)?;
    let result = store.operation(&operation.id)?;
    let passed = if let Some(hash) = &result.artifact {
        let output: Value = serde_json::from_slice(&store.artifact(hash)?)?;
        if result.state == "succeeded" {
            store.complete_run(
                &verifier.id,
                "acceptance process finished",
                std::slice::from_ref(hash),
            )?;
        }
        run.state == "completed" && output["exit_code"] == 0
    } else {
        false
    };
    Ok(Acceptance {
        passed,
        reason: format!("containerized Node assertions: operation {}", result.state),
        verifier_run: Some(verifier.id),
    })
}

pub fn command(root: &Path, args: &[String]) -> Result<()> {
    let options = Options::parse(args)?;
    let experiment = root
        .join("evaluations")
        .join(uuid::Uuid::new_v4().to_string());
    fs::create_dir_all(&experiment)?;
    let version = crate::provider::find(&options.provider)?
        .and_then(|program| Command::new(program).arg("--version").output().ok())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned());
    fs::write(
        experiment.join("experiment.json"),
        serde_json::to_vec_pretty(&json!({
            "options":options, "runtime_version":env!("CARGO_PKG_VERSION"), "provider_cli_version":version,
            "model":options.model.as_deref().unwrap_or("provider default; not pinned"), "model_seed":"unsupported by these CLI adapters",
            "fixture_sha256":hex::encode(Sha256::digest(SERVER_SOURCE.as_bytes())), "created_at":crate::storage::unix_time(),
            "schema_metric":"UTF-8 bytes, not tokenizer-specific tokens", "cost_metric":"unavailable; no price assumptions"
        }))?,
    )?;
    let cases = prepare(&experiment, &options)?;
    fs::write(
        experiment.join("cases.json"),
        serde_json::to_vec_pretty(&cases)?,
    )?;
    println!(
        "evaluation: {}  cases: {}",
        experiment.display(),
        cases.len()
    );
    if options.prepare_only {
        return Ok(());
    }
    let mut report = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(experiment.join("results.jsonl"))?;
    let mut rows = Vec::new();
    for case in cases {
        let started = Instant::now();
        let (error, restart) = if let Some(cut) = &case.restart_at {
            crate::restart::execute(&case.root, &case.run_id, cut, options.wall_seconds)?
        } else {
            (
                kernel::drive(&case.root, &case.run_id)
                    .err()
                    .map(|error| format!("{error:#}")),
                Value::Null,
            )
        };
        let execution_ms = started.elapsed().as_millis();
        let mut store = Store::open(&case.root)?;
        let run = store.run(&case.run_id)?;
        let events = store.events(&run.id)?;
        let acceptance = match case.task.as_str() {
            "read" => textual_acceptance(&store, &run, &[MARKER])?,
            "log" => textual_acceptance(&store, &run, &[LOG_ERROR, "codec.rs:73"])?,
            "repair" => repair_acceptance(&mut store, &case.root, &run, &options.image)?,
            _ => unreachable!(),
        };
        let mut event_log = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(case.root.join(format!("events-{}.jsonl", run.id)))?;
        for event in &events {
            writeln!(event_log, "{}", serde_json::to_string(event)?)?;
        }
        event_log.sync_all()?;
        let wrong_tools = events
            .iter()
            .filter(|event| {
                event.kind == "model.response" && event.payload["action"]["kind"] == "invoke"
            })
            .filter(|event| {
                let tool = event.payload["action"]["capability"]
                    .as_str()
                    .unwrap_or_default();
                match case.task.as_str() {
                    "read" => !["workspace.read", "workspace.search"].contains(&tool),
                    "log" => tool != "mcp.eval.build_log",
                    "repair" => {
                        !["workspace.read", "workspace.search", "workspace.write"].contains(&tool)
                    }
                    _ => true,
                }
            })
            .count();
        let invalid_arguments = events
            .iter()
            .filter(|event| {
                event.kind == "action.rejected"
                    && event.payload["error"]
                        .as_str()
                        .is_some_and(|error| error.starts_with("invalid arguments"))
            })
            .count();
        let row = json!({"case":case, "state":run.state, "acceptance":acceptance, "metrics":trace::metrics(&events),
            "wrong_tools":wrong_tools, "invalid_arguments":invalid_arguments, "execution_ms":execution_ms,
            "execution_and_acceptance_ms":started.elapsed().as_millis(), "runtime_error":error,
            "context_overflow":events.iter().any(|event| event.kind == "context.over_limit"),"restart":restart});
        writeln!(report, "{}", serde_json::to_string(&row)?)?;
        report.sync_all()?;
        rows.push(row);
        println!(
            "{} tools  {}  {}  accepted={}  tokens={}",
            case.size,
            case.mode,
            case.task,
            acceptance.passed,
            store.model_tokens(&run.id)?
        );
    }
    fs::write(
        experiment.join("paired-summary.json"),
        serde_json::to_vec_pretty(&crate::evaluation_report::summarize(&rows)?)?,
    )?;
    println!(
        "raw results: {}",
        experiment.join("results.jsonl").display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_or_duplicate_experimental_conditions() {
        for args in [
            ["--sizes", "51"],
            ["--modes", "unknown"],
            ["--tasks", "read,read"],
            ["--actions", "0"],
        ] {
            assert!(Options::parse(&args.map(str::to_owned)).is_err());
        }
    }

    #[test]
    fn acceptance_requires_answer_and_successful_evidence() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run("read", directory.path(), "codex", json!([]), json!({}), "")?;
        store.state(&run.id, "running", json!({}))?;
        let operation = store.begin_operation(
            &run.id,
            "workspace.read",
            json!({"path":"fixture.txt"}),
            true,
        )?;
        let hash = store.put_artifact(MARKER.as_bytes())?;
        store.operation_state(&operation, "succeeded", Some(&hash), json!({}))?;
        store.complete_run(&run.id, "unverified claim", &[hash])?;
        let run = store.run(&run.id)?;
        assert!(!textual_acceptance(&store, &run, &[MARKER])?.passed);
        Ok(())
    }

    #[test]
    fn log_acceptance_checks_error_identifier_and_source_location() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        for summary in [LOG_ERROR.to_owned(), format!("{LOG_ERROR} at codec.rs:73")] {
            let run =
                store.create_run("log", directory.path(), "codex", json!([]), json!({}), "")?;
            store.state(&run.id, "running", json!({}))?;
            let operation = store.begin_operation(
                &run.id,
                "workspace.read",
                json!({"path":"fixture.txt"}),
                true,
            )?;
            let hash = store.put_artifact(format!("{LOG_ERROR} at codec.rs:73").as_bytes())?;
            store.operation_state(&operation, "succeeded", Some(&hash), json!({}))?;
            store.complete_run(&run.id, &summary, &[hash])?;
            let result =
                textual_acceptance(&store, &store.run(&run.id)?, &[LOG_ERROR, "codec.rs:73"])?;
            assert_eq!(result.passed, summary.contains("codec.rs:73"));
        }
        Ok(())
    }
}
