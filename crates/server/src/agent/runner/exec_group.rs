//! Read-only tool concurrency group (bounded parallel execution).
use crate::agent::session::SessionRuntime;
use crate::agent::{executor, tools, AgentState};
use rust_tunnel_common::{AgentCommand, AgentResult};
use super::{
    agent_result_to_text, client_supports_git_exec, client_supports_read_range,
    client_supports_search_patch, ParsedToolCall, READONLY_CONCURRENCY,
    MIN_GIT_EXEC_CLIENT_VERSION, MIN_READ_RANGE_CLIENT_VERSION,
    MIN_SEARCH_PATCH_CLIENT_VERSION,
};

pub(crate) async fn exec_readonly_group(
    agent: AgentState,
    rt: &SessionRuntime,
    group: &[ParsedToolCall],
) -> Vec<String> {
    let client_id = rt.client_id.clone();
    let workspace_id = rt.workspace_id.clone();
    let root_path = rt.root_path.clone();
    let docker_container = rt.docker_container.clone();
    let runtime_type = rt.runtime_type.clone();

    let mut results = Vec::with_capacity(group.len());
    for window in group.chunks(READONLY_CONCURRENCY) {
        let mut handles = Vec::with_capacity(window.len());
        for call in window {
            let agent = agent.clone();
            let cid = client_id.clone();
            let wid = workspace_id.clone();
            let rpath = root_path.clone();
            let dc = docker_container.clone();
            let rt_type = runtime_type.clone();
            let c_name = call.name.clone();
            let c_args = call.args.clone();
            handles.push(tokio::spawn(async move {
                exec_readonly_one(&agent, &cid, &wid, &rpath, dc.as_deref(), &rt_type, &c_name, &c_args).await
            }));
        }
        for h in handles {
            results.push(
                h.await
                    .unwrap_or_else(|e| format!("error: tool task panicked: {e}")),
            );
        }
    }
    results
}

/// 执行单个只读工具调用并返回结果文本（不抢 workspace_lock）。
/// 解析错误/版本不足/docker 未启动等失败折叠为错误文本（与串行路径一致）。
#[allow(clippy::too_many_arguments)]
async fn exec_readonly_one(
    agent: &AgentState,
    client_id: &str,
    workspace_id: &str,
    root_path: &str,
    docker_container: Option<&str>,
    runtime_type: &str,
    call_name: &str,
    call_args: &str,
) -> String {
    let command = match tools::parse_tool_call(call_name, call_args) {
        Ok(cmd) => cmd,
        Err(e) => return format!("error: {e}"),
    };
    // 版本门控：Search/GitExec 需要客户端最低版本
    let gated = match &command {
        AgentCommand::Search { .. } => Some((
            MIN_SEARCH_PATCH_CLIENT_VERSION,
            client_supports_search_patch as fn(Option<&str>) -> bool,
        )),
        AgentCommand::GitExec { .. } => Some((
            MIN_GIT_EXEC_CLIENT_VERSION,
            client_supports_git_exec as fn(Option<&str>) -> bool,
        )),
        AgentCommand::ReadFileRange { .. } => Some((
            MIN_READ_RANGE_CLIENT_VERSION,
            client_supports_read_range as fn(Option<&str>) -> bool,
        )),
        AgentCommand::CodeOutline { .. } | AgentCommand::ReadSymbol { .. } => Some((
            MIN_READ_RANGE_CLIENT_VERSION,
            client_supports_read_range as fn(Option<&str>) -> bool,
        )),
        _ => None,
    };
    if let Some((min_version, supports)) = gated {
        let version = agent.registry.get(client_id).await.and_then(|e| e.client_version.clone());
        if !supports(version.as_deref()) {
            return format!(
                "error: tool '{}' requires client >= {}.{}.{}; please upgrade the client",
                call_name, min_version.0, min_version.1, min_version.2,
            );
        }
    }
    let result = if runtime_type == "docker" && docker_container.is_none() {
        AgentResult::Error { message: "docker container not started".into() }
    } else {
        executor::exec_on_client_readonly(
            agent,
            workspace_id,
            client_id,
            root_path,
            docker_container,
            command,
        )
        .await
    };
    agent_result_to_text(&result)
}
