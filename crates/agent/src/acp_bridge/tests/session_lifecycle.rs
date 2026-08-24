use super::super::*;
use super::helpers::*;

#[tokio::test]
async fn test_submit_prompt_busy_persists_pending() {
    // busy 入队：消息同时落 agent_pending_prompts（persist_id=Some），
    // 重启后可恢复。
    let db = Database::new(":memory:").await.unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db.clone());
    let mut agent = spawned_agent();
    agent.busy = true;
    bridge.sessions.lock().await.insert("sess-1".into(), agent);

    bridge
        .submit_prompt("sess-1", "排队消息一", vec![])
        .await
        .expect("enqueue should succeed");

    let rows = db.agent_pending_list("sess-1").await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, "排队消息一");
    let sessions = bridge.sessions.lock().await;
    let queued = &sessions.get("sess-1").unwrap().pending_prompts;
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].persist_id.as_deref(), Some(rows[0].0.as_str()));
}

#[tokio::test]
async fn test_ensure_session_restores_persisted_pending() {
    // 恢复路径：DB 里残留的排队 prompt 在 ensure_session 重拉时载入条目；
    // 落库失败的纯内存降级项（persist_id=None）保留在队尾，不与 DB 行重复。
    let db = Database::new(":memory:").await.unwrap();
    db.save_server_auth("secret").await.unwrap();
    db.agent_create_workspace(
        "w1", "proj", "nas", "host", "/workspace", None, None, "gemini",
        None, Some("model-1"), None, None,
    )
    .await
    .unwrap();
    db.agent_create_session("sess-1", "w1", None, None)
        .await
        .unwrap();
    db.agent_pending_enqueue("p1", "sess-1", "重启前排队", "[]")
        .await
        .unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db.clone())
        .with_llm_gateway(test_gateway(&db));
    // exited 旧条目：带一项无持久副本的降级 prompt（验证合并去重）
    let mut exited = spawned_agent();
    exited.exited = true;
    exited.pending_prompts.push_back(PendingPrompt {
        content: "降级内存项".into(),
        refs: vec![],
        persist_id: None,
    });
    bridge.sessions.lock().await.insert("sess-1".into(), exited);

    // 直接调恢复逻辑（ensure_session 在 spawn 前调用同一函数；spawn 失败
    // 会移除占位条目，事后无法观测内存态）。
    bridge.restore_pending_prompts("sess-1").await;
    let sessions = bridge.sessions.lock().await;
    let queue = &sessions.get("sess-1").unwrap().pending_prompts;
    assert_eq!(queue.len(), 2, "DB 行 + 降级内存项");
    // DB 行在前（FIFO），带持久 id；降级项居队尾
    assert_eq!(queue[0].content, "重启前排队");
    assert_eq!(queue[0].persist_id.as_deref(), Some("p1"));
    assert_eq!(queue[1].content, "降级内存项");
    assert_eq!(queue[1].persist_id, None);
}

#[tokio::test]
async fn test_ensure_session_no_gateway_rejected_early() {
    // 网关未注入（生产：启动时无 provider → llm_state 为空）→ 前置拦截，
    // 不发起任何控制通道请求（agent spawn 出来也只会每个 LLM 请求 502）。
    let db = Database::new(":memory:").await.unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db);
    let (ws_tx, _rx) = mpsc::channel(16);
    let err = bridge
        .ensure_session("sess-1", &acp_workspace(), ws_tx, TEST_CONN_ID)
        .await
        .expect_err("missing gateway should be rejected");
    assert!(err.contains("LLM 网关未配置"), "err: {err}");
    assert!(err.contains("gateway:"), "stage 前缀缺失: {err}");
}

#[tokio::test]
async fn test_spawn_failure_persisted_with_stage_attribution() {
    // spawn 失败（llm_proxy 阶段：客户端离线）→ 归因带 stage 前缀持久化到
    // agent_sessions.last_spawn_error，重启后仍可追溯。
    let db = Database::new(":memory:").await.unwrap();
    db.agent_create_workspace(
        "w1", "proj", "ghost", "host", "/workspace", None, None, "gemini",
        None, Some("model-1"), None, None,
    )
    .await
    .unwrap();
    db.agent_create_session("sess-1", "w1", None, None)
        .await
        .unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db.clone())
        .with_llm_gateway(test_gateway(&db));
    let mut ws = acp_workspace();
    ws.client_id = "ghost".into();
    let (ws_tx, _rx) = mpsc::channel(16);
    let err = bridge
        .ensure_session("sess-1", &ws, ws_tx, TEST_CONN_ID)
        .await
        .expect_err("offline client should fail");
    assert!(err.starts_with("llm_proxy:"), "stage 前缀缺失: {err}");
    let s = db.agent_get_session("sess-1").await.unwrap().unwrap();
    let persisted = s.last_spawn_error.expect("spawn error should persist");
    assert!(persisted.starts_with("llm_proxy:"), "persisted: {persisted}");
}

#[tokio::test]
async fn test_ensure_session_docker_rejected() {
    let bridge = mock_bridge(|_| unreachable!("docker rejection should not spawn")).await;
    let (ws_tx, _rx) = mpsc::channel(16);
    let err = bridge
        .ensure_session("sess-1", &docker_workspace(), ws_tx, TEST_CONN_ID)
        .await
        .expect_err("docker workspace should be rejected");
    assert!(err.contains("docker"), "err: {err}");
}

#[tokio::test]
async fn test_ensure_session_missing_model_config_rejected() {
    // workspace/session/全局默认均未配置模型 → spawn 前门禁拦截（不发起任何请求）
    let bridge = mock_bridge(|_| unreachable!("missing model should not spawn")).await;
    let mut ws = acp_workspace();
    ws.llm_model_id = None;
    let (ws_tx, _rx) = mpsc::channel(16);
    let err = bridge
        .ensure_session("sess-1", &ws, ws_tx, TEST_CONN_ID)
        .await
        .expect_err("no model config should be rejected");
    assert!(err.contains("未配置"), "err: {err}");
}

#[tokio::test]
async fn test_session_spawned_reflects_registry() {
    // 未登记 → false；登记存活条目 → true；exited 陈旧条目 → false
    let bridge = mock_bridge(|_| unreachable!("no requests expected")).await;
    assert!(!bridge.session_spawned("sess-1").await);
    bridge
        .sessions
        .lock()
        .await
        .insert("sess-1".into(), spawned_agent());
    assert!(bridge.session_spawned("sess-1").await);
    let mut exited = spawned_agent();
    exited.exited = true;
    bridge.sessions.lock().await.insert("sess-1".into(), exited);
    assert!(!bridge.session_spawned("sess-1").await);
}

#[tokio::test]
async fn test_ensure_session_session_model_passes_gate() {
    // session.model 已配置、workspace 未配 llm_model_id → 门禁放行，进入 spawn
    // 路径（此处 LLM 代理绑定失败 → 错误是 bind，而非「未配置」）。
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
        None,
        None,
        None,
    )
    .await
    .unwrap();
    db.agent_create_session("sess-1", "w1", None, Some("gpt-4o"))
        .await
        .unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let (tx, mut rx) = mpsc::channel(32);
    registry
        .register("nas", None, tx)
        .await;
    let registry2 = registry.clone();
    tokio::spawn(async move {
        while let Some(req) = rx.recv().await {
            let sid = match &req {
                ControlMessage::AgentSpawnRequest { session_id, .. } => session_id.clone(),
                ControlMessage::AgentLlmProxyStart { session_id } => session_id.clone(),
                // AgentLlmProxyStop 是单向清理消息（无响应、无等待者），跳过不协商。
                ControlMessage::AgentLlmProxyStop { .. } => continue,
                other => panic!("unexpected request: {other:?}"),
            };
            let resp = match &req {
                ControlMessage::AgentLlmProxyStart { session_id } => {
                    ControlMessage::AgentLlmProxyReady {
                        session_id: session_id.clone(),
                        port: 0, // 绑定失败：spawn 快速失败，验证门禁已过
                    }
                }
                other => panic!("unexpected request: {other:?}"),
            };
            registry2.resolve_spawn_pending(&sid, resp).await;
        }
    });
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db.clone())
        .with_llm_gateway(test_gateway(&db));
    let mut ws = acp_workspace();
    ws.llm_model_id = None;
    let (ws_tx, _rx) = mpsc::channel(16);
    let err = bridge
        .ensure_session("sess-1", &ws, ws_tx, TEST_CONN_ID)
        .await
        .expect_err("spawn should be attempted past the model gate");
    assert!(
        err.contains("failed to bind"),
        "error should be from spawn, not model gate: {err}"
    );
}

#[tokio::test]
async fn test_ensure_session_offline_client_fails() {
    // 客户端未注册 → start_llm_proxy 报 NotConnected，before spawn_agent
    let db = Database::new(":memory:").await.unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db.clone())
        .with_llm_gateway(test_gateway(&db));
    let mut ws = acp_workspace();
    ws.client_id = "ghost".into();
    let (ws_tx, _rx) = mpsc::channel(16);
    let err = bridge
        .ensure_session("sess-1", &ws, ws_tx, TEST_CONN_ID)
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
        .ensure_session("sess-1", &ws, ws_tx, TEST_CONN_ID)
        .await
        .expect_err("unsupported agent type should fail locally");
    assert!(err.contains("unsupported agent type"), "err: {err}");
}

#[tokio::test]
async fn test_wait_ready_reports_cached_spawn_failure() {
    // 预 spawn 失败会移除占位条目：后到/在等的 wait_ready 必须拿到缓存的
    // 真实原因，而非误导性的 "session not spawned"。
    let db = Database::new(":memory:").await.unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db.clone())
        .with_llm_gateway(test_gateway(&db));
    let mut ws = acp_workspace();
    ws.client_id = "ghost".into();
    let (ws_tx, _rx) = mpsc::channel(16);
    let _ = bridge
        .ensure_session("sess-1", &ws, ws_tx, TEST_CONN_ID)
        .await;

    let err = bridge
        .wait_ready("sess-1")
        .await
        .expect_err("failed spawn should surface via wait_ready");
    assert!(
        err.contains("llm proxy start failed"),
        "real spawn error should propagate, got: {err}"
    );
}

#[tokio::test]
async fn test_wait_ready_unknown_session_reports_not_spawned() {
    // 从未尝试过 spawn 的会话：无失败缓存，保持 "session not spawned"。
    let db = Database::new(":memory:").await.unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db);
    let err = bridge
        .wait_ready("sess-x")
        .await
        .expect_err("unknown session should error");
    assert_eq!(err, "session not spawned");
}

#[tokio::test]
async fn test_wait_ready_retry_clears_stale_failure() {
    // 失败缓存不得污染新一轮尝试：重试（新占位插入）清旧值后，新一轮
    // 在途期间条目缺失的极端窗口不应再报旧错误。这里验证重试失败后
    // 缓存被新错误覆盖（而非残留首次错误）。
    let bridge = mock_bridge(|req| match req {
        ControlMessage::AgentLlmProxyStart { session_id } => {
            ControlMessage::AgentLlmProxyReady {
                session_id,
                port: 0, // 绑定失败：每次 spawn 都以相同原因失败
            }
        }
        other => panic!("unexpected request: {other:?}"),
    })
    .await;
    let ws = acp_workspace();
    for _ in 0..2 {
        let (ws_tx, _rx) = mpsc::channel(16);
        let _ = bridge
            .ensure_session("sess-1", &ws, ws_tx, TEST_CONN_ID)
            .await;
    }
    let err = bridge
        .wait_ready("sess-1")
        .await
        .expect_err("failed spawn should surface via wait_ready");
    assert!(
        err.contains("failed to bind"),
        "latest failure should be cached, got: {err}"
    );
}

#[tokio::test]
async fn test_wait_ready_visible_when_ready_set_before_subscribe() {
    // 回归：spawn 在首个 wait_ready（首个 subscribe）之前完成时，就绪置位
    // 必须对后到订阅者可见。spawn_ready 建通道只存 Sender（Receiver 当场
    // drop），watch::Sender::send 在无接收者时静默失败且**不写入新值**——
    // 预 spawn 快于首条消息时 wait_ready 曾空等整个 READY_TIMEOUT，误报
    // 「spawn 仍在进行」。ensure_session 成功路径必须用 send_modify 置位。
    let db = Database::new(":memory:").await.unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db);
    let agent = spawned_agent();
    let sender = agent.spawn_ready.clone();
    bridge.sessions.lock().await.insert("sess-1".into(), agent);
    // 无任何订阅者时置位（等价 ensure_session 成功路径的 send_modify）
    sender.send_modify(|ready| *ready = true);
    tokio::time::timeout(Duration::from_secs(2), bridge.wait_ready("sess-1"))
        .await
        .expect("wait_ready must not time out")
        .expect("ready set before subscribe must be visible");
}

#[tokio::test]
async fn test_ensure_session_dedup_skips_respawn() {
    // 会话已登记 → ensure_session 直接 Ok，不发起任何 spawn
    let bridge = mock_bridge(|_| unreachable!("dedup should not spawn")).await;
    bridge
        .sessions
        .lock()
        .await
        .insert("sess-1".into(), spawned_agent());
    let (ws_tx, _rx) = mpsc::channel(16);
    bridge
        .ensure_session("sess-1", &acp_workspace(), ws_tx, TEST_CONN_ID)
        .await
        .expect("dedup should return Ok");
}

#[tokio::test]
async fn test_ensure_session_exited_entry_respawns() {
    // 回归：exited=true 的陈旧条目不能短路 dedup。用离线客户端让 spawn
    // 路径快速失败——若 ensure_session 直接 Ok（bug 行为）则 expect_err panic。
    let db = Database::new(":memory:").await.unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db.clone())
        .with_llm_gateway(test_gateway(&db));
    let mut exited = spawned_agent();
    exited.exited = true;
    bridge.sessions.lock().await.insert("sess-1".into(), exited);
    let mut ws = acp_workspace();
    ws.client_id = "ghost".into();
    let (ws_tx, _rx) = mpsc::channel(16);
    let err = bridge
        .ensure_session("sess-1", &ws, ws_tx, TEST_CONN_ID)
        .await
        .expect_err("exited entry must attempt respawn, not short-circuit Ok");
    assert!(err.contains("llm proxy start failed"), "err: {err}");
    // 陈旧条目已被移除（spawn 失败后不留占位），再次 ensure_session 仍可重试
    assert!(
        !bridge.sessions.lock().await.contains_key("sess-1"),
        "stale exited entry should be gone"
    );
}

#[tokio::test]
async fn test_prompt_unknown_session_errors() {
    let db = Database::new(":memory:").await.unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db);
    let err = bridge
        .prompt("no-such-session", "hi")
        .await
        .expect_err("unknown session should error");
    assert!(err.contains("not spawned"), "err: {err}");
}

#[tokio::test]
async fn test_prompt_after_exit_errors() {
    let db = Database::new(":memory:").await.unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db);
    let mut exited = spawned_agent();
    exited.exited = true;
    bridge.sessions.lock().await.insert("sess-1".into(), exited);
    let err = bridge
        .prompt("sess-1", "hi")
        .await
        .expect_err("exited session should error");
    assert!(err.contains("exited"), "err: {err}");
}

#[tokio::test]
async fn test_handle_spawn_exit_marks_exited() {
    let db = Database::new(":memory:").await.unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db);
    bridge
        .sessions
        .lock()
        .await
        .insert("sess-1".into(), spawned_agent());
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
