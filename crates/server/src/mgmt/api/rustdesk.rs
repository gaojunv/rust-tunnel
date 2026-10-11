//! RustDesk 会合/中继配置 API。

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use rust_tunnel_rustdesk::key::ServerKey;

use super::ApiState;
use crate::config::rustdesk_validation::{validate_rustdesk_config, RustdeskValidationInput};
use crate::dynamic_config::RustdeskDynamicConfig;
use crate::rustdesk_runtime::nat_test_port;

/// 未配置 RustDesk 时 GET 展示的缺省会合端口（上游默认）。
const DEFAULT_RENDEZVOUS_PORT: u16 = 21116;
/// 未配置 RustDesk 时 GET 展示的缺省中继端口（上游默认）。
const DEFAULT_RELAY_PORT: u16 = 21117;

/// `GET /api/rustdesk`：读取 RustDesk 配置 + 运行状态。
pub async fn get_rustdesk_config(State(state): State<ApiState>) -> Json<super::dto::RustdeskConfig> {
    let cfg = {
        let dc = state.server_state.dynamic_config.read().await;
        dc.rustdesk.clone()
    };
    let rt = state.server_state.proxy_ports.rustdesk_runtime.read().await;
    // 未配置（表里无行）时给一份 enabled=false 的缺省视图，字段仍按契约填满，
    // 与 trojan GET 的「无配置不报错」语义一致。
    let (enabled, rendezvous_port, relay_port, relay_server, always_use_relay, local_ip, mask, key_set) =
        match cfg {
            Some(c) => (
                c.enabled,
                c.rendezvous_port,
                c.relay_port,
                c.relay_server,
                c.always_use_relay,
                c.local_ip,
                c.mask,
                !c.key.is_empty(),
            ),
            None => (
                false,
                DEFAULT_RENDEZVOUS_PORT,
                DEFAULT_RELAY_PORT,
                String::new(),
                false,
                String::new(),
                String::new(),
                false,
            ),
        };
    Json(super::dto::RustdeskConfig {
        enabled,
        rendezvous_port,
        relay_port,
        relay_server,
        always_use_relay,
        local_ip,
        mask,
        key_set,
        running: rt.listening,
        nat_test_port: nat_test_port(rendezvous_port),
        online_peers: rt.online_peers,
        known_peers: rt.known_peers,
        relay_pairs: rt.relay_pairs,
        last_error: rt.last_error.clone(),
        relay_total_pairs: rt.relay_total_pairs,
        rejected_total: rt.rejected_total,
    })
}

/// 校验后的 `POST /api/rustdesk` 入参。
struct ParsedUpdate {
    enabled: bool,
    rendezvous_port: u16,
    relay_port: u16,
    relay_server: String,
    always_use_relay: bool,
    local_ip: String,
    mask: String,
    key: String,
    /// 仅 `generate_key=true` 时非空，用于一次性回传新公钥。
    returned_key: Option<String>,
}

/// 解析一个端口字段；缺失或越界返回 400 中文错误。
fn parse_port_field(
    payload: &serde_json::Value,
    field: &str,
    min: u16,
) -> Result<u16, (StatusCode, String)> {
    match payload[field].as_u64() {
        Some(p) if p >= u64::from(min) && p <= 65535 => Ok(u16::try_from(p).unwrap_or(u16::MAX)),
        _ => Err((
            StatusCode::BAD_REQUEST,
            format!("{field} 无效（需在 {min}..=65535）"),
        )),
    }
}

/// 解析并校验 POST 入参；可选字段留空时保留 `existing` 的原值。
fn parse_update(
    payload: &serde_json::Value,
    existing: Option<&RustdeskDynamicConfig>,
) -> Result<ParsedUpdate, (StatusCode, String)> {
    let enabled = payload["enabled"].as_bool().unwrap_or(false);
    // parse_port_field 只负责「字段缺失/越界」这类粗筛（会合 ≥2、中继 ≥1），
    // 语义冲突（两端口相等、中继撞 NAT 探测端口、与其他服务端口冲突…）交给共享校验器，
    // 这样 400 的文案能区分「字段本身不对」与「字段间打架」。
    let rendezvous_port = parse_port_field(payload, "rendezvous_port", 2)?;
    let relay_port = parse_port_field(payload, "relay_port", 1)?;

    let relay_server = match payload["relay_server"].as_str() {
        Some(s) => s.trim().to_string(),
        None => existing.map(|c| c.relay_server.clone()).unwrap_or_default(),
    };

    let local_ip = match payload["local_ip"].as_str() {
        Some(s) => s.trim().to_string(),
        None => existing.map(|c| c.local_ip.clone()).unwrap_or_default(),
    };
    let mask = match payload["mask"].as_str() {
        Some(s) => s.trim().to_string(),
        None => existing.map(|c| c.mask.clone()).unwrap_or_default(),
    };
    let always_use_relay = payload["always_use_relay"]
        .as_bool()
        .unwrap_or_else(|| existing.is_some_and(|c| c.always_use_relay));

    // 密钥语义：key 非空则原样存（ServerKey::parse 之后决定它是口令/公钥/私钥）。
    // generate_key=true 时新造一对：**持久化私钥**（to_secret_base64，服务端才能签名
    // IdPk），**回传公钥**（ServerKey::key()，即操作者粘进 RustDesk 客户端的那串）。
    // 这层不对称最容易记反：存进去的必须是私钥，发出去的必须是公钥。
    let mut key = match payload["key"].as_str() {
        Some(k) if !k.is_empty() => k.to_string(),
        _ => existing.map(|c| c.key.clone()).unwrap_or_default(),
    };
    let mut returned_key = None;
    if payload["generate_key"].as_bool().unwrap_or(false) {
        let sk = ServerKey::generate();
        key = sk
            .to_secret_base64()
            .unwrap_or_else(|| sk.key().to_string());
        returned_key = Some(sk.key().to_string());
    }

    let parsed = ParsedUpdate {
        enabled,
        rendezvous_port,
        relay_port,
        relay_server,
        always_use_relay,
        local_ip,
        mask,
        key,
        returned_key,
    };
    // 与配置层共用同一套校验（此处 other_ports 为空：跨服务端口冲突在校验器之外、
    // 需要 live 端口表的地方单独再跑一次）。
    validate_rustdesk_config(&RustdeskValidationInput {
        enabled: parsed.enabled,
        rendezvous_port: Some(parsed.rendezvous_port),
        relay_port: Some(parsed.relay_port),
        relay_server: &parsed.relay_server,
        mask: &parsed.mask,
        other_ports: &[],
    })
    .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    Ok(parsed)
}

/// `POST /api/rustdesk`：更新配置并应用监听（启停/改端口/换密钥）。
pub async fn update_rustdesk_config(
    State(state): State<ApiState>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    let existing = {
        let dc = state.server_state.dynamic_config.read().await;
        dc.rustdesk.clone()
    };
    let parsed = match parse_update(&payload, existing.as_ref()) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };

    // 跨服务端口冲突校验：与 SS/Trojan 的**运行时 live 端口表**比对。刻意只用 live 表、
    // 不看 dynamic_config——DB 命中路径会把「已禁用但配置仍在」的端口也带回来，拿它校验会
    // 把合法配置误判成冲突；live 表只在 listener 真正起来时登记，语义才准确。
    let other_ports: Vec<(u16, &str)> = {
        let mut v: Vec<(u16, &str)> = Vec::new();
        for port in state.server_state.proxy_ports.get_shadowsocks_ports().await {
            v.push((port, "Shadowsocks"));
        }
        for port in state.server_state.proxy_ports.get_trojan_ports().await {
            v.push((port, "Trojan"));
        }
        v
    };
    if let Err(e) = validate_rustdesk_config(&RustdeskValidationInput {
        enabled: parsed.enabled,
        rendezvous_port: Some(parsed.rendezvous_port),
        relay_port: Some(parsed.relay_port),
        relay_server: &parsed.relay_server,
        mask: &parsed.mask,
        other_ports: &other_ports,
    }) {
        return (StatusCode::BAD_REQUEST, e.to_string()).into_response();
    }

    // Save to DB（单行表整行替换）
    if let Some(db) = state.server_state.db() {
        if let Err(e) = db
            .save_rustdesk_config(
                parsed.enabled,
                parsed.rendezvous_port,
                parsed.relay_port,
                &parsed.key,
                &parsed.relay_server,
                parsed.always_use_relay,
                &parsed.local_ip,
                &parsed.mask,
            )
            .await
        {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")).into_response();
        }
    }

    // Update in-memory config（new_cfg 同时供 lifecycle 段复用）
    let new_cfg = RustdeskDynamicConfig {
        enabled: parsed.enabled,
        rendezvous_port: parsed.rendezvous_port,
        relay_port: parsed.relay_port,
        key: parsed.key,
        relay_server: parsed.relay_server,
        always_use_relay: parsed.always_use_relay,
        local_ip: parsed.local_ip,
        mask: parsed.mask,
    };
    {
        let mut dc = state.server_state.dynamic_config.write().await;
        dc.rustdesk = Some(new_cfg.clone());
    }

    // Handle listener lifecycle。失败**不回滚** DB/内存配置（与 trojan 更新路径一致）：
    // 已保存的是「期望配置」，apply 失败的原因由 `rustdesk_runtime.last_error` 记录，
    // 并由 GET /api/rustdesk 的 `last_error` 字段回传给界面；修好端口冲突后再点一次保存
    // 即可重试，回滚反而让「保存→失败→界面回退」更困惑。
    if let Err(e) =
        crate::rustdesk_runtime::apply_rustdesk_config(&state.server_state, &new_cfg).await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to apply rustdesk config: {e}"),
        )
            .into_response();
    }

    // key 只在 generate_key 时一次性回传（GET 永不回传），故响应体按需拼装
    let mut body = serde_json::json!({
        "status": "ok",
        "enabled": parsed.enabled,
        "rendezvous_port": parsed.rendezvous_port,
        "relay_port": parsed.relay_port,
    });
    if let Some(k) = parsed.returned_key {
        body["key"] = serde_json::Value::String(k);
    }
    Json(body).into_response()
}
