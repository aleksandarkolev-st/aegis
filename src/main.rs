use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use arun::kernel;
use arun::storage::{Event, Store};
use serde_json::json;

fn root() -> Result<PathBuf> {
    Ok(std::env::current_dir()?.join(".arun"))
}

fn usage() {
    println!(
        "arun [run <task> [--provider chatgpt|claude|grok] [--allow-write] [--allow-process <program>] [--actions <limit>] [--foreground]]"
    );
    println!(
        "arun attach|status|cancel|replay <run-id> | list | inspect <artifact-hash> | login|probe <provider>"
    );
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
    let mut last = 0;
    loop {
        let store = Store::open(root)?;
        for event in store
            .events(id)?
            .iter()
            .filter(|event| event.seq > last)
            .cloned()
            .collect::<Vec<_>>()
        {
            render(&event);
            last = event.seq;
        }
        let run = store.run(id)?;
        if run.state != "running" && run.state != "ready" {
            println!("run {}: {}", id, run.state);
            break;
        }
        thread::sleep(Duration::from_millis(500));
    }
    Ok(())
}

fn spawn(root: &Path, id: &str) -> Result<()> {
    let log = File::create(root.join(format!("daemon-{id}.log")))?;
    let error = log.try_clone()?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("serve")
        .arg(root)
        .arg(id)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(error));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x00000008 | 0x00000200);
    }
    command.spawn().context("start detached run")?;
    Ok(())
}

fn run(root: &Path, args: &[String]) -> Result<()> {
    let mut provider = "codex";
    let mut write = false;
    let mut foreground = false;
    let mut actions = 40_u64;
    let mut programs = Vec::new();
    let mut task = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--provider" => {
                index += 1;
                provider = args.get(index).context("--provider needs a value")?;
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
            argument if argument.starts_with('-') => bail!("unknown option: {argument}"),
            argument => task.push(argument),
        }
        index += 1;
    }
    if task.is_empty() {
        bail!("task is required");
    }
    let provider = match provider {
        "chatgpt" | "codex" => "codex",
        "claude-code" | "claude" => "claude",
        "grok" => "grok",
        _ => bail!("choose chatgpt, claude, or grok"),
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
    let mut store = Store::open(root)?;
    let run = store.create_run(
        &task.join(" "),
        &std::env::current_dir()?,
        provider,
        json!(grants),
        json!({"actions": actions, "model_seconds": 180, "process_seconds": 60}),
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
    println!(
        "Agent Runtime — ChatGPT login via Codex CLI (default). Type /help or Ctrl-D to detach."
    );
    loop {
        print!("> ");
        io::stdout().flush()?;
        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line == "/help" {
            usage();
            continue;
        }
        if line == "/status" {
            for run in Store::open(root)?.runs()? {
                println!("{} {} {}", run.id, run.state, run.task);
            }
            continue;
        }
        if line == "/cancel" {
            println!("Use 'arun cancel <run-id>'");
            continue;
        }
        if line.starts_with('/') {
            println!("Unknown command. Try /help");
            continue;
        }
        run(root, &[line.to_owned()])?;
    }
    Ok(())
}

fn login(provider: &str) -> Result<()> {
    let (program, arguments): (&str, &[&str]) = match provider {
        "chatgpt" | "codex" => (
            if cfg!(windows) { "codex.cmd" } else { "codex" },
            &["login"],
        ),
        "claude" | "claude-code" => ("claude", &["auth", "login"]),
        "grok" => ("grok", &["login"]),
        _ => bail!("choose chatgpt, claude, or grok"),
    };
    let status = Command::new(program).args(arguments).status()?;
    if !status.success() {
        bail!("login exited with {status}");
    }
    Ok(())
}

fn main() -> Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let root = root()?;
    let required =
        |index: usize| -> Result<&str> { Ok(arguments.get(index).context("missing argument")?) };
    match arguments.first().map(String::as_str) {
        None => interactive(&root),
        Some("run") => run(&root, &arguments[1..]),
        Some("serve") => kernel::drive(Path::new(required(1)?), required(2)?),
        Some("worker") => {
            let value = arun::worker::execute(Path::new(required(1)?), required(2)?)?;
            println!("{value}");
            Ok(())
        }
        Some("attach") => attach(&root, required(1)?),
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
        Some("replay") | Some("trace") => {
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
