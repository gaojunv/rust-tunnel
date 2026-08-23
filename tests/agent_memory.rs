//! AI 记忆体端到端集成测试（rag feature 门控）。
//!
//! 覆盖 3 个用例：
//!   1. 记忆 CRUD e2e：settings 配置（mock embedding）→ POST /memories 手动建 →
//!      GET 列表断言 → pin toggle → PUT 编辑 → DELETE 删除 → 列表为空
//!   2. 归档蒸馏 e2e：灌消息会话 → POST /api/agent/sessions/:id/archive →
//!      轮询 GET /api/agent/memories 出现蒸馏条目 + 会话 distilled=1
//!      （蒸馏 LLM 调用经 mock 上游 + mock embedding 全链路）
//!   3. 并发防重 e2e：同一会话并发两次 archive → 蒸馏 LLM 只调一次（CAS 唯一
//!      赢家）、记忆无重复
//!
//! 蒸馏 LLM 上游 mock：distill 走 `resolve_with_failover` → `call_upstream_with_body`
//! （非流式），URL 为 `{base_url}/v1/chat/completions`，返回
//! `choices[0].message.content` 里的 facts JSON。embedding mock 复用 ingest 同款
//! `POST {base}/embeddings`（全部返回同一向量，便于断言作用域/去重行为）。
//!
//! 会话/工作区/消息用 DB 直接 seed（与 agent_basic.rs 同法），归档与记忆路由走
//! 真实 HTTP。无 rag 构建（memory 路由不存在、harness 不注入运行时）由
//! `cargo check -p rust-tunnel-server --no-default-features` 验证，不写测试。

#![cfg(feature = "rag")]

#[path = "common/mod.rs"]
mod common;

use common::api_client::ApiClient;
use common::{wait_until, HarnessOpts, TestHarness};
use reqwest::StatusCode;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// 起一个返回固定维度向量的本地 embedding server，返回 `base_url`。
/// 与 rag_multiformat.rs / ingest 单测同构（`POST {base}/embeddings`）。
async fn start_mock_embedding(dim: usize) -> String {
    use axum::extract::Json as J;
    use axum::routing::post;
    use axum::Router;
    let app = Router::new().route(
        "/embeddings",
        post(move |body: J<Value>| async move {
            let n = body["input"].as_array().map_or(1, Vec::len);
            let data: Vec<_> = (0..n)
                .map(|i| {
                    json!({
                        "index": i,
                        "embedding": vec![0.1f32; dim],
                        "object": "embedding"
                    })
                })
                .collect();
            J(json!({ "object": "list", "data": data }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock embedding server");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("mock embedding serve");
    });
    format!("http://{addr}")
}

/// 起一个 mock LLM 上游（`POST /v1/chat/completions`），蒸馏调用返回固定 facts
/// JSON。返回 `(base_url, hit_count)`——hit_count 用于断言"蒸馏只跑一次"。
async fn start_mock_distill_llm() -> (String, Arc<AtomicUsize>) {
    use axum::routing::post;
    use axum::Router;
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_task = hits.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |_body: axum::extract::Json<Value>| {
            let hits = hits_task.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                axum::Json(json!({
                    "id": "cmpl-memory-distill",
                    "object": "chat.completion",
                    "model": "distill-model",
                    "choices": [{
                        "index": 0,
                        "message": {
                            "role": "assistant",
                            "content": r#"{"facts":[{"content":"用户偏好简洁代码","scope":"workspace","tags":["rust"],"confidence":0.9}]}"#,
                        },
                        "finish_reason": "stop"
                    }]
                }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock distill llm server");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("mock distill llm serve");
    });
    (format!("http://{addr}"), hits)
}

/// 启用记忆体：PUT settings（emb_base_url 指向 mock embedding，dim=8）。
async fn enable_memory(api: &ApiClient, emb_base: &str) {
    let (status, body) = api
        .put_json(
            "/api/agent/memory/settings",
            json!({
                "enabled": true,
                "emb_base_url": emb_base,
                "emb_api_key": "sk-test",
                "emb_model": "test-model",
                "emb_dimension": 8,
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "enable memory settings: {body}");
    assert_eq!(body["enabled"], json!(true));
    assert_eq!(body["has_key"], json!(true), "key 已存应回 has_key=true");
    assert_eq!(body["emb_api_key"], json!(""), "密文不回传");
}

/// 注册 provider + model 到 LLM 网关（base_url 直指 mock 上游）。返回 provider id。
async fn register_llm_model(api: &ApiClient, base_url: &str, model_name: &str) -> String {
    let (status, body) = api
        .post_json(
            "/api/llm/providers",
            json!({
                "name": "memory-e2e-mock",
                "provider_type": "deepseek",
                "base_url": base_url,
                "api_key": "sk-test",
            }),
        )
        .await;
    assert!(status.is_success(), "create provider: {status} {body}");
    let pid = body["id"].as_str().expect("provider id").to_string();
    let (status, body) = api
        .post_json(
            &format!("/api/llm/providers/{pid}/models"),
            json!({ "model_name": model_name, "alias": model_name }),
        )
        .await;
    assert!(status.is_success(), "create model: {status} {body}");
    pid
}

/// 清理：POST /api/agent/memory/clear —— 删向量 shard（EdgeShard Drop 同步 flush，
/// 目录被 harness 的 tempdir 移除后 flush 会 panic）并清空记忆表。对齐
/// rag_multiformat 的删 KB 清理约定，防止 harness 析构时任务级 panic。
async fn clear_memory(api: &ApiClient) {
    let (status, _) = api.post_json("/api/agent/memory/clear", json!({})).await;
    assert_eq!(status, StatusCode::OK, "clear memory");
}

/// seed workspace + session + 5 条消息（≥ MIN_DISTILL_MESSAGES=4），返回 session id。
async fn seed_session(harness: &TestHarness, model: Option<&str>) -> String {
    let db = harness.server_state.db().expect("harness db");
    db.agent_create_workspace(
        "ws-memory-e2e",
        "memory-e2e",
        "mem-e2e-client",
        "host",
        "/tmp",
        None,
        None,
        "",
        None,
        None,
        None,
        None,
    )
    .await
    .expect("create workspace");
    db.agent_create_session("sess-memory-e2e", "ws-memory-e2e", None, model)
        .await
        .expect("create session");
    for i in 0..5 {
        let role = if i % 2 == 0 { "user" } else { "assistant" };
        db.agent_add_message(
            &format!("m-mem-{i}"),
            "sess-memory-e2e",
            role,
            &format!("消息 {i} 内容，用于蒸馏"),
            None,
        )
        .await
        .expect("add message");
    }
    "sess-memory-e2e".to_string()
}

// ── 用例 1：记忆 CRUD ────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn memory_crud_roundtrip() {
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        let emb = start_mock_embedding(8).await;
        let harness = TestHarness::spawn(HarnessOpts::default()).await;
        let api = harness.api_client();

        // 默认设置：disabled、无 key、dim=0
        let (status, body) = api.get_json("/api/agent/memory/settings").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["enabled"], json!(false));
        assert_eq!(body["has_key"], json!(false));
        assert_eq!(body["emb_dimension"], json!(0));

        // 启用 + emb 配置
        enable_memory(&api, &emb).await;

        // 手动建记忆
        let (status, body) = api
            .post_json(
                "/api/agent/memories",
                json!({
                    "content": "用户喜欢简洁的代码",
                    "scope_type": "workspace",
                    "client_id": "c1",
                    "workspace_id": "w1",
                    "tags": ["rust", "clean"],
                }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "create: {body}");
        assert_eq!(body["content"], json!("用户喜欢简洁的代码"));
        assert_eq!(body["scope_type"], json!("workspace"));
        assert_eq!(body["source_trigger"], json!("manual"));
        assert_eq!(body["confidence"], json!(1.0));
        assert_eq!(body["pinned"], json!(false));
        assert_eq!(body["tags"], json!(["rust", "clean"]));
        let id = body["id"].as_str().expect("memory id").to_string();

        // 列表断言 {memories, total}
        let (status, body) = api.get_json("/api/agent/memories").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["memories"].as_array().unwrap().len(), 1);
        assert_eq!(body["total"], json!(1));

        // 作用域过滤：workspace_id=w1 命中、w2 不命中
        let (_, body) = api
            .get_json("/api/agent/memories?scope=workspace&workspace_id=w1")
            .await;
        assert_eq!(body["total"], json!(1));
        let (_, body) = api
            .get_json("/api/agent/memories?scope=workspace&workspace_id=w2")
            .await;
        assert_eq!(body["total"], json!(0));

        // pin toggle → pinned=true
        let (status, _) = api
            .post_json(&format!("/api/agent/memories/{id}/pin"), json!({}))
            .await;
        assert_eq!(status, StatusCode::OK);
        let (_, body) = api.get_json(&format!("/api/agent/memories/{id}")).await;
        assert_eq!(body["pinned"], json!(true));

        // PUT 编辑 content/tags/scope/confidence
        let (status, body) = api
            .put_json(
                &format!("/api/agent/memories/{id}"),
                json!({
                    "content": "用户偏好简洁实现",
                    "tags": ["rust", "clean", "tunnel"],
                    "scope_type": "global",
                    "confidence": 0.85,
                }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "update: {body}");
        assert_eq!(body["content"], json!("用户偏好简洁实现"));
        assert_eq!(body["scope_type"], json!("global"), "scope 变更应落库");
        assert_eq!(body["confidence"], json!(0.85));
        assert_eq!(body["tags"], json!(["rust", "clean", "tunnel"]));

        // DELETE → 列表为空
        let status = api.delete_status(&format!("/api/agent/memories/{id}")).await;
        assert_eq!(status, StatusCode::OK);
        let (status, body) = api.get_json("/api/agent/memories").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["memories"].as_array().unwrap().len(), 0);
        assert_eq!(body["total"], json!(0));

        // 清理向量 shard（防 EdgeShard 析构 flush 撞上已删 tempdir）
        clear_memory(&api).await;
    })
    .await;
    result.expect("test timed out");
}

// ── 用例 2：归档蒸馏 ─────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn archive_distills_session_memories() {
    let result = tokio::time::timeout(Duration::from_secs(60), async {
        let emb = start_mock_embedding(8).await;
        let (llm_url, llm_hits) = start_mock_distill_llm().await;
        let harness = TestHarness::spawn(HarnessOpts::default()).await;
        let api = harness.api_client();

        // 会话模型注册到 mock LLM 上游（蒸馏走 resolve_with_failover 命中它）
        register_llm_model(&api, &llm_url, "distill-model").await;
        let sid = seed_session(&harness, Some("distill-model")).await;
        enable_memory(&api, &emb).await;

        // 归档 → 触发后台蒸馏（CAS 置 distilled=1 同步完成，蒸馏异步执行）
        let (status, _) = api
            .post_json(&format!("/api/agent/sessions/{sid}/archive"), json!({}))
            .await;
        assert_eq!(status, StatusCode::OK, "archive");

        // 轮询：蒸馏条目出现（source_session_id 对齐）→ 取出 trigger 与 total
        let base = harness.api_base.clone();
        let sid_cl = sid.clone();
        let found = wait_until("distilled memory appears", move || {
            let api = ApiClient::new(base.clone());
            let sid = sid_cl.clone();
            async move {
                let (code, body) = api.get_json("/api/agent/memories").await;
                if !code.is_success() {
                    return None;
                }
                let arr = body["memories"].as_array().cloned().unwrap_or_default();
                let m = arr
                    .iter()
                    .find(|m| m["source_session_id"] == sid)?;
                Some((
                    m["source_trigger"].as_str().unwrap_or("").to_string(),
                    m["content"].as_str().unwrap_or("").to_string(),
                    m["scope_type"].as_str().unwrap_or("").to_string(),
                    m["tags"].clone(),
                ))
            }
        })
        .await
        .expect("distilled memory appeared");

        // 蒸馏条目来源/内容/作用域/tags 与 mock facts 一致
        assert_eq!(found.0, "archive", "蒸馏触发源应为 archive");
        assert_eq!(found.1, "用户偏好简洁代码");
        assert_eq!(found.2, "workspace");
        assert_eq!(found.3, json!(["rust"]));

        // 会话蒸馏防重标记 distilled=1（workspace 会话列表 API 透出）
        let (status, body) = api
            .get_json("/api/agent/workspaces/ws-memory-e2e/sessions")
            .await;
        assert_eq!(status, StatusCode::OK);
        let s = body
            .as_array()
            .and_then(|a| a.iter().find(|s| s["id"] == sid))
            .expect("session in list");
        assert_eq!(s["distilled"], json!(1), "归档后 distilled 应置 1");

        // 蒸馏 LLM 恰好调用一次
        assert_eq!(llm_hits.load(Ordering::SeqCst), 1, "归档只应蒸馏一次");

        clear_memory(&api).await;
    })
    .await;
    result.expect("test timed out");
}

// ── 用例 3：并发防重 ─────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_archive_distills_once() {
    let result = tokio::time::timeout(Duration::from_secs(60), async {
        let emb = start_mock_embedding(8).await;
        let (llm_url, llm_hits) = start_mock_distill_llm().await;
        let harness = TestHarness::spawn(HarnessOpts::default()).await;
        let api = harness.api_client();

        register_llm_model(&api, &llm_url, "distill-model").await;
        let sid = seed_session(&harness, Some("distill-model")).await;
        enable_memory(&api, &emb).await;

        // 并发两次 archive：CAS（distilled 0→1）保证只有唯一赢家触发蒸馏
        let base = harness.api_base.clone();
        let sid1 = sid.clone();
        let s1 = tokio::spawn(async move {
            ApiClient::new(base.clone())
                .post_json(&format!("/api/agent/sessions/{sid1}/archive"), json!({}))
                .await
        });
        let base2 = harness.api_base.clone();
        let sid2 = sid.clone();
        let s2 = tokio::spawn(async move {
            ApiClient::new(base2)
                .post_json(&format!("/api/agent/sessions/{sid2}/archive"), json!({}))
                .await
        });
        let (r1, r2) = tokio::join!(s1, s2);
        assert_eq!(r1.expect("task1").0, StatusCode::OK);
        assert_eq!(r2.expect("task2").0, StatusCode::OK);

        // 轮询蒸馏条目出现（确认蒸馏确实完成）
        let base = harness.api_base.clone();
        let sid_cl = sid.clone();
        wait_until("distilled memory appears", move || {
            let api = ApiClient::new(base.clone());
            let sid = sid_cl.clone();
            async move {
                let (code, body) = api.get_json("/api/agent/memories").await;
                if !code.is_success() {
                    return None;
                }
                let arr = body["memories"].as_array().cloned().unwrap_or_default();
                if arr.iter().any(|m| m["source_session_id"] == sid) {
                    Some(())
                } else {
                    None
                }
            }
        })
        .await
        .expect("distilled memory appeared");

        // 蒸馏 LLM 只调一次（CAS 防重核心断言）
        assert_eq!(
            llm_hits.load(Ordering::SeqCst),
            1,
            "并发归档/断线触发只应蒸馏一次（CAS 唯一赢家）"
        );

        // 记忆无重复：单条事实只落一行
        let (_, body) = api.get_json("/api/agent/memories").await;
        let arr = body["memories"].as_array().expect("memories array");
        assert_eq!(arr.len(), 1, "无重复记忆");
        assert_eq!(arr[0]["source_trigger"], json!("archive"));

        clear_memory(&api).await;
    })
    .await;
    result.expect("test timed out");
}
