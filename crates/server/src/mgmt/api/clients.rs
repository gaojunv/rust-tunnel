use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::client_registry::ClientRegistry;

#[derive(Debug, Clone, Serialize)]
pub struct ClientView {
    pub name: String,
    pub hostname: Option<String>,
    pub note: Option<String>,
    pub online: bool,
    pub connected_at: Option<DateTime<Utc>>,
    pub last_seen_at: DateTime<Utc>,
    pub first_seen_at: DateTime<Utc>,
    pub client_version: Option<String>,
    pub referenced_by_rules: u32,
}

pub async fn list_clients_impl(reg: &ClientRegistry) -> Result<Vec<ClientView>, String> {
    let db = reg.db();
    let db_rows = db.list_clients().await.map_err(|e| e.to_string())?;
    let mut online = std::collections::HashMap::new();
    for e in reg.list_online().await {
        online.insert(e.name.clone(), e);
    }
    let mut out = Vec::with_capacity(db_rows.len());
    for r in db_rows {
        let entry = online.remove(&r.name);
        let refs = db
            .rules_referencing_client(&r.name)
            .await
            .map(|v| v.len() as u32)
            .unwrap_or(0);
        out.push(ClientView {
            name: r.name,
            hostname: r.hostname,
            note: r.note,
            online: entry.is_some(),
            connected_at: entry.as_ref().map(|e| e.connected_at),
            last_seen_at: r.last_seen_at,
            first_seen_at: r.first_seen_at,
            client_version: entry.and_then(|e| e.client_version.clone()),
            referenced_by_rules: refs,
        });
    }
    Ok(out)
}

pub async fn delete_client_impl(reg: &ClientRegistry, name: &str) -> Result<(), String> {
    let db = reg.db();
    let refs = db
        .rules_referencing_client(name)
        .await
        .map_err(|e| e.to_string())?;
    if !refs.is_empty() {
        let names: Vec<&str> = refs.iter().map(|(_, n)| n.as_str()).collect();
        return Err(format!(
            "client '{name}' is referenced by rules: {}",
            names.join(", ")
        ));
    }
    if reg.get(name).await.is_some() {
        reg.disconnect(name, "removed by admin").await;
    }
    db.delete_client(name).await.map_err(|e| e.to_string())?;
    Ok(())
}

// ---- Axum handlers ----
// Use ApiState from the parent module (consistent with existing handlers)

pub async fn list_clients(State(state): State<super::ApiState>) -> Response {
    let reg = match state.server_state.client_registry.as_ref() {
        Some(r) => r.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "server has no client registry",
            )
                .into_response();
        }
    };
    match list_clients_impl(&reg).await {
        Ok(list) => Json(serde_json::json!({ "clients": list })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
pub struct UpdateNoteBody {
    pub note: Option<String>,
}

pub async fn patch_client_note(
    State(state): State<super::ApiState>,
    Path(name): Path<String>,
    Json(body): Json<UpdateNoteBody>,
) -> Response {
    let db = match state.server_state.db() {
        Some(db) => db.clone(),
        None => {
            return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
        }
    };
    match db.update_client_note(&name, body.note.as_deref()).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub async fn delete_client(
    State(state): State<super::ApiState>,
    Path(name): Path<String>,
) -> Response {
    let reg = match state.server_state.client_registry.as_ref() {
        Some(r) => r.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "server has no client registry",
            )
                .into_response();
        }
    };
    match delete_client_impl(&reg, &name).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) if e.contains("referenced") => (StatusCode::CONFLICT, e).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub async fn kick_client(
    State(state): State<super::ApiState>,
    Path(name): Path<String>,
) -> Response {
    let reg = match state.server_state.client_registry.as_ref() {
        Some(r) => r.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "server has no client registry",
            )
                .into_response();
        }
    };
    if reg.get(&name).await.is_none() {
        return (StatusCode::NOT_FOUND, format!("client '{name}' not online")).into_response();
    }
    reg.disconnect(&name, "kicked").await;
    StatusCode::NO_CONTENT.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client_registry::ClientRegistry;
    use crate::db::Database;
    use tokio::sync::mpsc;

    async fn setup() -> (ClientRegistry, Database) {
        let db = Database::new(":memory:").await.unwrap();
        db.save_server_auth("pw").await.unwrap();
        let reg = ClientRegistry::new(db.clone());
        (reg, db)
    }

    #[tokio::test]
    async fn list_merges_online_and_offline() {
        let (reg, db) = setup().await;
        // offline row
        db.upsert_client("offline-host", Some("h.local"))
            .await
            .unwrap();
        // online (register)
        let (tx, _rx) = mpsc::channel(8);
        reg.register(
            "online-host",
            Some("nas.local".into()),
            Some("0.4.0".into()),
            "pw",
            tx,
        )
        .await
        .unwrap();

        let list = list_clients_impl(&reg).await.unwrap();
        assert_eq!(list.len(), 2);
        let online = list.iter().find(|c| c.name == "online-host").unwrap();
        assert!(online.online);
        assert_eq!(online.client_version.as_deref(), Some("0.4.0"));
        let offline = list.iter().find(|c| c.name == "offline-host").unwrap();
        assert!(!offline.online);
    }

    #[tokio::test]
    async fn delete_rejects_when_referenced() {
        let (reg, db) = setup().await;
        db.upsert_client("home-nas", None).await.unwrap();
        // Insert a proxy_rules row referencing home-nas
        let routes_json = serde_json::json!([{
            "path": "/",
            "backends": [{ "kind": "client", "addr": "localhost:80", "client_name": "home-nas",
                          "weight": 100, "protocol": "http1", "scheme": "http" }],
            "load_balancing": "round_robin"
        }])
        .to_string();
        db.save_proxy_rule(
            "r1",
            "web",
            "http",
            "0.0.0.0:80",
            None,
            Some(&routes_json),
            false,
            false,
            None,
            true,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        let err = delete_client_impl(&reg, "home-nas")
            .await
            .expect_err("should reject");
        assert!(err.contains("referenced"));
    }

    #[tokio::test]
    async fn delete_online_kicks_first() {
        let (reg, _db) = setup().await;
        let (tx, mut rx) = mpsc::channel(8);
        reg.register("home-nas", None, None, "pw", tx)
            .await
            .unwrap();

        delete_client_impl(&reg, "home-nas").await.unwrap();

        // Should receive Disconnect
        use rust_tunnel_common::ControlMessage;
        let msg = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(msg, ControlMessage::Disconnect { .. }));
        assert!(reg.get("home-nas").await.is_none());
    }
}
