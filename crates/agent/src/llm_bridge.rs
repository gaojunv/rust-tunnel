//! Bridge AgentLlmProxyRequest to the server-side LLM gateway, streaming
//! response chunks back to the client over the control channel.
//!
//! 客户端内嵌 LLM 回环代理把 agent 进程的 LLM API 请求经控制通道转交服务端，
//! 本模块按 workspace 的 `llm_model_id` 解析 model_name，改写请求体 `model`
//! 字段后**直接函数调用** LLM 网关 handler（`handle_messages` /
//! `handle_chat_completions` / `handle_responses`），让网关的模型组故障转移、
//! 格式转换、用量统计、RAG 注入等管线全部生效——与外部 HTTP 流量共享同一条代码路径。
//! **LLM secret 只在服务端接触，客户端永不持有。**

use std::sync::Arc;

use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue};
use axum::Json;
use futures_util::{Stream, StreamExt};
use serde_json::Value;

use crate::db::Database;
use crate::llm::openai_handler::LlmHandlerState;
use crate::llm::{anthropic_handler, openai_handler, responses_handler, LlmProtocol, LlmState};

/// LLM 网关入口（直接函数调用 handler 时用）。
#[derive(Debug, Clone)]
pub struct LlmGatewayEndpoint {
    /// 网关共享状态（handler 的 `State<LlmHandlerState>` 来源）。
    pub llm_state: Arc<LlmState>,
    /// 内部 API key（`Authorization: Bearer` 头注入，绕开外部认证；
    /// 用量统计以此 key 归属 ACP 流量）。
    pub api_key: String,
}

/// 一个 LLM 代理响应块，对应 `ControlMessage::AgentLlmProxyChunk` 的载荷。
/// 独立 struct 让 [`forward`] 返回精确的 Stream 类型；调用方（AcpBridge）
/// 转换成 ControlMessage 下发到客户端控制通道。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentLlmProxyChunk {
    /// 代理请求标识。
    pub request_id: String,
    /// 原始响应字节（SSE 块或错误消息）
    pub data: Vec<u8>,
    /// 响应结束标记（Task 3 评审契约：**所有**响应必须以 done=true 收尾，
    /// 包括错误路径）
    pub done: bool,
    /// HTTP 状态码（错误时前端据此区分 400/502 等）
    pub status: u16,
}

/// 处理一个 LLM 代理请求，返回 `AgentLlmProxyChunk` 流。
///
/// 解析链路：`resolve_effective_model`（session.model → workspace.llm_model_id →
/// 全局默认 → 第一个可用）得到网关可解析的模型引用，改写请求体 `model` 字段后
/// 按路径分发到网关 handler（`/v1/messages` → Anthropic 入口；
/// `/v1/chat/completions` → OpenAI 入口；`/v1/responses` → Responses 入口）。
/// 网关自动完成模型组故障转移、格式转换、用量统计、RAG 注入等管线。
///
/// # 契约
/// 无论成功/失败，流总是以 `done=true` 的 chunk 结束（见 [`AgentLlmProxyChunk`]）。
pub fn forward(
    db: Database,
    session_id: String,
    request_id: String,
    gateway: LlmGatewayEndpoint,
    path: String,
    body: Vec<u8>,
) -> impl Stream<Item = AgentLlmProxyChunk> {
    async_stream::stream! {
        // 1. 解析请求体。
        let mut body_json: Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => {
                yield AgentLlmProxyChunk {
                    request_id,
                    data: format!("invalid llm proxy request body: {e}").into_bytes(),
                    done: true,
                    status: 400,
                };
                return;
            }
        };
        let is_stream = body_json
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        // 2. 模型解析：如果请求体中的 model 已经可被网关解析（模型名/别名/组名），
        //    则保留请求体中的 model（tier 环境变量注入场景）；否则走
        //    resolve_effective_model（session → workspace → 全局默认 → 第一个可用）。
        let request_model = body_json
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string);
        let model_name = if let Some(ref rm) = request_model {
            if crate::llm::router::model_resolvable(
                gateway.llm_state.as_ref(),
                rm,
            )
            .await
            {
                rm.clone()
            } else {
                match super::session::resolve_effective_model(
                    &db,
                    Some(gateway.llm_state.as_ref()),
                    &session_id,
                )
                .await
                {
                    Ok(name) => name,
                    Err(e) => {
                        tracing::warn!(
                            session_id,
                            request_id = %request_id,
                            error = %e,
                            "llm proxy: model resolve failed"
                        );
                        yield AgentLlmProxyChunk {
                            request_id,
                            data: e.into_bytes(),
                            done: true,
                            status: 502,
                        };
                        return;
                    }
                }
            }
        } else {
            match super::session::resolve_effective_model(
                &db,
                Some(gateway.llm_state.as_ref()),
                &session_id,
            )
            .await
            {
                Ok(name) => name,
                Err(e) => {
                    tracing::warn!(
                        session_id,
                        request_id = %request_id,
                        error = %e,
                        "llm proxy: model resolve failed"
                    );
                    yield AgentLlmProxyChunk {
                        request_id,
                        data: e.into_bytes(),
                        done: true,
                        status: 502,
                    };
                    return;
                }
            }
        };
        body_json["model"] = Value::String(model_name);

        // 3. 按路径分发到协议入口 handler（与 shared_listener 的 llm_handle
        //    白名单一致；path 可能带 query，如 `/v1/messages?beta=true`）。
        let clean_path = path.split('?').next().unwrap_or("/");
        let is_messages = clean_path == "/v1/messages";
        let is_chat_completions = clean_path == "/v1/chat/completions";
        let is_models = clean_path == "/v1/models";
        let is_responses = clean_path == "/v1/responses";
        if !is_messages && !is_chat_completions && !is_models && !is_responses {
            yield AgentLlmProxyChunk {
                request_id,
                data: format!("unsupported llm proxy path: {clean_path}").into_bytes(),
                done: true,
                status: 404,
            };
            return;
        }
        let protocol = if is_messages { LlmProtocol::Anthropic } else { LlmProtocol::OpenAI };

        // 4. 直接函数调用网关 handler——手工构造 axum extractor 实参
        //    （State/Json 只是元组包装；handler 逻辑不依赖 axum 运行时）。
        let handler_state = LlmHandlerState {
            llm: gateway.llm_state.clone(),
            protocol: Some(protocol),
        };
        let mut headers = HeaderMap::new();
        if let Ok(v) = HeaderValue::from_str(&format!("Bearer {}", gateway.api_key)) {
            headers.insert(header::AUTHORIZATION, v);
        }
        let resp = if is_models {
            openai_handler::handle_list_models(State(handler_state), headers).await
        } else if is_messages {
            anthropic_handler::handle_messages(State(handler_state), headers, Json(body_json)).await
        } else if is_responses {
            responses_handler::handle_responses(State(handler_state), headers, Json(body_json)).await
        } else {
            openai_handler::handle_chat_completions(State(handler_state), headers, Json(body_json)).await
        };

        // 5. 流式/非流式统一走 body data stream 回传。
        let status = resp.status().as_u16();
        let mut stream = resp.into_body().into_data_stream();
        while let Some(item) = stream.next().await {
            match item {
                Ok(bytes) => {
                    // 协议 1MB 上限：body data stream 的块可能远超限制（非流式
                    // 整个响应、compat 流式重写后单块），必须切 ≤512KB 再发，
                    // 否则 AgentLlmProxyChunk 编码超限会断开客户端控制连接
                    // （对称缺陷，见 client llm_proxy 的 413 保护）。
                    const MAX_CHUNK: usize = 512 * 1024;
                    if bytes.is_empty() {
                        continue;
                    }
                    for piece in bytes.chunks(MAX_CHUNK) {
                        yield AgentLlmProxyChunk {
                            request_id: request_id.clone(),
                            data: piece.to_vec(),
                            done: false,
                            status,
                        };
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        session_id,
                        request_id = %request_id,
                        error = %e,
                        "llm proxy: gateway stream read failed"
                    );
                    yield AgentLlmProxyChunk {
                        request_id: request_id.clone(),
                        data: format!("gateway stream read failed: {e}").as_bytes().to_vec(),
                        done: true,
                        status: if is_stream { status } else { 502 },
                    };
                    return;
                }
            }
        }
        // 正常结束：空 body 的 done 收尾 chunk
        yield AgentLlmProxyChunk {
            request_id,
            data: Vec::new(),
            done: true,
            status,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::auth::generate_api_key;
    use futures_util::StreamExt;

    /// 构造带 DB 的 endpoint：API key 写入 DB（authenticate 走 hash 查询）。
    async fn test_gateway(db: &Database) -> LlmGatewayEndpoint {
        let (raw_key, key_hash, key_prefix) = generate_api_key();
        db.llm_save_api_key(
            "__acp_internal__",
            &key_hash,
            &key_prefix,
            "ACP Internal",
            None,
        )
        .await
        .unwrap();
        LlmGatewayEndpoint {
            llm_state: Arc::new(LlmState::new(Some(db.clone()), None)),
            api_key: raw_key,
        }
    }

    /// 造 session → workspace(llm_model_id) → model → provider 全链路。
    async fn seed_configured_session(db: &Database, session_id: &str, model_id: &str) {
        db.agent_create_workspace(&rust_tunnel_persistence::agent::AgentWorkspaceCreateOpts {
            id: "w1".to_owned(),
            name: "proj".to_owned(),
            client_id: "nas".to_owned(),
            runtime_type: "host".to_owned(),
            root_path: "/workspace".to_owned(),
            docker_image: None,
            docker_container_id: None,
            agent_type: String::new(),
            agent_path: None,
            llm_model_id: None,
            agent_config_overrides: None,
            claude_tier_models: None,
        })
        .await
        .unwrap();
        db.agent_set_workspace_llm_model_id("w1", model_id)
            .await
            .unwrap();
        db.agent_create_session(session_id, "w1", None, None)
            .await
            .unwrap();
    }

    async fn save_provider_model(db: &Database, model_id: &str, base_url: &str, enabled: bool) {
        db.llm_save_provider(
            "prov-1",
            "test-provider",
            "deepseek",
            base_url,
            "sk-test-123",
            None,
            None,
            true,
        )
        .await
        .unwrap();
        db.llm_save_model(
            model_id, "prov-1", "gpt-test", "gpt-test", "", enabled, None,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn test_forward_unconfigured_session_returns_502_done() {
        let db = Database::new(":memory:").await.unwrap();
        let gw = test_gateway(&db).await;
        let stream = forward(
            db,
            "sess-missing".into(),
            "req-1".into(),
            gw,
            "/v1/chat/completions".into(),
            br#"{"model":"gpt-test","stream":true}"#.to_vec(),
        );
        let chunks: Vec<AgentLlmProxyChunk> = stream.collect().await;
        assert_eq!(chunks.len(), 1, "error path must emit exactly one chunk");
        assert!(chunks[0].done, "error path must end with done=true");
        assert_eq!(chunks[0].status, 502);
        assert_eq!(chunks[0].request_id, "req-1");
        assert!(
            String::from_utf8_lossy(&chunks[0].data).contains("session not found"),
            "err body: {}",
            String::from_utf8_lossy(&chunks[0].data)
        );
    }

    #[tokio::test]
    async fn test_forward_workspace_without_model_id_returns_502_done() {
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(&rust_tunnel_persistence::agent::AgentWorkspaceCreateOpts {
            id: "w1".to_owned(),
            name: "proj".to_owned(),
            client_id: "nas".to_owned(),
            runtime_type: "host".to_owned(),
            root_path: "/workspace".to_owned(),
            docker_image: None,
            docker_container_id: None,
            agent_type: String::new(),
            agent_path: None,
            llm_model_id: None,
            agent_config_overrides: None,
            claude_tier_models: None,
        })
        .await
        .unwrap();
        db.agent_create_session("sess-1", "w1", None, None)
            .await
            .unwrap();
        let gw = test_gateway(&db).await;

        let stream = forward(
            db,
            "sess-1".into(),
            "req-1".into(),
            gw,
            "/v1/chat/completions".into(),
            br#"{"model":"gpt-test","stream":true}"#.to_vec(),
        );
        let chunks: Vec<AgentLlmProxyChunk> = stream.collect().await;
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].done);
        assert_eq!(chunks[0].status, 502);
        // workspace/session/全局默认均未配置 → "no LLM model configured"
        assert!(
            String::from_utf8_lossy(&chunks[0].data).contains("no LLM model configured"),
            "err body: {}",
            String::from_utf8_lossy(&chunks[0].data)
        );
    }

    #[tokio::test]
    async fn test_forward_disabled_model_returns_502_done() {
        let db = Database::new(":memory:").await.unwrap();
        save_provider_model(&db, "model-off", "https://llm.example.test", false).await;
        seed_configured_session(&db, "sess-1", "model-off").await;
        let gw = test_gateway(&db).await;

        let stream = forward(
            db,
            "sess-1".into(),
            "req-1".into(),
            gw,
            "/v1/chat/completions".into(),
            br#"{"model":"gpt-test","stream":true}"#.to_vec(),
        );
        let chunks: Vec<AgentLlmProxyChunk> = stream.collect().await;
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].done);
        assert_eq!(chunks[0].status, 502);
        assert!(String::from_utf8_lossy(&chunks[0].data).contains("disabled"));
    }

    #[tokio::test]
    async fn test_forward_malformed_body_returns_400_done() {
        let db = Database::new(":memory:").await.unwrap();
        let gw = test_gateway(&db).await;
        let stream = forward(
            db,
            "sess-1".into(),
            "req-1".into(),
            gw,
            "/v1/chat/completions".into(),
            b"not json".to_vec(),
        );
        let chunks: Vec<AgentLlmProxyChunk> = stream.collect().await;
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].done);
        assert_eq!(chunks[0].status, 400);
    }

    #[tokio::test]
    async fn test_forward_unknown_path_returns_404_done() {
        // 不在白名单内的路径 → 404 done（对应 llm_handle 的路径白名单）
        let db = Database::new(":memory:").await.unwrap();
        let gw = test_gateway(&db).await;
        // 注意：必须先有有效 session + 模型配置，否则在路径白名单检查之前就被
        // resolve_effective_model 的 502 拦截（模型注入在路径分发之前）。
        seed_configured_session(&db, "sess-1", "model-1").await;
        save_provider_model(&db, "model-1", "http://127.0.0.1:1", true).await;
        let stream = forward(
            db,
            "sess-1".into(),
            "req-1".into(),
            gw,
            "/v1/unknown".into(),
            br#"{"model":"gpt-test"}"#.to_vec(),
        );
        let chunks: Vec<AgentLlmProxyChunk> = stream.collect().await;
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].done);
        assert_eq!(chunks[0].status, 404);
        assert!(String::from_utf8_lossy(&chunks[0].data).contains("unsupported llm proxy path"));
    }

    #[tokio::test]
    async fn test_forward_upstream_unreachable_ends_with_done() {
        // session + model 配置完整，handler 正常运行，但上游 provider
        // 不可达 → 网关回 5xx 错误响应（done=true 收尾，契约不变）
        let db = Database::new(":memory:").await.unwrap();
        save_provider_model(&db, "model-1", "http://127.0.0.1:1", true).await;
        seed_configured_session(&db, "sess-1", "model-1").await;
        let gw = test_gateway(&db).await;

        let stream = forward(
            db,
            "sess-1".into(),
            "req-1".into(),
            gw,
            "/v1/chat/completions".into(),
            br#"{"model":"gpt-test","stream":false}"#.to_vec(),
        );
        let chunks: Vec<AgentLlmProxyChunk> = stream.collect().await;
        let last = chunks.last().expect("at least one chunk");
        assert!(last.done, "must end with done=true");
        assert!(
            last.status >= 400,
            "upstream failure → error status, got {}",
            last.status
        );
    }

    #[tokio::test]
    async fn test_forward_messages_path_reaches_anthropic_handler() {
        // `/v1/messages`（可带 query，如 `?beta=true`）应路由到 Anthropic
        // handler：错误响应须为 Anthropic 格式（顶层 `type: "error"`）。
        let db = Database::new(":memory:").await.unwrap();
        save_provider_model(&db, "model-1", "http://127.0.0.1:1", true).await;
        seed_configured_session(&db, "sess-1", "model-1").await;
        let gw = test_gateway(&db).await;

        let stream = forward(
            db,
            "sess-1".into(),
            "req-1".into(),
            gw,
            "/v1/messages?beta=true".into(), // Claude Code 路径（带 query）
            br#"{"model":"gpt-test","stream":false}"#.to_vec(),
        );
        let chunks: Vec<AgentLlmProxyChunk> = stream.collect().await;
        let last = chunks.last().expect("at least one chunk");
        assert!(last.done);
        assert!(last.status >= 400);
        let body: String = chunks
            .iter()
            .map(|c| String::from_utf8_lossy(&c.data).into_owned())
            .collect();
        assert!(
            body.contains("\"type\":\"error\""),
            "anthropic error format expected, body: {body}"
        );
    }

    #[tokio::test]
    async fn test_forward_responses_path_reaches_responses_handler() {
        // `/v1/responses` 应路由到 Responses handler（Codex 等客户端）。
        // 上游不可达 → 网关回 OpenAI 格式错误（`"error": { "message": ..., "type": ... }`）。
        let db = Database::new(":memory:").await.unwrap();
        save_provider_model(&db, "model-1", "http://127.0.0.1:1", true).await;
        seed_configured_session(&db, "sess-1", "model-1").await;
        let gw = test_gateway(&db).await;

        let stream = forward(
            db,
            "sess-1".into(),
            "req-1".into(),
            gw,
            "/v1/responses".into(),
            br#"{"model":"gpt-test","input":"hi","stream":false}"#.to_vec(),
        );
        let chunks: Vec<AgentLlmProxyChunk> = stream.collect().await;
        let last = chunks.last().expect("at least one chunk");
        assert!(last.done, "must end with done=true");
        assert!(
            last.status >= 400,
            "upstream failure → error status, got {}",
            last.status
        );
        let body: String = chunks
            .iter()
            .map(|c| String::from_utf8_lossy(&c.data).into_owned())
            .collect();
        // Responses handler 走 OpenAI 错误格式（不是 Anthropic `"type":"error"`）
        assert!(
            body.contains("\"error\""),
            "openai error format expected for responses path, body: {body}"
        );
    }

    #[tokio::test]
    async fn test_forward_bad_key_returns_401_done() {
        // endpoint 的 api_key 不在 DB 中 → handler 认证失败 → 401
        let db = Database::new(":memory:").await.unwrap();
        save_provider_model(&db, "model-1", "http://127.0.0.1:1", true).await;
        seed_configured_session(&db, "sess-1", "model-1").await;
        let gw = LlmGatewayEndpoint {
            llm_state: Arc::new(LlmState::new(Some(db.clone()), None)),
            api_key: "sk-not-in-db".into(),
        };

        let stream = forward(
            db,
            "sess-1".into(),
            "req-1".into(),
            gw,
            "/v1/chat/completions".into(),
            br#"{"model":"gpt-test","stream":false}"#.to_vec(),
        );
        let chunks: Vec<AgentLlmProxyChunk> = stream.collect().await;
        let last = chunks.last().expect("at least one chunk");
        assert!(last.done);
        assert_eq!(last.status, 401);
    }

    #[tokio::test]
    async fn test_forward_injects_model_name_into_body() {
        // model_name 注入后请求应通过 handler 的 model 校验与路由解析
        // （进入上游调用阶段，失败→5xx），而非因缺 model 字段 400。
        let db = Database::new(":memory:").await.unwrap();
        save_provider_model(&db, "model-1", "http://127.0.0.1:1", true).await;
        seed_configured_session(&db, "sess-1", "model-1").await;
        let gw = test_gateway(&db).await;

        let stream = forward(
            db,
            "sess-1".into(),
            "req-1".into(),
            gw,
            "/v1/chat/completions".into(),
            br#"{"stream":false}"#.to_vec(), // 没有 model 字段
        );
        let chunks: Vec<AgentLlmProxyChunk> = stream.collect().await;
        let last = chunks.last().expect("at least one chunk");
        assert!(last.done);
        // model 注入生效的标志：错误不再是 "model is required"，
        // 而是更靠后的管线阶段（此处为 handler 参数校验 "messages is required"）。
        let body: String = chunks
            .iter()
            .map(|c| String::from_utf8_lossy(&c.data).into_owned())
            .collect();
        assert!(
            !body.contains("model is required"),
            "model injected → must not fail on missing model, body: {body}"
        );
    }

    #[tokio::test]
    async fn test_forward_resolvable_request_model_kept() {
        // tier 切换场景：请求体 model 可被网关解析（enabled 模型）→ 保留，
        // 不被 workspace 配置覆盖。判别手段：workspace 故意指向 disabled 模型——
        // 旧逻辑覆盖后 resolve 报 "disabled"（502），新逻辑保留请求模型走到
        // 上游调用阶段（不可达 → 5xx，且错误不含 "disabled"）。
        let db = Database::new(":memory:").await.unwrap();
        save_provider_model(&db, "model-req", "http://127.0.0.1:1", true).await;
        db.llm_save_model("model-off", "prov-1", "gpt-off", "gpt-off", "", false, None)
            .await
            .unwrap();
        seed_configured_session(&db, "sess-1", "model-off").await;
        let gw = test_gateway(&db).await;

        let stream = forward(
            db,
            "sess-1".into(),
            "req-1".into(),
            gw,
            "/v1/chat/completions".into(),
            br#"{"model":"gpt-test","stream":false}"#.to_vec(),
        );
        let chunks: Vec<AgentLlmProxyChunk> = stream.collect().await;
        let last = chunks.last().expect("at least one chunk");
        assert!(last.done);
        let body: String = chunks
            .iter()
            .map(|c| String::from_utf8_lossy(&c.data).into_owned())
            .collect();
        assert!(
            !body.contains("disabled"),
            "resolvable request model must be kept (not overridden by workspace), body: {body}"
        );
        assert!(
            last.status >= 400,
            "kept model reaches upstream (unreachable → error), got {}",
            last.status
        );
    }

    #[tokio::test]
    async fn test_forward_unresolvable_request_model_falls_back() {
        // 向后兼容：请求体 model 网关不可解析（claude-code 默认发的
        // claude-sonnet-4-5 等真实 tier 名）→ 回退 resolve_effective_model
        // 覆盖为 workspace 模型（行为同旧逻辑：进入上游调用 → 不可达 5xx，
        // 而非 model not found 4xx）。
        let db = Database::new(":memory:").await.unwrap();
        save_provider_model(&db, "model-1", "http://127.0.0.1:1", true).await;
        seed_configured_session(&db, "sess-1", "model-1").await;
        let gw = test_gateway(&db).await;

        let stream = forward(
            db,
            "sess-1".into(),
            "req-1".into(),
            gw,
            "/v1/chat/completions".into(),
            // 带合法 messages：回退路径可走到上游调用（5xx）；若 model 被
            // 错误保留则路由 resolve 失败于更早阶段（4xx）。
            br#"{"model":"claude-sonnet-4-5","messages":[{"role":"user","content":"hi"}],"stream":false}"#.to_vec(),
        );
        let chunks: Vec<AgentLlmProxyChunk> = stream.collect().await;
        let last = chunks.last().expect("at least one chunk");
        assert!(last.done);
        assert!(
            last.status >= 500,
            "unresolvable model falls back to workspace model → upstream 5xx, got {}",
            last.status
        );
    }

    #[tokio::test]
    async fn test_forward_model_with_disabled_provider_not_resolvable() {
        // 模型启用但所属 provider 禁用 → 视为不可解析 → 回退 workspace 模型
        // （与 available_models 的过滤语义一致；若被保留，路由期 resolve 失败
        // 返回 4xx 而非上游 5xx）。
        let db = Database::new(":memory:").await.unwrap();
        db.llm_save_provider(
            "prov-off",
            "disabled-provider",
            "deepseek",
            "http://127.0.0.1:1",
            "sk-test",
            None,
            None,
            false,
        )
        .await
        .unwrap();
        db.llm_save_model(
            "model-poff",
            "prov-off",
            "gpt-poff",
            "gpt-poff",
            "",
            true,
            None,
        )
        .await
        .unwrap();
        save_provider_model(&db, "model-1", "http://127.0.0.1:1", true).await;
        seed_configured_session(&db, "sess-1", "model-1").await;
        let gw = test_gateway(&db).await;

        let stream = forward(
            db,
            "sess-1".into(),
            "req-1".into(),
            gw,
            "/v1/chat/completions".into(),
            br#"{"model":"gpt-poff","messages":[{"role":"user","content":"hi"}],"stream":false}"#
                .to_vec(),
        );
        let chunks: Vec<AgentLlmProxyChunk> = stream.collect().await;
        let last = chunks.last().expect("at least one chunk");
        assert!(last.done);
        assert!(
            last.status >= 500,
            "disabled-provider model must fall back to workspace model → upstream 5xx, got {}",
            last.status
        );
    }

    /// 回归：非流式大响应（单块远超 1MB 协议切块线）必须被切 ≤512KB 下发，
    /// 否则 AgentLlmProxyChunk 编码超限会断开客户端控制连接。
    #[tokio::test]
    async fn test_forward_slices_large_body_chunk() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let db = Database::new(":memory:").await.unwrap();

        // mock 上游：返回 ~600KB 非流式 JSON 响应（单块远超协议 1MB 切块线）。
        // 必须是合法 JSON——网关故障转移循环对非流式 200 做 body 质量校验
        // （空/非 JSON → 502 重试），纯字节填充会被判为畸形响应。
        let big_body = format!(
            "{{\"id\":\"chatcmpl-big\",\"object\":\"chat.completion\",\"choices\":[{{\"index\":0,\"message\":{{\"role\":\"assistant\",\"content\":\"{}\"}},\"finish_reason\":\"stop\"}}]}}",
            "a".repeat(600 * 1024)
        )
        .into_bytes();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body_for_server = big_body.clone();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 8192];
            loop {
                match sock.read(&mut tmp).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&tmp[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                }
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\ncontent-type: application/json\r\n\r\n",
                body_for_server.len()
            );
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(&body_for_server).await;
        });

        save_provider_model(&db, "model-big", &format!("http://{addr}"), true).await;
        seed_configured_session(&db, "sess-big", "model-big").await;
        let gw = test_gateway(&db).await;

        let stream = forward(
            db,
            "sess-big".into(),
            "req-big".into(),
            gw,
            "/v1/chat/completions".into(),
            // 必须有合法 messages，否则 handler 在参数校验就 400，到不了上游
            br#"{"model":"gpt-test","messages":[{"role":"user","content":"hi"}],"stream":false}"#
                .to_vec(),
        );
        let chunks: Vec<AgentLlmProxyChunk> = stream.collect().await;

        // 关键断言：每块 ≤512KB；所有 data 拼接 == 上游 body；done 只在末块
        let mut joined = Vec::new();
        for (i, c) in chunks.iter().enumerate() {
            assert!(
                c.data.len() <= 512 * 1024,
                "chunk {i} exceeds 512KB: {} bytes",
                c.data.len()
            );
            if i + 1 < chunks.len() {
                assert!(!c.done, "done must only be on the final chunk");
            }
            joined.extend_from_slice(&c.data);
        }
        assert_eq!(
            joined, big_body,
            "sliced chunks must reassemble to the upstream body"
        );
        assert!(
            chunks.last().unwrap().done,
            "stream must end with done=true"
        );
        assert!(
            chunks.len() > 1,
            "600KB body should span multiple ≤512KB chunks, got {}",
            chunks.len()
        );
    }
}
