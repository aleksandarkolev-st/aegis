use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rmcp::{
    ServiceExt,
    model::CallToolRequestParams,
    transport::{ConfigureCommandExt, TokioChildProcess},
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tokio::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Server {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
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
    runtime()?.block_on(async {
        tokio::time::timeout(Duration::from_secs(20), async {
            let command = Command::new(&server.command).configure(|command| {
                command
                    .args(&server.args)
                    .current_dir(workspace)
                    .kill_on_drop(true);
            });
            let client = ().serve(TokioChildProcess::new(command)?).await?;
            let tools = client.list_all_tools().await?;
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
                    let digest =
                        Sha256::digest(format!("{}{}", description, input_schema).as_bytes());
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
            Ok(result)
        })
        .await
        .context("MCP discovery timed out")?
    })
}

pub fn call(server: &Server, workspace: &Path, name: &str, arguments: Value) -> Result<Value> {
    let arguments: Map<String, Value> = arguments
        .as_object()
        .context("MCP arguments must be an object")?
        .clone();
    runtime()?.block_on(async {
        tokio::time::timeout(Duration::from_secs(30), async {
            let command = Command::new(&server.command).configure(|command| {
                command
                    .args(&server.args)
                    .current_dir(workspace)
                    .kill_on_drop(true);
            });
            let client = ().serve(TokioChildProcess::new(command)?).await?;
            let tools = client.list_all_tools().await?;
            if !tools.iter().any(|tool| tool.name == name) {
                bail!("MCP server does not expose this tool");
            }
            let result = client
                .call_tool(CallToolRequestParams::new(name.to_owned()).with_arguments(arguments))
                .await?;
            client.cancel().await?;
            serde_json::to_value(result).map_err(Into::into)
        })
        .await
        .context("MCP tool call timed out")?
    })
}
