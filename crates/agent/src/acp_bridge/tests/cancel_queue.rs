use super::super::*;
use super::helpers::*;

/// busy 时 submit_prompt 入队并推 queued 帧；当前回合终态后自动发下一条；
/// done 只在队列排空后发（回合连续）。
#[tokio::test]
async fn test_submit_prompt_queues_when_busy_and_drains() {
    let db = Database::new(":memory:").await.unwrap();
    db.save_server_auth("secret").await.unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let (tx, _rx) = mpsc::channel::<ControlMessage>(32);
    registry.register("nas", None, tx).await;
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db);

    let (ws_tx, mut ws_rx) = mpsc::channel::<serde_json::Value>(32);
    let (permit_tx, permit_rx) = mpsc::channel::<()>(16);
    setup_handshake_with(
        &bridge,
        ws_tx.clone(),
        serde_json::json!([]),
        Arc::new(Mutex::new(Vec::new())),
        Some(permit_rx),
        None,
        None,
        false,
        None,
    )
    .await;

    // 第一条：空闲 → 直接跑（busy）
    bridge
        .submit_prompt("sess-1", "first", vec![])
        .await
        .expect("first prompt accepted");
    assert!(bridge.sessions.lock().await.get("sess-1").unwrap().busy);

    // 第二条：busy → 排队 + queued 帧
    bridge
        .submit_prompt("sess-1", "second", vec![])
        .await
        .expect("second prompt queued");
    assert_eq!(
        bridge
            .sessions
            .lock()
            .await
            .get("sess-1")
            .unwrap()
            .pending_prompts
            .len(),
        1,
        "busy prompt should be queued"
    );

    // 放行第一条 → 终态回调 drain → 自动发第二条
    permit_tx.send(()).await.unwrap();

    let mut events = Vec::new();
    let mut text_chunks = 0;
    let mut sent_second_permit = false;
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), ws_rx.recv())
            .await
            .expect("timed out waiting for events")
            .expect("ws channel closed");
        // mock 每回合发 1 个正文 chunk（thought 也是 assistant_chunk 类型，
        // 用 thought 字段区分，只数正文用于判定「下一回合已开跑」）
        if ev["type"] == "assistant_chunk"
            && !ev
                .get("thought")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        {
            text_chunks += 1;
            if text_chunks >= 2 && !sent_second_permit {
                // 第二条已开跑：放行其 PromptResponse
                sent_second_permit = true;
                permit_tx.send(()).await.unwrap();
            }
        }
        let done = ev["type"] == "done";
        events.push(ev);
        if done {
            break;
        }
    }
    assert!(
        sent_second_permit,
        "queued prompt should auto-send after first turn"
    );
    assert_eq!(text_chunks, 2, "both turns should stream");
    let queued = events.iter().filter(|e| e["type"] == "queued").count();
    assert_eq!(queued, 1, "busy queue should push a queued frame");
    assert_eq!(
        events.last().unwrap()["type"],
        "done",
        "done only after the queue drains"
    );
    assert!(!bridge.sessions.lock().await.get("sess-1").unwrap().busy);
}

/// 优雅取消：收到 session/cancel、无 AgentExecCancel、busy 保持到
/// PromptResponse 到达才复位，取消回合不发生产者终态帧。
#[tokio::test]
async fn test_cancel_graceful_keeps_process_and_busy() {
    let db = Database::new(":memory:").await.unwrap();
    db.save_server_auth("secret").await.unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let cancels = register_cancel_observer(&registry).await;
    // 默认 cancel_grace（10s）：测试期间不会触发兜底杀进程
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db);

    let (ws_tx, mut ws_rx) = mpsc::channel::<serde_json::Value>(32);
    let (permit_tx, permit_rx) = mpsc::channel::<()>(16);
    let recorded = Arc::new(Mutex::new(Vec::<String>::new()));
    let recorded2 = recorded.clone();
    setup_handshake_with(
        &bridge,
        ws_tx.clone(),
        serde_json::json!([]),
        Arc::new(Mutex::new(Vec::new())),
        Some(permit_rx),
        Some(recorded2),
        None,
        false,
        None,
    )
    .await;

    bridge
        .prompt("sess-1", "hello")
        .await
        .expect("prompt should send");
    // 消费 mock 的流式通知（assistant_chunk/tool_call/tool_result/thought/plan）
    for _ in 0..5 {
        tokio::time::timeout(Duration::from_secs(5), ws_rx.recv())
            .await
            .expect("timed out waiting for stream")
            .expect("closed");
    }

    bridge.cancel("sess-1").await;
    assert!(
        bridge.sessions.lock().await.get("sess-1").unwrap().busy,
        "graceful cancel keeps busy until PromptResponse arrives"
    );
    // 放行 PromptResponse → 终态回调清 busy。
    // 注意：mock 逐行处理，session/cancel 通知排在 prompt 响应之后才被读取，
    // 因此 session/cancel 的断言放在放行之后。
    permit_tx.send(()).await.unwrap();
    wait_until(Duration::from_secs(2), async || {
        !bridge.sessions.lock().await.get("sess-1").unwrap().busy
    })
    .await;
    // mock 已收到 session/cancel 通知
    wait_until(Duration::from_secs(2), async || {
        recorded.lock().await.iter().any(|m| m == "session/cancel")
    })
    .await;

    // 无 AgentExecCancel（进程保留）
    assert!(
        cancels.lock().await.is_empty(),
        "graceful cancel must not kill the process"
    );
    // 被取消的回合不发生产者终态帧（stopped 已由 WS handler 回发）
    let stale = tokio::time::timeout(Duration::from_millis(300), ws_rx.recv()).await;
    assert!(
        matches!(stale, Err(_) | Ok(None)),
        "cancelled turn must not emit a terminal frame: {stale:?}"
    );
}

/// 兜底杀进程：cancel_grace 调极短，mock agent 不响应 cancel →
/// 超时后 send_agent_cancel + cancel_fallback 帧 + busy 复位。
#[tokio::test]
async fn test_cancel_fallback_kills_after_grace() {
    let db = Database::new(":memory:").await.unwrap();
    db.save_server_auth("secret").await.unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let cancels = register_cancel_observer(&registry).await;
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db)
        .with_cancel_grace(Duration::from_millis(50));

    let (ws_tx, mut ws_rx) = mpsc::channel::<serde_json::Value>(32);
    // `_permit_tx` 必须保持存活到测试结束：drop 会让 mock 的 recv 返回 None
    // 而放行 PromptResponse（回合结束，兜底无从触发）；且永不 send → 回合
    // 一直 busy，agent 不响应 cancel。
    let (_permit_tx, permit_rx) = mpsc::channel::<()>(16);
    setup_handshake_with(
        &bridge,
        ws_tx.clone(),
        serde_json::json!([]),
        Arc::new(Mutex::new(Vec::new())),
        Some(permit_rx),
        None,
        None,
        false,
        None,
    )
    .await;

    bridge
        .prompt("sess-1", "hello")
        .await
        .expect("prompt should send");
    bridge.cancel("sess-1").await;

    // 等待兜底任务：cancel_fallback 帧
    let frame = loop {
        let ev = tokio::time::timeout(Duration::from_secs(2), ws_rx.recv())
            .await
            .expect("timed out waiting for cancel_fallback")
            .expect("closed");
        if ev["type"] == "cancel_fallback" {
            break ev;
        }
    };
    assert_eq!(frame["type"], "cancel_fallback");
    // 兜底杀进程已下发
    assert_eq!(
        cancels.lock().await.as_slice(),
        &["sess-1".to_string()],
        "fallback should send AgentExecCancel for the session"
    );
    // busy 已复位（回合不再被卡死）
    assert!(!bridge.sessions.lock().await.get("sess-1").unwrap().busy);
}

/// cancel 后队列自动 drain：被取消的回合结束后，排队的消息自动续跑，
/// 新回合的 done 不被 cancelled_turns 误吞。
#[tokio::test]
async fn test_cancel_queued_prompts_auto_send_after_graceful_stop() {
    let db = Database::new(":memory:").await.unwrap();
    db.save_server_auth("secret").await.unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let cancels = register_cancel_observer(&registry).await;
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db);

    let (ws_tx, mut ws_rx) = mpsc::channel::<serde_json::Value>(32);
    let (permit_tx, permit_rx) = mpsc::channel::<()>(16);
    setup_handshake_with(
        &bridge,
        ws_tx.clone(),
        serde_json::json!([]),
        Arc::new(Mutex::new(Vec::new())),
        Some(permit_rx),
        None,
        None,
        false,
        None,
    )
    .await;

    bridge
        .submit_prompt("sess-1", "first", vec![])
        .await
        .expect("first accepted");
    bridge
        .submit_prompt("sess-1", "second", vec![])
        .await
        .expect("second queued");
    bridge
        .submit_prompt("sess-1", "third", vec![])
        .await
        .expect("third queued");
    assert_eq!(
        bridge
            .sessions
            .lock()
            .await
            .get("sess-1")
            .unwrap()
            .pending_prompts
            .len(),
        2
    );

    // 取消当前回合（优雅路径，进程保留）
    bridge.cancel("sess-1").await;

    // 放行被取消的第一回合 → 队列自动 drain（second → third → done）
    permit_tx.send(()).await.unwrap();
    let mut events = Vec::new();
    let mut text_chunks = 0;
    let mut permits_sent = 0;
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), ws_rx.recv())
            .await
            .expect("timed out waiting for drain")
            .expect("closed");
        // 只数正文 chunk（thought 也是 assistant_chunk 类型）判定回合推进
        if ev["type"] == "assistant_chunk"
            && !ev
                .get("thought")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        {
            text_chunks += 1;
            // 第 2、3 个正文 chunk 分别放行 second/third 的 PromptResponse
            if text_chunks >= 2 && permits_sent < 2 {
                permits_sent += 1;
                permit_tx.send(()).await.unwrap();
            }
        }
        let done = ev["type"] == "done";
        events.push(ev);
        if done {
            break;
        }
    }
    assert_eq!(
        text_chunks, 3,
        "queued prompts should auto-send after the cancelled turn"
    );
    // 新回合的 done 不被 cancelled_turns 误吞
    assert_eq!(events.last().unwrap()["type"], "done");
    let queued = events.iter().filter(|e| e["type"] == "queued").count();
    assert_eq!(queued, 2, "second and third were queued");
    assert!(
        cancels.lock().await.is_empty(),
        "graceful cancel queue drain must not kill the process"
    );
    assert!(!bridge.sessions.lock().await.get("sess-1").unwrap().busy);
}

/// cancel 路径 flush：回合缓冲的 assistant 文本/thought 在取消时仍正确落库。
/// `on_receiving_result`（PromptResponse 到达，含 cancelled）先 flush 已有缓冲
/// ——用户能看到的那部分回合过程可追溯，且思考/正文顺序不被颠倒（M11 补测）。
#[tokio::test]
async fn test_cancel_flushes_buffered_turn_segments() {
    let db = Database::new(":memory:").await.unwrap();
    db.save_server_auth("secret").await.unwrap();
    db.agent_create_workspace(&rust_tunnel_persistence::agent::AgentWorkspaceCreateOpts {
        id: "w1".to_owned(),
        name: "proj".to_owned(),
        client_id: "nas".to_owned(),
        runtime_type: "host".to_owned(),
        root_path: "/workspace".to_owned(),
        docker_image: None,
        docker_container_id: None,
        agent_type: "gemini".to_owned(),
        agent_path: None,
        llm_model_id: None,
        agent_config_overrides: None,
        claude_tier_models: None,
    })
    .await
    .unwrap();
    db.agent_create_session("sess-1", "w1", None, Some("gpt-4o"))
        .await
        .unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db.clone());

    let (ws_tx, mut ws_rx) = mpsc::channel::<serde_json::Value>(32);
    let (permit_tx, permit_rx) = mpsc::channel::<()>(16);
    setup_handshake_with(
        &bridge,
        ws_tx.clone(),
        serde_json::json!([]),
        Arc::new(Mutex::new(Vec::new())),
        Some(permit_rx),
        None,
        None,
        false,
        None,
    )
    .await;

    bridge
        .prompt("sess-1", "hello")
        .await
        .expect("prompt should send");
    // 消费 mock 的流式通知（assistant_chunk/tool_call/tool_result/thought/plan）
    // ——tool_call/plan 边界已各自 flush 掉对应缓冲段，缓冲此时为空。
    for _ in 0..5 {
        tokio::time::timeout(Duration::from_secs(5), ws_rx.recv())
            .await
            .expect("timed out waiting for stream")
            .expect("closed");
    }

    // 回合进行中（mock 在 permit 上等待）补入未到边界的缓冲段：模拟最后一个
    // 工具之后的思考→正文尾部（无 tool_call/plan 触发边界 flush）。
    {
        let mut s = bridge.sessions.lock().await;
        let agent = s.get_mut("sess-1").unwrap();
        agent.turn_segments = vec![
            TurnSegment {
                thought: true,
                content: "尾部思考".into(),
                parent_tool_call_id: None,
            },
            TurnSegment {
                thought: false,
                content: "尾部正文".into(),
                parent_tool_call_id: None,
            },
        ];
    }

    // 触发 cancel（真实路径：记代数 + 发 session/cancel + 兜底任务）
    bridge.cancel("sess-1").await;
    // 放行 PromptResponse（stop_reason=cancelled）→ 终态回调 flush 缓冲
    permit_tx.send(()).await.unwrap();
    wait_until(Duration::from_secs(2), async || {
        !bridge.sessions.lock().await.get("sess-1").unwrap().busy
    })
    .await;

    let rows = db.agent_list_messages("sess-1").await.unwrap();
    // 缓冲段已按顺序落库：思考行先行、正文随后（rowid 顺序 = 对话顺序）
    let tail: Vec<(bool, String)> = rows
        .iter()
        .filter(|r| r.kind == "message")
        .map(|r| (r.name.as_deref() == Some("thought"), r.content.clone()))
        .collect();
    assert!(
        tail.contains(&(true, "尾部思考".to_string())),
        "cancel 后缓冲 thought 段应落库: {tail:?}"
    );
    assert!(
        tail.contains(&(false, "尾部正文".to_string())),
        "cancel 后缓冲正文段应落库: {tail:?}"
    );
    let thought_pos = tail
        .iter()
        .position(|(t, c)| *t && c == "尾部思考")
        .unwrap();
    let text_pos = tail
        .iter()
        .position(|(t, c)| !*t && c == "尾部正文")
        .unwrap();
    assert!(thought_pos < text_pos, "思考段必须先于正文段落库: {tail:?}");
    // 缓冲已清空（flush 后 turn_segments 归零）
    assert!(
        bridge
            .sessions
            .lock()
            .await
            .get("sess-1")
            .unwrap()
            .turn_segments
            .is_empty(),
        "flush 后缓冲应清空"
    );
    // 被取消的回合不发生产者终态帧（stopped 已由 WS handler 回发）
    let stale = tokio::time::timeout(Duration::from_millis(300), ws_rx.recv()).await;
    assert!(
        matches!(stale, Err(_) | Ok(None)),
        "cancelled turn must not emit a terminal frame: {stale:?}"
    );
}

// ── ACP 会话上下文持久化：session/resume + session/delete ──
