//! 测试共享辅助：workspace/agent 构造、mock ACP agent、handshake 装配等。
//! 各域测试文件经 `use super::helpers::*;` 引入（含 mpsc re-export）。

use super::super::*;
pub(super) use tokio::sync::mpsc;

/// 测试固定连接 id：setup_handshake 注册的 ws_tx 属此连接，detach 用例
/// 用同一 id 验证「自己 detach 才清空」。
pub(super) const TEST_CONN_ID: u64 = 42;

/// 构造 workspace 记录（agent_type 已配置 + host 运行时）。
pub(super) fn acp_workspace() -> AgentWorkspaceRecord {
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
        agent_config_overrides: None,
        claude_tier_models: None,
        github_token: None,
        github_owner: None,
        github_repo: None,
        created_at: "t".into(),
        updated_at: "t".into(),
    }
}

pub(super) fn docker_workspace() -> AgentWorkspaceRecord {
    let mut ws = acp_workspace();
    ws.runtime_type = "docker".into();
    ws
}

/// 测试用的会话条目默认值（connection/duplex 未建立）。
pub(super) fn spawned_agent() -> SpawnedAgent {
    SpawnedAgent {
        memory_block: None,
        skill_list_block: None,
        wiki_list_block: None,
        mcp_token: None,
        acp_session_id: None,
        connection: None,
        agent_io: None,
        stdout_tx: None,
        client_id: "nas".into(),
        ws_tx: None,
        ws_conns: Vec::new(),
        ws_conn_id: 0,
        ws_conn_watch: watch::channel(0).0,
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
        pending_prompts: VecDeque::new(),
        cancel_notify: Arc::new(tokio::sync::Notify::new()),
        file_hashes: HashMap::new(),
    }
}

/// 装配 mock agent（duplex → pump → mock_acp_agent）并完成 ACP handshake。
/// `ws_tx` 注册为会话条目的初始事件通道；连接任务的通知处理器此后每次事件
/// 从条目动态解析通道（见 `current_ws_tx`）。
pub(super) async fn setup_handshake(bridge: &AcpBridge, ws_tx: mpsc::Sender<serde_json::Value>) {
    setup_handshake_with(
        bridge,
        ws_tx,
        serde_json::json!([]),
        Arc::new(Mutex::new(Vec::new())),
        None,
        None,
        None,
        false,
        None,
    )
    .await;
}

/// `setup_handshake` 的参数化版本：`config_options` 注入 `session/new` 响应
/// 的 `configOptions`（空数组 = 无配置项），`applied` 记录收到的
/// `session/set_config_option` 调用（config_id, value）。
///
/// `prompt_permits`（None 立即响应）：mock 收到 `session/prompt` 后先回流式
/// 通知，再等待一个许可才回 PromptResponse——队列/取消测试需要精确控制「回合
/// 何时结束」。`recorded`（None 不记录）：收集 mock 收到的 method/通知名
/// （如 `session/cancel`），供断言。
///
/// `persisted_id`：传给 `acp_handshake` 的持久化 ACP session id（resume 测试
/// 用；None = 全新会话路径）。`resume_fails`：true 时 mock 的 `session/resume`
/// 回 error（测回退 session/new）。`fail_config_id`：mock 对该 config_id 的
/// `session/set_config_option` 回 JSON-RPC error（测「单条失败不阻断其余注入」）。
#[allow(clippy::too_many_arguments)]
pub(super) async fn setup_handshake_with(
    bridge: &AcpBridge,
    ws_tx: mpsc::Sender<serde_json::Value>,
    config_options: serde_json::Value,
    applied: Arc<Mutex<Vec<(String, String)>>>,
    prompt_permits: Option<mpsc::Receiver<()>>,
    recorded: Option<Arc<Mutex<Vec<String>>>>,
    persisted_id: Option<&str>,
    resume_fails: bool,
    fail_config_id: Option<&str>,
) {
    let (agent_io, pump_io) = tokio::io::duplex(64 * 1024);
    let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>(128);
    let (control_tx, control_rx) = mpsc::channel::<ControlMessage>(32);

    let mut agent = spawned_agent();
    agent.agent_io = Some(agent_io);
    agent.stdout_tx = Some(stdout_tx.clone());
    agent.ws_tx = Some(ws_tx.clone());
    agent.ws_conn_id = TEST_CONN_ID;
    // 广播列表同步登记（与真实路径 ensure_session 建条目一致）：多连接 fan-out
    // 测试依赖 setup 时就含首连接。
    agent.ws_conns = vec![(TEST_CONN_ID, ws_tx.clone())];
    bridge.sessions.lock().await.insert("sess-1".into(), agent);

    tokio::spawn(run_stdio_pump(
        pump_io,
        stdout_rx,
        control_tx,
        "sess-1".into(),
    ));
    tokio::spawn(mock_acp_agent(
        control_rx,
        stdout_tx,
        config_options,
        applied,
        prompt_permits,
        recorded,
        resume_fails,
        fail_config_id.map(str::to_string),
        // 本 helper 不含 MCP 注入参数：缺省 http=false + 丢弃记录
        // （MCP 注入协商走专用 setup_handshake_mcp）。
        false,
        Arc::new(Mutex::new(Vec::new())),
    ));

    bridge
        .acp_handshake(
            "sess-1",
            "claude-code",
            "/mock",
            persisted_id.map(str::to_string),
            45678,
            None,
        )
        .await
        .expect("handshake should complete");
}

/// 装配 mock agent 并完成 ACP handshake，带 MCP 注入参数（专用 MCP 协商测试）。
///
/// `mcp_http`：initialize 响应声明 `mcpCapabilities.http`（false = 无 http 能力）；
/// `mcp_token`：传给 `acp_handshake` 的本会话 MCP 端点 token（None = 不注入）；
/// `mcp_servers` 记录 mock 收到的 `session/new` 与 `session/resume` 的
/// `params.mcpServers`（原样 JSON，空数组 = 未注入）。
pub(super) async fn setup_handshake_mcp(
    bridge: &AcpBridge,
    ws_tx: mpsc::Sender<serde_json::Value>,
    mcp_http: bool,
    mcp_token: Option<&str>,
    mcp_servers: Arc<Mutex<Vec<serde_json::Value>>>,
    agent_type: &str,
) {
    let (agent_io, pump_io) = tokio::io::duplex(64 * 1024);
    let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>(128);
    let (control_tx, control_rx) = mpsc::channel::<ControlMessage>(32);

    let mut agent = spawned_agent();
    agent.agent_io = Some(agent_io);
    agent.stdout_tx = Some(stdout_tx.clone());
    agent.ws_tx = Some(ws_tx.clone());
    agent.ws_conn_id = TEST_CONN_ID;
    agent.ws_conns = vec![(TEST_CONN_ID, ws_tx.clone())];
    bridge.sessions.lock().await.insert("sess-1".into(), agent);

    tokio::spawn(run_stdio_pump(
        pump_io,
        stdout_rx,
        control_tx,
        "sess-1".into(),
    ));
    tokio::spawn(mock_acp_agent(
        control_rx,
        stdout_tx,
        serde_json::json!([]),
        Arc::new(Mutex::new(Vec::new())),
        None,
        None,
        false,
        None,
        mcp_http,
        mcp_servers,
    ));

    bridge
        .acp_handshake(
            "sess-1",
            agent_type,
            "/mock",
            None,
            45678,
            mcp_token.map(str::to_string),
        )
        .await
        .expect("handshake should complete");
}

/// 构造一个注册了模拟客户端 + 自动应答协商请求的 bridge。
/// `respond` 闭包把收到的请求转成响应消息。
pub(super) async fn mock_bridge<F>(respond: F) -> AcpBridge
where
    F: Fn(ControlMessage) -> ControlMessage + Send + 'static,
{
    let db = Database::new(":memory:").await.unwrap();
    db.save_server_auth("secret").await.unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let (tx, mut rx) = mpsc::channel(32);
    registry.register("nas", None, tx).await;
    let registry2 = registry.clone();
    tokio::spawn(async move {
        loop {
            let Some(req) = rx.recv().await else {
                return;
            };
            let sid = match &req {
                ControlMessage::AgentSpawnRequest { session_id, .. } => session_id.clone(),
                ControlMessage::AgentLlmProxyStart { session_id } => session_id.clone(),
                // AgentLlmProxyStop 是单向清理消息（无响应、无等待者），跳过不协商。
                ControlMessage::AgentLlmProxyStop { .. } => continue,
                other => panic!("unexpected request: {other:?}"),
            };
            registry2.resolve_spawn_pending(&sid, respond(req)).await;
        }
    });
    AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db.clone())
        .with_llm_gateway(test_gateway(&db))
}

/// 测试用 dummy 网关：仅让 ensure_session 的「网关未配置」前置门禁放行，
/// 不会真有 LLM 请求发出（mock 注册表在到达那步前已按脚本应答/报错）。
pub(super) fn test_gateway(db: &Database) -> crate::llm_bridge::LlmGatewayEndpoint {
    crate::llm_bridge::LlmGatewayEndpoint {
        llm_state: std::sync::Arc::new(crate::llm::LlmState::new(Some(db.clone()), None)),
        api_key: "test-internal-key".into(),
    }
}

/// 装配 fs 测试环境：内存 DB（workspace `/ws` + session）、注册客户端（spawn loop
/// 应答 AgentExecRequest 返回固定结果）、活跃会话条目（client_id=nas）。
pub(super) async fn fs_test_env(
    exec_result: rust_tunnel_common::AgentResult,
) -> (
    Database,
    AgentSpawner,
    Arc<Mutex<HashMap<String, SpawnedAgent>>>,
) {
    let db = Database::new(":memory:").await.unwrap();
    db.save_server_auth("secret").await.unwrap();
    db.agent_create_workspace(
        "w1", "proj", "nas", "host", "/ws", None, None, "gemini", None, None, None, None,
    )
    .await
    .unwrap();
    db.agent_create_session("sess-1", "w1", None, Some("gpt-4o"))
        .await
        .unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let (tx, mut rx) = mpsc::channel(32);
    registry.register("nas", None, tx).await;
    let registry2 = registry.clone();
    tokio::spawn(async move {
        while let Some(req) = rx.recv().await {
            let ControlMessage::AgentExecRequest { request_id, .. } = req else {
                panic!("unexpected request: {req:?}");
            };
            registry2
                .deliver_agent_response("nas", &request_id, exec_result.clone())
                .await;
        }
    });
    let sessions = Arc::new(Mutex::new(HashMap::new()));
    let mut agent = spawned_agent();
    agent.client_id = "nas".into();
    sessions.lock().await.insert("sess-1".into(), agent);
    (
        db,
        AgentSpawner::new(std::sync::Arc::new(registry)),
        sessions,
    )
}

/// 装配 MCP 隧道测试环境：注册客户端 + 可选会话条目（带 mcp_token）。
/// `seed_session` 为 true 时在 DB 建 workspace/session（valid-token 用例需要
/// `agent_get_session` 解析 workspace_id）。
#[cfg(feature = "rag")]
pub(super) async fn mcp_tunnel_env(
    mcp_token: Option<&str>,
    seed_session: bool,
) -> (AcpBridge, mpsc::Receiver<ControlMessage>) {
    let db = Database::new(":memory:").await.unwrap();
    db.save_server_auth("secret").await.unwrap();
    if seed_session {
        db.agent_create_workspace(
            "w1",
            "proj",
            "nas",
            "host",
            "/workspace",
            None,
            None,
            "gemini",
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        db.agent_create_session("sess-1", "w1", None, Some("gpt-4o"))
            .await
            .unwrap();
    }
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let (tx, rx) = mpsc::channel(32);
    registry.register("nas", None, tx).await;
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db);
    if let Some(t) = mcp_token {
        let mut agent = spawned_agent();
        agent.mcp_token = Some(t.to_string());
        bridge.sessions.lock().await.insert("sess-1".into(), agent);
    }
    (bridge, rx)
}

/// 发一个 AgentLlmProxyRequest（/mcp/ 路径）并取回唯一一个 AgentLlmProxyChunk。
#[cfg(feature = "rag")]
pub(super) async fn send_mcp_request(
    bridge: &AcpBridge,
    rx: &mut mpsc::Receiver<ControlMessage>,
    path: &str,
    body: Vec<u8>,
) -> (String, u16, bool, Vec<u8>) {
    bridge
        .handle_client_msg(
            "nas",
            ControlMessage::AgentLlmProxyRequest {
                request_id: "req-1".into(),
                session_id: "sess-1".into(),
                path: path.into(),
                body,
            },
        )
        .await;
    match tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("timed out waiting for chunk")
        .expect("channel closed")
    {
        ControlMessage::AgentLlmProxyChunk {
            request_id,
            status,
            done,
            data,
        } => (request_id, status, done, data),
        other => panic!("expected AgentLlmProxyChunk, got {other:?}"),
    }
}

/// 模拟 ACP agent：newline-delimited JSON-RPC。从 `stdin_rx`（pump 的
/// AgentSpawnData{stdin:true} 转来）读请求行，把响应/通知写到 `stdout_tx`
/// （→ pump → ACP crate）。`config_options` 注入 `session/new` 响应的
/// `configOptions`；`applied` 记录收到的 `session/set_config_option`
/// 调用（config_id, value）——value 形态以 ACP 实际序列化为准（select 为
/// 裸字符串，boolean 为 bool + 顶层 type）。
///
/// `prompt_permits`（None 立即响应）：mock 收到 `session/prompt` 后先回流式
/// 通知，再等待一个许可才回 PromptResponse——队列/取消测试用。
/// `recorded`（None 不记录）：收集收到的 method/通知名（如 `session/cancel`）。
/// `resume_fails`：true 时 `session/resume` 回 JSON-RPC error（测回退 session/new）。
#[allow(clippy::too_many_arguments)]
pub(super) async fn mock_acp_agent(
    mut stdin_rx: mpsc::Receiver<ControlMessage>,
    stdout_tx: mpsc::Sender<Vec<u8>>,
    config_options: serde_json::Value,
    applied: Arc<Mutex<Vec<(String, String)>>>,
    mut prompt_permits: Option<mpsc::Receiver<()>>,
    recorded: Option<Arc<Mutex<Vec<String>>>>,
    resume_fails: bool,
    fail_config_id: Option<String>,
    mcp_http: bool,
    mcp_servers: Arc<Mutex<Vec<serde_json::Value>>>,
) {
    let mut buf = String::new();
    while let Some(msg) = stdin_rx.recv().await {
        let ControlMessage::AgentSpawnData {
            data, stdin: true, ..
        } = msg
        else {
            continue;
        };
        buf.push_str(&String::from_utf8_lossy(&data));
        while let Some(pos) = buf.find('\n') {
            let line: String = buf.drain(..=pos).collect();
            let Ok(json) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            let method = json
                .get("method")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string();
            if let Some(r) = &recorded {
                r.lock().await.push(method.clone());
            }
            let id = json.get("id").cloned().unwrap_or(serde_json::Value::Null);
            // 单条请求可能产出多条输出行（prompt：通知 + 响应）。
            let mut out_lines: Vec<serde_json::Value> = Vec::new();
            match method.as_str() {
                "initialize" => {
                    // 声明 loadSession + session/resume/delete 能力，与
                    // claude-agent-acp 对齐（resume 测试依赖它）。mcpCapabilities
                    // 由调用方配置：缺省 http=false → acp_handshake 不注入
                    // mcpServers（与真实无该能力的 agent 行为一致）。
                    out_lines.push(serde_json::json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {
                            "protocolVersion": 1,
                            "agentCapabilities": {
                                "loadSession": true,
                                "mcpCapabilities": { "http": mcp_http, "sse": false },
                                "sessionCapabilities": { "resume": {}, "delete": {} }
                            }
                        }
                    }));
                }
                "session/new" => {
                    // 记录注入的 mcpServers（http=true + token 时应有 1 条 http
                    // server；否则空数组）。与 applied 同模式：mock 侧收到什么
                    // 原样入列，断言交给测试。
                    let servers = json
                        .pointer("/params/mcpServers")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!([]));
                    mcp_servers.lock().await.push(servers);
                    out_lines.push(serde_json::json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": { "sessionId": "acp-1", "configOptions": config_options }
                    }));
                }
                "session/resume" => {
                    // 成功回显请求的 sessionId；失败回 JSON-RPC error
                    // （acp_handshake 据此回退 session/new）。resume 与 new 同样
                    // 记录 mcpServers（两种会话建立路径都注入）。
                    let servers = json
                        .pointer("/params/mcpServers")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!([]));
                    mcp_servers.lock().await.push(servers);
                    if resume_fails {
                        out_lines.push(serde_json::json!({
                            "jsonrpc": "2.0", "id": id,
                            "error": { "code": -32601, "message": "session not found" }
                        }));
                    } else {
                        let req_sid = json
                            .pointer("/params/sessionId")
                            .and_then(|v| v.as_str())
                            .unwrap_or("?")
                            .to_string();
                        out_lines.push(serde_json::json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": { "sessionId": req_sid, "configOptions": config_options }
                        }));
                    }
                }
                "session/delete" => {
                    out_lines.push(serde_json::json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {}
                    }));
                }
                "session/set_config_option" => {
                    let params = json.get("params").cloned().unwrap_or_default();
                    let config_id = params
                        .get("configId")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    // 指定的 config_id 硬失败：回 JSON-RPC error，验证调用方
                    // 「单条失败不阻断其余注入」。
                    if fail_config_id.as_deref() == Some(config_id.as_str()) {
                        out_lines.push(serde_json::json!({
                            "jsonrpc": "2.0", "id": id,
                            "error": { "code": -32000, "message": "invalid config value" }
                        }));
                        continue;
                    }
                    // ACP 实际序列化：select 的 value 是裸字符串（"sonnet"）；
                    // boolean 是 bool（{"type":"boolean","value":true} 平铺到
                    // params 顶层）。响应必须带 configOptions 字段（schema 的
                    // SetSessionConfigOptionResponse 必填，缺则反序列化报错）。
                    let value = params.get("value").cloned().unwrap_or_default();
                    let value_str = value
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| value.to_string());
                    applied.lock().await.push((config_id, value_str));
                    out_lines.push(serde_json::json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": { "configOptions": config_options }
                    }));
                }
                "session/prompt" => {
                    // 流式通知：先发（回合立即进入 running 状态，前端可见）。
                    let stream_lines = vec![
                        serde_json::json!({
                            "jsonrpc": "2.0", "method": "session/update",
                            "params": { "sessionId": "acp-1",
                                "update": { "sessionUpdate": "agent_message_chunk",
                                    "content": { "type": "text", "text": "hello from mock" } } }
                        }),
                        serde_json::json!({
                            "jsonrpc": "2.0", "method": "session/update",
                            "params": { "sessionId": "acp-1",
                                "update": { "sessionUpdate": "tool_call", "toolCallId": "call_1",
                                    "title": "shell", "status": "completed",
                                    "rawInput": { "cmd": "ls" } } }
                        }),
                        // ToolCallUpdate 不带 title：验证 name 从前序 ToolCall 缓存补
                        serde_json::json!({
                            "jsonrpc": "2.0", "method": "session/update",
                            "params": { "sessionId": "acp-1",
                                "update": { "sessionUpdate": "tool_call_update",
                                    "toolCallId": "call_1", "status": "completed",
                                    "rawOutput": "a.rs" } }
                        }),
                        // plan + thought：验证新事件类型的落库
                        serde_json::json!({
                            "jsonrpc": "2.0", "method": "session/update",
                            "params": { "sessionId": "acp-1",
                                "update": { "sessionUpdate": "agent_thought_chunk",
                                    "content": { "type": "text", "text": "思考一下" } } }
                        }),
                        serde_json::json!({
                            "jsonrpc": "2.0", "method": "session/update",
                            "params": { "sessionId": "acp-1",
                                "update": { "sessionUpdate": "plan",
                                    "entries": [ { "content": "步骤一", "priority": "high",
                                        "status": "in_progress" } ] } }
                        }),
                    ];
                    for line_value in &stream_lines {
                        let mut bytes = serde_json::to_vec(line_value).unwrap();
                        bytes.push(b'\n');
                        if stdout_tx.send(bytes).await.is_err() {
                            return;
                        }
                    }
                    // 响应 gate：None 立即回；Some 等待一个许可（队列/取消测试
                    // 需要精确控制「回合何时结束」）。等待期间回合保持 busy。
                    if let Some(permits) = &mut prompt_permits {
                        let _ = permits.recv().await;
                    }
                    let resp = serde_json::json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": { "stopReason": "end_turn" }
                    });
                    let mut bytes = serde_json::to_vec(&resp).unwrap();
                    bytes.push(b'\n');
                    if stdout_tx.send(bytes).await.is_err() {
                        return;
                    }
                }
                other => {
                    tracing::debug!("mock agent: unknown method {other}");
                    continue;
                }
            }
            for line_value in out_lines {
                let mut bytes = serde_json::to_vec(&line_value).unwrap();
                bytes.push(b'\n');
                if stdout_tx.send(bytes).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// 建一个含 workspace/session + 空会话表的 persist 环境。
pub(super) async fn persist_env() -> (Database, Arc<Mutex<HashMap<String, SpawnedAgent>>>) {
    let db = Database::new(":memory:").await.unwrap();
    db.agent_create_workspace(
        "w1",
        "proj",
        "nas",
        "host",
        "/workspace",
        None,
        None,
        "gemini",
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    db.agent_create_session("sess-1", "w1", None, Some("gpt-4o"))
        .await
        .unwrap();
    let sessions = Arc::new(Mutex::new(HashMap::new()));
    sessions
        .lock()
        .await
        .insert("sess-1".into(), spawned_agent());
    (db, sessions)
}

/// 端到端 ensure_session mock 客户端：注册模拟客户端，自动应答协商请求
/// （AgentLlmProxyStart → AgentLlmProxyReady、AgentSpawnRequest →
/// AgentSpawnResponse success），并扮演 ACP 进程——stdin 数据（JSON-RPC 请求）
/// 经 pump → 控制通道到达后转发给 [`mock_acp_agent`] 逐行应答，进程 stdout
/// 经 `SpawnedAgent.stdout_tx` 送回 pump → ACP 连接。生产接线
/// `ensure_session` 全链路（start_llm_proxy → spawn_agent → handshake → 配置注入）
/// 不经此桥无法完成，测试由此验证真实调用顺序。
#[allow(clippy::too_many_arguments)]
pub(super) async fn spawn_e2e_client(
    registry: &crate::test_helpers::TestRegistry,
    sessions: &Arc<Mutex<HashMap<String, SpawnedAgent>>>,
    config_options: serde_json::Value,
    applied: Arc<Mutex<Vec<(String, String)>>>,
    fail_config_id: Option<&str>,
    mcp_http: bool,
    mcp_servers: Arc<Mutex<Vec<serde_json::Value>>>,
) {
    let (tx, mut rx) = mpsc::channel::<ControlMessage>(64);
    registry.register("nas", None, tx).await;
    let registry2 = registry.clone();
    let (control_tx, control_rx) = mpsc::channel::<ControlMessage>(32);
    let (proc_stdout_tx, mut proc_stdout_rx) = mpsc::channel::<Vec<u8>>(128);
    // 进程侧 ACP 模拟（复用 handshake 测试的 mock_acp_agent 逻辑）
    tokio::spawn(mock_acp_agent(
        control_rx,
        proc_stdout_tx,
        config_options,
        applied,
        None,
        None,
        false,
        fail_config_id.map(str::to_string),
        mcp_http,
        mcp_servers,
    ));
    // 客户端侧：协商应答 + stdin（server→process）桥接到进程侧
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            match msg {
                ControlMessage::AgentLlmProxyStart { session_id } => {
                    registry2
                        .resolve_spawn_pending(
                            &session_id,
                            ControlMessage::AgentLlmProxyReady {
                                session_id: session_id.clone(),
                                port: 45678,
                            },
                        )
                        .await;
                }
                ControlMessage::AgentSpawnRequest { session_id, .. } => {
                    registry2
                        .resolve_spawn_pending(
                            &session_id,
                            ControlMessage::AgentSpawnResponse {
                                session_id: session_id.clone(),
                                success: true,
                                error: None,
                            },
                        )
                        .await;
                }
                ControlMessage::AgentSpawnData {
                    data,
                    stdin: true,
                    session_id,
                } => {
                    let _ = control_tx
                        .send(ControlMessage::AgentSpawnData {
                            data,
                            stdin: true,
                            session_id,
                        })
                        .await;
                }
                // AgentLlmProxyStop / AgentExecCancel 等单向清理：忽略。
                _ => {}
            }
        }
    });
    // 进程 stdout（process→server）桥接到 pump（经 SpawnedAgent.stdout_tx）
    let sessions3 = sessions.clone();
    tokio::spawn(async move {
        while let Some(bytes) = proc_stdout_rx.recv().await {
            let st = sessions3
                .lock()
                .await
                .get("sess-1")
                .and_then(|a| a.stdout_tx.clone());
            if let Some(st) = st {
                let _ = st.send(bytes).await;
            }
        }
    });
}

/// 构造带注册客户端（nas）的空 bridge（MCP 注入协商测试用）。
pub(super) async fn handshake_test_bridge() -> AcpBridge {
    let db = Database::new(":memory:").await.unwrap();
    db.save_server_auth("secret").await.unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let (tx, _rx) = mpsc::channel::<ControlMessage>(32);
    registry.register("nas", None, tx).await;
    AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db)
}

/// 注册客户端并观察 AgentExecCancel（优雅取消测试断言「未杀进程」，
/// 兜底测试断言「已杀进程」）。
pub(super) async fn register_cancel_observer(
    registry: &crate::test_helpers::TestRegistry,
) -> Arc<Mutex<Vec<String>>> {
    let (client_tx, mut client_rx) = mpsc::channel::<ControlMessage>(32);
    registry.register("nas", None, client_tx).await;
    let cancels = Arc::new(Mutex::new(Vec::<String>::new()));
    let observer = cancels.clone();
    tokio::spawn(async move {
        while let Some(msg) = client_rx.recv().await {
            if let ControlMessage::AgentExecCancel { request_id } = msg {
                observer.lock().await.push(request_id);
            }
        }
    });
    cancels
}

/// 轮询 `cond`（async 闭包）直到返回 true（超时 panic）。
pub(super) async fn wait_until<F, Fut>(timeout: Duration, mut cond: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = std::time::Instant::now() + timeout;
    while !cond().await {
        assert!(
            std::time::Instant::now() < deadline,
            "condition not met within {timeout:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 建 workspace w1 + session sess-1（`agent_set_acp_session_id` 落库需要
/// session 行；`agent_get_session` 断言同样依赖它）。
pub(super) async fn seeded_bridge() -> (AcpBridge, Database) {
    let db = Database::new(":memory:").await.unwrap();
    db.save_server_auth("secret").await.unwrap();
    db.agent_create_workspace(
        "w1", "proj", "nas", "host", "/ws", None, None, "gemini", None, None, None, None,
    )
    .await
    .unwrap();
    db.agent_create_session("sess-1", "w1", None, Some("gpt-4o"))
        .await
        .unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db.clone());
    (bridge, db)
}
