//! 远程控制控制面：会话授权表、可远程列表构建与能力下发。
//!
//! 数据面（P2P 屏幕/输入流）跑在仓库内置的 hbbs 会合 + hbbr 中继上；本模块只负责
//! 控制面——谁可以被远程、谁能连谁、会话授权与生命周期。
//!
//! 硬约束：**所有 S→C 的远程变体推送前都必须过 [`client_supports_remote`]**。老客户端
//! 收到未知 bincode 变体会反序列化失败并断开控制连接（与既有 `AgentExecCancel` 同因）。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use base64::Engine;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::agent::runner::client_supports_remote;
use rust_tunnel_common::{ControlMessage, RemoteMode, RemoteParams, RemotePeerInfo};
use rust_tunnel_rustdesk::key::ServerKey;

use super::client_registry::ClientEntry;
use super::state::ServerState;

/// 会话授权默认 TTL：被控端须在此期间内应答，否则视为过期（由 `issue`/`reap_expired` 回收）。
pub const REMOTE_SESSION_TTL: Duration = Duration::from_secs(60);

/// 一条已签发、等待被控端应答的远程会话（内存态）。
#[derive(Debug, Clone)]
pub struct PendingSession {
    /// 一次性鉴权 token（被控端在数据面握手时校验）。
    pub token: String,
    /// 被控端客户端名。
    pub target_client: String,
    /// 控制端客户端名。
    pub controller_client: String,
    /// 会话模式。
    pub mode: RemoteMode,
    /// 签发时刻（单调时钟，仅用于 TTL 判定）。
    pub created_at: Instant,
}

/// 内存会话授权表：`session_id -> PendingSession`。
///
/// 只在控制面存在；token 的实际校验由两端在数据面自行完成，服务端不再回查。
/// `session_id` 全局唯一（uuid v4），跨客户端不会碰撞。
#[derive(Debug, Default)]
pub struct RemoteSessionTable {
    inner: Mutex<HashMap<String, PendingSession>>,
}

impl RemoteSessionTable {
    /// 创建空表。
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// 签发一条会话，返回 `(session_id, token)`。
    ///
    /// `session_id` 为 uuid v4；`token` 为 32 字节随机数的 URL-safe 无填充 base64。
    /// 顺带回收过期条目，避免长期无应答的挂起会话无限堆积。
    pub async fn issue(
        &self,
        target: &str,
        controller: &str,
        mode: RemoteMode,
    ) -> (String, String) {
        let mut table = self.inner.lock().await;
        reap_locked(&mut table, REMOTE_SESSION_TTL);
        let session_id = uuid::Uuid::new_v4().to_string();
        let token = random_token();
        table.insert(
            session_id.clone(),
            PendingSession {
                token: token.clone(),
                target_client: target.to_string(),
                controller_client: controller.to_string(),
                mode,
                created_at: Instant::now(),
            },
        );
        (session_id, token)
    }

    /// 读取会话快照（不消费）；未知 `session_id` 返回 `None`。
    pub async fn get(&self, session_id: &str) -> Option<PendingSession> {
        self.inner.lock().await.get(session_id).cloned()
    }

    /// 一次性消费会话（移除并返回）；未知 `session_id` 返回 `None`。
    pub async fn take(&self, session_id: &str) -> Option<PendingSession> {
        self.inner.lock().await.remove(session_id)
    }

    /// 结束并清理会话（移除，不返回内容）。
    pub async fn end(&self, session_id: &str) {
        self.inner.lock().await.remove(session_id);
    }

    /// 移除 `client` 参与的全部会话，返回 `(session_id, 对端客户端名)` 列表，
    /// 供控制连接断开时通知对端。`client` 为被控端时对端取控制端，反之亦然。
    pub async fn remove_for_client(&self, client: &str) -> Vec<(String, String)> {
        let mut table = self.inner.lock().await;
        let ids: Vec<String> = table
            .iter()
            .filter(|(_, s)| s.target_client == client || s.controller_client == client)
            .map(|(id, _)| id.clone())
            .collect();
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(s) = table.remove(&id) {
                let peer = if s.controller_client == client {
                    s.target_client
                } else {
                    s.controller_client
                };
                out.push((id, peer));
            }
        }
        out
    }

    /// 回收创建时刻早于 `now - ttl` 的会话，返回回收条数。
    pub async fn reap_expired(&self, ttl: Duration) -> usize {
        let mut table = self.inner.lock().await;
        reap_locked(&mut table, ttl)
    }

    /// 当前挂起会话条数（诊断/测试用）。
    pub async fn session_count(&self) -> usize {
        self.inner.lock().await.len()
    }
}

/// 从表中剔除已过 TTL 的会话，返回剔除条数。
fn reap_locked(table: &mut HashMap<String, PendingSession>, ttl: Duration) -> usize {
    let before = table.len();
    table.retain(|_, s| s.created_at.elapsed() < ttl);
    before - table.len()
}

/// 生成一次性鉴权 token：32 字节随机数的 URL-safe 无填充 base64。
fn random_token() -> String {
    let bytes: [u8; 32] = rand::random();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

// ── 能力下发与可远程列表 ─────────────────────────────────────────

/// 由服务端当前 RustDesk 动态配置构造会合/中继参数。
///
/// 服务端未启用（`rustdesk` 为 `None` 或 `enabled == false`）时返回 `None`。
/// `rendezvous_host` 一律留空串——服务端不知道自己的公网域名，由客户端用自身
/// `config.server` 的 host 段补齐。
///
/// **密钥必须过 [`ServerKey::parse`] 归一化后再下发，绝不能直接送 DB 里的原始列。**
/// Web UI 的「生成」按钮把 64 字节 NaCl **私钥**写进该列（`to_secret_base64`），直接
/// 下发等于把服务端签名私钥泄露给每一个 GUI 客户端；且 hbbs 侧比对的是
/// `ServerKey::key()`（归一化后的**公钥**字符串，见 `rendezvous.rs` 的
/// `licence_key` 校验），送原始列会让 `licence_key` 永远比对失败、所有会话都建不起来。
/// 归一化后：私钥列 → 公钥字符串，公钥列/共享口令 → 原样。密钥无法解析时视为未启用。
pub async fn remote_params(state: &ServerState) -> Option<RemoteParams> {
    let cfg = state.dynamic_config.read().await;
    let rd = cfg.rustdesk.as_ref()?;
    if !rd.enabled {
        return None;
    }
    let server_key = match ServerKey::parse(&rd.key) {
        Ok(sk) => sk.key().to_owned(),
        Err(e) => {
            warn!("RustDesk 密钥无法解析，远程控制参数不可用：{e}");
            return None;
        }
    };
    Some(RemoteParams {
        rendezvous_host: String::new(),
        rendezvous_port: rd.rendezvous_port,
        relay_port: rd.relay_port,
        relay_server: rd.relay_server.clone(),
        public_key: server_key,
        always_use_relay: rd.always_use_relay,
    })
}

/// 服务端 RustDesk 是否启用（不看密钥能否解析，只做「要不要广播」的廉价判断）。
///
/// 刻意不调 [`remote_params`]：那条路径在密钥损坏时会 `warn!`，而本函数会被
/// 「每次客户端断开」触发，用它会让日志被同一条告警刷屏。
async fn rustdesk_enabled(state: &ServerState) -> bool {
    state
        .dynamic_config
        .read()
        .await
        .rustdesk
        .as_ref()
        .is_some_and(|rd| rd.enabled)
}

/// 构建可远程客户端列表：只含已开启被控、在线、版本支持且已分配 RustDesk ID 的客户端。
///
/// `exclude` 为收件人自身的客户端名——必须排除，否则「自己也开了被控」的客户端会在
/// 自己的列表里看到自己，点一下就是连自己（会自建一条到本机的回环会话）。
pub async fn build_remote_peer_list(
    state: &ServerState,
    exclude: Option<&str>,
) -> Vec<RemotePeerInfo> {
    let Some(registry) = state.client_registry.as_ref() else {
        return Vec::new();
    };
    registry
        .list_remote_peers()
        .await
        .into_iter()
        .filter_map(|entry| {
            if exclude == Some(entry.name.as_str()) {
                return None;
            }
            let caps = entry.remote_caps();
            if caps.rd_id.is_empty() {
                return None;
            }
            Some(RemotePeerInfo {
                client_name: entry.name.clone(),
                display_name: entry
                    .hostname
                    .clone()
                    .filter(|h| !h.is_empty())
                    .unwrap_or_else(|| entry.name.clone()),
                platform: caps.platform,
                displays: caps.displays,
                rd_id: caps.rd_id,
            })
        })
        .collect()
}

/// 向所有支持远程控制的在线客户端广播最新可远程列表。
///
/// 列表按收件人各自构建：每个收件人都要排除自己（见 [`build_remote_peer_list`]）。
/// 逐客户端按版本门控。
pub async fn broadcast_remote_peer_list(state: &ServerState) {
    let Some(registry) = state.client_registry.as_ref() else {
        return;
    };
    // RustDesk 未启用时谁都不可能成为可远程对端，广播空列表只是噪音。
    if !rustdesk_enabled(state).await {
        return;
    }
    for entry in registry.list_online().await {
        if !client_supports_remote(entry.client_version.as_deref()) {
            continue;
        }
        let peers = build_remote_peer_list(state, Some(&entry.name)).await;
        let msg = ControlMessage::RemotePeerList { peers };
        let _ = entry.control_sender.send(msg).await;
    }
}

/// 向单个客户端推送远程能力通告 `RemoteCapability`（注册后首包 / 配置变更后）。
///
/// 客户端离线或版本不支持时静默返回 `false`。
/// - 服务端 RustDesk 未启用 → `params: None, rd_id: ""`（客户端据此隐藏远程功能）。
/// - 已启用 → 分配（或复用）稳定 `rd_id` 并写入注册项，`params: Some(...)`。
/// - `rd_id` 分配失败不拒绝注册：记 `warn!` 并回落成 `params: None`。
pub async fn push_remote_config(state: &ServerState, client_name: &str) -> bool {
    let Some(registry) = state.client_registry.as_ref() else {
        return false;
    };
    let Some(entry) = registry.get(client_name).await else {
        return false;
    };
    if !client_supports_remote(entry.client_version.as_deref()) {
        return false;
    }
    let msg = match remote_params(state).await {
        Some(params) => match registry.db().allocate_remote_id(client_name).await {
            Ok(rd_id) => {
                entry.set_rd_id(rd_id.clone());
                ControlMessage::RemoteCapability {
                    params: Some(params),
                    rd_id,
                }
            }
            Err(e) => {
                warn!(
                    "为客户端 '{}' 分配 remote_id 失败，回落为未启用远程: {}",
                    client_name, e
                );
                ControlMessage::RemoteCapability {
                    params: None,
                    rd_id: String::new(),
                }
            }
        },
        None => ControlMessage::RemoteCapability {
            params: None,
            rd_id: String::new(),
        },
    };
    entry.control_sender.send(msg).await.is_ok()
}

// ── 会话授权流程 ─────────────────────────────────────────────────

/// 向控制器回送一次「请求被拒」。用 `RemoteSessionResponse{accepted:false}` 表达：
/// 拒绝发生在会话签发之前、没有真实 `session_id`，故语义上是「对请求的应答」而非
/// 「已存在会话的结束」；`session_id` 置空串。原因里带上目标名，便于控制器在只有
/// 单个待决请求时对号入座。
async fn deny_connect(controller: &ClientEntry, reason: String) {
    let _ = controller
        .control_sender
        .send(ControlMessage::RemoteSessionResponse {
            session_id: String::new(),
            accepted: false,
            reason: Some(reason),
        })
        .await;
}

/// 处理控制器的连接请求：校验目标 → 签发会话 → 双向下发（Incoming / Grant）。
///
/// 任一校验不满足即回 [`deny_connect`]。全部 S→C 推送均已按版本门控。
pub async fn handle_remote_connect_request(
    state: &ServerState,
    controller: &ClientEntry,
    target_client: &str,
    mode: RemoteMode,
) {
    if !client_supports_remote(controller.client_version.as_deref()) {
        return;
    }
    let Some(registry) = state.client_registry.as_ref() else {
        return;
    };
    let Some(target) = registry.get(target_client).await else {
        deny_connect(controller, format!("目标客户端 '{target_client}' 不在线")).await;
        return;
    };
    let caps = target.remote_caps();
    if !caps.enabled {
        deny_connect(
            controller,
            format!("目标客户端 '{target_client}' 未开启被远程控制"),
        )
        .await;
        return;
    }
    if !client_supports_remote(target.client_version.as_deref()) {
        deny_connect(
            controller,
            format!("目标客户端 '{target_client}' 版本不支持远程控制"),
        )
        .await;
        return;
    }
    if caps.rd_id.is_empty() {
        deny_connect(
            controller,
            format!("目标客户端 '{target_client}' 缺少 RustDesk ID"),
        )
        .await;
        return;
    }
    let Some(params) = remote_params(state).await else {
        deny_connect(controller, "服务端未启用 RustDesk".to_string()).await;
        return;
    };

    let (session_id, token) = state
        .remote_sessions
        .issue(target_client, &controller.name, mode)
        .await;

    let incoming = ControlMessage::RemoteSessionIncoming {
        session_id: session_id.clone(),
        token: token.clone(),
        from_name: controller.name.clone(),
        mode,
    };
    if target.control_sender.send(incoming).await.is_err() {
        state.remote_sessions.end(&session_id).await;
        deny_connect(
            controller,
            format!("目标客户端 '{target_client}' 控制通道已关闭"),
        )
        .await;
        return;
    }

    let grant = ControlMessage::RemoteConnectGrant {
        session_id,
        token,
        target_rd_id: caps.rd_id,
        target_platform: caps.platform,
        params,
    };
    let _ = controller.control_sender.send(grant).await;
}

/// 处理被控端应答：转发给控制器；拒绝则同时清理会话。
///
/// 只有该会话的被控端有权应答（其余来源记 `warn!` 丢弃）。
pub async fn handle_session_response(
    state: &ServerState,
    responder: &str,
    session_id: &str,
    accepted: bool,
    reason: Option<String>,
) {
    let Some(session) = state.remote_sessions.get(session_id).await else {
        debug!("远程会话响应来自未知 session '{session_id}'（已超时/结束）");
        return;
    };
    if session.target_client != responder {
        warn!(
            "非被控端 '{}' 应答了会话 '{session_id}'（被控端应为 '{}'）",
            responder, session.target_client
        );
        return;
    }
    notify_controller_response(
        state,
        &session.controller_client,
        session_id,
        accepted,
        reason,
    )
    .await;
    if !accepted {
        state.remote_sessions.end(session_id).await;
    }
}

/// 把被控端应答转发给控制器（按版本门控）。
async fn notify_controller_response(
    state: &ServerState,
    controller: &str,
    session_id: &str,
    accepted: bool,
    reason: Option<String>,
) {
    let Some(registry) = state.client_registry.as_ref() else {
        return;
    };
    let Some(entry) = registry.get(controller).await else {
        return;
    };
    if !client_supports_remote(entry.client_version.as_deref()) {
        return;
    }
    let _ = entry
        .control_sender
        .send(ControlMessage::RemoteSessionResponse {
            session_id: session_id.to_string(),
            accepted,
            reason,
        })
        .await;
}

/// 处理任一端发起的会话结束：一次性消费会话并通知对端。
///
/// 只有会话的两端之一有权结束它；非参与方的结束请求被丢弃（`session_id` 是
/// uuid v4 本不可猜，此处是纵深防御：不校验的话任何客户端只要拿到 session_id
/// 就能拆掉别人的会话）。
pub async fn handle_session_end(state: &ServerState, from: &str, session_id: &str, reason: &str) {
    let Some(session) = state.remote_sessions.get(session_id).await else {
        debug!("远程会话结束来自未知 session '{session_id}'（已超时/结束）");
        return;
    };
    let peer = if session.controller_client == from {
        session.target_client
    } else if session.target_client == from {
        session.controller_client
    } else {
        warn!("非参与方 '{from}' 请求结束会话 '{session_id}'，已忽略");
        return;
    };
    state.remote_sessions.end(session_id).await;
    notify_end(state, &peer, session_id, reason).await;
}

/// 控制连接断开时清理该客户端参与的会话并通知对端（best-effort）。
pub async fn handle_client_disconnected(state: &ServerState, client_name: &str) {
    for (session_id, peer) in state.remote_sessions.remove_for_client(client_name).await {
        notify_end(state, &peer, &session_id, "对端已离线").await;
    }
}

/// 向指定客户端下发 `RemoteSessionEnd`（按版本门控，best-effort）。
async fn notify_end(state: &ServerState, client: &str, session_id: &str, reason: &str) {
    let Some(registry) = state.client_registry.as_ref() else {
        return;
    };
    let Some(entry) = registry.get(client).await else {
        return;
    };
    if !client_supports_remote(entry.client_version.as_deref()) {
        return;
    }
    let _ = entry
        .control_sender
        .send(ControlMessage::RemoteSessionEnd {
            session_id: session_id.to_string(),
            reason: reason.to_string(),
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::mgmt::dynamic_config::{DynamicConfig, RustdeskDynamicConfig};
    use rust_tunnel_common::ControlMessage;
    use tokio::sync::mpsc;

    // ── RemoteSessionTable ──

    #[tokio::test]
    async fn test_issue_then_take_is_one_shot() {
        let table = RemoteSessionTable::new();
        let (sid, token) = table
            .issue("target", "controller", RemoteMode::Control)
            .await;
        assert!(!sid.is_empty());
        assert!(!token.is_empty());

        let first = table.take(&sid).await.expect("first take should hit");
        assert_eq!(first.token, token);
        assert_eq!(first.target_client, "target");
        assert_eq!(first.controller_client, "controller");
        assert_eq!(first.mode, RemoteMode::Control);

        // 一次性：第二次 take 为空
        assert!(table.take(&sid).await.is_none());
        assert_eq!(table.session_count().await, 0);
    }

    #[tokio::test]
    async fn test_take_unknown_session_is_none() {
        let table = RemoteSessionTable::new();
        assert!(table.take("no-such-session").await.is_none());
    }

    #[tokio::test]
    async fn test_reap_expired_removes_stale_only() {
        let table = RemoteSessionTable::new();
        let (old, _) = table.issue("t-old", "c", RemoteMode::Control).await;
        // ttl=0 表示「全部过期」
        assert_eq!(table.reap_expired(Duration::ZERO).await, 1);
        assert!(table.take(&old).await.is_none());

        // 大 TTL 不误杀
        let (fresh, _) = table.issue("t-fresh", "c", RemoteMode::ViewOnly).await;
        assert_eq!(table.reap_expired(REMOTE_SESSION_TTL).await, 0);
        assert!(table.take(&fresh).await.is_some());
    }

    #[tokio::test]
    async fn test_concurrent_issue_no_collision() {
        let table = std::sync::Arc::new(RemoteSessionTable::new());
        let mut handles = Vec::new();
        for i in 0..32u32 {
            let t = table.clone();
            handles.push(tokio::spawn(async move {
                let (sid, token) = t
                    .issue(&format!("t{i}"), &format!("c{i}"), RemoteMode::Control)
                    .await;
                (sid, token)
            }));
        }
        let mut sids = std::collections::HashSet::new();
        let mut tokens = std::collections::HashSet::new();
        for h in handles {
            let (sid, token) = h.await.unwrap();
            assert!(sids.insert(sid), "session_id 重复");
            assert!(tokens.insert(token), "token 重复");
        }
        assert_eq!(sids.len(), 32);
        assert_eq!(table.session_count().await, 32);
    }

    #[tokio::test]
    async fn test_remove_for_client_returns_peer() {
        let table = RemoteSessionTable::new();
        let (a, _) = table
            .issue("target", "controller", RemoteMode::Control)
            .await;
        let (b, _) = table
            .issue("other-target", "controller", RemoteMode::Control)
            .await;
        let (c, _) = table.issue("x", "y", RemoteMode::Control).await;

        let mut removed = table.remove_for_client("controller").await;
        removed.sort();
        assert_eq!(removed.len(), 2);
        assert!(removed
            .iter()
            .all(|(_, peer)| peer == "target" || peer == "other-target"));
        assert!(table.take(&a).await.is_none());
        assert!(table.take(&b).await.is_none());
        // 不相关的会话保留
        assert!(table.take(&c).await.is_some());
    }

    // ── 服务端集成：门控 / 列表 / rd_id ──

    async fn state_with_rustdesk(enabled: bool) -> ServerState {
        let db = Database::new(":memory:").await.unwrap();
        db.save_server_auth("secret").await.unwrap();
        let state = ServerState::with_db(db);
        state
            .set_dynamic_config(DynamicConfig {
                log_level: "info".to_string(),
                llm_request_logging: true,
                ss: None,
                trojan: None,
                rustdesk: Some(RustdeskDynamicConfig {
                    enabled,
                    rendezvous_port: 21116,
                    relay_port: 21117,
                    key: "pubkey".to_string(),
                    relay_server: String::new(),
                    always_use_relay: false,
                    local_ip: String::new(),
                    mask: String::new(),
                }),
                reverse_proxy: crate::mgmt::dynamic_config::ReverseProxySettings {
                    max_connections: 10000,
                    connection_timeout_secs: 30,
                    buffer_size: 8192,
                },
                dns: crate::mgmt::dynamic_config::DnsSettings {
                    tunnel_domain: "tunnel.local".to_string(),
                    mesh_domain: "mesh.local".to_string(),
                },
            })
            .await;
        state
    }

    async fn register(
        state: &ServerState,
        name: &str,
        version: &str,
    ) -> mpsc::Receiver<ControlMessage> {
        let (tx, rx) = mpsc::channel(32);
        state
            .client_registry
            .as_ref()
            .unwrap()
            .register(name, None, Some(version.to_string()), "secret", tx)
            .await
            .unwrap();
        rx
    }

    #[tokio::test]
    async fn test_old_client_receives_no_remote_variants() {
        // 版本门控：0.9.0 老客户端收不到任何远程变体（否则 bincode 反序列化失败断连）。
        let state = state_with_rustdesk(true).await;
        let mut rx = register(&state, "old", "0.9.0").await;

        assert!(!push_remote_config(&state, "old").await);
        broadcast_remote_peer_list(&state).await;

        // 通道内不应出现任何消息
        let got = tokio::time::timeout(Duration::from_millis(150), rx.recv()).await;
        assert!(got.is_err(), "老客户端不应收到远程变体，却收到 {got:?}");
    }

    #[tokio::test]
    async fn test_push_remote_config_enabled_carries_params_and_rd_id() {
        let state = state_with_rustdesk(true).await;
        let mut rx = register(&state, "nas", "0.10.0").await;

        assert!(push_remote_config(&state, "nas").await);
        let msg = rx.recv().await.expect("should receive RemoteCapability");
        match msg {
            ControlMessage::RemoteCapability { params, rd_id } => {
                let p = params.expect("rustdesk enabled → params present");
                assert_eq!(p.rendezvous_host, "");
                assert_eq!(p.rendezvous_port, 21116);
                assert_eq!(p.relay_port, 21117);
                assert_eq!(p.public_key, "pubkey");
                assert_eq!(rd_id.len(), 9);
                assert!(rd_id.chars().all(|c| c.is_ascii_digit()));
            }
            other => panic!("expected RemoteCapability, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_rd_id_allocation_is_idempotent() {
        // 两次推送应复用同一 rd_id（persistence 侧幂等，端到端验证）。
        let state = state_with_rustdesk(true).await;
        let mut rx = register(&state, "nas", "0.10.0").await;

        assert!(push_remote_config(&state, "nas").await);
        let first = match rx.recv().await.unwrap() {
            ControlMessage::RemoteCapability { rd_id, .. } => rd_id,
            other => panic!("expected RemoteCapability, got {other:?}"),
        };
        assert!(push_remote_config(&state, "nas").await);
        let second = match rx.recv().await.unwrap() {
            ControlMessage::RemoteCapability { rd_id, .. } => rd_id,
            other => panic!("expected RemoteCapability, got {other:?}"),
        };
        assert_eq!(first, second, "rd_id 分配应幂等");
        assert_eq!(
            state
                .client_registry
                .as_ref()
                .unwrap()
                .db()
                .get_remote_id("nas")
                .await
                .unwrap()
                .as_deref(),
            Some(first.as_str())
        );
    }

    #[tokio::test]
    async fn test_push_remote_config_disabled_is_params_none() {
        let state = state_with_rustdesk(false).await;
        let mut rx = register(&state, "nas", "0.10.0").await;
        assert!(push_remote_config(&state, "nas").await);
        match rx.recv().await.unwrap() {
            ControlMessage::RemoteCapability { params, rd_id } => {
                assert!(params.is_none());
                assert_eq!(rd_id, "");
            }
            other => panic!("expected RemoteCapability, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_build_remote_peer_list_only_enabled_online() {
        let state = state_with_rustdesk(true).await;
        let registry = state.client_registry.as_ref().unwrap();

        let _rx_a = register(&state, "a", "0.10.0").await;
        let _rx_b = register(&state, "b", "0.10.0").await;
        let _rx_old = register(&state, "old", "0.9.0").await;

        // a 开启被控并分配 rd_id；b 未开启；old 版本不支持
        let a = registry.get("a").await.unwrap();
        a.set_remote_caps(true, "macos".to_string(), 2);
        a.set_rd_id("123456789".to_string());
        let b = registry.get("b").await.unwrap();
        b.set_remote_caps(false, "linux".to_string(), 1);
        b.set_rd_id("987654321".to_string());

        let peers = build_remote_peer_list(&state, None).await;
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].client_name, "a");
        assert_eq!(peers[0].platform, "macos");
        assert_eq!(peers[0].displays, 2);
        assert_eq!(peers[0].rd_id, "123456789");
        // hostname 缺失 → display_name 回落客户端名
        assert_eq!(peers[0].display_name, "a");

        // 排除自身：a 自己拉列表时不应看到 a（否则点一下就是连自己）。
        let self_excluded = build_remote_peer_list(&state, Some("a")).await;
        assert!(self_excluded.is_empty(), "a 不应在自己的列表里看到自己");
    }

    #[tokio::test]
    async fn test_broadcast_reaches_supported_clients_only() {
        let state = state_with_rustdesk(true).await;
        let registry = state.client_registry.as_ref().unwrap();
        let mut rx_new = register(&state, "new", "0.10.0").await;
        let _rx_peer = register(&state, "peer", "0.10.0").await;
        let mut rx_old = register(&state, "old", "0.9.0").await;

        // new 与 peer 都开启被控；new 广播后应只看到 peer（自己被排除）。
        for (name, platform, rd_id) in [
            ("new", "windows", "111222333"),
            ("peer", "macos", "444555666"),
        ] {
            let e = registry.get(name).await.unwrap();
            e.set_remote_caps(true, platform.to_string(), 1);
            e.set_rd_id(rd_id.to_string());
        }

        broadcast_remote_peer_list(&state).await;

        let msg = rx_new.recv().await.expect("new client should receive list");
        match msg {
            ControlMessage::RemotePeerList { peers } => {
                assert_eq!(peers.len(), 1);
                assert_eq!(peers[0].client_name, "peer", "列表须排除收件人自身");
            }
            other => panic!("expected RemotePeerList, got {other:?}"),
        }
        let got = tokio::time::timeout(Duration::from_millis(150), rx_old.recv()).await;
        assert!(got.is_err(), "老客户端不应收到 RemotePeerList");
    }

    #[tokio::test]
    async fn test_disconnect_removes_peer_from_broadcast() {
        // 客户端离线后别人的「可远程」列表必须跟着更新，否则列表里会一直挂着一个
        // 点不动的条目（点下去只有「目标客户端不在线」）。
        let state = state_with_rustdesk(true).await;
        let registry = state.client_registry.as_ref().unwrap();
        let mut rx_new = register(&state, "new", "0.10.0").await;
        let _rx_peer = register(&state, "peer", "0.10.0").await;

        for (name, platform, rd_id) in [
            ("new", "windows", "111222333"),
            ("peer", "macos", "444555666"),
        ] {
            let e = registry.get(name).await.unwrap();
            e.set_remote_caps(true, platform.to_string(), 1);
            e.set_rd_id(rd_id.to_string());
        }

        broadcast_remote_peer_list(&state).await;
        match rx_new.recv().await.expect("应收到列表") {
            ControlMessage::RemotePeerList { peers } => assert_eq!(peers.len(), 1),
            other => panic!("expected RemotePeerList, got {other:?}"),
        }

        // peer 离线 → 广播出的列表应不再含它。
        registry.disconnect("peer", "test").await;
        broadcast_remote_peer_list(&state).await;
        match rx_new.recv().await.expect("应收到更新后的列表") {
            ControlMessage::RemotePeerList { peers } => {
                assert!(peers.is_empty(), "离线客户端不应还在列表里，实得 {peers:?}");
            }
            other => panic!("expected RemotePeerList, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_broadcast_skipped_when_rustdesk_disabled() {
        // 未启用 RustDesk 时谁都不可能成为可远程对端，广播空列表只是噪音。
        let state = state_with_rustdesk(false).await;
        let mut rx = register(&state, "new", "0.10.0").await;
        broadcast_remote_peer_list(&state).await;
        let got = tokio::time::timeout(Duration::from_millis(150), rx.recv()).await;
        assert!(got.is_err(), "未启用 RustDesk 时不应广播 RemotePeerList");
    }

    #[tokio::test]
    async fn test_remote_params_normalizes_secret_key_to_public() {
        // 回归防线：DB 里的 key 列可能是 Web UI「生成」写入的 64 字节 NaCl 私钥。
        // 下发给客户端的必须是归一化后的**公钥字符串**——送私钥既是泄露，也会让
        // hbbs 侧的 licence_key 比对永远失败。
        let state = state_with_rustdesk(true).await;
        let sk = ServerKey::generate();
        let secret = sk.to_secret_base64().expect("generated key can sign");
        let public = sk.key().to_owned();
        assert_ne!(secret, public, "私钥与公钥字符串必须不同");

        let mut cfg = state.dynamic_config.read().await.clone();
        cfg.rustdesk.as_mut().unwrap().key = secret;
        state.set_dynamic_config(cfg).await;

        let params = remote_params(&state).await.expect("enabled → params");
        assert_eq!(params.public_key, public, "应下发公钥字符串");
        assert_ne!(
            params.public_key.len(),
            88,
            "绝不能下发 64 字节私钥的 base64"
        );
    }

    #[tokio::test]
    async fn test_remote_params_none_when_key_unparsable() {
        // base64 可解但长度非法（既非 32 也非 64）→ ServerKey::parse 报错 →
        // 视为不可用，而不是把坏 key 透传给客户端。
        let state = state_with_rustdesk(true).await;
        let mut cfg = state.dynamic_config.read().await.clone();
        cfg.rustdesk.as_mut().unwrap().key = "AAAA".to_string(); // base64 → 3 字节
        state.set_dynamic_config(cfg).await;

        assert!(remote_params(&state).await.is_none());
    }

    #[tokio::test]
    async fn test_connect_request_denied_when_target_offline() {
        let state = state_with_rustdesk(true).await;
        let registry = state.client_registry.as_ref().unwrap();
        let mut rx_ctrl = register(&state, "ctrl", "0.10.0").await;
        let ctrl = registry.get("ctrl").await.unwrap();

        handle_remote_connect_request(&state, &ctrl, "ghost", RemoteMode::Control).await;

        match rx_ctrl.recv().await.unwrap() {
            ControlMessage::RemoteSessionResponse {
                accepted, reason, ..
            } => {
                assert!(!accepted);
                assert!(reason.unwrap_or_default().contains("不在线"));
            }
            other => panic!("expected RemoteSessionResponse, got {other:?}"),
        }
        assert_eq!(state.remote_sessions.session_count().await, 0);
    }

    #[tokio::test]
    async fn test_connect_request_denied_when_target_not_enabled() {
        let state = state_with_rustdesk(true).await;
        let registry = state.client_registry.as_ref().unwrap();
        let mut rx_ctrl = register(&state, "ctrl", "0.10.0").await;
        let _rx_target = register(&state, "target", "0.10.0").await;
        let ctrl = registry.get("ctrl").await.unwrap();

        handle_remote_connect_request(&state, &ctrl, "target", RemoteMode::Control).await;
        match rx_ctrl.recv().await.unwrap() {
            ControlMessage::RemoteSessionResponse {
                accepted, reason, ..
            } => {
                assert!(!accepted);
                assert!(reason.unwrap_or_default().contains("未开启"));
            }
            other => panic!("expected RemoteSessionResponse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_connect_request_grant_and_incoming() {
        let state = state_with_rustdesk(true).await;
        let registry = state.client_registry.as_ref().unwrap();
        let mut rx_ctrl = register(&state, "ctrl", "0.10.0").await;
        let mut rx_target = register(&state, "target", "0.10.0").await;
        let target = registry.get("target").await.unwrap();
        target.set_remote_caps(true, "macos".to_string(), 1);
        target.set_rd_id("555666777".to_string());
        let ctrl = registry.get("ctrl").await.unwrap();

        handle_remote_connect_request(&state, &ctrl, "target", RemoteMode::ViewOnly).await;

        // 被控端收到 Incoming
        let (sid, token) = match rx_target.recv().await.unwrap() {
            ControlMessage::RemoteSessionIncoming {
                session_id,
                token,
                from_name,
                mode,
            } => {
                assert_eq!(from_name, "ctrl");
                assert_eq!(mode, RemoteMode::ViewOnly);
                (session_id, token)
            }
            other => panic!("expected RemoteSessionIncoming, got {other:?}"),
        };
        // 控制器收到 Grant
        match rx_ctrl.recv().await.unwrap() {
            ControlMessage::RemoteConnectGrant {
                session_id,
                token: grant_token,
                target_rd_id,
                target_platform,
                params,
            } => {
                assert_eq!(session_id, sid);
                assert_eq!(grant_token, token);
                assert_eq!(target_rd_id, "555666777");
                assert_eq!(target_platform, "macos");
                assert_eq!(params.rendezvous_port, 21116);
            }
            other => panic!("expected RemoteConnectGrant, got {other:?}"),
        }
        assert_eq!(state.remote_sessions.session_count().await, 1);
    }

    #[tokio::test]
    async fn test_session_response_forwarded_and_denial_cleans_up() {
        let state = state_with_rustdesk(true).await;
        let registry = state.client_registry.as_ref().unwrap();
        let mut rx_ctrl = register(&state, "ctrl", "0.10.0").await;
        let _rx_target = register(&state, "target", "0.10.0").await;
        let target = registry.get("target").await.unwrap();
        target.set_remote_caps(true, "linux".to_string(), 1);
        target.set_rd_id("444555666".to_string());
        let ctrl = registry.get("ctrl").await.unwrap();

        handle_remote_connect_request(&state, &ctrl, "target", RemoteMode::Control).await;
        let sid = match rx_ctrl.recv().await.unwrap() {
            ControlMessage::RemoteConnectGrant { session_id, .. } => session_id,
            other => panic!("expected RemoteConnectGrant, got {other:?}"),
        };

        // 被控端拒绝 → 控制器收到 forwarded Response，会话被清理
        handle_session_response(&state, "target", &sid, false, Some("用户拒绝".into())).await;
        match rx_ctrl.recv().await.unwrap() {
            ControlMessage::RemoteSessionResponse {
                session_id,
                accepted,
                reason,
            } => {
                assert_eq!(session_id, sid);
                assert!(!accepted);
                assert_eq!(reason.as_deref(), Some("用户拒绝"));
            }
            other => panic!("expected RemoteSessionResponse, got {other:?}"),
        }
        assert!(state.remote_sessions.take(&sid).await.is_none());
    }

    #[tokio::test]
    async fn test_session_response_accept_keeps_session() {
        let state = state_with_rustdesk(true).await;
        let registry = state.client_registry.as_ref().unwrap();
        let mut rx_ctrl = register(&state, "ctrl", "0.10.0").await;
        let _rx_target = register(&state, "target", "0.10.0").await;
        let target = registry.get("target").await.unwrap();
        target.set_remote_caps(true, "linux".to_string(), 1);
        target.set_rd_id("444555666".to_string());
        let ctrl = registry.get("ctrl").await.unwrap();

        handle_remote_connect_request(&state, &ctrl, "target", RemoteMode::Control).await;
        let sid = match rx_ctrl.recv().await.unwrap() {
            ControlMessage::RemoteConnectGrant { session_id, .. } => session_id,
            other => panic!("expected RemoteConnectGrant, got {other:?}"),
        };
        handle_session_response(&state, "target", &sid, true, None).await;
        match rx_ctrl.recv().await.unwrap() {
            ControlMessage::RemoteSessionResponse {
                session_id,
                accepted,
                ..
            } => {
                assert_eq!(session_id, sid);
                assert!(accepted);
            }
            other => panic!("expected RemoteSessionResponse, got {other:?}"),
        }
        // 接受后会话保留（等数据面 / 后续 End）
        assert!(state.remote_sessions.get(&sid).await.is_some());
    }

    #[tokio::test]
    async fn test_client_disconnected_ends_sessions_and_notifies_peer() {
        let state = state_with_rustdesk(true).await;
        let registry = state.client_registry.as_ref().unwrap();
        let mut rx_ctrl = register(&state, "ctrl", "0.10.0").await;
        let _rx_target = register(&state, "target", "0.10.0").await;
        let target = registry.get("target").await.unwrap();
        target.set_remote_caps(true, "linux".to_string(), 1);
        target.set_rd_id("777888999".to_string());
        let ctrl = registry.get("ctrl").await.unwrap();

        handle_remote_connect_request(&state, &ctrl, "target", RemoteMode::Control).await;
        let sid = match rx_ctrl.recv().await.unwrap() {
            ControlMessage::RemoteConnectGrant { session_id, .. } => session_id,
            other => panic!("expected RemoteConnectGrant, got {other:?}"),
        };

        // 被控端断开 → 控制器收到 End，会话被清理
        handle_client_disconnected(&state, "target").await;
        match rx_ctrl.recv().await.unwrap() {
            ControlMessage::RemoteSessionEnd { session_id, reason } => {
                assert_eq!(session_id, sid);
                assert_eq!(reason, "对端已离线");
            }
            other => panic!("expected RemoteSessionEnd, got {other:?}"),
        }
        assert_eq!(state.remote_sessions.session_count().await, 0);
    }

    #[tokio::test]
    async fn test_session_end_notifies_peer_and_cleans_up() {
        let state = state_with_rustdesk(true).await;
        let registry = state.client_registry.as_ref().unwrap();
        let mut rx_ctrl = register(&state, "ctrl", "0.10.0").await;
        let mut rx_target = register(&state, "target", "0.10.0").await;
        let target = registry.get("target").await.unwrap();
        target.set_remote_caps(true, "linux".to_string(), 1);
        target.set_rd_id("222333444".to_string());
        let ctrl = registry.get("ctrl").await.unwrap();

        handle_remote_connect_request(&state, &ctrl, "target", RemoteMode::Control).await;
        let sid = match rx_ctrl.recv().await.unwrap() {
            ControlMessage::RemoteConnectGrant { session_id, .. } => session_id,
            other => panic!("expected grant, got {other:?}"),
        };
        let _incoming = rx_target.recv().await.unwrap();

        // 被控端主动结束 → 控制器收到 End
        handle_session_end(&state, "target", &sid, "user ended").await;
        match rx_ctrl.recv().await.unwrap() {
            ControlMessage::RemoteSessionEnd { session_id, reason } => {
                assert_eq!(session_id, sid);
                assert_eq!(reason, "user ended");
            }
            other => panic!("expected RemoteSessionEnd, got {other:?}"),
        }
        assert_eq!(state.remote_sessions.session_count().await, 0);
    }
}
