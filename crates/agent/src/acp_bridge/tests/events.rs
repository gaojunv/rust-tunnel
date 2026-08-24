use super::super::*;
use super::helpers::*;

#[tokio::test]
async fn test_acp_handshake_prompt_streams_events() {
    // 端到端：duplex → pump → mock agent（JSON-RPC）→ 事件流经 ws_tx。
    let db = Database::new(":memory:").await.unwrap();
    db.save_server_auth("secret").await.unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let (tx, _rx) = mpsc::channel::<ControlMessage>(32);
    registry.register("nas", None, tx).await;
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db);

    let (ws_tx, mut ws_rx) = mpsc::channel::<serde_json::Value>(16);
    setup_handshake(&bridge, ws_tx.clone()).await;
    // 写回连接句柄与 ACP session id
    {
        let s = bridge.sessions.lock().await;
        let agent = s.get("sess-1").unwrap();
        assert!(agent.connection.is_some(), "connection should be stored");
        assert_eq!(agent.acp_session_id.as_ref().unwrap().0.as_ref(), "acp-1");
    }

    // prompt：异步回合，事件流经 ws_tx
    bridge
        .prompt("sess-1", "hello")
        .await
        .expect("prompt should send");

    // 事件序列：assistant_chunk → tool_call → tool_result(名从缓存补)
    // → assistant_chunk(thought) → plan → done
    let mut events = Vec::new();
    for _ in 0..6 {
        let ev = tokio::time::timeout(std::time::Duration::from_secs(5), ws_rx.recv())
            .await
            .expect("timed out waiting for ws event")
            .expect("ws channel closed");
        events.push(ev);
    }
    assert_eq!(events[0]["type"], "assistant_chunk");
    assert_eq!(events[0]["content"], "hello from mock");
    assert_eq!(events[1]["type"], "tool_call");
    assert_eq!(events[1]["name"], "shell");
    assert_eq!(events[2]["type"], "tool_result");
    assert_eq!(
        events[2]["name"], "shell",
        "name should be cached from ToolCall"
    );
    assert_eq!(events[2]["result"], "a.rs");
    assert_eq!(events[3]["type"], "assistant_chunk");
    assert_eq!(events[3]["thought"], true);
    assert_eq!(events[3]["content"], "思考一下");
    assert_eq!(events[4]["type"], "plan");
    assert_eq!(events[4]["entries"][0]["content"], "步骤一");
    assert_eq!(events[5]["type"], "done");
    // 回合耗时：done 帧携带 duration_ms（回合开始于 prompt 置 busy 时）
    assert!(
        events[5]["duration_ms"].as_u64().is_some(),
        "done 帧应携带 duration_ms: {}",
        events[5]
    );
    // 回合结束：busy 复位，可再次 prompt
    assert!(!bridge.sessions.lock().await.get("sess-1").unwrap().busy);
}

#[tokio::test]
async fn test_multitab_broadcasts_frames_and_detach_stops_old_tab() {
    // 回归（H5）：多标签页/多窗口共用同一 ACP 进程。旧实现「最新连接获胜」——
    // ensure_session 把流式帧切到最新连接，回合进行中被动打开的第二个标签页把
    // 正在运行回合的帧/done 全劫走，原标签页永久卡 running。修复后流式/终态帧
    // fan-out 到全部连接；已 detach 的旧标签页不再收到后续帧。
    let db = Database::new(":memory:").await.unwrap();
    db.save_server_auth("secret").await.unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let (tx, _rx) = mpsc::channel::<ControlMessage>(32);
    registry.register("nas", None, tx).await;
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db);

    // 标签页 A：handshake 建立常驻连接任务（setup 已把 A 登记进 ws_conns）。
    let (ws_tx_a, mut ws_rx_a) = mpsc::channel::<serde_json::Value>(16);
    setup_handshake(&bridge, ws_tx_a.clone()).await;

    // 标签页 B：重连 → ensure_session dedup 把 B 追加进广播列表（独立 conn_id）。
    let (ws_tx_b, mut ws_rx_b) = mpsc::channel::<serde_json::Value>(16);
    bridge
        .ensure_session("sess-1", &acp_workspace(), ws_tx_b, TEST_CONN_ID + 1)
        .await
        .expect("reconnect dedup should register second connection");

    bridge
        .prompt("sess-1", "hello")
        .await
        .expect("prompt should send");

    // 回合帧（assistant_chunk / tool_call / tool_result / thought / plan / done）
    // 应**同时**到达 A 与 B——广播而非「最新连接获胜」劫持。
    for expected in [
        "assistant_chunk",
        "tool_call",
        "tool_result",
        "assistant_chunk",
        "plan",
        "done",
    ] {
        let ev_a = tokio::time::timeout(std::time::Duration::from_secs(5), ws_rx_a.recv())
            .await
            .expect("timed out waiting for ws event on tab A")
            .expect("ws channel closed");
        assert_eq!(ev_a["type"], expected, "event on tab A: {ev_a}");
        let ev_b = tokio::time::timeout(std::time::Duration::from_secs(5), ws_rx_b.recv())
            .await
            .expect("timed out waiting for ws event on tab B")
            .expect("ws channel closed");
        assert_eq!(ev_b["type"], expected, "event on tab B: {ev_b}");
    }

    // A 关闭：detach 只移除 A（且把主通道顺延到 B），B 继续收到后续回合帧。
    bridge.detach_ws_tx("sess-1", TEST_CONN_ID).await;

    bridge
        .prompt("sess-1", "again")
        .await
        .expect("prompt should send");

    for expected in [
        "assistant_chunk",
        "tool_call",
        "tool_result",
        "assistant_chunk",
        "plan",
        "done",
    ] {
        let ev_b = tokio::time::timeout(std::time::Duration::from_secs(5), ws_rx_b.recv())
            .await
            .expect("timed out waiting for ws event on tab B after A detached")
            .expect("ws channel closed");
        assert_eq!(
            ev_b["type"], expected,
            "event on tab B after A detached: {ev_b}"
        );
    }
    // A 已从广播列表移除：后续帧不再到达（只有 Ok(Some) 才是泄漏）。
    let stale = tokio::time::timeout(std::time::Duration::from_millis(200), ws_rx_a.recv()).await;
    assert!(
        !matches!(stale, Ok(Some(_))),
        "detached tab A should receive nothing: {stale:?}"
    );
}

#[tokio::test]
async fn test_cancel_suppresses_terminal_frame() {
    // 回归（评审 Finding 4）：取消/杀进程后 PromptResponse 才到达时，
    // on_receiving_result 不应再发 done/error 终态帧（stopped 帧已由 WS
    // handler 回发；kill 后回调以 Err 到达，不抑制会再补一条误导性 error）。
    let db = Database::new(":memory:").await.unwrap();
    db.save_server_auth("secret").await.unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let (tx, _rx) = mpsc::channel::<ControlMessage>(32);
    registry.register("nas", None, tx).await;
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db);

    let (ws_tx, mut ws_rx) = mpsc::channel::<serde_json::Value>(16);
    setup_handshake(&bridge, ws_tx.clone()).await;

    // 模拟 cancel 已执行：prompt 会分配 turn_generation=1，cancel 把该
    // 代数记入 cancelled_turns（真实路径由 cancel() 在 busy 时插入当前代数）。
    // 这里直接预置，等价于 cancel 发生在 prompt 之后但终态回调之前。
    bridge
        .sessions
        .lock()
        .await
        .get_mut("sess-1")
        .unwrap()
        .cancelled_turns
        .insert(1);

    bridge
        .prompt("sess-1", "hello")
        .await
        .expect("prompt should send");

    // 流式通知不受取消抑制（mock agent 仍在回话：assistant_chunk / tool_call /
    // tool_result / thought / plan）；终态 done 应被抑制。
    for expected in [
        "assistant_chunk",
        "tool_call",
        "tool_result",
        "assistant_chunk",
        "plan",
    ] {
        let ev = tokio::time::timeout(std::time::Duration::from_secs(5), ws_rx.recv())
            .await
            .expect("timed out waiting for streamed event")
            .expect("ws channel closed");
        assert_eq!(ev["type"], expected, "event: {ev}");
    }
    let terminal = tokio::time::timeout(std::time::Duration::from_millis(300), ws_rx.recv()).await;
    assert!(
        terminal.is_err(),
        "cancelled turn must not emit a terminal frame"
    );
    // busy 已复位（回合状态不被卡死），且取消标记已消费（供下一回合从干净态开始）
    {
        let s = bridge.sessions.lock().await;
        assert!(!s.get("sess-1").unwrap().busy);
        assert!(s.get("sess-1").unwrap().cancelled_turns.is_empty());
    }
}

#[tokio::test]
async fn test_process_crash_sends_error_frame() {
    // 回归（H1）：进程崩溃（exited 置位、非用户取消）后 PromptResponse 到达
    // 时，终态回调必须发 error 帧上报前端——否则前端 running 永久卡死。
    // 与 test_cancel_suppresses_terminal_frame 相对：取消抑制终态帧，崩溃上报。
    let db = Database::new(":memory:").await.unwrap();
    db.save_server_auth("secret").await.unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let (tx, _rx) = mpsc::channel::<ControlMessage>(32);
    registry.register("nas", None, tx).await;
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db);

    let (ws_tx, mut ws_rx) = mpsc::channel::<serde_json::Value>(16);
    // prompt_permits 控制 mock 何时回 PromptResponse：允许我们在回调触发前
    // 置 exited=true，精确模拟「进程在回合中途崩溃」。
    let (permits_tx, permits_rx) = mpsc::channel::<()>(1);
    setup_handshake_with(
        &bridge,
        ws_tx.clone(),
        serde_json::json!([]),
        Arc::new(Mutex::new(Vec::new())),
        Some(permits_rx),
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

    // 消费流式通知，确保 mock 已进入等待许可（回合 busy、PromptResponse 未回）。
    for expected in [
        "assistant_chunk",
        "tool_call",
        "tool_result",
        "assistant_chunk",
        "plan",
    ] {
        let ev = tokio::time::timeout(std::time::Duration::from_secs(5), ws_rx.recv())
            .await
            .expect("timed out waiting for streamed event")
            .expect("ws channel closed");
        assert_eq!(ev["type"], expected, "event: {ev}");
    }

    // 模拟进程崩溃：直接置 exited=true（等价 handle_spawn_exit 的语义——进程
    // 退出后 ACP 连接关闭，PromptResponse 回调以 Err 触发，`alive` 变 false）。
    bridge
        .sessions
        .lock()
        .await
        .get_mut("sess-1")
        .unwrap()
        .exited = true;

    // 释放许可，mock 回 PromptResponse → 终态回调触发。
    permits_tx.send(()).await.expect("permit send");

    // 崩溃（非取消）必须发 error 帧，前端据此解除 running。
    let err = tokio::time::timeout(std::time::Duration::from_secs(5), ws_rx.recv())
        .await
        .expect("timed out waiting for error frame")
        .expect("ws channel closed");
    assert_eq!(err["type"], "error", "crash must emit error frame: {err}");
    assert!(
        err["message"].as_str().unwrap_or("").contains("进程已退出"),
        "error message should mention process exit: {err}"
    );
    // busy 已复位（回合状态不被卡死）。
    assert!(!bridge.sessions.lock().await.get("sess-1").unwrap().busy);
}

#[tokio::test]
async fn test_cancel_then_immediate_new_prompt_not_suppressed() {
    // 回归（P0-5）：cancel 后立即重发 prompt，新回合的终态回调不得被旧回合
    // 的取消标记误吞（单布尔时代会错误抑制新回合的 done 帧）。
    let db = Database::new(":memory:").await.unwrap();
    db.save_server_auth("secret").await.unwrap();
    let registry = crate::test_helpers::TestRegistry::new(&db);
    let (tx, _rx) = mpsc::channel::<ControlMessage>(32);
    registry.register("nas", None, tx).await;
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db);

    let (ws_tx, mut ws_rx) = mpsc::channel::<serde_json::Value>(16);
    setup_handshake(&bridge, ws_tx.clone()).await;

    // 模拟旧回合（turn_generation=1）已跑完且被取消：预置计数器到 1，
    // 并把代数 1 记入 cancelled_turns。接下来的 prompt 会分配代数 2，
    // 其终态回调不应被代数 1 的取消标记抑制。
    {
        let mut sessions = bridge.sessions.lock().await;
        let agent = sessions.get_mut("sess-1").unwrap();
        agent.turn_generation = 1;
        agent.cancelled_turns.insert(1);
    }

    bridge
        .prompt("sess-1", "hello")
        .await
        .expect("prompt should send");

    // 新回合应正常收到 done 帧（不被旧回合的取消标记抑制）
    let mut got_done = false;
    let mut events = Vec::new();
    for _ in 0..10 {
        match tokio::time::timeout(std::time::Duration::from_secs(5), ws_rx.recv()).await {
            Ok(Some(ev)) if ev["type"] == "done" => {
                got_done = true;
                break;
            }
            Ok(Some(ev)) => {
                events.push(ev);
                continue;
            } // 流式事件，继续等终态
            Ok(None) => panic!("ws channel closed unexpectedly, events so far: {events:?}"),
            Err(_) => panic!("timed out waiting for done frame, events so far: {events:?}"),
        }
    }
    assert!(
        got_done,
        "new turn must emit done frame, not suppressed by old cancel. events: {events:?}"
    );
    // busy 已复位，旧取消标记仍残留（未被本回合消费）
    {
        let s = bridge.sessions.lock().await;
        assert!(!s.get("sess-1").unwrap().busy);
        assert!(s.get("sess-1").unwrap().cancelled_turns.contains(&1));
    }
}

#[tokio::test]
async fn test_acp_events_persisted_to_db() {
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
    let (tx, _rx) = mpsc::channel::<ControlMessage>(32);
    registry.register("nas", None, tx).await;
    let bridge = AcpBridge::new(AgentSpawner::new(std::sync::Arc::new(registry)), db.clone());

    let (ws_tx, mut ws_rx) = mpsc::channel::<serde_json::Value>(16);
    setup_handshake(&bridge, ws_tx.clone()).await;
    bridge.prompt("sess-1", "hello").await.expect("prompt");
    // 收完终态帧：此时终态回调的落库已完成（done 帧在落库之后发送）
    loop {
        let ev = tokio::time::timeout(std::time::Duration::from_secs(5), ws_rx.recv())
            .await
            .expect("timed out")
            .expect("closed");
        if ev["type"] == "done" {
            break;
        }
    }

    let rows = db.agent_list_messages("sess-1").await.unwrap();
    // assistant 文本：在 plan/tool 边界 flush 成一行（不再攒到终态），
    // 保证 DB rowid 顺序 = 对话顺序——文本行必须排在 tool_calls 行之前，
    // 刷新后历史里正文才出现在其调用的工具之前。
    let texts: Vec<_> = rows
        .iter()
        .filter(|r| r.kind == "message" && r.name.is_none())
        .collect();
    assert_eq!(texts.len(), 1);
    assert_eq!(texts[0].content, "hello from mock");
    let first_text = rows
        .iter()
        .position(|r| r.kind == "message" && r.name.is_none())
        .unwrap();
    let first_call = rows.iter().position(|r| r.kind == "tool_calls").unwrap();
    assert!(
        first_text < first_call,
        "text row should precede tool_calls row (boundary flush): {rows:?}"
    );
    // thought 行
    let thoughts: Vec<_> = rows
        .iter()
        .filter(|r| r.name.as_deref() == Some("thought"))
        .collect();
    assert_eq!(thoughts.len(), 1);
    assert_eq!(thoughts[0].content, "思考一下");
    // plan 行（entries JSON）
    let plans: Vec<_> = rows
        .iter()
        .filter(|r| r.name.as_deref() == Some("plan"))
        .collect();
    assert_eq!(plans.len(), 1);
    assert!(plans[0].content.contains("步骤一"));
    // tool_calls 行：tool_calls JSON 含 tool_kind
    let calls: Vec<_> = rows.iter().filter(|r| r.kind == "tool_calls").collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].tool_call_id.as_deref(), Some("call_1"));
    let call_json: serde_json::Value =
        serde_json::from_str(calls[0].tool_calls.as_deref().unwrap()).unwrap();
    assert_eq!(call_json[0]["tool_kind"], "other"); // mock 未带 kind → 默认
    assert_eq!(call_json[0]["arguments"], "{\"cmd\":\"ls\"}");
    // tool_result 行：M2 起 content 为结构化 JSON（text/status），status 落库
    // 供前端区分失败/成功。
    let results: Vec<_> = rows.iter().filter(|r| r.kind == "tool_result").collect();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].tool_call_id.as_deref(), Some("call_1"));
    let result_json: serde_json::Value = serde_json::from_str(&results[0].content).unwrap();
    assert_eq!(result_json["text"], "a.rs");
    assert_eq!(result_json["status"], "completed");
}

#[tokio::test]
async fn test_flush_preserves_thought_before_text() {
    // 回归：思考→回复 的回合（无工具边界，done 一次性 flush）落库顺序必须
    // 保持 thought 在正文之前。旧实现 text_buf/thought_buf 独立缓冲 + flush
    // 硬编码先正文后思考，DB rowid 反了 → 刷新后思考卡与回复顺序颠倒。
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
    let sessions = Arc::new(Mutex::new(HashMap::new()));
    sessions
        .lock()
        .await
        .insert("sess-1".into(), spawned_agent());

    // thought chunk 先到达、正文后到达（真实 ACP 回合顺序），done 时才 flush
    for (content, thought) in [("先思考", true), ("再回复", false)] {
        persist_acp_frame(
            &db,
            &sessions,
            "sess-1",
            &serde_json::json!({
                "type": "assistant_chunk",
                "content": content,
                "thought": thought,
            }),
        )
        .await;
    }
    flush_acp_turn_buffers(&db, &sessions, "sess-1").await;

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
    // rowid 顺序 = 对话顺序：思考行必须先于正文行
    let thought_pos = rows
        .iter()
        .position(|r| r.name.as_deref() == Some("thought"))
        .unwrap();
    let text_pos = rows
        .iter()
        .position(|r| r.kind == "message" && r.name.is_none())
        .unwrap();
    assert!(
        thought_pos < text_pos,
        "thought must precede text: {rows:?}"
    );
}

#[tokio::test]
async fn test_flush_preserves_interleaved_thought_text() {
    // 交错变体：正文先出、再思考、再正文（无工具边界）。每段独立落行，
    // 顺序严格按到达顺序保持，不能按类型归并重排。
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
    db.agent_create_session("sess-2", "w1", None, Some("gpt-4o"))
        .await
        .unwrap();
    let sessions = Arc::new(Mutex::new(HashMap::new()));
    sessions
        .lock()
        .await
        .insert("sess-2".into(), spawned_agent());

    for (content, thought) in [("正文一", false), ("思考", true), ("正文二", false)] {
        persist_acp_frame(
            &db,
            &sessions,
            "sess-2",
            &serde_json::json!({
                "type": "assistant_chunk",
                "content": content,
                "thought": thought,
            }),
        )
        .await;
    }
    flush_acp_turn_buffers(&db, &sessions, "sess-2").await;

    let rows = db.agent_list_messages("sess-2").await.unwrap();
    let kinds: Vec<(bool, String)> = rows
        .iter()
        .filter(|r| r.kind == "message")
        .map(|r| (r.name.as_deref() == Some("thought"), r.content.clone()))
        .collect();
    assert_eq!(
        kinds,
        vec![
            (false, "正文一".to_string()),
            (true, "思考".to_string()),
            (false, "正文二".to_string()),
        ],
        "rows must keep arrival order: {rows:?}"
    );
}

#[tokio::test]
async fn test_turn_segments_split_by_parent() {
    // 主 agent 文本与子 agent 文本交错到达：同一缓冲段内混入不同 parent 的
    // chunk 必须在 parent 变化处切分 segment，保证每行消息的父归属正确。
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
    let sessions = Arc::new(Mutex::new(HashMap::new()));
    sessions
        .lock()
        .await
        .insert("sess-1".into(), spawned_agent());

    // 主 agent 开场 → 子 agent（task_1）文本 → 回到主 agent
    persist_acp_frame(
        &db,
        &sessions,
        "sess-1",
        &serde_json::json!({"type": "assistant_chunk", "content": "主开场"}),
    )
    .await;
    persist_acp_frame(
        &db,
        &sessions,
        "sess-1",
        &serde_json::json!({
            "type": "assistant_chunk",
            "content": "子文本",
            "parent_tool_call_id": "task_1",
        }),
    )
    .await;
    // 同 parent 的后续 chunk 应合并进子 agent 段
    persist_acp_frame(
        &db,
        &sessions,
        "sess-1",
        &serde_json::json!({
            "type": "assistant_chunk",
            "content": "续",
            "parent_tool_call_id": "task_1",
        }),
    )
    .await;
    persist_acp_frame(
        &db,
        &sessions,
        "sess-1",
        &serde_json::json!({"type": "assistant_chunk", "content": "主收尾"}),
    )
    .await;
    flush_acp_turn_buffers(&db, &sessions, "sess-1").await;

    let rows = db.agent_list_messages("sess-1").await.unwrap();
    let texts: Vec<(String, Option<String>)> = rows
        .iter()
        .filter(|r| r.kind == "message")
        .map(|r| (r.content.clone(), r.parent_tool_call_id.clone()))
        .collect();
    assert_eq!(
        texts,
        vec![
            ("主开场".to_string(), None),
            // 同 parent 相邻 chunk 合并成一段，归属正确
            ("子文本续".to_string(), Some("task_1".to_string())),
            ("主收尾".to_string(), None),
        ],
        "segments must split at parent change: {rows:?}"
    );
}

// ── M2：tool_result 结构化 content 落库 ──────────────────────
