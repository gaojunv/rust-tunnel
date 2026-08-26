//! 会话生命周期：ensure_session（spawn/attach/resume）、ready 等待与 spawn 失败归因。

use std::collections::{HashMap, VecDeque};
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

use crate::db::agent::AgentWorkspaceRecord;

use super::super::pump::run_stdio_pump;
use super::super::{AcpBridge, SpawnedAgent, READY_TIMEOUT, SPAWN_TIMEOUT};

use super::current_ws_tx;

impl AcpBridge {
    /// 惰性确保 session 就绪：stdio pump → LLM 代理 → agent 进程 → ACP handshake。
    ///
    /// 幂等：会话表已有条目则直接返回（同一 session 的多个 WS 连接/多条
    /// 消息不会重复 spawn —— Task 3 评审契约，防重复 AgentLlmProxyStart）。
    /// 幂等命中时刷新条目里的 WS 事件通道（重连/多标签页共用同一进程）。
    ///
    /// `ws_tx` 是 WS 事件通道：handshake 建立后 ACP 事件流经
    /// [`crate::acp_events::map_update`] 推回前端。
    ///
    /// # Errors
    /// - `workspace.runtime_type` 非 `host` 时返回错误（ACP 暂不支持 docker）
    /// - LLM 网关未注入或模型配置缺失时返回带阶段前缀的错误
    /// - `start_llm_proxy` / `spawn_agent` / `acp_handshake` 任一阶段失败时返回对应阶段错误并清理占位条目
    #[allow(
        clippy::too_many_lines,
        reason = "spawn 流水线顺序编排含 LLM 代理/模型门禁/spawn/handshake/配置注入等多阶段共享状态，拆分会打散局部变量与错误归因"
    )]
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
                        if let Some(slot) = a.ws_conns.iter_mut().find(|(id, _)| *id == conn_id) {
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
                    wiki_list_block: None,
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
            if !crate::session::has_any_model_config(
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
                elapsed_ms = u64::try_from(pipeline_start.elapsed().as_millis()).unwrap_or(u64::MAX),
                "acp spawn stage: llm proxy ready (port {port})"
            );
            // 2) spawn agent 进程（env 注入 LLM 代理地址）。解析有效模型引用注入
            //    agent 进程（opencode 经 `OPENCODE_CONFIG_CONTENT` 注入；claude-code
            //    不用它，仍走 ACP set_config_option）。解析失败不阻断 spawn——
            //    best-effort，实际请求的 model 由 llm_bridge 每次按 session 从 DB
            //    重新解析覆盖。
            let spawn_model = crate::session::resolve_effective_model(
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
            // key ∈ {opus,sonnet,haiku,subagent}）→ ANTHROPIC_DEFAULT_*_MODEL / CLAUDE_CODE_SUBAGENT_MODEL 等 env。
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
                                match crate::session::resolve_workspace_model_ref(
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
                                            "subagent" => ("CLAUDE_CODE_SUBAGENT_MODEL", None),
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
                elapsed_ms = u64::try_from(pipeline_start.elapsed().as_millis()).unwrap_or(u64::MAX),
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
                elapsed_ms = u64::try_from(pipeline_start.elapsed().as_millis()).unwrap_or(u64::MAX),
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
            elapsed_ms = u64::try_from(pipeline_start.elapsed().as_millis()).unwrap_or(u64::MAX),
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
    ///
    /// # Errors
    /// - 会话不存在且无失败缓存时返回 `session not spawned`
    /// - 存在失败缓存（`spawn_errors`）时返回 `agent spawn failed: <原因>`
    /// - 等待超时且通道未关闭时返回超时提示
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
}
