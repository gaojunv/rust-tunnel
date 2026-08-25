use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use super::router::list_available_models;
use super::{ChatCompletionRequest, ChatMessage, LlmProtocol, LlmState};

/// State for LLM request handlers.
#[derive(Clone)]
pub struct LlmHandlerState {
    pub llm: Arc<LlmState>,
    /// Which protocol matched this request; None means "not set" (shouldn't happen).
    pub protocol: Option<LlmProtocol>,
}

impl LlmHandlerState {
    /// Return an error response in the format appropriate for the matched protocol.
    #[must_use]
    pub fn error_for_protocol(
        &self,
        status: StatusCode,
        message: String,
        error_type: &str,
    ) -> Response {
        match self.protocol {
            Some(LlmProtocol::Anthropic) => {
                super::upstream::error_response_anthropic(status, message, error_type)
            }
            _ => super::upstream::error_response(status, message, error_type),
        }
    }
}

/// GET /v1/models — list available models.
pub async fn handle_list_models(
    State(state): State<LlmHandlerState>,
    headers: HeaderMap,
) -> Response {
    // Validate API key
    if super::auth::authenticate(&state.llm, &headers)
        .await
        .is_none()
    {
        return state.error_for_protocol(
            StatusCode::UNAUTHORIZED,
            "Invalid API key".into(),
            "authentication_error",
        );
    }

    match list_available_models(&state.llm).await {
        Ok(models) => Json(serde_json::json!({
            "object": "list",
            "data": models,
        }))
        .into_response(),
        Err(e) => state.error_for_protocol(StatusCode::INTERNAL_SERVER_ERROR, e, "server_error"),
    }
}

/// POST /v1/chat/completions — chat completion.
pub async fn handle_chat_completions(
    State(state): State<LlmHandlerState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    // Validate API key
    let (api_key_id, api_key_name) =
        match super::pipeline::authenticate_or_reject(&state, &headers, "openai").await {
            Ok(a) => a,
            Err(resp) => return resp,
        };

    // Extract model name
    let model = match super::pipeline::extract_model_or_reject(
        &state,
        &body,
        &api_key_id,
        &api_key_name,
        "openai",
    )
    .await
    {
        Ok(m) => m,
        Err(resp) => return resp,
    };

    // Resolve model → 候选链（模型组故障转移）
    let chain = match super::pipeline::resolve_chain_or_reject(
        &state,
        &model,
        &api_key_id,
        &api_key_name,
        "openai",
    )
    .await
    {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    // 首选候选：provider 级配置（compat 开关）以首选为准——RAG/compat 改写在循环外只做一次
    let first_candidate = chain.candidates[0].clone();
    let provider = first_candidate.provider.clone();
    let actual_model = first_candidate.model_name.clone();
    let model_id = first_candidate.model_id.clone();

    // Build unified request
    let stream = body
        .get("stream")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    // api_key_id_for_rag 需在 api_key_id 稍后 move 进 ctx 之前 clone。
    let api_key_id_for_rag = api_key_id.clone();

    // ── 惰性结构化判断 ──
    // ChatMessage.content 是 Option<String>，多模态 content 数组无法结构化。
    // 因此仅在 RAG 或 compat 需要操作 messages 时才反序列化；否则保留原始
    // Value，由 raw_body 原样透传（build_upstream_body 以 raw_body 为基底）。
    let mut kb_id_for_rag: Option<String> = None;
    if let Some(ref db) = state.llm.db {
        kb_id_for_rag = db
            .rag_get_kb_id_for_api_key(&api_key_id_for_rag)
            .await
            .ok()
            .flatten();
    }
    let compat_enabled =
        super::compat::compat_tool_history_enabled(provider.extra_config.as_deref());
    let need_structured = compat_enabled || kb_id_for_rag.is_some();

    // messages 存在性校验保持无条件（与旧行为一致）；完整反序列化仅 in need_structured。
    if body.get("messages").is_none() {
        // 记录请求错误（缺少 messages）
        if let Some(ref db) = state.llm.db {
            let ctx = super::usage::UsageContext {
                api_key_id: Some(api_key_id.clone()),
                api_key_name: api_key_name.clone(),
                provider_id: Some(provider.id.clone()),
                provider_name: provider.name.clone(),
                model_id: Some(model_id.clone()),
                model_name: actual_model.clone(),
                requested_model: model.clone(),
                protocol: "openai".into(),
                stream,
                rag_chunks_injected: None,
                failover_from: None,
            };
            ctx.record_failure(db, 400, "invalid_request_error", std::time::Instant::now());
        }
        return state.error_for_protocol(
            StatusCode::BAD_REQUEST,
            "messages is required".into(),
            "invalid_request_error",
        );
    }

    let mut request_messages: Vec<ChatMessage> = Vec::new();
    if need_structured {
        match serde_json::from_value(
            body.get("messages")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        ) {
            Ok(m) => request_messages = m,
            Err(e) => {
                // 记录请求解析错误
                if let Some(ref db) = state.llm.db {
                    let ctx = super::usage::UsageContext {
                        api_key_id: Some(api_key_id.clone()),
                        api_key_name: api_key_name.clone(),
                        provider_id: Some(provider.id.clone()),
                        provider_name: provider.name.clone(),
                        model_id: Some(model_id.clone()),
                        model_name: actual_model.clone(),
                        requested_model: model.clone(),
                        protocol: "openai".into(),
                        stream,
                        rag_chunks_injected: None,
                        failover_from: None,
                    };
                    ctx.record_failure(db, 400, "invalid_request_error", std::time::Instant::now());
                }
                return state.error_for_protocol(
                    StatusCode::BAD_REQUEST,
                    format!("invalid messages: {e}"),
                    "invalid_request_error",
                );
            }
        }
    }

    let mut request = ChatCompletionRequest {
        model: actual_model.clone(),
        messages: request_messages,
        stream,
        max_tokens: body
            .get("max_tokens")
            .and_then(serde_json::Value::as_u64)
            .map(|v| v as u32),
        temperature: body
            .get("temperature")
            .and_then(serde_json::Value::as_f64)
            .map(|v| v as f32),
        top_p: body
            .get("top_p")
            .and_then(serde_json::Value::as_f64)
            .map(|v| v as f32),
        // OpenAI 兼容入口：tools / tool_choice 直接透传上游。
        tools: body.get("tools").and_then(|v| v.as_array()).cloned(),
        tool_choice: body.get("tool_choice").cloned(),
        raw_body: Some(body.clone()),
    };

    // 用量采集上下文
    let mut ctx = super::usage::UsageContext {
        api_key_id: Some(api_key_id),
        api_key_name,
        provider_id: Some(provider.id.clone()),
        provider_name: provider.name.clone(),
        model_id: Some(model_id),
        model_name: actual_model,
        requested_model: model,
        protocol: "openai".into(),
        stream,
        rag_chunks_injected: None,
        failover_from: None,
    };
    let started = std::time::Instant::now();
    let db = state.llm.db.clone();

    // RAG 注入 + compat 改写（共享流水线阶段；改写后的 messages 回写 raw_body）。
    super::pipeline::inject_rag_and_compat(
        &state.llm,
        db.as_ref(),
        kb_id_for_rag,
        compat_enabled,
        &mut request,
        &mut ctx,
    )
    .await;

    // 日志统计：message_count 在未结构化时取原始数组长度，has_tools 取 tools 存在性。
    let message_count = if need_structured {
        request.messages.len()
    } else {
        body["messages"].as_array().map_or(0, Vec::len)
    };
    let has_tools = request.tools.is_some();

    let prepared = super::pipeline::PreparedRequest {
        request,
        message_count,
        has_tools,
        compat_enabled,
        // OpenAI 入口无原始 Anthropic body，直通分支永不触发。
        anthropic_body: None,
    };
    super::pipeline::run_execution(
        &state,
        "openai",
        prepared,
        &chain,
        &first_candidate.model_name,
        ctx,
        db,
        started,
        super::pipeline::ResponsePostProcess::None,
    )
    .await
}

/// 非流式响应：从 OpenAI chat.completion body 中解析伪工具调用文本，
/// 还原为结构化 `tool_calls`，让客户端能正常执行工具。
///
/// 只在 compat 模式（`compat_tool_history`）开启时调用。
/// 如果响应中没有伪工具调用格式，原样返回。
pub async fn rewrite_pseudo_tool_calls_in_response(resp: Response) -> Response {
    let (parts, body) = resp.into_parts();
    let bytes = match axum::body::to_bytes(body, super::upstream::MAX_UPSTREAM_BODY_BYTES).await {
        Ok(b) => b,
        Err(e) => {
            // 超限必须返回 502，而非旧行为"透传原始状态码 + 空内容"——否则客户端
            // 拿到 "200 + 空响应"的假象，整段生成结果静默丢失。对齐
            // `format::convert_openai_to_anthropic_response` 的模式。
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .header("Content-Type", "text/plain; charset=utf-8")
                .body(Body::from(format!(
                    "failed to read upstream response (too large or read error): {e}"
                )))
                .unwrap_or_else(|e| {
                    tracing::error!("failed to build error response: {}", e);
                    Response::new(Body::from("failed to read upstream response"))
                });
        }
    };

    let mut json: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return Response::from_parts(parts, Body::from(bytes)),
    };

    // 只处理 OpenAI chat.completion 格式
    let Some(choices) = json.get_mut("choices").and_then(|c| c.as_array_mut()) else {
        return Response::from_parts(parts, Body::from(bytes));
    };

    for choice in choices.iter_mut() {
        let Some(message) = choice.get_mut("message") else {
            continue;
        };
        let Some(content) = message.get("content").and_then(|c| c.as_str()) else {
            continue;
        };

        let mut scanner = super::compat::TagScanner::new();
        let mut events = scanner.push(content);
        events.extend(scanner.finish());

        let mut text_parts: Vec<String> = Vec::new();
        let mut calls: Vec<serde_json::Value> = Vec::new();
        for e in events {
            match e {
                super::compat::ScanEvent::Text(t) => {
                    let t = t.trim();
                    if !t.is_empty() {
                        text_parts.push(t.to_string());
                    }
                }
                super::compat::ScanEvent::ToolCall(v) => calls.push(v),
                super::compat::ScanEvent::Discarded(_) => {}
            }
        }

        if calls.is_empty() {
            // 无工具调用但可能有剥离发生：仅在内容确实变化时回写
            let joined = text_parts.join("\n");
            if joined != content.trim() {
                message["content"] = serde_json::Value::String(joined);
            }
        } else {
            let remaining = text_parts.join("\n");
            if remaining.is_empty() {
                message["content"] = serde_json::Value::Null;
            } else {
                message["content"] = serde_json::Value::String(remaining);
            }
            message["tool_calls"] = serde_json::Value::Array(calls);
            choice["finish_reason"] = serde_json::Value::String("tool_calls".into());
        }
    }

    let new_bytes = serde_json::to_vec(&json).unwrap_or_else(|_| bytes.to_vec());
    Response::from_parts(parts, Body::from(new_bytes))
}

/// 流式响应：增量解析伪工具调用，文本按到达顺序透传，tool_call 完整后注入结构化 chunk。
///
/// 只在 compat 模式开启时调用。内部状态机：
///   上游 chunk → 按 SSE 行提取 delta.content → TagScanner 增量解析
///   Text 事件 → 立即包装为 OpenAI chunk 发出
///   ToolCall 事件 → 暂存
///   finish_reason / [DONE] → 先发暂存的 tool_calls chunk（最后一个带
///   finish_reason="tool_calls"），再发 usage chunk，最后 [DONE]。
pub fn rewrite_pseudo_tool_calls_in_stream(resp: Response) -> Response {
    use bytes::Bytes;
    use futures_util::StreamExt;

    let (parts, body) = resp.into_parts();
    let stream: futures_util::stream::BoxStream<'static, Result<Bytes, axum::Error>> =
        body.into_data_stream().boxed();

    /// 把一段 delta.content 喂给 scanner，把事件序列化为输出 chunk。
    macro_rules! drain_events {
        ($events:expr, $queue:expr, $id:expr, $model:expr, $pending:expr) => {
            for e in $events {
                match e {
                    super::compat::ScanEvent::Text(t) => {
                        if !t.is_empty() {
                            let chunk = serde_json::json!({
                                "id": $id, "model": $model,
                                "choices": [{"index": 0,
                                    "delta": {"content": t},
                                    "finish_reason": null}]
                            });
                            $queue.push_back(Bytes::from(format!("data: {chunk}\n\n")));
                        }
                    }
                    super::compat::ScanEvent::ToolCall(v) => $pending.push(v),
                    super::compat::ScanEvent::Discarded(_) => {}
                }
            }
        };
    }

    /// 发暂存的 tool_calls chunk（最后一个带 finish_reason）。
    macro_rules! flush_calls {
        ($queue:expr, $id:expr, $model:expr, $pending:expr) => {
            if !$pending.is_empty() {
                let calls = std::mem::take($pending);
                let n = calls.len();
                for (i, call) in calls.into_iter().enumerate() {
                    let is_last = i + 1 == n;
                    let chunk = serde_json::json!({
                        "id": $id, "model": $model,
                        "choices": [{"index": 0,
                            "delta": {"tool_calls": [{
                                "index": i,
                                "id": call["id"],
                                "type": "function",
                                "function": call["function"],
                            }]},
                            "finish_reason": if is_last {
                                serde_json::Value::String("tool_calls".into())
                            } else {
                                serde_json::Value::Null
                            }}]
                    });
                    $queue.push_back(Bytes::from(format!("data: {chunk}\n\n")));
                }
            }
        };
    }

    /// unfold 状态机的内部状态：上游流 + 行缓冲 + 增量解析器 + 输出队列。
    struct State {
        stream: futures_util::stream::BoxStream<'static, Result<Bytes, axum::Error>>,
        byte_buf: Vec<u8>,
        scanner: Option<super::compat::TagScanner>,
        id: String,
        model: String,
        pending_calls: Vec<serde_json::Value>,
        usage_chunk: Option<serde_json::Value>,
        saw_finish: bool,
        saw_done: bool,
        finished: bool,
        queue: std::collections::VecDeque<Bytes>,
    }

    let state = State {
        stream,
        byte_buf: Vec::new(),
        scanner: Some(super::compat::TagScanner::new()),
        id: String::new(),
        model: String::new(),
        pending_calls: Vec::new(),
        usage_chunk: None,
        saw_finish: false,
        saw_done: false,
        finished: false,
        queue: std::collections::VecDeque::new(),
    };

    // 逐 chunk 产出：每个 poll 先清空队列，再按需读一个上游 chunk 转换，
    // 队列空且上游未结束时继续读——客户端无需等流结束即可收到增量文本。
    let out = futures_util::stream::unfold(state, |mut st| async move {
        loop {
            // 1) 队列非空：优先产出已转换的 chunk。
            if let Some(b) = st.queue.pop_front() {
                return Some((b, st));
            }
            if st.finished {
                return None;
            }

            // 2) 从上游读一个 chunk，逐行解析填充队列。
            let mut upstream_done = false;
            match st.stream.next().await {
                Some(Ok(bytes)) => {
                    st.byte_buf.extend_from_slice(&bytes);
                    while let Some(pos) = st.byte_buf.iter().position(|&b| b == b'\n') {
                        let line_bytes: Vec<u8> = st.byte_buf.drain(..=pos).collect();
                        let line = String::from_utf8_lossy(
                            line_bytes
                                .strip_suffix(b"\r\n")
                                .or_else(|| line_bytes.strip_suffix(b"\n"))
                                .unwrap_or(&line_bytes),
                        )
                        .into_owned();
                        let Some(payload) = line.strip_prefix("data:") else {
                            continue;
                        };
                        let payload = payload.trim();
                        if payload.is_empty() {
                            continue;
                        }
                        if payload == "[DONE]" {
                            st.saw_done = true;
                            // byte_buf 中 [DONE] 之后仅剩尾随空行（`\n` / `\r\n`），安全丢弃。
                            st.byte_buf.clear();
                            upstream_done = true;
                            break;
                        }
                        let Ok(chunk) = serde_json::from_str::<serde_json::Value>(payload) else {
                            continue;
                        };
                        if st.id.is_empty() {
                            st.id = chunk["id"].as_str().unwrap_or("").to_string();
                            st.model = chunk["model"].as_str().unwrap_or("").to_string();
                        }
                        if chunk.get("usage").is_some_and(serde_json::Value::is_object) {
                            st.usage_chunk = Some(chunk.clone());
                        }
                        if let Some(content) = chunk["choices"][0]["delta"]["content"].as_str() {
                            if let Some(scanner) = st.scanner.as_mut() {
                                drain_events!(
                                    scanner.push(content),
                                    st.queue,
                                    st.id,
                                    st.model,
                                    st.pending_calls
                                );
                            } else {
                                tracing::warn!("scanner unexpectedly None");
                            }
                        }
                        // 上游原生 tool_calls（模型走了结构化路径）：原样透传
                        if chunk["choices"][0]["delta"]["tool_calls"].is_array() {
                            st.queue
                                .push_back(Bytes::from(format!("data: {payload}\n\n")));
                            // 如果原生 tool_calls chunk 也携带 usage，清除 usage_chunk 防止重复
                            if chunk.get("usage").is_some_and(serde_json::Value::is_object) {
                                st.usage_chunk = None;
                            }
                            continue;
                        }
                        if let Some(reason) = chunk["choices"][0]["finish_reason"].as_str() {
                            if st.pending_calls.is_empty() {
                                // 无伪工具调用：原样透传 finish chunk
                                if !st.saw_finish {
                                    st.saw_finish = true;
                                    // 如果 finish chunk 已携带 usage，清除 usage_chunk 防止重复发出
                                    if chunk.get("usage").is_some_and(serde_json::Value::is_object)
                                    {
                                        st.usage_chunk = None;
                                    }
                                    st.queue
                                        .push_back(Bytes::from(format!("data: {payload}\n\n")));
                                }
                            } else {
                                // 有伪工具调用：finish 由 flush_calls 的 tool_calls 收尾承担
                                st.saw_finish = true;
                                let _ = reason;
                            }
                        }
                        // 非 content/non-tool_calls/finish_reason 的 chunk（如 delta: {"role":"assistant"}）
                        // 原样透传。排除仅 usage 的 chunk（由收尾分支统一发出）。
                        if chunk["choices"][0]["delta"]["content"].as_str().is_none()
                            && !chunk["choices"][0]["delta"]["tool_calls"].is_array()
                            && chunk["choices"][0]["finish_reason"].as_str().is_none()
                            && !chunk.get("usage").is_some_and(serde_json::Value::is_object)
                        {
                            st.queue
                                .push_back(Bytes::from(format!("data: {payload}\n\n")));
                        }
                    }
                }
                Some(Err(_)) | None => upstream_done = true,
            }

            // 3) 上游断流/[DONE]：清算 scanner（未闭合标签剥离）→ 冲刷 tool_calls →
            //    usage → [DONE]，全部进队列后由下一轮循环产出。
            if upstream_done {
                // take 后 scanner 移出状态，finish() 消耗局部变量不再有 borrow 冲突
                if let Some(scanner) = st.scanner.take() {
                    drain_events!(
                        scanner.finish(),
                        st.queue,
                        st.id,
                        st.model,
                        st.pending_calls
                    );
                }
                flush_calls!(st.queue, st.id, st.model, &mut st.pending_calls);
                if st.saw_done {
                    // [DONE] 行已在流中：usage 从上游 copy 是完整的，直接转发。
                    if let Some(u) = st.usage_chunk.take() {
                        st.queue.push_back(Bytes::from(format!("data: {u}\n\n")));
                    }
                    st.queue.push_back(Bytes::from_static(b"data: [DONE]\n\n"));
                } else {
                    // 上游断流未发 [DONE]：补发 usage + [DONE]。
                    if !st.saw_finish {
                        if let Some(u) = st.usage_chunk.take() {
                            st.queue.push_back(Bytes::from(format!("data: {u}\n\n")));
                        }
                    }
                    let ends_done = st.queue.back().is_some_and(|b: &Bytes| {
                        std::str::from_utf8(b).is_ok_and(|s| s.ends_with("data: [DONE]\n\n"))
                    });
                    if !ends_done {
                        st.queue.push_back(Bytes::from_static(b"data: [DONE]\n\n"));
                    }
                }
                st.finished = true;
            }
        }
    })
    .map(Ok::<_, std::io::Error>);

    Response::from_parts(parts, Body::from_stream(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header;
    use axum::http::HeaderValue;

    /// 构造带真实临时 DB 的 LlmState，并插入一个启用的 provider+model。
    /// 返回 (state, 有效 API key, _tempdir 守卫)。
    async fn state_with_db() -> (LlmState, String, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = rust_tunnel_persistence::Database::new(tmp.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();

        let pid = uuid::Uuid::new_v4().to_string();
        db.llm_save_provider(
            &pid,
            "DS",
            "deepseek",
            "https://api.deepseek.com",
            "sk-upstream",
            None::<&str>,
            None::<&str>,
            true,
        )
        .await
        .unwrap();
        let mid = uuid::Uuid::new_v4().to_string();
        db.llm_save_model(&mid, &pid, "deepseek-chat", "fast-model", "[]", true, None)
            .await
            .unwrap();

        let (key, hash, prefix) = crate::auth::generate_api_key();
        let kid = uuid::Uuid::new_v4().to_string();
        db.llm_save_api_key(&kid, &hash, &prefix, "test", None)
            .await
            .unwrap();

        (LlmState::new(Some(db), None), key, tmp)
    }

    fn authed_headers(key: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {key}")).unwrap(),
        );
        headers
    }

    /// 构造带 RAG 的 LlmState：真实临时 DB + 真实 VectorStore（tempdir）+ KB + chunk + api key 绑 KB。
    /// `emb_base_url` 由调用点启动的 mock embedding server 提供（返回固定 8 维向量）。
    /// 返回 (state, 有效 API key, _tempdir 守卫)。
    #[cfg(feature = "rag")]
    async fn state_with_rag(emb_base_url: &str) -> (LlmState, String, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = rust_tunnel_persistence::Database::new(tmp.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();

        let pid = uuid::Uuid::new_v4().to_string();
        db.llm_save_provider(
            &pid,
            "DS",
            "deepseek",
            "https://api.deepseek.com",
            "sk-upstream",
            None::<&str>,
            None::<&str>,
            true,
        )
        .await
        .unwrap();
        let mid = uuid::Uuid::new_v4().to_string();
        db.llm_save_model(&mid, &pid, "deepseek-chat", "fast-model", "[]", true, None)
            .await
            .unwrap();

        // 知识库：emb 指向 mock embedding server，维度 8（与 mock 返回一致）
        let kb_id = uuid::Uuid::new_v4().to_string();
        db.rag_create_kb(&rust_tunnel_persistence::rag::RagCreateKbOpts {
            id: kb_id.clone(),
            name: "rag-kb".to_owned(),
            description: String::new(),
            emb_base_url: emb_base_url.to_owned(),
            emb_api_key: "sk-emb".to_owned(),
            emb_model: "emb-model".to_owned(),
            emb_dimension: 8,
            top_k: 5,
            chunk_size: 1000,
            chunk_overlap: 0,
            score_threshold: 0.0,
            enabled: true,
        })
        .await
        .unwrap();

        // 一个分块（向量随后 upsert 进 store）；先建文档（rag_chunks.doc_id 有 FK 约束）
        let chunk_id = uuid::Uuid::new_v4().to_string();
        let doc_id = uuid::Uuid::new_v4().to_string();
        db.rag_create_document(&doc_id, &kb_id, "install.md", "hash", "md")
            .await
            .unwrap();
        db.rag_insert_chunks(&[(
            chunk_id.clone(),
            doc_id.clone(),
            kb_id.clone(),
            0,
            "指南/安装".into(),
            "RAG 知识库测试内容".into(),
            8,
        )])
        .await
        .unwrap();

        // API key 绑定该知识库
        let (key, hash, prefix) = crate::auth::generate_api_key();
        let kid = uuid::Uuid::new_v4().to_string();
        db.llm_save_api_key(&kid, &hash, &prefix, "rag-test", Some(&kb_id))
            .await
            .unwrap();

        // rag_store 指向同一 tempdir（VectorStore 内部再拼 rag/<kb_id>）；直接在 state 上 upsert，
        // 保证整个生命周期只有这一个 VectorStore 实例（避免同目录并发打开两个 EdgeShard）。
        let state = LlmState::new_with_rag(Some(db), None, tmp.path());
        state
            .rag_store
            .upsert(
                &kb_id,
                8,
                vec![crate::rag::store::ChunkPoint {
                    id: chunk_id,
                    vector: vec![0.1f32; 8],
                    doc_id,
                    seq: 0,
                    heading_path: "指南/安装".into(),
                }],
            )
            .await
            .unwrap();

        (state, key, tmp)
    }

    /// RAG 注入端到端：api key 绑 KB → 请求 chat completions → 上游收到的 messages[0]
    /// 是注入的 system 消息且含 `<knowledge_base>`；usage log 记录 rag_chunks_injected=1。
    #[cfg(feature = "rag")]
    #[tokio::test]
    async fn rag_injects_knowledge_base_into_messages() {
        use axum::routing::post;
        use axum::Router;

        // mock embedding server：任意输入返回固定 8 维向量（与 KB 维度一致）
        let emb_app = Router::new().route(
            "/embeddings",
            post(|body: axum::Json<serde_json::Value>| async move {
                let n = body["input"].as_array().map_or(1, std::vec::Vec::len);
                let data: Vec<_> = (0..n)
                    .map(|i| {
                        serde_json::json!({
                            "index": i,
                            "embedding": vec![0.1f32; 8],
                            "object": "embedding"
                        })
                    })
                    .collect();
                axum::Json(serde_json::json!({"object": "list", "data": data}))
            }),
        );
        let emb_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let emb_addr = emb_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(emb_listener, emb_app).await.unwrap();
        });

        // mock upstream LLM：回显请求体，返回一个普通 chat completion
        let captured = std::sync::Arc::new(tokio::sync::Mutex::new(serde_json::Value::Null));
        let captured2 = captured.clone();
        let llm_app = Router::new().route(
            "/v1/chat/completions",
            post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                let captured = captured2.clone();
                async move {
                    *captured.lock().await = body;
                    axum::Json(serde_json::json!({
                        "id": "chatcmpl-rag",
                        "object": "chat.completion",
                        "model": "deepseek-chat",
                        "choices": [{
                            "index": 0,
                            "message": {"role": "assistant", "content": "基于资料的回答"},
                            "finish_reason": "stop"
                        }],
                        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
                    }))
                }
            }),
        );
        let llm_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let llm_addr = llm_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(llm_listener, llm_app).await.unwrap();
        });

        let (state, key, _tmp) = state_with_rag(&format!("http://{emb_addr}")).await;
        let db = state.db.clone().unwrap();

        // provider base_url 指向 mock upstream
        let providers = db.llm_list_providers().await.unwrap();
        db.llm_save_provider(
            &providers[0].id,
            "DS",
            "deepseek",
            &format!("http://{llm_addr}"),
            "sk-upstream",
            None::<&str>,
            None::<&str>,
            true,
        )
        .await
        .unwrap();

        let resp = handle_chat_completions(
            State(LlmHandlerState {
                llm: std::sync::Arc::new(state),
                protocol: None,
            }),
            authed_headers(&key),
            Json(serde_json::json!({
                "model": "deepseek-chat",
                "messages": [{"role": "user", "content": "关于知识库的问题"}]
            })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        // 上游收到的 messages[0] 是注入的 system 消息，含 <knowledge_base>
        let body = captured.lock().await.clone();
        let msgs = body["messages"]
            .as_array()
            .expect("upstream should receive messages");
        assert_eq!(
            msgs[0]["role"], "system",
            "messages[0] 应为注入的 system: {body}"
        );
        let sys = msgs[0]["content"].as_str().expect("system content");
        assert!(
            sys.contains("<knowledge_base>"),
            "system 应含 <knowledge_base>: {sys}"
        );
        assert!(
            sys.contains("RAG 知识库测试内容"),
            "system 应含 chunk 内容: {sys}"
        );

        // usage log 记录 rag_chunks_injected = 1（fire-and-forget 写入，稍等）
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let logs = db
            .llm_query_usage_logs("1970-01-01T00:00:00Z", "2999-01-01T00:00:00Z", 10, 0)
            .await
            .unwrap();
        assert_eq!(logs.len(), 1, "应有一条成功的 usage log");
        assert_eq!(logs[0].rag_chunks_injected, Some(1));
    }

    /// 降级直通端到端：api key **绑定**了 KB，但 KB 的 emb_base_url 不可达 →
    /// 检索降级为空，请求原样透传上游（messages 无 knowledge_base 注入），
    /// usage log 记录成功且 rag_chunks_injected 为 None。验证「RAG 永不阻断会话」。
    #[cfg(feature = "rag")]
    #[tokio::test]
    async fn rag_degrades_to_pass_through_when_embedding_unreachable() {
        use axum::routing::post;
        use axum::Router;

        // mock upstream LLM：回显请求体，返回一个普通 chat completion
        let captured = std::sync::Arc::new(tokio::sync::Mutex::new(serde_json::Value::Null));
        let captured2 = captured.clone();
        let llm_app = Router::new().route(
            "/v1/chat/completions",
            post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                let captured = captured2.clone();
                async move {
                    *captured.lock().await = body;
                    axum::Json(serde_json::json!({
                        "id": "chatcmpl-degrade",
                        "object": "chat.completion",
                        "model": "deepseek-chat",
                        "choices": [{
                            "index": 0,
                            "message": {"role": "assistant", "content": "ok"},
                            "finish_reason": "stop"
                        }],
                        "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8}
                    }))
                }
            }),
        );
        let llm_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let llm_addr = llm_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(llm_listener, llm_app).await.unwrap();
        });

        // KB 的 emb_base_url 指向不可达地址（127.0.0.1:1 → connection refused），
        // api key 仍绑定该 KB → 检索阶段降级为空。
        let (state, key, _tmp) = state_with_rag("http://127.0.0.1:1").await;
        let db = state.db.clone().unwrap();

        let providers = db.llm_list_providers().await.unwrap();
        db.llm_save_provider(
            &providers[0].id,
            "DS",
            "deepseek",
            &format!("http://{llm_addr}"),
            "sk-upstream",
            None::<&str>,
            None::<&str>,
            true,
        )
        .await
        .unwrap();

        let resp = handle_chat_completions(
            State(LlmHandlerState {
                llm: std::sync::Arc::new(state),
                protocol: None,
            }),
            authed_headers(&key),
            Json(serde_json::json!({
                "model": "deepseek-chat",
                "messages": [{"role": "user", "content": "直通问题"}]
            })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        // 上游收到的 messages 无注入：只有原始 user 消息，不含 <knowledge_base>
        let body = captured.lock().await.clone();
        let msgs = body["messages"]
            .as_array()
            .expect("upstream should receive messages");
        assert_eq!(msgs.len(), 1, "降级直通不应注入 system: {body}");
        assert_eq!(msgs[0]["role"], "user");
        assert!(
            !body.to_string().contains("<knowledge_base>"),
            "不应注入 knowledge_base: {body}"
        );

        // usage log：成功记录，rag_chunks_injected 为 None（未注入）
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let logs = db
            .llm_query_usage_logs("1970-01-01T00:00:00Z", "2999-01-01T00:00:00Z", 10, 0)
            .await
            .unwrap();
        assert_eq!(logs.len(), 1, "应有一条成功的 usage log");
        assert_eq!(logs[0].success, 1);
        assert_eq!(
            logs[0].rag_chunks_injected, None,
            "降级直通不应记录注入 chunk 数"
        );
    }

    #[tokio::test]
    async fn test_chat_completions_requires_auth() {
        let (state, _key, _tmp) = state_with_db().await;
        let resp = handle_chat_completions(
            State(LlmHandlerState {
                llm: std::sync::Arc::new(state),
                protocol: None,
            }),
            HeaderMap::new(),
            Json(serde_json::json!({"model": "deepseek-chat", "messages": []})),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_model_not_found_includes_available_models() {
        let (state, key, _tmp) = state_with_db().await;
        let resp = handle_chat_completions(
            State(LlmHandlerState {
                llm: std::sync::Arc::new(state),
                protocol: None,
            }),
            authed_headers(&key),
            Json(serde_json::json!({
                "model": "nonexistent-model",
                "messages": [{"role": "user", "content": "hi"}]
            })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        // spec: 未匹配 → 返回 404，body 中包含可用模型列表
        let available = v["error"]["available_models"]
            .as_array()
            .expect("error should include available_models list");
        assert!(
            available.iter().any(|m| m.as_str() == Some("fast-model")),
            "available_models should contain the alias: {v}"
        );
    }

    #[tokio::test]
    async fn test_list_models_returns_enabled_only() {
        let (state, key, _tmp) = state_with_db().await;
        let resp = handle_list_models(
            State(LlmHandlerState {
                llm: std::sync::Arc::new(state),
                protocol: None,
            }),
            authed_headers(&key),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["object"], "list");
        let data = v["data"].as_array().unwrap();
        assert_eq!(data.len(), 1);
        assert_eq!(data[0]["id"], "fast-model");
        assert_eq!(data[0]["object"], "model");
    }

    #[tokio::test]
    async fn test_x_api_key_header_authenticates() {
        // Anthropic 原生认证方式：x-api-key 头应与 Authorization: Bearer 等效。
        let (state, key, _tmp) = state_with_db().await;
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_str(&key).unwrap());
        let resp = handle_list_models(
            State(LlmHandlerState {
                llm: std::sync::Arc::new(state),
                protocol: None,
            }),
            headers,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ── 失败请求记录到 usage log 的验证 ──────────────────────────

    /// 认证失败必须写入一条 failure 记录到 llm_usage_logs。
    #[tokio::test]
    async fn test_auth_failure_is_logged() {
        let (state, _key, _tmp) = state_with_db().await;
        let db = state.db.clone().unwrap(); // clone before state is consumed

        let resp = handle_chat_completions(
            State(LlmHandlerState {
                llm: std::sync::Arc::new(state),
                protocol: None,
            }),
            HeaderMap::new(),
            Json(serde_json::json!({"model": "deepseek-chat", "messages": []})),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // fire-and-forget 写入是异步的，稍等一下
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let logs = db
            .llm_query_usage_logs("1970-01-01T00:00:00Z", "2999-01-01T00:00:00Z", 10, 0)
            .await
            .unwrap();
        assert_eq!(logs.len(), 1, "认证失败应写入一条 usage log");
        assert_eq!(logs[0].success, 0);
        assert_eq!(logs[0].status_code, 401);
        assert_eq!(logs[0].error_type.as_deref(), Some("authentication_error"));
    }

    /// 模型未找到必须写入一条 failure 记录到 llm_usage_logs。
    #[tokio::test]
    async fn test_model_not_found_is_logged() {
        let (state, key, _tmp) = state_with_db().await;
        let db = state.db.clone().unwrap();

        let resp = handle_chat_completions(
            State(LlmHandlerState {
                llm: std::sync::Arc::new(state),
                protocol: None,
            }),
            authed_headers(&key),
            Json(serde_json::json!({
                "model": "nonexistent-model",
                "messages": [{"role": "user", "content": "hi"}]
            })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let logs = db
            .llm_query_usage_logs("1970-01-01T00:00:00Z", "2999-01-01T00:00:00Z", 10, 0)
            .await
            .unwrap();
        assert_eq!(logs.len(), 1, "模型未找到应写入一条 usage log");
        assert_eq!(logs[0].success, 0);
        assert_eq!(logs[0].status_code, 404);
        assert_eq!(
            logs[0].error_type.as_deref(),
            Some("model_resolution_error")
        );
    }

    /// 上游连接失败（不可达地址）必须写入一条 failure 记录。
    #[tokio::test]
    async fn test_upstream_failure_is_logged() {
        let (state, key, _tmp) = state_with_db().await;
        let db = state.db.clone().unwrap();

        // 把 provider base_url 改为不可达地址，触发上游连接失败 → 502
        let providers = db.llm_list_providers().await.unwrap();
        let pid = &providers[0].id;
        db.llm_save_provider(
            pid,
            "DS",
            "deepseek",
            "http://127.0.0.1:1", // 没人监听的端口
            "sk-upstream",
            None::<&str>,
            None::<&str>,
            true,
        )
        .await
        .unwrap();

        let resp = handle_chat_completions(
            State(LlmHandlerState {
                llm: std::sync::Arc::new(state),
                protocol: None,
            }),
            authed_headers(&key),
            Json(serde_json::json!({
                "model": "deepseek-chat",
                "messages": [{"role": "user", "content": "hi"}]
            })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let logs = db
            .llm_query_usage_logs("1970-01-01T00:00:00Z", "2999-01-01T00:00:00Z", 10, 0)
            .await
            .unwrap();
        assert_eq!(logs.len(), 1, "上游连接失败应写入一条 usage log");
        assert_eq!(logs[0].success, 0);
        assert_eq!(logs[0].status_code, 502);
        assert_eq!(logs[0].error_type.as_deref(), Some("upstream_error"));
        // 路由成功的记录应包含 provider/model 标识
        assert_eq!(logs[0].provider_name, "DS");
        assert_eq!(logs[0].model_name, "deepseek-chat");
    }

    // ── 伪工具调用解析集成测试 ──────────────────────────────────

    #[tokio::test]
    async fn test_rewrite_pseudo_tool_calls_in_response() {
        // 模拟上游返回的 OpenAI chat.completion 响应，content 中包含伪工具调用
        let upstream_body = serde_json::json!({
            "id": "chatcmpl-1",
            "model": "deepseek-chat",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "[调用工具 Bash] {\"command\":\"ls\"}"
                },
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        });
        let resp = Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/json")
            .body(Body::from(serde_json::to_vec(&upstream_body).unwrap()))
            .unwrap();

        let converted = rewrite_pseudo_tool_calls_in_response(resp).await;
        let bytes = axum::body::to_bytes(converted.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        // content 应为 null（只有工具调用，无文本）
        assert!(v["choices"][0]["message"]["content"].is_null());
        // 应有结构化 tool_calls
        let calls = v["choices"][0]["message"]["tool_calls"]
            .as_array()
            .expect("应有 tool_calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "Bash");
        let args: serde_json::Value =
            serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["command"], "ls");
        // finish_reason 应为 tool_calls
        assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
    }

    #[tokio::test]
    async fn test_rewrite_pseudo_tool_calls_overflow_returns_502() {
        // 构造超过 MAX_UPSTREAM_BODY_BYTES 的响应体：to_bytes 超限必须返回 502，
        // 而非旧行为"200 + 空内容"的静默降级。
        let big: Vec<u8> = vec![b'x'; crate::upstream::MAX_UPSTREAM_BODY_BYTES + 1024];
        let resp = Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/json")
            .body(Body::from(big))
            .unwrap();
        let converted = rewrite_pseudo_tool_calls_in_response(resp).await;
        assert_eq!(converted.status(), StatusCode::BAD_GATEWAY);
        // 错误文案对齐 convert_openai_to_anthropic_response：纯文本、含超限说明
        let bytes = axum::body::to_bytes(converted.into_body(), 4096)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            text.contains("failed to read upstream response"),
            "错误文案不符: {text}"
        );
    }

    #[tokio::test]
    async fn test_rewrite_preserves_text_and_tool_calls() {
        let upstream_body = serde_json::json!({
            "id": "chatcmpl-1",
            "model": "deepseek-chat",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "我来执行命令\n[调用工具 Bash] {\"command\":\"ls\"}"
                },
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        });
        let resp = Response::builder()
            .status(StatusCode::OK)
            .body(Body::from(serde_json::to_vec(&upstream_body).unwrap()))
            .unwrap();

        let converted = rewrite_pseudo_tool_calls_in_response(resp).await;
        let bytes = axum::body::to_bytes(converted.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        // 文本部分保留
        assert_eq!(
            v["choices"][0]["message"]["content"].as_str().unwrap(),
            "我来执行命令"
        );
        // 同时有 tool_calls
        assert!(v["choices"][0]["message"]["tool_calls"].is_array());
    }

    #[tokio::test]
    async fn test_rewrite_no_pseudo_tool_calls_passthrough() {
        // 没有伪工具调用时，响应应原样通过
        let upstream_body = serde_json::json!({
            "id": "chatcmpl-1",
            "model": "deepseek-chat",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "普通回复"},
                "finish_reason": "stop"
            }]
        });
        let resp = Response::builder()
            .status(StatusCode::OK)
            .body(Body::from(serde_json::to_vec(&upstream_body).unwrap()))
            .unwrap();

        let converted = rewrite_pseudo_tool_calls_in_response(resp).await;
        let bytes = axum::body::to_bytes(converted.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(
            v["choices"][0]["message"]["content"].as_str().unwrap(),
            "普通回复"
        );
        assert!(v["choices"][0]["message"].get("tool_calls").is_none());
        assert_eq!(v["choices"][0]["finish_reason"], "stop");
    }

    #[tokio::test]
    async fn test_rewrite_multiple_tool_calls() {
        let upstream_body = serde_json::json!({
            "id": "chatcmpl-1",
            "model": "deepseek-chat",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "[调用工具 Bash] {\"command\":\"ls\"}\n[调用工具 Read] {\"path\":\"/tmp\"}"
                },
                "finish_reason": "stop"
            }]
        });
        let resp = Response::builder()
            .status(StatusCode::OK)
            .body(Body::from(serde_json::to_vec(&upstream_body).unwrap()))
            .unwrap();

        let converted = rewrite_pseudo_tool_calls_in_response(resp).await;
        let bytes = axum::body::to_bytes(converted.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        let calls = v["choices"][0]["message"]["tool_calls"]
            .as_array()
            .expect("应有 tool_calls");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["function"]["name"], "Bash");
        assert_eq!(calls[1]["function"]["name"], "Read");
    }

    // ── 流式伪工具调用解析集成测试 ──────────────────────────────

    #[tokio::test]
    async fn test_stream_rewrite_with_tool_calls() {
        // 模拟上游 SSE 流，content 中包含伪工具调用
        let sse_data = concat!(
            "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"[调用工具 Bash] {\\\"command\\\":\\\"ls\\\"}\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let resp = Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/event-stream")
            .body(Body::from(sse_data))
            .unwrap();

        let converted = rewrite_pseudo_tool_calls_in_stream(resp);
        let bytes = axum::body::to_bytes(converted.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();

        // 应包含 tool_calls chunk
        assert!(text.contains("tool_calls"), "应包含 tool_calls: {text}");
        assert!(text.contains("Bash"), "应包含工具名: {text}");
        assert!(text.contains("call_1"), "应包含 call id: {text}");
        // finish_reason 应为 tool_calls
        assert!(
            text.contains("\"finish_reason\":\"tool_calls\""),
            "finish_reason 应为 tool_calls: {text}"
        );
        // 不应包含原始伪工具调用文本
        assert!(
            !text.contains("[调用工具"),
            "不应包含伪工具调用文本: {text}"
        );
    }

    #[tokio::test]
    async fn test_stream_rewrite_no_tool_calls_passthrough() {
        let sse_data = concat!(
            "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"普通文本\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let resp = Response::builder()
            .status(StatusCode::OK)
            .body(Body::from(sse_data))
            .unwrap();

        let converted = rewrite_pseudo_tool_calls_in_stream(resp);
        let bytes = axum::body::to_bytes(converted.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();

        // 应原样透传
        assert!(text.contains("普通文本"), "应包含原始文本: {text}");
        assert!(text.contains("[DONE]"), "应包含 [DONE]: {text}");
    }

    #[tokio::test]
    async fn test_stream_rewrite_mixed_content() {
        let sse_data = concat!(
            "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"我来执行\\n[调用工具 Bash] {\\\"command\\\":\\\"ls\\\"}\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let resp = Response::builder()
            .status(StatusCode::OK)
            .body(Body::from(sse_data))
            .unwrap();

        let converted = rewrite_pseudo_tool_calls_in_stream(resp);
        let bytes = axum::body::to_bytes(converted.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();

        // 应包含剩余文本和 tool_calls
        assert!(text.contains("我来执行"), "应包含剩余文本: {text}");
        assert!(text.contains("tool_calls"), "应包含 tool_calls: {text}");
    }

    /// 回归：compat 流式重写命中伪工具调用时，必须保留上游末尾的 usage chunk，
    /// 否则下游 AnthropicSseTranslator 只能走 `[DONE]` 分支硬编码 output_tokens=0，
    /// 导致 UsageSseScanner 统计的 tokens 全 0。
    #[tokio::test]
    async fn test_stream_rewrite_preserves_usage_chunk() {
        let sse_data = concat!(
            "data: {\"id\":\"c1\",\"model\":\"deepseek-v4-flash\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"[调用工具 Bash] {\\\"command\\\":\\\"ls\\\"}\"},\"finish_reason\":null}],\"usage\":null}\n\n",
            "data: {\"id\":\"c1\",\"model\":\"deepseek-v4-flash\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":87,\"completion_tokens\":16,\"total_tokens\":103,\"prompt_cache_hit_tokens\":60,\"prompt_cache_miss_tokens\":27}}\n\n",
            "data: [DONE]\n\n",
        );
        let resp = Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/event-stream")
            .body(Body::from(sse_data))
            .unwrap();

        let converted = rewrite_pseudo_tool_calls_in_stream(resp);
        let bytes = axum::body::to_bytes(converted.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();

        // 伪工具调用重写仍然生效
        assert!(text.contains("tool_calls"), "应包含 tool_calls: {text}");
        // 上游 usage 必须保留在重写后的流里
        assert!(
            text.contains("\"prompt_tokens\":87"),
            "重写后丢失上游 prompt_tokens: {text}"
        );
        assert!(
            text.contains("\"completion_tokens\":16"),
            "重写后丢失上游 completion_tokens: {text}"
        );

        // 端到端：重写输出经 Anthropic 翻译 + usage 扫描，必须解析出非零 tokens。
        let anthropic_resp = Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/event-stream")
            .body(Body::from(text.clone()))
            .unwrap();
        let anthropic_stream =
            crate::anthropic_handler::convert_openai_stream_to_anthropic_for_test(anthropic_resp);
        let anthropic_bytes = axum::body::to_bytes(anthropic_stream.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let mut scanner = crate::usage::UsageSseScanner::new();
        scanner.push(&anthropic_bytes);
        let u = scanner.finish();
        assert_eq!(u.prompt_tokens, 87, "端到端 prompt_tokens 不应为 0");
        assert_eq!(u.completion_tokens, 16, "端到端 completion_tokens 不应为 0");
    }

    /// Regression: finish chunk carries usage with no pseudo tool calls →
    /// usage must appear exactly once (not duplicated by raw forward + post-loop emit).
    #[tokio::test]
    async fn test_stream_rewrite_usage_not_duplicated_on_finish_chunk() {
        let sse_data = concat!(
            "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\n",
            "data: [DONE]\n\n",
        );
        let resp = Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/event-stream")
            .body(Body::from(sse_data))
            .unwrap();
        let converted = rewrite_pseudo_tool_calls_in_stream(resp);
        let bytes = axum::body::to_bytes(converted.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();

        // usage 应恰好出现一次（无重复）
        let count = text.matches("\"prompt_tokens\"").count();
        assert_eq!(
            count, 1,
            "usage should appear exactly once, got {count} in: {text}"
        );
    }

    // ── v2 增量流式：文本即时透传 + 跨 chunk 标签 ─────────────────

    /// v2 增量流式：文本 chunk 应立即出现在输出（不等流结束）。
    /// 用"读到第一个文本 chunk 时输出已含文本"验证增量性——
    /// 实现上通过逐 chunk 喂入、单 chunk 内检查输出来观察。
    #[tokio::test]
    async fn test_stream_v2_text_flushed_immediately() {
        // 两个 data 行之间夹一个大文本，标签在后一个 chunk：
        // 若实现仍是全流缓冲，单次输出里必然同时包含 [DONE] 之后的内容；
        // 增量实现则文本先行。这里验证最终输出：文本在前、tool_calls 在后、
        // 且不包含标签原文。
        let sse_data = concat!(
            "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"开始执行\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"<tool_call>\\n{\\\"name\\\":\\\"Bash\\\",\\\"arguments\\\":{\\\"command\\\":\\\"ls\\\"}}\\n</tool_call>\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\n",
            "data: [DONE]\n\n",
        );
        let resp = Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/event-stream")
            .body(Body::from(sse_data))
            .unwrap();
        let converted = rewrite_pseudo_tool_calls_in_stream(resp);
        let bytes = axum::body::to_bytes(converted.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains("开始执行"), "文本应透传: {text}");
        assert!(text.contains("tool_calls"), "应有 tool_calls: {text}");
        assert!(text.contains("Bash"), "应有工具名: {text}");
        assert!(!text.contains("tool_call>"), "标签原文不得泄漏: {text}");
        assert!(
            text.contains("\"prompt_tokens\":10"),
            "usage 应保留: {text}"
        );
        assert!(text.contains("[DONE]"), "[DONE] 应保留: {text}");
        // finish_reason 应是 tool_calls（有工具调用的收尾）
        assert!(text.contains("\"finish_reason\":\"tool_calls\""), "{text}");
    }

    /// v2 增量流式：<tool_call> 起始标签被切到两个网络 chunk。
    #[tokio::test]
    async fn test_stream_v2_tag_split_across_network_chunks() {
        let sse_data = concat!(
            "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"<to\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ol_call>{\\\"name\\\":\\\"A\\\",\\\"arguments\\\":{}}</tool_call>\"},\"finish_reason\":null}]}\n\n",
            "data: [DONE]\n\n",
        );
        let resp = Response::builder()
            .status(StatusCode::OK)
            .body(Body::from(sse_data))
            .unwrap();
        let converted = rewrite_pseudo_tool_calls_in_stream(resp);
        let bytes = axum::body::to_bytes(converted.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains("tool_calls"), "跨 chunk 标签应解析: {text}");
        assert!(!text.contains("<to"), "半个标签不得泄漏: {text}");
    }

    /// v2 增量流式：坏 JSON 剥离且不泄漏。
    #[tokio::test]
    async fn test_stream_v2_broken_json_stripped() {
        let sse_data = concat!(
            "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"前文<tool_call>{bad</tool_call>后文\"},\"finish_reason\":null}]}\n\n",
            "data: [DONE]\n\n",
        );
        let resp = Response::builder()
            .status(StatusCode::OK)
            .body(Body::from(sse_data))
            .unwrap();
        let converted = rewrite_pseudo_tool_calls_in_stream(resp);
        let bytes = axum::body::to_bytes(converted.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains("前文"), "{text}");
        assert!(text.contains("后文"), "{text}");
        assert!(!text.contains("bad"), "坏 JSON 不得泄漏: {text}");
        assert!(!text.contains("tool_call>"), "标签不得泄漏: {text}");
    }

    /// v2 流式 UTF-8 安全：多字节字符（中文 3 字节、emoji 4 字节）被从字符中间
    /// 切到两个网络块时，byte_buf 按行缓冲不得物化 U+FFFD 替换符。
    #[tokio::test]
    async fn test_stream_v2_multibyte_utf8_split_across_chunks() {
        use futures_util::stream;

        let sse_data = concat!(
            "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"你好👋\"},\"finish_reason\":null}]}\n\n",
            "data: [DONE]\n\n",
        );
        let bytes = sse_data.as_bytes();

        // 逐字节边界切碎喂入（最严苛的切法），每个切点都不得产生乱码
        for i in 1..bytes.len() {
            let chunks: Vec<Result<bytes::Bytes, std::io::Error>> = vec![
                Ok(bytes::Bytes::copy_from_slice(&bytes[..i])),
                Ok(bytes::Bytes::copy_from_slice(&bytes[i..])),
            ];
            let body = Body::from_stream(stream::iter(chunks));
            let resp = Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "text/event-stream")
                .body(body)
                .unwrap();
            let converted = rewrite_pseudo_tool_calls_in_stream(resp);
            let out = axum::body::to_bytes(converted.into_body(), 1024 * 1024)
                .await
                .unwrap();
            let text = String::from_utf8(out.to_vec())
                .unwrap_or_else(|e| panic!("split at byte {i} produced invalid utf8: {e}"));
            assert!(
                !text.contains('\u{FFFD}'),
                "split at byte {i} produced replacement char: {text}"
            );
            assert!(
                text.contains("你好👋"),
                "split at byte {i} lost content: {text}"
            );
        }
    }

    /// v2 增量流式真正流式化：上游未发 [DONE] 时，客户端消费第一个输出 chunk
    /// 即应收到已解析的文本。旧实现全量缓冲（out: String），流结束后才一次性
    /// 返回，客户端完全失去增量；此测试用"只发首 chunk、先不结束上游"验证即时性。
    #[tokio::test]
    async fn test_stream_rewrite_is_incremental() {
        use futures_util::StreamExt;
        use tokio::sync::mpsc;

        // 构造"永不发完"的上游流：首 chunk 含文本，[DONE] 由测试主动补发。
        let (tx, rx) = mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(8);
        let body = Body::from_stream(futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        }));
        let resp = Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/event-stream")
            .body(body)
            .unwrap();

        // 只发第一个 chunk（含即时文本），先不发 [DONE]
        tx.send(Ok(bytes::Bytes::copy_from_slice(
            "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"即时文本\"},\"finish_reason\":null}]}\n\n"
                .as_bytes(),
        )))
        .await
        .unwrap();

        let converted = rewrite_pseudo_tool_calls_in_stream(resp);
        let mut out = converted.into_body().into_data_stream();
        // 读取第一个输出 chunk：此时上游流仍打开，应已收到文本 → 真流式证明
        let first = out.next().await.expect("首 chunk 应有输出").unwrap();
        let text = String::from_utf8(first.to_vec()).unwrap();
        assert!(
            text.contains("即时文本"),
            "上游未结束时首 chunk 应含文本（真流式）: {text}"
        );

        // 补发 [DONE] 并关闭上游，流应正常结束
        tx.send(Ok(bytes::Bytes::from_static(b"data: [DONE]\n\n")))
            .await
            .unwrap();
        drop(tx);
        let mut rest = String::new();
        while let Some(item) = out.next().await {
            rest.push_str(core::str::from_utf8(&item.unwrap()).unwrap());
        }
        assert!(rest.contains("[DONE]"), "流应正常收尾: {rest}");
    }

    /// v2 非流式：新标签格式还原结构化 tool_calls。
    #[tokio::test]
    async fn test_nonstream_v2_tag_parsed() {
        let upstream_body = serde_json::json!({
            "id": "chatcmpl-1", "model": "m",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant",
                    "content": "看下\n<tool_call>\n{\"name\":\"Bash\",\"arguments\":{\"command\":\"ls\"}}\n</tool_call>"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3}
        });
        let resp = Response::builder()
            .status(StatusCode::OK)
            .body(Body::from(serde_json::to_vec(&upstream_body).unwrap()))
            .unwrap();
        let converted = rewrite_pseudo_tool_calls_in_response(resp).await;
        let bytes = axum::body::to_bytes(converted.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            v["choices"][0]["message"]["content"].as_str().unwrap(),
            "看下"
        );
        let calls = v["choices"][0]["message"]["tool_calls"].as_array().unwrap();
        assert_eq!(calls[0]["function"]["name"], "Bash");
        assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
    }

    /// v2 非流式：坏 JSON 剥离，content 中无残留。
    #[tokio::test]
    async fn test_nonstream_v2_broken_stripped() {
        let upstream_body = serde_json::json!({
            "id": "x", "model": "m",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant",
                    "content": "正常<tool_call>{oops</tool_call>结尾"},
                "finish_reason": "stop"
            }]
        });
        let resp = Response::builder()
            .status(StatusCode::OK)
            .body(Body::from(serde_json::to_vec(&upstream_body).unwrap()))
            .unwrap();
        let converted = rewrite_pseudo_tool_calls_in_response(resp).await;
        let bytes = axum::body::to_bytes(converted.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let c = v["choices"][0]["message"]["content"].as_str().unwrap();
        assert!(c.contains("正常"), "{c}");
        assert!(c.contains("结尾"), "{c}");
        assert!(!c.contains("oops"), "{c}");
        assert!(!c.contains("tool_call"), "{c}");
    }

    /// 回归：未启用 RAG/compat 时，多模态 content 数组（image_url 等）不结构化，
    /// raw_body 透传基底保持 messages 原样上行——不再因 ChatMessage.content 是
    /// Option<String> 而 400 拒绝。
    #[test]
    fn multimodal_content_array_passes_through_when_no_rag_compat() {
        // 模块顶部 `use super::*` 已引入 ChatMessage（此处 struct 字面量经类型推断使用）。
        use crate::ChatCompletionRequest;
        let raw = serde_json::json!({
            "model": "alias",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "what is this?"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
            ]}],
            "stream": false,
        });
        let req = ChatCompletionRequest {
            model: "real".into(),
            messages: vec![], // 未结构化：多模态时 request.messages 为空
            stream: false,
            max_tokens: None,
            temperature: None,
            top_p: None,
            tools: None,
            tool_choice: None,
            raw_body: Some(raw),
        };
        let body = crate::upstream::build_upstream_body(&req);
        // messages 保持客户端原样的数组（content 为数组）
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["content"][0]["type"], "text");
        assert_eq!(msgs[0]["content"][1]["type"], "image_url");
    }
}
