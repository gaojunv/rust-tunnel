//! 检索 / test-embedding / SSE 事件流（从旧 `rag/query_events.rs` 搬运，仅改挂载路径与 SSE 透传）。

use std::time::{Duration, Instant};

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
    Json,
};
use serde::Deserialize;
use tokio::sync::broadcast;

use crate::auth::validate_token;
use crate::llm::rag::embedder::Embedder;
use crate::llm::rag::retriever;
use crate::mgmt::api::dto::SseQuery;
use crate::mgmt::api::ApiState;

use super::{knowledge_rt, llm_state};

const SSE_TIMEOUT: Duration = Duration::from_secs(30);
const SSE_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Deserialize)]
pub struct TestEmbeddingRequest {
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    pub model: String,
    #[serde(default)]
    pub kb_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct QueryKnowledgeRequest {
    pub text: String,
}

/// 探测 embedding 服务：向 `POST {base_url}/embeddings` 发一条探针文本，返回维度与耗时。
pub async fn test_embedding(
    State(state): State<ApiState>,
    Json(body): Json<TestEmbeddingRequest>,
) -> impl IntoResponse {
    if body.base_url.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "base_url is required").into_response();
    }
    if body.model.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "model is required").into_response();
    }
    let mut api_key = body.api_key.clone();
    if let Some(kb_id) = body.kb_id.as_deref().filter(|s| !s.trim().is_empty()) {
        let rt = match knowledge_rt(&state).await {
            Ok(rt) => rt,
            Err(e) => return e.into_response(),
        };
        let Some(kb) = (match rt.db.ks_get(kb_id).await {
            Ok(r) => r,
            Err(e) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}"))
                    .into_response()
            }
        }) else {
            return (StatusCode::NOT_FOUND, "knowledge source not found").into_response();
        };
        if api_key.trim().is_empty() {
            api_key = crate::llm::crypto::decrypt_field(rt.cipher.as_ref(), &kb.emb_api_key)
                .unwrap_or_default();
        }
    }
    let started = Instant::now();
    let embedder = Embedder::new(&body.base_url, &api_key, &body.model);
    match embedder.embed_one("dimension probe").await {
        Ok(v) => {
            let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(0);
            Json(serde_json::json!({ "dimension": v.len(), "latency_ms": latency_ms }))
                .into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, format!("embedding failed: {e}")).into_response(),
    }
}

/// 检索知识库：embedding 查询向量 → top-K → 阈值过滤，返回命中 chunk 及分数。
pub async fn query_knowledge(
    State(state): State<ApiState>,
    Path(source_id): Path<String>,
    Json(body): Json<QueryKnowledgeRequest>,
) -> impl IntoResponse {
    let rt = match knowledge_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    if body.text.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "text is required").into_response();
    }
    let Some(kb) = (match rt.db.ks_get(&source_id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }) else {
        return (StatusCode::NOT_FOUND, "knowledge source not found").into_response();
    };
    let chunks = retriever::retrieve(&rt.db, &rt.store, rt.cipher.as_ref(), &kb, &body.text).await;
    let chunks: Vec<serde_json::Value> = chunks
        .into_iter()
        .map(|c| {
            serde_json::json!({
                "heading_path": c.heading_path,
                "content": c.content,
                "score": c.score,
            })
        })
        .collect();
    Json(serde_json::json!({ "chunks": chunks })).into_response()
}

/// GET /api/knowledge/events — SSE 事件流（文档摄入状态，含 `kind` 字段透传 `KbEvent`）。
/// token 走 query 参数认证（public 路由），keep-alive 30s，事件名 `knowledge`。
pub async fn sse_knowledge_events(
    State(state): State<ApiState>,
    Query(params): Query<SseQuery>,
) -> impl IntoResponse {
    if state.auth_config.is_enabled() {
        let token = params.token.as_deref().unwrap_or("");
        let is_valid =
            !token.is_empty() && validate_token(token, &state.auth_config.jwt_secret).is_ok();
        if !is_valid {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    }

    let Some(llm) = llm_state(&state).await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "LLM gateway not initialized",
        )
            .into_response();
    };
    let mut rx = llm.rag_tx.subscribe();
    let stream = async_stream::stream! {
        loop {
            match tokio::time::timeout(SSE_TIMEOUT, rx.recv()).await {
                Ok(Ok(ev)) => {
                    let json = serde_json::to_string(&ev).unwrap_or_default();
                    yield Ok::<_, std::convert::Infallible>(
                        Event::default().event("knowledge").data(json),
                    );
                }
                Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
                    yield Ok::<_, std::convert::Infallible>(
                        Event::default().event("sync").data(format!(r#"{{"lagged":{n}}}"#)),
                    );
                }
                Ok(Err(broadcast::error::RecvError::Closed)) => break,
                Err(_) => {
                    yield Ok::<_, std::convert::Infallible>(
                        Event::default().event("ping").data(""),
                    );
                }
            }
        }
    };
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(SSE_KEEPALIVE_INTERVAL))
        .into_response()
}
