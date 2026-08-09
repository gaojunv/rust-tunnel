//! Glue between the agent loop and the tunnel: workspace lock + registry.agent_exec.
use super::AgentState;
use rust_tunnel_common::{AgentCommand, AgentResult};

/// Execute a command on the workspace's client, serialized per workspace.
/// Never errors at the Rust level: transport failures become AgentResult::Error.
/// request_id 由 AgentState 预生成并记入 inflight，供 WS cancel 分支下发真取消。
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
    let request_id = agent.inflight_begin(workspace_id).await;
    let result = agent
        .registry
        .agent_exec(
            client_id,
            &request_id,
            workspace_id,
            root_path,
            docker_container,
            command,
            std::time::Duration::from_secs(120),
        )
        .await;
    // 无论成败都清 inflight，避免 stale 条目。
    agent.inflight_end(workspace_id).await;
    match result {
        Ok(result) => result,
        Err(e) => AgentResult::Error {
            message: format!("tunnel execution failed: {e}"),
        },
    }
}
