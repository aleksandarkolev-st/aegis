use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use arun::kernel;
use arun::storage::{Event, Store};
use serde_json::json;

fn root() -> Result<PathBuf> {
    Ok(std::env::current_dir()?.join(".arun"))
}

fn usage() {
    println!(
        "arun [run <task> [--provider chatgpt|claude|grok] [--mode eager|lazy|artifact|durable] [--allow-write] [--allow-process <program> --image <local-image>] [--allow-mcp <server:tool>] [--actions <limit>] [--model-tokens <limit>] [--wall-seconds <limit>] [--foreground]]"
    );
    println!(
        "arun attach|resume|status|cancel|replay|trace <run-id> | resolve <run-id> <op-id> succeeded|failed <note> | list | inspect <artifact-hash> | login|probe <provider>"
    );
    println!("arun tasks|context|tools|artifacts <run-id> | mcp add <name> <command> [args...]");
    println!(
        "arun eval [--provider chatgpt|claude|grok] [--sizes 50,100,250,500] [--modes eager,lazy,artifact,durable] [--tasks read,log,repair] [--repeats 1] [--prepare-only]"
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

fn render(event: &Event) {
    match event.kind.as_str() {
        "model.response" => println!("● {}", event.payload.get("action").unwrap_or(&json!(null))),
        "operation.succeeded" => println!(
            "✓ operation {} artifact {}",
            event.payload["id"], event.payload["artifact"]
        ),
        "operation.failed" | "operation.outcome_unknown" | "action.rejected" | "model.failed" => {
            println!("! {} {}", event.kind, event.payload)
        }
        "run.completed" | "run.waiting_recovery" | "run.cancelled" => {
            println!("✓ {} {}", event.kind, event.payload)
        }
        "capability.search" => println!("● discovered {}", event.payload["matches"]),
        _ => {}
    }
}

fn attach(root: &Path, id: &str) -> Result<()> {
    uuid::Uuid::parse_str(id).context("invalid run ID")?;
    let mut last = 0;
    let started = Instant::now();
    loop {
        let store = Store::open(root)?;
        for event in store.events_since(id, last)? {
            render(&event);
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
    let mut context_chars = 256_000_u64;
    let mut programs = Vec::new();
    let mut image = None;
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
            "--allow-process" => {
                index += 1;
                programs.push(
                    args.get(index)
                        .context("--allow-process needs a program")?
                        .clone(),
                );
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
            argument if argument.starts_with('-') => bail!("unknown option: {argument}"),
            argument => task.push(argument),
        }
        index += 1;
    }
    if task.is_empty() {
        bail!("task is required");
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
    let run = store.create_run(
        &task.join(" "),
        &std::env::current_dir()?,
        provider,
        json!(grants),
        json!({"model": model, "endpoint":endpoint, "mode": mode, "actions": actions, "model_tokens": model_tokens, "wall_seconds": wall_seconds, "context_chars": context_chars,
            "model_seconds": 180, "process_seconds": 60, "container_image": image}),
        "Provide evidence from successful operations",
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

fn main() -> Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let root = root()?;
    let required =
        |index: usize| -> Result<&str> { Ok(arguments.get(index).context("missing argument")?) };
    match arguments.first().map(String::as_str) {
        None => interactive(&root),
        Some("run") => run(&root, &arguments[1..]),
        Some("eval") => arun::evaluation::command(&root, &arguments[1..]),
        Some("serve") => kernel::drive(Path::new(required(1)?), required(2)?),
        Some("worker") => {
            let value = arun::worker::execute(Path::new(required(1)?), required(2)?)?;
            println!("{value}");
            Ok(())
        }
        Some("mcp") => {
            if required(1)? != "add" {
                bail!("use 'arun mcp add <name> <command> [args...]'");
            }
            let name = required(2)?;
            if name.is_empty()
                || !name.chars().all(|character| {
                    character.is_ascii_alphanumeric() || character == '_' || character == '-'
                })
            {
                bail!("MCP server name must be alphanumeric, underscore, or hyphen");
            }
            let server = arun::mcp::Server {
                name: name.into(),
                command: required(3)?.into(),
                args: arguments.get(4..).unwrap_or_default().to_vec(),
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
