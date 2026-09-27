use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use arun::{mcp, process, storage::Store, trace};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const BRIDGE: &str = include_str!("../benchmarks/arc/bridge.mjs");

fn hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn record(file: &mut File, value: Value) -> Result<()> {
    writeln!(file, "{value}")?;
    file.sync_all()?;
    Ok(())
}

fn capture(mut command: Command, prefix: &Path, seconds: u64) -> Result<Value> {
    let stdout = prefix.with_extension("stdout.jsonl");
    let stderr = prefix.with_extension("stderr.txt");
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(File::create(&stdout)?))
        .stderr(Stdio::from(File::create(&stderr)?));
    let mut child = process::spawn(command)?;
    let start = Instant::now();
    let mut stopped = None;
    let status = loop {
        if fs::metadata(&stdout)?.len() + fs::metadata(&stderr)?.len() > 32 * 1024 * 1024 {
            stopped = Some("capture_limit");
        } else if start.elapsed() >= Duration::from_secs(seconds) {
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
    Ok(
        json!({"exit_code":status.code(),"stopped":stopped,"stdout":stdout,"stderr":stderr,"elapsed_ms":start.elapsed().as_millis()}),
    )
}

fn control(bridge: &Path, session: &Path, operation: &str, value: &str) -> Result<Value> {
    let mut command = Command::new("node");
    command
        .arg(bridge)
        .arg(session)
        .args(["--controller", operation, value]);
    let result = capture(
        command,
        &session.join(format!("controller-{}", uuid::Uuid::new_v4())),
        25,
    )?;
    if result["exit_code"] != 0 || !result["stopped"].is_null() {
        bail!(
            "ARC controller did not finish; inspect session evidence, do not retry uncertain commands"
        );
    }
    let stdout = Path::new(
        result["stdout"]
            .as_str()
            .context("controller stdout missing")?,
    );
    if fs::metadata(stdout)?.len() > 2 * 1024 * 1024 {
        bail!("ARC controller reply exceeds limit");
    }
    serde_json::from_slice(&fs::read(stdout)?).context("ARC controller reply is invalid")
}

fn ready(record: &Value, binary: &str, plan: &str, bridge: &str, runtime: &str) -> bool {
    record["implementation_complete"] == true
        && record["functional_verified"] == true
        && record["installed_ux_verified"] == true
        && record["pending"].as_array().is_some_and(Vec::is_empty)
        && record["benchmark_binary_sha256"] == binary
        && record["plan_sha256"] == plan
        && record["bridge_sha256"] == bridge
        && record["aegis_binary_sha256"] == runtime
}

fn main() -> Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let mut run = false;
    let mut preparation = false;
    let mut competition = false;
    let mut reviewed = None;
    let mut runtime = None;
    let mut backend = "chatgpt".to_owned();
    let mut model = "gpt-5.5".to_owned();
    let mut games = Vec::<String>::new();
    let mut moves = 300_u64;
    let mut seconds = 1800_u64;
    let mut index = 0;
    while index < arguments.len() {
        let flag = arguments[index].as_str();
        match flag {
            "--prepare-only" => preparation = true,
            "--run" => run = true,
            "--competition" => competition = true,
            _ => {
                index += 1;
                let value = arguments.get(index).context("ARC option needs a value")?;
                match flag {
                    "--ready" => reviewed = Some(PathBuf::from(value)),
                    "--aegis" => runtime = Some(dunce::canonicalize(value)?),
                    "--provider" => backend = value.clone(),
                    "--model" => model = value.clone(),
                    "--games" => games = value.split(',').map(str::to_owned).collect(),
                    "--moves" => moves = value.parse()?,
                    "--seconds" => seconds = value.parse()?,
                    _ => bail!("unknown ARC option: {flag}"),
                }
            }
        }
        index += 1;
    }
    if (run && preparation)
        || !["chatgpt", "claude", "grok"].contains(&backend.as_str())
        || !(1..=500).contains(&moves)
        || !(60..=7200).contains(&seconds)
        || model.is_empty()
        || model.len() > 128
        || !model
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._/".contains(&byte))
        || games.len() > 1000
        || games.iter().any(|game| {
            game.len() > 128
                || !game.contains('-')
                || !game
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
        })
        || games
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != games.len()
        || (competition && !games.is_empty())
    {
        bail!("invalid bounded ARC configuration; competition selects all discovered games");
    }
    let binary_hash = hash(&fs::read(std::env::current_exe()?)?);
    let plan_hash = hash(include_bytes!("../plan.txt"));
    let bridge_hash = hash(BRIDGE.as_bytes());
    let mut attestation = Value::Null;
    if run {
        let reviewed = reviewed.context("ARC agent benchmarks stay gated until implementation and functional verification are complete")?;
        if fs::metadata(&reviewed)?.len() > 8192 {
            bail!("readiness record exceeds limit");
        }
        attestation = serde_json::from_slice(&fs::read(reviewed)?)?;
        let runtime = runtime
            .as_ref()
            .context("--aegis must identify the verified native runtime")?;
        if !ready(
            &attestation,
            &binary_hash,
            &plan_hash,
            &bridge_hash,
            &hash(&fs::read(runtime)?),
        ) {
            bail!(
                "ARC readiness review incomplete or source/build changed; no API or agent calls started"
            );
        }
        arun::direct::provider(&backend)?;
    }
    let root = std::env::current_dir()?
        .join(".arun/arc-bench")
        .join(uuid::Uuid::new_v4().to_string());
    let workspace = root.join("workspace");
    let session = root.join("session");
    fs::create_dir_all(&workspace)?;
    fs::create_dir_all(&session)?;
    let root = dunce::canonicalize(root)?;
    let workspace = dunce::canonicalize(workspace)?;
    let session = dunce::canonicalize(session)?;
    let bridge = root.join("bridge.mjs");
    fs::write(&bridge, BRIDGE)?;
    let manifest = json!({"kind":"arc-agi-3-online-v1","prepare_only":!run,"competition_mode":competition,"benchmark_binary_sha256":binary_hash,"plan_sha256":plan_hash,"bridge_sha256":bridge_hash,"readiness":attestation,"provider":backend,"provider_version":null,"provider_transport":"aegis-direct-v1","native_provider_cli_started":false,"model":model,"requested_games":games,"move_limit_per_game":moves,"seconds_per_game":seconds,"cold_start":true,"policy":"Authoritative closed server scorecard; public subset is not an official competition score. Aegis uses direct provider HTTP, not a native agent CLI. Reset only after GAME_OVER, each game once. API actions and normalized/model token receipts are distinct metrics. No automatic uncertain-move retry or in-flight score polling."});
    fs::write(
        root.join("experiment.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    fs::write(
        session.join("state.json"),
        serde_json::to_vec(
            &json!({"schema":1,"moves":0,"move_limit":moves,"calls":0,"bytes":0,"byte_limit":200*1024*1024,"pending":null,"played":[],"closed":false}),
        )?,
    )?;
    let mut log = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(root.join("attempts.jsonl"))?;
    record(
        &mut log,
        json!({"kind":"experiment.prepared","created_at":arun::storage::unix_time(),"manifest_sha256":hash(&serde_json::to_vec(&manifest)?)}),
    )?;
    if !run {
        println!(
            "ARC preparation only; no API or model calls. Records: {}",
            root.display()
        );
        return Ok(());
    }
    fs::write(
        session.join("authorized.json"),
        serde_json::to_vec(&json!({"bridge_sha256":bridge_hash,"review":attestation}))?,
    )?;
    record(
        &mut log,
        json!({"kind":"scorecard.open.started","created_at":arun::storage::unix_time()}),
    )?;
    let discovered = control(
        &bridge,
        &session,
        "initialize",
        if competition { "competition" } else { "sample" },
    )?;
    let mut available = discovered
        .as_array()
        .context("ARC game registry missing")?
        .iter()
        .map(|game| {
            game["game_id"]
                .as_str()
                .context("game ID missing")
                .map(str::to_owned)
        })
        .collect::<Result<Vec<_>>>()?;
    available.sort();
    if competition {
        games = available.clone();
    } else if games.is_empty() {
        games = vec![available.first().context("no ARC games available")?.clone()];
    }
    if games.iter().any(|game| !available.contains(game)) {
        bail!("requested game not in frozen discovery; scorecard remains open, no agent calls");
    }
    fs::write(
        root.join("selected-games.json"),
        serde_json::to_vec_pretty(
            &json!({"selected":games,"registry":discovered,"registry_sha256":hash(&serde_json::to_vec(&discovered)?)}),
        )?,
    )?;
    let state_root = workspace.join(".arun");
    let server = mcp::Server {
        name: "arc".into(),
        command: "node".into(),
        args: vec![
            bridge.to_string_lossy().into_owned(),
            session.to_string_lossy().into_owned(),
        ],
        policy: mcp::Policy {
            trusted_host: true,
            ..Default::default()
        },
    };
    let tools = mcp::discover(&server, &workspace)?;
    if tools.len() != 2
        || !tools.iter().any(|tool| tool.name == "observe")
        || !tools.iter().any(|tool| tool.name == "act")
    {
        bail!("ARC tool registry changed");
    }
    let mut store = Store::open(&state_root)?;
    store.register_mcp(&server, &tools)?;
    store.set_learning(&workspace, false)?;
    let mut totals = 0_u64;
    let mut complete_usage = true;
    let mut finished_runs = 0_u64;
    for game in &games {
        record(
            &mut log,
            json!({"kind":"game.start.requested","game_id":game,"created_at":arun::storage::unix_time()}),
        )?;
        let initial = control(&bridge, &session, "start", game)?;
        let task = format!(
            "Solve this unfamiliar ARC-AGI-3 interactive game by discovering its mechanics. Search for the ARC observe/act capabilities. The grid is 64 by 64; hexadecimal rows encode palette indices, x is column and y is row. Inspect full tool-result artifacts to see the frame, not just truncated recent-event summaries. Available actions are authoritative; do not assume their meanings. RESET is only available after GAME_OVER. Stop if uncertain or exhausted; never retry a lost move. Reach WIN, then finish using evidence from the winning tool operation. Initial observation: {initial}"
        );
        let run = store.create_run(&task,&workspace,&backend,json!(["mcp:arc:observe","mcp:arc:act"]),json!({"mode":"durable","model":model,"actions":moves*3+20,"model_tokens":400000,"wall_seconds":seconds,"model_seconds":180,"process_seconds":30,"context_chars":256000}),"Independent authoritative ARC server state/scorecard, not a model completion claim")?;
        record(
            &mut log,
            json!({"kind":"attempt.started","game_id":game,"run_id":run.id,"created_at":arun::storage::unix_time(),"task_sha256":hash(task.as_bytes())}),
        )?;
        let mut command = Command::new(runtime.as_ref().unwrap());
        command
            .args(["serve"])
            .arg(&state_root)
            .arg(&run.id)
            .current_dir(&workspace)
            .env_remove("ARC_API_KEY");
        let execution = capture(
            command,
            &root.join(format!("game-{}", run.id)),
            seconds + 10,
        )?;
        let events = store.events(&run.id)?;
        fs::write(
            root.join(format!("events-{}.json", run.id)),
            serde_json::to_vec(&events)?,
        )?;
        let metrics = trace::metrics(&events);
        let accounted = metrics.model_attempts > 0
            && metrics.unaccounted_model_attempts == 0
            && metrics.estimated_turns == 0
            && execution["stopped"].is_null();
        totals = totals.saturating_add(metrics.model_tokens);
        complete_usage &= accounted;
        finished_runs += 1;
        let final_state: Value = serde_json::from_slice(&fs::read(session.join("state.json"))?)?;
        record(
            &mut log,
            json!({"kind":"attempt.finished","game_id":game,"run_id":run.id,"runtime_state":store.run(&run.id)?.state,"execution":execution,"metrics":metrics,"complete_usage":accounted,"server_state":final_state["frame"]["state"],"moves_reserved":final_state["moves"],"uncertain":!final_state["pending"].is_null(),"finished_at":arun::storage::unix_time()}),
        )?;
        if !final_state["pending"].is_null() || session.join("request.lock").exists() || !accounted
        {
            break;
        }
    }
    let state: Value = serde_json::from_slice(&fs::read(session.join("state.json"))?)?;
    let scorecard = if state["pending"].is_null() && !session.join("request.lock").exists() {
        record(
            &mut log,
            json!({"kind":"scorecard.close.started","created_at":arun::storage::unix_time()}),
        )?;
        control(&bridge, &session, "close", "")?
    } else {
        Value::Null
    };
    let completed_games = scorecard["total_environments_completed"].as_u64();
    let tokens_per_completed_game = if complete_usage {
        completed_games
            .filter(|count| *count > 0)
            .map(|count| totals as f64 / count as f64)
    } else {
        None
    };
    let summary = json!({"official_closed_scorecard":scorecard,"scorecard_sha256":if scorecard.is_null(){Value::Null}else{json!(hash(&serde_json::to_vec(&scorecard)?))},"competition_mode":competition,"recorded_model_tokens":totals,"complete_usage":complete_usage,"finished_model_runs":finished_runs,"all_selected_games_attempted":finished_runs as usize == games.len(),"server_completed_games":completed_games,"tokens_per_server_completed_game":tokens_per_completed_game,"selected_games":games,"played_games":state["played"],"uncertain_request":state["pending"],"notes":"Server score is authoritative RHAE, not token efficiency or percentage of games solved. Token ratio includes all recorded failed-game costs and uses the closed scorecard completed-game denominator; missing usage, missing closure or no wins yields null. Public sample results must not be labeled competition scores. No learned evaluation memory or previous-game conversations."});
    fs::write(
        root.join("summary.json"),
        serde_json::to_vec_pretty(&summary)?,
    )?;
    record(
        &mut log,
        json!({"kind":"experiment.finished","summary_sha256":hash(&serde_json::to_vec(&summary)?),"finished_at":arun::storage::unix_time()}),
    )?;
    println!("ARC records: {}", root.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incomplete_reviews_and_changed_builds_cannot_start_arc() {
        let mut review = json!({"implementation_complete":true,"functional_verified":true,"installed_ux_verified":true,"pending":[],"benchmark_binary_sha256":"binary","plan_sha256":"plan","bridge_sha256":"bridge","aegis_binary_sha256":"runtime"});
        assert!(ready(&review, "binary", "plan", "bridge", "runtime"));
        assert!(!ready(&review, "binary", "plan", "changed", "runtime"));
        review["pending"] = json!(["Claude verification"]);
        assert!(!ready(&review, "binary", "plan", "bridge", "runtime"));
        assert!(!ready(&Value::Null, "binary", "plan", "bridge", "runtime"));
    }
}
