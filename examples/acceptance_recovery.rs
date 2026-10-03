//! Prepare a controlled same-run acceptance recovery diagnostic without inference.
//! The actual explicitly selected Aegis executable subsequently owns execution.
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use arun::{acceptance::Check, storage::Store, worker};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const GRADER: &str = include_str!("../benchmarks/coding/grade.mjs");
const TASK: &str = include_str!("../benchmarks/coding/interval-overlay.txt");

fn hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn grade_output(store: &Store, artifact: &str) -> Result<Value> {
    let result: Value = serde_json::from_slice(&store.artifact(artifact)?)?;
    let output = result["output_artifact"]
        .as_str()
        .context("Acceptance output missing")?;
    let grade: Value = serde_json::from_slice(&store.artifact(output)?)?;
    ensure!(
        grade["task"] == "interval-overlay" && grade["total_cases"] == 6,
        "Unexpected independent grader result"
    );
    Ok(grade)
}

fn verify(path: &str) -> Result<()> {
    let path = dunce::canonicalize(path)?;
    let allowed = dunce::canonicalize(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".arun/acceptance-recovery"),
    )?;
    ensure!(
        path.starts_with(allowed) && path.file_name().is_some_and(|name| name == "manifest.json"),
        "Verify a diagnostic manifest inside this workspace"
    );
    ensure!(
        fs::metadata(&path)?.len() < 16_384,
        "Diagnostic manifest too large"
    );
    let manifest: Value = serde_json::from_slice(&fs::read(&path)?)?;
    let root = path.parent().context("Manifest directory missing")?;
    let workspace = root.join("workspace");
    let store = Store::open(&workspace.join(".arun"))?;
    let id = manifest["run_id"].as_str().context("Run ID missing")?;
    let run = store.run(id)?;
    ensure!(run.state == "completed", "Controlled run did not complete");
    let binary = manifest["binary"].as_str().context("Binary missing")?;
    ensure!(
        manifest["binary_sha256"] == hash(&fs::read(binary)?),
        "Diagnostic runtime changed"
    );
    ensure!(
        manifest["task_sha256"] == hash(run.task.as_bytes()),
        "Immutable task differs from manifest"
    );
    ensure!(
        manifest["task_sha256"] == hash(&fs::read(workspace.join("TASK.txt"))?),
        "Task file changed"
    );
    let check = Check::from_run(&run)?.context("Frozen acceptance missing")?;
    ensure!(
        manifest["acceptance_sha256"] == hash(&serde_json::to_vec(&check)?)
            && manifest["image"] == check.image,
        "Acceptance contract changed"
    );
    let events = store.events(id)?;
    let failed = events
        .iter()
        .find(|event| event.kind == "acceptance.failed")
        .context("Initial bad candidate was not rejected")?;
    let first_model = events
        .iter()
        .find(|event| event.kind == "model.started")
        .context("No real model recovery")?;
    let passed = events
        .iter()
        .rev()
        .find(|event| event.kind == "acceptance.passed")
        .context("Final independent acceptance did not pass")?;
    let completed = events
        .iter()
        .find(|event| event.kind == "run.completed")
        .context("Missing durable completion")?;
    ensure!(
        failed.seq < first_model.seq && first_model.seq < passed.seq && passed.seq < completed.seq,
        "Recovery/completion sequence invalid"
    );
    ensure!(
        events
            .iter()
            .filter(|event| event.kind == "run.completed")
            .count()
            == 1,
        "Unexpected completion count"
    );
    ensure!(
        events
            .iter()
            .filter(|event| event.kind == "model.started")
            .all(|event| event.payload["route"]["model"] == manifest["model"]
                && event.payload["route"]["reasoning_effort"] == manifest["reasoning_effort"]),
        "Model route differs from diagnostic"
    );
    let initial_artifact = failed.payload["artifact"]
        .as_str()
        .context("Initial acceptance artifact missing")?;
    let final_artifact = passed.payload["artifact"]
        .as_str()
        .context("Final acceptance artifact missing")?;
    ensure!(
        completed.payload["acceptance"] == final_artifact,
        "Completion does not bind the passing check"
    );
    let initial_grade = grade_output(&store, initial_artifact)?;
    let final_grade = grade_output(&store, final_artifact)?;
    ensure!(
        initial_grade["passed"] == false && initial_grade["passed_cases"] == 5,
        "Starting candidate is not the known 5/6 regression"
    );
    ensure!(
        final_grade["passed"] == true && final_grade["passed_cases"] == 6,
        "Repair did not pass all independent assertions"
    );
    let final_hash = hash(&fs::read(workspace.join("index.mjs"))?);
    ensure!(
        manifest["starting_source_sha256"] != final_hash,
        "Candidate was never changed"
    );
    ensure!(
        manifest["starting_source_sha256"]
            == hash(&fs::read(
                manifest["source"]
                    .as_str()
                    .context("Starting source missing")?
            )?),
        "Original retained trial was modified"
    );
    ensure!(
        store.unresolved(id)?.is_empty(),
        "Unresolved operations remain"
    );
    let metrics = arun::trace::metrics(&events);
    ensure!(
        metrics.unaccounted_model_attempts == 0 && metrics.estimated_turns == 0,
        "Recovery usage is incomplete"
    );
    let receipt = json!({
        "phase":"verified", "kind":"controlled-acceptance-recovery-v1", "run_id":id,
        "binary_sha256":manifest["binary_sha256"],"model":manifest["model"],"reasoning_effort":manifest["reasoning_effort"],
        "acceptance_sha256":manifest["acceptance_sha256"], "initial_grade":initial_grade,"final_grade":final_grade,
        "initial_rejection_before_model":true,"completion_after_passing_acceptance":true,
        "task_unchanged":true,"frozen_acceptance_unchanged":true,"original_trial_unchanged":true,
        "final_source_sha256":final_hash,"metrics":metrics,
        "serve_elapsed_seconds":completed.created_at.saturating_sub(events.iter().find(|event|event.kind == "acceptance.started").context("Initial acceptance start missing")?.created_at),
        "production_readiness_verified":false,
        "scope":"Deliberately seeded known failed candidate repaired within a new single run. This is not an unbiased coding score, Codex comparison or multi-hour proof."
    });
    fs::write(
        root.join("receipt.json"),
        serde_json::to_vec_pretty(&receipt)?,
    )?;
    fs::write(
        root.join("events.json"),
        serde_json::to_vec_pretty(&events)?,
    )?;
    println!("{}", serde_json::to_string(&receipt)?);
    Ok(())
}

fn main() -> Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.first().map(String::as_str) == Some("--verify") && arguments.len() == 2 {
        return verify(&arguments[1]);
    }
    if arguments.first().map(String::as_str) != Some("--prepare-only") {
        bail!(
            "Use --prepare-only with explicit --source, --binary, --model and --image. This helper makes no model calls."
        );
    }
    let mut source = None;
    let mut binary = None;
    let mut model = None;
    let mut image = None;
    let mut reasoning = "medium".to_owned();
    let mut options = arguments[1..].chunks_exact(2);
    for option in &mut options {
        match option[0].as_str() {
            "--source" => source = Some(dunce::canonicalize(&option[1])?),
            "--binary" => binary = Some(dunce::canonicalize(&option[1])?),
            "--model" => model = Some(option[1].clone()),
            "--image" => image = Some(option[1].clone()),
            "--reasoning" => reasoning = option[1].clone(),
            _ => bail!("Unknown recovery diagnostic option"),
        }
    }
    if !options.remainder().is_empty() {
        bail!("Every diagnostic option needs a value");
    }
    let source = source.context("--source is required")?;
    let binary = binary.context("--binary is required")?;
    let model = model.context("--model is required")?;
    let image = image.context("--image is required")?;
    if !binary.is_file()
        || model.is_empty()
        || model.len() > 128
        || !arun::catalog::valid_effort(&reasoning)
        || fs::metadata(&source)?.len() > 64_000
    {
        bail!("Invalid diagnostic binary/model/reasoning/source");
    }
    let original = fs::read_to_string(&source)?;
    let binary_hash = hash(&fs::read(&binary)?);
    let script = "const fs=await import('node:fs'); if(fs.existsSync('/workspace/.arun/runs.sqlite')) throw Error('Runtime metadata exposed'); const {grade}=await import('data:text/javascript,'+encodeURIComponent(process.argv.slice(1).join(''))); const result=await grade('interval-overlay',await import('./index.mjs')); console.log(JSON.stringify(result)); if(!result.passed) process.exitCode=1;";
    let mut check_args = vec!["--input-type=module".into(), "-e".into(), script.into()];
    let mut chunk = String::new();
    for character in GRADER.chars() {
        if chunk.len() + character.len_utf8() > 4096 {
            check_args.push(std::mem::take(&mut chunk));
        }
        chunk.push(character);
    }
    if !chunk.is_empty() {
        check_args.push(chunk);
    }
    let check = Check {
        name: "Frozen independent interval overlay assertions".into(),
        program: "node".into(),
        args: check_args,
        image: image.clone(),
        seconds: 30,
    };
    check.validate()?;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(".arun/acceptance-recovery")
        .join(uuid::Uuid::new_v4().to_string());
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace)?;
    fs::write(workspace.join("index.mjs"), &original)?;
    fs::write(workspace.join("TASK.txt"), TASK)?;
    fs::write(
        workspace.join("verify.mjs"),
        "await import('./index.mjs');\n",
    )?;
    let state = workspace.join(".arun");
    let mut store = Store::open(&state)?;
    store.set_learning(&workspace, false)?;
    let run = store.create_run(
        TASK,
        &workspace,
        "codex",
        json!(["workspace.read","workspace.write","process.run","process:node"]),
        json!({
            "provider_transport":"aegis-direct-v1", "model":model,
            "reasoning_effort":reasoning, "mode":"durable", "wall_seconds":600,
            "model_seconds":180, "model_response_bytes":262144,
            "process_seconds":30, "container_image":image, "acceptance_check":check,
            "command_scopes":{"commands":[{"program":"node","args":["--check","index.mjs"]},{"program":"node","args":["verify.mjs"]}]},
            "filesystem_scopes":{"read":["index.mjs","verify.mjs","TASK.txt"],"write":["index.mjs","verify.mjs"]}
        }),
        &check.name,
    )?;
    store.state(
        &run.id,
        "running",
        json!({"source":"controlled recovery preparation"}),
    )?;
    let read =
        store.begin_operation(&run.id, "workspace.read", json!({"path":"index.mjs"}), true)?;
    store.operation_state(&read, "dispatched", None, json!({}))?;
    let result = worker::execute(&state, &read.id)?;
    let proof = store.put_artifact(&serde_json::to_vec(&result)?)?;
    store.operation_state(
        &read,
        "succeeded",
        Some(&proof),
        json!({"capability":"workspace.read"}),
    )?;
    let proposal = store.put_artifact(&serde_json::to_vec(&json!({
        "summary":"Controlled candidate proposal; independent correctness must be checked",
        "evidence":[proof]
    }))?)?;
    store.event(&run.id, "completion.proposed", json!({"artifact":proposal}))?;
    store.state(
        &run.id,
        "ready",
        json!({"reason":"prepared for explicit same-run acceptance recovery diagnostic"}),
    )?;
    store.save_snapshot(&run.id)?;
    let manifest = json!({
        "kind":"controlled-acceptance-recovery-v1", "directory":root,
        "workspace":workspace,"state_root":state,"run_id":run.id,
        "binary":binary,"binary_sha256":binary_hash,"model":model,
        "reasoning_effort":reasoning,"source":source,"starting_source_sha256":hash(original.as_bytes()),
        "task_sha256":hash(TASK.as_bytes()),"grader_sha256":hash(GRADER.as_bytes()),
        "acceptance_sha256":hash(&serde_json::to_vec(&check)?),"image":image,
        "proposal":proposal,"prepared_read_evidence":proof,"model_calls":0,
        "production_readiness_verified":false,
        "scope":"Known failed candidate is deliberately proposed for frozen acceptance. Subsequent native serve owns model/tool execution. This is a recovery regression, not an unbiased benchmark or a Codex comparison."
    });
    fs::write(
        root.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    println!("{}", serde_json::to_string(&manifest)?);
    Ok(())
}
