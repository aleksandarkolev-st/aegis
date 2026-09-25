use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::storage::Store;
use crate::{capability, mcp};

const MAX_FILE: u64 = 2 * 1024 * 1024;
const MAX_OUTPUT: usize = 1024 * 1024;

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
    let store = Store::open(root)?;
    let operation = store.operation(operation_id)?;
    if operation.state != "dispatched" {
        bail!("operation is not dispatched");
    }
    let run = store.run(&operation.run_id)?;
    if run.state != "running" {
        bail!("run is not active");
    }
    let grants: Vec<String> = serde_json::from_value(run.grants)?;
    let manifest = capability::permitted(&store, &operation.capability, &grants)?
        .context("capability not granted")?;
    if manifest.version != operation.capability_version {
        bail!("capability version changed after intent was recorded");
    }
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
                    .any(|grant| grant == &format!("process:{program}"))
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
            let output = Command::new(program)
                .args(arguments)
                .current_dir(workspace)
                .output()
                .with_context(|| format!("execute {program}"))?;
            let mut combined = output.stdout;
            combined.extend_from_slice(&output.stderr);
            let truncated = combined.len() > MAX_OUTPUT;
            combined.truncate(MAX_OUTPUT);
            Ok(
                json!({"exit_code": output.status.code(), "output": String::from_utf8_lossy(&combined), "truncated": truncated}),
            )
        }
        name if name.starts_with("mcp.") => {
            let (server_name, tool_name) = name
                .trim_start_matches("mcp.")
                .split_once('.')
                .context("invalid MCP capability")?;
            let server = store.mcp_server(server_name)?;
            mcp::call(&server, workspace, tool_name, args.clone())
        }
        _ => bail!("unknown capability"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
