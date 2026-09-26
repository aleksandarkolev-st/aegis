use std::fs::{self, File};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::storage::{Operation, Run, Store};
use crate::{capability, mcp};

const MAX_FILE: u64 = 2 * 1024 * 1024;
const MAX_OUTPUT: u64 = 32 * 1024 * 1024;

pub fn container_name(operation_id: &str) -> Result<String> {
    uuid::Uuid::parse_str(operation_id).context("invalid operation ID")?;
    Ok(format!("arun-{operation_id}"))
}

fn docker_command(
    run: &Run,
    operation: &Operation,
    program: &str,
    arguments: &[String],
) -> Result<Command> {
    let image = run
        .budgets
        .get("container_image")
        .and_then(Value::as_str)
        .context("process.run requires a container image")?;
    if image.is_empty() || image.starts_with('-') {
        bail!("invalid container image");
    }
    let source = dunce::simplified(Path::new(&run.workspace)).to_string_lossy();
    if source.contains(',') {
        bail!("Docker bind mount cannot contain a comma");
    }
    let grants: Vec<String> = serde_json::from_value(run.grants.clone())?;
    let mount = format!(
        "type=bind,source={},target=/workspace{}",
        source,
        if grants.iter().any(|grant| grant == "workspace.write") {
            ""
        } else {
            ",readonly"
        }
    );
    let mut command = Command::new("docker");
    command.args([
        "run",
        "--rm",
        "--pull=never",
        "--name",
        &container_name(&operation.id)?,
        "--network",
        "none",
        "--read-only",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--pids-limit",
        "128",
        "--memory",
        "1g",
        "--cpus",
        "2",
        "--workdir",
        "/workspace",
        "--mount",
        &mount,
        "--tmpfs",
        "/tmp:rw,size=256m",
        "--tmpfs",
        "/workspace/.arun:rw,noexec,size=1m",
        "--env",
        "HOME=/tmp",
        "--env",
        "CARGO_TARGET_DIR=/tmp/target",
        "--entrypoint",
        program,
        image,
    ]);
    command.args(arguments);
    Ok(command)
}

fn relative(workspace: &Path, path: &str) -> Result<PathBuf> {
    let relative = Path::new(path);
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("path must be relative and cannot traverse directories");
    }
    if relative.components().any(|component| {
        let name = component.as_os_str().to_string_lossy();
        name.eq_ignore_ascii_case(".git") || name.eq_ignore_ascii_case(".arun")
    }) {
        bail!("runtime and Git metadata are not workspace files");
    }
    let target = workspace.join(relative);
    let existing = if target.exists() {
        &target
    } else {
        target.parent().context("missing parent")?
    };
    if !existing
        .canonicalize()?
        .starts_with(workspace.canonicalize()?)
    {
        bail!("path escapes workspace through symlink");
    }
    Ok(target)
}

fn string<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("missing string argument: {key}"))
}

pub fn execute(root: &Path, operation_id: &str) -> Result<Value> {
    uuid::Uuid::parse_str(operation_id).context("invalid operation ID")?;
    let lock = File::options()
        .write(true)
        .create(true)
        .open(root.join(format!("operation-{operation_id}.lock")))?;
    fs2::FileExt::try_lock_exclusive(&lock)
        .context("operation already executing in another worker")?;
    let mut store = Store::open(root)?;
    let operation = store.operation(operation_id)?;
    if operation.state != "dispatched" {
        bail!("operation is not dispatched");
    }
    let run = store.run(&operation.run_id)?;
    if run.state != "running" {
        bail!("run is not active");
    }
    if crate::kernel::remaining_seconds(&store, &run)? == 0 {
        bail!("task wall-clock budget exhausted before worker claim");
    }
    if store.interrupt_requested(&run.id, crate::interrupt::Scope::Operation, &operation.id)? {
        bail!("operation interrupted before worker claim");
    }
    if operation.capability == crate::acceptance::CAPABILITY {
        let check = crate::acceptance::authorized_check(&store, &run, &operation)?;
        store.claim_operation(&operation)?;
        let mut isolated = run.clone();
        isolated.grants = json!([]);
        isolated.budgets["container_image"] = json!(check.image);
        isolated.budgets["process_seconds"] = json!(check.seconds);
        return execute_container(
            &mut store,
            root,
            &isolated,
            &operation,
            &check.program,
            &check.args,
        );
    }
    let grants: Vec<String> = serde_json::from_value(run.grants.clone())?;
    let manifest = capability::permitted(&store, &operation.capability, &grants)?
        .context("capability not granted")?;
    if manifest.version != operation.capability_version {
        bail!("capability version changed after intent was recorded");
    }
    capability::validate_arguments(&manifest, &operation.arguments)?;
    store.claim_operation(&operation)?;
    let workspace = Path::new(&run.workspace);
    let args = &operation.arguments;
    match operation.capability.as_str() {
        "workspace.read" => {
            let path = relative(workspace, string(args, "path")?)?;
            if fs::metadata(&path)?.len() > MAX_FILE {
                bail!("file too large; use search");
            }
            Ok(json!({"content": fs::read_to_string(path)?}))
        }
        "workspace.search" => {
            let query = string(args, "query")?;
            if query.is_empty() {
                bail!("empty search query");
            }
            let mut pending = vec![workspace.to_path_buf()];
            let mut matches = Vec::new();
            while let Some(directory) = pending.pop() {
                for entry in fs::read_dir(directory)? {
                    let entry = entry?;
                    let name = entry.file_name();
                    if name == ".git" || name == ".arun" || name == "target" {
                        continue;
                    }
                    let kind = entry.file_type()?;
                    if kind.is_dir() {
                        pending.push(entry.path());
                    } else if kind.is_file() && entry.metadata()?.len() <= MAX_FILE {
                        if let Ok(text) = fs::read_to_string(entry.path()) {
                            for (line_number, line) in text.lines().enumerate() {
                                if line.contains(query) {
                                    let path = entry
                                        .path()
                                        .strip_prefix(workspace)?
                                        .to_string_lossy()
                                        .into_owned();
                                    matches.push(json!({"path": path, "line": line_number + 1, "text": line.chars().take(300).collect::<String>()}));
                                    if matches.len() >= 100 {
                                        return Ok(json!({"matches": matches, "truncated": true}));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Ok(json!({"matches": matches, "truncated": false}))
        }
        "workspace.write" => {
            let path = relative(workspace, string(args, "path")?)?;
            let content = string(args, "content")?;
            if content.len() > MAX_FILE as usize {
                bail!("write exceeds file limit");
            }
            fs::write(&path, content)?;
            Ok(
                json!({"path": path.strip_prefix(workspace)?.to_string_lossy(), "bytes": content.len()}),
            )
        }
        "process.run" => {
            let program = string(args, "program")?;
            if program.is_empty()
                || program
                    .bytes()
                    .any(|byte| byte == b'/' || byte == 92 || byte == b':')
                || !grants
                    .iter()
                    .any(|grant| grant == &format!("process:{program}") || grant == "process:*")
            {
                bail!("program is not explicitly granted");
            }
            let arguments = args
                .get("args")
                .and_then(Value::as_array)
                .context("missing args array")?
                .iter()
                .map(|argument| {
                    argument
                        .as_str()
                        .map(str::to_owned)
                        .context("process argument must be a string")
                })
                .collect::<Result<Vec<_>>>()?;
            execute_container(&mut store, root, &run, &operation, program, &arguments)
        }
        name if name.starts_with("mcp.") => {
            let (server_name, tool_name) = name
                .trim_start_matches("mcp.")
                .split_once('.')
                .context("invalid MCP capability")?;
            let server = store.mcp_server(server_name)?;
            let allow_write = run.grants.as_array().is_some_and(|grants| {
                grants
                    .iter()
                    .any(|grant| grant.as_str() == Some("workspace.write"))
            });
            mcp::call_with_access(
                &server,
                workspace,
                tool_name,
                args.clone(),
                allow_write,
                Some(&operation.id),
            )
        }
        _ => bail!("unknown capability"),
    }
}

fn execute_container(
    store: &mut Store,
    root: &Path,
    run: &Run,
    operation: &Operation,
    program: &str,
    arguments: &[String],
) -> Result<Value> {
    let seconds = run.budgets["process_seconds"]
        .as_u64()
        .unwrap_or(60)
        .min(crate::kernel::remaining_seconds(store, run)?);
    if seconds == 0 {
        bail!("process deadline exhausted before launch");
    }
    let output_dir = tempfile::tempdir_in(root)?;
    let path = output_dir.path().join("process-output");
    let file = File::create(&path)?;
    let mut command = docker_command(run, operation, program, arguments)?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(file.try_clone()?))
        .stderr(Stdio::from(file));
    let mut child = crate::process::spawn(command)
        .context("start Docker; ensure its daemon and local image are available")?;
    let started = Instant::now();
    let deadline = Duration::from_secs(seconds);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        let oversized = fs::metadata(&path)?.len() > MAX_OUTPUT;
        let current = Store::open(root)?;
        let cancelled = current.run(&run.id)?.state != "running";
        let interrupted = current.interrupt_requested(
            &run.id,
            crate::interrupt::Scope::Operation,
            &operation.id,
        )?;
        if oversized || cancelled || interrupted || started.elapsed() >= deadline {
            child.kill()?;
            crate::kernel::cleanup_container(operation);
            if interrupted {
                bail!("operation interrupted by user");
            }
            bail!("container output limit, cancellation, or deadline reached");
        }
        thread::sleep(Duration::from_millis(100));
    };
    let bytes = fs::read(&path)?;
    if bytes.len() as u64 > MAX_OUTPUT {
        bail!("process output exceeded 32 MiB")
    }
    let hash = store.put_artifact(&bytes)?;
    store.link_artifact(&operation.id, &hash, "process.output")?;
    Ok(
        json!({"exit_code":status.code(), "output_artifact":hash, "bytes":bytes.len(), "preview":String::from_utf8_lossy(&bytes).chars().take(300).collect::<String>()}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exhausted_tasks_cannot_claim_or_execute_a_dispatched_write() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join(".arun");
        let mut store = Store::open(&root)?;
        let run = store.create_run(
            "write",
            directory.path(),
            "codex",
            json!(["workspace.write"]),
            json!({"wall_seconds":0}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let operation = store.begin_operation(
            &run.id,
            "workspace.write",
            json!({"path":"fixture.txt","content":"must not execute"}),
            false,
        )?;
        store.operation_state(&operation, "dispatched", None, json!({}))?;
        assert!(
            execute(&root, &operation.id)
                .unwrap_err()
                .to_string()
                .contains("budget exhausted")
        );
        assert_eq!(store.operation(&operation.id)?.state, "dispatched");
        assert!(!directory.path().join("fixture.txt").exists());
        Ok(())
    }

    #[test]
    fn adapter_cannot_repeat_a_claimed_write_before_result_commit() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join(".arun");
        let target = directory.path().join("fixture.txt");
        fs::write(&target, "original")?;
        let mut store = Store::open(&root)?;
        let run = store.create_run(
            "write",
            directory.path(),
            "codex",
            json!(["workspace.write"]),
            json!({}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let operation = store.begin_operation(
            &run.id,
            "workspace.write",
            json!({"path":"fixture.txt","content":"written once"}),
            false,
        )?;
        store.operation_state(&operation, "dispatched", None, json!({}))?;
        execute(&root, &operation.id)?;
        assert_eq!(store.operation(&operation.id)?.state, "executing");
        fs::write(&target, "later external edit")?;
        assert!(execute(&root, &operation.id).is_err());
        assert_eq!(fs::read_to_string(target)?, "later external edit");
        assert_eq!(store.event_count(&run.id, "operation.executing")?, 1);
        store.reconcile(&run.id)?;
        assert_eq!(store.operation(&operation.id)?.state, "outcome_unknown");
        assert_eq!(store.run(&run.id)?.state, "waiting_recovery");
        Ok(())
    }

    #[test]
    fn worker_revalidates_schema_before_claiming_an_operation() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join(".arun");
        let mut store = Store::open(&root)?;
        let run = store.create_run(
            "write",
            directory.path(),
            "codex",
            json!(["workspace.write"]),
            json!({}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let operation = store.begin_operation(
            &run.id,
            "workspace.write",
            json!({"path":"fixture.txt","content":7}),
            false,
        )?;
        store.operation_state(&operation, "dispatched", None, json!({}))?;
        assert!(
            execute(&root, &operation.id)
                .unwrap_err()
                .to_string()
                .starts_with("invalid arguments")
        );
        assert_eq!(store.operation(&operation.id)?.state, "dispatched");
        assert!(!directory.path().join("fixture.txt").exists());
        Ok(())
    }

    #[test]
    fn rejects_traversal_and_internal_metadata() -> Result<()> {
        let directory = tempfile::tempdir()?;
        assert!(relative(directory.path(), "../outside").is_err());
        assert!(relative(directory.path(), ".git/config").is_err());
        assert!(relative(directory.path(), "C:/outside").is_err());
        assert!(relative(directory.path(), "safe.txt").is_ok());
        Ok(())
    }

    #[test]
    fn worker_checks_recorded_grants_not_model_claims() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join(".arun");
        let mut store = Store::open(&root)?;
        let run = store.create_run(
            "test",
            directory.path(),
            "claude",
            json!(["workspace.read"]),
            json!({}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let operation = store.begin_operation(
            &run.id,
            "workspace.write",
            json!({"path":"a.txt","content":"bad"}),
            false,
        )?;
        store.operation_state(&operation, "dispatched", None, json!({}))?;
        assert!(execute(&root, &operation.id).is_err());
        assert!(!directory.path().join("a.txt").exists());
        Ok(())
    }

    #[test]
    fn process_command_is_containerized_without_network() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "test",
            directory.path(),
            "codex",
            json!(["workspace.read", "process.run", "process:cargo"]),
            json!({"container_image":"rust:1.98"}),
            "",
        )?;
        let operation = store.begin_operation(
            &run.id,
            "process.run",
            json!({"program":"cargo","args":["test"]}),
            false,
        )?;
        let args = docker_command(&run, &operation, "cargo", &["test".into()])?
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(args.windows(2).any(|pair| pair == ["--network", "none"]));
        assert!(args.iter().any(|argument| argument == "--pull=never"));
        assert!(args.iter().any(|argument| argument.ends_with(",readonly")));
        assert!(
            args.iter()
                .any(|argument| argument.starts_with("/workspace/.arun:"))
        );
        assert_eq!(args.last().unwrap(), "test");
        Ok(())
    }
}
