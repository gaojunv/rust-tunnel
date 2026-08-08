//! ACP bridge: owns agent sessions over the tunnel, translates between
//! the frontend WebSocket protocol and ACP JSON-RPC.
//!
//! 生命周期：`ensure_session` 惰性 spawn（LLM 回环代理 → agent 进程），
//! 进程 stdio 经控制通道（AgentSpawnData）与 ACP 连接互转；空闲 30 分钟由
//! reaper 回收。ACP 连接建立（handshake）与 stdio pump 在 Task 6 联调，
//! 本 Task 交付结构、spawn 编排、docker 拒绝、事件路由。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use futures_util::StreamExt;
use tokio::sync::{mpsc, Mutex};

use crate::common::ControlMessage;
use crate::server::db::Database;
use crate::server::llm::crypto::LlmCipher;
use crate::server::persistence::db::agent::AgentWorkspaceRecord;

use super::llm_bridge;
use super::spawner::AgentSpawner;

/// spawn/协商超时：LLM 代理启动与 agent 进程拉起各限 30s。
const SPAWN_TIMEOUT: Duration = Duration::from_secs(30);
/// 空闲 30 分钟杀进程（重挂 ACP 连接由客户端 spawn manager 处理）。
const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// reaper 检查间隔。
const REAP_INTERVAL: Duration = Duration::from_secs(60);

/// 一个已 spawn 的 ACP agent 会话。
struct SpawnedAgent {
    /// ACP 侧 session id（handshake 成功后填充；Task 6 读取）。
    /// LLM 模型配置不在此缓存——AgentLlmProxyRequest 按 session 从 DB 解析
    /// （session → workspace.llm_model_id），保证服务端配置变更即时生效。
    #[allow(dead_code)] // Task 6: ACP handshake 建立后写入并用于 prompt/cancel
    acp_session_id: Option<agent_client_protocol::schema::v1::SessionId>,
    /// 最近活动时间（prompt / cancel / stdio 都会刷新；idle reaper 依据）。
    last_activity: std::time::Instant,
    /// AgentSpawnExit 已到达（进程结束）。
    exited: bool,
}

/// ACP 会话桥：惰性拉起 agent 进程、把 LLM 代理请求路由到服务端网关。
#[derive(Clone)]
pub struct AcpBridge {
    spawner: AgentSpawner,
    db: Database,
    /// 提供商字段解密器（来自 LLM 网关 LlmState；None 时仅支持明文历史值）。
    cipher: Option<LlmCipher>,
    /// 本服务端进程的活跃 ACP 会话表：session_id → SpawnedAgent。
    sessions: Arc<Mutex<HashMap<String, SpawnedAgent>>>,
}

impl AcpBridge {
    pub fn new(spawner: AgentSpawner, db: Database) -> Self {
        let bridge = Self {
            spawner,
            db,
            cipher: None,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        };
        bridge.start_idle_reaper();
        bridge
    }

    /// 注入 LLM 字段解密器（提供商 API Key 落库加密；bin/server.rs 在 LLM
    /// 网关初始化后调用）。None = 无主密钥，仅能读历史明文值。
    #[must_use]
    pub fn with_cipher(mut self, cipher: Option<LlmCipher>) -> Self {
        self.cipher = cipher;
        self
    }

    /// 后台回收空闲 ACP agent：超 `IDLE_TIMEOUT` 未活动即移除会话表条目。
    /// 真杀进程（控制通道下发生成进程退出语义）在 Task 6 联调。
    fn start_idle_reaper(&self) {
        let sessions = self.sessions.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(REAP_INTERVAL).await;
                let stale: Vec<String> = sessions
                    .lock()
                    .await
                    .iter()
                    .filter(|(_, a)| a.last_activity.elapsed() > IDLE_TIMEOUT)
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in stale {
                    sessions.lock().await.remove(&id);
                    tracing::info!(session_id = %id, "killed idle ACP agent");
                    // TODO(Task 6): 经 registry 下发进程退出语义
                    // （AgentExecCancel request_id = session_id，由客户端
                    //  spawn manager 终止对应进程）。
                }
            }
        });
    }

    /// 惰性确保 session 就绪：LLM 代理 → agent 进程 → ACP handshake。
    ///
    /// 幂等：会话表已有条目则直接返回（同一 session 的多个 WS 连接/多条
    /// 消息不会重复 spawn —— Task 3 评审契约，防重复 AgentLlmProxyStart）。
    ///
    /// `ws_tx` 是 WS 事件通道：handshake 建立后 ACP 事件流经
    /// [`super::acp_events::map_update`] 推回前端（Task 6 接线）。
    pub async fn ensure_session(
        &self,
        session_id: &str,
        workspace: &AgentWorkspaceRecord,
        ws_tx: mpsc::Sender<serde_json::Value>,
    ) -> Result<(), String> {
        // 幂等守卫 + 占位登记在一次锁内完成：并发 ensure_session（同一 session
        // 的多个 WS 连接/多条消息）看到占位条目直接短路，杜绝重复
        // AgentLlmProxyStart / AgentSpawnRequest（Task 3 评审契约）。
        // 校验也在此锁内做（无 await，持锁开销可忽略）。
        {
            let mut sessions = self.sessions.lock().await;
            if sessions.contains_key(session_id) {
                return Ok(());
            }
            if workspace.runtime_type != "host" {
                return Err("ACP 模式暂不支持 docker workspace，请改用 host 模式".into());
            }
            // spawn 前校验模型已配置（不缓存；AgentLlmProxyRequest 按 session
            // 从 DB 解析，保证配置变更即时生效）
            workspace
                .llm_model_id
                .as_deref()
                .ok_or_else(|| "workspace 未配置 LLM 模型（llm_model_id）".to_string())?;
            sessions.insert(
                session_id.to_string(),
                SpawnedAgent {
                    acp_session_id: None,
                    last_activity: std::time::Instant::now(),
                    exited: false,
                },
            );
        }
        // 拿锁期不 spawn（避免长时间持锁阻塞 prompt/cancel）。
        let agent_type = &workspace.agent_type;
        let outcome = async {
            // 1) 客户端内嵌 LLM 回环代理
            let port = self
                .spawner
                .start_llm_proxy(&workspace.client_id, session_id, SPAWN_TIMEOUT)
                .await?;
            // 2) spawn agent 进程（env 注入 LLM 代理地址）
            self.spawner
                .spawn_agent(
                    &workspace.client_id,
                    session_id,
                    agent_type,
                    workspace.agent_path.as_deref(),
                    port,
                    &workspace.root_path,
                    SPAWN_TIMEOUT,
                )
                .await?;
            // 3) ACP handshake（Task 6：stdio pump + WS 事件接线）
            self.acp_handshake(session_id, &ws_tx).await
        }
        .await;
        if outcome.is_err() {
            // spawn 失败：移除占位，允许后续重试。
            self.sessions.lock().await.remove(session_id);
        }
        outcome
    }

    /// ACP handshake：initialize → session/new（或 session/load 恢复）。
    ///
    /// stdio pump 链路：agent 进程 stdout 以 `AgentSpawnData(stdin=false)`
    /// 从控制通道到达 → 写进 duplex 一端；duplex 另一端由
    /// `agent-client-protocol` 的 Client 端驱动，其输出经 duplex 读回 → 以
    /// `AgentSpawnData(stdin=true)` 下发给客户端进程 stdin。
    ///
    /// 真实 crate API 形状（agent-client-protocol 2.0，已核对源码）：
    /// ```no_run
    /// # let client_end = tokio::io::duplex(64 * 1024).0;
    /// use agent_client_protocol::ByteStreams;
    /// use agent_client_protocol::schema::v1::InitializeRequest;
    /// // 连接以独立任务驱动，main_fn 持有 ConnectionTo<Agent>：
    /// //   cx.send_request(InitializeRequest::new(ProtocolVersion::V1)).block_task().await?;
    /// //   cx.build_session(cwd)?.block_task().run_until(async |mut session| {
    /// //       session.send_prompt(...)?; while ... session.read_update()...
    /// //   }).await?;
    /// // transport: ByteStreams::new(reader, writer) 接 duplex 一端
    /// # let _ = client_end;
    /// ```
    ///
    /// 注意：`agent_client_protocol::Client` 是角色标记（unit struct），并非
    /// 连接句柄；连接句柄是 `ConnectionTo<Agent>`，会话是 `ActiveSession`，
    /// 因此 `SpawnedAgent` 不直接持有 crate 的 Client。
    async fn acp_handshake(
        &self,
        _session_id: &str,
        _ws_tx: &mpsc::Sender<serde_json::Value>,
    ) -> Result<(), String> {
        todo!("ACP handshake - implemented against crate API in Task 6 (stdio pump + WS wiring)")
    }

    /// 向 ACP 会话发送一条 prompt。连接尚未建立（handshake 未完成）时报错。
    pub async fn prompt(&self, session_id: &str, content: &str) -> Result<(), String> {
        let mut sessions = self.sessions.lock().await;
        let agent = sessions
            .get_mut(session_id)
            .ok_or_else(|| "session not spawned".to_string())?;
        if agent.exited {
            return Err("agent process has exited".into());
        }
        agent.last_activity = std::time::Instant::now();
        // TODO(Task 6): 经已建立的 ActiveSession 发送 prompt：
        //   session.send_prompt(content)? 后循环 session.read_update()，
        //   每个 SessionUpdate 经 map_update 映射后 push 到 ensure_session
        //   时注册的 ws_tx；收到 StopReason 后发 {"type":"done"}。
        let _ = content; // Task 5 分层：prompt 转发留 Task 6 联调
        Ok(())
    }

    /// 取消进行中的回合：ACP session/cancel + 客户端 AgentExecCancel。
    pub async fn cancel(&self, session_id: &str) {
        tracing::info!(session_id, "ACP cancel requested");
        {
            let mut sessions = self.sessions.lock().await;
            if let Some(agent) = sessions.get_mut(session_id) {
                agent.last_activity = std::time::Instant::now();
            }
        }
        // TODO(Task 6): ACP session/cancel + registry AgentExecCancel
        // （request_id = session_id，客户端据此终止在跑的命令/回合）。
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

    /// AgentSpawnData（进程 stdio 字节）：写入对应会话的 ACP stdio pump。
    /// pump（duplex 配对 + 下发 stdin）在 Task 6 联调；本 Task 只维护活动
    /// 时间戳与退出标记。
    async fn handle_spawn_data(&self, session_id: &str, data: Vec<u8>, stdin: bool) {
        let mut sessions = self.sessions.lock().await;
        match sessions.get_mut(session_id) {
            Some(agent) if !agent.exited => {
                agent.last_activity = std::time::Instant::now();
                tracing::trace!(
                    session_id,
                    stdin,
                    len = data.len(),
                    "acp spawn data routed"
                );
                // TODO(Task 6): stdin=false → 写 duplex 读端（喂 ACP crate）；
                // duplex 写端读回的字节 → AgentSpawnData(stdin=true) 下发客户端。
            }
            Some(_) => tracing::debug!(session_id, "spawn data after process exit, dropped"),
            None => tracing::debug!(
                session_id,
                "spawn data for unknown session (not yet registered), dropped"
            ),
        }
    }

    /// AgentSpawnExit（进程退出）：标记会话已退出；后续 prompt 报错。
    async fn handle_spawn_exit(&self, session_id: &str, code: Option<i32>) {
        let mut sessions = self.sessions.lock().await;
        match sessions.get_mut(session_id) {
            Some(agent) => {
                agent.exited = true;
                agent.last_activity = std::time::Instant::now();
                tracing::info!(session_id, code, "acp agent process exited");
            }
            None => tracing::debug!(session_id, "spawn exit for unknown session"),
        }
    }

    /// AgentLlmProxyRequest：解析模型配置 → 服务端注入 key → 上游流式转发，
    /// 响应块经客户端控制通道回发。**必须**以 done=true chunk 收尾（契约）。
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
        // 会话必须已登记（ensure_session 已跑）。未登记（LLM 代理请求先于
        // ensure_session）时无法解析模型，按契约发 502 done chunk。
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
        let db = self.db.clone();
        let cipher = self.cipher.clone();
        tokio::spawn(async move {
            // forward 内部按 session → workspace.llm_model_id → model → provider
            // 解析，服务端注入 api key 后调上游。
            let stream = llm_bridge::forward(db, cipher, session_id, request_id.clone(), path, body);
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    /// 构造 workspace 记录（agent_type 已配置 + host 运行时）。
    fn acp_workspace() -> AgentWorkspaceRecord {
        AgentWorkspaceRecord {
            id: "w1".into(),
            name: "proj".into(),
            client_id: "nas".into(),
            runtime_type: "host".into(),
            root_path: "/workspace".into(),
            docker_image: None,
            docker_container_id: None,
            approval_mode: "safe".into(),
            system_prompt: None,
            agent_type: "gemini".into(),
            agent_path: None,
            llm_model_id: Some("model-1".into()),
            created_at: "t".into(),
            updated_at: "t".into(),
        }
    }

    fn docker_workspace() -> AgentWorkspaceRecord {
        let mut ws = acp_workspace();
        ws.runtime_type = "docker".into();
        ws
    }

    /// 构造一个注册了模拟客户端 + 自动应答协商请求的 bridge。
    /// `respond` 闭包把收到的请求转成响应消息。
    async fn mock_bridge<F>(respond: F) -> AcpBridge
    where
        F: Fn(ControlMessage) -> ControlMessage + Send + 'static,
    {
        let db = Database::new(":memory:").await.unwrap();
        db.save_server_auth("secret").await.unwrap();
        let registry = crate::server::client_registry::ClientRegistry::new(db.clone());
        let (tx, mut rx) = mpsc::channel(32);
        registry
            .register("nas", None, None, "secret", tx)
            .await
            .unwrap();
        let registry2 = registry.clone();
        tokio::spawn(async move {
            loop {
                let Some(req) = rx.recv().await else {
                    return;
                };
                let sid = match &req {
                    ControlMessage::AgentSpawnRequest { session_id, .. } => session_id.clone(),
                    ControlMessage::AgentLlmProxyStart { session_id } => session_id.clone(),
                    other => panic!("unexpected request: {other:?}"),
                };
                registry2.resolve_spawn_pending(&sid, respond(req)).await;
            }
        });
        AcpBridge::new(AgentSpawner::new(registry), db)
    }

    #[tokio::test]
    async fn test_ensure_session_docker_rejected() {
        let bridge = mock_bridge(|_| unreachable!("docker rejection should not spawn")).await;
        let (ws_tx, _rx) = mpsc::channel(16);
        let err = bridge
            .ensure_session("sess-1", &docker_workspace(), ws_tx)
            .await
            .expect_err("docker workspace should be rejected");
        assert!(err.contains("docker"), "err: {err}");
    }

    #[tokio::test]
    async fn test_ensure_session_missing_model_id_rejected() {
        let bridge = mock_bridge(|_| unreachable!("missing model should not spawn")).await;
        let mut ws = acp_workspace();
        ws.llm_model_id = None;
        let (ws_tx, _rx) = mpsc::channel(16);
        let err = bridge
            .ensure_session("sess-1", &ws, ws_tx)
            .await
            .expect_err("workspace without llm_model_id should be rejected");
        assert!(err.contains("llm_model_id"), "err: {err}");
    }

    #[tokio::test]
    async fn test_ensure_session_offline_client_fails() {
        // 客户端未注册 → start_llm_proxy 报 NotConnected，before spawn_agent
        let db = Database::new(":memory:").await.unwrap();
        let registry = crate::server::client_registry::ClientRegistry::new(db.clone());
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);
        let mut ws = acp_workspace();
        ws.client_id = "ghost".into();
        let (ws_tx, _rx) = mpsc::channel(16);
        let err = bridge
            .ensure_session("sess-1", &ws, ws_tx)
            .await
            .expect_err("offline client should fail");
        assert!(err.contains("llm proxy start failed"), "err: {err}");
    }

    #[tokio::test]
    async fn test_ensure_session_unsupported_agent_type() {
        // 模拟客户端应答 LLM 代理端口；agent_type 非法在本地报错（spawn 前）
        let bridge = mock_bridge(|req| match req {
            ControlMessage::AgentLlmProxyStart { session_id } => {
                ControlMessage::AgentLlmProxyReady {
                    session_id,
                    port: 45678,
                }
            }
            other => panic!("unexpected request: {other:?}"),
        })
        .await;
        let mut ws = acp_workspace();
        ws.agent_type = "cursor".into();
        let (ws_tx, _rx) = mpsc::channel(16);
        let err = bridge
            .ensure_session("sess-1", &ws, ws_tx)
            .await
            .expect_err("unsupported agent type should fail locally");
        assert!(err.contains("unsupported agent type"), "err: {err}");
    }

    #[tokio::test]
    async fn test_ensure_session_dedup_skips_respawn() {
        // 会话已登记 → ensure_session 直接 Ok，不发起任何 spawn
        let bridge = mock_bridge(|_| unreachable!("dedup should not spawn")).await;
        bridge.sessions.lock().await.insert(
            "sess-1".into(),
            SpawnedAgent {
                acp_session_id: None,
                last_activity: std::time::Instant::now(),
                exited: false,
            },
        );
        let (ws_tx, _rx) = mpsc::channel(16);
        bridge
            .ensure_session("sess-1", &acp_workspace(), ws_tx)
            .await
            .expect("dedup should return Ok");
    }

    #[tokio::test]
    async fn test_prompt_unknown_session_errors() {
        let db = Database::new(":memory:").await.unwrap();
        let registry = crate::server::client_registry::ClientRegistry::new(db.clone());
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);
        let err = bridge
            .prompt("no-such-session", "hi")
            .await
            .expect_err("unknown session should error");
        assert!(err.contains("not spawned"), "err: {err}");
    }

    #[tokio::test]
    async fn test_prompt_after_exit_errors() {
        let db = Database::new(":memory:").await.unwrap();
        let registry = crate::server::client_registry::ClientRegistry::new(db.clone());
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);
        bridge.sessions.lock().await.insert(
            "sess-1".into(),
            SpawnedAgent {
                acp_session_id: None,
                last_activity: std::time::Instant::now(),
                exited: true,
            },
        );
        let err = bridge
            .prompt("sess-1", "hi")
            .await
            .expect_err("exited session should error");
        assert!(err.contains("exited"), "err: {err}");
    }

    #[tokio::test]
    async fn test_handle_spawn_exit_marks_exited() {
        let db = Database::new(":memory:").await.unwrap();
        let registry = crate::server::client_registry::ClientRegistry::new(db.clone());
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);
        bridge.sessions.lock().await.insert(
            "sess-1".into(),
            SpawnedAgent {
                acp_session_id: None,
                last_activity: std::time::Instant::now(),
                exited: false,
            },
        );
        bridge
            .handle_client_msg(
                "nas",
                ControlMessage::AgentSpawnExit {
                    session_id: "sess-1".into(),
                    code: Some(1),
                },
            )
            .await;
        assert!(
            bridge.sessions.lock().await.get("sess-1").unwrap().exited,
            "exit should be recorded"
        );
    }

    #[tokio::test]
    async fn test_handle_llm_proxy_request_unregistered_session_502() {
        let db = Database::new(":memory:").await.unwrap();
        db.save_server_auth("secret").await.unwrap();
        let registry = crate::server::client_registry::ClientRegistry::new(db.clone());
        let (tx, mut rx) = mpsc::channel(32);
        registry
            .register("nas", None, None, "secret", tx)
            .await
            .unwrap();
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);

        bridge
            .handle_client_msg(
                "nas",
                ControlMessage::AgentLlmProxyRequest {
                    request_id: "req-1".into(),
                    session_id: "sess-unknown".into(),
                    path: "/v1/chat/completions".into(),
                    body: br#"{"stream":true}"#.to_vec(),
                },
            )
            .await;
        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for chunk")
            .expect("channel closed");
        match msg {
            ControlMessage::AgentLlmProxyChunk {
                request_id,
                done: true,
                status: 502,
                ..
            } => assert_eq!(request_id, "req-1"),
            other => panic!("expected 502 done chunk, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_handle_llm_proxy_request_offline_client_dropped() {
        // 来源客户端离线：无法回发，请求被丢弃（不 panic、不 hang）
        let db = Database::new(":memory:").await.unwrap();
        let registry = crate::server::client_registry::ClientRegistry::new(db.clone());
        let bridge = AcpBridge::new(AgentSpawner::new(registry), db);
        bridge
            .handle_client_msg(
                "ghost",
                ControlMessage::AgentLlmProxyRequest {
                    request_id: "req-1".into(),
                    session_id: "sess-1".into(),
                    path: "/v1/chat/completions".into(),
                    body: br#"{"stream":true}"#.to_vec(),
                },
            )
            .await;
    }
}
