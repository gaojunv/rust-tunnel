use super::super::*;

#[tokio::test]
async fn test_handle_llm_proxy_request_unregistered_session_502() {
    let db = Database::new(":memory:").await.unwrap();
    db.save_server_auth("secret").await.unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let (tx, mut rx) = mpsc::channel(32);
    registry
        .register("nas", None, tx)
        .await;
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db);

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
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db);
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

// ── MCP remember 端点 token 校验（`/mcp/<token>` 路由）────────
