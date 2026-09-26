use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use arun::kernel;
use arun::storage::Store;
use serde_json::json;

fn root() -> Result<PathBuf> {
    Ok(std::env::current_dir()?.join(".arun"))
}

fn usage() {
    println!(
        "aegis / arun: launch with no arguments for guided terminal tasks, login, and settings."
    );
    println!(
        "arun run <task> [--provider chatgpt|claude|grok|custom] [--model <id>] [--endpoint <url> --api-key-env <name> --response-format schema|json|none] [--mode eager|lazy|artifact|durable] [--allow-write] [--allow-process <program> --image <local-image>] [--acceptance <check.json>] [--allow-mcp <server:tool>] [--actions <limit>] [--model-tokens <limit>] [--wall-seconds <limit>] [--context-chars <limit>] [--foreground]"
    );
    println!(
        "arun attach|resume|status|cancel|replay|trace <run-id> | resolve <run-id> <op-id> succeeded|failed <note> | list | inspect <artifact-hash> | login|probe <provider>"
    );
    println!("arun interrupt <run-id> operation|model");
    println!(
        "Provider capture budget: --model-response-bytes <1024..33554432> (default 8388608); also available in F7 custom budgets."
    );
    println!(
        "Long commands: arun run <task> --process-seconds <1..7200> --wall-seconds <task-limit>"
    );
    println!(
        "Exact commands: --command-scopes <reviewed.json> --image <local-image>; guided scopes are available in F7 settings."
    );
    println!(
        "arun tasks|context|tools|artifacts <run-id> | mcp add <name> --image <local-image> [--allow-write] -- <command> [args...] | mcp add <name> --trusted-host -- <command> [args...]"
    );
    println!(
        "arun eval [--provider chatgpt|claude|grok] [--sizes 50,100,250,500] [--modes eager,lazy,artifact,durable] [--tasks read,log,repair] [--repeats 1] [--restart-at operation.executing|operation.succeeded|checkpoint.created] [--prepare-only] | eval-report <results.jsonl>"
    );
}

fn view(root: &Path, command: &str, id: &str) -> Result<()> {
    let store = Store::open(root)?;
    let run = store.run(id)?;
    match command {
        "tasks" => {
            for milestone in store.milestones(id)? {
                println!(
                    "[{}] {}  {}",
                    if milestone.state == "completed" {
                        "✓"
                    } else if milestone.state == "active" {
                        "→"
                    } else {
                        " "
                    },
                    milestone.title,
                    milestone.evidence.join(", ")
                );
            }
        }
        "context" => {
            println!(
                "run: {} state: {} provider: {}",
                id, run.state, run.provider
            );
            println!(
                "events: {}  artifacts: {}  model tokens: {}",
                store.events(id)?.len(),
                store
                    .events(id)?
                    .iter()
                    .filter(|event| event.kind == "operation.succeeded")
                    .count(),
                store.model_tokens(id)?
            );
            if let Some(checkpoint) = store.last_checkpoint(id)? {
                println!("handoff: {}", serde_json::to_string_pretty(&checkpoint)?);
            }
        }
        "tools" => {
            for event in store
                .events(id)?
                .iter()
                .filter(|event| event.kind == "capability.activated")
            {
                println!("{}", event.payload["id"]);
            }
        }
        "artifacts" => {
            for event in store.events(id)?.iter().filter(|event| {
                event.kind == "operation.succeeded" || event.kind == "checkpoint.created"
            }) {
                println!("{} {}", event.kind, event.payload["artifact"]);
            }
        }
        _ => bail!("unknown view"),
    }
    Ok(())
}

fn attach(root: &Path, id: &str) -> Result<()> {
    uuid::Uuid::parse_str(id).context("invalid run ID")?;
    let mut last = 0;
    let started = Instant::now();
    let mut terminal = arun::terminal::Terminal::default();
    terminal.load_ui(root)?;
    loop {
        let store = Store::open(root)?;
        for event in store.events_since(id, last)? {
            terminal.render_event(&event)?;
            last = event.seq;
        }
        let run = store.run(id)?;
        if run.state != "running" && run.state != "ready" {
            println!("run {}: {}", id, run.state);
            break;
        }
        if started.elapsed() > Duration::from_secs(3) && !kernel::is_active(root, id)? {
            println!("! no runner holds this run; use 'arun resume {id}'");
            break;
        }
        thread::sleep(Duration::from_millis(500));
    }
    Ok(())
}

fn spawn(root: &Path, id: &str) -> Result<()> {
    kernel::spawn(root, id, None)
}

fn run(root: &Path, args: &[String]) -> Result<()> {
    let mut provider = "codex";
    let mut model = None;
    let mut endpoint_url = None;
    let mut api_key_env = None;
    let mut response_format = arun::endpoint::ResponseFormat::Schema;
    let mut allow_insecure = false;
    let mut mode = "durable";
    let mut write = false;
    let mut foreground = false;
    let mut actions = 40_u64;
    let mut model_tokens = 400_000_u64;
    let mut wall_seconds = 3600_u64;
    let mut process_seconds = 60_u64;
    let mut context_chars = 256_000_u64;
    let mut model_response_bytes = arun::budget::DEFAULT_MODEL_RESPONSE_BYTES;
    let mut programs = Vec::new();
    let mut image = None;
    let mut acceptance_check = None;
    let mut command_scopes = None;
    let mut mcp_tools = Vec::new();
    let mut task = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--provider" => {
                index += 1;
                provider = args.get(index).context("--provider needs a value")?;
            }
            "--model" => {
                index += 1;
                let value = args.get(index).context("--model needs an ID")?;
                if value.trim().is_empty() {
                    bail!("model ID cannot be empty");
                }
                model = Some(value.clone());
            }
            "--endpoint" => {
                index += 1;
                endpoint_url = Some(args.get(index).context("--endpoint needs a URL")?.clone());
            }
            "--api-key-env" => {
                index += 1;
                api_key_env = Some(
                    args.get(index)
                        .context("--api-key-env needs a variable name")?
                        .clone(),
                );
            }
            "--response-format" => {
                index += 1;
                response_format = match args
                    .get(index)
                    .context("--response-format needs schema, json, or none")?
                    .as_str()
                {
                    "schema" => arun::endpoint::ResponseFormat::Schema,
                    "json" => arun::endpoint::ResponseFormat::Json,
                    "none" => arun::endpoint::ResponseFormat::None,
                    _ => bail!("response format must be schema, json, or none"),
                };
            }
            "--allow-insecure-endpoint" => allow_insecure = true,
            "--mode" => {
                index += 1;
                mode = args.get(index).context("--mode needs a value")?;
                if !matches!(mode, "eager" | "lazy" | "artifact" | "durable") {
                    bail!("choose eager, lazy, artifact, or durable mode");
                }
            }
            "--allow-write" => write = true,
            "--acceptance" => {
                index += 1;
                let path = args.get(index).context("--acceptance needs a JSON file")?;
                acceptance_check = Some(arun::acceptance::Check::from_file(Path::new(path))?);
            }
            "--allow-process" => {
                index += 1;
                programs.push(
                    args.get(index)
                        .context("--allow-process needs a program")?
                        .clone(),
                );
            }
            "--command-scopes" => {
                index += 1;
                let path = args
                    .get(index)
                    .context("--command-scopes needs a JSON file")?;
                command_scopes = Some(arun::policy::CommandScopes::from_file(Path::new(path))?);
            }
            "--image" => {
                index += 1;
                image = Some(
                    args.get(index)
                        .context("--image needs a locally available Docker image")?
                        .clone(),
                );
            }
            "--allow-mcp" => {
                index += 1;
                mcp_tools.push(
                    args.get(index)
                        .context("--allow-mcp needs server:tool")?
                        .clone(),
                );
            }
            "--foreground" => foreground = true,
            "--actions" => {
                index += 1;
                actions = args
                    .get(index)
                    .context("--actions needs a limit")?
                    .parse()?;
                if actions == 0 || actions > 1000 {
                    bail!("action limit must be 1..1000");
                }
            }
            "--model-tokens" => {
                index += 1;
                model_tokens = args
                    .get(index)
                    .context("--model-tokens needs a limit")?
                    .parse()?;
                if model_tokens == 0 {
                    bail!("model token limit must be positive");
                }
            }
            "--wall-seconds" => {
                index += 1;
                wall_seconds = args
                    .get(index)
                    .context("--wall-seconds needs a limit")?
                    .parse()?;
                if wall_seconds == 0 {
                    bail!("wall-clock limit must be positive");
                }
            }
            "--context-chars" => {
                index += 1;
                context_chars = args
                    .get(index)
                    .context("--context-chars needs a limit")?
                    .parse()?;
                if context_chars == 0 {
                    bail!("context limit must be positive");
                }
            }
            "--model-response-bytes" => {
                index += 1;
                model_response_bytes = args
                    .get(index)
                    .context("--model-response-bytes needs a byte limit")?
                    .parse()?;
                arun::budget::validate_response_bytes(model_response_bytes)?;
            }
            "--process-seconds" => {
                index += 1;
                process_seconds = args
                    .get(index)
                    .context("--process-seconds needs a deadline")?
                    .parse()?;
                if !(1..=7200).contains(&process_seconds) {
                    bail!("command deadline must be 1..7200 seconds");
                }
            }
            argument if argument.starts_with('-') => bail!("unknown option: {argument}"),
            argument => task.push(argument),
        }
        index += 1;
    }
    if task.is_empty() {
        bail!("task is required");
    }
    if let Some(scopes) = &command_scopes {
        programs.extend(
            scopes
                .commands
                .iter()
                .map(|command| command.program.clone()),
        );
        programs.sort();
        programs.dedup();
    }
    if !programs.is_empty() && image.is_none() {
        bail!("--allow-process requires --image; native processes are not supported");
    }
    let provider = match provider {
        "chatgpt" | "codex" => "codex",
        "claude-code" | "claude" => "claude",
        "grok" => "grok",
        "custom" => "custom",
        _ => bail!("choose chatgpt, claude, grok, or custom"),
    };
    let endpoint = if provider == "custom" {
        if model.is_none() {
            bail!("custom endpoints require a model ID");
        }
        let endpoint = arun::endpoint::Endpoint {
            base_url: endpoint_url.context("custom endpoints require a URL")?,
            api_key_env,
            response_format,
            allow_insecure,
        };
        endpoint.url()?;
        Some(endpoint)
    } else {
        if endpoint_url.is_some()
            || api_key_env.is_some()
            || allow_insecure
            || response_format != arun::endpoint::ResponseFormat::Schema
        {
            bail!(
                "endpoint options require the custom provider; native providers use their CLI login"
            );
        }
        None
    };
    let mut grants = vec!["workspace.read".to_owned()];
    if write {
        grants.push("workspace.write".into());
    }
    if !programs.is_empty() {
        grants.push("process.run".into());
    }
    for program in programs {
        grants.push(format!("process:{program}"));
    }
    for tool in mcp_tools {
        grants.push(format!("mcp:{tool}"));
    }
    let mut store = Store::open(root)?;
    let acceptance = acceptance_check
        .as_ref()
        .map(|check| check.name.as_str())
        .unwrap_or("Provide evidence from successful operations");
    let run = store.create_run(
        &task.join(" "),
        &std::env::current_dir()?,
        provider,
        json!(grants),
        json!({"model": model, "endpoint":endpoint, "mode": mode, "actions": actions, "model_tokens": model_tokens, "wall_seconds": wall_seconds, "context_chars": context_chars,
            "model_seconds": 180, "model_response_bytes": model_response_bytes, "process_seconds": process_seconds, "container_image": image, "acceptance_check":acceptance_check, "command_scopes":command_scopes}),
        acceptance,
    )?;
    println!("run: {} provider: {}", run.id, run.provider);
    drop(store);
    if foreground {
        kernel::drive(root, &run.id)?;
        attach(root, &run.id)?;
    } else {
        spawn(root, &run.id)?;
        println!(
            "Use 'arun attach {}' to follow; exiting this terminal will not cancel it.",
            run.id
        );
    }
    Ok(())
}

fn interactive(root: &Path) -> Result<()> {
    arun::session::interactive(root)
}

fn login(provider: &str) -> Result<()> {
    arun::model::login(provider)
}

fn main() {
    if let Err(error) = execute() {
        eprintln!(
            "Aegis: {}",
            arun::terminal::friendly_error(&format!("{error:#}"))
        );
        std::process::exit(1);
    }
}

fn execute() -> Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let root = root()?;
    let required =
        |index: usize| -> Result<&str> { Ok(arguments.get(index).context("missing argument")?) };
    match arguments.first().map(String::as_str) {
        None => interactive(&root),
        Some("run") => run(&root, &arguments[1..]),
        Some("eval") => arun::evaluation::command(&root, &arguments[1..]),
        Some("eval-report") => {
            println!(
                "{}",
                serde_json::to_string_pretty(&arun::evaluation_report::from_file(Path::new(
                    required(1)?
                ))?)?
            );
            Ok(())
        }
        Some("restart-check") => {
            let (error, observation) =
                arun::restart::execute(Path::new(required(1)?), required(2)?, required(3)?, 60)?;
            println!("{}", serde_json::to_string_pretty(&observation)?);
            if let Some(error) = error {
                bail!(error);
            }
            Ok(())
        }
        Some("serve") => kernel::drive(Path::new(required(1)?), required(2)?),
        Some("worker") => {
            let value = arun::worker::execute(Path::new(required(1)?), required(2)?)?;
            println!("{value}");
            Ok(())
        }
        Some("mcp") => {
            if required(1)? != "add" {
                bail!("use 'arun mcp add <name> --image <local-image> -- <command> [args...]'");
            }
            let name = required(2)?;
            if name.is_empty()
                || !name.chars().all(|character| {
                    character.is_ascii_alphanumeric() || character == '_' || character == '-'
                })
            {
                bail!("MCP server name must be alphanumeric, underscore, or hyphen");
            }
            let mut policy = arun::mcp::Policy::default();
            let mut index = 3;
            while let Some(flag) = arguments.get(index) {
                match flag.as_str() {
                    "--image" => {
                        index += 1;
                        policy.image = Some(required(index)?.into());
                    }
                    "--allow-write" => policy.write = true,
                    "--trusted-host" => policy.trusted_host = true,
                    "--" => {
                        index += 1;
                        break;
                    }
                    flag if flag.starts_with('-') => bail!("unknown MCP policy option: {flag}"),
                    _ => break,
                }
                index += 1;
            }
            let server = arun::mcp::Server {
                name: name.into(),
                command: required(index)?.into(),
                args: arguments.get(index + 1..).unwrap_or_default().to_vec(),
                policy,
            };
            let tools = arun::mcp::discover(&server, &std::env::current_dir()?)?;
            Store::open(&root)?.register_mcp(&server, &tools)?;
            for tool in tools {
                println!(
                    "mcp.{}.{}  grant: mcp:{}:{}",
                    server.name, tool.name, server.name, tool.name
                );
            }
            Ok(())
        }
        Some("attach") => attach(&root, required(1)?),
        Some("tasks" | "context" | "tools" | "artifacts") => {
            view(&root, required(0)?, required(1)?)
        }
        Some("resume") => {
            let id = required(1)?;
            let mut store = Store::open(&root)?;
            let run = store.run(id)?;
            if matches!(run.state.as_str(), "completed" | "cancelled" | "failed")
                || store.unknown_count(id)? > 0
            {
                bail!("run cannot resume until unknown operations are reconciled");
            }
            if run.state == "waiting_recovery" {
                store.state(id, "ready", json!({"source":"user_resume"}))?;
            }
            drop(store);
            if arguments.iter().any(|argument| argument == "--foreground") {
                kernel::drive(&root, id)?;
                attach(&root, id)
            } else {
                spawn(&root, id)?;
                println!("resuming {id}");
                Ok(())
            }
        }
        Some("status") => {
            println!(
                "{}",
                serde_json::to_string_pretty(&Store::open(&root)?.run(required(1)?)?)?
            );
            Ok(())
        }
        Some("list") => {
            for run in Store::open(&root)?.runs()? {
                println!("{} {} {}", run.id, run.state, run.task);
            }
            Ok(())
        }
        Some("cancel") => {
            Store::open(&root)?.state(required(1)?, "cancelled", json!({"source":"user"}))?;
            Ok(())
        }
        Some("interrupt") => {
            let scope = match required(2)? {
                "operation" => arun::interrupt::Scope::Operation,
                "model" => arun::interrupt::Scope::Model,
                _ => bail!("interrupt scope must be operation or model"),
            };
            let target = Store::open(&root)?.request_interrupt(required(1)?, scope)?;
            println!(
                "{}",
                target.as_deref().unwrap_or("no active work for this scope")
            );
            Ok(())
        }
        Some("resolve") => {
            let id = required(1)?.to_owned();
            let operation = required(2)?.to_owned();
            let succeeded = match required(3)? {
                "succeeded" => true,
                "failed" => false,
                _ => bail!("outcome must be succeeded or failed"),
            };
            let note = arguments.get(4..).unwrap_or_default().join(" ");
            Store::open(&root)?.resolve_unknown(&id, &operation, succeeded, &note)?;
            println!("reconciled {operation}; use 'arun resume {id}'");
            Ok(())
        }
        Some("trace") => {
            let store = Store::open(&root)?;
            let id = required(1)?;
            arun::trace::display(&store.run(id)?, &store.events(id)?);
            Ok(())
        }
        Some("replay") => {
            for event in Store::open(&root)?.events(required(1)?)? {
                println!("{}", serde_json::to_string(&event)?);
            }
            Ok(())
        }
        Some("inspect") => {
            let bytes = Store::open(&root)?.artifact(required(1)?)?;
            io::stdout().write_all(&bytes)?;
            Ok(())
        }
        Some("login") => login(required(1)?),
        Some("probe") => {
            Store::open(&root)?;
            let provider = match required(1)? {
                "chatgpt" => "codex",
                "claude-code" => "claude",
                other => other,
            };
            let prompt = r#"Return only this JSON action: {"kind":"blocked","reason":"probe successful"}. Do not use tools."#;
            let (action, _) = arun::model::call(provider, prompt, &root, Duration::from_secs(120))?;
            println!("{action:?}");
            Ok(())
        }
        Some("help") | Some("--help") => {
            usage();
            Ok(())
        }
        Some(other) => bail!("unknown command: {other}"),
    }
}
