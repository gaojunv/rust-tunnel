use super::super::*;
use super::helpers::*;

/// 断线重拉：持有多余的持久化 ACP session id + agent 支持 session/resume →
/// 握手走 resume（sessionId 复用、DB 落库同 id），不建全新会话。
#[tokio::test]
async fn test_handshake_resumes_persisted_session() {
    let (bridge, db) = seeded_bridge().await;
    let recorded = Arc::new(Mutex::new(Vec::<String>::new()));
    let (ws_tx, _ws_rx) = mpsc::channel::<serde_json::Value>(16);
    setup_handshake_with(
        &bridge,
        ws_tx,
        serde_json::json!([]),
        Arc::new(Mutex::new(Vec::new())),
        None,
        Some(recorded.clone()),
        Some("acp-persisted-1"),
        false,
        None,
    )
    .await;

    // mock 收到 session/resume，未走 session/new
    let methods = recorded.lock().await.clone();
    assert!(
        methods.iter().any(|m| m == "session/resume"),
        "should send session/resume, got: {methods:?}"
    );
    assert!(
        !methods.iter().any(|m| m == "session/new"),
        "resume-capable path must not send session/new: {methods:?}"
    );
    // 会话条目的 acp_session_id 复用持久化 id
    assert_eq!(
        bridge
            .sessions
            .lock()
            .await
            .get("sess-1")
            .unwrap()
            .acp_session_id
            .as_ref()
            .unwrap()
            .0
            .as_ref(),
        "acp-persisted-1"
    );
    // DB 落库同 id（下次重拉继续 resume）
    assert_eq!(
        db.agent_get_session("sess-1")
            .await
            .unwrap()
            .unwrap()
            .acp_session_id,
        Some("acp-persisted-1".into())
    );
}

/// 无持久化 id（首次会话）→ 行为不变：session/new，新 id 落库。
#[tokio::test]
async fn test_handshake_new_when_no_persisted_id() {
    let (bridge, db) = seeded_bridge().await;
    let recorded = Arc::new(Mutex::new(Vec::<String>::new()));
    let (ws_tx, _ws_rx) = mpsc::channel::<serde_json::Value>(16);
    setup_handshake_with(
        &bridge,
        ws_tx,
        serde_json::json!([]),
        Arc::new(Mutex::new(Vec::new())),
        None,
        Some(recorded.clone()),
        None,
        false,
        None,
    )
    .await;

    let methods = recorded.lock().await.clone();
    assert!(
        methods.iter().any(|m| m == "session/new"),
        "no persisted id should use session/new: {methods:?}"
    );
    assert!(!methods.iter().any(|m| m == "session/resume"));
    // 新 id（mock 固定返回 acp-1）落库
    assert_eq!(
        db.agent_get_session("sess-1")
            .await
            .unwrap()
            .unwrap()
            .acp_session_id,
        Some("acp-1".into())
    );
}

/// resume 失败（会话文件缺失/已清理）→ 回退 session/new，新 id 落库，
/// 会话建立不阻断。
#[tokio::test]
async fn test_handshake_resume_failure_falls_back_to_new() {
    let (bridge, db) = seeded_bridge().await;
    let recorded = Arc::new(Mutex::new(Vec::<String>::new()));
    let (ws_tx, _ws_rx) = mpsc::channel::<serde_json::Value>(16);
    setup_handshake_with(
        &bridge,
        ws_tx,
        serde_json::json!([]),
        Arc::new(Mutex::new(Vec::new())),
        None,
        Some(recorded.clone()),
        Some("acp-persisted-1"),
        true,
        None,
    )
    .await;

    let methods = recorded.lock().await.clone();
    let resume_pos = methods.iter().position(|m| m == "session/resume");
    let new_pos = methods.iter().position(|m| m == "session/new");
    assert!(
        resume_pos.is_some(),
        "should attempt resume first: {methods:?}"
    );
    assert!(
        new_pos.is_some(),
        "resume failure should fall back to new: {methods:?}"
    );
    assert!(
        resume_pos < new_pos,
        "resume must be attempted before new: {methods:?}"
    );
    // 回退后使用新 id（mock 固定返回 acp-1）落库
    assert_eq!(
        db.agent_get_session("sess-1")
            .await
            .unwrap()
            .unwrap()
            .acp_session_id,
        Some("acp-1".into())
    );
}

/// kill_and_delete（会话删除路径）先发 ACP session/delete（让 agent 清理其
/// 持久化会话文件），再移除会话条目。方法等待 session/delete 响应后才继续
/// → 断言时已记录。
#[tokio::test]
async fn test_kill_and_delete_sends_session_delete() {
    let (bridge, _db) = seeded_bridge().await;
    let recorded = Arc::new(Mutex::new(Vec::<String>::new()));
    let (ws_tx, _ws_rx) = mpsc::channel::<serde_json::Value>(16);
    setup_handshake_with(
        &bridge,
        ws_tx,
        serde_json::json!([]),
        Arc::new(Mutex::new(Vec::new())),
        None,
        Some(recorded.clone()),
        None,
        false,
        None,
    )
    .await;

    bridge.kill_and_delete("sess-1").await;
    assert!(
        recorded.lock().await.iter().any(|m| m == "session/delete"),
        "kill_and_delete() should send session/delete to clean up client-side session data"
    );
    assert!(
        !bridge.sessions.lock().await.contains_key("sess-1"),
        "kill_and_delete() should remove the session entry"
    );
}

/// kill()（归档路径）**不**发 session/delete：归档后重开会话仍可
/// session/resume 恢复上下文，客户端持久化会话数据必须保留。
#[tokio::test]
async fn test_kill_preserves_client_session_data() {
    let (bridge, _db) = seeded_bridge().await;
    let recorded = Arc::new(Mutex::new(Vec::<String>::new()));
    let (ws_tx, _ws_rx) = mpsc::channel::<serde_json::Value>(16);
    setup_handshake_with(
        &bridge,
        ws_tx,
        serde_json::json!([]),
        Arc::new(Mutex::new(Vec::new())),
        None,
        Some(recorded.clone()),
        None,
        false,
        None,
    )
    .await;

    bridge.kill("sess-1").await;
    assert!(
        !recorded.lock().await.iter().any(|m| m == "session/delete"),
        "kill() (archive path) must preserve client-side session data: {:?}",
        recorded.lock().await
    );
    assert!(!bridge.sessions.lock().await.contains_key("sess-1"));
}
