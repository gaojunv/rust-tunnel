//! Skill 库（Chat Memory 二期）端到端集成测试（rag feature 门控）。
//!
//! 覆盖 2 个用例：
//!   1. Skill CRUD e2e：settings 开 skill_enabled → POST /skills 手动建 →
//!      GET 列表（不含 content）→ GET :id（含 content）→ toggle enabled →
//!      PUT 编辑（含 scope 变更）→ DELETE 删除 → 列表为空；校验错误 400
//!   2. 归档蒸馏 e2e（**不配置 embedding**）：skill 不走向量，skill_enabled=1
//!      即可蒸馏——mock LLM 返回 facts+skills，归档后轮询 GET /api/agent/skills
//!      出现蒸馏条目（source_trigger=archive）、distilled=1、LLM 只调一次，
//!      且 facts 因 emb 未配置被跳过（/api/agent/memories 为空）
//!
//! mock 与 harness 用法照抄 agent_memory.rs：mock LLM 上游
//! （POST /v1/chat/completions 返回固定 JSON + hit_count），会话/工作区/消息
//! 用 DB 直接 seed，API 走真实 HTTP。skill 不写向量 shard，无需 clear 清理。

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

/// 起一个 mock LLM 上游（`POST /v1/chat/completions`），蒸馏调用返回固定
/// facts+skills JSON。返回 `(base_url, hit_count)`。
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
                    "id": "cmpl-skill-distill",
                    "object": "chat.completion",
                    "model": "distill-model",
                    "choices": [{
                        "index": 0,
                        "message": {
                            "role": "assistant",
                            "content": r##"{"facts":[{"content":"用户偏好简洁代码","scope":"workspace","tags":["rust"],"confidence":0.9}],"skills":[{"name":"发布前检查清单","description":"发布新版本前的核对步骤，涉及版本号、changelog、构建验证","content":"# 发布前检查\n\n1. 更新版本号\n2. 更新 CHANGELOG\n3. cargo build --release 验证\n4. 打 tag 并推送","scope":"workspace","tags":["release"]}]}"##,
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

/// 注册 provider + model 到 LLM 网关（base_url 直指 mock 上游）。
async fn register_llm_model(api: &ApiClient, base_url: &str, model_name: &str) {
    let (status, body) = api
        .post_json(
            "/api/llm/providers",
            json!({
                "name": "skill-e2e-mock",
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
}

/// 开启 skill 开关（不配 embedding——skill 蒸馏/注入均不依赖向量）。
async fn enable_skills(api: &ApiClient) {
    let (status, body) = api
        .put_json(
            "/api/agent/memory/settings",
            json!({ "skill_enabled": true, "skill_list_max": 20 }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "enable skills: {body}");
    assert_eq!(body["skill_enabled"], json!(true));
}

/// seed workspace + session + 5 条消息（≥ MIN_DISTILL_MESSAGES=4），返回 session id。
async fn seed_session(harness: &TestHarness, model: Option<&str>) -> String {
    let db = harness.server_state.db().expect("harness db");
    db.agent_create_workspace(
        "ws-skill-e2e",
        "skill-e2e",
        "skill-e2e-client",
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
    db.agent_create_session("sess-skill-e2e", "ws-skill-e2e", None, model)
        .await
        .expect("create session");
    for i in 0..5 {
        let role = if i % 2 == 0 { "user" } else { "assistant" };
        db.agent_add_message(
            &format!("m-skill-{i}"),
            "sess-skill-e2e",
            role,
            &format!("消息 {i} 内容，含发布流程讨论"),
            None,
        )
        .await
        .expect("add message");
    }
    "sess-skill-e2e".to_string()
}

// ── 用例 1：Skill CRUD ───────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn skill_crud_roundtrip() {
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        let harness = TestHarness::spawn(HarnessOpts::default()).await;
        let api = harness.api_client();

        // 默认设置：skill_enabled=false、skill_list_max=20
        let (status, body) = api.get_json("/api/agent/memory/settings").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["skill_enabled"], json!(false));
        assert_eq!(body["skill_list_max"], json!(20));

        enable_skills(&api).await;

        // 校验错误：空 name → 400
        let (status, _) = api
            .post_json(
                "/api/agent/skills",
                json!({ "name": "  ", "content": "步骤", "scope_type": "global" }),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "empty name rejected");

        // 校验错误：空 content → 400
        let (status, _) = api
            .post_json(
                "/api/agent/skills",
                json!({ "name": "x", "content": "", "scope_type": "global" }),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "empty content rejected");

        // 手动建 skill
        let (status, body) = api
            .post_json(
                "/api/agent/skills",
                json!({
                    "name": "排查数据库连接失败",
                    "description": "数据库连接超时/拒绝时的排查步骤",
                    "content": "# 排查步骤\n\n1. 确认网络连通\n2. 检查凭据\n3. 查看服务端日志",
                    "scope_type": "workspace",
                    "client_id": "c1",
                    "workspace_id": "w1",
                    "tags": ["db", "troubleshooting"],
                }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "create: {body}");
        assert_eq!(body["name"], json!("排查数据库连接失败"));
        assert_eq!(body["scope_type"], json!("workspace"));
        assert_eq!(body["source_trigger"], json!("manual"));
        assert_eq!(body["enabled"], json!(true));
        assert_eq!(body["use_count"], json!(0));
        assert!(body["content"].as_str().unwrap().contains("排查步骤"));
        let id = body["id"].as_str().expect("skill id").to_string();

        // 列表断言 {skills, total}；**列表项不含 content**
        let (status, body) = api.get_json("/api/agent/skills").await;
        assert_eq!(status, StatusCode::OK);
        let arr = body["skills"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(body["total"], json!(1));
        assert!(
            arr[0].get("content").is_none(),
            "list 不应含 content 全文: {}",
            arr[0]
        );

        // 详情含 content
        let (status, body) = api.get_json(&format!("/api/agent/skills/{id}")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["content"].as_str().unwrap().contains("检查凭据"));

        // 作用域过滤：workspace_id=w1 命中、w2 不命中
        let (_, body) = api
            .get_json("/api/agent/skills?scope=workspace&workspace_id=w1")
            .await;
        assert_eq!(body["total"], json!(1));
        let (_, body) = api
            .get_json("/api/agent/skills?scope=workspace&workspace_id=w2")
            .await;
        assert_eq!(body["total"], json!(0));

        // toggle → enabled=false；enabled=true 过滤后不命中
        let (status, _) = api
            .post_json(&format!("/api/agent/skills/{id}/toggle"), json!({}))
            .await;
        assert_eq!(status, StatusCode::OK);
        let (_, body) = api.get_json(&format!("/api/agent/skills/{id}")).await;
        assert_eq!(body["enabled"], json!(false));
        let (_, body) = api.get_json("/api/agent/skills?enabled=true").await;
        assert_eq!(body["total"], json!(0));

        // PUT 编辑 name/description/tags/scope（scope 变更落库）
        let (status, body) = api
            .put_json(
                &format!("/api/agent/skills/{id}"),
                json!({
                    "name": "排查数据库连接失败（v2）",
                    "description": "更新后的触发边界描述",
                    "tags": ["db"],
                    "scope_type": "global",
                }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "update: {body}");
        assert_eq!(body["name"], json!("排查数据库连接失败（v2）"));
        assert_eq!(body["scope_type"], json!("global"), "scope 变更应落库");
        assert_eq!(body["tags"], json!(["db"]));

        // DELETE → 列表为空
        let status = api.delete_status(&format!("/api/agent/skills/{id}")).await;
        assert_eq!(status, StatusCode::OK);
        let (_, body) = api.get_json("/api/agent/skills").await;
        assert_eq!(body["skills"].as_array().unwrap().len(), 0);
        assert_eq!(body["total"], json!(0));
    })
    .await;
    result.expect("test timed out");
}

// ── 用例 2：归档蒸馏（无 embedding，skill 照常产出）───────────────

#[tokio::test(flavor = "multi_thread")]
async fn archive_distills_skills_without_embedding() {
    let result = tokio::time::timeout(Duration::from_secs(60), async {
        let (llm_url, llm_hits) = start_mock_distill_llm().await;
        let harness = TestHarness::spawn(HarnessOpts::default()).await;
        let api = harness.api_client();

        register_llm_model(&api, &llm_url, "distill-model").await;
        let sid = seed_session(&harness, Some("distill-model")).await;
        enable_skills(&api).await; // 只开 skill_enabled，不配 embedding

        // 归档 → 触发后台蒸馏（CAS 置 distilled=1 同步完成，蒸馏异步执行）
        let (status, _) = api
            .post_json(&format!("/api/agent/sessions/{sid}/archive"), json!({}))
            .await;
        assert_eq!(status, StatusCode::OK, "archive");

        // 轮询：蒸馏 skill 出现（source_session_id 对齐）
        let base = harness.api_base.clone();
        let sid_cl = sid.clone();
        let found = wait_until("distilled skill appears", move || {
            let api = ApiClient::new(base.clone());
            let sid = sid_cl.clone();
            async move {
                let (code, body) = api.get_json("/api/agent/skills").await;
                if !code.is_success() {
                    return None;
                }
                let arr = body["skills"].as_array().cloned().unwrap_or_default();
                let s = arr.iter().find(|s| s["source_session_id"] == sid)?;
                Some((
                    s["id"].as_str().unwrap_or("").to_string(),
                    s["source_trigger"].as_str().unwrap_or("").to_string(),
                    s["name"].as_str().unwrap_or("").to_string(),
                    s["scope_type"].as_str().unwrap_or("").to_string(),
                    s["tags"].clone(),
                ))
            }
        })
        .await
        .expect("distilled skill appeared");

        // 蒸馏条目来源/名称/作用域/tags 与 mock skills 一致
        assert_eq!(found.1, "archive", "蒸馏触发源应为 archive");
        assert_eq!(found.2, "发布前检查清单");
        assert_eq!(found.3, "workspace");
        assert_eq!(found.4, json!(["release"]));

        // 详情含完整 content
        let (status, body) = api
            .get_json(&format!("/api/agent/skills/{}", found.0))
            .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["content"].as_str().unwrap().contains("更新 CHANGELOG"));

        // 会话蒸馏防重标记 distilled=1
        let (status, body) = api
            .get_json("/api/agent/workspaces/ws-skill-e2e/sessions")
            .await;
        assert_eq!(status, StatusCode::OK);
        let s = body
            .as_array()
            .and_then(|a| a.iter().find(|s| s["id"] == sid))
            .expect("session in list");
        assert_eq!(s["distilled"], json!(1), "归档后 distilled 应置 1");

        // 蒸馏 LLM 恰好调用一次
        assert_eq!(llm_hits.load(Ordering::SeqCst), 1, "归档只应蒸馏一次");

        // 核心断言：emb 未配置 → facts 跳过，但 skills 已落库
        let (_, body) = api.get_json("/api/agent/memories").await;
        assert_eq!(
            body["total"],
            json!(0),
            "embedding 未配置时 facts 应跳过（skill 不受影响）"
        );
    })
    .await;
    result.expect("test timed out");
}
