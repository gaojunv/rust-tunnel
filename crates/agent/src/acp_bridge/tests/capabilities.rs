use super::super::*;
use super::helpers::*;

#[test]
fn test_client_capabilities_declare_fs() {
    // fs 能力必须声明：agent 才走 fs/read_text_file 而非静默报「不支持」。
    let caps = client_capabilities();
    assert!(caps.fs.read_text_file);
    assert!(caps.fs.write_text_file);
}

#[test]
fn test_client_capabilities_declare_subagent_transcript() {
    // opt-in 约定：_meta["subagent-transcript"]=true 请求 agent 转发子 agent
    // 事件的 parentToolUseId/subagent 元数据。不支持 _meta 的 agent 忽略该键。
    let caps = client_capabilities();
    let meta = caps.meta.expect("capabilities should carry _meta");
    assert_eq!(
        meta.get("subagent-transcript")
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "_meta.subagent-transcript must be true: {meta:?}"
    );
}

#[test]
fn test_client_capabilities_declare_elicitation_form() {
    // 声明 elicitation.form：claude-code-acp 据此启用 AskUserQuestion（否则放入
    // disallowedTools 报「not enabled in this context」）。只声明 form、不声明
    // url（缺省 None → 序列化不含该键）。
    let caps = client_capabilities();
    let elicitation = caps
        .elicitation
        .as_ref()
        .expect("capabilities should carry elicitation");
    assert!(
        elicitation.form.is_some(),
        "form capability must be declared"
    );
    assert!(
        elicitation.url.is_none(),
        "url capability must not be declared"
    );
    let json = serde_json::to_value(&caps).unwrap();
    assert!(json["elicitation"]["form"].is_object());
    assert!(
        json["elicitation"].get("url").is_none(),
        "url key must be absent: {json}"
    );
}

#[tokio::test]
async fn test_elicit_or_disconnect_cancels_on_detach() {
    // 断线即时取消：表单等待期间 detach_ws_tx 写入 0 → conn_watch 值变化
    // → wait_for 唤醒 → Cancel，不等满 5 分钟表单超时（镜像审批的 detach 测试）。
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

    // elicitation 挂起（永不返回）：等待中途断线由 watch 唤醒短路取消。
    let elicitation: Arc<ElicitFn> = Arc::new(|_, _, _, _| {
        Box::pin(async {
            std::future::pending::<()>().await;
            ElicitationResult::Cancel
        })
    });
    let handle = tokio::spawn(elicit_or_disconnect(
        elicitation,
        "sess-1".into(),
        "please choose".into(),
        serde_json::json!({}),
        mpsc::channel::<serde_json::Value>(1).0,
        TEST_CONN_ID,
        conn_rx,
    ));

    // 表单在途时连接断开 → 立即取消
    bridge.detach_ws_tx("sess-1", TEST_CONN_ID).await;
    let result = tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("must cancel promptly on disconnect")
        .unwrap();
    assert_eq!(result, ElicitationResult::Cancel);
}

#[tokio::test]
async fn test_elicit_or_disconnect_accepts_when_connected() {
    // 连接保持时 conn_watch 值不变：表单正常完成（Accept + content），不被误取消。
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

    let content = serde_json::from_value::<
        std::collections::BTreeMap<
            String,
            agent_client_protocol::schema::v1::ElicitationContentValue,
        >,
    >(serde_json::json!({"name": "Alice"}))
    .unwrap();
    let elicitation: Arc<ElicitFn> = Arc::new(move |_, _, _, _| {
        let content = content.clone();
        Box::pin(async move { ElicitationResult::Accept(Some(content)) })
    });
    let result = elicit_or_disconnect(
        elicitation,
        "sess-1".into(),
        "please choose".into(),
        serde_json::json!({}),
        mpsc::channel::<serde_json::Value>(1).0,
        TEST_CONN_ID,
        conn_rx,
    )
    .await;
    match result {
        ElicitationResult::Accept(Some(content)) => {
            assert_eq!(
                content.get("name"),
                Some(
                    &agent_client_protocol::schema::v1::ElicitationContentValue::String(
                        "Alice".into()
                    )
                )
            );
        }
        other => panic!("expected Accept, got {other:?}"),
    }
}

#[test]
fn test_to_workspace_relative() {
    assert_eq!(
        to_workspace_relative("/ws", "/ws/a/b.txt").unwrap(),
        "a/b.txt"
    );
    assert_eq!(to_workspace_relative("/ws", "/ws/a.txt").unwrap(), "a.txt");
    // 工作区外 / 非绝对 / 根目录自身 → Err
    assert!(to_workspace_relative("/ws", "/etc/passwd").is_err());
    assert!(to_workspace_relative("/ws", "a/b.txt").is_err());
    assert!(to_workspace_relative("/ws", "/ws").is_err());
    // 前缀歧义：/wsx 不在 /ws 下
    assert!(to_workspace_relative("/ws", "/wsx/a").is_err());
}
