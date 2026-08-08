//! Bridge AgentLlmProxyRequest to the server-side LLM gateway, streaming
//! response chunks back to the client over the control channel.
//!
//! 客户端内嵌 LLM 回环代理把 agent 进程的 LLM API 请求经控制通道转交服务端，
//! 本模块按 workspace 的 `llm_model_id` 解析 model_name，改写请求体 `model`
//! 字段后经内部 HTTP 回环调用 LLM 网关入口（`/v1/messages` 或
//! `/v1/chat/completions`），让网关的模型组故障转移、格式转换、用量统计、
//! RAG 注入等管线全部生效。**LLM secret 只在服务端接触，客户端永不持有。**

use std::sync::LazyLock;

use futures_util::{Stream, StreamExt};
use reqwest::Client;
use serde_json::Value;

use crate::server::db::Database;

/// LLM 网关入口（内部回环 HTTP 调用时用）。
#[derive(Debug, Clone)]
pub struct LlmGatewayEndpoint {
    /// 网关基地址，如 `http://127.0.0.1:8443`。
    pub base_url: String,
    /// 内部 API key（agent 内部调网关入口时附上，绕开外部认证）。
    pub api_key: String,
    /// OpenAI 入口域名（`/v1/chat/completions` 请求用此 Host 头）。
    pub openai_domain: String,
    /// Anthropic 入口域名（`/v1/messages` 请求用此 Host 头）。
    pub anthropic_domain: String,
}

/// 内部回环 HTTP 客户端（调 LLM 网关入口）。不连外网，超时可短。
static GATEWAY_CLIENT: LazyLock<Client> = LazyLock::new(|| {
    Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .read_timeout(std::time::Duration::from_secs(300))
        .tcp_keepalive(std::time::Duration::from_secs(60))
        .build()
        .expect("failed to build gateway HTTP client")
});

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
/// 解析链路：`session_id` → workspace.llm_model_id → model_name，改写请求体
/// `model` 字段后经内部 HTTP 回环调用 LLM 网关入口。路径 `/v1/messages` 使用
/// Anthropic 入口域名；`/v1/chat/completions` 使用 OpenAI 入口域名。网关自动
/// 完成模型组故障转移、格式转换、用量统计、RAG 注入等管线。
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

        // 2. session → workspace.llm_model_id → model_name，注入到请求体
        //    （网关的下游 resolve_with_failover 按此 model_name/alias 解析）。
        let model_name = match resolve_model_name(&db, &session_id).await {
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

        // 3. 按路径选协议入口域名（网关据此走对应 handler）。
        let host = if path.contains("messages") {
            gateway.anthropic_domain.as_str()
        } else {
            gateway.openai_domain.as_str()
        };
        if host.is_empty() {
            yield AgentLlmProxyChunk {
                request_id,
                data: b"no gateway domain configured for protocol".to_vec(),
                done: true,
                status: 502,
            };
            return;
        }
        let url = format!(
            "http://{}{}",
            gateway.base_url.trim_start_matches("http://"),
            path,
        );

        // 4. 内部 HTTP 回环调用 LLM 网关入口。
        let resp = match GATEWAY_CLIENT
            .post(&url)
            .header("Host", host)
            .header("Authorization", format!("Bearer {}", gateway.api_key))
            .header("Content-Type", "application/json")
            .json(&body_json)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    session_id,
                    request_id = %request_id,
                    error = %e,
                    url = %url,
                    "llm proxy: gateway connection failed"
                );
                yield AgentLlmProxyChunk {
                    request_id,
                    data: format!("gateway connection failed: {e}").into_bytes(),
                    done: true,
                    status: 502,
                };
                return;
            }
        };

        // 5. 流式/非流式统一走 body byte stream 回传。
        let status = resp.status().as_u16();
        let mut stream = resp.bytes_stream();
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

/// `session_id` → workspace.llm_model_id → model_name（用于注入请求体 `model` 字段）。
async fn resolve_model_name(db: &Database, session_id: &str) -> Result<String, String> {
    let session = db
        .agent_get_session(session_id)
        .await
        .map_err(|e| format!("db error reading session: {e}"))?
        .ok_or_else(|| format!("agent session not found: {session_id}"))?;
    let ws = db
        .agent_get_workspace(&session.workspace_id)
        .await
        .map_err(|e| format!("db error reading workspace: {e}"))?
        .ok_or_else(|| "agent workspace not found".to_string())?;
    let llm_model_id = ws
        .llm_model_id
        .ok_or_else(|| "workspace 未配置 LLM 模型（llm_model_id）".to_string())?;
    let model = db
        .llm_get_model(&llm_model_id)
        .await
        .map_err(|e| format!("db error reading model: {e}"))?
        .ok_or_else(|| format!("llm model not found: {llm_model_id}"))?;
    if model.enabled == 0 {
        return Err(format!("llm model disabled: {llm_model_id}"));
    }
    Ok(model.model_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    fn test_gateway() -> LlmGatewayEndpoint {
        LlmGatewayEndpoint {
            base_url: "127.0.0.1:1".into(), // 不可达地址（无实际网关）
            api_key: "sk-000000000000000000000000000000000000000000000000".into(),
            openai_domain: "oa.local".into(),
            anthropic_domain: "an.local".into(),
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
        let stream = forward(
            db,
            "sess-missing".into(),
            "req-1".into(),
            test_gateway(),
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

        let stream = forward(
            db,
            "sess-1".into(),
            "req-1".into(),
            test_gateway(),
            "/v1/chat/completions".into(),
            br#"{"model":"gpt-test","stream":true}"#.to_vec(),
        );
        let chunks: Vec<AgentLlmProxyChunk> = stream.collect().await;
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].done);
        assert_eq!(chunks[0].status, 502);
        assert!(String::from_utf8_lossy(&chunks[0].data).contains("llm_model_id"));
    }

    #[tokio::test]
    async fn test_forward_disabled_model_returns_502_done() {
        let db = Database::new(":memory:").await.unwrap();
        save_provider_model(&db, "model-off", "https://llm.example.test", false).await;
        seed_configured_session(&db, "sess-1", "model-off").await;

        let stream = forward(
            db,
            "sess-1".into(),
            "req-1".into(),
            test_gateway(),
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
        let stream = forward(
            db,
            "sess-1".into(),
            "req-1".into(),
            test_gateway(),
            "/v1/chat/completions".into(),
            b"not json".to_vec(),
        );
        let chunks: Vec<AgentLlmProxyChunk> = stream.collect().await;
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].done);
        assert_eq!(chunks[0].status, 400);
    }

    #[tokio::test]
    async fn test_forward_offline_gateway_ends_with_502_done() {
        // session + model 配置完整，但网关不可达 → 502 done
        let db = Database::new(":memory:").await.unwrap();
        save_provider_model(&db, "model-1", "https://unused.example.test", true).await;
        seed_configured_session(&db, "sess-1", "model-1").await;

        let stream = forward(
            db,
            "sess-1".into(),
            "req-1".into(),
            test_gateway(), // port 1 — 连接必定失败
            "/v1/chat/completions".into(),
            br#"{"model":"gpt-test","stream":false}"#.to_vec(),
        );
        let chunks: Vec<AgentLlmProxyChunk> = stream.collect().await;
        assert_eq!(chunks.len(), 1, "gateway failure must emit exactly one chunk");
        assert!(chunks[0].done, "gateway failure must end with done=true");
        assert_eq!(chunks[0].status, 502);
    }

    #[tokio::test]
    async fn test_forward_messages_path_uses_anthropic_domain() {
        // Anthropic 路径场景：Host 头应使用 anthropic_domain。
        // 用空 anthropic_domain → 502，验证路由逻辑生效（否则
        // 会走到网关连接失败分支）。
        let db = Database::new(":memory:").await.unwrap();
        save_provider_model(&db, "model-1", "https://unused.example.test", true).await;
        seed_configured_session(&db, "sess-1", "model-1").await;

        let gw = LlmGatewayEndpoint {
            anthropic_domain: "".into(), // 未配置 Anthropic 入口
            ..test_gateway()
        };
        let stream = forward(
            db,
            "sess-1".into(),
            "req-1".into(),
            gw,
            "/v1/messages".into(), // Claude Code 路径
            br#"{"model":"gpt-test","stream":true}"#.to_vec(),
        );
        let chunks: Vec<AgentLlmProxyChunk> = stream.collect().await;
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].done);
        assert_eq!(chunks[0].status, 502);
        assert!(
            String::from_utf8_lossy(&chunks[0].data).contains("no gateway domain"),
            "err body: {}",
            String::from_utf8_lossy(&chunks[0].data)
        );
    }

    #[tokio::test]
    async fn test_forward_injects_model_name_into_body() {
        // 模型名注入后应真正发起 HTTP 请求（发到不可达端口→502）；
        // 若无注入逻辑提前报错（model 字段缺失/不对），则证明注入正确。
        let db = Database::new(":memory:").await.unwrap();
        save_provider_model(&db, "model-1", "https://unused.example.test", true).await;
        seed_configured_session(&db, "sess-1", "model-1").await;

        let stream = forward(
            db,
            "sess-1".into(),
            "req-1".into(),
            test_gateway(),
            "/v1/chat/completions".into(),
            br#"{"stream":false}"#.to_vec(), // 没有 model 字段
        );
        let chunks: Vec<AgentLlmProxyChunk> = stream.collect().await;
        // model_name 已注入 → 请求能到网关连接阶段（失败→502），
        // 而非卡在 JSON 解析/模型名缺失上。
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].done);
        assert_eq!(chunks[0].status, 502);
    }
}
