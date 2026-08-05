use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};

use super::dto::{
    BreakerSnapshotView, GroupMemberView, ModelGroupDetailView, ModelGroupRequest, ModelGroupView,
    ReplaceMembersRequest,
};
use super::ApiState;
use crate::server::llm::{
    auth::generate_api_key,
    breaker::ModelBreakers,
    crypto::{decrypt_field, encrypt_field, LlmCipher},
    provider::{is_valid_provider_type, resolve_base_url, VALID_PROVIDER_TYPES},
    ApiKeyView, CreateApiKeyRequest, CreateApiKeyResponse, LlmGatewayConfig, ModelConfig,
    ModelRequest, ProviderConfig, ProviderRequest,
};

/// 取当前 LLM 字段加密器（未初始化 / 未配置主密钥时为 None → 明文兼容）。
async fn llm_cipher(state: &ApiState) -> Option<LlmCipher> {
    let guard = state.server_state.proxy_state.llm_state.read().await;
    guard.as_ref().and_then(|l| l.cipher.clone())
}

// ── Gateway config ────────────────────────────────────────────

pub async fn get_gateway_config(State(state): State<ApiState>) -> impl IntoResponse {
    let llm_guard = state.server_state.proxy_state.llm_state.read().await;
    match llm_guard.as_ref() {
        Some(llm) => {
            let cfg = llm.gateway_config.read().await;
            Json(serde_json::to_value(&*cfg).unwrap_or_default()).into_response()
        }
        None => Json(serde_json::json!({"enabled": false})).into_response(),
    }
}

pub async fn update_gateway_config(
    State(state): State<ApiState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let enabled = body["enabled"].as_bool().unwrap_or(false);

    // 字段名改为 openai_domain / anthropic_domain；兼容旧 domain 字段
    let openai_domain = body["openai_domain"]
        .as_str()
        .or_else(|| body["domain"].as_str())
        .unwrap_or("")
        .to_string();
    let openai_domain = if openai_domain.is_empty() {
        None
    } else {
        Some(openai_domain)
    };
    let anthropic_domain = body["anthropic_domain"].as_str().unwrap_or("").to_string();
    let anthropic_domain = if anthropic_domain.is_empty() {
        None
    } else {
        Some(anthropic_domain)
    };

    let listen = body["listen"].as_str().unwrap_or("0.0.0.0:443").to_string();
    let tls_enabled = body["tls_enabled"].as_bool().unwrap_or(false);
    let tls_acme = body["tls_acme"].as_bool().unwrap_or(false);

    let config = LlmGatewayConfig {
        enabled,
        openai_domain: openai_domain.clone(),
        anthropic_domain: anthropic_domain.clone(),
        listen: listen.clone(),
        tls_enabled,
        tls_acme,
    };

    // 校验
    if let Some(err) = config.validate() {
        return (StatusCode::BAD_REQUEST, err).into_response();
    }

    // 收集所有配置的域名
    let domains = config.configured_domains();

    let rule_id = "__llm_gateway__".to_string();
    let tls = if tls_enabled {
        Some(crate::server::reverse_proxy::ProxyTlsConfig {
            enabled: true,
            acme: tls_acme,
            domain: openai_domain.clone().or_else(|| anthropic_domain.clone()),
        })
    } else {
        None
    };

    let rule = crate::server::reverse_proxy::ProxyRule {
        id: rule_id.clone(),
        name: "LLM Gateway".into(),
        rule_type: crate::server::reverse_proxy::RuleType::Llm,
        listen: listen.clone(),
        domains,
        routes: vec![],
        tls,
        enabled,
        created_at: None,
        cert_status: None,
    };

    // Step 1: Persist to DB first (fail early if DB is unavailable)
    if let Err(e) = state.server_state.proxy_state.save_rule(&rule).await {
        tracing::error!("Failed to persist LLM gateway rule: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to persist gateway config: {}", e),
        )
            .into_response();
    }

    // Step 2: Update in-memory rules
    {
        let mut rules = state.server_state.proxy_state.rules.lock().await;
        rules.insert(rule_id.clone(), rule);
    }

    // Step 3: Update in-memory gateway config
    {
        let llm_guard = state.server_state.proxy_state.llm_state.read().await;
        if let Some(llm) = llm_guard.as_ref() {
            *llm.gateway_config.write().await = Some(config.clone());
        }
    }

    // Step 4: Reconcile listener
    if let Err(e) = state
        .server_state
        .proxy_state
        .reconcile_http_listener(&listen)
        .await
    {
        tracing::error!("Failed to reconcile LLM listener: {}", e);
    }

    Json(serde_json::json!({"status": "ok"})).into_response()
}

// ── Provider CRUD ─────────────────────────────────────────────

pub async fn list_providers(State(state): State<ApiState>) -> impl IntoResponse {
    let db = match state.server_state.db() {
        Some(db) => db,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "no database").into_response(),
    };

    let records = match db.llm_list_providers().await {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("DB error: {}", e),
            )
                .into_response()
        }
    };

    // extra_config 落库前已加密，回显给前端时需解密；历史明文原样透传，
    // 解密失败（如主密钥轮换）降级为 None 而不是让整个列表请求失败。
    let cipher = llm_cipher(&state).await;
    let providers: Vec<ProviderConfig> = records
        .into_iter()
        .map(|r| {
            let extra_config =
                r.extra_config
                    .and_then(|ec| match decrypt_field(cipher.as_ref(), &ec) {
                        Ok(pt) => Some(pt),
                        Err(e) => {
                            tracing::warn!(
                                "failed to decrypt extra_config for provider {}: {}",
                                r.id,
                                e
                            );
                            None
                        }
                    });
            ProviderConfig {
                id: r.id,
                name: r.name,
                provider_type: r.provider_type,
                base_url: r.base_url,
                api_key: String::new(),
                extra_config,
                anthropic_base_url: r.anthropic_base_url,
                enabled: r.enabled != 0,
                created_at: r.created_at,
                updated_at: r.updated_at,
            }
        })
        .collect();

    Json(serde_json::json!({"providers": providers})).into_response()
}

pub async fn create_provider(
    State(state): State<ApiState>,
    Json(body): Json<ProviderRequest>,
) -> impl IntoResponse {
    let db = match state.server_state.db() {
        Some(db) => db,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "no database").into_response(),
    };

    if body.name.is_empty() {
        return (StatusCode::BAD_REQUEST, "name is required").into_response();
    }

    if !is_valid_provider_type(&body.provider_type) {
        return (
            StatusCode::BAD_REQUEST,
            format!("invalid provider_type. valid: {:?}", VALID_PROVIDER_TYPES),
        )
            .into_response();
    }

    let base_url = match resolve_base_url(&body.provider_type, &body.base_url) {
        Ok(u) => u,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };

    let id = uuid::Uuid::new_v4().to_string();

    // 敏感字段落库前加密（AES-256-GCM；未配置主密钥时明文兼容）
    let cipher = llm_cipher(&state).await;
    let api_key = encrypt_field(cipher.as_ref(), &body.api_key);
    // 新建：null 或缺失均视为“无配置”
    let extra_config = body
        .extra_config
        .flatten()
        .map(|ec| encrypt_field(cipher.as_ref(), &ec));

    if let Err(e) = db
        .llm_save_provider(
            &id,
            &body.name,
            &body.provider_type,
            &base_url,
            &api_key,
            extra_config.as_deref(),
            body.anthropic_base_url.as_deref(),
            true,
        )
        .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("DB error: {}", e),
        )
            .into_response();
    }

    (
        StatusCode::CREATED,
        Json(serde_json::json!({"status": "ok", "id": id})),
    )
        .into_response()
}

pub async fn update_provider(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<ProviderRequest>,
) -> impl IntoResponse {
    let db = match state.server_state.db() {
        Some(db) => db,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "no database").into_response(),
    };

    let existing = match db.llm_get_provider(&id).await {
        Ok(Some(r)) => r,
        Ok(None) => return (StatusCode::NOT_FOUND, "provider not found").into_response(),
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("DB error: {}", e),
            )
                .into_response()
        }
    };

    if !is_valid_provider_type(&body.provider_type) {
        return (
            StatusCode::BAD_REQUEST,
            format!("invalid provider_type. valid: {:?}", VALID_PROVIDER_TYPES),
        )
            .into_response();
    }

    let base_url = match resolve_base_url(&body.provider_type, &body.base_url) {
        Ok(u) => u,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };

    let enabled = existing.enabled != 0;
    let cipher = llm_cipher(&state).await;

    // 敏感字段的“未提供即保留”语义：
    // - api_key 为空 → 沿用已存密文（不要二次加密）
    // - extra_config 三态：字段缺失 → 沿用已有值；显式 null → 清除；字符串 → 加密后覆盖
    let api_key = if body.api_key.is_empty() {
        existing.api_key.clone()
    } else {
        encrypt_field(cipher.as_ref(), &body.api_key)
    };
    let extra_config = match &body.extra_config {
        None => existing.extra_config.clone(),
        Some(None) => None,
        Some(Some(ec)) => Some(encrypt_field(cipher.as_ref(), ec)),
    };
    // anthropic_base_url: None 表示不修改，Some 表示更新（含清空）
    let anthropic_base_url = body
        .anthropic_base_url
        .as_deref()
        .or(existing.anthropic_base_url.as_deref());

    if let Err(e) = db
        .llm_save_provider(
            &id,
            &body.name,
            &body.provider_type,
            &base_url,
            &api_key,
            extra_config.as_deref(),
            anthropic_base_url,
            enabled,
        )
        .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("DB error: {}", e),
        )
            .into_response();
    }

    Json(serde_json::json!({"status": "ok"})).into_response()
}

pub async fn toggle_provider(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let db = match state.server_state.db() {
        Some(db) => db,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "no database").into_response(),
    };
    let enabled = body["enabled"].as_bool().unwrap_or(false);
    if let Err(e) = db.llm_toggle_provider(&id, enabled).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("DB error: {}", e),
        )
            .into_response();
    }
    StatusCode::OK.into_response()
}

pub async fn delete_provider(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let db = match state.server_state.db() {
        Some(db) => db,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "no database").into_response(),
    };
    if let Err(e) = db.llm_delete_provider(&id).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("DB error: {}", e),
        )
            .into_response();
    }
    StatusCode::OK.into_response()
}

// ── Model CRUD ─────────────────────────────────────────────────

pub async fn list_provider_models(
    State(state): State<ApiState>,
    Path(provider_id): Path<String>,
) -> impl IntoResponse {
    let db = match state.server_state.db() {
        Some(db) => db,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "no database").into_response(),
    };
    let records = match db.llm_list_models_for_provider(&provider_id).await {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("DB error: {}", e),
            )
                .into_response()
        }
    };
    let models: Vec<ModelConfig> = records
        .into_iter()
        .map(|r| ModelConfig {
            id: r.id,
            provider_id: r.provider_id,
            model_name: r.model_name,
            alias: r.alias,
            tags: serde_json::from_str(&r.tags).unwrap_or_default(),
            extra_config: r.extra_config,
            enabled: r.enabled != 0,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
        .collect();
    Json(serde_json::json!({"models": models})).into_response()
}

pub async fn list_all_models(State(state): State<ApiState>) -> impl IntoResponse {
    let db = match state.server_state.db() {
        Some(db) => db,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "no database").into_response(),
    };
    let records = match db.llm_list_models().await {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("DB error: {}", e),
            )
                .into_response()
        }
    };
    let models: Vec<ModelConfig> = records
        .into_iter()
        .map(|r| ModelConfig {
            id: r.id,
            provider_id: r.provider_id,
            model_name: r.model_name,
            alias: r.alias,
            tags: serde_json::from_str(&r.tags).unwrap_or_default(),
            extra_config: r.extra_config,
            enabled: r.enabled != 0,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
        .collect();
    Json(serde_json::json!({"models": models})).into_response()
}

pub async fn add_model(
    State(state): State<ApiState>,
    Path(provider_id): Path<String>,
    Json(body): Json<ModelRequest>,
) -> impl IntoResponse {
    let db = match state.server_state.db() {
        Some(db) => db,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "no database").into_response(),
    };
    let id = uuid::Uuid::new_v4().to_string();
    let tags = serde_json::to_string(&body.tags.unwrap_or_default()).unwrap_or_default();
    let alias = body.alias.unwrap_or_default();

    if let Err(e) = db
        .llm_save_model(
            &id,
            &provider_id,
            &body.model_name,
            &alias,
            &tags,
            true,
            body.extra_config.as_deref(),
        )
        .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("DB error: {}", e),
        )
            .into_response();
    }
    (
        StatusCode::CREATED,
        Json(serde_json::json!({"status": "ok", "id": id})),
    )
        .into_response()
}

pub async fn update_model(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<ModelRequest>,
) -> impl IntoResponse {
    let db = match state.server_state.db() {
        Some(db) => db,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "no database").into_response(),
    };
    let tags = serde_json::to_string(&body.tags.unwrap_or_default()).unwrap_or_default();
    let alias = body.alias.unwrap_or_default();

    if let Err(e) = db
        .llm_update_model(
            &id,
            &body.model_name,
            &alias,
            &tags,
            body.extra_config.as_deref(),
        )
        .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("DB error: {}", e),
        )
            .into_response();
    }
    StatusCode::OK.into_response()
}

pub async fn delete_model(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let db = match state.server_state.db() {
        Some(db) => db,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "no database").into_response(),
    };
    if let Err(e) = db.llm_delete_model(&id).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("DB error: {}", e),
        )
            .into_response();
    }
    StatusCode::OK.into_response()
}

// ── API Key CRUD ───────────────────────────────────────────────

pub async fn list_api_keys(State(state): State<ApiState>) -> impl IntoResponse {
    let db = match state.server_state.db() {
        Some(db) => db,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "no database").into_response(),
    };
    let records = match db.llm_list_api_keys().await {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("DB error: {}", e),
            )
                .into_response()
        }
    };
    let keys: Vec<ApiKeyView> = records
        .into_iter()
        .map(|r| ApiKeyView {
            id: r.id,
            key_prefix: r.key_prefix,
            name: r.name,
            enabled: r.enabled != 0,
            created_at: r.created_at,
            last_used_at: r.last_used_at,
            kb_id: r.kb_id,
        })
        .collect();
    Json(serde_json::json!({"api_keys": keys})).into_response()
}

/// 校验 `kb_id` 指向的 RAG 知识库存在。`None`（解绑/未指定）直接通过。
async fn ensure_kb_exists(
    db: &crate::server::db::Database,
    kb_id: Option<&str>,
) -> Result<(), (StatusCode, String)> {
    let Some(kb_id) = kb_id else {
        return Ok(());
    };
    match db.rag_get_kb(kb_id).await {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err((StatusCode::BAD_REQUEST, "kb not found".to_string())),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("DB error: {}", e),
        )),
    }
}

pub async fn create_api_key(
    State(state): State<ApiState>,
    Json(body): Json<CreateApiKeyRequest>,
) -> impl IntoResponse {
    let db = match state.server_state.db() {
        Some(db) => db,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "no database").into_response(),
    };

    // 指定 kb_id 时先校验知识库存在（不存在 → 400）
    if let Err((status, msg)) = ensure_kb_exists(db, body.kb_id.as_deref()).await {
        return (status, msg).into_response();
    }

    let (key, hash, prefix) = generate_api_key();
    let id = uuid::Uuid::new_v4().to_string();

    if let Err(e) = db
        .llm_save_api_key(&id, &hash, &prefix, &body.name, body.kb_id.as_deref())
        .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("DB error: {}", e),
        )
            .into_response();
    }

    let resp = CreateApiKeyResponse {
        id,
        key,
        key_prefix: prefix,
        name: body.name,
    };
    (StatusCode::CREATED, Json(serde_json::json!(resp))).into_response()
}

pub async fn toggle_api_key(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let db = match state.server_state.db() {
        Some(db) => db,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "no database").into_response(),
    };

    // 先校验、后变更：kb_id 的类型与存在性校验前置到 enabled toggle 之前，
    // 全部校验通过后才依次应用变更，保证组合 PATCH（enabled + kb_id）失败时
    // 不产生「enabled 已落库但请求返回 400」的部分生效。
    let new_kb_id: Option<&str> = match body.get("kb_id") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) => Some(s),
        Some(_) => {
            return (StatusCode::BAD_REQUEST, "kb_id must be a string or null").into_response()
        }
    };
    if let Err((status, msg)) = ensure_kb_exists(db, new_kb_id).await {
        return (status, msg).into_response();
    }

    // enabled：仅当请求体含 "enabled" 键时才 toggle，
    // 避免只绑 kb_id 的请求把 key 误禁用。
    if body.get("enabled").is_some() {
        let enabled = body["enabled"].as_bool().unwrap_or(false);
        if let Err(e) = db.llm_toggle_api_key(&id, enabled).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("DB error: {}", e),
            )
                .into_response();
        }
    }

    // kb_id：含键则绑定/解绑（Value::Null → 解绑）。存在性已在上面校验。
    if body.get("kb_id").is_some() {
        if let Err(e) = db.llm_set_api_key_kb(&id, new_kb_id).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("DB error: {}", e),
            )
                .into_response();
        }
    }

    StatusCode::OK.into_response()
}

pub async fn delete_api_key(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let db = match state.server_state.db() {
        Some(db) => db,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "no database").into_response(),
    };
    if let Err(e) = db.llm_delete_api_key(&id).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("DB error: {}", e),
        )
            .into_response();
    }
    StatusCode::OK.into_response()
}

// ── Usage stats ────────────────────────────────────────────────

/// 用量查询的时间范围参数（RFC3339）。缺省则回落到最近 24 小时。
#[derive(Debug, serde::Deserialize)]
pub struct UsageQueryParams {
    pub start: Option<String>,
    pub end: Option<String>,
    /// 聚合维度：`api_key` / `model` / `provider`（仅 aggregate 端点用）。
    pub group_by: Option<String>,
    /// 明细分页（仅 logs 端点用）。
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// 解析并校验时间范围，返回 (start, end) 的 SQLite datetime 格式字符串。
/// 缺省时回落到最近 24h；范围超过 31 天则报错。
///
/// SQLite 的 datetime('now') 存储格式为 "YYYY-MM-DD HH:MM:SS"（无时区），
/// 而前端传入的是 RFC3339 格式（带T和Z）。为确保字符串比较正确，
/// 需要转换为 SQLite 兼容格式。
fn resolve_range(p: &UsageQueryParams) -> Result<(String, String), String> {
    let end = match &p.end {
        Some(s) => chrono::DateTime::parse_from_rfc3339(s)
            .map_err(|e| format!("invalid end: {e}"))?
            .with_timezone(&chrono::Utc),
        None => chrono::Utc::now(),
    };
    let start = match &p.start {
        Some(s) => chrono::DateTime::parse_from_rfc3339(s)
            .map_err(|e| format!("invalid start: {e}"))?
            .with_timezone(&chrono::Utc),
        None => end - chrono::Duration::hours(24),
    };
    if end < start {
        return Err("end must be >= start".into());
    }
    if (end - start) > chrono::Duration::days(31) {
        return Err("range must be <= 31 days".into());
    }
    // 转换为 SQLite datetime 格式: "YYYY-MM-DD HH:MM:SS"
    let fmt = "%Y-%m-%d %H:%M:%S";
    Ok((start.format(fmt).to_string(), end.format(fmt).to_string()))
}

pub async fn get_usage_summary(
    State(state): State<ApiState>,
    axum::extract::Query(params): axum::extract::Query<UsageQueryParams>,
) -> impl IntoResponse {
    let db = match state.server_state.db() {
        Some(db) => db,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "no database").into_response(),
    };
    let (start, end) = match resolve_range(&params) {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };
    match db.llm_usage_summary(&start, &end).await {
        Ok(s) => Json(serde_json::json!({"summary": s})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("DB error: {}", e),
        )
            .into_response(),
    }
}

pub async fn get_usage_aggregate(
    State(state): State<ApiState>,
    axum::extract::Query(params): axum::extract::Query<UsageQueryParams>,
) -> impl IntoResponse {
    let db = match state.server_state.db() {
        Some(db) => db,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "no database").into_response(),
    };
    let (start, end) = match resolve_range(&params) {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };
    let group_by = params.group_by.as_deref().unwrap_or("api_key");
    if !matches!(group_by, "api_key" | "model" | "provider") {
        return (
            StatusCode::BAD_REQUEST,
            "group_by must be one of: api_key, model, provider",
        )
            .into_response();
    }
    match db.llm_aggregate_usage(&start, &end, group_by).await {
        Ok(rows) => Json(serde_json::json!({"group_by": group_by, "rows": rows})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("DB error: {}", e),
        )
            .into_response(),
    }
}

pub async fn get_usage_logs(
    State(state): State<ApiState>,
    axum::extract::Query(params): axum::extract::Query<UsageQueryParams>,
) -> impl IntoResponse {
    let db = match state.server_state.db() {
        Some(db) => db,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "no database").into_response(),
    };
    let (start, end) = match resolve_range(&params) {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };
    let limit = params.limit.unwrap_or(50).clamp(1, 500);
    let offset = params.offset.unwrap_or(0).max(0);

    // 并行查询日志和总数
    let (logs_result, count_result) = tokio::join!(
        db.llm_query_usage_logs(&start, &end, limit, offset),
        db.llm_count_usage_logs(&start, &end)
    );

    match (logs_result, count_result) {
        (Ok(logs), Ok(total)) => {
            Json(serde_json::json!({"logs": logs, "total": total})).into_response()
        }
        (Err(e), _) | (_, Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("DB error: {}", e),
        )
            .into_response(),
    }
}

// ── 模型组（多模型故障转移）───────────────────────────────────────

/// 从 server_state 取 LlmState 的 breakers（组详情/重置用）。
/// llm_state 未初始化（LLM 网关从未启用）时回落为空熔断器集合。
async fn llm_breakers(state: &ApiState) -> ModelBreakers {
    state
        .server_state
        .proxy_state
        .llm_state
        .read()
        .await
        .as_ref()
        .map(|l| l.breakers.clone())
        .unwrap_or_default()
}

/// GET /api/llm/model-groups
pub async fn list_model_groups(State(state): State<ApiState>) -> impl IntoResponse {
    let db = match state.server_state.db() {
        Some(db) => db,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "no database").into_response(),
    };
    let groups = match db.llm_list_model_groups().await {
        Ok(g) => g,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("DB error: {}", e),
            )
                .into_response()
        }
    };
    let mut views = Vec::with_capacity(groups.len());
    for g in groups {
        let member_count = db.llm_group_member_count(&g.id).await.unwrap_or(0);
        views.push(ModelGroupView {
            id: g.id,
            name: g.name,
            enabled: g.enabled != 0,
            member_count,
            created_at: g.created_at,
            updated_at: g.updated_at,
        });
    }
    Json(serde_json::json!({"groups": views})).into_response()
}

/// POST /api/llm/model-groups
pub async fn create_model_group(
    State(state): State<ApiState>,
    Json(body): Json<ModelGroupRequest>,
) -> impl IntoResponse {
    let db = match state.server_state.db() {
        Some(db) => db,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "no database").into_response(),
    };
    if body.name.is_empty() {
        return (StatusCode::BAD_REQUEST, "name is required").into_response();
    }
    match db.llm_group_name_conflicts(&body.name, None).await {
        Ok(true) => {
            return (
                StatusCode::CONFLICT,
                "name conflicts with existing model/alias/group",
            )
                .into_response()
        }
        Ok(false) => {}
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("DB error: {}", e),
            )
                .into_response()
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    if let Err(e) = db
        .llm_create_model_group(&id, &body.name, body.enabled.unwrap_or(true))
        .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("DB error: {}", e),
        )
            .into_response();
    }
    (
        StatusCode::CREATED,
        Json(serde_json::json!({"status": "ok", "id": id})),
    )
        .into_response()
}

/// GET /api/llm/model-groups/:id
pub async fn get_model_group(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let db = match state.server_state.db() {
        Some(db) => db,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "no database").into_response(),
    };
    let group = match db.llm_get_model_group(&id).await {
        Ok(Some(g)) => g,
        Ok(None) => return (StatusCode::NOT_FOUND, "group not found").into_response(),
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("DB error: {}", e),
            )
                .into_response()
        }
    };
    let members = match db.llm_list_group_members(&id).await {
        Ok(m) => m,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("DB error: {}", e),
            )
                .into_response()
        }
    };
    let breakers = llm_breakers(&state).await;
    // provider 名映射
    let providers = db.llm_list_providers().await.unwrap_or_default();
    let provider_name = |pid: &str| {
        providers
            .iter()
            .find(|p| p.id == pid)
            .map(|p| p.name.clone())
            .unwrap_or_default()
    };
    let member_views: Vec<GroupMemberView> = members
        .into_iter()
        .map(|m| {
            let snap = breakers.snapshot(&m.model_id);
            GroupMemberView {
                model_id: m.model_id,
                priority: m.priority,
                model_name: m.model_name,
                alias: m.alias,
                provider_id: m.provider_id.clone(),
                provider_name: provider_name(&m.provider_id),
                model_enabled: m.model_enabled != 0,
                breaker: BreakerSnapshotView {
                    state: format!("{:?}", snap.state).to_lowercase(),
                    consecutive_failures: snap.consecutive_failures,
                    cooldown_remaining_secs: snap.cooldown_remaining_secs,
                },
            }
        })
        .collect();
    Json(ModelGroupDetailView {
        id: group.id,
        name: group.name,
        enabled: group.enabled != 0,
        created_at: group.created_at,
        updated_at: group.updated_at,
        members: member_views,
    })
    .into_response()
}

/// PUT /api/llm/model-groups/:id
pub async fn update_model_group(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<ModelGroupRequest>,
) -> impl IntoResponse {
    let db = match state.server_state.db() {
        Some(db) => db,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "no database").into_response(),
    };
    let group = match db.llm_get_model_group(&id).await {
        Ok(Some(g)) => g,
        Ok(None) => return (StatusCode::NOT_FOUND, "group not found").into_response(),
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("DB error: {}", e),
            )
                .into_response()
        }
    };
    if body.name.is_empty() {
        return (StatusCode::BAD_REQUEST, "name is required").into_response();
    }
    match db.llm_group_name_conflicts(&body.name, Some(&id)).await {
        Ok(true) => {
            return (
                StatusCode::CONFLICT,
                "name conflicts with existing model/alias/group",
            )
                .into_response()
        }
        Ok(false) => {}
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("DB error: {}", e),
            )
                .into_response()
        }
    }
    let enabled = body.enabled.unwrap_or(group.enabled != 0);
    if let Err(e) = db.llm_update_model_group(&id, &body.name, enabled).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("DB error: {}", e),
        )
            .into_response();
    }
    Json(serde_json::json!({"status": "ok"})).into_response()
}

/// DELETE /api/llm/model-groups/:id
pub async fn delete_model_group(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let db = match state.server_state.db() {
        Some(db) => db,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "no database").into_response(),
    };
    match db.llm_get_model_group(&id).await {
        Ok(Some(_)) => {}
        Ok(None) => return (StatusCode::NOT_FOUND, "group not found").into_response(),
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("DB error: {}", e),
            )
                .into_response()
        }
    }
    if let Err(e) = db.llm_delete_model_group(&id).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("DB error: {}", e),
        )
            .into_response();
    }
    Json(serde_json::json!({"status": "ok"})).into_response()
}

/// PUT /api/llm/model-groups/:id/members（整体替换；priority 兜底重排为 1..N）
pub async fn replace_group_members(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<ReplaceMembersRequest>,
) -> impl IntoResponse {
    let db = match state.server_state.db() {
        Some(db) => db,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "no database").into_response(),
    };
    match db.llm_get_model_group(&id).await {
        Ok(Some(_)) => {}
        Ok(None) => return (StatusCode::NOT_FOUND, "group not found").into_response(),
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("DB error: {}", e),
            )
                .into_response()
        }
    }
    // 校验 model_id 存在
    let models = db.llm_list_models().await.unwrap_or_default();
    for m in &body.members {
        if !models.iter().any(|mo| mo.id == m.model_id) {
            return (
                StatusCode::BAD_REQUEST,
                format!("model {} not found", m.model_id),
            )
                .into_response();
        }
    }
    // priority 兜底：按传入顺序重排为 1..N（前端传错也能自愈）
    let normalized: Vec<(String, i32)> = body
        .members
        .iter()
        .enumerate()
        .map(|(i, m)| (m.model_id.clone(), (i + 1) as i32))
        .collect();
    if let Err(e) = db.llm_replace_group_members(&id, &normalized).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("DB error: {}", e),
        )
            .into_response();
    }
    Json(serde_json::json!({"status": "ok"})).into_response()
}

/// POST /api/llm/model-groups/:id/reset-breaker
pub async fn reset_group_breaker(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let db = match state.server_state.db() {
        Some(db) => db,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "no database").into_response(),
    };
    match db.llm_get_model_group(&id).await {
        Ok(Some(_)) => {}
        Ok(None) => return (StatusCode::NOT_FOUND, "group not found").into_response(),
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("DB error: {}", e),
            )
                .into_response()
        }
    }
    let members = db.llm_list_group_members(&id).await.unwrap_or_default();
    let ids: Vec<String> = members.into_iter().map(|m| m.model_id).collect();
    let count = ids.len();
    let breakers = llm_breakers(&state).await;
    breakers.reset_many(&ids);
    Json(serde_json::json!({"status": "ok", "reset": count})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, Method, Request, StatusCode as HttpStatus};
    use axum::routing::{get, patch};
    use axum::Router;
    use serde_json::{json, Value};
    use std::sync::Arc;
    use tower::ServiceExt;

    use crate::server::auth::AuthConfig;
    use crate::server::control::ServerState;
    use crate::server::db::Database;

    /// 内存 DB 的 ApiState（api key CRUD 不依赖 llm_state 字段加密器）。
    async fn test_api_state() -> ApiState {
        let db = Database::new(":memory:").await.expect("in-memory db");
        ApiState {
            server_state: ServerState::with_db(db),
            auth_config: Arc::new(AuthConfig::new(None, None)),
            log_store: None,
        }
    }

    /// 覆盖 api key 全部路由的测试 Router（免 JWT，auth_config 关闭）。
    fn test_router(state: ApiState) -> Router {
        Router::new()
            .route(
                "/api/llm/api-keys",
                get(super::list_api_keys).post(super::create_api_key),
            )
            .route(
                "/api/llm/api-keys/:id",
                patch(super::toggle_api_key).delete(super::delete_api_key),
            )
            .with_state(state)
    }

    /// oneshot 请求助手：返回 (status, json body)。
    async fn call(app: &Router, req: Request<Body>) -> (HttpStatus, Value) {
        let resp = app.clone().oneshot(req).await.expect("router responds");
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 8 * 1024 * 1024)
            .await
            .expect("read response body");
        let body = serde_json::from_slice(&bytes).unwrap_or(json!(null));
        (status, body)
    }

    fn json_request(method: Method, uri: String, body: &Value) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .expect("build request")
    }

    /// 直接向内存 DB 建一个知识库，返回 kb_id。
    async fn seed_kb(db: &Database) -> String {
        let kb_id = uuid::Uuid::new_v4().to_string();
        db.rag_create_kb(
            &kb_id,
            "测试库",
            "",
            "http://127.0.0.1:9999",
            "sk-test",
            "test-model",
            8,
            5,
            1000,
            0,
            0.5,
            true,
        )
        .await
        .expect("seed kb");
        kb_id
    }

    /// 辅助：GET /api/llm/api-keys 返回的 api_keys 数组。
    async fn list_keys(app: &Router) -> Value {
        let (status, body) = call(
            app,
            json_request(Method::GET, "/api/llm/api-keys".to_string(), &json!(null)),
        )
        .await;
        assert_eq!(status, HttpStatus::OK, "list keys: {body}");
        body["api_keys"].clone()
    }

    #[tokio::test]
    async fn create_and_bind_api_key_to_kb() {
        let state = test_api_state().await;
        let db = state.server_state.db().expect("db").clone();
        let kb_id = seed_kb(&db).await;
        let app = test_router(state);

        // 建 KB → POST {name, kb_id} → 列表返回绑定后的 kb_id
        let req = json_request(
            Method::POST,
            "/api/llm/api-keys".to_string(),
            &json!({ "name": "cursor", "kb_id": kb_id }),
        );
        let (status, body) = call(&app, req).await;
        assert_eq!(status, HttpStatus::CREATED, "create key: {body}");
        let key_id = body["id"].as_str().expect("key id").to_string();

        let keys = list_keys(&app).await;
        assert_eq!(keys[0]["id"], json!(key_id));
        assert_eq!(keys[0]["kb_id"], json!(kb_id), "绑定后列表应返回 kb_id");

        // PATCH kb_id: null → 解绑
        let req = json_request(
            Method::PATCH,
            format!("/api/llm/api-keys/{key_id}"),
            &json!({ "kb_id": null }),
        );
        let (status, _) = call(&app, req).await;
        assert_eq!(status, HttpStatus::OK, "unbind should succeed");

        let keys = list_keys(&app).await;
        assert_eq!(keys[0]["kb_id"], Value::Null, "解绑后 kb_id 应为 null");
    }

    #[tokio::test]
    async fn create_api_key_rejects_unknown_kb() {
        let state = test_api_state().await;
        let app = test_router(state);

        let req = json_request(
            Method::POST,
            "/api/llm/api-keys".to_string(),
            &json!({ "name": "cursor", "kb_id": "nonexistent" }),
        );
        let (status, _) = call(&app, req).await;
        assert_eq!(status, HttpStatus::BAD_REQUEST, "unknown kb_id → 400");

        // 校验：失败的创建不应留下任何 key
        let keys = list_keys(&app).await;
        assert_eq!(keys.as_array().map(Vec::len), Some(0), "no key created");
    }

    #[tokio::test]
    async fn patch_bind_kb_keeps_key_enabled() {
        let state = test_api_state().await;
        let db = state.server_state.db().expect("db").clone();
        let kb_id = seed_kb(&db).await;
        let app = test_router(state);

        let req = json_request(
            Method::POST,
            "/api/llm/api-keys".to_string(),
            &json!({ "name": "cursor" }),
        );
        let (status, body) = call(&app, req).await;
        assert_eq!(status, HttpStatus::CREATED, "create key: {body}");
        let key_id = body["id"].as_str().expect("key id").to_string();

        // 只绑定 kb_id、不含 enabled 键 —— 不应把 key 禁用（回归 unwrap_or(false) 的坑）
        let req = json_request(
            Method::PATCH,
            format!("/api/llm/api-keys/{key_id}"),
            &json!({ "kb_id": kb_id }),
        );
        let (status, _) = call(&app, req).await;
        assert_eq!(status, HttpStatus::OK);

        let keys = list_keys(&app).await;
        assert_eq!(keys[0]["enabled"], json!(true), "仅绑定 kb_id 不应禁用 key");
        assert_eq!(keys[0]["kb_id"], json!(kb_id));
    }

    #[tokio::test]
    async fn patch_kb_rejects_unknown_kb() {
        let state = test_api_state().await;
        let app = test_router(state);

        let req = json_request(
            Method::POST,
            "/api/llm/api-keys".to_string(),
            &json!({ "name": "cursor" }),
        );
        let (status, body) = call(&app, req).await;
        assert_eq!(status, HttpStatus::CREATED, "create key: {body}");
        let key_id = body["id"].as_str().expect("key id").to_string();

        let req = json_request(
            Method::PATCH,
            format!("/api/llm/api-keys/{key_id}"),
            &json!({ "kb_id": "nonexistent" }),
        );
        let (status, _) = call(&app, req).await;
        assert_eq!(status, HttpStatus::BAD_REQUEST, "unknown kb on bind → 400");

        let keys = list_keys(&app).await;
        assert_eq!(keys[0]["kb_id"], Value::Null, "绑定失败不应改动 kb_id");
    }

    #[tokio::test]
    async fn patch_enabled_still_toggles() {
        let state = test_api_state().await;
        let app = test_router(state);

        let req = json_request(
            Method::POST,
            "/api/llm/api-keys".to_string(),
            &json!({ "name": "cursor" }),
        );
        let (status, body) = call(&app, req).await;
        assert_eq!(status, HttpStatus::CREATED, "create key: {body}");
        let key_id = body["id"].as_str().expect("key id").to_string();

        let req = json_request(
            Method::PATCH,
            format!("/api/llm/api-keys/{key_id}"),
            &json!({ "enabled": false }),
        );
        let (status, _) = call(&app, req).await;
        assert_eq!(status, HttpStatus::OK, "disable key");

        let keys = list_keys(&app).await;
        assert_eq!(keys[0]["enabled"], json!(false));
    }

    #[tokio::test]
    async fn patch_combined_failure_does_not_toggle_enabled() {
        let state = test_api_state().await;
        let app = test_router(state);

        let req = json_request(
            Method::POST,
            "/api/llm/api-keys".to_string(),
            &json!({ "name": "cursor" }),
        );
        let (status, body) = call(&app, req).await;
        assert_eq!(status, HttpStatus::CREATED, "create key: {body}");
        let key_id = body["id"].as_str().expect("key id").to_string();

        // 组合 PATCH：enabled=false + 未知 kb_id → 400，且 key 的 enabled 不被静默改变
        let req = json_request(
            Method::PATCH,
            format!("/api/llm/api-keys/{key_id}"),
            &json!({ "enabled": false, "kb_id": "nonexistent" }),
        );
        let (status, _) = call(&app, req).await;
        assert_eq!(status, HttpStatus::BAD_REQUEST, "unknown kb → 400");

        let keys = list_keys(&app).await;
        assert_eq!(keys[0]["enabled"], json!(true), "校验失败不应改动 enabled");
        assert_eq!(keys[0]["kb_id"], Value::Null, "绑定失败不应改动 kb_id");
    }
}
