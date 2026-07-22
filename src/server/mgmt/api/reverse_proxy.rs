use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;

use super::ApiState;
use crate::server::reverse_proxy::ProxyRule;

// ── Proxy Rules Endpoints ─────────────────────────────────────────

/// Request body for creating a proxy rule
#[derive(Debug, Deserialize)]
pub struct CreateProxyRuleRequest {
    pub name: String,
    #[serde(rename = "type")]
    pub rule_type: String,
    pub listen: String,
    #[serde(default)]
    pub domains: Vec<String>,
    #[serde(default)]
    pub routes: Vec<crate::server::reverse_proxy::Route>,
    pub tls: Option<crate::server::reverse_proxy::ProxyTlsConfig>,
    #[serde(default = "default_rule_enabled")]
    pub enabled: bool,
}

/// Request body for updating a proxy rule
#[derive(Debug, Deserialize)]
pub struct UpdateProxyRuleRequest {
    pub name: String,
    #[serde(rename = "type")]
    pub rule_type: String,
    pub listen: String,
    #[serde(default)]
    pub domains: Vec<String>,
    #[serde(default)]
    pub routes: Vec<crate::server::reverse_proxy::Route>,
    pub tls: Option<crate::server::reverse_proxy::ProxyTlsConfig>,
    #[serde(default = "default_rule_enabled")]
    pub enabled: bool,
}

fn default_rule_enabled() -> bool {
    true
}

// GET /api/proxy/rules — list all proxy rules
pub async fn list_proxy_rules(State(state): State<ApiState>) -> impl IntoResponse {
    let rules = state.server_state.proxy_state.rules.lock().await;
    let rules_vec: Vec<&ProxyRule> = rules.values().collect();
    Json(serde_json::json!({ "rules": rules_vec }))
}

// POST /api/proxy/rules — create a new proxy rule
pub async fn create_proxy_rule(
    State(state): State<ApiState>,
    Json(body): Json<CreateProxyRuleRequest>,
) -> impl IntoResponse {
    let rule_type = match body.rule_type.to_lowercase().as_str() {
        "http" => crate::server::reverse_proxy::RuleType::Http,
        "tcp" => crate::server::reverse_proxy::RuleType::Tcp,
        "udp" => crate::server::reverse_proxy::RuleType::Udp,
        "llm" => crate::server::reverse_proxy::RuleType::Llm,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "Invalid rule type. Use: http, tcp, udp, llm" })),
            )
                .into_response();
        }
    };

    let id = uuid::Uuid::new_v4().to_string();

    let mut rule = ProxyRule {
        id: id.clone(),
        name: body.name,
        rule_type: rule_type.clone(),
        listen: body.listen,
        domains: body.domains,
        routes: body.routes,
        tls: body.tls,
        enabled: body.enabled,
        created_at: Some(chrono::Utc::now().to_rfc3339()),
        cert_status: None,
    };

    if let Err(e) = crate::server::reverse_proxy::validate_rule_for_save(&rule) {
        return (StatusCode::BAD_REQUEST, e).into_response();
    }
    crate::server::reverse_proxy::sanitize_rule(&mut rule);

    let cert_status = crate::server::reverse_proxy::resolve_cert_source_for_rule(
        &rule,
        state.server_state.proxy_state.cert_manager(),
    )
    .await;
    rule.cert_status = Some(cert_status.clone());

    if let Err(e) = state.server_state.proxy_state.save_rule(&rule).await {
        tracing::error!("Failed to save proxy rule: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "Failed to save proxy rule" })),
        )
            .into_response();
    }

    {
        let mut rules = state.server_state.proxy_state.rules.lock().await;
        rules.insert(id.clone(), rule.clone());
    }

    if rule.rule_type == crate::server::reverse_proxy::RuleType::Http {
        if let Err(e) = state
            .server_state
            .proxy_state
            .reconcile_http_listener(&rule.listen)
            .await
        {
            tracing::warn!("Reconcile failed on create, rolling back: {}", e);
            let _ = state
                .server_state
                .proxy_state
                .rules
                .lock()
                .await
                .remove(&id);
            if let Err(del_err) = state.server_state.proxy_state.delete_rule(&id).await {
                tracing::error!(
                    "Compensating delete failed after reconcile error: {}",
                    del_err
                );
            }
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": format!("{}", e),
                    "conflicts": conflicts_from_error(&e),
                })),
            )
                .into_response();
        }
    }

    // 反代规则变更可能影响 Trojan 共享/独立模式
    crate::server::trojan_runtime::sync_trojan_mode(&state.server_state).await;

    if cert_status.source == crate::server::reverse_proxy::CertSourceKind::PendingIssuance {
        if let Some(mgr) = state.server_state.proxy_state.cert_manager().cloned() {
            if rule.tls.as_ref().is_some_and(|t| t.acme) {
                let domain = cert_status.covering_domain.clone();
                tokio::spawn(async move {
                    if let Err(e) = mgr.request_acme_certificate(&domain).await {
                        tracing::error!("ACME issuance failed for {}: {}", domain, e);
                    }
                });
            }
        }
    }

    (
        StatusCode::CREATED,
        Json(serde_json::json!({ "rule": rule })),
    )
        .into_response()
}

fn conflicts_from_error(
    e: &crate::server::reverse_proxy::error::ReconcileError,
) -> Vec<serde_json::Value> {
    use crate::server::reverse_proxy::error::ReconcileError as E;
    match e {
        E::DomainConflict {
            domain,
            other_rule_id,
            ..
        } => vec![serde_json::json!({
            "rule_id": other_rule_id,
            "reason": format!("domain {} already claimed", domain),
        })],
        E::TlsMismatch {
            existing_tls,
            new_tls,
            ..
        } => vec![serde_json::json!({
            "reason": format!("tls mismatch: existing={} new={}", existing_tls, new_tls),
        })],
        E::BindFailed { source, .. } => vec![serde_json::json!({
            "reason": format!("bind failed: {}", source),
        })],
        E::NoCertManager { .. } => vec![serde_json::json!({
            "reason": "TLS enabled but no certificate manager configured",
        })],
    }
}

// PUT /api/proxy/rules/:id — update a proxy rule
pub async fn update_proxy_rule(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateProxyRuleRequest>,
) -> impl IntoResponse {
    let rule_type = match body.rule_type.to_lowercase().as_str() {
        "http" => crate::server::reverse_proxy::RuleType::Http,
        "tcp" => crate::server::reverse_proxy::RuleType::Tcp,
        "udp" => crate::server::reverse_proxy::RuleType::Udp,
        "llm" => crate::server::reverse_proxy::RuleType::Llm,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "Invalid rule type. Use: http, tcp, udp, llm" })),
            )
                .into_response();
        }
    };

    let previous = {
        let rules = state.server_state.proxy_state.rules.lock().await;
        rules.get(&id).cloned()
    };
    let previous = match previous {
        Some(r) => r,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "Rule not found" })),
            )
                .into_response();
        }
    };

    let mut rule = ProxyRule {
        id: id.clone(),
        name: body.name,
        rule_type: rule_type.clone(),
        listen: body.listen,
        domains: body.domains,
        routes: body.routes,
        tls: body.tls,
        enabled: body.enabled,
        created_at: previous.created_at.clone(),
        cert_status: None,
    };

    if let Err(e) = crate::server::reverse_proxy::validate_rule_for_save(&rule) {
        return (StatusCode::BAD_REQUEST, e).into_response();
    }
    crate::server::reverse_proxy::sanitize_rule(&mut rule);

    let cert_status = crate::server::reverse_proxy::resolve_cert_source_for_rule(
        &rule,
        state.server_state.proxy_state.cert_manager(),
    )
    .await;
    rule.cert_status = Some(cert_status.clone());

    if let Err(e) = state.server_state.proxy_state.save_rule(&rule).await {
        tracing::error!("Failed to save proxy rule: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "Failed to save proxy rule" })),
        )
            .into_response();
    }

    {
        let mut rules = state.server_state.proxy_state.rules.lock().await;
        rules.insert(id.clone(), rule.clone());
    }

    let old_listen = previous.listen.clone();
    let new_listen = rule.listen.clone();

    if rule.rule_type == crate::server::reverse_proxy::RuleType::Http {
        if let Err(e) = state
            .server_state
            .proxy_state
            .reconcile_http_listener(&new_listen)
            .await
        {
            tracing::warn!("Reconcile failed on update, rolling back: {}", e);
            {
                let mut rules = state.server_state.proxy_state.rules.lock().await;
                rules.insert(id.clone(), previous.clone());
            }
            let _ = state.server_state.proxy_state.save_rule(&previous).await;

            let ports: std::collections::HashSet<&str> = [old_listen.as_str(), new_listen.as_str()]
                .into_iter()
                .collect();
            for port in ports {
                if let Err(rb) = state
                    .server_state
                    .proxy_state
                    .reconcile_http_listener(port)
                    .await
                {
                    tracing::error!("Rollback reconcile failed for {}: {}", port, rb);
                }
            }
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": format!("{}", e),
                    "conflicts": conflicts_from_error(&e),
                })),
            )
                .into_response();
        }
    }
    if previous.rule_type == crate::server::reverse_proxy::RuleType::Http
        && old_listen != new_listen
    {
        if let Err(e) = state
            .server_state
            .proxy_state
            .reconcile_http_listener(&old_listen)
            .await
        {
            tracing::warn!("Old-port reconcile failed after update: {}", e);
        }
    }

    // 新旧端口 reconcile 都完成后，检查 Trojan 共享/独立模式是否需要切换
    crate::server::trojan_runtime::sync_trojan_mode(&state.server_state).await;

    if cert_status.source == crate::server::reverse_proxy::CertSourceKind::PendingIssuance {
        if let Some(mgr) = state.server_state.proxy_state.cert_manager().cloned() {
            if rule.tls.as_ref().is_some_and(|t| t.acme) {
                let domain = cert_status.covering_domain.clone();
                tokio::spawn(async move {
                    if let Err(e) = mgr.request_acme_certificate(&domain).await {
                        tracing::error!("ACME issuance failed for {}: {}", domain, e);
                    }
                });
            }
        }
    }

    Json(serde_json::json!({ "rule": rule })).into_response()
}

// DELETE /api/proxy/rules/:id — delete a proxy rule
pub async fn delete_proxy_rule(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let deleted = {
        let mut rules = state.server_state.proxy_state.rules.lock().await;
        rules.remove(&id)
    };
    let deleted = match deleted {
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "Rule not found" })),
            )
                .into_response();
        }
        Some(r) => r,
    };

    if deleted.rule_type != crate::server::reverse_proxy::RuleType::Http {
        let listener_handle = {
            let mut listeners = state.server_state.proxy_state.tcp_listeners.lock().await;
            listeners.remove(&id)
        };
        if let Some(handle) = listener_handle {
            handle.abort();
        }
    }

    if let Err(e) = state.server_state.proxy_state.delete_rule(&id).await {
        tracing::error!("Failed to delete proxy rule from database: {}", e);
    }

    if deleted.rule_type == crate::server::reverse_proxy::RuleType::Http {
        if let Err(e) = state
            .server_state
            .proxy_state
            .reconcile_http_listener(&deleted.listen)
            .await
        {
            tracing::warn!("Reconcile failed on delete: {}", e);
        }
    }

    // 共享 listener 可能已删除/降级，检查 Trojan 是否需要回退独立监听
    crate::server::trojan_runtime::sync_trojan_mode(&state.server_state).await;

    Json(serde_json::json!({ "deleted": id })).into_response()
}

/// Get reverse proxy config
pub async fn get_reverse_proxy_config(State(state): State<ApiState>) -> impl IntoResponse {
    let dc = state.server_state.dynamic_config.read().await;
    Json(serde_json::json!(dc.reverse_proxy))
}

/// Update reverse proxy config
pub async fn update_reverse_proxy_config(
    State(state): State<ApiState>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    let max_conn = payload["max_connections"].as_u64().unwrap_or(10000) as u32;
    let timeout = payload["connection_timeout_secs"].as_u64().unwrap_or(30);
    let buffer = payload["buffer_size"].as_u64().unwrap_or(8192) as usize;

    // Save to DB
    if let Some(db) = state.server_state.db() {
        if let Err(e) = db
            .save_reverse_proxy_config(max_conn, timeout, buffer)
            .await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("DB error: {}", e),
            )
                .into_response();
        }
    }

    // Update in-memory
    {
        let mut dc = state.server_state.dynamic_config.write().await;
        dc.reverse_proxy = crate::server::dynamic_config::ReverseProxySettings {
            max_connections: max_conn,
            connection_timeout_secs: timeout,
            buffer_size: buffer,
        };
    }

    StatusCode::OK.into_response()
}
