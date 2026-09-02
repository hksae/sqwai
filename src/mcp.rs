//! MCP transport, discovery, and async tool invocation.

use anyhow::{Result, bail};
use rmcp::model::{CallToolRequestParams, CallToolResponse, Tool};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

pub enum Connection {
    Stdio(Arc<Mutex<RunningService<RoleClient, ()>>>),
}

pub struct Registry {
    tools: Vec<crate::providers::ToolSpec>,
    connections: HashMap<String, Connection>,
}

impl Registry {
    pub async fn from_config(config: &crate::config::McpConfig) -> Result<Self> {
        let mut registry = Self {
            tools: Vec::new(),
            connections: HashMap::new(),
        };
        for server in config.servers.iter().filter(|server| server.enabled) {
            let connection = match &server.transport {
                crate::config::McpTransport::Stdio { command, args, env } => {
                    connect_stdio(command, args, env).await?
                }
                crate::config::McpTransport::Http { .. } => {
                    bail!(
                        "MCP server '{}' uses HTTP, which is not supported yet",
                        server.name
                    )
                }
            };
            let discovered = list_tools(&connection).await?;
            registry.tools.extend(specs(&server.name, &discovered));
            registry.connections.insert(server.name.clone(), connection);
        }
        Ok(registry)
    }

    pub fn specs(&self) -> &[crate::providers::ToolSpec] {
        &self.tools
    }

    pub fn contains(&self, name: &str) -> bool {
        split_namespaced(name).is_some_and(|(server, _)| self.connections.contains_key(server))
    }

    pub async fn call(
        &self,
        namespaced: &str,
        arguments: serde_json::Value,
    ) -> Result<(String, bool)> {
        let Some((server, tool)) = split_namespaced(namespaced) else {
            bail!("invalid MCP tool name '{namespaced}'")
        };
        let Some(connection) = self.connections.get(server) else {
            bail!("MCP server '{server}' is not connected")
        };
        let args = arguments.as_object().cloned().unwrap_or_default();
        let response = match connection {
            Connection::Stdio(client) => {
                client
                    .lock()
                    .await
                    .call_tool_once(
                        CallToolRequestParams::new(tool.to_string()).with_arguments(args),
                    )
                    .await?
            }
        };
        match response {
            CallToolResponse::Complete(result) => {
                let output = if let Some(structured) = result.structured_content {
                    serde_json::to_string_pretty(&structured)?
                } else {
                    serde_json::to_string(&result.content)?
                };
                Ok((output, result.is_error.unwrap_or(false)))
            }
            _ => bail!("MCP tool '{namespaced}' requires an unsupported follow-up interaction"),
        }
    }
}

fn split_namespaced(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix("mcp__")?;
    let (server, tool) = rest.split_once("__")?;
    (!server.is_empty() && !tool.is_empty()).then_some((server, tool))
}

pub async fn connect_stdio(
    command: &str,
    args: &[String],
    env: &std::collections::BTreeMap<String, String>,
) -> Result<Connection> {
    use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
    use tokio::process::Command;
    let transport = TokioChildProcess::new(Command::new(command).configure(|cmd| {
        cmd.args(args);
        cmd.envs(env);
    }))?;
    Ok(Connection::Stdio(Arc::new(Mutex::new(
        ().serve(transport).await?,
    ))))
}

pub async fn list_tools(connection: &Connection) -> Result<Vec<Tool>> {
    match connection {
        Connection::Stdio(client) => Ok(client.lock().await.list_all_tools().await?),
    }
}

pub fn specs(server: &str, tools: &[Tool]) -> Vec<crate::providers::ToolSpec> {
    tools
        .iter()
        .map(|tool| crate::providers::ToolSpec {
            name: namespaced_name(server, tool.name.as_ref()),
            description: tool
                .description
                .as_deref()
                .unwrap_or("MCP tool")
                .to_string(),
            parameters: serde_json::Value::Object((*tool.input_schema).clone()),
        })
        .collect()
}

pub fn namespaced_name(server: &str, tool: &str) -> String {
    format!("mcp__{server}__{tool}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_tools_to_provider_specs() {
        let tool = Tool::new(
            "issues",
            "list issues",
            serde_json::json!({"type":"object"})
                .as_object()
                .unwrap()
                .clone(),
        );
        let specs = specs("github", &[tool]);
        assert_eq!(specs[0].name, "mcp__github__issues");
        assert_eq!(specs[0].description, "list issues");
    }
    #[test]
    fn namespaces_server_tools() {
        assert_eq!(namespaced_name("github", "issues"), "mcp__github__issues");
        assert_eq!(
            split_namespaced("mcp__github__issues"),
            Some(("github", "issues"))
        );
    }
}
