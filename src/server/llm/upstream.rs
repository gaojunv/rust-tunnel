use std::sync::LazyLock;

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use futures_util::StreamExt;
use reqwest::Client;

use super::ChatCompletionRequest;

/// Reusable HTTP client with connection pooling.
///
/// Timeout strategy:
/// - `connect_timeout`: 30 s — fast failure when the upstream is unreachable.
/// - `read_timeout`:   300 s (5 min) — detects genuinely hung connections while
///   allowing very large streaming responses (e.g. long tool-use / task-plan
///   generations) to complete without a premature "connection closed mid-response"
///   error.
/// - No global `timeout` — streaming LLM responses can legitimately take minutes;
///   the upstream provider enforces its own deadline.
///
/// HTTP/1.1 is forced (not HTTP/2) for two reasons:
/// 1. **read_timeout accuracy**: reqwest's `read_timeout` resets on every frame,
///    including HTTP/2 PING frames. Upstream providers that send h2 pings during
///    long generations would prevent the read timeout from ever firing, masking
///    a genuinely hung connection. HTTP/1.1 has no such pings.
/// 2. **Upstream compatibility**: Some LLM provider gateways have aggressive
///    idle timeouts (~120 s) on HTTP/2 connections. HTTP/1.1 with TCP keepalive
///    is more resilient to these middlebox timeouts.
static UPSTREAM_CLIENT: LazyLock<Client> = LazyLock::new(|| {
    Client::builder()
        .http1_only()
        .connect_timeout(std::time::Duration::from_secs(30))
        .read_timeout(std::time::Duration::from_secs(300))
        .tcp_keepalive(std::time::Duration::from_secs(60))
        .pool_max_idle_per_host(10)
        .build()
        .expect("failed to build upstream HTTP client")
});

/// Strip potential secrets (API keys, tokens) from upstream error messages.
fn sanitize_error_message(body: &str) -> String {
    // Truncate at a valid UTF-8 character boundary (max 500 chars).
    let end = if body.len() <= 500 {
        body.len()
    } else {
        // Find the last complete char boundary at or before byte 500.
        // floor_char_boundary is stabilized in Rust 1.79+; we implement manually
        // to stay compatible.
        let mut boundary = 500;
        while boundary > 0 && !body.is_char_boundary(boundary) {
            boundary -= 1;
        }
        boundary
    };

    let truncated = if end < body.len() {
        format!("{}...", &body[..end])
    } else {
        body.to_string()
    };

    // Redact patterns (best-effort — not a security guarantee):
    // - `sk-<hex/alphanumeric>` → `sk-***`
    // - `Bearer <token>` (case-insensitive prefix) → `Bearer ***`
    //
    // We walk bytes and do manual substring matching to avoid pulling in `regex`.
    let bytes = truncated.as_bytes();
    let n = bytes.len();
    let mut out = String::with_capacity(n);
    let mut pos = 0;

    while pos < n {
        // ── Check for `Bearer ` / `bearer ` ──
        let remaining = &truncated[pos..];
        if remaining.len() > 7 {
            let lower = remaining[..7].to_ascii_lowercase();
            if lower == "bearer " {
                // Skip the "Bearer " / "bearer " prefix.
                pos += 7;
                // Skip the token.
                while pos < n && !bytes[pos].is_ascii_whitespace() {
                    pos += 1;
                }
                out.push_str("Bearer ***");
                continue;
            }
        }

        // ── Check for `sk-` followed by alphanumeric ──
        if pos + 3 <= n && bytes[pos] == b's' && bytes[pos + 1] == b'k' && bytes[pos + 2] == b'-' {
            let mut key_end = pos + 3;
            while key_end < n && bytes[key_end].is_ascii_alphanumeric() && key_end - pos <= 67 {
                key_end += 1;
            }
            if key_end > pos + 3 {
                out.push_str("sk-***");
                pos = key_end;
                continue;
            }
        }

        // ── Regular character ──
        let ch = truncated[pos..].chars().next().unwrap();
        out.push(ch);
        pos += ch.len_utf8();
    }

    out
}

/// 构造发往上游的 OpenAI 请求体。
///
/// 两种模式：
/// - 透传（`raw_body` 有值）：以原始请求体为基底，只覆盖网关必须改写的字段
///   （model 别名 → 真实模型名、stream 恒为显式布尔、流式注入 stream_options.include_usage），
///   其余参数原样上行。messages 保留客户端原样（含多模态 content 数组）。
/// - 重建（`raw_body` 为 None，anthropic 转换路径）：从 request 逐字段拼回，
///   可选字段仅在有值时挂上（避免部分上游对 null 敏感），
///   messages 用带 skip_serializing_if 的 ChatMessage 序列化。
///
/// 独立成公共函数是为了让调用方在发送前拿到完整请求体写日志
/// （`log_llm_request` 记录的就是这个 body，与实际发送内容逐字节一致）。
pub fn build_upstream_body(request: &ChatCompletionRequest) -> serde_json::Value {
    // 透传模式：以原始请求体为基底，定点覆盖网关必须改写的字段。
    if let Some(mut raw) = request.raw_body.clone() {
        // raw_body 恒为对象：openai handler 先 400 校验 model/messages 后才构造请求，
        // anthropic_to_openai 也构造对象 passthrough。
        debug_assert!(raw.is_object(), "raw_body 应为 JSON 对象");
        raw["model"] = request.model.clone().into(); // 别名 → 真实模型名
                                                     // stream 必须始终与网关决策一致：客户端省略 stream（或传非布尔值）时，
                                                     // 也保证上游收到显式布尔值，避免透传模式下 stream 变成 null。
                                                     // request.stream 与客户端显式值同源（as_bool().unwrap_or(false)），
                                                     // 显式提供时覆盖为同值（no-op），与重建模式旧行为保持一致。
        raw["stream"] = request.stream.into();
        if request.stream {
            // 幂等注入 include_usage：保留客户端已有 stream_options 字段。
            let so = raw
                .as_object_mut()
                .and_then(|o| o.get_mut("stream_options"))
                .and_then(|v| v.as_object_mut());
            if let Some(so) = so {
                so.insert("include_usage".into(), serde_json::Value::Bool(true));
            } else {
                raw["stream_options"] = serde_json::json!({ "include_usage": true });
            }
        }
        return raw;
    }
    // 重建模式：anthropic 转换路径（raw_body 为 None）。
    let mut req_body = serde_json::json!({
        "model": request.model,
        "messages": request.messages,
        "stream": request.stream,
    });
    if let Some(v) = request.max_tokens {
        req_body["max_tokens"] = v.into();
    }
    if let Some(v) = request.temperature {
        req_body["temperature"] = v.into();
    }
    if let Some(v) = request.top_p {
        req_body["top_p"] = v.into();
    }
    if let Some(tools) = &request.tools {
        req_body["tools"] = serde_json::Value::Array(tools.clone());
    }
    if let Some(choice) = &request.tool_choice {
        req_body["tool_choice"] = choice.clone();
    }
    if request.stream {
        req_body["stream_options"] = serde_json::json!({ "include_usage": true });
    }
    req_body
}

/// Call an upstream LLM provider with OpenAI-compatible format.
/// Supports both streaming (SSE) and non-streaming modes.
pub async fn call_upstream(
    base_url: &str,
    api_key: &str,
    request: &ChatCompletionRequest,
) -> Result<Response, (StatusCode, String)> {
    let req_body = build_upstream_body(request);
    call_upstream_with_body(base_url, api_key, &req_body).await
}

/// 用已构造好的请求体调用上游。
///
/// 调用方（handler）先用 `build_upstream_body` 构造 body、写入完整请求日志，
/// 再走这里发送——保证日志内容与实际发送的请求体一致。
pub async fn call_upstream_with_body(
    base_url: &str,
    api_key: &str,
    req_body: &serde_json::Value,
) -> Result<Response, (StatusCode, String)> {
    let client = &*UPSTREAM_CLIENT;

    let url = format!("{}/v1/chat/completions", base_url.trim_end_matches('/'));

    let req = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(req_body);

    let resp = req.send().await.map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            format!("Upstream connection failed: {}", e),
        )
    })?;

    let status = resp.status();

    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        // Sanitize: strip potential API key from error message
        let sanitized = sanitize_error_message(&body_text);
        // 诊断日志：上游 4xx/5xx 时记录转换后的请求体摘要，便于定位字段兼容问题。
        // 注意脱敏：messages 只记录结构（role/长度/工具字段），不记录正文内容。
        let req_debug = summarize_request_for_log(req_body);
        tracing::warn!(
            target: "llm_upstream",
            status = status.as_u16(),
            url = %url,
            request = %req_debug,
            upstream_error = %sanitized,
            "LLM upstream rejected request"
        );

        // 增强诊断：记录完整请求体（截断到 8KB）到系统日志，用于对比子代理/主代理差异。
        // 脱敏：移除 Authorization 头，但保留请求体的完整内容（包含 messages/tools）。
        let full_req = serde_json::to_string_pretty(req_body).unwrap_or_default();
        let truncated_req = if full_req.len() > 8192 {
            format!(
                "{}...\n[truncated, total {} bytes]",
                &full_req[..8192],
                full_req.len()
            )
        } else {
            full_req
        };
        tracing::warn!(
            target: "llm_upstream_debug",
            status = status.as_u16(),
            model = %req_body.get("model").and_then(|m| m.as_str()).unwrap_or(""),
            stream = req_body.get("stream").and_then(|s| s.as_bool()).unwrap_or(false),
            message_count = req_body.get("messages").and_then(|m| m.as_array()).map_or(0, Vec::len),
            has_tools = req_body.get("tools").is_some(),
            full_request_body = %truncated_req,
            upstream_error_full = %sanitized,
            "LLM upstream 4xx/5xx - full request dump"
        );

        return Err((
            status,
            format!("Upstream error {}: {}", status.as_u16(), sanitized),
        ));
    }

    let is_stream = req_body
        .get("stream")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);
    if is_stream {
        relay_upstream_stream(resp).await
    } else {
        relay_upstream_body(resp).await
    }
}

/// Relay a streaming (SSE) upstream response to the client.
///
/// 当上游连接意外断开（如 LLM 服务商网关 idle timeout），向客户端发送一个
/// OpenAI 风格的 SSE error chunk 再正常关闭流，而不是让 hyper 在中途截断响应
/// （客户端收到 "Connection closed mid-response"）。
async fn relay_upstream_stream(resp: reqwest::Response) -> Result<Response, (StatusCode, String)> {
    let byte_stream = resp.bytes_stream().map(|result| {
        result
            .map(|bytes| bytes.to_vec())
            .map_err(|e| std::io::Error::other(e.to_string()))
    });

    let body = Body::from_stream(byte_stream);
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header("Connection", "keep-alive")
        .body(body)
        .unwrap())
}

/// Relay a non-streaming upstream response body to the client.
async fn relay_upstream_body(resp: reqwest::Response) -> Result<Response, (StatusCode, String)> {
    let body_bytes = resp.bytes().await.map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            format!("Failed to read upstream response: {}", e),
        )
    })?;

    let body = Body::from(body_bytes.to_vec());
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(body)
        .unwrap())
}

/// 判定上游失败是否可转移（换下一个候选重试）。
///
/// 可转移：5xx（含连接/超时映射来的 502）、429（受 `failover_on_429` 开关控制）。
/// 不可转移：其余 4xx（请求本身问题，换模型大概率同样失败）。
pub fn is_retryable(status: StatusCode, failover_on_429: bool) -> bool {
    if status.is_server_error() {
        return true;
    }
    status == StatusCode::TOO_MANY_REQUESTS && failover_on_429
}

/// 从 provider `extra_config` JSON 读 `failover_on_429` 开关（默认 true）。
pub fn failover_on_429_enabled(extra_config: Option<&str>) -> bool {
    let Some(ec) = extra_config else { return true };
    serde_json::from_str::<serde_json::Value>(ec)
        .ok()
        .and_then(|v| v.get("failover_on_429")?.as_bool())
        .unwrap_or(true)
}

/// 定点改写请求体的 model 字段（重试循环内用，其他字段不动）。
pub fn set_body_model(body: &mut serde_json::Value, model: &str) {
    body["model"] = serde_json::Value::String(model.to_string());
}

/// 流式上游调用的首字节守卫。
///
/// 与 `call_upstream_with_body` 的差别：拿到 2xx 响应后不直接 relay，
/// 而是先缓冲到第一个 SSE `data:` 事件（30s 首字节超时），成功再把
/// "已缓冲前缀 + 剩余流"拼成响应体返回；失败按 `(status, msg)` 返回，
/// 供外层故障转移循环判定。
///
/// 适用场景：模型组候选链——确保客户端收到首字节前可以换候选重发。
pub async fn call_upstream_stream_guarded(
    base_url: &str,
    api_key: &str,
    req_body: &serde_json::Value,
) -> Result<Response, (StatusCode, String)> {
    use futures_util::StreamExt;

    let client = &*UPSTREAM_CLIENT;
    let url = format!("{}/v1/chat/completions", base_url.trim_end_matches('/'));
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(req_body)
        .send()
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                format!("Upstream connection failed: {}", e),
            )
        })?;

    let status = resp.status();
    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        let sanitized = sanitize_error_message(&body_text);
        return Err((
            status,
            format!("Upstream error {}: {}", status.as_u16(), sanitized),
        ));
    }

    /// 首事件缓冲上限：SSE 首事件通常 <64KB；超限视为非 SSE/恶意流，走转移。
    const MAX_PREFIX_BYTES: usize = 4 * 1024 * 1024;

    // 缓冲到第一个 SSE data 事件（含跨 chunk 到达的情况），30s 首字节超时。
    let mut stream = resp.bytes_stream();
    let mut prefix: Vec<u8> = Vec::new();
    let first_event_deadline = std::time::Duration::from_secs(30);
    let collect = async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| {
                (
                    StatusCode::BAD_GATEWAY,
                    format!("Upstream stream read failed: {}", e),
                )
            })?;
            prefix.extend_from_slice(&chunk);
            if prefix.len() > MAX_PREFIX_BYTES {
                return Err((
                    StatusCode::BAD_GATEWAY,
                    "Upstream prefix exceeded limit (not an SSE stream?)".to_string(),
                ));
            }
            // 上游 SSE 事件结尾：\n\n（OpenAI 标准）或 \r\n\r\n（CRLF 风格）
            if prefix.windows(4).any(|w| w == b"\r\n\r\n")
                || prefix.windows(2).any(|w| w == b"\n\n")
            {
                return Ok(());
            }
        }
        // 流正常结束但没等到事件——空流也放行（上游立刻 [DONE] 的边界场景）
        Ok(())
    };
    tokio::time::timeout(first_event_deadline, collect)
        .await
        .map_err(|_| {
            (
                StatusCode::GATEWAY_TIMEOUT,
                "Upstream first-byte timeout".to_string(),
            )
        })??;

    // 拼"前缀 replay + 剩余流"的响应体
    let prefix_stream = futures_util::stream::once(async move {
        Ok::<_, std::io::Error>(prefix)
    });
    let rest_stream = stream.map(|r| r.map(|b| b.to_vec()).map_err(|e| std::io::Error::other(e.to_string())));
    let body = Body::from_stream(prefix_stream.chain(rest_stream));
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header("Connection", "keep-alive")
        .body(body)
        .unwrap())
}

/// 候选链执行循环的结果。
///
/// `Success` 同时携带完整的 `Response` 与出账 `Candidate`，体积较大；
/// 接口按 Task 6 brief 约定（非 Box），仅在成功分支 move 一次，非热点，故 allow。
#[allow(clippy::large_enum_variant)]
pub enum FailoverOutcome {
    /// 某候选成功。
    Success {
        /// 上游响应（流式为首字节守卫后的拼接体）。
        resp: Response,
        /// 实际出账的候选（usage 落库以此为准）。
        candidate: crate::server::llm::router::Candidate,
        /// 是否发生过转移（首选不是出账候选）。
        failed_over: bool,
    },
    /// 全部候选失败：返回最后一个被尝试候选的错误。
    Exhausted {
        /// HTTP 状态码（全熔断无尝试时为 503）。
        status: StatusCode,
        /// 错误消息。
        message: String,
        /// 是否尝试过非首选候选。
        failed_over: bool,
    },
}

/// 在候选链上执行上游调用：熔断跳过 + 可转移重试。
///
/// 调用方负责 RAG/compat 改写与 `req_body` 构造；本函数循环内仅
/// clone body + 定点改 `model`。中途失败尝试不记 usage（仅 warn 日志）。
pub async fn execute_with_failover(
    breakers: &crate::server::llm::breaker::ModelBreakers,
    chain: &crate::server::llm::router::CandidateChain,
    req_body: &serde_json::Value,
    stream: bool,
) -> FailoverOutcome {
    let mut attempts = 0usize;
    let mut last_err: Option<(StatusCode, String)> = None;
    let mut last_attempted: Option<&str> = None; // 实际发起过上游调用的候选 model_id

    for cand in &chain.candidates {
        if !breakers.allow(&cand.model_id) {
            tracing::debug!(
                model_id = %cand.model_id,
                model = %cand.model_name,
                "LLM failover: candidate skipped (circuit open)"
            );
            continue;
        }
        attempts += 1;
        last_attempted = Some(cand.model_id.as_str());

        let mut body = req_body.clone();
        set_body_model(&mut body, &cand.model_name);

        // 首字节守卫仅用于有转移目标（链长 >1）的流式请求；
        // 单元素链流式回到 relay 直通（响应头即放行），与改造前行为完全一致，
        // 避免首 token 延迟 >30s 的合法请求（超长 prefill、推理模型）被守卫 504 打断。
        let result = if stream && chain.candidates.len() > 1 {
            call_upstream_stream_guarded(&cand.provider.base_url, &cand.provider.api_key, &body).await
        } else {
            call_upstream_with_body(&cand.provider.base_url, &cand.provider.api_key, &body).await
        };

        match result {
            Ok(resp) => {
                breakers.record_success(&cand.model_id);
                // 按 model_id 判定：组内允许"同 model_name 不同 provider/model_id"冗余候选，
                // 真实 provider 转移必须计为 failed_over（usage 出账归因）。
                let failed_over = chain
                    .candidates
                    .first()
                    .is_some_and(|first| first.model_id != cand.model_id);
                return FailoverOutcome::Success {
                    resp,
                    candidate: cand.clone(),
                    failed_over,
                };
            }
            Err((status, msg)) => {
                let retryable = is_retryable(
                    status,
                    failover_on_429_enabled(cand.provider.extra_config.as_deref()),
                );
                if retryable {
                    tracing::warn!(
                        group = ?chain.group_name,
                        from_model = %cand.model_name,
                        status = status.as_u16(),
                        reason = %msg,
                        "LLM failover: candidate failed, trying next"
                    );
                    breakers.record_failure(&cand.model_id);
                    last_err = Some((status, msg));
                    continue;
                }
                // 不可转移（4xx）：上游可达，计成功；立即终止
                breakers.record_success(&cand.model_id);
                return FailoverOutcome::Exhausted {
                    status,
                    message: msg,
                    // 与 Success 分支同源判定：实际尝试的最后一个候选非首选即转移
                    // （如 A retryable 失败后 B 返回 4xx → last_attempted=B≠首选 → true）
                    failed_over: last_attempted.is_some_and(|mid| {
                        Some(mid) != chain.candidates.first().map(|c| c.model_id.as_str())
                    }),
                };
            }
        }
    }

    match last_err {
        Some((status, message)) => FailoverOutcome::Exhausted {
            status,
            message,
            // 与 Success 分支同源判定：实际尝试的最后一个候选非首选即转移。
            // 首选被熔断跳过（allow=false 不计 attempts）+ 备选失败也计转移，
            // 避免 usage 把 model_name 记为从未尝试的首选、failover_from 归因错误。
            failed_over: last_attempted.is_some_and(|mid| {
                Some(mid) != chain.candidates.first().map(|c| c.model_id.as_str())
            }),
        },
        None => FailoverOutcome::Exhausted {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: if attempts > 0 {
                "all candidates failed".to_string()
            } else {
                "all_candidates_unavailable".to_string()
            },
            failed_over: false,
        },
    }
}

/// 透传原始请求到上游 Anthropic 端点，不做格式转换。
///
/// 认证策略：同时支持 `x-api-key`（Anthropic 原生）和 `Authorization: Bearer`（OpenAI 风格）。
/// 先尝试 `x-api-key`，若返回 401 则回退到 Bearer 头重试。
pub async fn call_upstream_raw(
    base_url: &str,
    api_key: &str,
    path: &str,
    body: &serde_json::Value,
    is_stream: bool,
) -> Result<Response, (StatusCode, String)> {
    let client = &*UPSTREAM_CLIENT;
    let url = format!(
        "{}/{}",
        base_url.trim_end_matches('/'),
        path.trim_start_matches('/')
    );

    // 先用 x-api-key 头尝试
    let resp = client
        .post(&url)
        .header("x-api-key", api_key)
        .header("Content-Type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .json(body)
        .send()
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                format!("Upstream connection failed: {}", e),
            )
        })?;

    // 如果 401，回退到 Bearer 头重试
    let resp = if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        client
            .post(&url)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .header("anthropic-version", "2023-06-01")
            .json(body)
            .send()
            .await
            .map_err(|e| {
                (
                    StatusCode::BAD_GATEWAY,
                    format!("Upstream connection failed: {}", e),
                )
            })?
    } else {
        resp
    };

    let status = resp.status();

    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        let sanitized = sanitize_error_message(&body_text);
        return Err((
            status,
            format!("Upstream error {}: {}", status.as_u16(), sanitized),
        ));
    }

    if is_stream {
        relay_upstream_stream(resp).await
    } else {
        relay_upstream_body(resp).await
    }
}

/// 生成请求体的结构摘要用于诊断日志：保留字段名和值类型/长度，
/// 但 messages 的正文、tools 的 description/parameters 不落地（避免泄露对话内容）。
fn summarize_request_for_log(req_body: &serde_json::Value) -> String {
    use serde_json::json;

    let mut summary = serde_json::Map::new();
    for (k, v) in req_body.as_object().into_iter().flatten() {
        match k.as_str() {
            "messages" => {
                let msgs: Vec<serde_json::Value> = v
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .map(|m| {
                                let role = m
                                    .get("role")
                                    .and_then(|r| r.as_str())
                                    .unwrap_or("?")
                                    .to_string();
                                let content_len = m
                                    .get("content")
                                    .and_then(|c| c.as_str())
                                    .map(str::len)
                                    .unwrap_or(0);
                                let has_tool_calls = m.get("tool_calls").is_some();
                                let tool_call_id = m.get("tool_call_id").is_some();
                                json!({
                                    "role": role,
                                    "content_len": content_len,
                                    "tool_calls": has_tool_calls,
                                    "tool_call_id": tool_call_id,
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                summary.insert(k.clone(), json!(msgs));
            }
            "tools" => {
                // 只记录工具名，不记录 description/parameters
                let names: Vec<&str> = v
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|t| {
                                t.get("function")
                                    .and_then(|f| f.get("name"))
                                    .and_then(|n| n.as_str())
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                summary.insert(k.clone(), json!(names));
            }
            _ => {
                summary.insert(k.clone(), v.clone());
            }
        }
    }
    serde_json::Value::Object(summary).to_string()
}

/// Build an OpenAI-format error response.
pub fn error_response(status: StatusCode, message: String, error_type: &str) -> Response {
    let body = serde_json::json!({
        "error": {
            "message": message,
            "type": error_type,
        }
    });
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

/// Build an Anthropic-format error response for Anthropic-protocol domains.
pub fn error_response_anthropic(status: StatusCode, message: String, error_type: &str) -> Response {
    let body = serde_json::json!({
        "type": "error",
        "error": {
            "type": error_type,
            "message": message,
        }
    });
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

/// Build a 404 "model not found" response that carries the available model list,
/// per spec: 未匹配 → 返回 404，body 中包含可用模型列表。
pub fn model_not_found_response(message: String, available_models: Vec<String>) -> Response {
    let body = serde_json::json!({
        "error": {
            "message": message,
            "type": "invalid_request_error",
            "available_models": available_models,
        }
    });
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_response_contains_openai_format() {
        let resp = error_response(
            StatusCode::UNAUTHORIZED,
            "Invalid key".into(),
            "authentication_error",
        );
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn test_error_response_anthropic_format() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let resp = error_response_anthropic(
            StatusCode::NOT_FOUND,
            "Not found".into(),
            "invalid_request_error",
        );
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body =
            rt.block_on(async { axum::body::to_bytes(resp.into_body(), 1024).await.unwrap() });
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["type"], "error"); // Anthropic top-level type
        assert_eq!(v["error"]["type"], "invalid_request_error");
        assert_eq!(v["error"]["message"], "Not found");
    }

    #[test]
    fn test_call_upstream_passthrough_status_code() {
        // Test that upstream errors pass through the original status code
        // rather than always returning 502 Bad Gateway
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            // Call a URL that returns 404
            let result = call_upstream(
                "http://127.0.0.1:1", // non-existent server
                "test-key",
                &ChatCompletionRequest {
                    model: "test".into(),
                    messages: vec![],
                    stream: false,
                    max_tokens: None,
                    temperature: None,
                    top_p: None,
                    tools: None,
                    tool_choice: None,
                    raw_body: None,
                },
            )
            .await;
            assert!(result.is_err());
            let (status, msg) = result.unwrap_err();
            // Connection refused should be 502
            assert_eq!(status, StatusCode::BAD_GATEWAY);
            // Error message should NOT contain the API key
            assert!(
                !msg.contains("test-key"),
                "API key should not be in error message: {}",
                msg
            );
        });
    }

    #[test]
    fn test_error_message_no_api_key_leak() {
        // Just validate the pattern — error messages about upstream errors
        // should be generic and not echo back the API key
        let msg = "Upstream connection failed: connection refused";
        // Generic error messages should not contain sk- patterns
        assert!(!msg.contains("sk-"));
    }

    // ── sanitize_error_message tests ────────────────────────────

    #[test]
    fn sanitize_short_message_passes_through() {
        let input = "A simple error message";
        let result = sanitize_error_message(input);
        assert_eq!(result, input);
    }

    #[test]
    fn sanitize_truncates_long_message() {
        let long = "x".repeat(1000);
        let result = sanitize_error_message(&long);
        assert!(result.len() <= 510); // 500 chars + "..." + some overhead
        assert!(result.ends_with("..."));
    }

    #[test]
    fn sanitize_truncation_is_utf8_safe() {
        // Build a string where byte 500 falls inside a multi-byte character
        let mut s = "a".repeat(499);
        s.push('\u{4E2D}'); // 3-byte char at position 499
        s.push_str("end");
        let result = sanitize_error_message(&s);
        // Must not panic and must produce valid UTF-8
        assert!(result.ends_with("..."));
        // The 3-byte char at position 499 should either be fully included or excluded
        assert!(std::str::from_utf8(result.as_bytes()).is_ok());
    }

    #[test]
    fn sanitize_redacts_sk_key() {
        let input = "Error: invalid api key sk-abc123def456 for request";
        let result = sanitize_error_message(input);
        assert!(!result.contains("sk-abc123def456"));
        assert!(result.contains("sk-***"));
    }

    #[test]
    fn sanitize_redacts_bearer_token() {
        let input = "Unauthorized: Bearer eyJhbGciOiJIUzI1NiJ9.token.payload";
        let result = sanitize_error_message(input);
        assert!(!result.contains("eyJhbGciOiJIUzI1NiJ9"));
        assert!(result.contains("Bearer ***"));
    }

    #[test]
    fn sanitize_redacts_lowercase_bearer() {
        let input = "unauthorized: bearer abcdefgh12345678";
        let result = sanitize_error_message(input);
        assert!(!result.contains("abcdefgh12345678"));
        assert!(result.contains("Bearer ***"));
    }

    #[test]
    fn sanitize_handles_multiple_keys() {
        let input = "Key sk-aaa111bbb222 and Bearer token123456789 for endpoint";
        let result = sanitize_error_message(input);
        assert!(!result.contains("sk-aaa111bbb222"));
        assert!(!result.contains("token123456789"));
        assert_eq!(
            result.matches("sk-***").count(),
            1,
            "should have one sk-***"
        );
        assert_eq!(
            result.matches("Bearer ***").count(),
            1,
            "should have one Bearer ***"
        );
    }

    #[test]
    fn sanitize_preserves_non_secret_content() {
        let input = "HTTP 502 Bad Gateway: upstream server returned error";
        let result = sanitize_error_message(input);
        assert!(result.contains("Bad Gateway"));
        assert!(result.contains("upstream server"));
    }

    #[test]
    fn sanitize_does_not_false_positive_on_unrelated_text() {
        // Text without any "sk-" or "Bearer" patterns should pass through unchanged.
        let input = "HTTP 502: task description processing failed";
        let result = sanitize_error_message(input);
        assert!(result.contains("task description"));
        assert!(result.contains("processing failed"));
    }

    #[test]
    fn sanitize_sk_without_alphanumeric_suffix_not_redacted() {
        // "sk-" without alphanumeric followers should not be redacted
        let input = "prefix sk- suffix";
        let result = sanitize_error_message(input);
        // "sk-" followed by a space (not alphanumeric) => not redacted
        assert!(result.contains("sk-"));
    }

    #[test]
    fn sanitize_empty_string_ok() {
        assert_eq!(sanitize_error_message(""), "");
    }

    #[test]
    fn sanitize_exactly_500_chars_not_truncated() {
        let s = "x".repeat(500);
        let result = sanitize_error_message(&s);
        assert!(!result.ends_with("..."));
        assert_eq!(result.len(), 500);
    }

    #[test]
    fn build_upstream_body_passthrough_unknown_params() {
        use crate::server::llm::ChatCompletionRequest;
        use crate::server::llm::ChatMessage;
        let raw = serde_json::json!({
            "model": "client-alias",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": false,
            "stop": ["\n\n"],
            "seed": 42,
            "user": "abc",
            "temperature": 0.7,
        });
        let req = ChatCompletionRequest {
            model: "real-model".into(),
            messages: vec![ChatMessage::text("user", "hi")],
            stream: false,
            max_tokens: None,
            temperature: Some(0.7),
            top_p: None,
            tools: None,
            tool_choice: None,
            raw_body: Some(raw),
        };
        let body = build_upstream_body(&req);
        // 未知参数原样保留
        assert_eq!(body["stop"], serde_json::json!(["\n\n"]));
        assert_eq!(body["seed"], 42);
        assert_eq!(body["user"], "abc");
        // model 被别名解析覆盖
        assert_eq!(body["model"], "real-model");
        // 非流式不注入 stream_options
        assert!(body.get("stream_options").is_none());
    }

    #[test]
    fn build_upstream_body_passthrough_omitted_stream_defaults_to_false() {
        use crate::server::llm::ChatCompletionRequest;
        use crate::server::llm::ChatMessage;
        // 客户端省略 stream 字段：透传模式也必须发出显式布尔 false
        // （回归：此前透传原样返回 → stream 变 null，破坏非流式上游语义）。
        let raw = serde_json::json!({
            "model": "client-alias",
            "messages": [{"role": "user", "content": "hi"}],
            "seed": 42,
        });
        let req = ChatCompletionRequest {
            model: "real-model".into(),
            messages: vec![ChatMessage::text("user", "hi")],
            stream: false,
            max_tokens: None,
            temperature: None,
            top_p: None,
            tools: None,
            tool_choice: None,
            raw_body: Some(raw),
        };
        let body = build_upstream_body(&req);
        assert_eq!(body["stream"], false);
        // 未知参数仍原样保留
        assert_eq!(body["seed"], 42);
        // 非流式不注入 stream_options
        assert!(body.get("stream_options").is_none());
    }

    #[test]
    fn build_upstream_body_stream_injects_include_usage_keeps_client_fields() {
        use crate::server::llm::ChatCompletionRequest;
        use crate::server::llm::ChatMessage;
        let raw = serde_json::json!({
            "model": "alias",
            "messages": [],
            "stream": true,
            "stream_options": {"max_tokens": 100},
        });
        let req = ChatCompletionRequest {
            model: "real".into(),
            messages: vec![ChatMessage::text("user", "hi")],
            stream: true,
            max_tokens: None,
            temperature: None,
            top_p: None,
            tools: None,
            tool_choice: None,
            raw_body: Some(raw),
        };
        let body = build_upstream_body(&req);
        let so = body["stream_options"].as_object().unwrap();
        // 客户端已有字段保留
        assert_eq!(so["max_tokens"], 100);
        // include_usage 被网关注入
        assert_eq!(so["include_usage"], true);
    }

    #[test]
    fn build_upstream_body_no_raw_body_rebuilds() {
        use crate::server::llm::ChatCompletionRequest;
        use crate::server::llm::ChatMessage;
        let req = ChatCompletionRequest {
            model: "m".into(),
            messages: vec![ChatMessage::text("user", "hi")],
            stream: false,
            max_tokens: Some(10),
            temperature: None,
            top_p: None,
            tools: None,
            tool_choice: None,
            raw_body: None,
        };
        let body = build_upstream_body(&req);
        assert_eq!(body["model"], "m");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["max_tokens"], 10);
    }

    #[test]
    fn test_is_retryable() {
        use axum::http::StatusCode;
        // 5xx 可转移
        assert!(is_retryable(StatusCode::INTERNAL_SERVER_ERROR, true));
        assert!(is_retryable(StatusCode::BAD_GATEWAY, true));
        assert!(is_retryable(StatusCode::SERVICE_UNAVAILABLE, true));
        // 429 受开关控制
        assert!(is_retryable(StatusCode::TOO_MANY_REQUESTS, true));
        assert!(!is_retryable(StatusCode::TOO_MANY_REQUESTS, false));
        // 其他 4xx 不可转移
        assert!(!is_retryable(StatusCode::BAD_REQUEST, true));
        assert!(!is_retryable(StatusCode::UNAUTHORIZED, true));
        assert!(!is_retryable(StatusCode::NOT_FOUND, true));
        // 2xx 不在此函数语义内（调用方只在失败时调用），但保守返回 false
        assert!(!is_retryable(StatusCode::OK, true));
    }

    #[test]
    fn test_failover_on_429_enabled() {
        // 默认（无 extra_config / 无该 key）为 true
        assert!(failover_on_429_enabled(None));
        assert!(failover_on_429_enabled(Some("{}")));
        assert!(failover_on_429_enabled(Some(r#"{"compat_tool_history":true}"#)));
        // 显式配置
        assert!(failover_on_429_enabled(Some(r#"{"failover_on_429":true}"#)));
        assert!(!failover_on_429_enabled(Some(r#"{"failover_on_429":false}"#)));
        // 非法 JSON 保守默认 true
        assert!(failover_on_429_enabled(Some("not-json")));
    }

    #[test]
    fn test_set_body_model() {
        let mut body = serde_json::json!({"model": "old", "messages": []});
        set_body_model(&mut body, "new-model");
        assert_eq!(body["model"], "new-model");
        assert_eq!(body["messages"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn test_stream_guarded_success_replays_prefix() {
        // 起裸 TCP mock：延迟 50ms 后吐两个 SSE chunk 再关
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = vec![0u8; 8192];
            // 读完请求头+体（简单粗暴：读一次足够测试体量）
            let _ = sock.read(&mut buf).await.unwrap();
            let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\r\n\r\ndata: [DONE]\r\n\r\n";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            sock.write_all(resp.as_bytes()).await.unwrap();
        });

        let req_body = serde_json::json!({"model": "m", "stream": true});
        let resp = call_upstream_stream_guarded(
            &format!("http://{}", addr),
            "k",
            &req_body,
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // 读全量 body：应包含两个 chunk（前缀 replay + 续传）
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains("hel"));
        assert!(text.contains("lo"));
        assert!(text.contains("[DONE]"));
    }

    #[tokio::test]
    async fn test_stream_guarded_connect_failure_retryable() {
        // 绑定后立即丢弃 listener → 端口不可连
        let addr = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap()
        };
        let req_body = serde_json::json!({"model": "m", "stream": true});
        let err = call_upstream_stream_guarded(&format!("http://{}", addr), "k", &req_body)
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_GATEWAY);
        assert!(is_retryable(err.0, true));
    }

    #[tokio::test]
    async fn test_stream_guarded_upstream_500_retryable() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = vec![0u8; 8192];
            let _ = sock.read(&mut buf).await.unwrap();
            let body = "{\"error\":\"boom\"}";
            let resp = format!(
                "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
        });
        let req_body = serde_json::json!({"model": "m", "stream": true});
        let err = call_upstream_stream_guarded(&format!("http://{}", addr), "k", &req_body)
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(is_retryable(err.0, true));
    }

    #[tokio::test]
    async fn test_stream_guarded_crlf_first_event_releases() {
        // 严格回归：mock 在首个 CRLF 事件后保持连接打开 2s 再发 [DONE] 并关闭——
        // 守卫若不能识别 \r\n\r\n，会傻等连接结束（>=2s，走"空流也放行"的 EOF 分支）
        // 才返回，ttfb 断言失败；正确行为 = 首个 CRLF 事件到达即放行（<1s）。
        // 修复前的宽松版（200ms 后关闭连接）会误走 EOF 分支通过，无法捕获 CRLF 回归。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = vec![0u8; 8192];
            let _ = sock.read(&mut buf).await.unwrap();
            // 裸 HTTP/1.1 无 Content-Length + Connection: close：靠连接结束标识 EOF
            let head =
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
            sock.write_all(head.as_bytes()).await.unwrap();
            sock.write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"cr\"}}]}\r\n\r\n")
                .await
                .unwrap();
            sock.flush().await.unwrap();
            // 关键：保持连接打开 2s，守卫应早已放行
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            sock.write_all(b"data: [DONE]\r\n\r\n").await.unwrap();
        });

        let req_body = serde_json::json!({"model": "m", "stream": true});
        let started = std::time::Instant::now();
        let resp = call_upstream_stream_guarded(&format!("http://{}", addr), "k", &req_body)
            .await
            .unwrap();
        let ttfb = started.elapsed();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            ttfb < std::time::Duration::from_secs(1),
            "首事件即放行，实际耗时 {:?}（CRLF 识别失效？）",
            ttfb
        );
        // 剩余流续传不丢字节：前缀已含第一段，续传含 [DONE]
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains("cr"));
        assert!(text.contains("[DONE]"));
    }

    /// 测试辅助：起一个行为可控的 mock 上游（每个连接调 handler 一次）。
    async fn start_behavior_upstream<F>(mut on_conn: F) -> String
    where
        F: FnMut(tokio::net::TcpStream) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
            + Send + 'static,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else { break };
                on_conn(sock).await;
            }
        });
        format!("http://{}", addr)
    }

    /// 快速构造 CandidateChain（测试用）。
    fn test_chain(specs: &[(&str, &str, &str)]) -> crate::server::llm::router::CandidateChain {
        use crate::server::llm::router::{Candidate, CandidateChain};
        use crate::server::llm::ProviderConfig;
        CandidateChain {
            candidates: specs
                .iter()
                .enumerate()
                .map(|(i, (base, model_name, model_id))| Candidate {
                    provider: ProviderConfig {
                        id: format!("p{}", i),
                        name: format!("P{}", i),
                        provider_type: "deepseek".into(),
                        base_url: base.to_string(),
                        api_key: "k".into(),
                        extra_config: None,
                        anthropic_base_url: None,
                        enabled: true,
                        created_at: String::new(),
                        updated_at: String::new(),
                    },
                    model_name: model_name.to_string(),
                    model_id: model_id.to_string(),
                    priority: i as i64,
                })
                .collect(),
            group_name: Some("g".into()),
        }
    }

    fn ok_sse_response_body() -> &'static str {
        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\r\n\r\ndata: [DONE]\r\n\r\n"
    }

    async fn write_http(sock: &mut tokio::net::TcpStream, status: &str, body: &str) {
        use tokio::io::AsyncWriteExt;
        let resp = format!(
            "HTTP/1.1 {}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            status,
            body.len(),
            body
        );
        sock.write_all(resp.as_bytes()).await.unwrap();
    }

    async fn drain_request(sock: &mut tokio::net::TcpStream) {
        use tokio::io::AsyncReadExt;
        let mut buf = vec![0u8; 8192];
        let _ = sock.read(&mut buf).await;
    }

    #[tokio::test]
    async fn test_failover_first_candidate_500_then_success() {
        let bad = start_behavior_upstream(|mut s| Box::pin(async move {
            drain_request(&mut s).await;
            write_http(&mut s, "500 Internal Server Error", "{\"e\":1}").await;
        })).await;
        let good = start_behavior_upstream(|mut s| Box::pin(async move {
            drain_request(&mut s).await;
            write_http(&mut s, "200 OK", ok_sse_response_body()).await;
        })).await;

        let breakers = crate::server::llm::breaker::ModelBreakers::new();
        let chain = test_chain(&[(&bad, "m-bad", "id-bad"), (&good, "m-good", "id-good")]);
        let body = serde_json::json!({"model": "router", "stream": true, "messages": []});

        let out = execute_with_failover(&breakers, &chain, &body, true).await;
        let FailoverOutcome::Success { resp, candidate, failed_over } = out else {
            panic!("expected success");
        };
        assert!(failed_over);
        assert_eq!(candidate.model_id, "id-good");
        assert_eq!(resp.status(), StatusCode::OK);
        // 坏候选被记一次失败
        assert_eq!(breakers.snapshot("id-bad").consecutive_failures, 1);
        // 好候选成功复位
        assert_eq!(breakers.snapshot("id-good").consecutive_failures, 0);
    }

    #[tokio::test]
    async fn test_failover_exhausted_returns_last_error() {
        let bad1 = start_behavior_upstream(|mut s| Box::pin(async move {
            drain_request(&mut s).await;
            write_http(&mut s, "500 Internal Server Error", "{\"e\":1}").await;
        })).await;
        let bad2 = start_behavior_upstream(|mut s| Box::pin(async move {
            drain_request(&mut s).await;
            write_http(&mut s, "503 Service Unavailable", "{\"e\":2}").await;
        })).await;

        let breakers = crate::server::llm::breaker::ModelBreakers::new();
        let chain = test_chain(&[(&bad1, "m1", "id1"), (&bad2, "m2", "id2")]);
        let body = serde_json::json!({"model": "router", "stream": false, "messages": []});

        let out = execute_with_failover(&breakers, &chain, &body, false).await;
        let FailoverOutcome::Exhausted { status, failed_over, .. } = out else {
            panic!("expected exhausted");
        };
        assert!(failed_over);
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "返回最后一个候选的错误");
    }

    #[tokio::test]
    async fn test_failover_400_not_retryable() {
        use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
        let hits = Arc::new(AtomicUsize::new(0));
        let h1 = hits.clone();
        let bad = start_behavior_upstream(move |mut s| {
            let h = h1.clone();
            Box::pin(async move {
                h.fetch_add(1, Ordering::SeqCst);
                drain_request(&mut s).await;
                write_http(&mut s, "400 Bad Request", "{\"e\":\"bad\"}").await;
            })
        }).await;
        let h2 = hits.clone();
        let never = start_behavior_upstream(move |mut s| {
            let h = h2.clone();
            Box::pin(async move {
                h.fetch_add(1, Ordering::SeqCst);
                drain_request(&mut s).await;
                write_http(&mut s, "200 OK", ok_sse_response_body()).await;
            })
        }).await;

        let breakers = crate::server::llm::breaker::ModelBreakers::new();
        let chain = test_chain(&[(&bad, "m1", "id1"), (&never, "m2", "id2")]);
        let body = serde_json::json!({"model": "router", "stream": false, "messages": []});

        let out = execute_with_failover(&breakers, &chain, &body, false).await;
        let FailoverOutcome::Exhausted { status, failed_over, .. } = out else {
            panic!("expected exhausted (400 应立即终止)");
        };
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(!failed_over, "400 不算转移");
        assert_eq!(hits.load(Ordering::SeqCst), 1, "第二个候选不应被请求");
        // 4xx 视为上游健康：不计失败
        assert_eq!(breakers.snapshot("id1").consecutive_failures, 0);
    }

    #[tokio::test]
    async fn test_failover_skips_broken_candidate() {
        use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
        let bad_hits = Arc::new(AtomicUsize::new(0));
        let bh = bad_hits.clone();
        let bad = start_behavior_upstream(move |mut s| {
            let bh = bh.clone();
            Box::pin(async move {
                bh.fetch_add(1, Ordering::SeqCst);
                drain_request(&mut s).await;
                write_http(&mut s, "500 Internal Server Error", "{}").await;
            })
        }).await;
        let good = start_behavior_upstream(|mut s| Box::pin(async move {
            drain_request(&mut s).await;
            write_http(&mut s, "200 OK", ok_sse_response_body()).await;
        })).await;

        let breakers = crate::server::llm::breaker::ModelBreakers::new();
        let chain = test_chain(&[(&bad, "m-bad", "id-bad"), (&good, "m-good", "id-good")]);
        let body = serde_json::json!({"model": "router", "stream": false, "messages": []});

        // 连续 5 次请求把 id-bad 打到熔断
        for _ in 0..5 {
            let _ = execute_with_failover(&breakers, &chain, &body, false).await;
        }
        assert_eq!(bad_hits.load(Ordering::SeqCst), 5);
        assert_eq!(
            breakers.snapshot("id-bad").state,
            crate::server::llm::breaker::BreakerStateView::Open
        );
        // 第 6 次：坏候选被跳过，直接打好候选
        let out = execute_with_failover(&breakers, &chain, &body, false).await;
        assert!(matches!(out, FailoverOutcome::Success { .. }));
        assert_eq!(bad_hits.load(Ordering::SeqCst), 5, "熔断后不再请求坏候选");
    }

    #[tokio::test]
    async fn test_failover_all_candidates_circuit_open() {
        let breakers = crate::server::llm::breaker::ModelBreakers::new();
        let chain = test_chain(&[
            ("http://127.0.0.1:1", "m1", "id1"),
            ("http://127.0.0.1:1", "m2", "id2"),
        ]);
        // 手动把两个候选打到熔断（5 连败）
        for _ in 0..crate::server::llm::breaker::FAILURE_THRESHOLD {
            breakers.record_failure("id1");
            breakers.record_failure("id2");
        }
        let body = serde_json::json!({"model": "router", "stream": false, "messages": []});
        let out = execute_with_failover(&breakers, &chain, &body, false).await;
        let FailoverOutcome::Exhausted { status, message, failed_over } = out else {
            panic!("expected exhausted");
        };
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(message, "all_candidates_unavailable");
        assert!(!failed_over);
    }

    #[tokio::test]
    async fn test_failover_single_candidate_stream_no_guard() {
        // 单元素链流式：不走首字节守卫（旧 relay 行为，响应头即放行）。
        // mock 先发 200 响应头、sleep 2s 才发首个 SSE 事件——若仍走守卫，
        // 会等待首事件 ≥2s 才返回；旧行为应 <1s 拿到 Response。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = vec![0u8; 8192];
            let _ = sock.read(&mut buf).await.unwrap();
            let head =
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
            sock.write_all(head.as_bytes()).await.unwrap();
            sock.flush().await.unwrap();
            // 关键：2s 后才发首事件（守卫在此处才会放行，旧 relay 应立即放行）
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let _ = sock
                .write_all(
                    b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\r\n\r\ndata: [DONE]\r\n\r\n",
                )
                .await;
        });

        let breakers = crate::server::llm::breaker::ModelBreakers::new();
        let chain = test_chain(&[(&format!("http://{}", addr), "m1", "id1")]);
        let body = serde_json::json!({"model": "router", "stream": true, "messages": []});

        let started = std::time::Instant::now();
        let out = execute_with_failover(&breakers, &chain, &body, true).await;
        let ttfb = started.elapsed();
        let FailoverOutcome::Success { resp, failed_over, .. } = out else {
            panic!("expected success");
        };
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(!failed_over);
        assert!(
            ttfb < std::time::Duration::from_secs(1),
            "单元素链流式应响应头即放行，实际耗时 {:?}（仍走首字节守卫？）",
            ttfb
        );
        // 剩余流透传完整（relay 直通，连接未截断）
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains("hi"));
        assert!(text.contains("[DONE]"));
    }

    #[tokio::test]
    async fn test_failover_preferred_broken_backup_fails_marks_transfer() {
        // 首选熔断跳过（allow=false 不计 attempts）+ 备选被尝试且失败 → attempts==1。
        // 修复前 failed_over 用 attempts>1 判定 → false（归因错误，model_name 记为首选）；
        // 修复后按 last_attempted（备选）≠ 首选判定 → true。
        let backup = start_behavior_upstream(|mut s| Box::pin(async move {
            drain_request(&mut s).await;
            write_http(&mut s, "500 Internal Server Error", "{\"e\":1}").await;
        })).await;

        let breakers = crate::server::llm::breaker::ModelBreakers::new();
        // 手动机 5 连败把首选熔断
        for _ in 0..crate::server::llm::breaker::FAILURE_THRESHOLD {
            breakers.record_failure("id-broken");
        }
        let chain = test_chain(&[
            ("http://127.0.0.1:1", "m-broken", "id-broken"),
            (&backup, "m-backup", "id-backup"),
        ]);
        let body = serde_json::json!({"model": "router", "stream": false, "messages": []});

        let out = execute_with_failover(&breakers, &chain, &body, false).await;
        let FailoverOutcome::Exhausted { status, failed_over, .. } = out else {
            panic!("expected exhausted");
        };
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            failed_over,
            "首选熔断跳过+备选失败应记为转移（failed_over=true）"
        );
        // 首选从未被请求（熔断跳过），备选被请求一次
        assert_eq!(
            breakers.snapshot("id-broken").state,
            crate::server::llm::breaker::BreakerStateView::Open
        );
    }

    #[tokio::test]
    async fn test_stream_guarded_prefix_overflow() {
        // mock：200 后持续吐无事件分隔符的字节（每次 1MB 纯文本行，无 \n\n）
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = vec![0u8; 8192];
            let _ = sock.read(&mut buf).await.unwrap();
            let head =
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
            sock.write_all(head.as_bytes()).await.unwrap();
            let chunk = vec![b'x'; 1024 * 1024];
            for _ in 0..8 {
                if sock.write_all(&chunk).await.is_err() {
                    break; // 客户端已断开（守卫返回 Err 后 drop）
                }
            }
        });

        let req_body = serde_json::json!({"model": "m", "stream": true});
        let err = call_upstream_stream_guarded(&format!("http://{}", addr), "k", &req_body)
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_GATEWAY);
        assert!(err.1.contains("prefix") || err.1.contains("limit"));
    }
}
