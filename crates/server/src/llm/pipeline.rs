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
use super::{ChatCompletionRequest, LlmState};
use crate::db::Database;

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
}

/// 认证网关 API key；失败时记录用量日志并返回 401 响应。
pub async fn authenticate_or_reject(
    state: &LlmHandlerState,
    headers: &HeaderMap,
    protocol: &str,
) -> Result<(String, String), Response> {
    match super::auth::authenticate(&state.llm, headers).await {
        Some(a) => Ok(a),
        None => {
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
}

/// 提取请求体中的 model 字段；缺失时记录用量日志并返回 400 响应。
pub async fn extract_model_or_reject(
    state: &LlmHandlerState,
    body: &Value,
    api_key_id: &str,
    api_key_name: &str,
    protocol: &str,
) -> Result<String, Response> {
    match body.get("model").and_then(Value::as_str) {
        Some(m) => Ok(m.to_string()),
        None => {
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
}

/// 解析模型路由（模型名/别名/模型组 → 候选链）；失败时记录用量日志并返回 404 响应。
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
#[cfg_attr(not(feature = "rag"), allow(unused_variables))]
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
        let outcome =
            super::rag::enhance(db, &state.rag_store, state.cipher.as_ref(), &kb_id, request).await;
        rag_injected = outcome.injected as i64;
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

/// 上游成功响应后处理（协议特有：Anthropic 回退路径需要把 OpenAI 格式转回）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponsePostProcess {
    /// OpenAI 入口：响应原样透传。
    None,
    /// Anthropic 入口：把上游 OpenAI 格式转成 Anthropic Messages 格式。
    ToAnthropic,
}

/// 构造上行请求体 → 写请求日志 → 候选链故障转移执行 → 结果处理（成功/全部失败）。
///
/// 调用方负责协议特有的请求解析与 RAG/compat 改写（`PreparedRequest` 已含最终内容），
/// 本函数只做与协议无关的执行与出账。
#[allow(clippy::too_many_arguments)]
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

    // 构造完整上游请求体（协议解析 + RAG/compat 改写后的最终内容），
    // 写入请求日志后发送，保证日志与实际发送内容一致。
    let req_body = super::upstream::build_upstream_body(request);
    super::log_llm_request(
        &state.llm,
        protocol,
        &request.model,
        message_count,
        has_tools,
        request.stream,
        None,
        None,
        0,
        &req_body,
    )
    .await;
    let outcome = super::upstream::execute_with_failover(
        &state.llm.breakers,
        chain,
        &req_body,
        request.stream,
    )
    .await;
    match outcome {
        super::upstream::FailoverOutcome::Success {
            resp,
            candidate,
            failed_over,
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
                protocol,
                &ctx.model_name,
                message_count,
                has_tools,
                request.stream,
                Some(200),
                None,
                elapsed_ms,
                // 完整请求体只在发送前日志落一次（sanitized），结果日志不重复
                &serde_json::Value::Null,
            )
            .await;
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
                protocol,
                &request.model,
                message_count,
                has_tools,
                request.stream,
                Some(status.as_u16()),
                Some(&msg),
                elapsed_ms,
                // 完整请求体只在发送前日志落一次（sanitized），结果日志不重复
                &serde_json::Value::Null,
            )
            .await;
            // 记录失败请求到用量日志，确保请求明细中可见
            if let Some(ref db) = db {
                // 全部候选失败但实际尝试过转移：failover_from 记首选（被跳过的）模型名
                if failed_over {
                    ctx.failover_from = Some(first_candidate_model_name.to_string());
                }
                ctx.record_failure(db, status.as_u16() as i32, "upstream_error", started);
            }
            state.error_for_protocol(status, msg, "upstream_error")
        }
    }
}
