use std::path::Path;
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, PaginatedRequestParams},
    transport::ConfigureCommandExt,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, ReadBuf};
use tokio::process::Command;

struct BoundedRead<Reader> {
    inner: Reader,
    line_bytes: usize,
}

impl<Reader: AsyncRead + Unpin> AsyncRead for BoundedRead<Reader> {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let state = self.get_mut();
        let mut bytes = [0; 8192];
        let length = output.remaining().min(bytes.len());
        let mut buffer = ReadBuf::new(&mut bytes[..length]);
        match Pin::new(&mut state.inner).poll_read(context, &mut buffer) {
            Poll::Ready(Ok(())) => {
                for byte in buffer.filled() {
                    if *byte == b'\n' {
                        state.line_bytes = 0;
                    } else {
                        state.line_bytes += 1;
                        if state.line_bytes > 32 * 1024 * 1024 {
                            return Poll::Ready(Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "MCP message exceeds 32 MiB",
                            )));
                        }
                    }
                }
                output.put_slice(buffer.filled());
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

fn transport(
    mut command: Command,
) -> Result<(
    tokio::process::Child,
    (
        BoundedRead<tokio::process::ChildStdout>,
        tokio::process::ChildStdin,
    ),
)> {
    let mut child = command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let output = child.stdout.take().context("MCP stdout missing")?;
    let input = child.stdin.take().context("MCP stdin missing")?;
    Ok((
        child,
        (
            BoundedRead {
                inner: output,
                line_bytes: 0,
            },
            input,
        ),
    ))
}

async fn tools(peer: &rmcp::service::Peer<rmcp::RoleClient>) -> Result<Vec<rmcp::model::Tool>> {
    let mut tools = Vec::new();
    let mut cursor = None;
    let mut cursors = std::collections::BTreeSet::new();
    let mut bytes = 0;
    for _ in 0..100 {
        let page = peer
            .list_tools(Some(PaginatedRequestParams::default().with_cursor(cursor)))
            .await?;
        bytes += serde_json::to_vec(&page)?.len();
        if bytes > 16 * 1024 * 1024 || tools.len() + page.tools.len() > 8192 {
            bail!("MCP registry exceeds limits");
        }
        tools.extend(page.tools);
        cursor = page.next_cursor;
        match &cursor {
            None => return Ok(tools),
            Some(cursor) if !cursors.insert(cursor.clone()) => {
                bail!("MCP pagination cursor repeated")
            }
            _ => {}
        }
    }
    bail!("MCP pagination exceeds 100 pages")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Server {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    #[serde(default)]
    pub policy: Policy,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub image: Option<String>,
    #[serde(default)]
    pub write: bool,
    #[serde(default)]
    pub trusted_host: bool,
}

pub fn validate(server: &Server) -> Result<()> {
    if server.command.is_empty() || server.command.starts_with('-') || server.command.contains('\0')
    {
        bail!("invalid MCP executable");
    }
    if let Some(image) = &server.policy.image {
        if server.policy.trusted_host
            || image.is_empty()
            || image.len() > 256
            || image.starts_with('-')
            || !image
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._/:@-".contains(&byte))
        {
            bail!("invalid MCP container policy");
        }
    } else if !server.policy.trusted_host || server.policy.write {
        bail!("MCP requires an isolated image or explicit trusted-host authorization");
    }
    Ok(())
}

struct Container {
    name: String,
    started: bool,
}

impl Drop for Container {
    fn drop(&mut self) {
        if !self.started {
            return;
        }
        let mut command = std::process::Command::new("docker");
        command
            .args(["rm", "-f", &self.name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if let Ok(mut child) = crate::process::spawn(command) {
            let started = std::time::Instant::now();
            while started.elapsed() < Duration::from_secs(5) {
                if child.try_wait().ok().flatten().is_some() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn command(
    server: &Server,
    workspace: &Path,
    allow_write: bool,
    operation_id: Option<&str>,
) -> Result<(Command, Option<Container>)> {
    validate(server)?;
    if let Some(image) = &server.policy.image {
        let workspace = dunce::canonicalize(workspace)?;
        let source = dunce::simplified(&workspace).to_string_lossy();
        if source.contains(',') {
            bail!("Docker workspace paths cannot contain commas");
        }
        let id = match operation_id {
            Some(id) => uuid::Uuid::parse_str(id)?,
            None => uuid::Uuid::new_v4(),
        };
        let container = Container {
            name: format!("arun-mcp-{id}"),
            started: false,
        };
        let mount = format!(
            "type=bind,source={source},target=/workspace{}",
            if allow_write && server.policy.write {
                ""
            } else {
                ",readonly"
            }
        );
        let mut command = Command::new("docker");
        command.args([
            "run",
            "--rm",
            "--interactive",
            "--pull=never",
            "--name",
            &container.name,
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
            "--env",
            "HOME=/tmp",
        ]);
        crate::worker::mask_metadata(command.as_std_mut(), &workspace)?;
        crate::process::background(command.as_std_mut());
        command.args(["--entrypoint", &server.command, image]);
        command.args(&server.args).kill_on_drop(true);
        Ok((command, Some(container)))
    } else {
        let command = Command::new(&server.command).configure(|command| {
            command
                .args(&server.args)
                .current_dir(workspace)
                .kill_on_drop(true);
            crate::process::background(command.as_std_mut());
        });
        Ok((command, None))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    pub server: String,
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub version: u32,
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?)
}

pub fn discover(server: &Server, workspace: &Path) -> Result<Vec<Tool>> {
    let (command, mut container) = command(server, workspace, false, None)?;
    if let Some(container) = &mut container {
        container.started = true;
    }
    runtime()?.block_on(async {
        tokio::time::timeout(Duration::from_secs(20), async {
            let (mut child, transport) = transport(command)?;
            let client = ().serve(transport).await?;
            let tools = tools(client.peer()).await?;
            let result = tools
                .into_iter()
                .map(|tool| {
                    let input_schema =
                        serde_json::to_value(&tool.input_schema).unwrap_or(Value::Null);
                    let description = tool
                        .description
                        .as_deref()
                        .unwrap_or_default()
                        .chars()
                        .take(500)
                        .collect::<String>();
                    let digest = Sha256::digest(
                        format!(
                            "{}{}{}",
                            description,
                            input_schema,
                            serde_json::to_string(server).expect("serializable MCP server")
                        )
                        .as_bytes(),
                    );
                    Tool {
                        server: server.name.clone(),
                        name: tool.name.to_string(),
                        description,
                        input_schema,
                        version: u32::from_be_bytes(digest[..4].try_into().unwrap()),
                    }
                })
                .collect();
            client.cancel().await?;
            let _ = child.kill().await;
            Ok(result)
        })
        .await
        .context("MCP discovery timed out")?
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_reader_rejects_overflow_before_forwarding_bytes() -> Result<()> {
        use tokio::io::AsyncReadExt;
        runtime()?.block_on(async {
            let mut reader = BoundedRead {
                inner: std::io::Cursor::new(b"abc"),
                line_bytes: 32 * 1024 * 1024 - 1,
            };
            let mut output = [0; 16];
            let error = reader.read(&mut output).await.unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
            assert_eq!(output, [0; 16]);
            Ok(())
        })
    }

    #[test]
    fn message_reader_resets_its_limit_at_each_newline() -> Result<()> {
        use tokio::io::AsyncReadExt;
        runtime()?.block_on(async {
            let mut reader = BoundedRead {
                inner: std::io::Cursor::new(b"a\nb\n"),
                line_bytes: 32 * 1024 * 1024 - 1,
            };
            let mut output = Vec::new();
            reader.read_to_end(&mut output).await?;
            assert_eq!(output, b"a\nb\n");
            assert_eq!(reader.line_bytes, 0);
            Ok(())
        })
    }

    #[test]
    fn host_execution_is_not_implicitly_trusted() {
        let mut server = Server {
            name: "fixture".into(),
            command: "node".into(),
            args: vec![],
            policy: Policy::default(),
        };
        assert!(validate(&server).is_err());
        server.policy.trusted_host = true;
        assert!(validate(&server).is_ok());
        server.policy.image = Some("node:22-alpine".into());
        assert!(validate(&server).is_err());
        server.policy.trusted_host = false;
        assert!(validate(&server).is_ok());
    }

    #[test]
    fn container_commands_do_not_accept_server_annotations_as_policy() -> Result<()> {
        let directory = tempfile::tempdir()?;
        std::fs::create_dir(directory.path().join(".arun"))?;
        std::fs::create_dir(directory.path().join(".git"))?;
        let server = Server {
            name: "fixture".into(),
            command: "node".into(),
            args: vec!["server.mjs".into()],
            policy: Policy {
                image: Some("node:22-alpine".into()),
                write: true,
                trusted_host: false,
            },
        };
        for allow_write in [false, true] {
            let (command, _container) = command(&server, directory.path(), allow_write, None)?;
            let args: Vec<_> = command
                .as_std()
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();
            assert!(args.windows(2).any(|pair| pair == ["--network", "none"]));
            assert!(
                args.iter()
                    .any(|arg| arg == "/workspace/.git:rw,noexec,size=1m")
            );
            assert!(
                args.iter()
                    .any(|arg| arg == "/workspace/.arun:rw,noexec,size=1m")
            );
            let mount = args
                .iter()
                .find(|arg| arg.starts_with("type=bind,"))
                .unwrap();
            assert_eq!(mount.ends_with(",readonly"), !allow_write);
            assert!(args.iter().any(|arg| arg == "--pull=never"));
        }
        Ok(())
    }
}

pub fn call(server: &Server, workspace: &Path, name: &str, arguments: Value) -> Result<Value> {
    call_with_access(server, workspace, name, arguments, false, None)
}

pub(crate) fn call_with_access(
    server: &Server,
    workspace: &Path,
    name: &str,
    arguments: Value,
    allow_write: bool,
    operation_id: Option<&str>,
) -> Result<Value> {
    let (command, mut container) = command(server, workspace, allow_write, operation_id)?;
    if let Some(container) = &mut container {
        container.started = true;
    }
    let arguments: Map<String, Value> = arguments
        .as_object()
        .context("MCP arguments must be an object")?
        .clone();
    runtime()?.block_on(async {
        tokio::time::timeout(Duration::from_secs(30), async {
            let (mut child, transport) = transport(command)?;
            let client = ().serve(transport).await?;
            let tools = tools(client.peer()).await?;
            if !tools.iter().any(|tool| tool.name == name) {
                bail!("MCP server does not expose this tool");
            }
            let result = client
                .call_tool(CallToolRequestParams::new(name.to_owned()).with_arguments(arguments))
                .await?;
            client.cancel().await?;
            let _ = child.kill().await;
            serde_json::to_value(result).map_err(Into::into)
        })
        .await
        .context("MCP tool call timed out")?
    })
}
