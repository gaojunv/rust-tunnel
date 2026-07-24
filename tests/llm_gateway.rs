//! Integration tests: LLM Gateway (spec docs/superpowers/specs/2026-07-22-llm-gateway-design.md)
//!
//! 覆盖：管理 API CRUD 全流程、提供商 API Key 加密落库、对外 OpenAI/Anthropic
//! 端点端到端（mock 上游）、认证强制、404 可用模型列表、流式转换、
//! 与普通 HTTP 反代规则同端口共存、Gateway 配置持久化恢复。

#[path = "common/mod.rs"]
mod common;

use common::{wait_until, HarnessOpts, TestHarness};
use reqwest::StatusCode;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TIMEOUT: Duration = Duration::from_secs(15);
const GW_DOMAIN: &str = "llm.test";
const ANTHROPIC_DOMAIN: &str = "anthropic.test";
const UPSTREAM_KEY: &str = "sk-upstream-secret";

// ── Mock 上游 LLM 提供商 ────────────────────────────────────────

#[derive(Debug, Clone)]
struct RecordedRequest {
    authorization: Option<String>,
    body: String,
}

struct MockUpstream {
    addr: SocketAddr,
    recorded: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl MockUpstream {
    /// 启动一个 OpenAI 兼容的 mock 上游：body 含 `"stream":true` 时回 SSE，
    /// 否则回标准 chat.completion JSON。
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let recorded: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded_task = recorded.clone();

        tokio::spawn(async move {
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let recorded = recorded_task.clone();
                tokio::spawn(async move {
                    // 读 headers
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
                    let authorization = headers.lines().find_map(|l| {
                        // header 名大小写不敏感，但 value 必须保持原样
                        let (name, value) = l.split_once(':')?;
                        if name.trim().eq_ignore_ascii_case("authorization") {
                            Some(value.trim().to_string())
                        } else {
                            None
                        }
                    });
                    recorded.lock().unwrap().push(RecordedRequest {
                        authorization,
                        body: body.clone(),
                    });

                    let response = if body.contains("\"stream\":true") {
                        mock_sse_response()
                    } else {
                        mock_json_response()
                    };
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.flush().await;
                });
            }
        });

        Self { addr, recorded }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.recorded.lock().unwrap().clone()
    }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn mock_json_response() -> String {
    let body = json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion",
        "created": 1,
        "model": "deepseek-chat",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "Hello from mock"},
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

fn mock_sse_response() -> String {
    let sse = concat!(
        "data: {\"id\":\"c1\",\"model\":\"deepseek-chat\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c1\",\"model\":\"deepseek-chat\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c1\",\"model\":\"deepseek-chat\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\n",
        "data: [DONE]\n\n",
    );
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        sse.len(),
        sse
    )
}

// ── 公共 setup ──────────────────────────────────────────────────

struct GatewayEnv {
    gateway_port: u16,
    gateway_key: String,
    provider_id: String,
    upstream: MockUpstream,
}

/// 完整搭建：gateway 配置 + provider + model(alias=fast-model) + 网关 API key，
/// 并等待 LLM 监听器就绪。
/// `anthropic_domain` 为 None 时不配置 Anthropic 入口。
async fn setup_gateway(
    harness: &TestHarness,
    api: &common::api_client::ApiClient,
    anthropic_domain: Option<&str>,
) -> GatewayEnv {
    let gateway_port = harness.exposed_ports[0];
    let upstream = MockUpstream::start().await;

    // Gateway 配置（内部创建 type=llm 的 ProxyRule 并 reconcile）
    let mut gw_body = json!({
        "enabled": true,
        "openai_domain": GW_DOMAIN,
        "listen": format!("127.0.0.1:{gateway_port}"),
        "tls_enabled": false,
    });
    if let Some(ad) = anthropic_domain {
        gw_body["anthropic_domain"] = json!(ad);
    }
    let (status, body) = api
        .put_json("/api/llm/gateway", gw_body)
        .await;
    assert!(status.is_success(), "PUT gateway failed: {status} {body}");

    // Provider（base_url 指向 mock 上游）
    let (status, body) = api
        .post_json(
            "/api/llm/providers",
            json!({
                "name": "mock-provider",
                "provider_type": "deepseek",
                "base_url": upstream.url(),
                "api_key": UPSTREAM_KEY,
            }),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "create provider failed: {body}"
    );
    let provider_id = body["id"].as_str().unwrap().to_string();

    // Model（上游真实名 deepseek-chat，对外别名 fast-model）
    let (status, body) = api
        .post_json(
            &format!("/api/llm/providers/{provider_id}/models"),
            json!({"model_name": "deepseek-chat", "alias": "fast-model", "tags": ["coding"]}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "add model failed: {body}");

    // 网关 API Key
    let (status, body) = api
        .post_json("/api/llm/api-keys", json!({"name": "e2e-test"}))
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
        provider_id,
        upstream,
    }
}

fn gateway_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}

// ── 测试：管理 API CRUD 全流程 ──────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn llm_mgmt_crud_flow() {
    tokio::time::timeout(TIMEOUT, async {
        let harness = TestHarness::spawn(HarnessOpts {
            exposed_port_count: 1,
            ..HarnessOpts::default()
        })
        .await;
        let api = harness.api_client();

        // ── Gateway 配置 GET/PUT ──
        let (status, _body) = api.get_json("/api/llm/gateway").await;
        assert!(status.is_success());

        let gw_port = harness.exposed_ports[0];
        let (status, _) = api
            .put_json(
                "/api/llm/gateway",
                json!({
                    "enabled": true,
                    "openai_domain": GW_DOMAIN,
                    "listen": format!("127.0.0.1:{gw_port}"),
                    "tls_enabled": false,
                }),
            )
            .await;
        assert!(status.is_success());

        let (status, body) = api.get_json("/api/llm/gateway").await;
        assert!(status.is_success());
        assert_eq!(body["enabled"], true);
        assert_eq!(body["openai_domain"], GW_DOMAIN);

        // enabled 时必须给域名
        let (status, _) = api
            .put_json("/api/llm/gateway", json!({"enabled": true, "domain": ""}))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // ── Provider CRUD ──
        let (status, body) = api
            .post_json(
                "/api/llm/providers",
                json!({
                    "name": "ds",
                    "provider_type": "deepseek",
                    "base_url": "https://api.deepseek.com",
                    "api_key": "sk-secret-1",
                    "extra_config": "{\"region\":\"cn\"}"
                }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let pid = body["id"].as_str().unwrap().to_string();

        // 非法 provider_type 被拒
        let (status, _) = api
            .post_json(
                "/api/llm/providers",
                json!({"name": "x", "provider_type": "openai", "base_url": "https://x", "api_key": "k"}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // base_url 为空时回落到该类型默认端点（spec 对接约定表）
        let (status, body) = api
            .post_json(
                "/api/llm/providers",
                json!({"name": "kimi-default", "provider_type": "kimi", "base_url": "", "api_key": "k"}),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let kimi_id = body["id"].as_str().unwrap().to_string();
        let (_, body) = api.get_json("/api/llm/providers").await;
        let kimi = body["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == kimi_id)
            .unwrap()
            .clone();
        assert_eq!(kimi["base_url"], "https://api.moonshot.cn");

        // 列表不泄露 api_key
        let providers = body["providers"].as_array().unwrap();
        for p in providers {
            assert!(
                p.get("api_key").is_none() || p["api_key"].as_str() == Some(""),
                "api_key must not be exposed: {p}"
            );
        }

        // 落库应为密文（enc:v1: 前缀），不是明文
        let db = harness.server_state.db().expect("db").clone();
        let stored = db.llm_get_provider(&pid).await.unwrap().unwrap();
        assert!(
            stored.api_key.starts_with("enc:v1:"),
            "provider api_key should be encrypted at rest, got {}",
            stored.api_key
        );
        assert!(!stored.api_key.contains("sk-secret-1"));
        assert!(stored.extra_config.unwrap().starts_with("enc:v1:"));

        // Update：不传 api_key / extra_config 时应保留原值（不覆盖、不二次加密）
        let (status, _) = api
            .put_json(
                &format!("/api/llm/providers/{pid}"),
                json!({
                    "name": "ds-renamed",
                    "provider_type": "deepseek",
                    "base_url": "https://api.deepseek.com",
                    "api_key": ""
                }),
            )
            .await;
        assert!(status.is_success());
        let stored2 = db.llm_get_provider(&pid).await.unwrap().unwrap();
        assert_eq!(stored2.name, "ds-renamed");
        assert_eq!(stored2.api_key, stored.api_key, "empty api_key must preserve stored ciphertext");
        assert!(stored2.extra_config.is_some(), "None extra_config must preserve existing value");

        // Update：非法 provider_type 被拒
        let (status, _) = api
            .put_json(
                &format!("/api/llm/providers/{pid}"),
                json!({"name": "ds", "provider_type": "bad", "base_url": "https://x", "api_key": ""}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // Toggle
        let (status, _) = api
            .patch_json(&format!("/api/llm/providers/{pid}"), json!({"enabled": false}))
            .await;
        assert!(status.is_success());
        let stored3 = db.llm_get_provider(&pid).await.unwrap().unwrap();
        assert_eq!(stored3.enabled, 0);

        // ── Model CRUD ──
        let (status, body) = api
            .post_json(
                &format!("/api/llm/providers/{pid}/models"),
                json!({"model_name": "deepseek-chat", "alias": "fast", "tags": ["coding"]}),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let mid = body["id"].as_str().unwrap().to_string();

        let (_, body) = api
            .get_json(&format!("/api/llm/providers/{pid}/models"))
            .await;
        let models = body["models"].as_array().unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["alias"], "fast");
        assert_eq!(models[0]["tags"], json!(["coding"]));

        let (_, body) = api.get_json("/api/llm/models").await;
        assert_eq!(body["models"].as_array().unwrap().len(), 1);

        // 更新别名/标签
        let (status, _) = api
            .put_json(
                &format!("/api/llm/models/{mid}"),
                json!({"model_name": "deepseek-chat", "alias": "fast-v2", "tags": ["cheap"]}),
            )
            .await;
        assert!(status.is_success());
        let (_, body) = api.get_json("/api/llm/models").await;
        assert_eq!(body["models"][0]["alias"], "fast-v2");
        assert_eq!(body["models"][0]["tags"], json!(["cheap"]));

        // ── API Key CRUD ──
        let (status, body) = api
            .post_json("/api/llm/api-keys", json!({"name": "cursor"}))
            .await;
        assert_eq!(status, StatusCode::CREATED);
        let key_id = body["id"].as_str().unwrap().to_string();
        let full_key = body["key"].as_str().unwrap().to_string();
        assert!(full_key.starts_with("sk-"));

        // 列表只给前缀，不给完整 key
        let (_, body) = api.get_json("/api/llm/api-keys").await;
        let keys = body["api_keys"].as_array().unwrap();
        assert_eq!(keys.len(), 1);
        assert!(keys[0]["key_prefix"].as_str().unwrap().contains("..."));
        assert!(keys[0].get("key").is_none(), "full key must not be listed");
        assert!(keys[0].get("key_hash").is_none(), "hash must not be listed");

        // Toggle + 吊销
        let (status, _) = api
            .patch_json(&format!("/api/llm/api-keys/{key_id}"), json!({"enabled": false}))
            .await;
        assert!(status.is_success());
        let status = api.delete_status(&format!("/api/llm/api-keys/{key_id}")).await;
        assert!(status.is_success());
        let (_, body) = api.get_json("/api/llm/api-keys").await;
        assert!(body["api_keys"].as_array().unwrap().is_empty());

        // ── 级联删除：删 provider 连带删模型 ──
        let status = api.delete_status(&format!("/api/llm/providers/{pid}")).await;
        assert!(status.is_success());
        let (_, body) = api.get_json("/api/llm/models").await;
        assert!(
            body["models"].as_array().unwrap().is_empty(),
            "models should be cascade-deleted with provider"
        );
    })
    .await
    .expect("test timed out");
}

// ── 测试：OpenAI 端点端到端 ─────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn llm_chat_completions_e2e() {
    tokio::time::timeout(TIMEOUT, async {
        let harness = TestHarness::spawn(HarnessOpts {
            exposed_port_count: 1,
            ..HarnessOpts::default()
        })
        .await;
        let api = harness.api_client();
        let env = setup_gateway(&harness, &api, None).await;

        // 别名路由
        let resp = gateway_client()
            .post(format!(
                "http://127.0.0.1:{}/v1/chat/completions",
                env.gateway_port
            ))
            .header("Host", GW_DOMAIN)
            .bearer_auth(&env.gateway_key)
            .json(&json!({
                "model": "fast-model",
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["choices"][0]["message"]["content"], "Hello from mock");
        assert_eq!(body["usage"]["total_tokens"], 8);

        // 上游必须收到：真实模型名 + 解密后的 provider key
        let recorded = env.upstream.requests();
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            recorded[0].authorization.as_deref(),
            Some(format!("Bearer {UPSTREAM_KEY}").as_str()),
            "upstream should receive the decrypted provider api key"
        );
        let upstream_body: Value = serde_json::from_str(&recorded[0].body).unwrap();
        assert_eq!(
            upstream_body["model"], "deepseek-chat",
            "alias should resolve to real model name"
        );
        assert_eq!(upstream_body["stream"], false);

        // 真实模型名直接路由也应可用
        let resp = gateway_client()
            .post(format!(
                "http://127.0.0.1:{}/v1/chat/completions",
                env.gateway_port
            ))
            .header("Host", GW_DOMAIN)
            .bearer_auth(&env.gateway_key)
            .json(&json!({
                "model": "deepseek-chat",
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    })
    .await
    .expect("test timed out");
}

// ── 测试：认证强制 ──────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn llm_gateway_auth_enforcement() {
    tokio::time::timeout(TIMEOUT, async {
        let harness = TestHarness::spawn(HarnessOpts {
            exposed_port_count: 1,
            ..HarnessOpts::default()
        })
        .await;
        let api = harness.api_client();
        let env = setup_gateway(&harness, &api, None).await;

        // 无 Authorization → 401 OpenAI 格式错误
        let resp = gateway_client()
            .post(format!(
                "http://127.0.0.1:{}/v1/chat/completions",
                env.gateway_port
            ))
            .header("Host", GW_DOMAIN)
            .json(&json!({"model": "fast-model", "messages": []}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["error"]["type"], "authentication_error");

        // 错误 key → 401
        let resp = gateway_client()
            .get(format!("http://127.0.0.1:{}/v1/models", env.gateway_port))
            .header("Host", GW_DOMAIN)
            .bearer_auth("sk-wrong-key")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // 禁用后的 key → 401
        let (_, body) = api.get_json("/api/llm/api-keys").await;
        let key_id = body["api_keys"][0]["id"].as_str().unwrap().to_string();
        let (status, _) = api
            .patch_json(
                &format!("/api/llm/api-keys/{key_id}"),
                json!({"enabled": false}),
            )
            .await;
        assert!(status.is_success());

        let resp = gateway_client()
            .get(format!("http://127.0.0.1:{}/v1/models", env.gateway_port))
            .header("Host", GW_DOMAIN)
            .bearer_auth(&env.gateway_key)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // 错误域名 → 不进入 LLM 处理（反代无匹配规则 → 非 401）
        let resp = gateway_client()
            .get(format!("http://127.0.0.1:{}/v1/models", env.gateway_port))
            .header("Host", "other.test")
            .bearer_auth(&env.gateway_key)
            .send()
            .await
            .unwrap();
        assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
    })
    .await
    .expect("test timed out");
}

// ── 测试：模型未找到 → 404 + 可用模型列表 ───────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn llm_model_not_found_lists_available() {
    tokio::time::timeout(TIMEOUT, async {
        let harness = TestHarness::spawn(HarnessOpts {
            exposed_port_count: 1,
            ..HarnessOpts::default()
        })
        .await;
        let api = harness.api_client();
        let env = setup_gateway(&harness, &api, None).await;

        let resp = gateway_client()
            .post(format!("http://127.0.0.1:{}/v1/chat/completions", env.gateway_port))
            .header("Host", GW_DOMAIN)
            .bearer_auth(&env.gateway_key)
            .json(&json!({"model": "does-not-exist", "messages": [{"role": "user", "content": "hi"}]}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body: Value = resp.json().await.unwrap();
        let available = body["error"]["available_models"]
            .as_array()
            .expect("404 body should carry available_models");
        assert!(
            available.iter().any(|m| m.as_str() == Some("fast-model")),
            "available_models should contain alias, body={body}"
        );
    })
    .await
    .expect("test timed out");
}

// ── 测试：/v1/models 列表 ───────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn llm_list_models_endpoint() {
    tokio::time::timeout(TIMEOUT, async {
        let harness = TestHarness::spawn(HarnessOpts {
            exposed_port_count: 1,
            ..HarnessOpts::default()
        })
        .await;
        let api = harness.api_client();
        let env = setup_gateway(&harness, &api, None).await;

        let resp = gateway_client()
            .get(format!("http://127.0.0.1:{}/v1/models", env.gateway_port))
            .header("Host", GW_DOMAIN)
            .bearer_auth(&env.gateway_key)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["object"], "list");
        let data = body["data"].as_array().unwrap();
        assert_eq!(data.len(), 1);
        assert_eq!(data[0]["id"], "fast-model");
        assert_eq!(data[0]["object"], "model");

        // 禁用 provider 后模型不再列出
        let (status, _) = api
            .patch_json(
                &format!("/api/llm/providers/{}", env.provider_id),
                json!({"enabled": false}),
            )
            .await;
        assert!(status.is_success());
        let resp = gateway_client()
            .get(format!("http://127.0.0.1:{}/v1/models", env.gateway_port))
            .header("Host", GW_DOMAIN)
            .bearer_auth(&env.gateway_key)
            .send()
            .await
            .unwrap();
        let body: Value = resp.json().await.unwrap();
        assert!(body["data"].as_array().unwrap().is_empty());
    })
    .await
    .expect("test timed out");
}

// ── 测试：Anthropic 端点（非流式 + 流式）──────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn llm_anthropic_messages_non_stream() {
    tokio::time::timeout(TIMEOUT, async {
        let harness = TestHarness::spawn(HarnessOpts {
            exposed_port_count: 1,
            ..HarnessOpts::default()
        })
        .await;
        let api = harness.api_client();
        let env = setup_gateway(&harness, &api, Some(ANTHROPIC_DOMAIN)).await;

        let resp = gateway_client()
            .post(format!("http://127.0.0.1:{}/v1/messages", env.gateway_port))
            .header("Host", ANTHROPIC_DOMAIN)
            .header("anthropic-version", "2023-06-01")
            .bearer_auth(&env.gateway_key)
            .json(&json!({
                "model": "fast-model",
                "max_tokens": 1024,
                "system": "You are helpful.",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: Value = resp.json().await.unwrap();

        // Anthropic Messages 格式
        assert_eq!(body["type"], "message");
        assert_eq!(body["role"], "assistant");
        assert_eq!(body["content"][0]["type"], "text");
        assert_eq!(body["content"][0]["text"], "Hello from mock");
        assert_eq!(
            body["stop_reason"], "end_turn",
            "OpenAI stop 必须映射为 end_turn"
        );
        assert_eq!(body["usage"]["input_tokens"], 5);
        assert_eq!(body["usage"]["output_tokens"], 3);

        // 上游收到的是转换后的 OpenAI 格式：system 变首条 system message
        let recorded = env.upstream.requests();
        let upstream_body: Value = serde_json::from_str(&recorded[0].body).unwrap();
        assert_eq!(upstream_body["model"], "deepseek-chat");
        assert_eq!(upstream_body["messages"][0]["role"], "system");
        assert_eq!(upstream_body["messages"][0]["content"], "You are helpful.");
        assert_eq!(upstream_body["messages"][1]["role"], "user");
    })
    .await
    .expect("test timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn llm_anthropic_messages_streaming() {
    tokio::time::timeout(TIMEOUT, async {
        let harness = TestHarness::spawn(HarnessOpts {
            exposed_port_count: 1,
            ..HarnessOpts::default()
        })
        .await;
        let api = harness.api_client();
        let env = setup_gateway(&harness, &api, Some(ANTHROPIC_DOMAIN)).await;

        let resp = gateway_client()
            .post(format!("http://127.0.0.1:{}/v1/messages", env.gateway_port))
            .header("Host", ANTHROPIC_DOMAIN)
            .header("anthropic-version", "2023-06-01")
            .bearer_auth(&env.gateway_key)
            .json(&json!({
                "model": "fast-model",
                "max_tokens": 1024,
                "stream": true,
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .send()
            .await
            .unwrap();
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

        // 必须是 Anthropic 事件流，而不是 OpenAI chunk 透传
        assert!(
            text.contains("event: message_start"),
            "missing message_start:\n{text}"
        );
        assert!(
            text.contains("event: content_block_start"),
            "missing block start:\n{text}"
        );
        assert!(
            text.contains("event: content_block_delta"),
            "missing delta:\n{text}"
        );
        assert!(
            text.contains("\"text\":\"Hello\""),
            "missing text delta:\n{text}"
        );
        assert!(
            text.contains("event: message_delta"),
            "missing message_delta:\n{text}"
        );
        assert!(
            text.contains("event: message_stop"),
            "missing message_stop:\n{text}"
        );
        assert!(text.contains("\"stop_reason\":\"end_turn\""));
        assert!(
            !text.contains("chat.completion.chunk"),
            "OpenAI chunks leaked:\n{text}"
        );
        assert!(!text.contains("[DONE]"));
    })
    .await
    .expect("test timed out");
}

// ── 测试：OpenAI 流式透传 ───────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn llm_openai_streaming_passthrough() {
    tokio::time::timeout(TIMEOUT, async {
        let harness = TestHarness::spawn(HarnessOpts {
            exposed_port_count: 1,
            ..HarnessOpts::default()
        })
        .await;
        let api = harness.api_client();
        let env = setup_gateway(&harness, &api, None).await;

        let resp = gateway_client()
            .post(format!(
                "http://127.0.0.1:{}/v1/chat/completions",
                env.gateway_port
            ))
            .header("Host", GW_DOMAIN)
            .bearer_auth(&env.gateway_key)
            .json(&json!({
                "model": "fast-model",
                "stream": true,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .send()
            .await
            .unwrap();
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
        // OpenAI 客户端应收到 OpenAI 格式的 chunk 流
        assert!(
            text.contains("\"delta\""),
            "should relay OpenAI chunks:\n{text}"
        );
        assert!(text.contains("\"content\":\"Hello\""));
        assert!(text.contains("data: [DONE]"));
    })
    .await
    .expect("test timed out");
}

// ── 测试：与普通 HTTP 规则同端口共存（回归：LLM 不得抢占其他域名）──

#[tokio::test(flavor = "multi_thread")]
async fn llm_gateway_coexists_with_http_rule() {
    tokio::time::timeout(TIMEOUT, async {
        let harness = TestHarness::spawn(HarnessOpts {
            exposed_port_count: 1,
            ..HarnessOpts::default()
        })
        .await;
        let api = harness.api_client();
        let env = setup_gateway(&harness, &api, None).await;

        // 普通 HTTP 后端
        let web_backend = MockUpstream::start().await;

        // 同一监听端口上加一条普通 HTTP 规则（web.test → web 后端）
        let (status, body) = api
            .post_json(
                "/api/proxy/rules",
                json!({
                    "name": "web",
                    "type": "http",
                    "listen": format!("127.0.0.1:{}", env.gateway_port),
                    "domains": ["web.test"],
                    "routes": [{
                        "path": "/",
                        "backends": [{
                            "kind": "direct",
                            "addr": web_backend.addr.to_string(),
                            "weight": 100,
                            "protocol": "http1",
                            "scheme": "http"
                        }]
                    }]
                }),
            )
            .await;
        assert!(
            status.is_success(),
            "create http rule failed: {status} {body}"
        );

        // 等规则生效：web.test 的 /v1/chat/completions 应到达 web 后端
        let base = format!("http://127.0.0.1:{}", env.gateway_port);
        wait_until("http rule active on shared port", || {
            let base = base.clone();
            async move {
                let client = reqwest::Client::builder()
                    .timeout(Duration::from_millis(300))
                    .build()
                    .ok()?;
                let resp = client
                    .post(format!("{base}/v1/chat/completions"))
                    .header("Host", "web.test")
                    .json(&json!({"model": "whatever"}))
                    .send()
                    .await
                    .ok()?;
                if resp.status() == StatusCode::OK {
                    Some(())
                } else {
                    None
                }
            }
        })
        .await
        .expect("http rule never became active on the shared port");

        // LLM 域名仍走 LLM 网关
        let resp = gateway_client()
            .post(format!("{base}/v1/chat/completions"))
            .header("Host", GW_DOMAIN)
            .bearer_auth(&env.gateway_key)
            .json(&json!({"model": "fast-model", "messages": [{"role": "user", "content": "hi"}]}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["choices"][0]["message"]["content"], "Hello from mock");
    })
    .await
    .expect("test timed out");
}

// ── 测试：Gateway 配置随 ProxyRule 持久化并可恢复 ───────────────

#[tokio::test(flavor = "multi_thread")]
async fn llm_gateway_config_restored_from_rule() {
    tokio::time::timeout(TIMEOUT, async {
        let harness = TestHarness::spawn(HarnessOpts {
            exposed_port_count: 1,
            ..HarnessOpts::default()
        })
        .await;
        let api = harness.api_client();
        let gw_port = harness.exposed_ports[0];

        let (status, _) = api
            .put_json(
                "/api/llm/gateway",
                json!({
                    "enabled": true,
                    "openai_domain": GW_DOMAIN,
                    "listen": format!("127.0.0.1:{gw_port}"),
                    "tls_enabled": false,
                }),
            )
            .await;
        assert!(status.is_success());

        // ProxyRule 列表中应存在固定的 __llm_gateway__ 规则
        let (_, body) = api.get_json("/api/proxy/rules").await;
        let rules = body["rules"].as_array().unwrap();
        let llm_rule = rules
            .iter()
            .find(|r| r["id"] == "__llm_gateway__")
            .expect("llm gateway rule should exist");
        assert_eq!(llm_rule["type"], "llm");
        assert_eq!(llm_rule["domains"], json!([GW_DOMAIN]));

        // 模拟重启：从 DB 重新加载规则 + 重新初始化 LLM 状态
        harness
            .server_state
            .proxy_state
            .load_from_db()
            .await
            .expect("load rules from db");
        harness
            .server_state
            .proxy_state
            .init_llm_state(harness.server_state.db().cloned(), Some([42u8; 32]))
            .await;

        // Gateway 配置应从规则恢复
        let (status, body) = api.get_json("/api/llm/gateway").await;
        assert!(status.is_success());
        assert_eq!(body["enabled"], true);
        assert_eq!(body["openai_domain"], GW_DOMAIN);
        assert_eq!(body["listen"], format!("127.0.0.1:{gw_port}"));
    })
    .await
    .expect("test timed out");
}
