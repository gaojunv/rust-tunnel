use super::super::*;
use super::helpers::*;

#[tokio::test]
async fn test_tool_result_persists_structured_json() {
    // M2 契约：中间态 running（空占位）→ 终态 completed（带 result + diffs），
    // 落库 content 为 JSON `{"text","status","diffs",...}`——status/diffs
    // 落库后刷新不丢（失败打叉、diff 展示依赖它）。
    let (db, sessions) = persist_env().await;

    // 中间态：空占位（running、无产出）
    persist_acp_frame(
        &db,
        &sessions,
        "sess-1",
        &serde_json::json!({
            "type": "tool_result",
            "id": "call_1",
            "name": "shell",
            "status": "running",
        }),
    )
    .await;
    // 终态：completed + result + diffs + locations
    persist_acp_frame(
        &db,
        &sessions,
        "sess-1",
        &serde_json::json!({
            "type": "tool_result",
            "id": "call_1",
            "name": "shell",
            "status": "completed",
            "result": "a.rs",
            "diffs": [{"old": "x", "new": "y"}],
            "locations": [{"path": "a.rs", "line": 3}],
        }),
    )
    .await;

    let rows = db.agent_list_messages("sess-1").await.unwrap();
    let results: Vec<_> = rows.iter().filter(|r| r.kind == "tool_result").collect();
    assert_eq!(results.len(), 1, "upsert 收敛为一行: {rows:?}");
    let v: serde_json::Value = serde_json::from_str(&results[0].content).unwrap();
    assert_eq!(v["text"], "a.rs");
    assert_eq!(v["status"], "completed");
    assert_eq!(v["diffs"][0]["new"], "y");
    assert_eq!(v["locations"][0]["line"], 3);
}

#[tokio::test]
async fn test_tool_result_failed_empty_text_persists() {
    // 失败工具恒显 ✓ 的根因修复：failed 终态即使 result 为空也要落库
    // （status=failed），前端据此打叉。
    let (db, sessions) = persist_env().await;

    persist_acp_frame(
        &db,
        &sessions,
        "sess-1",
        &serde_json::json!({
            "type": "tool_result",
            "id": "call_1",
            "name": "shell",
            "status": "running",
        }),
    )
    .await;
    // 失败终态：无 result 文本
    persist_acp_frame(
        &db,
        &sessions,
        "sess-1",
        &serde_json::json!({
            "type": "tool_result",
            "id": "call_1",
            "name": "shell",
            "status": "failed",
        }),
    )
    .await;

    let rows = db.agent_list_messages("sess-1").await.unwrap();
    let results: Vec<_> = rows.iter().filter(|r| r.kind == "tool_result").collect();
    assert_eq!(results.len(), 1);
    let v: serde_json::Value = serde_json::from_str(&results[0].content).unwrap();
    assert_eq!(v["text"], "", "失败终态 text 为空");
    assert_eq!(
        v["status"], "failed",
        "status 必须落库: {}",
        results[0].content
    );
}

#[tokio::test]
async fn test_tool_result_late_placeholder_does_not_overwrite() {
    // 空占位语义：已完成（JSON 落库）后迟到的中间态 running 帧不得覆盖真实结果。
    let (db, sessions) = persist_env().await;

    persist_acp_frame(
        &db,
        &sessions,
        "sess-1",
        &serde_json::json!({
            "type": "tool_result",
            "id": "call_1",
            "name": "shell",
            "status": "completed",
            "result": "真实结果",
        }),
    )
    .await;
    // 迟到的中间态帧（刷新/重连乱序）：空占位 → "" → upsert 不覆盖
    persist_acp_frame(
        &db,
        &sessions,
        "sess-1",
        &serde_json::json!({
            "type": "tool_result",
            "id": "call_1",
            "name": "shell",
            "status": "running",
        }),
    )
    .await;

    let rows = db.agent_list_messages("sess-1").await.unwrap();
    let results: Vec<_> = rows.iter().filter(|r| r.kind == "tool_result").collect();
    assert_eq!(results.len(), 1);
    let v: serde_json::Value = serde_json::from_str(&results[0].content).unwrap();
    assert_eq!(v["text"], "真实结果");
    assert_eq!(v["status"], "completed");
}

#[tokio::test]
async fn test_acp_persistence_survives_ws_disconnect() {
    // 断线（detach_ws_tx 置 ws_tx=None）期间回合跑完：消息仍落库。
    // 用真实断线路径 detach_ws_tx——区别于 drop(ws_rx) 只关接收端
    // （后者 ws_tx 仍占位，通知处理器仍能过 current_ws_tx 守卫，落库
    // 本来就发生）；detach 后条目 ws_tx=None，验证 persist 已移出 ws_tx
    // guard 之前（评审修复）：断线点之后到达的帧同样落库。
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
    let (tx, _rx) = mpsc::channel::<ControlMessage>(32);
    registry.register("nas", None, tx).await;
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db.clone());

    let (ws_tx, _ws_rx) = mpsc::channel::<serde_json::Value>(16);
    setup_handshake(&bridge, ws_tx.clone()).await;
    // 真实断线：清空条目 WS 通道（本连接自己的通道，sender 通道身份匹配）
    bridge.detach_ws_tx("sess-1", TEST_CONN_ID).await;
    bridge.prompt("sess-1", "hello").await.expect("prompt");
    // 等终态回调落库完成：轮询 DB（断线下无终态帧，只能轮询落库结果）。
    // break 条件要求 tool_result 与终态 flush 的文本行（kind='message' 且
    // name=None）都已落库，避免「tool_result 已落、终态 flush 未完成」的
    // 理论 flake 窗口。
    let mut rows = Vec::new();
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        rows = db.agent_list_messages("sess-1").await.unwrap();
        if rows.iter().any(|r| r.kind == "tool_result")
            && rows.iter().any(|r| r.kind == "message" && r.name.is_none())
        {
            break;
        }
    }
    assert!(
        rows.iter().any(|r| r.kind == "tool_calls"),
        "tool_call should persist without ws consumer: {rows:?}"
    );
    assert!(rows.iter().any(|r| r.kind == "tool_result"));
    assert!(
        rows.iter().any(|r| r.kind == "message" && r.name.is_none()),
        "assistant text should persist without ws consumer"
    );
}
