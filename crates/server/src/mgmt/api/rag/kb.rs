//! RAG 知识库（KB）CRUD handlers。
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};

use crate::db::rag::RagKnowledgeBaseRecord;
use crate::llm::crypto::{decrypt_field, encrypt_field};
use crate::mgmt::api::ApiState;

use super::dto::{CreateKbRequest, UpdateKbRequest};
use super::{llm_state, rag_rt, RagRuntime};

/// KB 视图 JSON：`emb_api_key` 不回显（同 provider `api_key` 策略），额外带文档数。
fn kb_json(kb: &RagKnowledgeBaseRecord, doc_count: i64) -> serde_json::Value {
    serde_json::json!({
        "id": kb.id,
        "name": kb.name,
        "description": kb.description,
        "emb_base_url": kb.emb_base_url,
        "emb_api_key": "",
        "emb_model": kb.emb_model,
        "emb_dimension": kb.emb_dimension,
        "top_k": kb.top_k,
        "chunk_size": kb.chunk_size,
        "chunk_overlap": kb.chunk_overlap,
        "score_threshold": kb.score_threshold,
        "enabled": kb.enabled != 0,
        "doc_count": doc_count,
        "created_at": kb.created_at,
        "updated_at": kb.updated_at,
    })
}

/// 校验创建/更新 KB 的参数（分块参数一致性），返回 None 表示合法。
fn validate_kb_params(
    top_k: i64,
    chunk_size: i64,
    chunk_overlap: i64,
    score_threshold: f64,
) -> Option<String> {
    if !(1..=20).contains(&top_k) {
        return Some("top_k must be 1-20".to_string());
    }
    if chunk_size < 1 {
        return Some("chunk_size must be >= 1".to_string());
    }
    if chunk_overlap >= chunk_size {
        return Some("chunk_overlap must be < chunk_size".to_string());
    }
    if !(0.0..=1.0).contains(&score_threshold) {
        return Some("score_threshold must be in [0, 1]".to_string());
    }
    None
}

/// 解析创建 KB 的 embedding 配置：请求显式提供完整 embedding（base_url / model
/// 非空且 dimension ≥ 1）时用之；否则回退到全局共享配置（`agent_memory_settings`，
/// 与 AI 记忆体共用同一套 embedding）。全局也未配置时返回 400。返回值
/// `(emb_base_url, emb_api_key_raw, emb_model, emb_dimension)`，key 为解密后的明文，
/// 由调用方落库前再加密。
async fn resolve_kb_embedding(
    rt: &RagRuntime,
    body: &CreateKbRequest,
) -> Result<(String, String, String, i64), (StatusCode, String)> {
    let explicit = body
        .emb_base_url
        .as_deref()
        .is_some_and(|s| !s.trim().is_empty())
        || body
            .emb_model
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
        || body.emb_dimension.is_some();
    if explicit {
        // 任一显式提供即要求完整：避免「只填 model、漏 base_url」静默回退。
        let base = body.emb_base_url.as_deref().ok_or((
            StatusCode::BAD_REQUEST,
            "emb_base_url is required when customizing embedding".to_string(),
        ))?;
        let model = body.emb_model.as_deref().ok_or((
            StatusCode::BAD_REQUEST,
            "emb_model is required when customizing embedding".to_string(),
        ))?;
        let dim = body.emb_dimension.ok_or((
            StatusCode::BAD_REQUEST,
            "emb_dimension must be >= 1 (probe it via /test-embedding)".to_string(),
        ))?;
        if base.trim().is_empty() {
            return Err((
                StatusCode::BAD_REQUEST,
                "emb_base_url is required".to_string(),
            ));
        }
        if model.trim().is_empty() {
            return Err((StatusCode::BAD_REQUEST, "emb_model is required".to_string()));
        }
        if dim < 1 {
            return Err((
                StatusCode::BAD_REQUEST,
                "emb_dimension must be >= 1 (probe it via /test-embedding)".to_string(),
            ));
        }
        let key = body.emb_api_key.clone().unwrap_or_default();
        return Ok((base.to_string(), key, model.to_string(), dim));
    }

    // 回退全局共享配置
    let s = rt
        .db
        .memory_get_settings()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")))?;
    if s.emb_base_url.trim().is_empty() || s.emb_model.trim().is_empty() || s.emb_dimension < 1 {
        return Err((
            StatusCode::BAD_REQUEST,
            "embedding is not configured: set a global embedding config in the shared settings first"
                .to_string(),
        ));
    }
    let key = decrypt_field(rt.cipher.as_ref(), &s.emb_api_key).unwrap_or_default();
    Ok((s.emb_base_url, key, s.emb_model, s.emb_dimension))
}

pub async fn list_kbs(State(state): State<ApiState>) -> impl IntoResponse {
    let rt = match rag_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    let records = match rt.db.rag_list_kbs().await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    };
    let mut kbs = Vec::with_capacity(records.len());
    for kb in records {
        let doc_count = rt.db.rag_count_kb_docs(&kb.id).await.unwrap_or(0);
        kbs.push(kb_json(&kb, doc_count));
    }
    Json(serde_json::json!({ "knowledge_bases": kbs })).into_response()
}

pub async fn create_kb(
    State(state): State<ApiState>,
    Json(body): Json<CreateKbRequest>,
) -> impl IntoResponse {
    let rt = match rag_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    if body.name.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "name is required").into_response();
    }
    let top_k = body.top_k.unwrap_or(5);
    let chunk_size = body.chunk_size.unwrap_or(512);
    let chunk_overlap = body.chunk_overlap.unwrap_or(64);
    let score_threshold = body.score_threshold.unwrap_or(0.3);
    if let Some(err) = validate_kb_params(top_k, chunk_size, chunk_overlap, score_threshold) {
        return (StatusCode::BAD_REQUEST, err).into_response();
    }
    let enabled = body.enabled.unwrap_or(true);

    // embedding 配置：请求显式提供则用之，否则回退全局共享配置（见 resolve_kb_embedding）。
    let (emb_base_url, emb_api_key_raw, emb_model, emb_dimension) =
        match resolve_kb_embedding(&rt, &body).await {
            Ok(v) => v,
            Err(e) => return e.into_response(),
        };

    // 敏感字段落库前加密（AES-256-GCM；未配置主密钥时明文兼容，同 llm.rs 模式）
    let cipher = llm_state(&state).await.and_then(|l| l.cipher.clone());
    let emb_api_key = encrypt_field(cipher.as_ref(), &emb_api_key_raw);

    let id = uuid::Uuid::new_v4().to_string();
    if let Err(e) = rt
        .db
        .rag_create_kb(
            &id,
            &body.name,
            &body.description,
            &emb_base_url,
            &emb_api_key,
            &emb_model,
            emb_dimension,
            top_k,
            chunk_size,
            chunk_overlap,
            score_threshold,
            enabled,
        )
        .await
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response();
    }

    (
        StatusCode::CREATED,
        Json(serde_json::json!({ "status": "ok", "id": id })),
    )
        .into_response()
}

pub async fn get_kb(State(state): State<ApiState>, Path(id): Path<String>) -> impl IntoResponse {
    let rt = match rag_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    let Some(kb) = (match rt.db.rag_get_kb(&id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }) else {
        return (StatusCode::NOT_FOUND, "knowledge base not found").into_response();
    };
    let doc_count = rt.db.rag_count_kb_docs(&id).await.unwrap_or(0);
    Json(kb_json(&kb, doc_count)).into_response()
}

pub async fn update_kb(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateKbRequest>,
) -> impl IntoResponse {
    let rt = match rag_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    let Some(existing) = (match rt.db.rag_get_kb(&id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }) else {
        return (StatusCode::NOT_FOUND, "knowledge base not found").into_response();
    };
    if body.name.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "name is required").into_response();
    }
    let top_k = body.top_k.unwrap_or(5);
    let chunk_size = body.chunk_size.unwrap_or(512);
    let chunk_overlap = body.chunk_overlap.unwrap_or(64);
    let score_threshold = body.score_threshold.unwrap_or(0.3);
    if let Some(err) = validate_kb_params(top_k, chunk_size, chunk_overlap, score_threshold) {
        return (StatusCode::BAD_REQUEST, err).into_response();
    }

    // ── emb 配置合并（编辑态 merge 语义）──
    // 未提供（None）= 沿用当前值；显式空字符串 = 非法（前端始终预填完整值，空串
    // 只可能是误清空，直接 400 好过静默保留）。api_key 例外：空 = 保留旧密钥
    // （后端不回显密钥，前端拿不到旧值，留空必须合法）。
    let new_base = match body.emb_base_url.as_deref() {
        None => existing.emb_base_url.clone(),
        Some(s) if s.trim().is_empty() => {
            return (StatusCode::BAD_REQUEST, "emb_base_url must not be empty").into_response();
        }
        Some(s) => s.trim().to_string(),
    };
    let new_model = match body.emb_model.as_deref() {
        None => existing.emb_model.clone(),
        Some(s) if s.trim().is_empty() => {
            return (StatusCode::BAD_REQUEST, "emb_model must not be empty").into_response();
        }
        Some(s) => s.trim().to_string(),
    };
    let new_dim = match body.emb_dimension {
        None => existing.emb_dimension,
        Some(d) if d < 1 => {
            return (
                StatusCode::BAD_REQUEST,
                "emb_dimension must be >= 1 (probe it via /test-embedding)",
            )
                .into_response();
        }
        Some(d) => d,
    };
    let new_api_key_raw = body
        .emb_api_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let emb_touched = body.emb_base_url.is_some()
        || body.emb_model.is_some()
        || body.emb_dimension.is_some()
        || new_api_key_raw.is_some();

    // 未携带任何 emb 字段：保持既有行为，只更新名称/描述/检索参数。
    if !emb_touched {
        if let Err(e) = rt
            .db
            .rag_update_kb_params(
                &id,
                &body.name,
                &body.description,
                top_k,
                chunk_size,
                chunk_overlap,
                score_threshold,
            )
            .await
        {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response();
        }
        return Json(serde_json::json!({ "status": "ok" })).into_response();
    }

    // 向量只取决于 (base_url, model, dimension)：三者任一变化 → 全量重建；
    // 仅 api_key 变化（密钥轮换）只替换密文，不触发昂贵的重建。
    let rebuild_needed = new_base != existing.emb_base_url
        || new_model != existing.emb_model
        || new_dim != existing.emb_dimension;

    let cipher = llm_state(&state).await.and_then(|l| l.cipher.clone());
    let new_api_key_enc = new_api_key_raw.map_or_else(
        || existing.emb_api_key.clone(),
        |k| encrypt_field(cipher.as_ref(), k),
    );

    if let Err(e) = rt
        .db
        .rag_update_kb_full(
            &id,
            &body.name,
            &body.description,
            top_k,
            chunk_size,
            chunk_overlap,
            score_threshold,
            &new_base,
            &new_api_key_enc,
            &new_model,
            new_dim,
        )
        .await
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response();
    }

    if !rebuild_needed {
        return Json(serde_json::json!({ "status": "ok" })).into_response();
    }

    // ── 全量重建：emb 配置已变，旧向量（维度固化在 shard）全部失效 ──
    // 顺序：软关挡新上传/检索 → 擦 shard（含缓存移除）→ 清 SQLite 分块 →
    // 逐文档 reindex（复用单文档流程，原文在 rag_docs/ 无需重传）→ 恢复启停。
    // 并发取舍与 delete_kb 同源：擦 shard 瞬间若有编辑前已在途的摄入任务持有旧
    // EdgeShard，其 Drop flush 可能任务级 panic（管理面低频操作，可接受）。
    let was_enabled = existing.enabled != 0;
    let _ = rt.db.rag_toggle_kb(&id, false).await;
    if let Err(e) = rt.store.delete_kb(&id).await {
        tracing::warn!(kb_id = %id, error = %e, "rag: store delete_kb (rebuild) failed");
    }
    if let Err(e) = rt.db.rag_delete_chunks_by_kb(&id).await {
        let _ = rt.db.rag_toggle_kb(&id, was_enabled).await;
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response();
    }
    let kb_new = match rt.db.rag_get_kb(&id).await {
        Ok(Some(k)) => k,
        Ok(None) => return (StatusCode::NOT_FOUND, "knowledge base not found").into_response(),
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    };
    let docs = match rt.db.rag_list_documents(&id).await {
        Ok(d) => d,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    };

    // 限并发 4：大库一次 spawn 全部摄入会瞬时打满远端 embedding 服务。
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(4));
    let mut reindexed = 0i64;
    let mut skipped = 0i64;
    let mut missing_source = 0i64;
    for doc in &docs {
        match super::docs::reindex_kb_doc(&rt, &kb_new, doc, Some(sem.clone())).await {
            Ok(super::docs::ReindexOutcome::Spawned) => reindexed += 1,
            Ok(super::docs::ReindexOutcome::Skipped) => skipped += 1,
            Ok(super::docs::ReindexOutcome::MissingSource) => missing_source += 1,
            Err(e) => {
                let _ = rt.db.rag_toggle_kb(&id, was_enabled).await;
                return e.into_response();
            }
        }
    }
    // 恢复原来的启停状态（不能硬置 true：编辑前可能就是禁用态）。
    let _ = rt.db.rag_toggle_kb(&id, was_enabled).await;

    Json(serde_json::json!({
        "status": "ok",
        "reindexed": reindexed,
        "skipped": skipped,
        "missing_source": missing_source,
    }))
    .into_response()
}

pub async fn patch_kb(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let rt = match rag_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    let Some(_existing) = (match rt.db.rag_get_kb(&id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }) else {
        return (StatusCode::NOT_FOUND, "knowledge base not found").into_response();
    };
    // 类型校验前置：非布尔 enabled 直接 400，避免 `as_bool().unwrap_or(false)`
    // 把一次类型错误静默变成「禁用 KB」（对齐 api-key PATCH 的校验语义）。
    let enabled = match body.get("enabled") {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(_) => return (StatusCode::BAD_REQUEST, "enabled must be a boolean").into_response(),
        None => return (StatusCode::BAD_REQUEST, "enabled is required").into_response(),
    };
    if let Err(e) = rt.db.rag_toggle_kb(&id, enabled).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response();
    }
    Json(serde_json::json!({ "status": "ok" })).into_response()
}

pub async fn delete_kb(State(state): State<ApiState>, Path(id): Path<String>) -> impl IntoResponse {
    let rt = match rag_rt(&state).await {
        Ok(rt) => rt,
        Err(e) => return e.into_response(),
    };
    let Some(_kb) = (match rt.db.rag_get_kb(&id).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response()
        }
    }) else {
        return (StatusCode::NOT_FOUND, "knowledge base not found").into_response();
    };

    // 先软关（enabled=0）：避免删除窗口内检索路径继续命中该库。
    let _ = rt.db.rag_toggle_kb(&id, false).await;

    // 并发约束：store.delete_kb 与在途 ingest/upsert 并发可能触发 EdgeShard Drop
    // flush panic（目录已删），见 store::VectorStore::delete_kb 文档。管理面操作低频，
    // 此处不额外加锁（YAGNI），依赖「软关 + 后台 ingest 短暂窗口」的务实取舍：
    // 窗口期内若在途 ingest 已发出分块写，DB 记录随级联删除一并清除，向量残留可忽略。
    if let Err(e) = rt.store.delete_kb(&id).await {
        tracing::warn!(kb_id = %id, error = %e, "rag: store delete_kb failed");
    }
    // 清理该库全部原文文件（best-effort，失败仅 warn）
    let source_dir = rt.store.data_dir().join("rag_docs").join(&id);
    if source_dir.exists() {
        if let Err(e) = tokio::fs::remove_dir_all(&source_dir).await {
            tracing::warn!(kb_id = %id, error = %e, "rag: remove kb doc source dir failed");
        }
    }
    if let Err(e) = rt.db.rag_delete_kb(&id).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response();
    }
    StatusCode::OK.into_response()
}
