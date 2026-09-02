//! MCP transport/discovery boundary.
//!
//! This module owns SDK-specific connection details. Tool bridging into the
//! agent registry will be added after transport compatibility is verified.

use anyhow::Result;
use rmcp::model::Tool;
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};

pub enum Connection {
    Stdio(RunningService<RoleClient, ()>),
}

pub async fn connect_stdio(command: &str, args: &[String]) -> Result<Connection> {
    use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
    use tokio::process::Command;
    let transport = TokioChildProcess::new(Command::new(command).configure(|cmd| {
        cmd.args(args);
    }))?;
    Ok(Connection::Stdio(().serve(transport).await?))
}

pub async fn list_tools(connection: &Connection) -> Result<Vec<Tool>> {
    match connection {
        Connection::Stdio(client) => Ok(client.list_all_tools().await?),
    }
}

pub fn namespaced_name(server: &str, tool: &str) -> String {
    format!("mcp__{server}__{tool}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespaces_server_tools() {
        assert_eq!(namespaced_name("github", "issues"), "mcp__github__issues");
    }
}
