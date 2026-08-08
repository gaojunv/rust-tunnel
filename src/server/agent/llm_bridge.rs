//! Bridge AgentLlmProxyRequest to the server-side LLM gateway, streaming
//! response chunks back to the client over the control channel.
//!
//! 客户端内嵌 LLM 回环代理把 agent 进程的 LLM API 请求经控制通道转交服务端，
//! 本模块按 workspace 的 `llm_model_id` 解析 model_name，改写请求体 `model`
//! 字段后**直接函数调用** LLM 网关 handler（`handle_messages` /
//! `handle_chat_completions`），让网关的模型组故障转移、格式转换、用量统计、
//! RAG 注入等管线全部生效——与外部 HTTP 流量共享同一条代码路径。
//! **LLM secret 只在服务端接触，客户端永不持有。**

use std::sync::Arc;

use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue};
use axum::Json;
use futures_util::{Stream, StreamExt};
use serde_json::Value;

use crate::server::db::Database;
use crate::server::llm::openai_handler::LlmHandlerState;
use crate::server::llm::{anthropic_handler, openai_handler, LlmProtocol, LlmState};

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
/// `/v1/chat/completions` → OpenAI 入口）。网关自动完成模型组故障转移、
/// 格式转换、用量统计、RAG 注入等管线。
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

        // 2. 统一模型解析（session → workspace → 全局默认 → 第一个可用），
        //    注入到请求体（网关的下游 resolve_with_failover 按此引用解析）。
        let model_name = match super::session::resolve_effective_model(
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
        };
        body_json["model"] = Value::String(model_name);

        // 3. 按路径分发到协议入口 handler（与 shared_listener 的 llm_handle
        //    白名单一致；path 可能带 query，如 `/v1/messages?beta=true`）。
        let clean_path = path.split('?').next().unwrap_or("/");
        let is_messages = clean_path == "/v1/messages";
        let is_chat_completions = clean_path == "/v1/chat/completions";
        let is_models = clean_path == "/v1/models";
        if !is_messages && !is_chat_completions && !is_models {
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
        } else {
            openai_handler::handle_chat_completions(State(handler_state), headers, Json(body_json)).await
        };

        // 5. 流式/非流式统一走 body data stream 回传。
        let status = resp.status().as_u16();
        let mut stream = resp.into_body().into_data_stream();
        while let Some(item) = stream.next().await {
            match item {
                Ok(bytes) => {
                    yield AgentLlmProxyChunk {
                        request_id: request_id.clone(),
                        data: bytes.to_vec(),
                        done: false,
                        status,
                    };
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
    use crate::server::llm::auth::generate_api_key;
    use futures_util::StreamExt;

    /// 构造带 DB 的 endpoint：API key 写入 DB（authenticate 走 hash 查询）。
    async fn test_gateway(db: &Database) -> LlmGatewayEndpoint {
        let (raw_key, key_hash, key_prefix) = generate_api_key();
        db.llm_save_api_key("__acp_internal__", &key_hash, &key_prefix, "ACP Internal", None)
            .await
            .unwrap();
        LlmGatewayEndpoint {
            llm_state: Arc::new(LlmState::new(Some(db.clone()), None)),
            api_key: raw_key,
        }
    }

    /// 造 session → workspace(llm_model_id) → model → provider 全链路。
    async fn seed_configured_session(db: &Database, session_id: &str, model_id: &str) {
        db.agent_create_workspace("w1", "proj", "nas", "host", "/workspace", None, None, "", None, None)
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
        db.llm_save_model(model_id, "prov-1", "gpt-test", "gpt-test", "", enabled, None)
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
        db.agent_create_workspace("w1", "proj", "nas", "host", "/workspace", None, None, "", None, None)
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
        assert!(last.status >= 400, "upstream failure → error status, got {}", last.status);
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
}
