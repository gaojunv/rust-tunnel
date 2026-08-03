//! 集成测试：LLM 网关多模型故障转移与熔断。
//!
//! 覆盖（Task 8）：模型组管理 API——CRUD、成员整体替换、组名冲突、
//! 熔断重置、删组后成员行清理。
//! 覆盖（Task 10）：故障转移 + 熔断端到端——
//! - OpenAI 非流式/流式请求转移（坏候选 500 → 好候选 200），usage 记 failover_from
//! - 全候选失败返回最后候选错误（503），usage 记 success=false 且 failover_from 有值
//! - 熔断：连续 5 次失败打开 → 请求跳过坏候选 → 手动重置恢复尝试
//! - 回归：单模型未配组时 failover_from 为空
//!
//! mock 上游按实例返回固定行为（500/503/200 JSON/200 SSE），并记录命中次数，
//! 便于断言熔断跳过等时序语义。

#[path = "common/mod.rs"]
mod common;

use common::{wait_until, HarnessOpts, TestHarness};
use reqwest::StatusCode;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TIMEOUT: Duration = Duration::from_secs(15);
const GW_DOMAIN: &str = "llm.test";
const UPSTREAM_KEY: &str = "sk-upstream-secret";

// ── Mock 上游 LLM 提供商（按实例固定行为）────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MockBehavior {
    /// 一律 500（坏上游）。
    Always500,
    /// 一律 503（坏上游）。
    Always503,
    /// 正常 200：body 含 `"stream":true` 回 SSE，否则回 JSON（好上游）。
    Ok,
}

#[derive(Debug, Clone)]
struct RecordedRequest {
    body: String,
}

struct MockUpstream {
    addr: SocketAddr,
    recorded: Arc<Mutex<Vec<RecordedRequest>>>,
    hits: Arc<AtomicUsize>,
}

impl MockUpstream {
    /// 启动一个行为固定的 mock 上游：记录每个请求体 + 命中计数。
    async fn start(behavior: MockBehavior) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let recorded: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let hits = Arc::new(AtomicUsize::new(0));
        let recorded_task = recorded.clone();
        let hits_task = hits.clone();

        tokio::spawn(async move {
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let recorded = recorded_task.clone();
                let hits = hits_task.clone();
                tokio::spawn(async move {
                    // 读 headers + body（按 Content-Length 收全）
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let header_end = loop {
                        match stream.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => {
                                buf.extend_from_slice(&chunk[..n]);
                                if let Some(pos) = find_subsequence(&buf, b"\r\n\r\n") {
                                    break pos + 4;
                                }
                            }
                        }
                    };
                    let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
                    let content_length = headers
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    while buf.len() < header_end + content_length {
                        match stream.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let body = String::from_utf8_lossy(&buf[header_end..]).to_string();
                    recorded.lock().unwrap().push(RecordedRequest { body: body.clone() });
                    hits.fetch_add(1, Ordering::SeqCst);

                    let is_stream = body.contains("\"stream\":true");
                    let response = match behavior {
                        MockBehavior::Always500 => {
                            mock_error_response("500 Internal Server Error")
                        }
                        MockBehavior::Always503 => {
                            mock_error_response("503 Service Unavailable")
                        }
                        MockBehavior::Ok => {
                            if is_stream {
                                mock_sse_response()
                            } else {
                                mock_json_response()
                            }
                        }
                    };
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.flush().await;
                });
            }
        });

        Self { addr, recorded, hits }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.recorded.lock().unwrap().clone()
    }

    fn hit_count(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// 好上游的非流式 chat.completion JSON（带 usage 以便落库）。
fn mock_json_response() -> String {
    let body = json!({
        "id": "chatcmpl-good",
        "object": "chat.completion",
        "created": 1,
        "model": "good-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "Hello from good"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8}
    });
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.to_string().len(),
        body
    )
}

/// 好上游的 SSE 流：两个 data chunk + 末尾 usage chunk + [DONE]。
fn mock_sse_response() -> String {
    let sse = concat!(
        "data: {\"id\":\"c-good\",\"model\":\"good-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c-good\",\"model\":\"good-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello from good\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c-good\",\"model\":\"good-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2,\"total_tokens\":7}}\n\n",
        "data: [DONE]\n\n",
    );
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        sse.len(),
        sse
    )
}

fn mock_error_response(status_line: &str) -> String {
    let body = "{\"error\":\"mock upstream error\"}";
    format!(
        "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

// ── 公共 setup ──────────────────────────────────────────────────

struct GatewayEnv {
    gateway_port: u16,
    gateway_key: String,
}

/// 完整搭建：gateway 配置 + 网关 API key，并等待 LLM 监听器就绪。
/// provider/model/组由各测试自建（便于组与不组两种形态复用）。
async fn setup_gateway(harness: &TestHarness, api: &common::api_client::ApiClient) -> GatewayEnv {
    let gateway_port = harness.exposed_ports[0];
    let (status, body) = api
        .put_json(
            "/api/llm/gateway",
            json!({
                "enabled": true,
                "openai_domain": GW_DOMAIN,
                "listen": format!("127.0.0.1:{gateway_port}"),
                "tls_enabled": false,
            }),
        )
        .await;
    assert!(status.is_success(), "PUT gateway failed: {status} {body}");

    let (status, body) = api
        .post_json("/api/llm/api-keys", json!({"name": "failover-e2e"}))
        .await;
    assert_eq!(status, StatusCode::CREATED, "create api key failed: {body}");
    let gateway_key = body["key"].as_str().unwrap().to_string();
    assert!(gateway_key.starts_with("sk-"));

    // 等 LLM 监听器就绪（401 = 已就绪但未认证）
    let base = format!("http://127.0.0.1:{gateway_port}");
    wait_until("llm gateway listener", || {
        let base = base.clone();
        async move {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_millis(300))
                .build()
                .ok()?;
            let resp = client
                .get(format!("{base}/v1/models"))
                .header("Host", GW_DOMAIN)
                .send()
                .await
                .ok()?;
            if resp.status() == StatusCode::UNAUTHORIZED {
                Some(())
            } else {
                None
            }
        }
    })
    .await
    .expect("llm gateway listener never came up");

    GatewayEnv {
        gateway_port,
        gateway_key,
    }
}

/// 建 provider + model，返回 (provider_id, model_id)。
async fn create_provider_model(
    api: &common::api_client::ApiClient,
    provider_name: &str,
    base_url: &str,
    model_name: &str,
    alias: &str,
) -> (String, String) {
    let (status, body) = api
        .post_json(
            "/api/llm/providers",
            json!({
                "name": provider_name,
                "provider_type": "deepseek",
                "base_url": base_url,
                "api_key": UPSTREAM_KEY,
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "create provider failed: {body}");
    let pid = body["id"].as_str().unwrap().to_string();

    let (status, body) = api
        .post_json(
            &format!("/api/llm/providers/{pid}/models"),
            json!({"model_name": model_name, "alias": alias}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "add model failed: {body}");
    let mid = body["id"].as_str().unwrap().to_string();
    (pid, mid)
}

/// 建组并整体替换成员（members = [(model_id, priority)]），返回组 id。
async fn create_group_with_members(
    api: &common::api_client::ApiClient,
    name: &str,
    members: &[(String, i32)],
) -> String {
    let (status, body) = api
        .post_json("/api/llm/model-groups", json!({"name": name}))
        .await;
    assert_eq!(status, StatusCode::CREATED, "create group failed: {body}");
    let gid = body["id"].as_str().unwrap().to_string();

    let member_json: Vec<Value> = members
        .iter()
        .map(|(mid, prio)| json!({"model_id": mid, "priority": prio}))
        .collect();
    let (status, body) = api
        .put_json(
            &format!("/api/llm/model-groups/{gid}/members"),
            json!({"members": member_json}),
        )
        .await;
    assert!(status.is_success(), "replace members failed: {status} {body}");
    gid
}

fn gateway_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}

/// 向 LLM 网关（OpenAI 入口）发一条 chat.completions 请求。
async fn chat_completion(gateway_port: u16, key: &str, body: Value) -> reqwest::Response {
    gateway_client()
        .post(format!(
            "http://127.0.0.1:{gateway_port}/v1/chat/completions"
        ))
        .header("Host", GW_DOMAIN)
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .unwrap()
}

/// 轮询取最新一条 usage log（落库是 fire-and-forget，需等待）。
async fn latest_usage_log(api: &common::api_client::ApiClient) -> Value {
    wait_until("usage log recorded", || {
        let api = &api;
        async move {
            let (status, body) = api.get_json("/api/llm/usage/logs").await;
            if !status.is_success() {
                return None;
            }
            let logs = body["logs"].as_array()?;
            if logs.is_empty() {
                return None;
            }
            Some(logs[0].clone())
        }
    })
    .await
    .expect("usage log never recorded")
}

// ── Task 8：模型组管理 API ──────────────────────────────────────

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

    // 删除：组删除后成员行需一并清理（FK 级联经 sqlx 默认的 PRAGMA foreign_keys=ON 已生效；db 层 llm_delete_model_group 亦显式清空）
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

// ── Task 10：故障转移 + 熔断端到端 ──────────────────────────────

/// e2e：组 [坏模型 500 → 好模型 200]，OpenAI 入口非流式请求转移成功，
/// 好上游收到的请求体 model 是好模型名，usage 记 failover_from=坏模型名且 success=true。
#[tokio::test(flavor = "multi_thread")]
async fn e2e_openai_failover() {
    tokio::time::timeout(TIMEOUT, async {
        let harness = TestHarness::spawn(HarnessOpts {
            exposed_port_count: 1,
            ..HarnessOpts::default()
        })
        .await;
        let api = harness.api_client();
        let env = setup_gateway(&harness, &api).await;

        let bad = MockUpstream::start(MockBehavior::Always500).await;
        let good = MockUpstream::start(MockBehavior::Ok).await;
        let (_bad_pid, bad_mid) =
            create_provider_model(&api, "bad-provider", &bad.url(), "bad-model", "").await;
        let (_good_pid, good_mid) =
            create_provider_model(&api, "good-provider", &good.url(), "good-model", "").await;
        create_group_with_members(&api, "router", &[(bad_mid, 1), (good_mid, 2)]).await;

        let resp = chat_completion(
            env.gateway_port,
            &env.gateway_key,
            json!({"model": "router", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body: Value = resp.json().await.unwrap();
        // 200 必须来自好上游
        assert_eq!(body["choices"][0]["message"]["content"], "Hello from good");
        assert_eq!(body["usage"]["total_tokens"], 8);

        // 坏上游被尝试一次（500 后转移），好上游被尝试一次
        assert_eq!(bad.hit_count(), 1);
        assert_eq!(good.hit_count(), 1);
        // 好上游收到的请求体：model 是好模型真实名，stream 为显式 false
        let good_reqs = good.requests();
        assert_eq!(good_reqs.len(), 1);
        let up: Value = serde_json::from_str(&good_reqs[0].body).unwrap();
        assert_eq!(up["model"], "good-model", "好上游应收到真实好模型名");
        assert_eq!(up["stream"], false);

        // usage：failover_from=坏模型名，model_name=好模型名，success=true
        let log = latest_usage_log(&api).await;
        assert_eq!(log["success"], 1);
        assert_eq!(log["model_name"], "good-model");
        assert_eq!(log["failover_from"], "bad-model");
        assert_eq!(log["requested_model"], "router");
    })
    .await
    .expect("test timed out");
}

/// e2e：组 [坏模型 500 → 好模型 200]，OpenAI 入口流式请求转移成功，
/// 响应为 text/event-stream 且含好上游 chunk 内容；usage 记 failover_from 且流式 tokens 落库。
#[tokio::test(flavor = "multi_thread")]
async fn e2e_openai_stream_failover() {
    tokio::time::timeout(TIMEOUT, async {
        let harness = TestHarness::spawn(HarnessOpts {
            exposed_port_count: 1,
            ..HarnessOpts::default()
        })
        .await;
        let api = harness.api_client();
        let env = setup_gateway(&harness, &api).await;

        let bad = MockUpstream::start(MockBehavior::Always500).await;
        let good = MockUpstream::start(MockBehavior::Ok).await;
        let (_bad_pid, bad_mid) =
            create_provider_model(&api, "bad-provider", &bad.url(), "bad-model", "").await;
        let (_good_pid, good_mid) =
            create_provider_model(&api, "good-provider", &good.url(), "good-model", "").await;
        create_group_with_members(&api, "router", &[(bad_mid, 1), (good_mid, 2)]).await;

        let resp = chat_completion(
            env.gateway_port,
            &env.gateway_key,
            json!({
                "model": "router",
                "stream": true,
                "messages": [{"role": "user", "content": "hi"}]
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("content-type")
                .unwrap()
                .to_str()
                .unwrap(),
            "text/event-stream"
        );
        let text = resp.text().await.unwrap();
        // 流内容来自好上游
        assert!(text.contains("Hello from good"), "body: {text}");
        assert!(text.contains("data: [DONE]"));

        // 好上游收到的请求体：model 是好模型真实名，stream=true
        let good_reqs = good.requests();
        assert_eq!(good_reqs.len(), 1);
        let up: Value = serde_json::from_str(&good_reqs[0].body).unwrap();
        assert_eq!(up["model"], "good-model");
        assert_eq!(up["stream"], true);

        // usage：failover_from=坏模型名，且流式 usage 已解析落库
        let log = latest_usage_log(&api).await;
        assert_eq!(log["success"], 1);
        assert_eq!(log["model_name"], "good-model");
        assert_eq!(log["failover_from"], "bad-model");
        assert_eq!(log["prompt_tokens"], 5);
        assert_eq!(log["completion_tokens"], 2);
        assert_eq!(log["total_tokens"], 7);
    })
    .await
    .expect("test timed out");
}

/// e2e：组 [bad1(500), bad2(503)] 全候选失败 → 响应 503，
/// usage 记 success=false 且 failover_from 有值（首选被尝试过转移）。
#[tokio::test(flavor = "multi_thread")]
async fn e2e_all_candidates_failed() {
    tokio::time::timeout(TIMEOUT, async {
        let harness = TestHarness::spawn(HarnessOpts {
            exposed_port_count: 1,
            ..HarnessOpts::default()
        })
        .await;
        let api = harness.api_client();
        let env = setup_gateway(&harness, &api).await;

        let bad1 = MockUpstream::start(MockBehavior::Always500).await;
        let bad2 = MockUpstream::start(MockBehavior::Always503).await;
        let (_p1, m1) =
            create_provider_model(&api, "bad1-provider", &bad1.url(), "bad1-model", "").await;
        let (_p2, m2) =
            create_provider_model(&api, "bad2-provider", &bad2.url(), "bad2-model", "").await;
        create_group_with_members(&api, "router", &[(m1, 1), (m2, 2)]).await;

        let resp = chat_completion(
            env.gateway_port,
            &env.gateway_key,
            json!({"model": "router", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(bad1.hit_count(), 1);
        assert_eq!(bad2.hit_count(), 1);

        // usage：失败记录，failover_from 记首选坏模型名
        let log = latest_usage_log(&api).await;
        assert_eq!(log["success"], 0);
        assert_eq!(log["status_code"], 503);
        assert_eq!(log["failover_from"], "bad1-model");
        assert_eq!(log["model_name"], "bad1-model");
    })
    .await
    .expect("test timed out");
}

/// e2e：熔断生效——组 [bad(500), good(200)] 连续 5 次失败后坏候选不再被请求
/// （熔断跳过），组详情 API 可见 state=open，手动重置后恢复尝试。
#[tokio::test(flavor = "multi_thread")]
async fn e2e_breaker_opens_and_manual_reset() {
    tokio::time::timeout(TIMEOUT, async {
        let harness = TestHarness::spawn(HarnessOpts {
            exposed_port_count: 1,
            ..HarnessOpts::default()
        })
        .await;
        let api = harness.api_client();
        let env = setup_gateway(&harness, &api).await;

        let bad = MockUpstream::start(MockBehavior::Always500).await;
        let good = MockUpstream::start(MockBehavior::Ok).await;
        let (_bad_pid, bad_mid) =
            create_provider_model(&api, "bad-provider", &bad.url(), "bad-model", "").await;
        let (_good_pid, good_mid) =
            create_provider_model(&api, "good-provider", &good.url(), "good-model", "").await;
        let gid = create_group_with_members(&api, "router", &[(bad_mid, 1), (good_mid, 2)]).await;

        // 连续 5 次：每次都转移到好候选成功；坏候选恰好累计 5 次失败 → 熔断打开
        for _ in 0..5 {
            let resp = chat_completion(
                env.gateway_port,
                &env.gateway_key,
                json!({"model": "router", "messages": [{"role": "user", "content": "hi"}]}),
            )
            .await;
            assert_eq!(resp.status(), StatusCode::OK);
        }
        assert_eq!(bad.hit_count(), 5, "前 5 次应各打一次坏候选");

        // 第 6 次：仍 200，但坏候选熔断跳过，命中数不再增长
        let resp = chat_completion(
            env.gateway_port,
            &env.gateway_key,
            json!({"model": "router", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(bad.hit_count(), 5, "熔断打开后坏候选不应再被请求");

        // 组详情：坏成员 breaker.state == "open"，连续失败 5
        let (status, detail) = api.get_json(&format!("/api/llm/model-groups/{gid}")).await;
        assert!(status.is_success(), "group detail failed: {status}");
        let members = detail["members"].as_array().unwrap();
        let bad_member = members
            .iter()
            .find(|m| m["model_name"] == "bad-model")
            .expect("bad member in group detail");
        assert_eq!(bad_member["breaker"]["state"], "open");
        assert_eq!(bad_member["breaker"]["consecutive_failures"], 5);
        assert!(
            bad_member["breaker"]["cooldown_remaining_secs"].as_u64().unwrap() > 0,
            "open 时应有剩余冷却秒数"
        );

        // 手动重置 → 再发一次 → 坏候选恢复被尝试（命中 6）
        let (status, reset) = api
            .post_json(
                &format!("/api/llm/model-groups/{gid}/reset-breaker"),
                json!({}),
            )
            .await;
        assert!(status.is_success(), "reset breaker failed: {status}");
        assert_eq!(reset["status"], "ok");
        assert_eq!(reset["reset"], 2);

        let resp = chat_completion(
            env.gateway_port,
            &env.gateway_key,
            json!({"model": "router", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(bad.hit_count(), 6, "重置后坏候选应恢复被尝试");
    })
    .await
    .expect("test timed out");
}

/// e2e 回归：只配 provider+model（不建组）的单模型请求行为不变——
/// 200 正常返回，usage 无 failover_from（NULL）。
#[tokio::test(flavor = "multi_thread")]
async fn e2e_single_model_no_group_regression() {
    tokio::time::timeout(TIMEOUT, async {
        let harness = TestHarness::spawn(HarnessOpts {
            exposed_port_count: 1,
            ..HarnessOpts::default()
        })
        .await;
        let api = harness.api_client();
        let env = setup_gateway(&harness, &api).await;

        let good = MockUpstream::start(MockBehavior::Ok).await;
        let (_pid, _mid) = create_provider_model(
            &api,
            "single-provider",
            &good.url(),
            "single-model",
            "single-alias",
        )
        .await;

        let resp = chat_completion(
            env.gateway_port,
            &env.gateway_key,
            json!({
                "model": "single-alias",
                "messages": [{"role": "user", "content": "hi"}]
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["choices"][0]["message"]["content"], "Hello from good");

        let log = latest_usage_log(&api).await;
        assert_eq!(log["success"], 1);
        assert_eq!(log["model_name"], "single-model");
        assert!(
            log["failover_from"].is_null(),
            "未配组不应有 failover_from: {log}"
        );
    })
    .await
    .expect("test timed out");
}
