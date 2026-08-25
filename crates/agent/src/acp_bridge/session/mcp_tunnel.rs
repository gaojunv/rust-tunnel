#[cfg(feature = "rag")]
use std::collections::HashMap;
#[cfg(feature = "rag")]
use std::sync::Arc;
#[cfg(feature = "rag")]
use tokio::sync::Mutex;

#[cfg(feature = "rag")]
use super::super::SpawnedAgent;
#[cfg(feature = "rag")]
use crate::db::Database;
#[cfg(feature = "rag")]
use crate::mcp::McpHttpResponse;
#[cfg(feature = "rag")]
use crate::memory::MemoryState;

/// 处理 ACP agent 经回环代理转发的 MCP 请求（`/mcp/<token>`）。
///
/// token 校验：与 [`SpawnedAgent`] 的 `mcp_token`（ensure_session 铸造）精确比对；
/// 条目移除（kill/重拉/reaper）即吊销。校验通过后解析 workspace 坐标并转交
/// [`crate::mcp::handle_request`] 做 JSON-RPC 分发（只暴露 remember 工具）。
#[cfg(feature = "rag")]
pub(super) async fn handle_mcp_tunnel(
    sessions: &Arc<Mutex<HashMap<String, SpawnedAgent>>>,
    memory: Option<&MemoryState>,
    db: &Database,
    session_id: &str,
    path: &str,
    body: Vec<u8>,
) -> McpHttpResponse {
    // 空 body → 405：防御旧客户端 GET SSE 探测等非 POST（MCP SDK 把 GET 405
    // 视为静默成功，不阻断 initialize）。
    if body.is_empty() {
        return McpHttpResponse {
            status: 405,
            data: Vec::new(),
        };
    }
    // 解析 token：strip_prefix("/mcp/") → 去 query（`/mcp/<token>?xxx`）→ 非空。
    let token = match path.strip_prefix("/mcp/").map(|t| t.split('?').next()) {
        Some(Some(t)) if !t.is_empty() => t,
        _ => return mcp_token_rejected(),
    };
    // 锁 sessions 比对 token（不匹配/会话不在 → 404 + JSON-RPC error）。
    let client_id = {
        let sessions = sessions.lock().await;
        match sessions.get(session_id) {
            Some(agent) if agent.mcp_token.as_deref() == Some(token) => agent.client_id.clone(),
            _ => return mcp_token_rejected(),
        }
    };
    // workspace 坐标：remember 落库需要（scope=workspace 时）。会话行缺失/DB 错误
    // 属服务端异常，500 防御。
    let workspace_id = if let Ok(Some(s)) = db.agent_get_session(session_id).await { s.workspace_id } else {
        tracing::warn!(session_id, "mcp tunnel: agent session missing in db");
        return mcp_internal_error("agent session not found");
    };
    // memory 缺失（防御：构造时未注入）→ 500。
    let Some(memory) = memory else {
        tracing::warn!(session_id, "mcp tunnel: memory runtime not configured");
        return mcp_internal_error("memory not configured");
    };
    crate::mcp::handle_request(memory, &client_id, &workspace_id, session_id, &body).await
}

/// token 无效/会话条目不在：404 + JSON-RPC error（MCP SDK 视连接失败，重试/报错）。
#[cfg(feature = "rag")]
fn mcp_token_rejected() -> McpHttpResponse {
    McpHttpResponse {
        status: 404,
        data: serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": serde_json::Value::Null,
            "error": {"code": -32001, "message": "invalid mcp token"},
        }))
        .unwrap_or_default(),
    }
}

/// 服务端内部错误：500 + JSON-RPC error。
#[cfg(feature = "rag")]
fn mcp_internal_error(message: &str) -> McpHttpResponse {
    McpHttpResponse {
        status: 500,
        data: serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": serde_json::Value::Null,
            "error": {"code": -32000, "message": message},
        }))
        .unwrap_or_default(),
    }
}
