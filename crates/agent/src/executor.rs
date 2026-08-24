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
    exec_on_client_impl(agent, workspace_id, client_id, root_path, docker_container, command, true).await
}

/// 只读命令的并发执行入口：跳过 workspace_lock（只读不触碰 git 状态/写盘，
/// 并行互不干扰）；其余（ShellWithTimeout 版本降级、超时、inflight 追踪）与
/// exec_on_client 完全一致。
pub async fn exec_on_client_readonly(
    agent: &AgentState,
    workspace_id: &str,
    client_id: &str,
    root_path: &str,
    docker_container: Option<&str>,
    command: AgentCommand,
) -> AgentResult {
    exec_on_client_impl(agent, workspace_id, client_id, root_path, docker_container, command, false).await
}

/// 内部实现：`take_lock` 控制是否获取 workspace_lock（只读并发跳过锁）。
async fn exec_on_client_impl(
    agent: &AgentState,
    workspace_id: &str,
    client_id: &str,
    root_path: &str,
    docker_container: Option<&str>,
    command: AgentCommand,
    take_lock: bool,
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
                .client_handle(client_id)
                .await
                .and_then(|h| h.client_version);
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
    // 只读命令跳过 workspace_lock（锁保护 git 状态安全与写互斥，只读并发无此需要）。
    // 先获取 Mutex（保持存活），再 lock（Guard 取引用）：两层分离使
    // Mutex 与 MutexGuard 同生命周期（Guard 通过引用借用 Mutex）。
    let mut lock_holder = if take_lock {
        Some(agent.workspace_lock(workspace_id).await)
    } else {
        None
    };
    let _guard = match lock_holder.as_mut() {
        Some(m) => Some(m.lock().await),
        None => None,
    };
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
    // 无论成败都清本条 inflight，避免 stale 条目（并发组各自精确清除自己的 id）。
    agent.inflight_end(workspace_id, &request_id).await;
    match result {
        Ok(result) => result,
        Err(e) => AgentResult::Error {
            message: format!("tunnel execution failed: {e}"),
        },
    }
}
