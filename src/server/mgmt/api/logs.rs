use axum::{
    body::Body,
    extract::{Query, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
    Json,
};
use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::{
    dto::{LogEntryResponse, LogsQuery, SetLevelRequest, SseQuery},
    ApiState,
};

// ── Log Viewer Endpoints ──────────────────────────────────────────

pub async fn sse_log_stream(
    State(state): State<ApiState>,
    Query(params): Query<SseQuery>,
) -> impl IntoResponse {
    // Check auth for SSE
    if state.auth_config.is_enabled() {
        let token = params.token.as_deref().unwrap_or("");

        let is_valid = if !token.is_empty() {
            crate::server::auth::validate_token(token, &state.auth_config.jwt_secret).is_ok()
        } else {
            false
        };

        if !is_valid {
            return axum::response::Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .body(Body::from("Unauthorized"))
                .unwrap();
        }
    }

    let log_store = match &state.log_store {
        Some(store) => store.clone(),
        None => {
            return axum::response::Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body(Body::from("Log store not initialized"))
                .unwrap();
        }
    };

    let min_level = params.level.as_deref().unwrap_or("info");
    let min_level_u8 = match min_level {
        "error" => 4u8,
        "warn" => 3,
        "info" => 2,
        "debug" => 1,
        "trace" => 0,
        _ => 2,
    };
    let source_filter = params.source.clone();

    let mut rx = log_store.tx.subscribe();
    let stream = async_stream::stream! {
        loop {
            match tokio::time::timeout(Duration::from_secs(30), rx.recv()).await {
                Ok(Ok(entry)) => {
                    // Apply filters
                    let entry_level = match entry.level.as_str() {
                        "TRACE" => 0, "DEBUG" => 1, "INFO" => 2, "WARN" => 3, "ERROR" => 4,
                        _ => 2,
                    };
                    if entry_level < min_level_u8 {
                        continue;
                    }
                    if let Some(ref src) = source_filter {
                        if !entry.source.starts_with(src) {
                            continue;
                        }
                    }

                    let json = serde_json::to_string(&LogEntryResponse {
                        id: entry.id,
                        timestamp: entry.timestamp,
                        level: entry.level.clone(),
                        source: entry.source.clone(),
                        target: entry.target.clone(),
                        message: entry.message.clone(),
                    })
                    .unwrap_or_default();

                    yield Ok::<_, std::convert::Infallible>(
                        Event::default().event("log").data(json),
                    );
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(n))) => {
                    yield Ok::<_, std::convert::Infallible>(
                        Event::default()
                            .event("sync")
                            .data(format!(r#"{{"lagged":{}}}"#, n)),
                    );
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                    break;
                }
                Err(_) => {
                    // Timeout — send ping to keep connection alive
                    yield Ok::<_, std::convert::Infallible>(
                        Event::default().event("ping").data(""),
                    );
                }
            }
        }
    };

    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(30)))
        .into_response()
}

pub async fn get_logs(
    State(state): State<ApiState>,
    Query(params): Query<LogsQuery>,
) -> impl IntoResponse {
    let log_store = match &state.log_store {
        Some(store) => store,
        None => {
            return axum::response::Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body(Body::from("Log store not initialized"))
                .unwrap();
        }
    };

    let limit = params.limit.unwrap_or(200).min(1000) as usize;

    // When before_id is specified, query DB directly for correct pagination
    // (in-memory entries have id=0, so DB pagination is the only correct path)
    if params.before_id.is_some() {
        let db_entries = log_store
            .query_db(
                params.level.as_deref(),
                params.source.as_deref(),
                params.search.as_deref(),
                limit as u32,
                params.before_id,
            )
            .await;

        let response: Vec<LogEntryResponse> = db_entries
            .into_iter()
            .map(|e| LogEntryResponse {
                id: e.id,
                timestamp: e.timestamp,
                level: e.level,
                source: e.source,
                target: e.target,
                message: e.message,
            })
            .collect();

        return Json(response).into_response();
    }

    // Query in-memory buffer first (fast path, no DB round-trip)
    let mem_entries = log_store
        .query(
            params.level.as_deref(),
            params.source.as_deref(),
            params.search.as_deref(),
            limit,
        )
        .await;

    // If in-memory buffer doesn't have enough entries, supplement from DB
    if mem_entries.len() < limit {
        let db_limit = (limit - mem_entries.len()) as u32;
        let db_entries = log_store
            .query_db(
                params.level.as_deref(),
                params.source.as_deref(),
                params.search.as_deref(),
                db_limit,
                None,
            )
            .await;

        // Merge: DB entries (older) first, then in-memory (newer)
        // Deduplicate by id for entries that were flushed to DB
        let mem_ids: HashSet<i64> = mem_entries
            .iter()
            .filter_map(|e| if e.id > 0 { Some(e.id) } else { None })
            .collect();

        let mut all_entries: Vec<LogEntryResponse> = db_entries
            .into_iter()
            .filter(|e| !mem_ids.contains(&e.id))
            .map(|e| LogEntryResponse {
                id: e.id,
                timestamp: e.timestamp,
                level: e.level,
                source: e.source,
                target: e.target,
                message: e.message,
            })
            .collect();

        all_entries.extend(mem_entries.into_iter().map(|e| LogEntryResponse {
            id: e.id,
            timestamp: e.timestamp,
            level: e.level,
            source: e.source,
            target: e.target,
            message: e.message,
        }));

        return Json(all_entries).into_response();
    }

    let response: Vec<LogEntryResponse> = mem_entries
        .into_iter()
        .map(|e| LogEntryResponse {
            id: e.id,
            timestamp: e.timestamp,
            level: e.level,
            source: e.source,
            target: e.target,
            message: e.message,
        })
        .collect();

    Json(response).into_response()
}

pub async fn get_logs_level(State(state): State<ApiState>) -> impl IntoResponse {
    let log_store = match &state.log_store {
        Some(store) => store,
        None => {
            return axum::response::Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body(Body::from("Log store not initialized"))
                .unwrap();
        }
    };

    let level_u8 = log_store.level.load(Ordering::Relaxed);
    let level_str = match level_u8 {
        0 => "trace",
        1 => "debug",
        2 => "info",
        3 => "warn",
        4 => "error",
        _ => "info",
    };

    Json(serde_json::json!({ "level": level_str })).into_response()
}

pub async fn put_logs_level(
    State(state): State<ApiState>,
    Json(body): Json<SetLevelRequest>,
) -> impl IntoResponse {
    let log_store = match &state.log_store {
        Some(store) => store,
        None => {
            return axum::response::Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body(Body::from("Log store not initialized"))
                .unwrap();
        }
    };

    let level_u8 = match body.level.to_lowercase().as_str() {
        "trace" => 0,
        "debug" => 1,
        "info" => 2,
        "warn" => 3,
        "error" => 4,
        _ => {
            return axum::response::Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from(
                    "Invalid level. Use: trace, debug, info, warn, error",
                ))
                .unwrap();
        }
    };

    log_store.level.store(level_u8, Ordering::Relaxed);
    tracing::info!("Log level changed to {}", body.level.to_lowercase());

    // Persist to DB
    if let Some(db) = state.server_state.db() {
        let _ = db
            .save_server_setting("log_level", &body.level.to_lowercase())
            .await;
    }

    // Update dynamic config
    {
        let mut dc = state.server_state.dynamic_config.write().await;
        dc.log_level = body.level.to_lowercase();
    }

    Json(serde_json::json!({ "level": body.level.to_lowercase() })).into_response()
}

pub async fn get_llm_logging(State(state): State<ApiState>) -> impl IntoResponse {
    let enabled = state
        .server_state
        .dynamic_config
        .read()
        .await
        .llm_request_logging;
    Json(serde_json::json!({ "enabled": enabled })).into_response()
}

pub async fn put_llm_logging(
    State(state): State<ApiState>,
    Json(body): Json<super::dto::SetLlmLoggingRequest>,
) -> impl IntoResponse {
    // 更新动态配置
    {
        let mut dc = state.server_state.dynamic_config.write().await;
        dc.llm_request_logging = body.enabled;
    }

    // 持久化到 DB
    if let Some(db) = state.server_state.db() {
        let _ = db
            .save_server_setting("llm_request_logging", if body.enabled { "true" } else { "false" })
            .await;
    }

    tracing::info!("LLM request logging toggled to {}", body.enabled);
    Json(serde_json::json!({ "enabled": body.enabled })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::auth::AuthConfig;
    use std::sync::Arc;

    async fn get_enabled(state: ApiState) -> bool {
        let resp = get_llm_logging(State(state)).await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice::<serde_json::Value>(&body).unwrap()["enabled"]
            .as_bool()
            .unwrap()
    }

    fn make_state() -> ApiState {
        ApiState {
            server_state: crate::server::control::ServerState::new(),
            auth_config: Arc::new(AuthConfig::new(None, None)),
            log_store: None,
        }
    }

    #[tokio::test]
    async fn test_get_llm_logging_default_true() {
        assert!(get_enabled(make_state()).await);
    }

    #[tokio::test]
    async fn test_put_llm_logging_toggles_and_persists() {
        let db = crate::server::db::Database::new(":memory:").await.unwrap();
        let state = ApiState {
            server_state: crate::server::control::ServerState::with_db(db),
            auth_config: Arc::new(AuthConfig::new(None, None)),
            log_store: None,
        };

        // PUT false
        let resp = put_llm_logging(
            State(state.clone()),
            Json(super::super::dto::SetLlmLoggingRequest { enabled: false }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["enabled"], serde_json::json!(false));

        // GET should reflect the new value
        assert!(!get_enabled(state.clone()).await);

        // DB should persist "false"
        let stored = state
            .server_state
            .db()
            .expect("db")
            .load_server_setting("llm_request_logging")
            .await
            .expect("load ok")
            .expect("stored");
        assert_eq!(stored, "false");
    }

    #[tokio::test]
    async fn test_put_llm_logging_turns_back_on() {
        let state = make_state();

        let resp = put_llm_logging(
            State(state.clone()),
            Json(super::super::dto::SetLlmLoggingRequest { enabled: true }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(get_enabled(state).await);
    }
}
