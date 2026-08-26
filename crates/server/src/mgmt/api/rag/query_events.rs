//! RAG query / test-embedding / SSE 事件流 handlers。
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
use tokio::sync::broadcast;

use crate::auth::validate_token;
use crate::llm::rag::embedder::Embedder;
use crate::llm::rag::retriever;
use crate::mgmt::api::dto::SseQuery;
use crate::mgmt::api::ApiState;

use super::dto::{QueryKbRequest, TestEmbeddingRequest};
use super::{llm_state, rag_rt};

/// SSE 订阅等待与保活间隔：30s，超时发 ping 保活。
const SSE_TIMEOUT: Duration = Duration::from_secs(30);
/// SSE KeepAlive 间隔：30s，与订阅超时对齐。
const SSE_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// 探测 embedding 服务：向 `POST {base_url}/embeddings` 发一条探针文本，返回维度与耗时。
/// 前端据此填写 KB 的 `emb_dimension`。
///
/// `kb_id` 可选（编辑 KB 场景）：提供时若请求 `api_key` 为空，则用该 KB 已存的密钥
/// （后端不回显密钥，前端编辑态拿不到旧值，留空测试必须能复用已存密钥）；
/// 请求带新密钥则以请求为准。`base_url`/`model` 始终以请求体为准（测的就是新值）。
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
        let rt = match rag_rt(&state).await {
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
            return (StatusCode::NOT_FOUND, "knowledge base not found").into_response();
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
pub async fn query_kb(
    State(state): State<ApiState>,
    Path(kb_id): Path<String>,
    Json(body): Json<QueryKbRequest>,
) -> impl IntoResponse {
    let rt = match rag_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    if body.text.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "text is required").into_response();
    }
    let Some(kb) = (match rt.db.ks_get(&kb_id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }) else {
        return (StatusCode::NOT_FOUND, "knowledge base not found").into_response();
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

/// 把统一的 `KbEvent` 映射回 KB SSE 的既有形态。
///
/// 统一流水线的事件多了 `kind` 字段，这里显式挑字段而非直接 serialize：前端的
/// 契约是 `{doc_id, kb_id, status, chunk_count, error}`，多发字段虽无害，但把
/// 转形写明能让批 5 统一前端时一眼看出该收敛什么。
fn kb_event_json(ev: &crate::llm::rag::ingest::KbEvent) -> serde_json::Value {
    serde_json::json!({
        "doc_id": ev.doc_id,
        "kb_id": ev.kb_id,
        "status": ev.status,
        "chunk_count": ev.chunk_count,
        "error": ev.error,
    })
}

/// GET /api/llm/kb/events — SSE 事件流（文档摄入状态）。token 走 query 参数认证
/// （public 路由，参照 `/api/logs/stream`），keep-alive 30s。
pub async fn sse_kb_events(
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
                    // 统一 channel 承载两个索引的事件；本端点只关心向量索引。
                    if ev.kind != crate::db::knowledge::IndexKind::Vector {
                        continue;
                    }
                    let json = serde_json::to_string(&kb_event_json(&ev)).unwrap_or_default();
                    yield Ok::<_, std::convert::Infallible>(
                        Event::default().event("kb").data(json),
                    );
                }
                Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
                    yield Ok::<_, std::convert::Infallible>(
                        Event::default().event("sync").data(format!(r#"{{"lagged":{n}}}"#)),
                    );
                }
                Ok(Err(broadcast::error::RecvError::Closed)) => break,
                Err(_) => {
                    // 超时 → ping 保活
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
