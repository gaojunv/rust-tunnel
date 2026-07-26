use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};

use super::ApiState;
use crate::server::llm::{
    auth::generate_api_key,
    crypto::{encrypt_field, LlmCipher},
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

    let providers: Vec<ProviderConfig> = records
        .into_iter()
        .map(|r| ProviderConfig {
            id: r.id,
            name: r.name,
            provider_type: r.provider_type,
            base_url: r.base_url,
            api_key: String::new(),
            extra_config: r.extra_config,
            anthropic_base_url: r.anthropic_base_url,
            enabled: r.enabled != 0,
            created_at: r.created_at,
            updated_at: r.updated_at,
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
    let extra_config = body
        .extra_config
        .as_deref()
        .map(|ec| encrypt_field(cipher.as_ref(), ec));

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
    // - extra_config 为 None → 沿用已有值；Some → 加密后覆盖
    let api_key = if body.api_key.is_empty() {
        existing.api_key.clone()
    } else {
        encrypt_field(cipher.as_ref(), &body.api_key)
    };
    let extra_config = match &body.extra_config {
        None => existing.extra_config.clone(),
        Some(ec) => Some(encrypt_field(cipher.as_ref(), ec)),
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
        .llm_save_model(&id, &provider_id, &body.model_name, &alias, &tags, true)
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
        .llm_update_model(&id, &body.model_name, &alias, &tags)
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
        })
        .collect();
    Json(serde_json::json!({"api_keys": keys})).into_response()
}

pub async fn create_api_key(
    State(state): State<ApiState>,
    Json(body): Json<CreateApiKeyRequest>,
) -> impl IntoResponse {
    let db = match state.server_state.db() {
        Some(db) => db,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "no database").into_response(),
    };
    let (key, hash, prefix) = generate_api_key();
    let id = uuid::Uuid::new_v4().to_string();

    if let Err(e) = db.llm_save_api_key(&id, &hash, &prefix, &body.name).await {
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
    let enabled = body["enabled"].as_bool().unwrap_or(false);
    if let Err(e) = db.llm_toggle_api_key(&id, enabled).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("DB error: {}", e),
        )
            .into_response();
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
