//! ACP session 生命周期与连接处理：spawn/attach/detach/resume/kill、回合
//! prompt/cancel、ACP handshake（含 fs 请求转发与审批）、客户端控制消息路由。
//!
//! `impl AcpBridge` 的会话方法集中在 `session.rs`；stdio pump 见 [`super::pump`]，
//! 事件/消息落库见 [`super::store`]，idle reaper 见 [`super::reaper`]。

use futures_util::StreamExt;
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch, Mutex};

use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, CreateElicitationRequest, CreateElicitationResponse,
    DeleteSessionRequest, ElicitationAcceptAction, ElicitationAction, ElicitationMode,
    InitializeRequest, McpServer, McpServerHttp, NewSessionRequest, PermissionOption,
    PermissionOptionId, PermissionOptionKind, PromptRequest, ReadTextFileRequest,
    ReadTextFileResponse, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, ResumeSessionRequest, SelectedPermissionOutcome,
    SessionConfigId, SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigOptionValue, SessionConfigValueId, SessionId, SessionNotification,
    SetSessionConfigOptionRequest, TextContent, WriteTextFileRequest, WriteTextFileResponse,
};
#[cfg(feature = "rag")]
use crate::agent::memory::MemoryState;
#[cfg(feature = "rag")]
use crate::agent::mcp::McpHttpResponse;
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{ByteStreams, Client};

use crate::agent::acp_events::map_update;
use crate::agent::llm_bridge;
use crate::agent::spawner::AgentSpawner;
use crate::agent::{ApprovalOption, ApprovalResult, ElicitationResult};
use crate::db::Database;
use crate::persistence::db::agent::AgentWorkspaceRecord;
use rust_tunnel_common::ControlMessage;

use super::pump::{run_stdio_pump, FuturesIo};
use super::reaper::touch_activity;
use super::store::{flush_acp_turn_buffers, persist_acp_frame};
use super::{
    AcpBridge, ApproveFn, ElicitFn, PendingPrompt, SpawnedAgent, CONFIG_OPTION_TIMEOUT,
    MAX_PENDING_PROMPTS, READY_TIMEOUT, SPAWN_TIMEOUT,
};

impl AcpBridge {
    /// 惰性确保 session 就绪：stdio pump → LLM 代理 → agent 进程 → ACP handshake。
    ///
    /// 幂等：会话表已有条目则直接返回（同一 session 的多个 WS 连接/多条
    /// 消息不会重复 spawn —— Task 3 评审契约，防重复 AgentLlmProxyStart）。
    /// 幂等命中时刷新条目里的 WS 事件通道（重连/多标签页共用同一进程）。
    ///
    /// `ws_tx` 是 WS 事件通道：handshake 建立后 ACP 事件流经
    /// [`crate::agent::acp_events::map_update`] 推回前端。
    pub async fn ensure_session(
        &self,
        session_id: &str,
        workspace: &AgentWorkspaceRecord,
        ws_tx: mpsc::Sender<serde_json::Value>,
        conn_id: u64,
    ) -> Result<(), String> {
        // 幂等守卫 + 占位登记 + pump 基础设施一次锁内完成：并发 ensure_session
        // （同一 session 的多个 WS 连接/多条消息）看到占位条目直接短路，杜绝
        // 重复 AgentLlmProxyStart / AgentSpawnRequest（Task 3 评审契约）。
        // 校验也在此锁内做（无 await，持锁开销可忽略）。
        //
        // pump 基础设施（duplex + stdout 通道）在 spawn 之前建好：agent 进程
        // 一经拉起随时可能产出 stdout（AgentSpawnData{stdin:false}），此时
        // handle_spawn_data 能立即转发到 pump 通道 → duplex 缓冲，ACP 连接
        // 建立后消费——handshake 期间早产字节不会丢（Task 6 评审要求）。
        // 本会话 MCP 端点访问令牌（`/mcp/<token>` 路径）：仅 rag + memory 启用时
        // 铸造（32 hex）。token 随 SpawnedAgent 条目生灭——kill/重拉/reaper 移除
        // 条目即吊销，不落盘。写入占位条目 + 交 acp_handshake 注入 mcpServers。
        #[cfg(feature = "rag")]
        let mcp_token: Option<String> = if self.memory.is_some() {
            Some(format!("{:032x}", rand::random::<u128>()))
        } else {
            None
        };
        #[cfg(not(feature = "rag"))]
        let mcp_token: Option<String> = None;
        let pump_setup: Option<(tokio::io::DuplexStream, mpsc::Receiver<Vec<u8>>)> = {
            let mut sessions = self.sessions.lock().await;
            // 兜底杀进程后重拉：旧（exited）条目里排队等待的 prompt 迁移到新条目，
            // 避免 cancel 期间提交、进程死时尚未消费的等待消息在重拉后丢失。
            let mut migrated_prompts = VecDeque::new();
            match sessions.get(session_id) {
                Some(agent) if !agent.exited => {
                    // 已有活跃进程：仅刷新事件通道（多标签页/重连共用同一进程；
                    // 广播列表按 conn_id 替换/追加，流式/终态帧 fan-out 到全部连接，
                    // 新连接不会把正在运行回合的帧切走）。同时记录本连接的 conn_id，
                    // 供 detach 按身份清空。
                    if let Some(a) = sessions.get_mut(session_id) {
                        a.ws_tx = Some(ws_tx.clone());
                        a.ws_conn_id = conn_id;
                        // 多连接广播列表：同 conn_id（重连）替换 tx，新 conn_id
                        // （新标签页/新窗口）追加——流式帧 fan-out 到全部连接，
                        // 不再「最新连接获胜」劫持正在运行的回合。
                        if let Some(slot) = a
                            .ws_conns
                            .iter_mut()
                            .find(|(id, _)| *id == conn_id)
                        {
                            slot.1 = ws_tx.clone();
                        } else {
                            a.ws_conns.push((conn_id, ws_tx.clone()));
                        }
                        // 通知审批等待者连接已切换（重连/多标签页）：旧 conn_id
                        // 的 request_permission 等待立即拒绝，新连接才能看到审批卡。
                        a.ws_conn_watch.send_replace(conn_id);
                    }
                    return Ok(());
                }
                // 进程已退出的陈旧条目：视为不存在，移除后走 spawn 路径重拉。
                // 否则死进程会阻塞 respawn 直到 30 分钟 reaper 清掉它，后续
                // prompt 一直报 "agent process has exited"。
                Some(_) => {
                    tracing::info!(session_id, "re-spawning exited ACP session");
                    migrated_prompts = sessions
                        .get(session_id)
                        .map(|a| a.pending_prompts.clone())
                        .unwrap_or_default();
                    sessions.remove(session_id);
                }
                None => {}
            }
            if workspace.runtime_type != "host" {
                return Err("ACP 模式暂不支持 docker workspace，请改用 host 模式".into());
            }
            let (agent_io, pump_io) = tokio::io::duplex(64 * 1024);
            let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>(128);
            sessions.insert(
                session_id.to_string(),
                SpawnedAgent {
                    acp_session_id: None,
                    connection: None,
                    agent_io: Some(agent_io),
                    stdout_tx: Some(stdout_tx),
                    client_id: workspace.client_id.clone(),
                    ws_tx: Some(ws_tx.clone()),
                    ws_conns: vec![(conn_id, ws_tx.clone())],
                    ws_conn_id: conn_id,
                    ws_conn_watch: watch::channel(conn_id).0,
                    busy: false,
                    cancelled_turns: std::collections::HashSet::new(),
                    turn_generation: 0,
                    turn_started_at: None,
                    last_activity: std::time::Instant::now(),
                    exited: false,
                    turn_segments: Vec::new(),
                    config_options: Vec::new(),
                    available_commands: Vec::new(),
                    spawn_ready: watch::channel(false).0,
                    pending_prompts: migrated_prompts,
                    cancel_notify: Arc::new(tokio::sync::Notify::new()),
                    memory_block: None,
                    skill_list_block: None,
                    mcp_token: mcp_token.clone(),
                    file_hashes: HashMap::new(),
                },
            );
            Some((pump_io, stdout_rx))
        };
        if pump_setup.is_some() {
            // 新一轮 spawn 尝试：清除上一次的失败缓存，避免 wait_ready 在新
            // 尝试在途时消费到陈旧错误（见 spawn_failure 的消费条件）。
            self.spawn_errors.lock().await.remove(session_id);
            self.restore_pending_prompts(session_id).await;
        }

        // 拿锁期不 spawn（避免长时间持锁阻塞 prompt/cancel）。先解析客户端
        // 控制通道并启动 pump（ACP→进程 stdin 方向；进程 stdout 方向已在占位
        // 里就绪）。客户端离线时 pump 不启动——start_llm_proxy 随后报错，
        // 占位被移除（stdout_tx drop → pump 收尾），无资源泄漏。
        let client_id = workspace.client_id.clone();
        let control_tx = self.spawner.client_control_sender(&client_id).await;
        if let (Some((pump_io, stdout_rx)), Some(control_tx)) = (pump_setup, control_tx) {
            let pump_session_id = session_id.to_string();
            tokio::spawn(async move {
                run_stdio_pump(pump_io, stdout_rx, control_tx, pump_session_id).await;
            });
        }

        let agent_type = &workspace.agent_type;
        // 阶段耗时打点：wait_ready 超时只能看到「spawn 仍在进行」，各阶段
        // （协商/握手/配置注入）的耗时分布是定位卡点（冷启动慢 vs 隧道 RTT
        // vs agent 不响应 set_config_option）的关键证据。
        let pipeline_start = std::time::Instant::now();
        let outcome: Result<(), String> = async {
            // 0a) LLM 网关门禁：网关未注入（启动时无 provider → llm_state 为空）
            //     时 spawn 出的 agent 每个 LLM 请求都会 502——前置拦截，错误直接
            //     指向配置入口，而非等用户发消息后看到莫名其妙的 502。
            if self.gateway.is_none() {
                return Err(
                    "gateway: LLM 网关未配置（无可用 provider），请先在 LLM 网关页添加 provider 与模型"
                        .into(),
                );
            }
            // 0b) 模型配置门禁：session.model / workspace.llm_model_id / 全局默认
            //     任一即可。实际 LLM 请求按 session 从 DB 解析（resolve_effective_model，
            //     含「第一个可用」兜底），此处只防 spawn 后才发现无模型。校验失败走
            //     通用错误路径（outcome Err → 占位被移除，允许重试）。
            if !crate::agent::session::has_any_model_config(
                &self.db,
                session_id,
                workspace.llm_model_id.as_deref(),
            )
            .await
            .map_err(|e| format!("model_gate: {e}"))?
            {
                return Err("model_gate: workspace 与 session 均未配置 LLM 模型".into());
            }
            // 1) 客户端内嵌 LLM 回环代理
            let port = self
                .spawner
                .start_llm_proxy(&client_id, session_id, SPAWN_TIMEOUT)
                .await
                .map_err(|e| format!("llm_proxy: {e}"))?;
            tracing::info!(
                session_id,
                elapsed_ms = pipeline_start.elapsed().as_millis() as u64,
                "acp spawn stage: llm proxy ready (port {port})"
            );
            // 2) spawn agent 进程（env 注入 LLM 代理地址）。解析有效模型引用注入
            //    agent 进程（opencode 经 `OPENCODE_CONFIG_CONTENT` 注入；claude-code
            //    不用它，仍走 ACP set_config_option）。解析失败不阻断 spawn——
            //    best-effort，实际请求的 model 由 llm_bridge 每次按 session 从 DB
            //    重新解析覆盖。
            let spawn_model = crate::agent::session::resolve_effective_model(
                &self.db,
                None,
                session_id,
            )
            .await
            .ok();
            // opencode 枚举服务端启用的网关模型 id，注入配置的 provider.models：
            // 让 opencode 只能（且能）通过白名单 provider 的路由消费 LLM，流量经
            // 回环代理进隧道。失败/无 gateway 用空 vec（不阻断 spawn，provider 仍
            // 注入、无 models 枚举）；非 opencode 恒空。
            let available_models: Vec<String> = if agent_type == "opencode" {
                if let Some(gw) = self.gateway.as_ref() {
                    crate::llm::router::list_available_models(&gw.llm_state)
                        .await
                        .unwrap_or_default()
                        .iter()
                        .filter_map(|m| m["id"].as_str().map(String::from))
                        .collect()
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            };
            // Claude Code tier 模型映射：workspace.claude_tier_models（JSON object，
            // key ∈ {opus,sonnet,haiku}）→ ANTHROPIC_DEFAULT_*_MODEL 等 env。
            // 仅 claude-code 且非空时解析；单档解析失败 warn 跳过该档，不阻断 spawn。
            // **不注入 ANTHROPIC_MODEL**——spawn 时锁死默认模型会覆盖 session.model
            // 的每请求动态解析。
            let tier_envs: Vec<(String, String)> = if agent_type == "claude-code" {
                if let Some(raw) = workspace.claude_tier_models.as_deref() {
                    match serde_json::from_str::<serde_json::Value>(raw) {
                        Ok(serde_json::Value::Object(map)) => {
                            let mut envs = Vec::new();
                            for (tier, val) in map {
                                let Some(s) = val.as_str() else { continue };
                                if s.trim().is_empty() {
                                    continue;
                                }
                                match crate::agent::session::resolve_workspace_model_ref(
                                    &self.db,
                                    Some(s),
                                )
                                .await
                                {
                                    Ok(Some(resolved)) => {
                                        let (env_key, dup_key) = match tier.as_str() {
                                            "opus" => {
                                                ("ANTHROPIC_DEFAULT_OPUS_MODEL", None::<&str>)
                                            }
                                            "sonnet" => ("ANTHROPIC_DEFAULT_SONNET_MODEL", None),
                                            "haiku" => (
                                                "ANTHROPIC_DEFAULT_HAIKU_MODEL",
                                                Some("ANTHROPIC_SMALL_FAST_MODEL"),
                                            ),
                                            _ => continue,
                                        };
                                        envs.push((env_key.to_string(), resolved.clone()));
                                        if let Some(dk) = dup_key {
                                            envs.push((dk.to_string(), resolved));
                                        }
                                    }
                                    Ok(None) => {}
                                    Err(e) => {
                                        tracing::warn!(
                                            session_id,
                                            tier,
                                            "claude tier model resolve failed, skipped: {e}"
                                        );
                                    }
                                }
                            }
                            envs
                        }
                        Ok(_) => Vec::new(),
                        Err(e) => {
                            tracing::warn!(
                                session_id,
                                "claude_tier_models not a JSON object, skipped: {e}"
                            );
                            Vec::new()
                        }
                    }
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            };
            self.spawner
                .spawn_agent(
                    &client_id,
                    session_id,
                    agent_type,
                    workspace.agent_path.as_deref(),
                    port,
                    &workspace.root_path,
                    SPAWN_TIMEOUT,
                    spawn_model.as_deref(),
                    &available_models,
                    &tier_envs,
                )
                .await
                .map_err(|e| format!("spawn_agent: {e}"))?;
            tracing::info!(
                session_id,
                elapsed_ms = pipeline_start.elapsed().as_millis() as u64,
                "acp spawn stage: agent process spawned"
            );
            // 3) ACP handshake（stdio pump 已就绪，此步建立 ACP 连接 + WS 接线；
            // ws_tx 由连接任务的处理器每次事件从会话条目动态解析，无需传入）。
            // 重拉（断连过久/reaper 杀进程后）时从 DB 取持久化的 ACP session id，
            // 交 handshake 优先 session/resume 恢复上下文；读失败视为无（重拉走全新）。
            let persisted_acp_session_id = self
                .db
                .agent_get_session(session_id)
                .await
                .ok()
                .flatten()
                .and_then(|s| s.acp_session_id);
            let root_path = workspace.root_path.clone();
            self.acp_handshake(
                session_id,
                agent_type,
                &root_path,
                persisted_acp_session_id,
                port,
                mcp_token,
            )
            .await
            .map_err(|e| format!("handshake: {e}"))?;
            tracing::info!(
                session_id,
                elapsed_ms = pipeline_start.elapsed().as_millis() as u64,
                "acp spawn stage: handshake complete"
            );
            Ok(())
        }
        .await;
        if let Err(e) = &outcome {
            // spawn 失败：移除占位，允许后续重试；同时缓存真实原因——已在
            // wait_ready 等待的并发调用（预 spawn 在途时发了首条消息）在
            // Sender drop 后能拿到它，而非误导性的 "session not spawned"。
            self.sessions.lock().await.remove(session_id);
            // start_llm_proxy 成功但 spawn_agent/handshake 失败时，代理端口仍
            // 在客户端监听：下发 AgentLlmProxyStop 释放（best-effort）。
            self.spawner.stop_llm_proxy(&client_id, session_id).await;
            self.spawn_errors
                .lock()
                .await
                .insert(session_id.to_string(), e.clone());
            // 持久化归因（best-effort）：重启后/会话列表仍可追溯最近一次失败
            // 阶段（内存 spawn_errors 重启即丢）。
            if let Err(db_err) = self
                .db
                .agent_update_session_spawn_error(session_id, Some(e))
                .await
            {
                tracing::warn!(session_id, "persist spawn error failed: {db_err}");
            }
            return outcome;
        }
        // spawn 成功：清空持久化的失败归因（新一轮尝试的错误已被上面覆盖；
        // 此处清掉历史残留，DTO 不再误报旧错误）。
        if let Err(db_err) = self
            .db
            .agent_update_session_spawn_error(session_id, None)
            .await
        {
            tracing::warn!(session_id, "clear spawn error failed: {db_err}");
        }
        // 握手成功：workspace 级 overrides 注入先于 session 级 config_state 回放
        // ——用户显式选择（config_state）覆盖 workspace 默认值。在 acp_handshake
        // 返回后才执行：该函数只收 session_id/root_path，此处持有 workspace 记录。
        self.apply_config_overrides(session_id, workspace).await;
        self.replay_config_state(session_id).await;
        tracing::info!(
            session_id,
            elapsed_ms = pipeline_start.elapsed().as_millis() as u64,
            "acp spawn stage: config injection complete"
        );
        // 配置注入完成后才放行 wait_ready：连接预 spawn（后台任务）场景下，
        // 用户路径的 wait_ready 经 watch 通道等待此信号——必须延后到 overrides/
        // config_state 已生效，首条 prompt 才不与在途 set_config_option 竞态
        // （恢复旧顺序：replay 先于 spawn_ready）。
        // 必须用 send_modify 而非 send：spawn_ready 建通道时只存了 Sender
        // （Receiver 当场 drop），预 spawn 在首条消息（首个 subscribe）之前
        // 完成时 receiver_count==0，`send` 会静默失败且**不写入新值**——
        // wait_ready 将永远看到 false，空等整个 READY_TIMEOUT 后误报
        // 「spawn 仍在进行」。send_modify 无接收者也更新值，恰好匹配
        // 「就绪状态置位」语义。
        if let Some(a) = self.sessions.lock().await.get_mut(session_id) {
            a.spawn_ready.send_modify(|ready| *ready = true);
        }
        // 回放完成后把最终快照推给当前 WS 连接（重连场景前端立即可见）。
        if let Some(frame) = self.session_state_frame(session_id).await {
            if let Some(ws_tx) = current_ws_tx(&self.sessions, session_id).await {
                let _ = ws_tx.try_send(frame);
            }
        }
        outcome
    }

    /// ACP handshake：initialize → 会话建立（session/new，或带持久化 id 时优先
    /// session/resume 恢复上下文）。
    ///
    /// 从占位条目取走 duplex 的 ACP 端，spawn 一个常驻连接任务（crate 的
    /// `Client` 角色 + `ByteStreams` transport），任务内完成
    /// `initialize` + 会话建立（`session/resume` 或 `session/new`），把
    /// `ConnectionTo<Agent>` 与 ACP session id 写回会话条目；随后 main_fn 挂起
    /// 等待 incoming EOF（保持连接存活，直到进程退出/会话被杀）。通知
    /// （`session/update`）经 [`map_update`] 映射后推会话条目当前的 ws_tx——
    /// 处理器每次事件动态解析，重连自动切到新连接；权限请求
    /// （`session/request_permission`）走审批回调。
    ///
    /// `persisted_acp_session_id` 为上次会话建立落库的 ACP session id（断线过久
    /// 重拉时从 DB 取）。agent 声明支持 `session/resume` 时优先 resume（凭 id 从
    /// 客户端磁盘恢复 agent 侧对话上下文），失败/不支持回退 `session/new`。
    /// 最终生效的 session id 落库（best-effort），供下次重拉继续 resume。
    ///
    /// `mcp_port` 是客户端内嵌 LLM 回环代理监听端口，`mcp_token` 是本会话 MCP
    /// 端点访问令牌（ensure_session 铸造，None 时不注入）。两者只在 agent 声明
    /// `mcp_capabilities.http` 时用于把 remember MCP server 注入 session/new 与
    /// session/resume 的 `mcpServers`（URL `http://127.0.0.1:{mcp_port}/mcp/{token}`，
    /// agent 经回环代理转发到服务端 `handle_mcp_tunnel`）。降级：能力缺失/无 token
    /// 不注入、不报错（仅 info 日志）。
    ///
    /// 注意：`agent_client_protocol::Client` 是角色标记（unit struct），并非
    /// 连接句柄；连接句柄是 `ConnectionTo<Agent>`。每 session 一条专用连接，
    /// 通知无需按 session id 过滤。
    pub(super) async fn acp_handshake(
        &self,
        session_id: &str,
        agent_type: &str,
        cwd: &str,
        persisted_acp_session_id: Option<String>,
        mcp_port: u16,
        mcp_token: Option<String>,
    ) -> Result<(), String> {
        // 取走 duplex 的 ACP 端（占用即移除；后续 kill 不再持有）。
        let agent_io = {
            let mut sessions = self.sessions.lock().await;
            sessions
                .get_mut(session_id)
                .ok_or_else(|| "session not spawned".to_string())?
                .agent_io
                .take()
                .ok_or_else(|| "ACP stdio pump not initialized".to_string())?
        };

        let sid = session_id.to_string();
        let cwd = cwd.to_string();
        // 拥有型拷贝：spawn 的连接任务需 'static（MCP 注入日志带 agent_type）。
        let agent_type = agent_type.to_string();
        let approval = self.approval.clone();
        let elicitation = self.elicitation.clone();
        let sessions = self.sessions.clone();
        let db = self.db.clone();
        let spawner = self.spawner.clone();
        let (setup_tx, setup_rx) = oneshot::channel();

        // 常驻连接任务：connect_with 的 main_fn 完成 handshake 后保持挂起，
        // 连接生命周期 = ACP 会话生命周期。
        let sid_log = sid.clone();
        tokio::spawn(async move {
            let result = Client
                .builder()
                .on_receive_notification(
                    {
                        let sid = sid.clone();
                        let sessions = sessions.clone();
                        let db = db.clone();
                        // tool_call_id → 工具名 缓存：ACP 的 ToolCallUpdate 常不带
                        // title，而前端 ChatStream 按 tool_result.name === tool_call.name
                        // 匹配卡片——从前序 ToolCall 事件的 title 补名，保证结果能挂上。
                        let mut tool_names: HashMap<String, String> = HashMap::new();
                        // tool_call_id → 已落库 args 缓存：claude-code 的 ToolCall 首帧
                        // rawInput 常是 {}（参数尚未到达），真正的命令/路径经后续
                        // ToolCallUpdate.rawInput 到达。若仅推送 WS 而不回填 DB，
                        // 重载后 tool_result 卡片从 tool_calls 行取到的仍是空 args
                        // （历史卡片无操作内容）。缓存已落值，后续含新 rawInput 的帧
                        // UPDATE 原 tool_calls 行补齐。
                        let mut tool_args: HashMap<String, String> = HashMap::new();
                        async move |notification: SessionNotification, _cx| {
                            // 专用连接：所有通知都属于本 session。tool_call 名缓存
                            // 先填（会话 detached 期间也可累积，重连后 tool_result
                            // 仍能补名）。
                            if let agent_client_protocol::schema::v1::SessionUpdate::ToolCall(tc) =
                                &notification.update
                            {
                                tool_names.insert(
                                    tc.tool_call_id.to_string(),
                                    tc.title.clone(),
                                );
                            }
                            // 会话存活守卫（评审修复：persist 移出 ws_tx guard）：
                            // touch_activity 只刷新 last_activity、不读 ws_tx——断线
                            // （detach_ws_tx 置 None）期间到达的事件必须继续落库，
                            // 后台跑完的回合同样可追溯。条目被 kill/reaper 回收后
                            // 返回 false，本帧放弃（不落库、不推送）。
                            if !touch_activity(&sessions, &sid).await {
                                return Ok(());
                            }
                            // 状态快照维护：config_option_update 全量替换；
                            // current_mode_update 只改写 mode 项的 current_value
                            // （claude-code-acp 改 mode 时两种通知都会发，顺序不定，
                            // 两处幂等保证最终一致）。只维护内存快照——帧推送已由
                            // map_update 完成（Task 2），这里不重复推。
                            match &notification.update {
                                agent_client_protocol::schema::v1::SessionUpdate::ConfigOptionUpdate(
                                    upd,
                                ) => {
                                    if let Some(a) = sessions.lock().await.get_mut(&sid) {
                                        a.config_options = upd.config_options.clone();
                                    }
                                }
                                agent_client_protocol::schema::v1::SessionUpdate::CurrentModeUpdate(
                                    mode,
                                ) => {
                                    if let Some(a) = sessions.lock().await.get_mut(&sid) {
                                        for o in &mut a.config_options {
                                            if matches!(
                                                &o.category,
                                                Some(SessionConfigOptionCategory::Mode)
                                            ) {
                                                if let SessionConfigKind::Select(sel) =
                                                    &mut o.kind
                                                {
                                                    sel.current_value = SessionConfigValueId::new(
                                                        mode.current_mode_id.0.clone(),
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                                agent_client_protocol::schema::v1::SessionUpdate::AvailableCommandsUpdate(
                                    upd,
                                ) => {
                                    if let Some(a) = sessions.lock().await.get_mut(&sid) {
                                        a.available_commands = upd.available_commands.clone();
                                    }
                                }
                                _ => {}
                            }
                            if let Some(mut frame) = map_update(&notification.update) {
                                // owned 拷贝：后面要对 frame 赋值，不能持 &str 借用
                                let frame_id = frame
                                    .get("id")
                                    .and_then(|v| v.as_str())
                                    .map(str::to_string);
                                // ToolCallUpdate 缺 title 时从缓存补 name；缺 args 时
                                // 从已落库值补（最终 rawOutput 帧常只带结果不带参数，
                                // 卡片展开详情需要 args）。
                                if frame["type"] == "tool_result" {
                                    if let Some(id) = &frame_id {
                                        if frame.get("name").is_none() {
                                            if let Some(name) = tool_names.get(id) {
                                                frame["name"] =
                                                    serde_json::Value::String(name.clone());
                                            }
                                        }
                                        if frame.get("args").is_none() {
                                            if let Some(args) = tool_args.get(id) {
                                                frame["args"] =
                                                    serde_json::Value::String(args.clone());
                                            }
                                        }
                                    }
                                }
                                // 落库（best-effort，不依赖 WS 存活）：tool/plan 直接
                                // 落；文本/thought 缓冲到终态合并落一行。断线期间
                                // 到达的帧同样落库——落库在推送之前、与推送解耦。
                                persist_acp_frame(&db, &sessions, &sid, &frame).await;
                                // args 回填：tool_call 帧登记已落库 args；tool_result 帧
                                // 携带了新 rawInput（与已落库值不同）时 UPDATE 原
                                // tool_calls 行补齐——重载后历史卡片才能看到操作内容。
                                match (frame["type"].as_str().unwrap_or(""), frame_id) {
                                    ("tool_call", Some(id)) => {
                                        if let Some(a) =
                                            frame.get("args").and_then(|v| v.as_str())
                                        {
                                            tool_args.insert(id.to_string(), a.to_string());
                                        }
                                    }
                                    ("tool_result", Some(id)) => {
                                        if let Some(a) =
                                            frame.get("args").and_then(|v| v.as_str())
                                        {
                                            // 空对象占位（"{}"）不算新信息：ToolCall 首帧
                                            // 的 rawInput={} 不能覆盖此前已回填的真参数。
                                            let meaningful = a.trim() != "{}";
                                            let persisted =
                                                tool_args.get(&id).map(String::as_str);
                                            if meaningful && persisted != Some(a) {
                                                if let Err(e) = db
                                                    .agent_update_tool_call_args(&sid, &id, a)
                                                    .await
                                                {
                                                    tracing::warn!(session_id = %sid,
                                                        "backfill tool_call args failed: {e}");
                                                }
                                                tool_args.insert(id.clone(), a.to_string());
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                                // 推送：广播到本会话所有连接（H5）——多标签页/多窗口
                                // 共用同一 ACP 进程，流式帧 fan-out 到全部连接，不再
                                // 「最新连接获胜」劫持：新打开的标签页不再把正在运行
                                // 回合的帧切走（原标签页永久卡 running）。连接列表空
                                // （全部断开）则跳过，不影响已完成的落库。try_send
                                // 丢帧（单连接消费跟不上）是实时流可容忍的，避免慢
                                // 消费端阻塞卡死整个 ACP 连接。
                                broadcast_ws_frame(&sessions, &sid, &frame, false).await;
                            }
                            Ok(())
                        }
                    },
                    agent_client_protocol::on_receive_notification!(),
                )
                .on_receive_request(
                    {
                        let sid = sid.clone();
                        let approval = approval.clone();
                        let sessions = sessions.clone();
                        async move |request: RequestPermissionRequest, responder, _cx| {
                            // 动态解析当前 WS 通道 + 连接标识 + 连接变化 watch
                            // （同 notification，评审 Finding 1）：审批弹层要推给
                            // 最新连接，而非 handshake 时捕获的旧通道。
                            let Some((ws_tx, conn_id, conn_rx)) =
                                current_ws_channel(&sessions, &sid).await
                            else {
                                // 会话已回收（kill/reaper）：无审批通道可推，立即拒绝。
                                let _ = responder.respond(RequestPermissionResponse::new(
                                    RequestPermissionOutcome::Cancelled,
                                ));
                                return Ok(());
                            };
                            // WS 已断开（detach 置 None）：构造一个立即失效的通道传给
                            // 审批回调，request_approval 发帧失败即按拒绝短路返回
                            // （评审 Finding 2：避免 5 分钟超时占用连接任务，阻塞
                            // agent 下一个工具调用）。若断开早于订阅，conn_rx 当前值
                            // 已 ≠ conn_id，approve_or_disconnect 直接短路拒绝。
                            let ws_tx = ws_tx.unwrap_or_else(|| {
                                let (tx, _rx) = mpsc::channel::<serde_json::Value>(1);
                                tx
                            });
                            let tool_name = request
                                .tool_call
                                .fields
                                .title
                                .clone()
                                .unwrap_or_else(|| "unknown tool".to_string());
                            let args_preview = request
                                .tool_call
                                .fields
                                .raw_input
                                .as_ref()
                                .map(acp_raw_to_string)
                                .unwrap_or_else(|| tool_name.clone());
                            // 透传 agent 给出的权限选项：用户可从中选具体选项（如
                            // AskUserQuestion / plan 审批），而非服务端硬编码挑选。
                            // options 为空时审批卡片保持 approve/deny 二元按钮。
                            let options: Vec<ApprovalOption> = request
                                .options
                                .iter()
                                .map(permission_option_to_approval)
                                .collect();
                            // 审批在途时连接断开/重连 → 立即拒绝（不等满审批超时），
                            // 避免审批帧缓冲在无人消费的旧 channel 里、新重连也看不到。
                            let result = approve_or_disconnect(
                                approval.clone(),
                                sid.clone(),
                                tool_name,
                                args_preview,
                                options,
                                ws_tx,
                                conn_id,
                                conn_rx,
                            )
                            .await;
                            let outcome = match result {
                                // 用户选中了具体选项：原样回传 option_id。
                                ApprovalResult::Selected(id) => Some(
                                    PermissionOptionId::from(id),
                                ),
                                // 无选项路径的批准/拒绝：fallback 到服务端挑默认选项
                                // （AllowAlways→AllowOnce / RejectAlways→RejectOnce）。
                                ApprovalResult::Approved => pick_option(
                                    &request.options,
                                    PermissionOptionKind::AllowAlways,
                                )
                                .or_else(|| {
                                    pick_option(
                                        &request.options,
                                        PermissionOptionKind::AllowOnce,
                                    )
                                }),
                                ApprovalResult::Denied => pick_option(
                                    &request.options,
                                    PermissionOptionKind::RejectAlways,
                                )
                                .or_else(|| {
                                    pick_option(
                                        &request.options,
                                        PermissionOptionKind::RejectOnce,
                                    )
                                }),
                            };
                            match outcome {
                                Some(option_id) => {
                                    let _ = responder.respond(RequestPermissionResponse::new(
                                        RequestPermissionOutcome::Selected(
                                            SelectedPermissionOutcome::new(option_id),
                                        ),
                                    ));
                                }
                                None => {
                                    let _ = responder.respond(RequestPermissionResponse::new(
                                        RequestPermissionOutcome::Cancelled,
                                    ));
                                }
                            }
                            Ok(())
                        }
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    {
                        // fs/read_text_file：绝对路径 → 工作区相对路径 → 经隧道到
                        // 客户端沙箱读取，内容回包给 agent（claude-code 据此读项目文件）。
                        let sid = sid.clone();
                        let db = db.clone();
                        let sessions = sessions.clone();
                        let spawner = spawner.clone();
                        async move |request: ReadTextFileRequest, responder, _cx| {
                            let outcome = exec_fs_read(
                                &db,
                                &spawner,
                                &sessions,
                                &sid,
                                &request.path.to_string_lossy(),
                            )
                            .await;
                            match outcome {
                                Ok(content) => {
                                    let _ = responder.respond(ReadTextFileResponse::new(content));
                                }
                                Err(e) => {
                                    let _ = responder.respond_with_error(
                                        agent_client_protocol::util::internal_error(
                                            format!("fs/read_text_file failed: {e}"),
                                        ),
                                    );
                                }
                            }
                            Ok(())
                        }
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    {
                        // fs/write_text_file：同 read，写文件到客户端沙箱。
                        let sid = sid.clone();
                        let db = db.clone();
                        let sessions = sessions.clone();
                        let spawner = spawner.clone();
                        async move |request: WriteTextFileRequest, responder, _cx| {
                            let outcome = exec_fs_write(
                                &db,
                                &spawner,
                                &sessions,
                                &sid,
                                &request.path.to_string_lossy(),
                                &request.content,
                            )
                            .await;
                            match outcome {
                                Ok(()) => {
                                    let _ = responder.respond(WriteTextFileResponse::new());
                                }
                                Err(e) => {
                                    let _ = responder.respond_with_error(
                                        agent_client_protocol::util::internal_error(
                                            format!("fs/write_text_file failed: {e}"),
                                        ),
                                    );
                                }
                            }
                            Ok(())
                        }
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    {
                        // elicitation/create：仅支持 mode=form。schema 是复杂 serde
                        // 枚举（string/number/integer/boolean/array + oneOf/anyOf/
                        // _meta 等），后端不重建表单模型，`serde_json::to_value` 原样
                        // JSON 透传前端渲染。会话已回收/无 WS 通道 → 立即回 Cancel
                        // （错误会被 agent 视为硬失败，Cancel 是稳的降级）。未声明
                        // url 能力；防御性兜底：收到 url/other 模式 → Cancel + warn。
                        let sid = sid.clone();
                        let sessions = sessions.clone();
                        let elicitation = elicitation.clone();
                        async move |request: CreateElicitationRequest, responder, _cx| {
                            let schema = match &request.mode {
                                ElicitationMode::Form(form) => {
                                    serde_json::to_value(&form.requested_schema).unwrap_or_else(
                                        |_| {
                                            serde_json::json!({"type": "object", "properties": {}})
                                        },
                                    )
                                }
                                _ => {
                                    tracing::warn!(
                                        session_id = %sid,
                                        "elicitation/create: unsupported mode, cancelling"
                                    );
                                    let _ = responder.respond(
                                        CreateElicitationResponse::new(ElicitationAction::Cancel),
                                    );
                                    return Ok(());
                                }
                            };
                            // 动态解析当前 WS 通道 + 连接标识 + 连接变化 watch
                            // （同 request_permission，评审 Finding 1）。
                            let Some((ws_tx, conn_id, conn_rx)) =
                                current_ws_channel(&sessions, &sid).await
                            else {
                                // 会话已回收（kill/reaper）：无表单通道可推，立即取消。
                                let _ = responder.respond(CreateElicitationResponse::new(
                                    ElicitationAction::Cancel,
                                ));
                                return Ok(());
                            };
                            // WS 已断开（detach 置 None）：构造立即失效通道，
                            // request_elicitation 发送失败即取消（同审批路径）。
                            let ws_tx = ws_tx.unwrap_or_else(|| {
                                let (tx, _rx) = mpsc::channel::<serde_json::Value>(1);
                                tx
                            });
                            let result = elicit_or_disconnect(
                                elicitation.clone(),
                                sid.clone(),
                                request.message.clone(),
                                schema,
                                ws_tx,
                                conn_id,
                                conn_rx,
                            )
                            .await;
                            let response = match result {
                                ElicitationResult::Accept(content) => {
                                    CreateElicitationResponse::new(ElicitationAction::Accept(
                                        ElicitationAcceptAction::new().content(content),
                                    ))
                                }
                                ElicitationResult::Decline => {
                                    CreateElicitationResponse::new(ElicitationAction::Decline)
                                }
                                ElicitationResult::Cancel => {
                                    CreateElicitationResponse::new(ElicitationAction::Cancel)
                                }
                            };
                            let _ = responder.respond(response);
                            Ok(())
                        }
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .connect_with(
                    {
                        // DuplexStream 非 Clone：拆成读写半各包一层 futures 适配。
                        let (agent_rd, agent_wr) = tokio::io::split(agent_io);
                        ByteStreams::new(FuturesIo(agent_wr), FuturesIo(agent_rd))
                    },
                    async move |cx| {
                        // 1) initialize；2) 会话建立（session/resume 或 session/new）。
                        // 失败则 setup 报错并关连接。
                        let setup = async {
                            // 声明客户端 fs 能力：agent 才会通过 fs/read_text_file 与
                            // fs/write_text_file 请求读文件（服务端经隧道转发到客户端
                            // 沙箱执行）。不声明则 agent 静默降级（如报「不支持」）。
                            let capabilities = client_capabilities();
                            let init_resp = cx
                                .send_request(
                                    InitializeRequest::new(ProtocolVersion::V1)
                                        .client_capabilities(capabilities),
                                )
                                .block_task()
                                .await?;
                            // agent 声明 session/resume 能力 + 持有多余的持久化 id →
                            // 优先 resume（凭 id 从客户端磁盘恢复 agent 侧对话上下文，
                            // 解决断线过久/进程被杀后上下文丢失）。
                            let resume_capable = init_resp
                                .agent_capabilities
                                .session_capabilities
                                .resume
                                .is_some();
                            // remember MCP server 注入：agent 声明 mcp http 能力且本
                            // 会话铸造了 token → session/new 与 session/resume 都带
                            // mcpServers（两种会话建立路径都会连 MCP）。非 rag（token
                            // 恒 None）/ memory 未启用 / agent 无 http 能力 → 不注入，
                            // 仅 info 日志，降级不报错。opencode 1.18.18 已复验兼容
                            // （2026-08-16，SDK 1.29.0 StreamableHTTP：initialize JSON
                            // 单响应 + 202 notification + GET 405 均符合规范）；若未来
                            // opencode 升级破坏兼容，手动探针 tests/opencode_mcp_probe.rs
                            // 复验。
                            let mcp_http_capable = init_resp.agent_capabilities.mcp_capabilities.http;
                            let mcp_servers: Option<Vec<McpServer>> =
                                match (mcp_http_capable, &mcp_token) {
                                    (true, Some(token)) => {
                                        tracing::info!(
                                            session_id = %sid,
                                            agent_type,
                                            "agent 声明 mcp http 能力，注入 remember MCP server"
                                        );
                                        Some(vec![McpServer::Http(McpServerHttp::new(
                                            "rust-tunnel-memory",
                                            format!("http://127.0.0.1:{mcp_port}/mcp/{token}"),
                                        ))])
                                    }
                                    (true, None) => {
                                        tracing::info!(
                                            session_id = %sid,
                                            "agent 声明 mcp http 能力但本会话未铸造 token，跳过 remember MCP 注入"
                                        );
                                        None
                                    }
                                    (false, _) => {
                                        tracing::info!(
                                            session_id = %sid,
                                            "agent 未声明 mcp http 能力，跳过 remember MCP 注入"
                                        );
                                        None
                                    }
                                };
                            // 统一构造请求（按需带 mcpServers）。
                            let build_new_req = |cwd: &str| match &mcp_servers {
                                Some(servers) => {
                                    NewSessionRequest::new(cwd).mcp_servers(servers.clone())
                                }
                                None => NewSessionRequest::new(cwd),
                            };
                            let build_resume_req = |session_id: SessionId, cwd: &str| {
                                match &mcp_servers {
                                    Some(servers) => ResumeSessionRequest::new(session_id, cwd)
                                        .mcp_servers(servers.clone()),
                                    None => ResumeSessionRequest::new(session_id, cwd),
                                }
                            };
                            // 会话建立：resume 成功复用旧 id；失败/无 id/不支持 →
                            // session/new（全新会话，原行为）。
                            let (acp_session_id, config_options): (
                                SessionId,
                                Vec<SessionConfigOption>,
                            ) = match persisted_acp_session_id.as_deref() {
                                Some(persisted) if resume_capable => {
                                    match cx
                                        .send_request(build_resume_req(
                                            SessionId::new(persisted),
                                            &cwd,
                                        ))
                                        .block_task()
                                        .await
                                    {
                                        Ok(resp) => {
                                            tracing::info!(
                                                session_id = %sid,
                                                acp_session_id = %persisted,
                                                "acp session resumed"
                                            );
                                            (
                                                SessionId::new(persisted),
                                                resp.config_options.clone().unwrap_or_default(),
                                            )
                                        }
                                        Err(e) => {
                                            // resume 失败（会话文件缺失/已清理等）
                                            // → 回退全新会话，不阻断建立。
                                            tracing::warn!(
                                                session_id = %sid,
                                                "acp resume failed, fall back to new session: {e}"
                                            );
                                            let new_session = cx
                                                .send_request(build_new_req(&cwd))
                                                .block_task()
                                                .await?;
                                            (
                                                new_session.session_id.clone(),
                                                new_session
                                                    .config_options
                                                    .clone()
                                                    .unwrap_or_default(),
                                            )
                                        }
                                    }
                                }
                                _ => {
                                    let new_session = cx
                                        .send_request(build_new_req(&cwd))
                                        .block_task()
                                        .await?;
                                    (
                                        new_session.session_id.clone(),
                                        new_session.config_options.clone().unwrap_or_default(),
                                    )
                                }
                            };
                            // 落库（best-effort）：断线重拉时凭它 session/resume
                            // 恢复上下文。失败仅 warn，不阻断会话建立。
                            if let Err(e) = db
                                .agent_set_acp_session_id(&sid, Some(acp_session_id.0.as_ref()))
                                .await
                            {
                                tracing::warn!(session_id = %sid, "persist acp_session_id failed: {e}");
                            }
                            // 写回会话条目：连接句柄 + ACP session id 供 prompt/cancel；
                            // config_options 捕获后供 set_config_option / session_state 帧。
                            {
                                let mut map = sessions.lock().await;
                                if let Some(agent) = map.get_mut(&sid) {
                                    agent.connection = Some(cx.clone());
                                    agent.acp_session_id = Some(acp_session_id);
                                    agent.config_options = config_options;
                                    agent.last_activity = std::time::Instant::now();
                                }
                            }
                            Ok::<(), agent_client_protocol::Error>(())
                        };
                        if let Err(e) = setup.await {
                            let _ = setup_tx.send(Err(format!("acp handshake failed: {e}")));
                            return Err(e);
                        }
                        let _ = setup_tx.send(Ok(()));
                        // 保持连接存活：进程 stdout EOF（pump 退出）时返回并关闭。
                        cx.incoming_closed().await;
                        Ok(())
                    },
                )
                .await;
            // setup 结果已由 main_fn 内部分支发送（初始化失败/成功各一次）；
            // 走到这里的是连接在 setup 之后因 EOF/错误收尾，仅记日志。
            if let Err(e) = result {
                tracing::debug!(session_id = %sid_log, "acp connection ended with error: {e}");
            }
        });

        match tokio::time::timeout(SPAWN_TIMEOUT, setup_rx).await {
            Ok(Ok(Ok(()))) => {
                // 握手成功即返回。wait_ready 的放行（spawn_ready.send(true)）由
                // ensure_session 在 apply_config_overrides + replay_config_state
                // 完成后统一执行——acp_handshake 只收 session_id/root_path，不持有
                // workspace 记录；把放行延后到配置注入完成，首条 prompt 才不与
                // 在途 set_config_option 竞态（恢复旧顺序：replay 先于 spawn_ready）。
                // 失败分支（下文）不发送 spawn_ready，wait_ready 以超时/Sender drop
                // 返回，语义与移动前一致。
                Ok(())
            }
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(_)) => Err("acp connection task dropped".into()),
            Err(_) => Err("acp handshake timed out".into()),
        }
    }

    /// 向 ACP 会话发送一条 prompt（fire-and-forget）。
    ///
    /// 发送 `session/prompt` 后立即返回；回合内的 `session/update` 通知经
    /// [`map_update`] 推送会话条目当前的 ws_tx，`PromptResponse` 到达时终态回调
    /// 处理。回合进行中重复 prompt 报错（`busy` 守卫；ACP 单连接不支持并发回合）
    /// ——用户路径请用 [`Self::submit_prompt`]（进行中自动排队）。
    ///
    /// 终态回调同时承担队列 drain：清 busy、唤醒取消兜底任务后，若 `pending_prompts`
    /// 非空则取队首异步续跑下一条（回合连续，不发 done），排空才发 done；本回合被
    /// 取消（代数命中）时抑制生产者终态帧但仍 drain 队列（停止后排队消息自动发送）。
    /// 兜底杀进程/进程崩溃（exited）后不 drain——排队消息在 ensure_session 重拉新
    /// 进程时迁移，避免往死进程发请求丢失。
    pub async fn prompt(&self, session_id: &str, content: &str) -> Result<(), String> {
        self.prompt_inner(session_id, content, false).await
    }

    /// `prompt` 的内部变体。`drain=true` 表示由终态回调的队列 drain 路径调用：
    /// 此时 `busy` 已被终态回调在同一锁内为下一条重新置位（防 `submit_prompt`
    /// 抢跑，见终态回调注释），故跳过 `busy` 检查，直接发送。
    async fn prompt_inner(
        &self,
        session_id: &str,
        content: &str,
        drain: bool,
    ) -> Result<(), String> {
        let (connection, acp_session_id, turn_gen, memory_block, skill_list_block) = {
            let mut sessions = self.sessions.lock().await;
            let agent = sessions
                .get_mut(session_id)
                .ok_or_else(|| "session not spawned".to_string())?;
            if agent.exited {
                return Err("agent process has exited".into());
            }
            // drain 路径：busy 已由终态回调为下一条置位，跳过检查；用户路径：
            // busy 时防御性报错（submit_prompt 已保证进行中消息排队）。
            if !drain && agent.busy {
                return Err("ACP 回合进行中，请等待完成或取消后再发送".into());
            }
            // 先校验再置 busy：校验失败（handshake 未完成等）不污染回合状态。
            let connection = agent
                .connection
                .clone()
                .ok_or_else(|| "ACP handshake not complete".to_string())?;
            let acp_session_id = agent
                .acp_session_id
                .clone()
                .ok_or_else(|| "ACP handshake not complete".to_string())?;
            agent.busy = true;
            agent.last_activity = std::time::Instant::now();
            agent.turn_started_at = Some(std::time::Instant::now());
            // 为本回合分配递增代数：cancel 时记录，终态回调据此判断是否抑制。
            // 解决单布尔跨回合共享导致 cancel 后立即重发 prompt 时误吞 done/
            // 误发 error 的竞态（cancelled 布尔无法区分"哪个回合被取消"）。
            agent.turn_generation += 1;
            let turn_gen = agent.turn_generation;
            let memory_block = agent.memory_block.clone().unwrap_or_default();
            let skill_list_block = agent.skill_list_block.clone().unwrap_or_default();
            (connection, acp_session_id, turn_gen, memory_block, skill_list_block)
        };

        let bridge = self.clone();
        let sessions = self.sessions.clone();
        let db = self.db.clone();
        let sid = session_id.to_string();
        // 上下文注入：`<memory>` 记忆块 + `<skills>` 技能清单**合并一次** prepend
        // 到发给 agent 的 user content 头部。只进 agent 侧上下文，不落 DB（持久化/
        // 蒸馏保持干净，无回环）。
        let final_content = {
            let mut parts: Vec<String> = Vec::with_capacity(2);
            if !memory_block.is_empty() {
                parts.push(memory_block);
            }
            if !skill_list_block.is_empty() {
                parts.push(skill_list_block);
            }
            if parts.is_empty() {
                content.to_string()
            } else {
                format!("{}\n\n{content}", parts.join("\n\n"))
            }
        };
        let prompt = vec![ContentBlock::Text(TextContent::new(final_content))];
        let send_result = connection
            .send_request_to(
                agent_client_protocol::Agent,
                PromptRequest::new(acp_session_id, prompt),
            )
            .on_receiving_result(async move |result| {
                // 终态落库先行：缓冲文本/thought 合并落库必须在 done 帧之前完成，
                // 否则前端 done 后 invalidate 的历史 refetch 可能读不到本回合并本。
                flush_acp_turn_buffers(&db, &sessions, &sid).await;
                // 终态：清 busy + 唤醒取消兜底任务 + 取当前 WS 通道 + 排空队列
                // （若会话存活）。取消/杀进程后的终态帧抑制按代数匹配而非全局
                // 布尔：cancel 后立即重发 prompt 时，新回合的终态回调不会被旧回合
                // 的取消标记误吞（评审 Finding）。
                let (next, cancelled, alive, duration_ms) = {
                    let mut map = sessions.lock().await;
                    match map.get_mut(&sid) {
                        Some(a) => {
                            a.busy = false;
                            // 唤醒取消兜底任务走优雅路径。用 notify_waiters 而非
                            // notify_one：notify_one 在无等待者时会暂存一个许可，
                            // 某正常回合的终态若先于兜底任务开始等待时调用，后续的
                            // 兜底任务会误消费陈旧许可而直接跳过杀进程（agent 真卡
                            // 死时进程无人杀）。
                            a.cancel_notify.notify_waiters();
                            let cancelled = a.cancelled_turns.remove(&turn_gen);
                            // 兜底杀进程/进程崩溃后不 drain：排队消息在
                            // ensure_session 重拉新进程时迁移（见 ensure_session），
                            // 避免往死进程发请求丢失。
                            let alive = !a.exited;
                            let next = if alive {
                                a.pending_prompts.pop_front()
                            } else {
                                None
                            };
                            // 防抢跑竞态（M1）：队列非空时锁内立即把 busy 重新置
                            // true——下一条已确定由 drain 路径运行。若不置位，紧跟
                            // 在后的 submit_prompt 会看到 busy=false 且队列空而抢跑
                            // 置忙，drain 的 prompt 拿到「回合进行中」错误把这条
                            // 排队消息静默丢弃。锁内同步完成（无 await），
                            // submit_prompt 观察不到 busy=false 的空窗。
                            if next.is_some() {
                                a.busy = true;
                            }
                            // 回合耗时：仅在本回合真正收尾（无排队下一条）时取出
                            // 计时——连续回合的下一条由 prompt_inner 重设起点。
                            let duration_ms = if next.is_none() {
                                a.turn_started_at
                                    .take()
                                    .map(|t| u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX))
                            } else {
                                None
                            };
                            (next, cancelled, alive, duration_ms)
                        }
                        // 会话已 kill/回收：条目移除，不再发终态帧。
                        None => return Ok(()),
                    }
                };
                // 队列非空：不发 done（回合连续），异步发起下一条 prompt。不在
                // 持锁状态 send_request（prompt 内部自己取锁）；spawn 避免同步
                // 递归 async 的深度风险（20 条排队 = 至多 20 层同步调用栈）。
                if let Some(next) = next {
                    // 取出即删持久行（best-effort）：本条已交执行，不再需要恢复。
                    if let Some(pid) = &next.persist_id {
                        let _ = db.agent_pending_delete(pid).await;
                    }
                    // 抽成独立 sync fn 发起下一条（不在 async 闭包里直接
                    // tokio::spawn(bridge.prompt(...))——闭包捕获环境会让 prompt
                    // future 被判定非 Send；独立函数里是普通 owned 数据，编译通过）。
                    spawn_drain_next(bridge.clone(), sid.clone(), next);
                    return Ok(());
                }
                // 队列排空：被取消的回合不发生产者终态帧（stopped 帧已由 WS
                // handler 回发、cancel_fallback 由兜底任务回发，再补 error/done
                // 会造成误导）。
                if cancelled {
                    return Ok(());
                }
                // 进程异常退出（非用户取消）：`alive=false` 由 handle_spawn_exit
                // 置位，可能是进程崩溃/被杀。此时必须把终态上报前端，否则前端
                // running 指示永久卡死（与 SpawnedAgent 注释一致：进程自行崩溃时
                // exited 也置位，仍须把错误上报）。回调以 Err 触发时下方 match
                // 会发 acp prompt failed；这里统一补一个明确的进程退出提示。
                if !alive {
                    broadcast_ws_frame(
                        &sessions,
                        &sid,
                        &serde_json::json!({
                            "type": "error",
                            "message": "agent 进程已退出，回合被终止"
                        }),
                        true,
                    )
                    .await;
                    return Ok(());
                }
                // 终态帧广播到所有连接（低频、必须送达）：前端在时通道很快被
                // push_task 排空，不会长期阻塞。连接全断开时为空广播（无消费端，
                // 与旧「无 ws_tx 直接返回」等价）。
                match result {
                    Ok(_resp) => {
                        // done 帧携带回合耗时（毫秒）：前端展示「x.xs」；排队
                        // 连续回合的中间 done 不发，只有收尾帧带耗时。
                        let done_frame = match duration_ms {
                            Some(ms) => serde_json::json!({"type": "done", "duration_ms": ms}),
                            None => serde_json::json!({"type": "done"}),
                        };
                        broadcast_ws_frame(
                            &sessions,
                            &sid,
                            &done_frame,
                            true,
                        )
                        .await;
                    }
                    Err(e) => {
                        broadcast_ws_frame(
                            &sessions,
                            &sid,
                            &serde_json::json!({
                                "type": "error",
                                "message": format!("acp prompt failed: {e}")
                            }),
                            true,
                        )
                        .await;
                    }
                }
                Ok(())
            });
        if let Err(e) = send_result {
            // 回调注册失败（连接关闭等）：清 busy，避免会话永久卡死。
            if let Some(a) = self.sessions.lock().await.get_mut(session_id) {
                a.busy = false;
            }
            return Err(format!("acp prompt send failed: {e}"));
        }
        Ok(())
    }

    /// 提交一条用户消息到 ACP 会话：空闲直接跑（走 [`Self::prompt`]），进行中回合
    /// 排队等待（推 `{"type":"queued"}` 帧通知前端）。回合连续：终态回调逐条续跑
    /// 队列，排空才发 done。
    ///
    /// `content` 为注入 @引用后的完整消息（调用方 mgmt/api/agent.rs 在分派前已
    /// `inject_refs`）；`refs` 原样随 PendingPrompt 存储备查。排队消息同样已由调用
    /// 方立即落库（user 落库在 submit_prompt 之前），刷新/重连后历史完整。
    ///
    /// 返回 Err 的场景：会话不存在 / 进程已退出 / 排队已达 `MAX_PENDING_PROMPTS`
    /// 上限。调用方应把错误以 error 帧回发前端。
    pub async fn submit_prompt(
        &self,
        session_id: &str,
        content: &str,
        refs: Vec<String>,
    ) -> Result<(), String> {
        // 第一段锁内快判：空闲直跑（最常见路径，零 DB 开销）；busy/队列非空走
        // 入队路径。队列非空但空闲（兜底杀进程后重拉迁移/恢复的旧消息）时，本条
        // 排到队尾、先跑队首——保持 FIFO 顺序。
        enum Act {
            Run(String),
            Enqueue,
        }
        let act = {
            let mut sessions = self.sessions.lock().await;
            let Some(a) = sessions.get_mut(session_id) else {
                return Err("session not spawned".to_string());
            };
            if a.exited {
                return Err("agent process has exited".into());
            }
            if a.busy {
                if a.pending_prompts.len() >= MAX_PENDING_PROMPTS {
                    return Err(format!("排队消息已达上限（{MAX_PENDING_PROMPTS} 条）"));
                }
                Act::Enqueue
            } else if a.pending_prompts.is_empty() {
                Act::Run(content.to_string())
            } else {
                Act::Enqueue
            }
        };
        let Act::Enqueue = act else {
            let Act::Run(c) = act else { unreachable!() };
            return self.prompt(session_id, &c).await;
        };

        // 入队路径：先落库（best-effort）再入内存队——INSERT 必先于 push 完成，
        // 否则 drain 取出后的 DELETE 可能先于 INSERT 执行，留下残留行导致重启后
        // 重复执行。落库失败降级为纯内存项（persist_id=None，重启后丢失，与旧
        // 行为一致）。
        let persist_id = format!("{:032x}", rand::random::<u128>());
        let refs_json = serde_json::to_string(&refs).unwrap_or_else(|_| "[]".into());
        let persisted = self
            .db
            .agent_pending_enqueue(&persist_id, session_id, content, &refs_json)
            .await
            .map_err(|e| {
                tracing::warn!(session_id, "persist pending prompt failed: {e}");
            })
            .is_ok();
        let item = PendingPrompt {
            content: content.to_string(),
            refs,
            persist_id: persisted.then_some(persist_id.clone()),
        };
        // 第二段锁内按最新状态落定：INSERT 期间回合可能恰好结束（busy→空闲），
        // 需重检——否则消息滞留队列无人 drain。
        let run_content = {
            let mut sessions = self.sessions.lock().await;
            let Some(a) = sessions.get_mut(session_id) else {
                // 条目被 kill/reaper 回收：回滚刚落的库行，报会话不存在
                drop(sessions);
                let _ = self.db.agent_pending_delete(&persist_id).await;
                return Err("session not spawned".to_string());
            };
            a.pending_prompts.push_back(item);
            // queued 帧：状态提示，广播到所有连接（多标签页都看到排队提示）；
            // try_send 无 await、持锁安全，丢帧可接受（与通知处理器同语义）。
            for (_, tx) in &a.ws_conns {
                let _ = tx.try_send(serde_json::json!({"type": "queued"}));
            }
            if a.busy {
                None
            } else {
                // INSERT 期间回合已结束（或本就空闲但队列有旧消息）：FIFO 跑队首。
                // busy 置位防抢跑（与终态 drain 的 M1 竞态同理）。
                let front = a.pending_prompts.pop_front();
                if front.is_some() {
                    a.busy = true;
                }
                front
            }
        };
        match run_content {
            Some(front) => {
                // 取出即删持久行（消息已被执行，不再需要恢复）；被回滚的直行路径
                // 同样在此删除（front 可能就是本条）。
                if let Some(fid) = &front.persist_id {
                    let _ = self.db.agent_pending_delete(fid).await;
                }
                // drain=true：busy 已在上面锁内为本条置位，跳过 prompt_inner 的
                // busy 检查（与终态回调的 drain 路径同语义）。
                self.prompt_inner(session_id, &front.content, true).await
            }
            None => Ok(()),
        }
    }

    /// 优雅取消进行中的回合：发 ACP `session/cancel` 通知（**保留进程**），等待
    /// agent 在 `cancel_grace` 内响应 PromptResponse（终态回调清 busy）；超时未
    /// 响应则兜底杀客户端进程（`AgentExecCancel{request_id = session_id}`，客户端
    /// spawn manager 终止内网侧 agent）并推 `{"type":"cancel_fallback"}` 帧。
    ///
    /// 与旧实现的区别：不再立即杀进程——直接杀会丢会话上下文（下次 prompt 走
    /// NewSessionRequest 建空会话）。busy 保持到 PromptResponse 到达才复位，期间
    /// 新消息经 [`Self::submit_prompt`] 排队；兜底杀进程后 `exited` 置位，下一次
    /// ensure_session 自动重拉新进程并迁移排队消息。
    pub async fn cancel(&self, session_id: &str) {
        tracing::info!(session_id, "ACP cancel requested");
        // 仅取消进行中的回合：非 busy 短路返回，防止无在途回合时把代数记入
        // cancelled_turns 后永不消费（泄漏）。
        let (client_id, connection, acp_session_id, turn_gen, cancel_notify) = {
            let mut sessions = self.sessions.lock().await;
            match sessions.get_mut(session_id) {
                Some(agent) if agent.busy => {
                    agent.last_activity = std::time::Instant::now();
                    // 记录当前回合代数为已取消：终态回调据此抑制生产者终态帧。
                    // 用代数而非布尔：cancel 后立即重发 prompt 时，新回合分配新
                    // 代数，不会被本条取消标记误伤。注意不清 busy——回合保持到
                    // PromptResponse 到达（终态回调清位）或兜底杀进程。
                    agent.cancelled_turns.insert(agent.turn_generation);
                    let turn_gen = agent.turn_generation;
                    (
                        agent.client_id.clone(),
                        agent.connection.clone(),
                        agent.acp_session_id.clone(),
                        turn_gen,
                        agent.cancel_notify.clone(),
                    )
                }
                _ => return, // 无进行中回合（或会话不存在）：无事可取消
            }
        };
        // ACP 协议层取消：让 agent 尽快停手（stop_reason = cancelled），进程保留。
        if let (Some(cx), Some(sid)) = (connection, acp_session_id) {
            let _ = cx.send_notification(CancelNotification::new(sid));
        }
        // 兜底任务：agent 未在 cancel_grace 内响应（终态回调未清 busy）则真杀。
        // 捕获的均为克隆（session_id / 代数 / Notify），锁只在二次确认时短暂持有。
        let sessions = self.sessions.clone();
        let spawner = self.spawner.clone();
        let sid = session_id.to_string();
        let grace = self.cancel_grace;
        tokio::spawn(async move {
            tokio::select! {
                // 优雅路径：终态回调清 busy 后 notify_waiters 唤醒，兜底不做任何事。
                _ = cancel_notify.notified() => {}
                // 超时：二次确认（仍 busy 且本代数仍被取消）才杀进程——避免误杀
                // 已恢复的回合 / 终态回调已清 busy 的正常路径。
                _ = tokio::time::sleep(grace) => {
                    let should_kill = {
                        let mut map = sessions.lock().await;
                        match map.get_mut(&sid) {
                            Some(a) if a.busy && a.cancelled_turns.contains(&turn_gen) => {
                                a.busy = false;
                                a.cancelled_turns.remove(&turn_gen);
                                true
                            }
                            _ => false,
                        }
                    };
                    if should_kill {
                        tracing::warn!(session_id = %sid, "ACP agent did not respond to cancel within grace; killing process");
                        spawner.send_agent_cancel(&client_id, &sid).await;
                        // cancel_fallback 帧：广播到所有连接（多标签页都解除 running）。
                        broadcast_ws_frame(
                            &sessions,
                            &sid,
                            &serde_json::json!({"type": "cancel_fallback"}),
                            false,
                        )
                        .await;
                    }
                }
            }
        });
    }

    /// 终结 ACP 会话：杀客户端进程 + 移除会话条目（idle reaper / 会话归档关闭
    /// 用）。与 `cancel` 的区别：不再保留会话，进程死后不重拉。
    ///
    /// **保留** agent 侧持久化会话数据：归档后重开会话可 `session/resume`
    /// 恢复上下文。需要连客户端数据一起清理请用 [`Self::kill_and_delete`]。
    pub async fn kill(&self, session_id: &str) {
        let client_id = self
            .sessions
            .lock()
            .await
            .get(session_id)
            .map(|a| a.client_id.clone());
        let Some(client_id) = client_id else {
            return;
        };
        self.spawner.send_agent_cancel(&client_id, session_id).await;
        // 终结会话：一并释放 LLM 回环代理监听端口（防泄漏）。
        self.spawner.stop_llm_proxy(&client_id, session_id).await;
        // 先 flush 回合缓冲：进行中回合未到终态/工具边界的流式文本落库（与
        // drop_client_sessions 一致），避免用户已看到的输出在归档/删除后丢库。
        flush_acp_turn_buffers(&self.db, &self.sessions, session_id).await;
        // AI 记忆蒸馏（归档触发）：flush 后会话内容完整再蒸馏。CAS 防重——
        // archive_session handler 的触发与这里多路并发，只有一个赢家。
        #[cfg(feature = "rag")]
        if let Some(memory) = self.memory.as_ref() {
            crate::agent::memory::distill::trigger_distill(memory, session_id, "archive").await;
        }
        self.sessions.lock().await.remove(session_id);
        tracing::info!(session_id, "killed ACP session");
    }

    /// 终结 ACP 会话并清理客户端持久化会话数据（**会话删除**用）：先发 ACP
    /// `session/delete` 让 agent 删除其持久化会话文件，再走 [`Self::kill`] 杀进程
    /// 移除条目。
    ///
    /// `session/delete` 是 best-effort（5s 超时防卡死）：连接已断/进程已死时忽略，
    /// 不影响终结。与 [`Self::kill`]（归档用，保留数据）和 idle reaper 的
    /// `send_agent_cancel`（回收空闲进程，保留数据供 resume）区别——只有用户
    /// 显式删除会话才清理 agent 侧数据。
    pub async fn kill_and_delete(&self, session_id: &str) {
        let (connection, acp_sid) = {
            let guard = self.sessions.lock().await;
            let Some(a) = guard.get(session_id) else {
                return;
            };
            (a.connection.clone(), a.acp_session_id.clone())
        };
        // 先让 agent 删除其持久化会话文件（best-effort，5s 超时防卡死）。
        if let (Some(cx), Some(sid)) = (connection, acp_sid) {
            let _ = tokio::time::timeout(
                Duration::from_secs(5),
                cx.send_request(DeleteSessionRequest::new(sid)).block_task(),
            )
            .await;
        }
        self.kill(session_id).await;
    }

    /// 断开/连接关闭时清空条目里的 WS 事件通道：ACP 回合在连接关闭后仍可能
    /// 存活，保留旧 sender 会让通知处理器 try_send 持续成功而事件无人消费
    /// （更严重的是 reaper 据此刷新活动，误以为会话仍然活跃）。
    ///
    /// 只清本连接自己注册的通道（按 `conn_id` 匹配）：刷新/重连时旧连接的
    /// close 检测可能晚于新连接注册（`ensure_session` 已把 `ws_tx` 换成新连接
    /// 的通道），旧连接 teardown 若无条件置 None 会把新连接的通道一起清掉
    /// → 后续 tool_result/done 帧全部丢弃、前端 running 卡死。
    pub async fn detach_ws_tx(&self, session_id: &str, my_conn_id: u64) {
        let mut sessions = self.sessions.lock().await;
        let Some(a) = sessions.get_mut(session_id) else {
            return;
        };
        // 从广播列表移除本连接（无论是否主通道）——流式帧不再发往已断开的连接。
        a.ws_conns.retain(|(id, _)| *id != my_conn_id);
        // 本连接是主通道（审批/elicitation 帧通道）：清空并把主通道顺延到剩余
        // 连接里最近注册的一个（ws_conns 按注册顺序 push，末尾即最近），保持
        // 审批/表单帧有主通道可发；全部断开则归 0。通知审批等待者连接已切换：
        // 值变化 → wait_for 唤醒 → 旧连接等待立即拒绝（detach 早于订阅时
        // borrow 已看到新值，同样短路拒绝）。
        if a.ws_conn_id == my_conn_id {
            a.ws_tx = None;
            a.ws_conn_id = a.ws_conns.last().map(|(id, _)| *id).unwrap_or(0);
            if let Some(tx) = a
                .ws_conns
                .iter()
                .find(|(id, _)| *id == a.ws_conn_id)
                .map(|(_, tx)| tx.clone())
            {
                a.ws_tx = Some(tx);
            }
            a.ws_conn_watch.send_replace(a.ws_conn_id);
        }
    }

    /// 客户端控制连接断开时清理该客户端的所有 ACP 会话：先 flush 回合缓冲
    /// （断线瞬间未到终态的内容也落库，刷新历史仍可追溯），再移除会话条目。
    /// 客户端进程随控制连接断开而终止，残留条目只会被 idle reaper 晚回收
    /// （30 分钟），此处在断开点即时清理，避免审批等待/缓冲长时间悬挂。
    pub async fn drop_client_sessions(&self, client_id: &str) {
        let sids: Vec<String> = self
            .sessions
            .lock()
            .await
            .iter()
            .filter(|(_, a)| a.client_id == client_id)
            .map(|(sid, _)| sid.clone())
            .collect();
        for sid in sids {
            flush_acp_turn_buffers(&self.db, &self.sessions, &sid).await;
            // AI 记忆蒸馏（断线触发）：flush 后内容完整再蒸馏。CAS 防重。
            #[cfg(feature = "rag")]
            if let Some(memory) = self.memory.as_ref() {
                crate::agent::memory::distill::trigger_distill(memory, &sid, "disconnect").await;
            }
            self.sessions.lock().await.remove(&sid);
            tracing::info!(
                session_id = %sid,
                client_id,
                "dropped ACP session on client disconnect"
            );
        }
    }

    /// 构造全量 session_state 帧；无状态（未握手/agent 不上报）返回 None。
    /// 同时包含 available_commands（若有），确保新连接的标签页能拿到最新命令列表。
    async fn session_state_frame(&self, session_id: &str) -> Option<serde_json::Value> {
        let sessions = self.sessions.lock().await;
        let agent = sessions.get(session_id)?;
        if agent.acp_session_id.is_none() || agent.config_options.is_empty() {
            return None;
        }
        let mut frame = serde_json::json!({
            "type": "session_state",
            "options": agent.config_options,
        });
        if !agent.available_commands.is_empty() {
            let commands: Vec<serde_json::Value> = agent
                .available_commands
                .iter()
                .map(|cmd| {
                    serde_json::json!({
                        "name": cmd.name,
                        "description": cmd.description,
                    })
                })
                .collect();
            frame["available_commands"] = serde_json::Value::Array(commands);
        }
        Some(frame)
    }

    /// 握手成功后注入 workspace 级 ACP 引擎选项覆盖（`agent_config_overrides`，
    /// JSON map：config_id → value）。先于 [`Self::replay_config_state`] 执行——
    /// session 级 config_state（用户显式选择）回放覆盖 workspace 默认。
    /// config_id 按字典序（`mode` 提前，与回放一致）逐项 set；agent 未暴露的
    /// config_id 或单条失败仅 warn 跳过，不阻断会话建立与其余项注入。
    pub(super) async fn apply_config_overrides(&self, session_id: &str, workspace: &AgentWorkspaceRecord) {
        let Some(raw) = workspace.agent_config_overrides.as_deref() else {
            return;
        };
        let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(raw)
        else {
            tracing::warn!(
                session_id,
                "agent_config_overrides not a JSON object, skipped"
            );
            return;
        };
        let mut entries: Vec<(String, String)> = map
            .into_iter()
            .filter_map(|(k, v)| match v.as_str() {
                Some(s) => Some((k, s.to_string())),
                None => {
                    tracing::warn!(
                        session_id,
                        config_id = %k,
                        "agent_config_overrides value not a string, skipped"
                    );
                    None
                }
            })
            .collect();
        entries.sort_by_key(|(k, _)| (if k == "mode" { 0 } else { 1 }, k.clone()));
        for (config_id, value) in entries {
            if let Err(e) = self.set_config_option(session_id, &config_id, &value).await {
                tracing::warn!(session_id, config_id, "apply config override skipped: {e}");
            }
        }
    }

    /// 握手成功后回放 DB 中持久化的配置（mode 优先：agent 侧 model 切换会
    /// 重建 effort 列表，mode 先行保证其余项在最终列表上生效）。单条失败
    /// （如新版 agent 移除某取值）跳过并 warn，不阻断其余。
    pub(super) async fn replay_config_state(&self, session_id: &str) {
        let saved = match self.db.agent_get_session(session_id).await {
            Ok(Some(record)) => record.config_state,
            _ => None,
        };
        let Some(saved) = saved else { return };
        let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(&saved)
        else {
            return;
        };
        let mut entries: Vec<(String, String)> = map
            .into_iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
            .collect();
        entries.sort_by_key(|(k, _)| if k == "mode" { 0 } else { 1 });
        for (config_id, value) in entries {
            if let Err(e) = self.set_config_option(session_id, &config_id, &value).await {
                tracing::warn!(session_id, config_id, "replay config_state skipped: {e}");
            }
        }
    }

    /// 切换 ACP 会话配置项：校验 config_id 在当前 options 中 → 发
    /// `session/set_config_option`。value 对 select 是 value-id 字符串，
    /// 对 boolean 是 "true"/"false"。成功后的状态更新以 agent 回推的
    /// config_option_update 为准（通知处理器全量替换快照）。
    pub async fn set_config_option(
        &self,
        session_id: &str,
        config_id: &str,
        value: &str,
    ) -> Result<(), String> {
        let (connection, acp_session_id, is_boolean) = {
            let mut sessions = self.sessions.lock().await;
            let agent = sessions
                .get_mut(session_id)
                .ok_or_else(|| "session not spawned".to_string())?;
            if agent.exited {
                return Err("agent process has exited".into());
            }
            let option = agent
                .config_options
                .iter()
                .find(|o| o.id.0.as_ref() == config_id)
                .ok_or_else(|| format!("unknown config option: {config_id}"))?;
            let is_boolean = matches!(&option.kind, SessionConfigKind::Boolean(_));
            let connection = agent
                .connection
                .clone()
                .ok_or_else(|| "ACP handshake not complete".to_string())?;
            let acp_session_id = agent
                .acp_session_id
                .clone()
                .ok_or_else(|| "ACP handshake not complete".to_string())?;
            agent.last_activity = std::time::Instant::now();
            (connection, acp_session_id, is_boolean)
        };
        let typed_value = if is_boolean {
            SessionConfigOptionValue::boolean(value == "true")
        } else {
            // schema 的 id 新类型只派生了 From<&'static str>；非静态 &str 经
            // SessionConfigValueId::new（内部 Into<Arc<str>> 走 std From<&str>）。
            SessionConfigOptionValue::value_id(SessionConfigValueId::new(value))
        };
        match tokio::time::timeout(
            CONFIG_OPTION_TIMEOUT,
            connection
                .send_request_to(
                    agent_client_protocol::Agent,
                    SetSessionConfigOptionRequest::new(
                        acp_session_id,
                        SessionConfigId::new(config_id),
                        typed_value,
                    ),
                )
                .block_task(),
        )
        .await
        {
            Err(_) => {
                // 超时对账：agent 可能实际已生效但响应丢失，也可能未生效——
                // 无论哪种，都把内存中的权威快照广播给前端，让其收敛回真实
                // 状态（前端 optimistic UI 得以回滚）。
                self.broadcast_config_snapshot(session_id).await;
                Err(format!("set_config_option timed out: {config_id}"))
            }
            Ok(inner) => {
                if let Err(e) = inner {
                    // 错误路径同样对账（agent 显式拒绝时快照即旧值）。
                    self.broadcast_config_snapshot(session_id).await;
                    return Err(format!("set_config_option failed: {e}"));
                }
                Ok(())
            }
        }
    }

    /// 把会话内存中的 config_options 快照以 `config_option_update` 帧广播给
    /// 当前 WS 连接（best-effort）——用于 set_config_option 超时/失败后的对账。
    async fn broadcast_config_snapshot(&self, session_id: &str) {
        let options = {
            let sessions = self.sessions.lock().await;
            sessions
                .get(session_id)
                .map(|a| a.config_options.clone())
        };
        let Some(options) = options else { return };
        if options.is_empty() {
            return;
        }
        let frame = serde_json::json!({
            "type": "config_option_update",
            "options": options,
        });
        if let Some(ws_tx) = current_ws_tx(&self.sessions, session_id).await {
            let _ = ws_tx.try_send(frame);
        }
    }

    /// 当前会话的配置快照（WS 连接建立后主动推送用）；未就绪返回 None。
    pub async fn session_config_options(
        &self,
        session_id: &str,
    ) -> Option<Vec<SessionConfigOption>> {
        let sessions = self.sessions.lock().await;
        let agent = sessions.get(session_id)?;
        if agent.acp_session_id.is_none() || agent.config_options.is_empty() {
            return None;
        }
        Some(agent.config_options.clone())
    }

    /// 会话是否已在 ACP 桥登记且进程存活（预 spawn 成功后、首条消息前，
    /// `set_config_option` 等帧也要能分派）。exited 的陈旧条目视同未就绪，
    /// 让调用方走错误路径而非静默丢弃。
    pub async fn session_spawned(&self, session_id: &str) -> bool {
        self.sessions
            .lock()
            .await
            .get(session_id)
            .is_some_and(|a| !a.exited)
    }

    /// 等待会话的 ACP 握手 + 配置注入完成（连接预 spawn 可能在后台进行）。
    /// 已就绪立即返回；超时、会话被移除（spawn 失败/Sender drop）返回 Err
    /// （spawn 失败时透出缓存的真实原因）。
    ///
    /// 统一以 `spawn_ready` watch 为准，不放行于 connection 已写入的瞬时状态：
    /// `connection` 在握手完成时即写回会话条目，而 `spawn_ready` 在
    /// `apply_config_overrides` + `replay_config_state` 全部完成后才置 true。
    /// 若以 connection 存在与否做快路径放行，首条 prompt 会与在途的
    /// `set_config_option` 竞态（workspace overrides/用户 config_state 尚未注入）。
    /// watch 为 true 即「握手 + 配置注入」均已完成的最终状态。
    ///
    /// 超时预算用 `READY_TIMEOUT` 而非 `SPAWN_TIMEOUT`：预 spawn 流水线
    /// （LLM 代理协商 → spawn 协商 → handshake → 配置注入）最坏耗时远超
    /// 30s，等待方必须覆盖整个在途尝试，否则冷启动慢时误报超时。
    pub async fn wait_ready(&self, session_id: &str) -> Result<(), String> {
        let mut rx = {
            let sessions = self.sessions.lock().await;
            match sessions.get(session_id) {
                Some(agent) => agent.spawn_ready.subscribe(),
                None => return Err(self.spawn_failure(session_id).await),
            }
        };
        // 订阅后才检查当前值：避免「subscribe 前已 send(true)」的窗口漏等
        // （重连/多标签页下条目已就绪，subscribe 即取到当前 true 值）。
        if *rx.borrow() {
            return Ok(());
        }
        // wait_for 的返回值借用 rx（Ref<bool>，非 Send）：直接 match 临时值
        // 并在语句结束即 drop，不持借用跨下方 await（否则 WS handler 的
        // future 非 Send）；也不能落局部变量——通配模式不 move，会活到块尾。
        let sender_dropped = match tokio::time::timeout(READY_TIMEOUT, rx.wait_for(|r| *r)).await {
            Ok(Ok(_)) => return Ok(()),
            Ok(Err(_)) => true,
            Err(_) => false,
        };
        if sender_dropped {
            Err(self.spawn_failure(session_id).await)
        } else {
            Err(
                "等待 ACP agent 就绪超时：spawn 仍在进行（agent 冷启动或网络较慢），请稍后重试"
                    .to_string(),
            )
        }
    }

    /// 会话不在 spawn 表时的错误描述：优先取最近一次 spawn 失败的真实原因
    /// （预 spawn 失败会移除占位条目并缓存原因；新一轮尝试开始前已清除旧值，
    /// 故此处读到的必属于最近一次已结束的尝试）。
    async fn spawn_failure(&self, session_id: &str) -> String {
        match self.spawn_errors.lock().await.get(session_id) {
            Some(e) => format!("agent spawn failed: {e}"),
            None => "session not spawned".to_string(),
        }
    }

    /// 恢复持久化的排队 prompt（服务端重启/reaper 回收后首次 spawn 时调用）。
    ///
    /// DB 为权威队列：占位条目里 migrated 的旧内存拷贝与 DB 行是同一批
    /// 消息——以 DB 版为准，仅补 `persist_id=None` 的落库失败降级项。
    pub(super) async fn restore_pending_prompts(&self, session_id: &str) {
        let rows = self
            .db
            .agent_pending_list(session_id)
            .await
            .unwrap_or_default();
        if rows.is_empty() {
            return;
        }
        let mut restored: VecDeque<PendingPrompt> = rows
            .into_iter()
            .map(|(id, content, refs)| PendingPrompt {
                content,
                refs: serde_json::from_str(&refs).unwrap_or_default(),
                persist_id: Some(id),
            })
            .collect();
        let mut sessions = self.sessions.lock().await;
        if let Some(a) = sessions.get_mut(session_id) {
            let memory_only = std::mem::take(&mut a.pending_prompts)
                .into_iter()
                .filter(|p| p.persist_id.is_none());
            restored.extend(memory_only);
            a.pending_prompts = restored;
        }
    }

    /// 路由客户端发来的 spawn/LLM 代理控制消息（server.rs 控制循环转交）。
    ///
    /// `client_name` 是消息来源客户端（AgentLlmProxyRequest 的响应必须回发
    /// 给同一客户端；session_id 用于解析模型配置）。
    pub async fn handle_client_msg(&self, client_name: &str, msg: ControlMessage) {
        match msg {
            ControlMessage::AgentSpawnData {
                session_id,
                data,
                stdin,
            } => {
                self.handle_spawn_data(&session_id, data, stdin).await;
            }
            ControlMessage::AgentSpawnExit { session_id, code } => {
                self.handle_spawn_exit(&session_id, code).await;
            }
            ControlMessage::AgentLlmProxyRequest {
                request_id,
                session_id,
                path,
                body,
            } => {
                self.handle_llm_proxy_request(client_name, request_id, session_id, path, body)
                    .await;
            }
            _ => {
                tracing::debug!("acp_bridge: ignoring control message");
            }
        }
    }

    /// AgentSpawnData（进程 stdio 字节）：进程 stdout（stdin=false）转发到对应
    /// 会话的 ACP stdio pump；stdin=true（服务端→进程方向）理论上不会从客户端
    /// 到达，直接忽略。锁内只取发送端克隆，实际发送在锁外——channel 满时
    /// 背压等待（ACP 侧持续消费；仅在进程卡死/连接中断时积压），不阻塞
    /// prompt/cancel/reaper 对会话表的访问。
    async fn handle_spawn_data(&self, session_id: &str, data: Vec<u8>, stdin: bool) {
        if stdin {
            tracing::trace!(
                session_id,
                len = data.len(),
                "acp spawn data stdin=true ignored"
            );
            return;
        }
        let stdout_tx = {
            let mut sessions = self.sessions.lock().await;
            match sessions.get_mut(session_id) {
                Some(agent) if !agent.exited => {
                    agent.last_activity = std::time::Instant::now();
                    agent.stdout_tx.clone()
                }
                Some(_) => None,
                None => None,
            }
        };
        match stdout_tx {
            Some(tx) => {
                tracing::trace!(
                    session_id,
                    len = data.len(),
                    "acp spawn data routed to pump"
                );
                if tx.send(data).await.is_err() {
                    tracing::debug!(session_id, "spawn data: pump closed, dropped");
                }
            }
            None => tracing::debug!(session_id, "spawn data for missing/exited session, dropped"),
        }
    }

    /// AgentSpawnExit（进程退出）：标记会话已退出；后续 prompt 报错。
    /// 同时 drop pump 发送端：进程 stdout 已全部到达（控制通道保序，exit 在
    /// 最后），pump 排空残余字节后退出 → duplex EOF → ACP 连接随之关闭。
    async fn handle_spawn_exit(&self, session_id: &str, code: Option<i32>) {
        let mut sessions = self.sessions.lock().await;
        match sessions.get_mut(session_id) {
            Some(agent) => {
                agent.exited = true;
                agent.stdout_tx = None;
                agent.last_activity = std::time::Instant::now();
                tracing::info!(session_id, code, "acp agent process exited");
            }
            None => tracing::debug!(session_id, "spawn exit for unknown session"),
        }
    }

    /// AgentLlmProxyRequest：经内部 HTTP 回环调 LLM 网关入口（`/v1/messages`
    /// 或 `/v1/chat/completions`），网关自动完成模型组故障转移、格式转换、
    /// 用量统计等全管线。响应块经客户端控制通道流式回发。
    async fn handle_llm_proxy_request(
        &self,
        client_name: &str,
        request_id: String,
        session_id: String,
        path: String,
        body: Vec<u8>,
    ) {
        // 响应回发给来源客户端（其控制通道）。
        let Some(control_tx) = self.spawner.client_control_sender(client_name).await else {
            tracing::warn!(client_name, %request_id, "llm proxy: client offline, dropping request");
            return;
        };
        // `/mcp/<token>`：ACP agent 经内网 LLM 回环代理发来的 MCP 请求（remember
        // 工具）。单 chunk 回 AgentLlmProxyChunk{status, data, done:true}（MCP 报文
        // ~1KB 量级，无需切片）。非 rag 构建无此端点，落下方 llm_bridge 白名单
        // 404（行为同现状，零回归）。
        if path.starts_with("/mcp/") {
            #[cfg(feature = "rag")]
            {
                let sessions = self.sessions.clone();
                let memory = self.memory.clone();
                let db = self.db.clone();
                let sid = session_id;
                let p = path;
                let b = body;
                tokio::spawn(async move {
                    let resp =
                        handle_mcp_tunnel(&sessions, memory.as_ref(), &db, &sid, &p, b).await;
                    let _ = control_tx
                        .send(ControlMessage::AgentLlmProxyChunk {
                            request_id,
                            data: resp.data,
                            done: true,
                            status: resp.status,
                        })
                        .await;
                });
                return;
            }
        }
        // 会话必须已登记（ensure_session 已跑）。未登记时无法解析模型，按契约发 502 done chunk。
        if !self.sessions.lock().await.contains_key(&session_id) {
            let _ = control_tx
                .send(ControlMessage::AgentLlmProxyChunk {
                    request_id,
                    data: format!("agent session not ready: {session_id}").into_bytes(),
                    done: true,
                    status: 502,
                })
                .await;
            return;
        }
        // 网关未注入（生产启动应在 init_llm_state 后注入）：全部 502。
        let Some(gateway) = self.gateway.clone() else {
            let _ = control_tx
                .send(ControlMessage::AgentLlmProxyChunk {
                    request_id,
                    data: b"llm gateway not configured (missing inject after init)".to_vec(),
                    done: true,
                    status: 502,
                })
                .await;
            return;
        };
        let db = self.db.clone();
        tokio::spawn(async move {
            let stream =
                llm_bridge::forward(db, session_id, request_id.clone(), gateway, path, body);
            futures_util::pin_mut!(stream);
            while let Some(chunk) = stream.next().await {
                let msg = ControlMessage::AgentLlmProxyChunk {
                    request_id: chunk.request_id,
                    data: chunk.data,
                    done: chunk.done,
                    status: chunk.status,
                };
                if control_tx.send(msg).await.is_err() {
                    break; // 客户端断连：停止转发
                }
            }
        });
    }
}
/// 处理 ACP agent 经回环代理转发的 MCP 请求（`/mcp/<token>`）。
///
/// token 校验：与 [`SpawnedAgent`] 的 `mcp_token`（ensure_session 铸造）精确比对；
/// 条目移除（kill/重拉/reaper）即吊销。校验通过后解析 workspace 坐标并转交
/// [`crate::agent::mcp::handle_request`] 做 JSON-RPC 分发（只暴露 remember 工具）。
#[cfg(feature = "rag")]
async fn handle_mcp_tunnel(
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
    let workspace_id = match db.agent_get_session(session_id).await {
        Ok(Some(s)) => s.workspace_id,
        _ => {
            tracing::warn!(session_id, "mcp tunnel: agent session missing in db");
            return mcp_internal_error("agent session not found");
        }
    };
    // memory 缺失（防御：构造时未注入）→ 500。
    let Some(memory) = memory else {
        tracing::warn!(session_id, "mcp tunnel: memory runtime not configured");
        return mcp_internal_error("memory not configured");
    };
    crate::agent::mcp::handle_request(memory, &client_id, &workspace_id, session_id, &body).await
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

/// 从终态回调发起下一条排队 prompt（fire-and-forget，队列 drain）。
///
/// 抽成独立 sync fn：在 async 闭包（`on_receiving_result` 回调）里直接
/// `tokio::spawn(bridge.prompt(...))` 会让 `prompt()` 的 opaque future 因闭包
/// 捕获环境被判定非 Send（编译错误）；独立函数上下文里 `bridge`/`sid`/`next` 是
/// 普通 owned 数据，`tokio::spawn` 正常编译。`prompt()` 本身 fire-and-forget
/// （发完即返回），spawn 后本函数立即返回，不构成同步递归。
fn spawn_drain_next(bridge: AcpBridge, sid: String, next: PendingPrompt) {
    tokio::spawn(async move {
        // drain=true：跳过 busy 检查——busy 已由终态回调为下一条锁内置位。
        if let Err(e) = bridge.prompt_inner(&sid, &next.content, true).await {
            tracing::warn!(session_id = %sid, "drain queued prompt failed: {e}");
        }
    });
}
/// 动态解析会话当前的 WS 事件通道：重连/多标签页时 `ensure_session` 的 dedup
/// 刷新、连接关闭时 [`AcpBridge::detach_ws_tx`] 清空，都会改动条目里的
/// `ws_tx`。连接任务的通知/请求处理器**每次事件**都读最新值，避免流式帧/审批
/// 弹层推给已断开的旧连接（旧 sender 的 try_send 会静默失败，前端只看到 done）。
/// 顺带刷新 last_activity——长回合无 stdout 时不被 idle reaper 误回收。
/// 会话不存在返回 None。
pub(super) async fn current_ws_tx(
    sessions: &Arc<Mutex<HashMap<String, SpawnedAgent>>>,
    sid: &str,
) -> Option<mpsc::Sender<serde_json::Value>> {
    let mut map = sessions.lock().await;
    let agent = map.get_mut(sid)?;
    agent.last_activity = std::time::Instant::now();
    agent.ws_tx.clone()
}
/// 动态解析会话当前的 WS 事件通道 + 连接标识 + 连接变化 watch（`request_permission`
/// 处理器专用）。与 [`current_ws_tx`] 同样的「每次事件读最新值」语义，额外返回
/// `ws_conn_id` 与 watch Receiver——`approve_or_disconnect` 据此在连接断开/重连时
/// 即时拒绝审批，不等满审批超时。会话不存在返回 None。
async fn current_ws_channel(
    sessions: &Arc<Mutex<HashMap<String, SpawnedAgent>>>,
    sid: &str,
) -> Option<(
    Option<mpsc::Sender<serde_json::Value>>,
    u64,
    watch::Receiver<u64>,
)> {
    let mut map = sessions.lock().await;
    let agent = map.get_mut(sid)?;
    agent.last_activity = std::time::Instant::now();
    Some((
        agent.ws_tx.clone(),
        agent.ws_conn_id,
        agent.ws_conn_watch.subscribe(),
    ))
}
/// 把帧广播到本会话**所有** WS 连接（多标签页/多窗口 fan-out）。遍历 `ws_conns`
/// 逐个发送：某连接断开/通道满只丢弃该连接，不影响其余——根治「最新连接获胜」
/// 劫持（H5）：被动打开的新标签页不再把正在运行回合的流式帧/终态帧切走，原标签页
/// 永久卡 running。审批/elicitation 请求帧不走这里（仍只发主通道 `ws_tx`，见
/// [`current_ws_channel`]——多连接下审批卡只在主连接展示，响应按 request_id
/// 全局可达，任意连接可答）。
///
/// 顺带刷新 last_activity（同 [`current_ws_tx`] 语义：长回合无 stdout 不被 idle
/// reaper 误回收）。`must_deliver` 用于终态帧（done/error）：低频、必须送达，
/// 走阻塞发送（前端在时通道很快被 push_task 排空，不会长期阻塞）；流式/状态帧
/// 用 try_send（实时流丢帧可容忍，避免慢消费端阻塞整个 ACP 连接任务）。
async fn broadcast_ws_frame(
    sessions: &Arc<Mutex<HashMap<String, SpawnedAgent>>>,
    sid: &str,
    frame: &serde_json::Value,
    must_deliver: bool,
) {
    let conns: Vec<mpsc::Sender<serde_json::Value>> = {
        let mut map = sessions.lock().await;
        let Some(a) = map.get_mut(sid) else {
            return;
        };
        a.last_activity = std::time::Instant::now();
        a.ws_conns.iter().map(|(_, tx)| tx.clone()).collect()
    };
    for tx in conns {
        if must_deliver {
            let _ = tx.send(frame.clone()).await;
        } else if tx.try_send(frame.clone()).is_err() {
            tracing::trace!(session_id = %sid, "acp event dropped (ws channel full/closed)");
        }
    }
}
/// 审批等待 + 断线/重连即时拒绝（select 逻辑独立成函数便于单元测试）。
///
/// 订阅后先检查当前值：若已 detach/重连（值 ≠ 捕获的 `captured_conn_id`，
/// 例如 detach 早于订阅发生）立即返回 `Denied`，不进入审批。随后 select 等
/// approval 完成，或 `conn_rx` 变为 ≠ 捕获值（连接断开/重连 → 审批帧缓冲在
/// 无人消费的旧 channel 里、新重连也收不到）→ 返回 `Denied`，避免
/// `request_approval` 等满 5 分钟超时阻塞 agent 的下一个工具调用。
#[allow(clippy::too_many_arguments)]
pub(super) async fn approve_or_disconnect(
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
#[allow(clippy::too_many_arguments)]
pub(super) async fn elicit_or_disconnect(
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
fn pick_option(
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
fn permission_option_to_approval(o: &PermissionOption) -> ApprovalOption {
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
pub(super) fn client_capabilities() -> agent_client_protocol::schema::v1::ClientCapabilities {
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
pub(super) fn to_workspace_relative(root_path: &str, abs_path: &str) -> Result<String, String> {
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
pub(super) async fn exec_fs_read(
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
            Duration::from_secs(120),
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
pub(super) async fn exec_fs_write(
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

    let use_write_file2 =
        crate::agent::runner::client_supports_edit(client_version.as_deref());

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
            Duration::from_secs(120),
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
fn acp_raw_to_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}
