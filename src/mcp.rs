//! MCP transport/discovery boundary.
//!
//! This module owns SDK-specific connection details. Tool bridging into the
//! agent registry will be added after transport compatibility is verified.

use anyhow::Result;
use rmcp::model::Tool;
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use std::sync::Arc;
use tokio::sync::Mutex;

pub enum Connection {
    Stdio(Arc<Mutex<RunningService<RoleClient, ()>>>),
}

pub async fn connect_stdio(command: &str, args: &[String]) -> Result<Connection> {
    use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
    use tokio::process::Command;
    let transport = TokioChildProcess::new(Command::new(command).configure(|cmd| {
        cmd.args(args);
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
    }
}
