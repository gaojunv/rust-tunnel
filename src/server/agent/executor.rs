//! Glue between the agent loop and the tunnel: workspace lock + registry.agent_exec.
use super::AgentState;
use crate::common::{AgentCommand, AgentResult};

/// Execute a command on the workspace's client, serialized per workspace.
/// Never errors at the Rust level: transport failures become AgentResult::Error.
pub async fn exec_on_client(
    agent: &AgentState,
    workspace_id: &str,
    client_id: &str,
    root_path: &str,
    docker_container: Option<&str>,
    command: AgentCommand,
) -> AgentResult {
    let lock = agent.workspace_lock(workspace_id).await;
    let _guard = lock.lock().await;
    match agent
        .registry
        .agent_exec(
            client_id,
            workspace_id,
            root_path,
            docker_container,
            command,
            std::time::Duration::from_secs(120),
        )
        .await
    {
        Ok(result) => result,
        Err(e) => AgentResult::Error {
            message: format!("tunnel execution failed: {e}"),
        },
    }
}
