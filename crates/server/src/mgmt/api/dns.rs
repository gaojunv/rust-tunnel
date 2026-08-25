//! DNS 记录与配置管理 API。

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};

use super::ApiState;
use rust_tunnel_common::DnsRecord;

// ── DNS Management Endpoints ───────────────────────────────────────

// GET /api/dns/records — list all DNS records

/// `GET /api/dns/records`：列出所有 DNS 记录。
pub async fn get_dns_records(State(state): State<ApiState>) -> impl IntoResponse {
    let dns_registry = match &state.server_state.dns_registry {
        Some(r) => r,
        None => return (StatusCode::SERVICE_UNAVAILABLE, "DNS not enabled").into_response(),
    };

    let records = dns_registry.list_records().await;
    let response: Vec<super::dto::DnsRecordResponse> = records
        .iter()
        .map(|r| super::dto::DnsRecordResponse {
            name: r.name().to_string(),
            record_type: r.record_type().to_string(),
            value: match r {
                DnsRecord::TunnelA {
                    target_ip, port, ..
                } => format!("{} (port {})", target_ip, port),
                DnsRecord::MeshA { target_ip, .. } => target_ip.clone(),
                DnsRecord::TunnelSrv { target, port, .. } => format!("{}:{}", target, port),
                DnsRecord::MeshSrv { target, port, .. } => format!("{}:{}", target, port),
                DnsRecord::Txt { text, .. } => text.clone(),
            },
        })
        .collect();

    Json(response).into_response()
}

// POST /api/dns/records — add manual DNS record
/// `POST /api/dns/records`：新增手工 DNS 记录。
pub async fn add_dns_record(
    State(state): State<ApiState>,
    Json(body): Json<super::dto::AddDnsRecordRequest>,
) -> impl IntoResponse {
    let dns_registry = match &state.server_state.dns_registry {
        Some(r) => r,
        None => return (StatusCode::SERVICE_UNAVAILABLE, "DNS not enabled").into_response(),
    };

    let record = match body.record_type.as_str() {
        "A" => DnsRecord::TunnelA {
            name: body.name.clone(),
            target_ip: body.value.clone(),
            port: body.port.unwrap_or(80),
        },
        _ => return (StatusCode::BAD_REQUEST, "Unsupported record type").into_response(),
    };

    dns_registry.add_manual_record(record).await;
    StatusCode::CREATED.into_response()
}

// DELETE /api/dns/records/:name — delete DNS record
/// `DELETE /api/dns/records/:name`：删除 DNS 记录。
pub async fn delete_dns_record(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let dns_registry = match &state.server_state.dns_registry {
        Some(r) => r,
        None => return (StatusCode::SERVICE_UNAVAILABLE, "DNS not enabled").into_response(),
    };

    dns_registry.remove_record(&name).await;
    StatusCode::OK.into_response()
}

/// Get DNS config
pub async fn get_dns_config(State(state): State<ApiState>) -> impl IntoResponse {
    let dc = state.server_state.dynamic_config.read().await;
    Json(serde_json::json!(dc.dns))
}

/// Update DNS config
pub async fn update_dns_config(
    State(state): State<ApiState>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    let tunnel_domain = payload["tunnel_domain"].as_str().unwrap_or("tunnel.local");
    let mesh_domain = payload["mesh_domain"].as_str().unwrap_or("mesh.local");

    if let Some(db) = state.server_state.db() {
        if let Err(e) = db.save_dns_config(tunnel_domain, mesh_domain).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("DB error: {}", e),
            )
                .into_response();
        }
    }

    {
        let mut dc = state.server_state.dynamic_config.write().await;
        dc.dns = crate::dynamic_config::DnsSettings {
            tunnel_domain: tunnel_domain.to_string(),
            mesh_domain: mesh_domain.to_string(),
        };
    }

    StatusCode::OK.into_response()
}
