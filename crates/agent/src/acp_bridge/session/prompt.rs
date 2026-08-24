//! 回合 prompt/cancel：busy 守卫、待处理队列、cancel 优雅停止与兜底强杀。

use std::collections::VecDeque;

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

use super::super::store::flush_acp_turn_buffers;
use super::super::{AcpBridge, PendingPrompt, MAX_PENDING_PROMPTS};

use super::*;

impl AcpBridge {
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
        let (connection, acp_session_id, turn_gen, memory_block, skill_list_block, wiki_list_block) = {
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
            let wiki_list_block = agent.wiki_list_block.clone().unwrap_or_default();
            (
                connection,
                acp_session_id,
                turn_gen,
                memory_block,
                skill_list_block,
                wiki_list_block,
            )
        };

        let bridge = self.clone();
        let sessions = self.sessions.clone();
        let db = self.db.clone();
        let sid = session_id.to_string();
        // 上下文注入：`<memory>` / `<skills>` / `<wikis>` **合并一次** prepend 到发给
        // agent 的 user content 头部。只进 agent 侧上下文，不落 DB（持久化/蒸馏保持
        // 干净，无回环）。
        let final_content = {
            let mut parts: Vec<String> = Vec::with_capacity(3);
            if !memory_block.is_empty() {
                parts.push(memory_block);
            }
            if !skill_list_block.is_empty() {
                parts.push(skill_list_block);
            }
            if !wiki_list_block.is_empty() {
                parts.push(wiki_list_block);
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
                                a.turn_started_at.take().map(|t| {
                                    u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX)
                                })
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
                        broadcast_ws_frame(&sessions, &sid, &done_frame, true).await;
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

    /// 恢复持久化的排队 prompt（服务端重启/reaper 回收后首次 spawn 时调用）。
    ///
    /// DB 为权威队列：占位条目里 migrated 的旧内存拷贝与 DB 行是同一批
    /// 消息——以 DB 版为准，仅补 `persist_id=None` 的落库失败降级项。
    pub(crate) async fn restore_pending_prompts(&self, session_id: &str) {
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
}
fn spawn_drain_next(bridge: AcpBridge, sid: String, next: PendingPrompt) {
    tokio::spawn(async move {
        // drain=true：跳过 busy 检查——busy 已由终态回调为下一条锁内置位。
        if let Err(e) = bridge.prompt_inner(&sid, &next.content, true).await {
            tracing::warn!(session_id = %sid, "drain queued prompt failed: {e}");
        }
    });
}
