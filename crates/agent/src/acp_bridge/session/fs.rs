//! ACP fs 请求隧道转发：read/write text file（workspace 内约束）。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

#[allow(unused_imports)]
use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, CreateElicitationRequest, CreateElicitationResponse,
    DeleteSessionRequest, ElicitationAcceptAction, ElicitationAction, ElicitationMode,
    InitializeRequest, McpServer, McpServerHttp, NewSessionRequest, PermissionOption,
    PermissionOptionId, PermissionOptionKind, PromptRequest, ReadTextFileRequest,
    ReadTextFileResponse, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, ResumeSessionRequest, SelectedPermissionOutcome, SessionConfigId,
    SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory, SessionConfigOptionValue,
    SessionConfigValueId, SessionId, SessionNotification, SetSessionConfigOptionRequest,
    TextContent, WriteTextFileRequest, WriteTextFileResponse,
};

use crate::db::Database;
use crate::spawner::AgentSpawner;

use super::super::SpawnedAgent;

use super::to_workspace_relative;

/// fs 请求的公共上下文：session → workspace（root_path / docker）→ 活跃进程 client_id。
struct FsContext {
    client_id: String,
    root_path: String,
    docker_container: Option<String>,
}
/// 解析 fs 请求上下文；任一环节缺失报 Err（session 未建 / 进程未 spawn / DB 无记录）。
async fn fs_context(
    db: &Database,
    sessions: &Arc<Mutex<HashMap<String, SpawnedAgent>>>,
    sid: &str,
) -> Result<FsContext, String> {
    let session = db
        .agent_get_session(sid)
        .await
        .map_err(|e| format!("session lookup failed: {e}"))?
        .ok_or_else(|| "session not found".to_string())?;
    let ws = db
        .agent_get_workspace(&session.workspace_id)
        .await
        .map_err(|e| format!("workspace lookup failed: {e}"))?
        .ok_or_else(|| "workspace not found".to_string())?;
    let client_id = {
        let sessions = sessions.lock().await;
        sessions
            .get(sid)
            .map(|a| a.client_id.clone())
            .ok_or_else(|| "session not spawned".to_string())?
    };
    Ok(FsContext {
        client_id,
        root_path: ws.root_path,
        docker_container: ws.docker_container_id,
    })
}
/// 执行 `fs/read_text_file`：绝对路径 → 工作区相对路径 → 经隧道转发到客户端
/// 沙箱读取，返回文本内容。成功且无截断时记录 SHA-256 用于 stale 检测。
pub(crate) async fn exec_fs_read(
    db: &Database,
    spawner: &AgentSpawner,
    sessions: &Arc<Mutex<HashMap<String, SpawnedAgent>>>,
    sid: &str,
    abs_path: &str,
) -> Result<String, String> {
    let ctx = fs_context(db, sessions, sid).await?;
    let rel = to_workspace_relative(&ctx.root_path, abs_path)?;
    let request_id = format!("{:032x}", rand::random::<u128>());
    let result = spawner
        .agent_exec(
            &ctx.client_id,
            &request_id,
            sid,
            &ctx.root_path,
            ctx.docker_container.as_deref(),
            rust_tunnel_common::AgentCommand::ReadFile { path: rel },
            Duration::from_mins(2),
        )
        .await
        .map_err(|e| format!("tunnel execution failed: {e}"))?;
    match result {
        rust_tunnel_common::AgentResult::FileContent { content } => {
            // 无截断标记时记录 hash 供后续 WriteFile2 stale 检测
            if !content.contains("[truncated") {
                use sha2::{Digest, Sha256};
                let hash = format!("{:x}", Sha256::digest(content.as_bytes()));
                let mut sessions = sessions.lock().await;
                if let Some(agent) = sessions.get_mut(sid) {
                    agent.file_hashes.insert(abs_path.to_string(), hash);
                }
            }
            Ok(content)
        }
        rust_tunnel_common::AgentResult::Error { message } => Err(message),
        other => Err(format!("unexpected read result: {other:?}")),
    }
}
/// 执行 `fs/write_text_file`：同 read，写文件到客户端沙箱。
/// 客户端 ≥0.8.0 时使用 WriteFile2（支持 stale 检测 + WriteOutcome）。
pub(crate) async fn exec_fs_write(
    db: &Database,
    spawner: &AgentSpawner,
    sessions: &Arc<Mutex<HashMap<String, SpawnedAgent>>>,
    sid: &str,
    abs_path: &str,
    content: &str,
) -> Result<(), String> {
    let ctx = fs_context(db, sessions, sid).await?;
    let rel = to_workspace_relative(&ctx.root_path, abs_path)?;

    // 提前查版本号 + expected_hash（短锁，无 await）
    let (client_version, expected_hash) = {
        let sessions_guard = sessions.lock().await;
        let ver = spawner.client_version(&ctx.client_id).await;
        let hash = sessions_guard
            .get(sid)
            .and_then(|a| a.file_hashes.get(abs_path).cloned());
        (ver, hash)
    };

    let use_write_file2 = crate::runner::client_supports_edit(client_version.as_deref());

    let command = if use_write_file2 {
        rust_tunnel_common::AgentCommand::WriteFile2 {
            path: rel,
            content: content.to_string(),
            expected_hash,
        }
    } else {
        rust_tunnel_common::AgentCommand::WriteFile {
            path: rel,
            content: content.to_string(),
        }
    };

    let request_id = format!("{:032x}", rand::random::<u128>());
    let result = spawner
        .agent_exec(
            &ctx.client_id,
            &request_id,
            sid,
            &ctx.root_path,
            ctx.docker_container.as_deref(),
            command,
            Duration::from_mins(2),
        )
        .await
        .map_err(|e| format!("tunnel execution failed: {e}"))?;
    match result {
        rust_tunnel_common::AgentResult::WriteOutcome { file_hash, .. } => {
            // WriteFile2 成功：刷新 hash
            if !file_hash.is_empty() {
                let mut sessions = sessions.lock().await;
                if let Some(agent) = sessions.get_mut(sid) {
                    agent.file_hashes.insert(abs_path.to_string(), file_hash);
                }
            }
            Ok(())
        }
        rust_tunnel_common::AgentResult::Success => Ok(()),
        rust_tunnel_common::AgentResult::Error { message } => {
            // stale 写入失败：清除该路径的 hash 缓存
            if message.contains("file changed externally") {
                let mut sessions = sessions.lock().await;
                if let Some(agent) = sessions.get_mut(sid) {
                    agent.file_hashes.remove(abs_path);
                }
            }
            Err(message)
        }
        other => Err(format!("unexpected write result: {other:?}")),
    }
}
/// 把 ACP 权限请求的 raw 输入编码成审批弹层的 args_preview 字符串。
/// 字符串直传；对象序列化为 JSON 文本（与 acp_events 的 encode_raw 同语义）。
pub(crate) fn acp_raw_to_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}
