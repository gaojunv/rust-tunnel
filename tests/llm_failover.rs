//! 集成测试：LLM 网关多模型故障转移与熔断。
//!
//! 当前覆盖（Task 8）：模型组管理 API——CRUD、成员整体替换、组名冲突、
//! 熔断重置、删组后成员行清理。Task 10 将扩写故障转移端到端场景。

#[path = "common/mod.rs"]
mod common;

use common::{HarnessOpts, TestHarness};
use reqwest::StatusCode;
use serde_json::json;

/// 管理 API：模型组 CRUD + 成员替换 + 组名冲突 + 熔断重置。
#[tokio::test(flavor = "multi_thread")]
async fn model_groups_mgmt_api() {
    let harness = TestHarness::spawn(HarnessOpts {
        exposed_port_count: 1,
        ..Default::default()
    })
    .await;
    let api = harness.api_client();

    // 准备 provider + 2 模型（复用 llm_gateway 测试的 API 形态）
    let (status, body) = api
        .post_json(
            "/api/llm/providers",
            json!({
                "name": "DS", "provider_type": "deepseek",
                "base_url": "https://api.deepseek.com", "api_key": "sk-x"
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "create provider failed: {body}");
    let pid = body["id"].as_str().unwrap().to_string();

    let (status, body) = api
        .post_json(
            &format!("/api/llm/providers/{pid}/models"),
            json!({"model_name": "deepseek-chat", "alias": "chat"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "add model failed: {body}");
    let m1 = body["id"].as_str().unwrap().to_string();

    let (status, body) = api
        .post_json(
            &format!("/api/llm/providers/{pid}/models"),
            json!({"model_name": "deepseek-reasoner"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "add model failed: {body}");
    let m2 = body["id"].as_str().unwrap().to_string();

    // 创建组
    let (status, body) = api
        .post_json("/api/llm/model-groups", json!({"name": "router"}))
        .await;
    assert_eq!(status, StatusCode::CREATED, "create group failed: {body}");
    let gid = body["id"].as_str().unwrap().to_string();

    // 组名冲突（撞模型 alias "chat" → 409）
    let (status, _) = api
        .post_json("/api/llm/model-groups", json!({"name": "chat"}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "name conflict should be 409");

    // 替换成员
    let (status, _) = api
        .put_json(
            &format!("/api/llm/model-groups/{gid}/members"),
            json!({
                "members": [
                    {"model_id": m1, "priority": 1},
                    {"model_id": m2, "priority": 2}
                ]
            }),
        )
        .await;
    assert!(status.is_success(), "replace members failed: {status}");

    // 详情：成员有序 + breaker 快照为 Closed
    let (status, detail) = api.get_json(&format!("/api/llm/model-groups/{gid}")).await;
    assert!(status.is_success(), "get group detail failed: {status}");
    assert_eq!(detail["name"], "router");
    let members = detail["members"].as_array().unwrap();
    assert_eq!(members.len(), 2);
    assert_eq!(members[0]["model_name"], "deepseek-chat");
    assert_eq!(members[0]["breaker"]["state"], "closed");
    assert_eq!(members[1]["priority"], 2);

    // 列表
    let (_, list) = api.get_json("/api/llm/model-groups").await;
    let groups = list["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["member_count"], 2);

    // 改名冲突自身豁免 + 改启用
    let (status, _) = api
        .put_json(
            &format!("/api/llm/model-groups/{gid}"),
            json!({"name": "router", "enabled": false}),
        )
        .await;
    assert!(status.is_success(), "update group failed: {status}");
    let (_, detail2) = api.get_json(&format!("/api/llm/model-groups/{gid}")).await;
    assert_eq!(detail2["enabled"], false);

    // 重置熔断（无熔断也 200，reset=成员数）
    let (status, reset) = api
        .post_json(
            &format!("/api/llm/model-groups/{gid}/reset-breaker"),
            json!({}),
        )
        .await;
    assert!(status.is_success(), "reset breaker failed: {status}");
    assert_eq!(reset["status"], "ok");
    assert_eq!(reset["reset"], 2);

    // 删除：组删除后成员行需一并清理（FK 级联不生效，由 handler 显式清空）
    let status = api
        .delete_status(&format!("/api/llm/model-groups/{gid}"))
        .await;
    assert!(status.is_success(), "delete group failed: {status}");
    let db = harness.server_state.db().expect("db").clone();
    assert_eq!(
        db.llm_group_member_count(&gid).await.unwrap(),
        0,
        "deleted group must leave no orphan member rows"
    );
    let (_, list2) = api.get_json("/api/llm/model-groups").await;
    assert_eq!(list2["groups"].as_array().unwrap().len(), 0);
}
