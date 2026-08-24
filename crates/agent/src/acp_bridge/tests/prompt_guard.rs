use super::super::*;
use super::helpers::*;

#[tokio::test]
async fn test_prompt_busy_guard_rejects_concurrent() {
    let db = Database::new(":memory:").await.unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db);
    let mut agent = spawned_agent();
    agent.busy = true;
    bridge.sessions.lock().await.insert("sess-1".into(), agent);
    let err = bridge
        .prompt("sess-1", "hi")
        .await
        .expect_err("busy turn should reject a concurrent prompt");
    assert!(err.contains("回合"), "err: {err}");
}

#[tokio::test]
async fn test_prompt_without_connection_errors() {
    // busy=false 但 handshake 未完成（connection 缺失）→ 报错且 busy 复位
    let db = Database::new(":memory:").await.unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db);
    bridge
        .sessions
        .lock()
        .await
        .insert("sess-1".into(), spawned_agent());
    let err = bridge
        .prompt("sess-1", "hi")
        .await
        .expect_err("no connection yet should error");
    assert!(err.contains("handshake"), "err: {err}");
    // busy 已复位，不会卡死后续 prompt
    assert!(!bridge.sessions.lock().await.get("sess-1").unwrap().busy);
}

#[tokio::test]
async fn test_cancel_unknown_session_is_noop() {
    let db = Database::new(":memory:").await.unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db);
    bridge.cancel("no-such-session").await; // 不 panic
}

#[tokio::test]
async fn test_kill_removes_entry() {
    let db = Database::new(":memory:").await.unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db);
    bridge
        .sessions
        .lock()
        .await
        .insert("sess-1".into(), spawned_agent());
    bridge.kill("sess-1").await;
    assert!(!bridge.sessions.lock().await.contains_key("sess-1"));
    bridge.kill("sess-1").await; // 幂等：不存在时静默 no-op
}

// ── ACP handshake + prompt 集成（mock JSON-RPC agent）────────
