use super::super::*;
use super::helpers::*;

#[tokio::test]
async fn test_detach_ws_tx_only_clears_own_connection() {
    // 刷新/重连竞态：旧连接 close 检测晚于新连接注册（ensure_session 已把
    // ws_tx 换成新连接的 sender）。旧连接 teardown 必须只清自己的通道，否则
    // 新连接后续 tool_result/done 全部丢弃（前端 running 卡死）。
    let db = Database::new(":memory:").await.unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db);
    let (tx, _rx) = mpsc::channel::<serde_json::Value>(16);
    const OLD: u64 = 1;
    const NEW: u64 = 2;

    // 旧连接独占通道（ws_conn_id=OLD）：其 teardown 应清空
    let mut a = spawned_agent();
    a.ws_tx = Some(tx.clone());
    a.ws_conn_id = OLD;
    bridge.sessions.lock().await.insert("sess-1".into(), a);
    bridge.detach_ws_tx("sess-1", OLD).await;
    assert!(
        bridge
            .sessions
            .lock()
            .await
            .get("sess-1")
            .unwrap()
            .ws_tx
            .is_none(),
        "own detach should clear ws_tx"
    );

    // 新连接已注册（ws_conn_id=NEW）：旧连接晚到的 teardown 不得清掉它
    let mut a = spawned_agent();
    a.ws_tx = Some(tx.clone());
    a.ws_conn_id = NEW;
    bridge.sessions.lock().await.insert("sess-1".into(), a);
    bridge.detach_ws_tx("sess-1", OLD).await;
    assert!(
        bridge
            .sessions
            .lock()
            .await
            .get("sess-1")
            .unwrap()
            .ws_tx
            .is_some(),
        "old connection teardown must not clear newer connection's ws_tx"
    );
    // 新连接自己的 teardown 仍能清空
    bridge.detach_ws_tx("sess-1", NEW).await;
    assert!(bridge
        .sessions
        .lock()
        .await
        .get("sess-1")
        .unwrap()
        .ws_tx
        .is_none());
}

// ── WS 连接变化 watch：审批断线/重连即时拒绝 ──────────────────

#[tokio::test]
async fn test_ws_conn_watch_denies_on_detach() {
    // 断线即时拒绝：审批等待期间 detach_ws_tx 写入 0 → conn_watch 值变化
    // → wait_for 唤醒 → Deny，不等满 5 分钟审批超时（旧实现审批帧缓冲在
    // 无人消费的 channel 里，新重连也看不到审批卡）。
    let db = Database::new(":memory:").await.unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db);

    let (ws_tx, _ws_rx) = mpsc::channel::<serde_json::Value>(16);
    let mut agent = spawned_agent();
    agent.ws_tx = Some(ws_tx);
    agent.ws_conn_id = TEST_CONN_ID;
    agent.ws_conn_watch.send_replace(TEST_CONN_ID);
    let conn_rx = agent.ws_conn_watch.subscribe();
    bridge.sessions.lock().await.insert("sess-1".into(), agent);

    // approval 挂起（永不返回）：等待中途断线由 watch 唤醒短路拒绝。
    let approval: Arc<ApproveFn> = Arc::new(|_, _, _, _, _, _| {
        Box::pin(async {
            std::future::pending::<()>().await;
            ApprovalResult::Approved
        })
    });
    let handle = tokio::spawn(approve_or_disconnect(
        approval,
        "sess-1".into(),
        "shell".into(),
        "ls".into(),
        vec![],
        mpsc::channel::<serde_json::Value>(1).0,
        TEST_CONN_ID,
        conn_rx,
    ));

    // 审批在途时连接断开 → 立即拒绝
    bridge.detach_ws_tx("sess-1", TEST_CONN_ID).await;
    let result = tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("must deny promptly on disconnect")
        .unwrap();
    assert_eq!(result, ApprovalResult::Denied);
}

#[tokio::test]
async fn test_ws_conn_watch_approves_when_connected() {
    // 连接保持时 conn_watch 值不变：审批正常完成（Approved），不被误拒。
    let db = Database::new(":memory:").await.unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db);

    let (ws_tx, _ws_rx) = mpsc::channel::<serde_json::Value>(16);
    let mut agent = spawned_agent();
    agent.ws_tx = Some(ws_tx);
    agent.ws_conn_id = TEST_CONN_ID;
    agent.ws_conn_watch.send_replace(TEST_CONN_ID);
    let conn_rx = agent.ws_conn_watch.subscribe();
    bridge.sessions.lock().await.insert("sess-1".into(), agent);

    let approval: Arc<ApproveFn> =
        Arc::new(|_, _, _, _, _, _| Box::pin(async { ApprovalResult::Approved }));
    let result = approve_or_disconnect(
        approval,
        "sess-1".into(),
        "shell".into(),
        "ls".into(),
        vec![],
        mpsc::channel::<serde_json::Value>(1).0,
        TEST_CONN_ID,
        conn_rx,
    )
    .await;
    assert_eq!(result, ApprovalResult::Approved);
}

// ── drop_client_sessions：客户端控制连接断开清理 ──────────────

#[tokio::test]
async fn test_drop_client_sessions_cleans_sessions_for_client() {
    // 两个 nas 会话 + 一个其他客户端会话：drop 后仅 nas 的两条被移除。
    let db = Database::new(":memory:").await.unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db);

    let mut nas1 = spawned_agent();
    nas1.client_id = "nas".into();
    let mut nas2 = spawned_agent();
    nas2.client_id = "nas".into();
    let mut other = spawned_agent();
    other.client_id = "other".into();
    {
        let mut sessions = bridge.sessions.lock().await;
        sessions.insert("sess-nas-1".into(), nas1);
        sessions.insert("sess-nas-2".into(), nas2);
        sessions.insert("sess-other".into(), other);
    }
    bridge.drop_client_sessions("nas").await;
    let sessions = bridge.sessions.lock().await;
    assert!(!sessions.contains_key("sess-nas-1"));
    assert!(!sessions.contains_key("sess-nas-2"));
    assert!(sessions.contains_key("sess-other"));
}

#[tokio::test]
async fn test_drop_client_sessions_flushes_buffers() {
    // 会话有缓冲 turn_segments（断线瞬间未到终态）：drop 时先 flush 落库，
    // 刷新历史仍可追溯（思考先行、正文随后）。
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
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db.clone());

    let mut agent = spawned_agent();
    agent.client_id = "nas".into();
    agent.turn_segments = vec![
        TurnSegment {
            thought: true,
            content: "先思考".into(),
            parent_tool_call_id: None,
        },
        TurnSegment {
            thought: false,
            content: "再回复".into(),
            parent_tool_call_id: None,
        },
    ];
    bridge.sessions.lock().await.insert("sess-1".into(), agent);

    bridge.drop_client_sessions("nas").await;

    let rows = db.agent_list_messages("sess-1").await.unwrap();
    let thoughts: Vec<_> = rows
        .iter()
        .filter(|r| r.name.as_deref() == Some("thought"))
        .collect();
    let texts: Vec<_> = rows
        .iter()
        .filter(|r| r.kind == "message" && r.name.is_none())
        .collect();
    assert_eq!(thoughts.len(), 1);
    assert_eq!(thoughts[0].content, "先思考");
    assert_eq!(texts.len(), 1);
    assert_eq!(texts[0].content, "再回复");
}

#[tokio::test]
async fn test_drop_client_sessions_unknown_client_noop() {
    // 不存在的 client：不 panic、不动其它会话条目。
    let db = Database::new(":memory:").await.unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db);
    bridge
        .sessions
        .lock()
        .await
        .insert("sess-1".into(), spawned_agent());
    bridge.drop_client_sessions("no-such-client").await;
    assert!(bridge.sessions.lock().await.contains_key("sess-1"));
}

// ── workspace 级 config overrides 注入 ──────────────────────
