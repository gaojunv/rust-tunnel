use super::super::*;
use super::helpers::*;

/// 握手后按 workspace.agent_config_overrides 注入；config_state 回放其后
/// （用户显式选择覆盖 workspace 默认）。快照中不存在的 config_id 跳过不报错。
#[tokio::test]
async fn test_apply_config_overrides_on_handshake() {
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
        "claude-code",
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
    // session 级 config_state：用户显式把 model 改为 opus —— 必须覆盖 workspace 注入
    db.agent_update_session_config_state("sess-1", "model", Some("opus"))
        .await
        .unwrap();

    let registry = crate::test_helpers::TestRegistry::new(&db);
    let (tx, _rx) = mpsc::channel::<ControlMessage>(32);
    registry.register("nas", None, tx).await;
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db.clone());

    let applied = Arc::new(Mutex::new(Vec::new()));
    let options = serde_json::json!([
        {"id": "model", "name": "Model", "type": "select",
         "currentValue": "sonnet",
         "options": [{"value": "sonnet", "name": "Sonnet"}, {"value": "opus", "name": "Opus"}]},
        {"id": "fast", "name": "Fast model", "type": "select",
         "currentValue": "haiku",
         "options": [{"value": "haiku", "name": "Haiku"}]}
    ]);
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
        None,
    )
    .await;

    // workspace 注入：fast → haiku；model 先被 workspace 设为 sonnet，随后
    // config_state 回放覆盖为 opus；nonexistent 不在快照中 → 跳过
    bridge
        .apply_config_overrides(
            "sess-1",
            &db.agent_get_workspace("w1").await.unwrap().unwrap(),
        )
        .await;
    bridge.replay_config_state("sess-1").await;

    let calls = applied.lock().await.clone();
    // 顺序：workspace 按 config_id 字典序（fast 先于 model）注入，回放其后
    assert_eq!(
        calls,
        vec![
            ("fast".to_string(), "haiku".to_string()),
            ("model".to_string(), "sonnet".to_string()),
            ("model".to_string(), "opus".to_string()),
        ]
    );
}

/// workspace 未配置 overrides（None / 非法 JSON / 空对象）→ 不发任何
/// set_config_option，不报错。
#[tokio::test]
async fn test_apply_config_overrides_noop_when_unset() {
    let db = Database::new(":memory:").await.unwrap();
    db.save_server_auth("secret").await.unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let (tx, _rx) = mpsc::channel::<ControlMessage>(32);
    registry.register("nas", None, tx).await;
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db);

    let options = serde_json::json!([
        {"id": "model", "name": "Model", "type": "select",
         "currentValue": "sonnet",
         "options": [{"value": "sonnet", "name": "Sonnet"}]}
    ]);

    for (label, overrides) in [
        ("none", None),
        ("not-json", Some("not-json")),
        ("empty-object", Some("{}")),
    ] {
        let ws = AgentWorkspaceRecord {
            agent_config_overrides: overrides.map(str::to_string),
            ..acp_workspace()
        };
        let applied = Arc::new(Mutex::new(Vec::new()));
        let (ws_tx, _ws_rx) = mpsc::channel::<serde_json::Value>(16);
        setup_handshake_with(
            &bridge,
            ws_tx,
            options.clone(),
            applied.clone(),
            None,
            None,
            None,
            false,
            None,
        )
        .await;
        bridge.apply_config_overrides("sess-1", &ws).await;
        assert!(
            applied.lock().await.is_empty(),
            "{label}: apply_config_overrides should be a no-op"
        );
    }
}
