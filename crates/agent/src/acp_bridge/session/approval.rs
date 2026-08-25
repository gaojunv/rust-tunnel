//! 审批/征询桥接：连接存活校验（断线即时拒绝）与选项映射。

use std::sync::Arc;
use tokio::sync::{mpsc, watch};

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

use crate::{ApprovalOption, ApprovalResult, ElicitationResult};

use super::super::{ApproveFn, ElicitFn};

/// 审批等待 + 断线/重连即时拒绝（select 逻辑独立成函数便于单元测试）。
///
/// 订阅后先检查当前值：若已 detach/重连（值 ≠ 捕获的 `captured_conn_id`，
/// 例如 detach 早于订阅发生）立即返回 `Denied`，不进入审批。随后 select 等
/// approval 完成，或 `conn_rx` 变为 ≠ 捕获值（连接断开/重连 → 审批帧缓冲在
/// 无人消费的旧 channel 里、新重连也收不到）→ 返回 `Denied`，避免
/// `request_approval` 等满 5 分钟超时阻塞 agent 的下一个工具调用。
#[allow(clippy::too_many_arguments)] // 保留：混合基础设施参数（Arc 回调+channel），不适合 Opts 化
pub(crate) async fn approve_or_disconnect(
    approval: Arc<ApproveFn>,
    sid: String,
    tool_name: String,
    args_preview: String,
    options: Vec<ApprovalOption>,
    ws_tx: mpsc::Sender<serde_json::Value>,
    captured_conn_id: u64,
    mut conn_rx: watch::Receiver<u64>,
) -> ApprovalResult {
    // subscribe 之后立即检查：可能在 subscribe 之前已变化（detach/重连已发生）。
    if *conn_rx.borrow() != captured_conn_id {
        return ApprovalResult::Denied;
    }
    tokio::select! {
        result = approval(sid, tool_name, "ACP 工具调用请求".to_string(), args_preview, options, ws_tx) => {
            result
        }
        _ = conn_rx.wait_for(|v| *v != captured_conn_id) => {
            // WS 断线/重连 → Deny（不等审批超时）。
            ApprovalResult::Denied
        }
    }
}
/// elicitation 等待 + 断线/重连即时取消（与 [`approve_or_disconnect`] 同模式，
/// 独立成函数便于单元测试）。表单等待期间连接断开/重连（conn_rx 变为 ≠ 捕获的
/// conn_id）→ 立即 `Cancel`，避免 `request_elicitation` 等满 5 分钟超时阻塞
/// agent 的下一个工具调用。
#[allow(clippy::too_many_arguments)] // 保留：混合基础设施参数（Arc 回调+channel），不适合 Opts 化
pub(crate) async fn elicit_or_disconnect(
    elicitation: Arc<ElicitFn>,
    sid: String,
    message: String,
    schema: serde_json::Value,
    ws_tx: mpsc::Sender<serde_json::Value>,
    captured_conn_id: u64,
    mut conn_rx: watch::Receiver<u64>,
) -> ElicitationResult {
    // subscribe 之后立即检查：可能在 subscribe 之前已变化（detach/重连已发生）。
    if *conn_rx.borrow() != captured_conn_id {
        return ElicitationResult::Cancel;
    }
    tokio::select! {
        result = elicitation(sid, message, schema, ws_tx) => {
            result
        }
        _ = conn_rx.wait_for(|v| *v != captured_conn_id) => {
            // WS 断线/重连 → Cancel（不等表单超时）。
            ElicitationResult::Cancel
        }
    }
}
/// 从权限选项里挑指定 kind 的 option id（approve→AllowAlways/AllowOnce，
/// deny→RejectAlways/RejectOnce；优先 Always 对齐"记住本会话"语义）。
pub(crate) fn pick_option(
    options: &[PermissionOption],
    kind: PermissionOptionKind,
) -> Option<PermissionOptionId> {
    options
        .iter()
        .find(|o| o.kind == kind)
        .map(|o| o.option_id.clone())
}
/// ACP `PermissionOption` → 审批卡片透传的轻量 `ApprovalOption`（kind 归一为
/// snake_case 字符串，前端据此渲染按钮样式 / 决定 remember 语义）。
pub(crate) fn permission_option_to_approval(o: &PermissionOption) -> ApprovalOption {
    ApprovalOption {
        id: o.option_id.to_string(),
        label: o.name.clone(),
        kind: match o.kind {
            PermissionOptionKind::AllowOnce => "allow_once".to_string(),
            PermissionOptionKind::AllowAlways => "allow_always".to_string(),
            PermissionOptionKind::RejectOnce => "reject_once".to_string(),
            PermissionOptionKind::RejectAlways => "reject_always".to_string(),
            // non_exhaustive：未来新增 kind 按自定义选项渲染（中性样式）。
            _ => "custom".to_string(),
        },
    }
}
