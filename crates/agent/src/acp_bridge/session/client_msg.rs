//! 客户端控制消息路由：spawn 数据/退出、LLM 代理请求。

use futures_util::StreamExt;

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

use crate::llm_bridge;
use rust_tunnel_common::ControlMessage;

use super::super::AcpBridge;
#[cfg(feature = "rag")]
use super::mcp_tunnel;

impl AcpBridge {
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
        if let Some(tx) = stdout_tx {
            tracing::trace!(
                session_id,
                len = data.len(),
                "acp spawn data routed to pump"
            );
            if tx.send(data).await.is_err() {
                tracing::debug!(session_id, "spawn data: pump closed, dropped");
            }
        } else {
            tracing::debug!(session_id, "spawn data for missing/exited session, dropped")
        }
    }

    /// AgentSpawnExit（进程退出）：标记会话已退出；后续 prompt 报错。
    /// 同时 drop pump 发送端：进程 stdout 已全部到达（控制通道保序，exit 在
    /// 最后），pump 排空残余字节后退出 → duplex EOF → ACP 连接随之关闭。
    async fn handle_spawn_exit(&self, session_id: &str, code: Option<i32>) {
        let mut sessions = self.sessions.lock().await;
        if let Some(agent) = sessions.get_mut(session_id) {
            agent.exited = true;
            agent.stdout_tx = None;
            agent.last_activity = std::time::Instant::now();
            tracing::info!(session_id, code, "acp agent process exited");
        } else {
            tracing::debug!(session_id, "spawn exit for unknown session")
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
                        mcp_tunnel::handle_mcp_tunnel(&sessions, memory.as_ref(), &db, &sid, &p, b)
                            .await;
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
