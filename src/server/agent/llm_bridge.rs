//! Bridge AgentLlmProxyRequest to the server-side LLM gateway, streaming
//! response chunks back to the client over the control channel.
//!
//! 客户端内嵌 LLM 回环代理把 agent 进程的 LLM API 请求经控制通道转交服务端，
//! 本模块按 workspace 的 `llm_model_id` 解析 model → provider，在服务端注入
//! api key 后调上游，响应以 [`AgentLlmProxyChunk`] 流式返回。**LLM secret
//! 只在服务端接触，客户端永不持有。**

use futures_util::{Stream, StreamExt};
use serde_json::Value;

use crate::server::db::Database;
use crate::server::llm::crypto::{LlmCipher, decrypt_field};
use crate::server::llm::upstream;
use crate::server::persistence::db::llm::{LlmModelRecord, LlmProviderRecord};

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
/// 解析链路：`llm_model_id`（workspace 配置，由 AcpBridge 传入）→ model →
/// provider；provider 的 `api_key` 在服务端解密后注入。上游调用复用现有
/// [`upstream::call_upstream_raw`]（透传原 path/body，OpenAI 风格与 Anthropic
/// 风格请求都直接转发），流式/非流式按请求体里的 `stream` 字段判定。
///
/// # 契约
/// 无论成功/失败，流总是以 `done=true` 的 chunk 结束（见 [`AgentLlmProxyChunk`]）。
pub fn forward(
    db: Database,
    cipher: Option<LlmCipher>,
    session_id: String,
    request_id: String,
    path: String,
    body: Vec<u8>,
) -> impl Stream<Item = AgentLlmProxyChunk> {
    async_stream::stream! {
        // 1. 解析请求体：`stream` 字段决定走流式还是整包透传。
        let body_json: Value = match serde_json::from_slice(&body) {
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

        // 2. session → workspace → model → provider → 解密 api key
        //    （secret 服务端注入，客户端永不持有）。
        let (provider, model, api_key) = match resolve_model(&db, cipher.as_ref(), &session_id).await {
            Ok(v) => v,
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

        // 3. 复用现有 upstream 通路调上游（透传 path/body）。
        match upstream::call_upstream_raw(
            &provider.base_url,
            &api_key,
            &path,
            &body_json,
            is_stream,
        )
        .await
        {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let (_parts, body) = resp.into_parts();
                let mut stream = body.into_data_stream();
                while let Some(item) = stream.next().await {
                    match item {
                        Ok(bytes) => yield AgentLlmProxyChunk {
                            request_id: request_id.clone(),
                            data: bytes.to_vec(),
                            done: false,
                            status,
                        },
                        Err(e) => {
                            tracing::warn!(
                                session_id,
                                model = %model.model_name,
                                request_id = %request_id,
                                error = %e,
                                "llm proxy: upstream stream read failed"
                            );
                            yield AgentLlmProxyChunk {
                                request_id: request_id.clone(),
                                data: format!("upstream stream read failed: {e}").into_bytes(),
                                done: true,
                                status: 502,
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
            Err((status, msg)) => {
                yield AgentLlmProxyChunk {
                    request_id,
                    data: msg.into_bytes(),
                    done: true,
                    status: status.as_u16(),
                };
            }
        }
    }
}

/// `session_id` → workspace.llm_model_id → (provider, model, 解密后的 api key)。
async fn resolve_model(
    db: &Database,
    cipher: Option<&LlmCipher>,
    session_id: &str,
) -> Result<(LlmProviderRecord, LlmModelRecord, String), String> {
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
    let provider = db
        .llm_get_provider(&model.provider_id)
        .await
        .map_err(|e| format!("db error reading provider: {e}"))?
        .ok_or_else(|| format!("llm provider not found: {}", model.provider_id))?;
    if provider.enabled == 0 {
        return Err(format!("llm provider disabled: {}", provider.id));
    }
    let api_key = decrypt_field(cipher, &provider.api_key)
        .map_err(|e| format!("failed to decrypt provider api key: {e}"))?;
    Ok((provider, model, api_key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    /// 造 session → workspace(llm_model_id) → model → provider 全链路。
    /// provider api key 用明文（测试不配 cipher）。
    async fn seed_configured_session(db: &Database, session_id: &str, model_id: &str) {
        db.agent_create_workspace("w1", "proj", "nas", "host", "/workspace", None, None)
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
            "deepseek", // 合法 provider_type（表 CHECK 约束）
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
        // 空库：session 不存在 → 仅一个 502 done chunk
        let db = Database::new(":memory:").await.unwrap();
        let stream = forward(
            db,
            None,
            "sess-missing".into(),
            "req-1".into(),
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
        // workspace 存在但 llm_model_id 为空 → 502 done
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace("w1", "proj", "nas", "host", "/workspace", None, None)
            .await
            .unwrap();
        db.agent_create_session("sess-1", "w1", None, None)
            .await
            .unwrap();

        let stream = forward(
            db,
            None,
            "sess-1".into(),
            "req-1".into(),
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
            None,
            "sess-1".into(),
            "req-1".into(),
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
        // body 解析在模型解析之前：畸形 body → 400 done（无需 session）
        let db = Database::new(":memory:").await.unwrap();
        let stream = forward(
            db,
            None,
            "sess-1".into(),
            "req-1".into(),
            "/v1/chat/completions".into(),
            b"not json".to_vec(),
        );
        let chunks: Vec<AgentLlmProxyChunk> = stream.collect().await;
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].done);
        assert_eq!(chunks[0].status, 400);
    }

    #[tokio::test]
    async fn test_forward_offline_upstream_ends_with_502_done() {
        // provider 指向不可达地址：call_upstream_raw 连接失败 → 一个 502 done chunk
        let db = Database::new(":memory:").await.unwrap();
        save_provider_model(&db, "model-1", "http://127.0.0.1:1", true).await;
        seed_configured_session(&db, "sess-1", "model-1").await;

        let stream = forward(
            db,
            None,
            "sess-1".into(),
            "req-1".into(),
            "/v1/chat/completions".into(),
            br#"{"model":"gpt-test","stream":false}"#.to_vec(),
        );
        let chunks: Vec<AgentLlmProxyChunk> = stream.collect().await;
        assert_eq!(chunks.len(), 1, "upstream failure must emit exactly one chunk");
        assert!(chunks[0].done, "upstream failure must end with done=true");
        assert_eq!(chunks[0].status, 502);
    }
}
