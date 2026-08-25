use super::super::*;
use super::helpers::*;

/// 生产接线端到端：`ensure_session` 内部真实调用顺序 apply_config_overrides →
/// replay_config_state → spawn_ready（不经手工逐函数调用）。workspace overrides
/// 注入后 session 级 config_state 回放覆盖之；全部完成后 spawn_ready 才置位。
#[tokio::test]
async fn test_ensure_session_production_config_injection_order() {
    let db = Database::new(":memory:").await.unwrap();
    db.save_server_auth("secret").await.unwrap();
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
        Some("model-1"),
        Some(r#"{"model":"sonnet","fast":"haiku"}"#),
        None,
    )
    .await
    .unwrap();
    db.agent_create_session("sess-1", "w1", None, None)
        .await
        .unwrap();
    // 用户显式 config_state：model→opus，必须覆盖 workspace 注入的 sonnet
    db.agent_update_session_config_state("sess-1", "model", Some("opus"))
        .await
        .unwrap();

    let options = serde_json::json!([
        {"id": "model", "name": "Model", "type": "select", "currentValue": "sonnet",
         "options": [{"value": "sonnet", "name": "Sonnet"}, {"value": "opus", "name": "Opus"}]},
        {"id": "fast", "name": "Fast model", "type": "select", "currentValue": "haiku",
         "options": [{"value": "haiku", "name": "Haiku"}]}
    ]);
    let applied = Arc::new(Mutex::new(Vec::new()));
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let bridge = AcpBridge::new(
        AgentSpawner::new(std::sync::Arc::new(registry.clone())),
        db.clone(),
    )
    .with_llm_gateway(test_gateway(&db));
    // 本用例不测 MCP 注入：mock 缺省 http=false，mcpServers 丢弃。
    spawn_e2e_client(
        &registry,
        &bridge.sessions,
        options,
        applied.clone(),
        None,
        false,
        Arc::new(Mutex::new(Vec::new())),
    )
    .await;

    let (ws_tx, _ws_rx) = mpsc::channel::<serde_json::Value>(16);
    let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
    bridge
        .ensure_session("sess-1", &ws, ws_tx, TEST_CONN_ID)
        .await
        .expect("ensure_session 应走完全部注入");

    // 调用顺序 = 生产接线：workspace overrides（fast 先于 model）→ config_state
    // 回放（model→opus，用户显式选择覆盖 workspace 默认）。
    let calls = applied.lock().await.clone();
    assert_eq!(
        calls,
        vec![
            ("fast".to_string(), "haiku".to_string()),
            ("model".to_string(), "sonnet".to_string()),
            ("model".to_string(), "opus".to_string()),
        ],
        "ensure_session 真实注入顺序错误: {calls:?}"
    );
    // 配置注入全部完成后 spawn_ready 才置位（wait_ready 依赖此信号，首条
    // prompt 不与在途 set_config_option 竞态）
    let ready = *bridge
        .sessions
        .lock()
        .await
        .get("sess-1")
        .unwrap()
        .spawn_ready
        .borrow();
    assert!(ready, "spawn_ready 应在配置注入完成后置位");
    // config_options 快照已从 session/new 捕获
    assert_eq!(
        bridge
            .sessions
            .lock()
            .await
            .get("sess-1")
            .unwrap()
            .config_options
            .len(),
        2
    );
}

// ── remember MCP server 注入协商 ───────────────────────────

/// agent 声明 mcp http 能力 + 会话 token → session/new 的 mcpServers 注入
/// remember MCP server（type=http、name=rust-tunnel-memory、URL 指向回环代理
/// `http://127.0.0.1:45678/mcp/tok123`）。
#[tokio::test]
async fn test_handshake_injects_mcp_server_when_http_capable() {
    let bridge = handshake_test_bridge().await;
    let (ws_tx, _ws_rx) = mpsc::channel::<serde_json::Value>(16);
    let mcp_servers = Arc::new(Mutex::new(Vec::new()));
    setup_handshake_mcp(
        &bridge,
        ws_tx,
        true,
        Some("tok123"),
        mcp_servers.clone(),
        "claude-code",
    )
    .await;

    let received = mcp_servers.lock().await.clone();
    assert_eq!(
        received.len(),
        1,
        "session/new 应记录一次 mcpServers: {received:?}"
    );
    let servers = received[0].as_array().expect("mcpServers 应为数组");
    assert_eq!(
        servers.len(),
        1,
        "http 能力 + token 应注入 1 条 server: {received:?}"
    );
    let entry = &servers[0];
    assert_eq!(entry["type"], "http");
    assert_eq!(entry["name"], "rust-tunnel-memory");
    assert_eq!(entry["url"], "http://127.0.0.1:45678/mcp/tok123");
}

/// agent 无 mcp http 能力（缺省 http=false）→ mcpServers 为空数组，handshake
/// 照常成功（降级不报错）。
#[tokio::test]
async fn test_handshake_skips_mcp_injection_without_http_capability() {
    let bridge = handshake_test_bridge().await;
    let (ws_tx, _ws_rx) = mpsc::channel::<serde_json::Value>(16);
    let mcp_servers = Arc::new(Mutex::new(Vec::new()));
    // 有 token 但无能力：能力是注入的前置条件
    setup_handshake_mcp(
        &bridge,
        ws_tx,
        false,
        Some("tok123"),
        mcp_servers.clone(),
        "claude-code",
    )
    .await;

    let received = mcp_servers.lock().await.clone();
    assert_eq!(received.len(), 1, "session/new 仍应记录（空）mcpServers");
    assert!(
        received[0].as_array().is_some_and(std::vec::Vec::is_empty),
        "无 http 能力时 mcpServers 应为空: {received:?}"
    );
    // 会话照常建立
    assert!(bridge
        .sessions
        .lock()
        .await
        .get("sess-1")
        .unwrap()
        .connection
        .is_some());
}

/// 有 http 能力但会话未铸造 token（memory 未启用等）→ 不注入，不报错。
#[tokio::test]
async fn test_handshake_skips_mcp_injection_without_token() {
    let bridge = handshake_test_bridge().await;
    let (ws_tx, _ws_rx) = mpsc::channel::<serde_json::Value>(16);
    let mcp_servers = Arc::new(Mutex::new(Vec::new()));
    setup_handshake_mcp(
        &bridge,
        ws_tx,
        true,
        None,
        mcp_servers.clone(),
        "claude-code",
    )
    .await;

    let received = mcp_servers.lock().await.clone();
    assert_eq!(received.len(), 1, "session/new 仍应记录（空）mcpServers");
    assert!(
        received[0].as_array().is_some_and(std::vec::Vec::is_empty),
        "无 token 时 mcpServers 应为空: {received:?}"
    );
}

/// opencode 与其他 agent 一样注入 remember MCP server。opencode 与隧道 MCP
/// 端点兼容性已复验（2026-08-16，opencode 1.18.18 全链路 probe，SDK 1.29.0
/// StreamableHTTP）：此前误诊的 initialize 挂起实为当时 LLM provider 配置问题，
/// 门控已移除，注入行为与 claude-code 等 agent 一致。
#[tokio::test]
async fn test_handshake_injects_mcp_for_opencode() {
    let bridge = handshake_test_bridge().await;
    let (ws_tx, _ws_rx) = mpsc::channel::<serde_json::Value>(16);
    let mcp_servers = Arc::new(Mutex::new(Vec::new()));
    setup_handshake_mcp(
        &bridge,
        ws_tx,
        true,
        Some("tok123"),
        mcp_servers.clone(),
        "opencode",
    )
    .await;

    let received = mcp_servers.lock().await.clone();
    assert_eq!(
        received.len(),
        1,
        "session/new 应记录一次 mcpServers: {received:?}"
    );
    let servers = received[0].as_array().expect("mcpServers 应为数组");
    assert_eq!(
        servers.len(),
        1,
        "http 能力 + token 应注入 1 条 server: {received:?}"
    );
    let entry = &servers[0];
    assert_eq!(entry["type"], "http");
    assert_eq!(entry["name"], "rust-tunnel-memory");
    let url = entry["url"].as_str().expect("mcp url 应为字符串");
    assert!(
        url.contains("/mcp/tok123"),
        "mcp url 应指向会话 token 端点: {url}"
    );
}

/// 生产接线端到端（MCP 注入）：带 memory 的 `ensure_session` 在 handshake 的
/// session/new 注入 mcpServers——URL 端口 = e2e mock 的 LLM 回环代理端口
/// （45678），token = ensure_session 铸造的 32 位 hex；spawn_ready 照常置位。
#[cfg(feature = "rag")]
#[tokio::test]
async fn test_ensure_session_injects_mcp_server_with_memory() {
    let db = Database::new(":memory:").await.unwrap();
    db.save_server_auth("secret").await.unwrap();
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
        Some("model-1"),
        None,
        None,
    )
    .await
    .unwrap();
    db.agent_create_session("sess-1", "w1", None, None)
        .await
        .unwrap();

    let options = serde_json::json!([
        {"id": "model", "name": "Model", "type": "select", "currentValue": "sonnet",
         "options": [{"value": "sonnet", "name": "Sonnet"}]}
    ]);
    let applied = Arc::new(Mutex::new(Vec::new()));
    let mcp_servers = Arc::new(Mutex::new(Vec::new()));
    let registry = crate::test_helpers::TestRegistry::new(&db);
    // memory 注入（token 铸造前置条件）：mock embedding server + 开启 settings。
    let base = crate::memory::mock_embedding_server(8).await;
    let (_mdb, memory) = crate::memory::test_memory_with_embedding(&base).await;
    let bridge = AcpBridge::new(
        AgentSpawner::new(std::sync::Arc::new(registry.clone())),
        db.clone(),
    )
    .with_memory(memory)
    .with_llm_gateway(test_gateway(&db));
    // e2e mock 声明 mcp http 能力并记录收到的 mcpServers。
    spawn_e2e_client(
        &registry,
        &bridge.sessions,
        options,
        applied.clone(),
        None,
        true,
        mcp_servers.clone(),
    )
    .await;

    let (ws_tx, _ws_rx) = mpsc::channel::<serde_json::Value>(16);
    let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
    bridge
        .ensure_session("sess-1", &ws, ws_tx, TEST_CONN_ID)
        .await
        .expect("ensure_session 应带 memory 完整走通");

    // session/new 收到注入的 mcpServers（1 条 http server）
    let received = mcp_servers.lock().await.clone();
    assert_eq!(
        received.len(),
        1,
        "session/new 应记录一次 mcpServers: {received:?}"
    );
    let servers = received[0].as_array().expect("mcpServers 应为数组");
    assert_eq!(
        servers.len(),
        1,
        "http 能力 + memory 应注入 1 条 server: {received:?}"
    );
    let entry = &servers[0];
    assert_eq!(entry["type"], "http");
    assert_eq!(entry["name"], "rust-tunnel-memory");
    let url = entry["url"].as_str().expect("mcp url 应为字符串");
    let token = url
        .strip_prefix("http://127.0.0.1:45678/mcp/")
        .expect("URL 应指向 e2e mock 的 LLM 回环代理端口: {url}");
    assert_eq!(token.len(), 32, "token 应为 32 位 hex: {token}");
    assert!(
        token.chars().all(|c| c.is_ascii_hexdigit()),
        "token 应全为 hex: {token}"
    );
    // 注入的 URL token 与会话条目内 mcp_token 一致（token 本体只存这里，吊销靠条目移除）
    assert_eq!(
        bridge
            .sessions
            .lock()
            .await
            .get("sess-1")
            .unwrap()
            .mcp_token
            .as_deref(),
        Some(token),
        "mcp_token 应与注入 URL 的 token 一致"
    );
    // 配置注入全部完成后 spawn_ready 照常置位（MCP 注入不改变 spawn 完成信号）
    assert!(
        *bridge
            .sessions
            .lock()
            .await
            .get("sess-1")
            .unwrap()
            .spawn_ready
            .borrow(),
        "spawn_ready 应在注入后置位"
    );
}

/// set_config_option 硬失败继续注入（overrides 路径）：mock 对 model 回 JSON-RPC
/// error，apply_config_overrides 必须跳过该条并继续注入 fast（nonexistent 本地
/// 校验跳过）——现有 mock 恒成功，此路径此前无独立测试。
#[tokio::test]
async fn test_config_injection_continues_after_hard_failure() {
    let db = Database::new(":memory:").await.unwrap();
    db.save_server_auth("secret").await.unwrap();
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
        Some("model-1"),
        Some(r#"{"model":"sonnet","fast":"haiku","nonexistent":"x"}"#),
        None,
    )
    .await
    .unwrap();
    db.agent_create_session("sess-1", "w1", None, None)
        .await
        .unwrap();

    let options = serde_json::json!([
        {"id": "model", "name": "Model", "type": "select", "currentValue": "sonnet",
         "options": [{"value": "sonnet", "name": "Sonnet"}, {"value": "opus", "name": "Opus"}]},
        {"id": "fast", "name": "Fast model", "type": "select", "currentValue": "haiku",
         "options": [{"value": "haiku", "name": "Haiku"}]}
    ]);
    let applied = Arc::new(Mutex::new(Vec::new()));
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db.clone());
    let (ws_tx, _ws_rx) = mpsc::channel::<serde_json::Value>(16);
    // mock 对 model 的 set_config_option 回 JSON-RPC error
    setup_handshake_with(
        &bridge,
        ws_tx,
        options,
        applied.clone(),
        None,
        None,
        None,
        false,
        Some("model"),
    )
    .await;

    let ws = db.agent_get_workspace("w1").await.unwrap().unwrap();
    bridge.apply_config_overrides("sess-1", &ws).await;

    let calls = applied.lock().await.clone();
    assert_eq!(
        calls,
        vec![("fast".to_string(), "haiku".to_string())],
        "model 硬失败后 fast 应继续注入: {calls:?}"
    );
}

/// set_config_option 硬失败继续注入（config_state 回放路径）：回放按 mode 优先
/// 排序，mode 硬失败后 fast 仍注入——单条失败不阻断整个回放。
#[tokio::test]
async fn test_replay_config_state_continues_after_hard_failure() {
    let db = Database::new(":memory:").await.unwrap();
    db.save_server_auth("secret").await.unwrap();
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
        Some("model-1"),
        None,
        None,
    )
    .await
    .unwrap();
    db.agent_create_session("sess-1", "w1", None, None)
        .await
        .unwrap();
    // 回放含 mode（会硬失败，且按排序必先于 fast）与 fast（应照常注入）
    db.agent_update_session_config_state("sess-1", "mode", Some("plan"))
        .await
        .unwrap();
    db.agent_update_session_config_state("sess-1", "fast", Some("haiku"))
        .await
        .unwrap();

    let options = serde_json::json!([
        {"id": "mode", "name": "Mode", "type": "select", "currentValue": "normal",
         "options": [{"value": "normal", "name": "Normal"}, {"value": "plan", "name": "Plan"}]},
        {"id": "fast", "name": "Fast model", "type": "select", "currentValue": "haiku",
         "options": [{"value": "haiku", "name": "Haiku"}]}
    ]);
    let applied = Arc::new(Mutex::new(Vec::new()));
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db.clone());
    let (ws_tx, _ws_rx) = mpsc::channel::<serde_json::Value>(16);
    setup_handshake_with(
        &bridge,
        ws_tx,
        options,
        applied.clone(),
        None,
        None,
        None,
        false,
        Some("mode"),
    )
    .await;

    bridge.replay_config_state("sess-1").await;

    let calls = applied.lock().await.clone();
    assert_eq!(
        calls,
        vec![("fast".to_string(), "haiku".to_string())],
        "mode 硬失败后 fast 回放应继续: {calls:?}"
    );
}

// ── submit_prompt 排队 + 优雅取消/兜底杀进程 ────────────────
