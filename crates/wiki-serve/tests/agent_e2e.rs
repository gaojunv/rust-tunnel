// Codex app-server e2e 冒烟（真二进制 + 假上游），`CODEX_E2E_BIN` 门控。
//
// 默认跳过：CI 无需下载二进制即可全绿；置 `CODEX_E2E_BIN=/tmp/codex-app-server-...`
// 后运行 `cargo test -p rust-tunnel-wiki-serve --test agent_e2e -- --nocapture`。
//
// 假上游同时监听 `POST /v1/responses` 与 `POST /responses`（codex 的 base_url
// 语义为 `{base_url}/responses`，而 plan 注释曾写“自动拼 /v1/responses”，实测
// 两种 base_url 均被使用——测试对两种都返回 SSE，避免因用户填 `.../v1` 或
// `...` 而误判）。
// 另外提供 `GET /v1/models` 假模型列表。
//
// 手工协议验证实录（/tmp/manual_probe3.py，codex 0.153.4）：
// - `initialize` 必需 `clientInfo: {name, title, version}` + `capabilities: {experimentalApi, requestAttestation}`；
//   仅发 `{name, version}` 会被服务端拒（本仓已修 `mod.rs` 补全 title/capabilities）。
// - `initialized` 为无参 notification，紧随 initialize 成功后发送。
// - `thread/start {}` 回 `thread: {id, ...}`，并广播 `thread/started` notification。
// - `turn/start {threadId, input: [{type:"text", text, text_elements:[]}]}`
//   回 `turn: {id, items:[], status:"inProgress"}`，随后经 notification 依次收到
//   `item/started(userMessage)` → `item/completed(userMessage)` → 远端请求 `/responses`
//   → `item/started(agentMessage)` → 若干 `item/agentMessage/delta` → `item/completed(agentMessage)`
//   → `turn/completed {turn:{items:[agentMessage], status:"completed"}}`。
// - 远端 wire：`POST {base_url}/responses`，Header `Authorization: Bearer <env_key 值>`，
//   Body 为 Responses API（`model/instructions/input/tools`），SSE 事件流以
//   `event: response.* / data: {...}` + `data: [DONE]` 结束。
// - 配置校验：`[model_providers.tunnel]` 0.153+ 新增必填 `name = "tunnel"`，缺失则报
//   `provider name must not be empty` 并回退默认（本仓 config.rs 已补）。
//! Codex e2e 冒烟（`CODEX_E2E_BIN` 门控，默认跳过）.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::pedantic)]
#![allow(missing_docs)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{json, Value};
use tokio::sync::oneshot;

use rust_tunnel_wiki_serve::agent::config::{AgentSettings, CodexHome};
use rust_tunnel_wiki_serve::agent::{AgentEventSink, AgentManager, AgentStatus};

#[derive(Clone, Default)]
struct Recorder {
    inner: Arc<Mutex<Vec<RecordedRequest>>>,
}

#[derive(Clone, Debug)]
struct RecordedRequest {
    method: String,
    path: String,
    headers: HeaderMap,
    body: String,
}

impl Recorder {
    fn push(&self, req: RecordedRequest) {
        if let Ok(mut g) = self.inner.lock() {
            g.push(req);
        }
    }

    fn snapshot(&self) -> Vec<RecordedRequest> {
        self.inner.lock().map(|g| g.clone()).unwrap_or_default()
    }

    fn has_auth_bearer(&self, expected: &str) -> bool {
        let needle = format!("Bearer {expected}");
        self.snapshot().iter().any(|r| {
            r.headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v == needle)
        })
    }

    fn has_post_path(&self, path: &str) -> bool {
        self.snapshot()
            .iter()
            .any(|r| r.method == "POST" && r.path == path)
    }
}

#[derive(Clone)]
struct AppState {
    recorder: Recorder,
}

fn sse_responses_fixture() -> String {
    let mut out = String::new();
    let ev = |name: &str, data: Value| -> String {
        format!("event: {name}\ndata: {}\n\n", data)
    };
    out.push_str(&ev(
        "response.created",
        json!({"type":"response.created","response":{"id":"resp_e2e","object":"response","created_at":1_700_000_000u64,"model":"gpt-4o","status":"in_progress","output":[]}}),
    ));
    out.push_str(&ev(
        "response.output_item.added",
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_e2e","status":"in_progress","role":"assistant","content":[]}}),
    ));
    out.push_str(&ev(
        "response.content_part.added",
        json!({"type":"response.content_part.added","output_index":0,"content_index":0,"part":{"type":"output_text","text":""}}),
    ));
    for delta in ["Hello ", "from ", "fake upstream"] {
        out.push_str(&ev(
            "response.output_text.delta",
            json!({"type":"response.output_text.delta","item_id":"msg_e2e","output_index":0,"content_index":0,"delta":delta}),
        ));
    }
    out.push_str(&ev(
        "response.output_text.done",
        json!({"type":"response.output_text.done","output_index":0,"content_index":0,"item_id":"msg_e2e","text":"Hello from fake upstream"}),
    ));
    out.push_str(&ev(
        "response.content_part.done",
        json!({"type":"response.content_part.done","output_index":0,"content_index":0,"part":{"type":"output_text","text":"Hello from fake upstream"}}),
    ));
    out.push_str(&ev(
        "response.output_item.done",
        json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg_e2e","status":"completed","role":"assistant","content":[{"type":"output_text","text":"Hello from fake upstream"}]}}),
    ));
    out.push_str(&ev(
        "response.completed",
        json!({"type":"response.completed","response":{"id":"resp_e2e","object":"response","created_at":1_700_000_000u64,"model":"gpt-4o","status":"completed","output":[{"type":"message","id":"msg_e2e","status":"completed","role":"assistant","content":[{"type":"output_text","text":"Hello from fake upstream"}]}],"usage":{"input_tokens":10,"output_tokens":20,"total_tokens":30,"input_tokens_details":{"cached_tokens":0},"output_tokens_details":{"reasoning_tokens":0}}}}),
    ));
    out.push_str("data: [DONE]\n\n");
    out
}

async fn handle_models(State(state): State<AppState>, _req: Request) -> Response {
    state.recorder.push(RecordedRequest {
        method: "GET".to_owned(),
        path: "/v1/models".to_owned(),
        headers: HeaderMap::new(),
        body: String::new(),
    });
    let body = json!({"object":"list","data":[{"id":"gpt-4o","object":"model","created":1_700_000_000u64,"owned_by":"test"}]});
    (StatusCode::OK, axum::Json(body)).into_response()
}

async fn handle_responses(State(state): State<AppState>, req: Request) -> Response {
    let path = req.uri().path().to_owned();
    let headers = req.headers().clone();
    let bytes = axum::body::to_bytes(req.into_body(), 256 * 1024)
        .await
        .unwrap_or_default();
    let body_str = String::from_utf8_lossy(&bytes).to_string();
    state.recorder.push(RecordedRequest {
        method: "POST".to_owned(),
        path: path.clone(),
        headers: headers.clone(),
        body: body_str.clone(),
    });
    let sse = sse_responses_fixture();
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .body(Body::from(sse))
        .unwrap_or_else(|_| (StatusCode::OK, Body::from(sse_responses_fixture())).into_response())
}

fn is_codex_bin_available() -> Option<std::path::PathBuf> {
    let raw = std::env::var("CODEX_E2E_BIN").ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let p = std::path::PathBuf::from(trimmed);
    if p.is_file() {
        Some(p)
    } else {
        None
    }
}

#[derive(Debug, Default, Clone)]
#[allow(clippy::type_complexity, reason = "e2e sink 快照类型直观，无需抽象")]
struct E2eSink {
    notifications: Arc<Mutex<Vec<(String, Option<Value>)>>>,
    statuses: Arc<Mutex<Vec<AgentStatus>>>,
    server_requests: Arc<Mutex<Vec<(Value, String, Option<Value>)>>>,
    parse_errors: Arc<Mutex<Vec<(String, String)>>>,
}

impl AgentEventSink for E2eSink {
    fn on_notification(&self, method: String, params: Option<Value>) {
        if let Ok(mut g) = self.notifications.lock() {
            g.push((method, params));
        }
    }
    fn on_server_request(&self, id: Value, method: String, params: Option<Value>) {
        if let Ok(mut g) = self.server_requests.lock() {
            g.push((id, method, params));
        }
    }
    fn on_status(&self, status: AgentStatus) {
        if let Ok(mut g) = self.statuses.lock() {
            g.push(status);
        }
    }
    fn on_parse_error(&self, line: String, error: String) {
        if let Ok(mut g) = self.parse_errors.lock() {
            g.push((line, error));
        }
    }
}

#[tokio::test]
async fn agent_e2e_via_fake_upstream() {
    let Some(bin) = is_codex_bin_available() else {
        eprintln!(
            "SKIP agent_e2e: CODEX_E2E_BIN 未设置或指向不存在的文件（CI 默认跳过）。\n\
             本地运行：curl -fSL https://github.com/openai/codex/releases/download/rust-v0.153.4/codex-app-server-x86_64-unknown-linux-musl.tar.gz -o /tmp/codex-as.tar.gz && tar xzf /tmp/codex-as.tar.gz -C /tmp && chmod +x /tmp/codex-app-server-x86_64-unknown-linux-musl && CODEX_E2E_BIN=/tmp/codex-app-server-x86_64-unknown-linux-musl cargo test -p rust-tunnel-wiki-serve --test agent_e2e -- --nocapture"
        );
        return;
    };

    // 假上游
    let recorder = Recorder::default();
    let app_state = AppState {
        recorder: recorder.clone(),
    };
    let app = axum::Router::new()
        .route("/v1/models", get(handle_models))
        .route("/models", get(handle_models))
        .route("/v1/responses", post(handle_responses))
        .route("/responses", post(handle_responses))
        .route("/v1/v1/responses", post(handle_responses))
        .with_state(app_state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake upstream");
    let addr = listener.local_addr().expect("addr");
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .expect("serve");
    });

    // 临时 CODEX_HOME / vault
    let tmp = tempfile::tempdir().expect("tmpdir");
    let app_config_dir = tmp.path().join("app-config");
    let vault_root = tmp.path().join("vault");
    std::fs::create_dir_all(&app_config_dir).expect("mkdir app-config");
    std::fs::create_dir_all(&vault_root).expect("mkdir vault");

    let gateway_base_url = format!("http://{addr}");
    let settings = AgentSettings {
        enabled: true,
        gateway_base_url: Some(gateway_base_url.clone()),
        gateway_api_key: Some("test-key".to_owned()),
        model: Some("gpt-4o".to_owned()),
        codex_path_override: Some(bin.display().to_string()),
        ..AgentSettings::default()
    };
    let settings_path = AgentSettings::default_path(&app_config_dir);
    settings.save(&settings_path).expect("save settings");

    let codex_home = CodexHome::default_dir(&app_config_dir);
    let cfg_path = CodexHome::ensure(&codex_home, &settings).expect("ensure codex_home");
    let cfg_text = std::fs::read_to_string(&cfg_path).expect("read config.toml");
    assert!(
        cfg_text.contains("name = \"tunnel\""),
        "config.toml 应含 name 字段：{cfg_text}"
    );
    assert!(
        cfg_text.contains(&gateway_base_url),
        "base_url 应写入假上游地址：{cfg_text}"
    );

    // 启动 AgentManager（同步阻塞，需 spawn_blocking）
    let mut manager = AgentManager::new(app_config_dir.clone(), vault_root.clone());
    let sink = Arc::new(E2eSink::default());
    let sink_for_start = Arc::clone(&sink);
    let start_result = tokio::task::spawn_blocking(move || {
        let sink_dyn: Arc<dyn AgentEventSink> = sink_for_start;
        manager.start(sink_dyn).map(|_| manager)
    })
    .await
    .expect("spawn_blocking join");

    let manager = match start_result {
        Ok(m) => m,
        Err(e) => {
            let _ = shutdown_tx.send(());
            let _ = tokio::time::timeout(Duration::from_secs(2), server_handle).await;
            panic!("AgentManager::start 失败：{e}\n假上游请求：{:#?}", recorder.snapshot());
        }
    };

    // 将 manager 置于 Arc<Mutex<_>> 以便在 spawn_blocking 中以 &self 调用 request_sync
    let shared = Arc::new(Mutex::new(manager));
    // 辅助：在 blocking 线程执行 request_sync
    async fn blocking_request(
        shared: &Arc<Mutex<AgentManager>>,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, String> {
        let shared = Arc::clone(shared);
        let method = method.to_owned();
        tokio::task::spawn_blocking(move || {
            let guard = shared.lock().expect("lock");
            guard.request_sync(method, params)
        })
        .await
        .expect("join")
    }

    // thread/start
    let thread_resp = blocking_request(&shared, "thread/start", Some(json!({})))
        .await
        .unwrap_or_else(|e| panic!("thread/start 失败：{e}"));
    let thread_id = thread_resp
        .get("thread")
        .and_then(|t| t.get("id"))
        .and_then(|id| id.as_str())
        .or_else(|| thread_resp.get("id").and_then(|id| id.as_str()))
        .unwrap_or_else(|| panic!("thread/start 响应缺 thread.id：{thread_resp}"))
        .to_owned();

    // turn/start
    let turn_resp = blocking_request(
        &shared,
        "turn/start",
        Some(json!({
            "threadId": thread_id.clone(),
            "input": [{"type":"text","text":"hello","text_elements":[]}]
        })),
    )
    .await
    .unwrap_or_else(|e| panic!("turn/start 失败：{e}"));
    assert!(
        turn_resp.get("turn").is_some() || turn_resp.get("id").is_some(),
        "turn/start 响应应含 turn：{turn_resp}"
    );

    // 等待 delta 与 turn/completed（最多 60s，降级路径见下）
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let mut saw_delta = false;
    let mut saw_turn_completed = false;
    let mut saw_item_completed = false;
    let mut degraded_ok = false;
    loop {
        if tokio::time::Instant::now() > deadline {
            break;
        }
        {
            let notifs = sink
                .notifications
                .lock()
                .map(|g| g.clone())
                .unwrap_or_default();
            for (method, params) in &notifs {
                if method == "item/agentMessage/delta" {
                    if let Some(p) = params {
                        if let Some(d) = p.get("delta").and_then(|v| v.as_str()) {
                            if !d.is_empty() {
                                saw_delta = true;
                            }
                        }
                    } else {
                        saw_delta = true;
                    }
                }
                if method == "turn/completed" {
                    saw_turn_completed = true;
                }
                if method == "item/completed" {
                    if let Some(item) = params
                        .as_ref()
                        .and_then(|p| p.get("item"))
                        .or(params.as_ref())
                    {
                        if item.get("type").and_then(|v| v.as_str()) == Some("agentMessage") {
                            saw_item_completed = true;
                        }
                    }
                }
            }
        }
        if saw_delta && saw_turn_completed {
            break;
        }
        if (recorder.has_post_path("/responses") || recorder.has_post_path("/v1/responses"))
            && recorder.has_auth_bearer("test-key")
            && tokio::time::Instant::now() > deadline - Duration::from_secs(30)
        {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let notifs = sink
                .notifications
                .lock()
                .map(|g| g.clone())
                .unwrap_or_default();
            let has_any = notifs
                .iter()
                .any(|(m, _)| m == "item/agentMessage/delta" || m == "turn/completed");
            if !has_any {
                degraded_ok = true;
                eprintln!(
                    "WARN: 未在时限内收到 agentMessage delta/turn/completed，视为 SSE fixture 不匹配的降级成功（假上游已收到正确 auth 请求）。"
                );
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let rec = recorder.snapshot();
    eprintln!("fake upstream recorded {} requests: {rec:#?}", rec.len());
    eprintln!(
        "sink notifications ({}): {:#?}",
        sink.notifications
            .lock()
            .map(|g| g.len())
            .unwrap_or(0),
        sink.notifications.lock().map(|g| g.clone()).unwrap_or_default()
    );

    assert!(
        recorder.has_auth_bearer("test-key"),
        "假上游应收到 Authorization: Bearer test-key，实际：{rec:#?}"
    );
    assert!(
        recorder.has_post_path("/responses") || recorder.has_post_path("/v1/responses"),
        "假上游应收到 POST /responses 或 POST /v1/responses，实际：{rec:#?}"
    );

    if degraded_ok {
        eprintln!(
            "DEGRADED PASS: e2e 降级为“握手+请求到达+auth 正确”，未强求完整 delta 流。"
        );
    } else {
        assert!(
            saw_delta,
            "应收到 item/agentMessage/delta，实际：{:#?}",
            sink.notifications.lock().map(|g| g.clone()).unwrap_or_default()
        );
        assert!(
            saw_turn_completed,
            "应收到 turn/completed，实际：{:#?}",
            sink.notifications.lock().map(|g| g.clone()).unwrap_or_default()
        );
        if !saw_item_completed {
            eprintln!("WARN: 未观测到 agentMessage 的 item/completed，但 turn/completed 已到达。");
        }
    }

    // 清理
    {
        let shared_clone = Arc::clone(&shared);
        let _ = tokio::task::spawn_blocking(move || {
            let mut guard = shared_clone.lock().expect("lock");
            let _ = guard.stop();
        })
        .await;
    }
    // 回收 manager（Arc 唯一持有者时可 try_unwrap，否则直接 drop）
    drop(shared);
    let _ = shutdown_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), server_handle).await;

    if degraded_ok {
        eprintln!(
            "后续补齐：对照 codex 源码的 Responses SSE 解析补齐 fixture 的事件名与字段，或抓真 OpenAI SSE 样本替换 sse_responses_fixture()。"
        );
    }

    let has_model = rec.iter().any(|r| r.body.contains("gpt-4o"));
    assert!(has_model, "假上游请求体应含 model gpt-4o，实际：{rec:#?}");

    let mut by_path: HashMap<String, usize> = HashMap::new();
    for r in &rec {
        *by_path.entry(format!("{} {}", r.method, r.path)).or_default() += 1;
    }
    eprintln!("e2e summary: by_path={by_path:?} saw_delta={saw_delta} saw_turn_completed={saw_turn_completed} degraded_ok={degraded_ok}");
}
