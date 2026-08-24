//! ACP handshake：initialize + 会话建立（resume/new）、通知/权限/fs 请求处理器装配。

use std::collections::HashMap;
use tokio::sync::{mpsc, oneshot};

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
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{ByteStreams, Client};

use crate::acp_events::map_update;
use crate::{ApprovalResult, ElicitationResult};

use super::super::pump::FuturesIo;
use super::super::reaper::touch_activity;
use super::super::store::persist_acp_frame;
use super::super::{AcpBridge, ApprovalOption, SPAWN_TIMEOUT};
use super::{
    acp_raw_to_string, approve_or_disconnect, broadcast_ws_frame, client_capabilities,
    current_ws_channel, elicit_or_disconnect, exec_fs_read, exec_fs_write,
    permission_option_to_approval, pick_option,
};

impl AcpBridge {
    pub(crate) async fn acp_handshake(
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
}
