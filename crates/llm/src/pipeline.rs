//! LLM 网关双协议入口的共享请求流水线。
//!
//! `openai_handler::handle_chat_completions` 与 `anthropic_handler::handle_messages`
//! 曾各自实现整条请求链路：认证、模型路由、用量上下文、RAG/compat 改写、
//! 上游故障转移执行与结果处理——两处平行代码改一处忘另一处是真实风险。
//!
//! 本模块把公共阶段下沉为可复用函数，两个 handler 只保留协议特有的请求解析
//! （OpenAI 透传 / Anthropic→OpenAI 转换、Anthropic 直通判定）与响应封装
//! （OpenAI 原样透传 / Anthropic 格式转换回传）。

use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use serde_json::Value;

use super::openai_handler::LlmHandlerState;
use super::router::{resolve_with_failover, CandidateChain};
use super::usage::UsageContext;
#[cfg(feature = "rag")]
use super::ChatMessage;
use super::LogLlmRequestOpts;
use super::{ChatCompletionRequest, LlmState};
use rust_tunnel_persistence::Database;

/// 取最后一条 user 消息的文本（RAG 检索的 query 来源）。
#[cfg(feature = "rag")]
fn last_user_text(messages: &[ChatMessage]) -> Option<String> {
    messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .and_then(|m| m.content.clone())
        .filter(|t| !t.trim().is_empty())
}

/// 协议特有解析完成后的统一请求描述，供共享执行流水线消费。
pub struct PreparedRequest {
    /// 统一请求（model 已解析为上游真实名）。
    pub request: ChatCompletionRequest,
    /// 请求日志用 message_count（openai 未结构化时取原始数组长度，anthropic 恒为结构化长度）。
    pub message_count: usize,
    /// 请求日志用 has_tools。
    pub has_tools: bool,
    /// compat 开关（provider extra_config 决定）。
    pub compat_enabled: bool,
    /// 原始 Anthropic 请求体（仅 anthropic 入口设置；openai/responses 入口为 None）。
    /// 配了 `anthropic_base_url` 的候选用它直发 `/v1/messages`（model 替换为候选名）。
    pub anthropic_body: Option<serde_json::Value>,
}

/// 认证网关 API key；失败时记录用量日志并返回 401 响应。
///
/// # Errors
/// `auth::authenticate` 返回 `None`（缺失或无效的 API key）时返回 `Err(Response)`，状态码 401。
pub async fn authenticate_or_reject(
    state: &LlmHandlerState,
    headers: &HeaderMap,
    protocol: &str,
) -> Result<(String, String), Response> {
    if let Some(a) = super::auth::authenticate(&state.llm, headers).await {
        Ok(a)
    } else {
        // 记录认证失败
        if let Some(ref db) = state.llm.db {
            let ctx = UsageContext {
                protocol: protocol.into(),
                ..Default::default()
            };
            ctx.record_failure(db, 401, "authentication_error", std::time::Instant::now());
        }
        Err(state.error_for_protocol(
            StatusCode::UNAUTHORIZED,
            "Invalid API key".into(),
            "authentication_error",
        ))
    }
}

/// 提取请求体中的 model 字段；缺失时记录用量日志并返回 400 响应。
///
/// # Errors
/// 请求体缺少 `model` 字段或其值非字符串时返回 `Err(Response)`，状态码 400。
#[allow(
    clippy::unused_async,
    reason = "与 authenticate/resolve 同为 async 流水线，保持调用端 await 统一"
)]
pub async fn extract_model_or_reject(
    state: &LlmHandlerState,
    body: &Value,
    api_key_id: &str,
    api_key_name: &str,
    protocol: &str,
) -> Result<String, Response> {
    if let Some(m) = body.get("model").and_then(Value::as_str) {
        Ok(m.to_string())
    } else {
        // 记录请求错误（缺少 model）
        if let Some(ref db) = state.llm.db {
            let ctx = UsageContext {
                api_key_id: Some(api_key_id.to_string()),
                api_key_name: api_key_name.to_string(),
                protocol: protocol.into(),
                ..Default::default()
            };
            ctx.record_failure(db, 400, "invalid_request_error", std::time::Instant::now());
        }
        Err(state.error_for_protocol(
            StatusCode::BAD_REQUEST,
            "model is required".into(),
            "invalid_request_error",
        ))
    }
}

/// 解析模型路由（模型名/别名/模型组 → 候选链）；失败时记录用量日志并返回 404 响应。
///
/// # Errors
/// `resolve_with_failover` 未找到匹配模型/别名/模型组时返回 `Err(Response)`，状态码 404。
pub async fn resolve_chain_or_reject(
    state: &LlmHandlerState,
    model: &str,
    api_key_id: &str,
    api_key_name: &str,
    protocol: &str,
) -> Result<CandidateChain, Response> {
    match resolve_with_failover(&state.llm, model).await {
        Ok(chain) => Ok(chain),
        Err(e) => {
            // 记录路由失败到用量日志
            if let Some(ref db) = state.llm.db {
                let ctx = UsageContext {
                    api_key_id: Some(api_key_id.to_string()),
                    api_key_name: api_key_name.to_string(),
                    requested_model: model.to_string(),
                    protocol: protocol.into(),
                    ..Default::default()
                };
                ctx.record_failure(db, 404, "model_resolution_error", std::time::Instant::now());
            }
            Err(super::router::resolve_error_response(&state.llm, e).await)
        }
    }
}

/// RAG 知识库注入 + compat 工具历史改写（原地修改 request，注入 chunk 数写回 ctx）。
///
/// 与旧行为保持严格一致：
/// - RAG 仅在 `rag` feature 下生效；检索失败/无命中时静默降级（`enhance` 永不 Err）。
/// - compat 改写仅在 provider 开启 `compat_tool_history` 时生效。
/// - 两类改写后都把结构化 messages 回写到 raw_body（透传基底的 messages 必须同步）。
#[cfg_attr(
    not(feature = "rag"),
    allow(
        unused_variables,
        clippy::unused_async,
        reason = "唯一的 await 在 rag 门控块内；签名必须保持 async 使调用方在两种构型下写法一致"
    )
)]
pub async fn inject_rag_and_compat(
    state: &LlmState,
    db: Option<&Database>,
    kb_id_for_rag: Option<String>,
    compat_enabled: bool,
    request: &mut ChatCompletionRequest,
    ctx: &mut UsageContext,
) {
    #[allow(unused_mut, reason = "assigned only inside the rag-gated inject block")]
    let mut rag_injected: i64 = 0;
    #[cfg(feature = "rag")]
    if let (Some(db), Some(kb_id)) = (db, kb_id_for_rag) {
        // rag 只返回结构化检索结果；注入本请求类型（system 消息置顶）由本层负责。
        if let Some(query_text) = last_user_text(&request.messages) {
            if let Some(rag_ctx) = super::rag::retrieve_context(
                db,
                &state.rag_store,
                state.cipher.as_ref(),
                &kb_id,
                &query_text,
            )
            .await
            {
                rag_injected = i64::try_from(rag_ctx.chunks.len()).unwrap_or(i64::MAX);
                request
                    .messages
                    .insert(0, ChatMessage::text("system", rag_ctx.system_message));
            }
        }
    }
    if rag_injected > 0 {
        ctx.rag_chunks_injected = Some(rag_injected);
        write_back_messages(request);
    }

    if compat_enabled {
        super::compat::rewrite_tool_history(&mut request.messages);
        super::compat::inject_tool_call_guidance(&mut request.messages);
        write_back_messages(request);
    }
}

/// 把改写后的结构化 messages 回写到 raw_body 透传基底。
fn write_back_messages(request: &mut ChatCompletionRequest) {
    if let Some(raw) = request.raw_body.as_mut() {
        if let Ok(v) = serde_json::to_value(&request.messages) {
            raw["messages"] = v;
        }
    }
}

/// 上游成功响应后处理（协议特有：不同入口需要不同的响应格式转换）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponsePostProcess {
    /// OpenAI 入口：响应原样透传。
    None,
    /// Anthropic 入口：把上游 OpenAI 格式转成 Anthropic Messages 格式。
    ToAnthropic,
    /// Responses 入口：把上游 OpenAI chat 格式转成 Responses API 格式。
    ToResponses,
}

/// 构造上行请求体 → 写请求日志 → 候选链故障转移执行 → 结果处理（成功/全部失败）。
///
/// 调用方负责协议特有的请求解析与 RAG/compat 改写（`PreparedRequest` 已含最终内容），
/// 本函数只做与协议无关的执行与出账。
#[allow(
    clippy::too_many_lines,
    reason = "请求执行编排：日志、故障转移、成功/失败双分支与出账，顺序流程不宜拆分"
)]
#[allow(
    clippy::too_many_arguments,
    reason = "内部执行函数，混合基础设施参数（state/chain/ctx/db）"
)]
pub async fn run_execution(
    state: &LlmHandlerState,
    protocol: &'static str,
    prepared: PreparedRequest,
    chain: &CandidateChain,
    first_candidate_model_name: &str,
    mut ctx: UsageContext,
    db: Option<Database>,
    started: std::time::Instant,
    post_process: ResponsePostProcess,
) -> Response {
    let request = &prepared.request;
    let message_count = prepared.message_count;
    let has_tools = prepared.has_tools;
    let compat_enabled = prepared.compat_enabled;

    // 构造两个上游请求体（协议解析 + RAG/compat 改写后的最终内容）：
    // - openai_body：转换路径请求体，候选走 v1/chat/completions（或 v1/responses）。
    // - anthropic_body：原始 Anthropic 请求体，配 anthropic_base_url 的候选直发 /v1/messages。
    //
    // 发送策略由 execute_with_failover 每候选独立判定（配 anthropic_base_url → 直发原始
    // Anthropic body；否则用 openai_body 打 OpenAI 端点）。此处把两个 body 都传下去，
    // 绝不能让"链上存在直通候选"把 openai_body 整个换掉——否则无 anthropic_base_url 的
    // 候选会拿着 Anthropic 格式 body 打 v1/chat/completions（上游 400，用户实证的根因）。
    //
    // 请求日志记录"首选候选实际发送的内容"（混合链下与真实发送一致）：
    // 首选配 anthropic_base_url 且入口带原始 body → 记录原始 Anthropic body（model 覆盖为
    // 首选候选真实名）；否则记录转换路径的 openai_body。
    let openai_body = super::upstream::build_upstream_body(request);
    let first_is_direct_anthropic = prepared.anthropic_body.is_some()
        && chain
            .candidates
            .first()
            .is_some_and(|c| c.provider.anthropic_base_url.is_some());
    let log_body = if first_is_direct_anthropic {
        let Some(mut raw) = prepared.anthropic_body.clone() else {
            tracing::error!("anthropic_body expected but missing");
            return state.error_for_protocol(
                StatusCode::INTERNAL_SERVER_ERROR,
                "anthropic_body missing".into(),
                "server_error",
            );
        };
        raw["model"] = request.model.clone().into();
        raw
    } else {
        openai_body.clone()
    };
    super::log_llm_request(
        &state.llm,
        &LogLlmRequestOpts {
            protocol: protocol.to_owned(),
            model: request.model.clone(),
            message_count,
            has_tools,
            stream: request.stream,
            status: None,
            error: None,
            elapsed_ms: 0,
            request_body: log_body.clone(),
        },
    )
    .await;
    let outcome = super::upstream::execute_with_failover(
        &state.llm.upstream_client,
        &state.llm.breakers,
        &state.llm.known_failures,
        chain,
        &openai_body,
        request.stream,
        prepared.anthropic_body.as_ref(),
    )
    .await;
    match outcome {
        super::upstream::FailoverOutcome::Success {
            resp,
            candidate,
            failed_over,
            upstream_anthropic,
        } => {
            // 出账候选与首选不同：改写 ctx 为实际出账方，并记录转移来源
            if failed_over {
                ctx.provider_id = Some(candidate.provider.id.clone());
                ctx.provider_name = candidate.provider.name.clone();
                ctx.model_id = Some(candidate.model_id.clone());
                ctx.model_name = candidate.model_name.clone();
                ctx.failover_from = Some(first_candidate_model_name.to_string());
            }
            let elapsed_ms = started.elapsed().as_millis();
            super::log_llm_request(
                &state.llm,
                &LogLlmRequestOpts {
                    protocol: protocol.to_owned(),
                    model: ctx.model_name.clone(),
                    message_count,
                    has_tools,
                    stream: request.stream,
                    status: Some(200),
                    error: None,
                    elapsed_ms,
                    request_body: serde_json::Value::Null,
                },
            )
            .await;
            // 直通成功：响应已是 Anthropic 格式，跳过 compat 伪工具重写与
            // OpenAI→Anthropic 转换（post_process），直接出账。
            if upstream_anthropic {
                return super::usage::wrap_and_record(resp, ctx, db, started).await;
            }
            // compat 模式：先解析伪工具调用还原为结构化 tool_calls，再做协议后处理。
            let resp = if compat_enabled {
                if request.stream {
                    // 真流式：返回 Body::from_stream，客户端即时收到增量文本
                    super::openai_handler::rewrite_pseudo_tool_calls_in_stream(resp)
                } else {
                    super::openai_handler::rewrite_pseudo_tool_calls_in_response(resp).await
                }
            } else {
                resp
            };
            let resp = match post_process {
                ResponsePostProcess::None => resp,
                ResponsePostProcess::ToAnthropic if request.stream => {
                    super::format::convert_openai_stream_to_anthropic(resp)
                }
                ResponsePostProcess::ToAnthropic => {
                    super::format::convert_openai_to_anthropic_response(resp).await
                }
                ResponsePostProcess::ToResponses if request.stream => {
                    super::responses::convert_openai_stream_to_responses(resp)
                }
                ResponsePostProcess::ToResponses => {
                    super::responses::convert_openai_to_responses_response(resp).await
                }
            };
            super::usage::wrap_and_record(resp, ctx, db, started).await
        }
        super::upstream::FailoverOutcome::Exhausted {
            status,
            message: msg,
            failed_over,
        } => {
            let elapsed_ms = started.elapsed().as_millis();
            super::log_llm_request(
                &state.llm,
                &LogLlmRequestOpts {
                    protocol: protocol.to_owned(),
                    model: request.model.clone(),
                    message_count,
                    has_tools,
                    stream: request.stream,
                    status: Some(status.as_u16()),
                    error: Some(msg.clone()),
                    elapsed_ms,
                    request_body: serde_json::Value::Null,
                },
            )
            .await;
            // 记录失败请求到用量日志，确保请求明细中可见
            if let Some(ref db) = db {
                // 全部候选失败但实际尝试过转移：failover_from 记首选（被跳过的）模型名
                if failed_over {
                    ctx.failover_from = Some(first_candidate_model_name.to_string());
                }
                ctx.record_failure(db, i32::from(status.as_u16()), "upstream_error", started);
            }
            state.error_for_protocol(status, msg, "upstream_error")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openai_handler::LlmHandlerState;
    use crate::{ChatCompletionRequest, ChatMessage, LlmProtocol, LlmState};
    use axum::http::{HeaderMap, HeaderValue, StatusCode};
    use serde_json::json;
    use std::sync::Arc;

    /// 本地内存库 fixture（替代 server 侧 test_helpers::in_memory_db）。
    async fn in_memory_db() -> rust_tunnel_persistence::Database {
        rust_tunnel_persistence::Database::new(":memory:")
            .await
            .expect("in-memory db")
    }

    fn handler_state(protocol: Option<LlmProtocol>) -> LlmHandlerState {
        let llm = Arc::new(LlmState::new(None, None));
        LlmHandlerState { llm, protocol }
    }

    fn handler_state_with_db(
        db: rust_tunnel_persistence::Database,
        protocol: Option<LlmProtocol>,
    ) -> LlmHandlerState {
        let llm = Arc::new(LlmState::new(Some(db), None));
        LlmHandlerState { llm, protocol }
    }

    fn authed_headers(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        h
    }

    fn make_request(
        model: &str,
        messages: Vec<ChatMessage>,
        raw_body: Option<serde_json::Value>,
    ) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: model.into(),
            messages,
            stream: false,
            max_tokens: None,
            temperature: None,
            top_p: None,
            tools: None,
            tool_choice: None,
            raw_body,
        }
    }

    // ── PreparedRequest ────────────────────────────────────────

    #[test]
    fn prepared_request_construction_minimal() {
        let req = make_request("gpt-4", vec![ChatMessage::text("user", "hi")], None);
        let pr = PreparedRequest {
            request: req,
            message_count: 1,
            has_tools: false,
            compat_enabled: false,
            anthropic_body: None,
        };
        assert_eq!(pr.request.model, "gpt-4");
        assert_eq!(pr.message_count, 1);
        assert!(!pr.has_tools);
        assert!(!pr.compat_enabled);
        assert!(pr.anthropic_body.is_none());
    }

    #[test]
    fn prepared_request_with_anthropic_body() {
        let req = make_request("claude-3", vec![ChatMessage::text("user", "hello")], None);
        let body = json!({"model":"claude-3","messages":[{"role":"user","content":"hello"}]});
        let pr = PreparedRequest {
            request: req,
            message_count: 1,
            has_tools: false,
            compat_enabled: false,
            anthropic_body: Some(body.clone()),
        };
        assert!(pr.anthropic_body.is_some());
        assert_eq!(pr.anthropic_body.unwrap()["model"], "claude-3");
    }

    #[test]
    fn prepared_request_has_tools_and_compat() {
        let req = make_request(
            "m",
            vec![ChatMessage::text("user", "hi")],
            Some(json!({"model":"m","messages":[{"role":"user","content":"hi"}]})),
        );
        let pr = PreparedRequest {
            request: req,
            message_count: 5,
            has_tools: true,
            compat_enabled: true,
            anthropic_body: None,
        };
        assert_eq!(pr.message_count, 5);
        assert!(pr.has_tools);
        assert!(pr.compat_enabled);
        assert!(pr.request.raw_body.is_some());
    }

    #[test]
    fn prepared_request_stream_and_model() {
        let mut req = make_request("model-x", vec![], None);
        req.stream = true;
        let pr = PreparedRequest {
            request: req,
            message_count: 0,
            has_tools: false,
            compat_enabled: false,
            anthropic_body: None,
        };
        assert!(pr.request.stream);
        assert_eq!(pr.request.model, "model-x");
        assert_eq!(pr.message_count, 0);
    }

    // ── ResponsePostProcess ────────────────────────────────────

    #[test]
    fn response_post_process_equality() {
        assert_eq!(ResponsePostProcess::None, ResponsePostProcess::None);
        assert_eq!(
            ResponsePostProcess::ToAnthropic,
            ResponsePostProcess::ToAnthropic
        );
        assert_eq!(
            ResponsePostProcess::ToResponses,
            ResponsePostProcess::ToResponses
        );
        assert_ne!(ResponsePostProcess::None, ResponsePostProcess::ToAnthropic);
        assert_ne!(
            ResponsePostProcess::ToAnthropic,
            ResponsePostProcess::ToResponses
        );
        assert_ne!(ResponsePostProcess::None, ResponsePostProcess::ToResponses);
    }

    #[test]
    fn response_post_process_clone_copy() {
        let a = ResponsePostProcess::ToAnthropic;
        let b = a;
        let c = a;
        assert_eq!(a, b);
        assert_eq!(a, c);
        let d: ResponsePostProcess = a;
        assert_eq!(d, ResponsePostProcess::ToAnthropic);
    }

    #[test]
    fn response_post_process_debug_contains_variant() {
        assert!(format!("{:?}", ResponsePostProcess::None).contains("None"));
        assert!(format!("{:?}", ResponsePostProcess::ToAnthropic).contains("ToAnthropic"));
        assert!(format!("{:?}", ResponsePostProcess::ToResponses).contains("ToResponses"));
    }

    #[test]
    fn response_post_process_match_exhaustive() {
        fn as_u8(v: ResponsePostProcess) -> u8 {
            match v {
                ResponsePostProcess::None => 0,
                ResponsePostProcess::ToAnthropic => 1,
                ResponsePostProcess::ToResponses => 2,
            }
        }
        assert_eq!(as_u8(ResponsePostProcess::None), 0);
        assert_eq!(as_u8(ResponsePostProcess::ToAnthropic), 1);
        assert_eq!(as_u8(ResponsePostProcess::ToResponses), 2);
    }

    // ── write_back_messages (private) ──────────────────────────

    #[test]
    fn write_back_messages_with_raw_body_updates_messages() {
        let mut req = make_request(
            "m",
            vec![ChatMessage::text("user", "hi")],
            Some(json!({"model":"m","messages":[{"role":"user","content":"old"}]})),
        );
        req.messages.push(ChatMessage::text("assistant", "hello"));
        write_back_messages(&mut req);
        let raw = req.raw_body.unwrap();
        let msgs = raw["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["content"], "hi");
        assert_eq!(msgs[1]["content"], "hello");
    }

    #[test]
    fn write_back_messages_without_raw_body_is_noop() {
        let mut req = make_request("m", vec![ChatMessage::text("user", "hi")], None);
        req.messages.push(ChatMessage::text("user", "extra"));
        write_back_messages(&mut req);
        assert!(req.raw_body.is_none());
        assert_eq!(req.messages.len(), 2);
    }

    #[test]
    fn write_back_messages_overwrites_stale_raw() {
        let mut req = make_request(
            "m",
            vec![ChatMessage::text("user", "new")],
            Some(
                json!({"model":"m","messages":[{"role":"user","content":"stale"}],"extra":"keep"}),
            ),
        );
        write_back_messages(&mut req);
        let raw = req.raw_body.unwrap();
        assert_eq!(raw["extra"], "keep");
        assert_eq!(raw["messages"][0]["content"], "new");
    }

    #[test]
    fn write_back_messages_empty_messages() {
        let mut req = make_request(
            "m",
            vec![],
            Some(json!({"model":"m","messages":[{"role":"user","content":"x"}]})),
        );
        write_back_messages(&mut req);
        let raw = req.raw_body.unwrap();
        assert_eq!(raw["messages"].as_array().unwrap().len(), 0);
    }

    // ── inject_rag_and_compat ──────────────────────────────────

    #[tokio::test]
    async fn inject_rag_and_compat_noop_when_disabled() {
        let state = LlmState::new(None, None);
        let mut req = make_request(
            "m",
            vec![ChatMessage::text("user", "hi")],
            Some(json!({"model":"m","messages":[{"role":"user","content":"hi"}]})),
        );
        let mut ctx = crate::usage::UsageContext {
            protocol: "openai".into(),
            ..Default::default()
        };
        let before = req.messages.clone();
        inject_rag_and_compat(&state, None, None, false, &mut req, &mut ctx).await;
        assert_eq!(req.messages.len(), before.len());
        assert_eq!(req.messages[0].content, before[0].content);
        assert!(ctx.rag_chunks_injected.is_none());
    }

    #[tokio::test]
    async fn inject_rag_and_compat_compat_rewrites_tool_history_and_guidance() {
        let state = LlmState::new(None, None);
        let mut req = make_request(
            "m",
            vec![
                ChatMessage::text("user", "call tool"),
                ChatMessage {
                    role: "assistant".into(),
                    content: Some("thinking".into()),
                    reasoning_content: None,
                    tool_calls: Some(vec![
                        json!({"id":"call_1","type":"function","function":{"name":"search","arguments":"{\"q\":\"hi\"}"}}),
                    ]),
                    tool_call_id: None,
                    name: None,
                },
                ChatMessage {
                    role: "tool".into(),
                    content: Some("result text".into()),
                    reasoning_content: None,
                    tool_calls: None,
                    tool_call_id: Some("call_1".into()),
                    name: None,
                },
            ],
            Some(json!({"model":"m","messages":[]})),
        );
        let mut ctx = crate::usage::UsageContext {
            protocol: "openai".into(),
            ..Default::default()
        };
        inject_rag_and_compat(&state, None, None, true, &mut req, &mut ctx).await;
        assert!(req.messages.iter().all(|m| m.tool_calls.is_none()));
        let has_tool_result = req
            .messages
            .iter()
            .any(|m| m.content.as_deref().unwrap_or("").contains("<tool_result"));
        assert!(
            has_tool_result,
            "tool result should be rewritten: {:?}",
            req.messages
        );
        let has_tool_call = req
            .messages
            .iter()
            .any(|m| m.content.as_deref().unwrap_or("").contains("<tool_call>"));
        assert!(
            has_tool_call,
            "tool_calls should be rewritten: {:?}",
            req.messages
        );
        let last = req.messages.last().unwrap();
        assert_eq!(last.role, "system");
        assert!(last.content.as_deref().unwrap().contains("<tool_call>"));
        let raw_msgs = req.raw_body.as_ref().unwrap()["messages"]
            .as_array()
            .unwrap();
        assert_eq!(raw_msgs.len(), req.messages.len());
        assert!(ctx.rag_chunks_injected.is_none());
    }

    #[tokio::test]
    async fn inject_rag_and_compat_writes_back_when_compat_enabled_without_raw_body() {
        let state = LlmState::new(None, None);
        let mut req = make_request("m", vec![ChatMessage::text("user", "hi")], None);
        let mut ctx = crate::usage::UsageContext {
            protocol: "openai".into(),
            ..Default::default()
        };
        inject_rag_and_compat(&state, None, None, true, &mut req, &mut ctx).await;
        assert_eq!(req.messages.len(), 2);
        assert!(req.raw_body.is_none());
    }

    #[tokio::test]
    async fn inject_rag_and_compat_with_db_but_no_kb_still_no_rag() {
        let db = in_memory_db().await;
        let state = crate::LlmState::new(Some(db.clone()), None);
        let mut req = make_request(
            "m",
            vec![ChatMessage::text("user", "hi")],
            Some(json!({"model":"m","messages":[{"role":"user","content":"hi"}]})),
        );
        let mut ctx = crate::usage::UsageContext {
            protocol: "openai".into(),
            ..Default::default()
        };
        inject_rag_and_compat(&state, Some(&db), None, false, &mut req, &mut ctx).await;
        assert!(ctx.rag_chunks_injected.is_none());
        assert_eq!(req.messages.len(), 1);
    }

    #[tokio::test]
    async fn inject_rag_and_compat_with_db_and_kb_id_noop_without_feature_or_data() {
        let db = in_memory_db().await;
        let state = crate::LlmState::new(Some(db.clone()), None);
        let mut req = make_request(
            "m",
            vec![ChatMessage::text("user", "hi")],
            Some(json!({"model":"m","messages":[{"role":"user","content":"hi"}]})),
        );
        let mut ctx = crate::usage::UsageContext {
            protocol: "openai".into(),
            ..Default::default()
        };
        inject_rag_and_compat(
            &state,
            Some(&db),
            Some("kb-unknown".into()),
            false,
            &mut req,
            &mut ctx,
        )
        .await;
        assert!(ctx.rag_chunks_injected.is_none());
    }

    // ── authenticate_or_reject / extract_model_or_reject / resolve_chain_or_reject ──

    #[tokio::test]
    async fn authenticate_or_reject_without_db_returns_401() {
        let state = handler_state(None);
        let headers = HeaderMap::new();
        let res = authenticate_or_reject(&state, &headers, "openai").await;
        assert!(res.is_err());
        let resp = res.unwrap_err();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"]["type"], "authentication_error");
    }

    #[tokio::test]
    async fn authenticate_or_reject_with_invalid_token_returns_401() {
        let state = handler_state(None);
        let headers = authed_headers("sk-invalid");
        let res = authenticate_or_reject(&state, &headers, "openai").await;
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn authenticate_or_reject_anthropic_protocol_returns_anthropic_error_shape() {
        let state = handler_state(Some(LlmProtocol::Anthropic));
        let headers = HeaderMap::new();
        let res = authenticate_or_reject(&state, &headers, "anthropic").await;
        let resp = res.unwrap_err();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["type"], "error");
        assert_eq!(v["error"]["type"], "authentication_error");
    }

    #[tokio::test]
    async fn extract_model_or_reject_success() {
        let state = handler_state(None);
        let body = json!({"model":"gpt-4","messages":[]});
        let res = extract_model_or_reject(&state, &body, "key-id", "key-name", "openai").await;
        assert_eq!(res.unwrap(), "gpt-4");
    }

    #[tokio::test]
    async fn extract_model_or_reject_missing_returns_400() {
        let state = handler_state(None);
        let body = json!({"messages":[]});
        let res = extract_model_or_reject(&state, &body, "key-id", "key-name", "openai").await;
        assert!(res.is_err());
        let resp = res.unwrap_err();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let b = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(v["error"]["type"], "invalid_request_error");
    }

    #[tokio::test]
    async fn extract_model_or_reject_missing_anthropic_shape() {
        let state = handler_state(Some(LlmProtocol::Anthropic));
        let body = json!({});
        let res = extract_model_or_reject(&state, &body, "k1", "n1", "anthropic").await;
        let resp = res.unwrap_err();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let b = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(v["type"], "error");
        assert_eq!(v["error"]["type"], "invalid_request_error");
    }

    #[tokio::test]
    async fn extract_model_or_reject_non_string_model_returns_400() {
        let state = handler_state(None);
        let body = json!({"model":123});
        let res = extract_model_or_reject(&state, &body, "k1", "n1", "openai").await;
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn resolve_chain_or_reject_without_db_returns_404() {
        let state = handler_state(None);
        let res = resolve_chain_or_reject(&state, "any-model", "k1", "n1", "openai").await;
        assert!(res.is_err());
        let resp = res.unwrap_err();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let b = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert!(v["error"]["type"].as_str().is_some());
    }

    #[tokio::test]
    async fn resolve_chain_or_reject_with_empty_db_returns_404_with_available_models() {
        let db = in_memory_db().await;
        let state = handler_state_with_db(db, None);
        let res = resolve_chain_or_reject(&state, "no-such-model", "k1", "n1", "openai").await;
        let resp = res.unwrap_err();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let b = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert!(
            v["error"]["available_models"].is_array(),
            "should include available_models: {v}"
        );
    }

    // ── LlmState::new lightweight construction ─────────────────

    #[test]
    fn llm_state_new_without_db_is_lightweight() {
        let s = LlmState::new(None, None);
        assert!(s.db.is_none());
        assert!(s.cipher.is_none());
    }

    #[tokio::test]
    async fn llm_state_new_with_in_memory_db() {
        let db = in_memory_db().await;
        let s = LlmState::new(Some(db), None);
        assert!(s.db.is_some());
    }

    #[cfg(feature = "rag")]
    #[test]
    fn last_user_message_extracted() {
        let msgs = vec![
            ChatMessage::text("system", "s"),
            ChatMessage::text("user", "第一句"),
            ChatMessage::text("assistant", "答"),
            ChatMessage::text("user", "第二句"),
        ];
        assert_eq!(last_user_text(&msgs), Some("第二句".to_string()));
    }

    #[cfg(feature = "rag")]
    #[test]
    fn last_user_text_blank_user_yields_none() {
        // 实现语义：find 命中最后的 user 再 filter 空白 → None（不回溯更早 user）。
        let msgs = vec![
            ChatMessage::text("user", "有效"),
            ChatMessage::text("user", "   "),
        ];
        assert_eq!(last_user_text(&msgs), None);
    }
}
