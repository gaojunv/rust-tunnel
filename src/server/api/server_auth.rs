use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine;
use chrono::{DateTime, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::server::db::Database;

#[derive(Debug, Serialize)]
pub struct ServerAuthView {
    pub client_token: String,
    pub updated_at: DateTime<Utc>,
}

pub async fn get_impl(db: &Database) -> Result<ServerAuthView, String> {
    let token = db
        .load_server_auth()
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "server_auth row missing (server bug)".to_string())?;
    Ok(ServerAuthView {
        client_token: token,
        updated_at: Utc::now(), // Simplification: db doesn't return updated_at yet.
    })
}

pub async fn rotate_impl(db: &Database) -> Result<String, String> {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    db.save_server_auth(&token)
        .await
        .map_err(|e| e.to_string())?;
    Ok(token)
}

pub async fn set_impl(db: &Database, token: &str) -> Result<(), String> {
    if token.trim().is_empty() {
        return Err("token cannot be empty".into());
    }
    db.save_server_auth(token)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

// ---- Axum handlers ----
// Use ApiState from the parent module (consistent with existing handlers)

pub async fn get_auth(State(state): State<super::ApiState>) -> Response {
    let db = match state.server_state.db() {
        Some(db) => db.clone(),
        None => {
            return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
        }
    };
    match get_impl(&db).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub async fn rotate_auth(State(state): State<super::ApiState>) -> Response {
    let db = match state.server_state.db() {
        Some(db) => db.clone(),
        None => {
            return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
        }
    };
    match rotate_impl(&db).await {
        Ok(t) => Json(serde_json::json!({ "client_token": t })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
pub struct SetAuthBody {
    pub token: String,
}

pub async fn put_auth(
    State(state): State<super::ApiState>,
    Json(body): Json<SetAuthBody>,
) -> Response {
    let db = match state.server_state.db() {
        Some(db) => db.clone(),
        None => {
            return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response();
        }
    };
    match set_impl(&db, &body.token).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::db::Database;

    #[tokio::test]
    async fn get_returns_token() {
        let db = Database::new(":memory:").await.unwrap();
        db.save_server_auth("hello").await.unwrap();
        let view = get_impl(&db).await.unwrap();
        assert_eq!(view.client_token, "hello");
    }

    #[tokio::test]
    async fn rotate_generates_new() {
        let db = Database::new(":memory:").await.unwrap();
        db.save_server_auth("old").await.unwrap();
        let new_token = rotate_impl(&db).await.unwrap();
        assert_ne!(new_token, "old");
        assert!(new_token.len() >= 32);
        let cur = db.load_server_auth().await.unwrap().unwrap();
        assert_eq!(cur, new_token);
    }

    #[tokio::test]
    async fn set_replaces() {
        let db = Database::new(":memory:").await.unwrap();
        db.save_server_auth("old").await.unwrap();
        set_impl(&db, "new").await.unwrap();
        assert_eq!(
            db.load_server_auth().await.unwrap().as_deref(),
            Some("new")
        );
    }
}
