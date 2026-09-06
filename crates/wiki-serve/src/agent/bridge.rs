// 测试代码豁免 panic 风险 lint（生产代码仍告警）
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
#![allow(clippy::missing_docs_in_private_items)]

//! Tauri 桥接层（唯一 `#[cfg(feature = "tauri")]` 的 agent 文件）.
//!
//! 将 [`crate::agent::AgentManager`] 的事件与命令透传为 Tauri command / event.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::agent::{AgentManager, AgentSettings, AgentStatus, ResolveSource};
use crate::error::IpcResult;

/// Tauri 事件 sink：发射 `agent:notification` / `agent:server-request` / `agent:status`.
#[derive(Debug)]
pub struct TauriEventSink(pub AppHandle);

impl crate::agent::AgentEventSink for TauriEventSink {
    fn on_notification(&self, method: String, params: Option<Value>) {
        let payload = serde_json::json!({ "method": method, "params": params });
        let _ = self.0.emit("agent:notification", payload);
    }
    fn on_server_request(&self, id: Value, method: String, params: Option<Value>) {
        let payload = serde_json::json!({ "id": id, "method": method, "params": params });
        let _ = self.0.emit("agent:server-request", payload);
    }
    fn on_status(&self, status: AgentStatus) {
        let _ = self.0.emit("agent:status", &status);
    }
    fn on_parse_error(&self, line: String, error: String) {
        let payload = serde_json::json!({ "line": line, "error": error });
        let _ = self.0.emit("agent:parse-error", payload);
    }
}

/// 二进制探测 DTO（前端设置页用）.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinaryResolveDto {
    /// 路径.
    pub path: String,
    /// 需前置的 argv.
    pub argv_prefix: Vec<String>,
    /// 来源.
    pub source: String,
    /// 版本（若已知）.
    pub version: Option<String>,
}

fn source_to_str(s: &ResolveSource) -> &'static str {
    match s {
        ResolveSource::Sidecar => "sidecar",
        ResolveSource::Override => "override",
        ResolveSource::Path => "path",
    }
}

/// 获取 agent 状态.
///
/// # Errors
///
/// 始终成功（状态快照）.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn agent_get_status(manager: State<'_, Arc<Mutex<AgentManager>>>) -> IpcResult<AgentStatus> {
    let guard = manager.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    Ok(guard.get_status())
}

/// 启动 agent（幂等，内部完成 `initialize`/`initialized` 握手）.
///
/// # Errors
///
/// 解析/启动/握手失败时返回 [`crate::error::IpcError::Agent`]。
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn agent_start(
    app: AppHandle,
    manager: State<'_, Arc<Mutex<AgentManager>>>,
) -> IpcResult<AgentStatus> {
    let sink: Arc<dyn crate::agent::AgentEventSink> = Arc::new(TauriEventSink(app));
    let mut guard = manager.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.start(sink).map_err(crate::error::IpcError::Agent)
}

/// 停止 agent.
///
/// # Errors
///
/// 通信失败时返回 [`crate::error::IpcError::Agent`]。
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn agent_stop(manager: State<'_, Arc<Mutex<AgentManager>>>) -> IpcResult<()> {
    let mut guard = manager.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.stop().map_err(crate::error::IpcError::Agent)
}

/// 探测二进制（不启动）.
///
/// # Errors
///
/// 始终成功（未找到返回 `None`，由前端按 `null` 处理）.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn agent_detect_binary(
    manager: State<'_, Arc<Mutex<AgentManager>>>,
) -> IpcResult<Option<BinaryResolveDto>> {
    let guard = manager.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let res = guard.detect_binary().map(|r| BinaryResolveDto {
        path: r.path.display().to_string(),
        argv_prefix: r.argv_prefix,
        source: source_to_str(&r.source).to_owned(),
        version: r.version,
    });
    Ok(res)
}

/// 泛型 JSON-RPC 调用.
///
/// # Errors
///
/// 未运行或对端错误时返回 [`crate::error::IpcError::Agent`]。
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn agent_request(
    manager: State<'_, Arc<Mutex<AgentManager>>>,
    method: String,
    params: Option<Value>,
) -> IpcResult<Value> {
    let guard = manager.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    guard
        .request_sync(method, params)
        .map_err(crate::error::IpcError::Agent)
}

/// 应答 server 发起的请求.
///
/// # Errors
///
/// 未运行或写失败时返回 [`crate::error::IpcError::Agent`]。
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn agent_respond(
    manager: State<'_, Arc<Mutex<AgentManager>>>,
    id: Value,
    result: Option<Value>,
    error: Option<Value>,
) -> IpcResult<()> {
    let guard = manager.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    guard
        .respond_sync(id, result, error)
        .map_err(crate::error::IpcError::Agent)
}

/// 获取 agent 设置.
///
/// # Errors
///
/// IO 失败时返回 [`crate::error::IpcError::Agent`]。
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn agent_get_settings(
    manager: State<'_, Arc<Mutex<AgentManager>>>,
) -> IpcResult<AgentSettings> {
    let guard = manager.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.get_settings().map_err(crate::error::IpcError::Agent)
}

/// 保存 agent 设置.
///
/// # Errors
///
/// 序列化或写入失败时返回 [`crate::error::IpcError::Agent`]。
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn agent_save_settings(
    manager: State<'_, Arc<Mutex<AgentManager>>>,
    settings: AgentSettings,
) -> IpcResult<()> {
    let mut guard = manager.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.save_settings(&settings).map_err(crate::error::IpcError::Agent)
}

/// 用系统默认浏览器打开外部 URL（OAuth 等）.
///
/// # Errors
///
/// URL 非法或打开失败时返回 [`crate::error::IpcError::Agent`]。
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn agent_open_external(url: String) -> IpcResult<()> {
    if url.trim().is_empty() {
        return Err(crate::error::IpcError::Agent("空 URL".to_owned()));
    }
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(crate::error::IpcError::Agent(format!("非法 URL：{url}")));
    }
    open::that(&url).map_err(|e| crate::error::IpcError::Agent(format!("打开浏览器失败：{e}")))?;
    Ok(())
}

/// 解析 `app_config_dir`（`app.path().app_config_dir()`）.
#[must_use]
pub fn resolve_app_config_dir(app: &AppHandle) -> PathBuf {
    if let Ok(dir) = app.path().app_config_dir() {
        return dir;
    }
    // 回退：HOME/.config/wiki-desktop 或 ./config
    std::env::var_os("HOME").map_or_else(
        || PathBuf::from("./config"),
        |h| PathBuf::from(h).join(".config").join("wiki-desktop"),
    )
}
