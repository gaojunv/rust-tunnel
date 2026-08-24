//! ACP 客户端能力声明与 workspace 相对路径换算。

use std::path::Path;

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

/// 本服务端声明的 ACP 客户端能力：fs 读写经隧道转发到客户端沙箱执行。
/// 不声明则 agent 静默降级（如报「不支持」）。
///
/// `_meta["subagent-transcript"] = true` 是 claude-code-acp 适配器的 opt-in 约定
/// （https://github.com/zed-industries/claude-code-acp）：声明后 agent 会在子 agent
/// （Task/Agent 工具）产出的 tool_call / tool_call_update / agent_message_chunk /
/// agent_thought_chunk 事件 `_meta.claudeCode` 里带 `subagent` 与
/// `parentToolUseId`，本服务端据此透传父归属给前端分组渲染。不支持 `_meta` 的
/// agent（gemini/opencode 等）会忽略未知键，无副作用（事件不带 `_meta`，映射端
/// 字段缺省、完全无感降级）。
pub(crate) fn client_capabilities() -> agent_client_protocol::schema::v1::ClientCapabilities {
    let mut meta = serde_json::Map::new();
    meta.insert(
        "subagent-transcript".to_string(),
        serde_json::Value::Bool(true),
    );
    agent_client_protocol::schema::v1::ClientCapabilities::new()
        .fs(
            agent_client_protocol::schema::v1::FileSystemCapabilities::new()
                .read_text_file(true)
                .write_text_file(true),
        )
        .meta(meta)
        // 声明 elicitation.form：claude-code-acp 据此启用 AskUserQuestion（否则放入
        // disallowedTools 报「not enabled in this context」）。只声明 form、不声明
        // url（缺省 None → 序列化不含；收到 url/other 模式回 Cancel + warn 降级）。
        .elicitation(
            agent_client_protocol::schema::v1::ElicitationCapabilities::new()
                .form(agent_client_protocol::schema::v1::ElicitationFormCapabilities::new()),
        )
}
/// 把 ACP 的绝对路径转成工作区相对路径。客户端 `resolve_sandboxed` 只接受相对
/// 路径（拒绝绝对路径、拒绝逃逸工作区）；ACP `Read/WriteTextFileRequest.path`
/// 约定为绝对路径，这里剥掉 root_path 前缀。路径在工作区外 → Err。
pub(crate) fn to_workspace_relative(root_path: &str, abs_path: &str) -> Result<String, String> {
    let root = Path::new(root_path);
    let abs = Path::new(abs_path);
    if !abs.is_absolute() {
        return Err(format!("fs request path must be absolute: {abs_path}"));
    }
    let rel = abs
        .strip_prefix(root)
        .map_err(|_| format!("fs request path is outside workspace root: {abs_path}"))?;
    if rel.as_os_str().is_empty() {
        return Err("fs request path is the workspace root itself".into());
    }
    Ok(rel.to_string_lossy().to_string())
}
