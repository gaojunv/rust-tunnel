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
    // 版本门控：老客户端不支持 ShellWithTimeout → 降级为 Shell（120s 默认超时）
    let command = match &command {
        AgentCommand::ShellWithTimeout {
            cmd,
            cwd,
            timeout_secs,
        } => {
            let version = agent
                .registry
                .get(client_id)
                .await
                .and_then(|e| e.client_version.clone());
            if super::runner::client_supports_shell_timeout(version.as_deref()) {
                command
            } else {
                tracing::warn!(
                    client_id,
                    timeout_secs,
                    "client too old for ShellWithTimeout, falling back to Shell (120s default)"
                );
                AgentCommand::Shell {
                    cmd: cmd.clone(),
                    cwd: cwd.clone(),
                }
            }
        }
        _ => command,
    };
    // 等待超时：Shell→150s、ShellWithTimeout→min(timeout_secs+30, 3630)、其他→120s
    let wait_timeout = match &command {
        AgentCommand::ShellWithTimeout { timeout_secs, .. } => {
            std::time::Duration::from_secs((timeout_secs + 30).min(3630))
        }
        AgentCommand::Shell { .. } => std::time::Duration::from_secs(150),
        _ => std::time::Duration::from_secs(120),
    };
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
            wait_timeout,
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
