use axum::{
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
            crate::auth::validate_token(token, &state.auth_config.jwt_secret).is_ok()
        } else {
            false
        };

        if !is_valid {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    }

    let log_store = match &state.log_store {
        Some(store) => store.clone(),
        None => {
            return (StatusCode::SERVICE_UNAVAILABLE, "Log store not initialized").into_response();
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
            return (StatusCode::SERVICE_UNAVAILABLE, "Log store not initialized").into_response();
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
            return (StatusCode::SERVICE_UNAVAILABLE, "Log store not initialized").into_response();
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
            return (StatusCode::SERVICE_UNAVAILABLE, "Log store not initialized").into_response();
        }
    };

    let level_u8 = match body.level.to_lowercase().as_str() {
        "trace" => 0,
        "debug" => 1,
        "info" => 2,
        "warn" => 3,
        "error" => 4,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                "Invalid level. Use: trace, debug, info, warn, error",
            )
                .into_response();
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

    // 同步 LLM 热路径开关投影（LlmState 持有的 AtomicBool）
    if let Some(llm) = state.server_state.llm_state.read().await.as_ref() {
        llm.request_logging
            .store(body.enabled, std::sync::atomic::Ordering::Relaxed);
    }

    // 持久化到 DB
    if let Some(db) = state.server_state.db() {
        let _ = db
            .save_server_setting(
                "llm_request_logging",
                if body.enabled { "true" } else { "false" },
            )
            .await;
    }

    tracing::info!("LLM request logging toggled to {}", body.enabled);
    Json(serde_json::json!({ "enabled": body.enabled })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthConfig;
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
            server_state: crate::control_plane::ServerState::new(),
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
        let db = crate::db::Database::new(":memory:").await.unwrap();
        let state = ApiState {
            server_state: crate::control_plane::ServerState::with_db(db),
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

    /// 完整端到端 round-trip：PUT 关闭 → dynamic_config 同步 → DB 持久化 →
    /// 模拟服务重启（load_or_seed 回读）→ 重启后的 GET 仍为 false。
    #[tokio::test]
    async fn test_llm_logging_round_trip_survives_restart() {
        let db = crate::db::Database::new(":memory:").await.unwrap();
        let state = ApiState {
            server_state: crate::control_plane::ServerState::with_db(db.clone()),
            auth_config: Arc::new(AuthConfig::new(None, None)),
            log_store: None,
        };

        // PUT 关闭
        let resp = put_llm_logging(
            State(state.clone()),
            Json(super::super::dto::SetLlmLoggingRequest { enabled: false }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);

        // dynamic_config 已同步更新
        assert!(
            !state
                .server_state
                .dynamic_config
                .read()
                .await
                .llm_request_logging,
            "dynamic_config should be false after PUT"
        );

        // DB 已持久化为 "false"
        let stored = db
            .load_server_setting("llm_request_logging")
            .await
            .unwrap()
            .expect("llm_request_logging row should exist after PUT");
        assert_eq!(stored, "false");

        // 模拟重启：与 bin/server.rs 启动流程一致 —— load_or_seed 从 DB 回读
        let config = crate::config::ServerConfig::default();
        let reloaded = crate::dynamic_config::DynamicConfig::load_or_seed(&db, &config).await;
        assert!(
            !reloaded.llm_request_logging,
            "restart must read back false from DB"
        );

        // 重启后的新 ServerState 应用该配置后，GET 也应返回 false
        let restarted = ApiState {
            server_state: crate::control_plane::ServerState::with_db(db.clone()),
            auth_config: Arc::new(AuthConfig::new(None, None)),
            log_store: None,
        };
        restarted.server_state.set_dynamic_config(reloaded).await;
        assert!(!get_enabled(restarted).await, "GET after restart is false");
    }

    /// load_or_seed 的开关解析逻辑：只有 "1"/"true" 视为开启，其余为关闭。
    #[tokio::test]
    async fn test_llm_logging_load_or_seed_parsing() {
        let db = crate::db::Database::new(":memory:").await.unwrap();
        let config = crate::config::ServerConfig::default();

        // "true" → 开启
        db.save_server_setting("llm_request_logging", "true")
            .await
            .unwrap();
        assert!(
            crate::dynamic_config::DynamicConfig::load_or_seed(&db, &config)
                .await
                .llm_request_logging
        );

        // "1" → 开启（旧版本/兼容值）
        db.save_server_setting("llm_request_logging", "1")
            .await
            .unwrap();
        assert!(
            crate::dynamic_config::DynamicConfig::load_or_seed(&db, &config)
                .await
                .llm_request_logging
        );

        // "false" → 关闭
        db.save_server_setting("llm_request_logging", "false")
            .await
            .unwrap();
        assert!(
            !crate::dynamic_config::DynamicConfig::load_or_seed(&db, &config)
                .await
                .llm_request_logging
        );

        // 未识别的字符串 → 关闭（严格解析）
        db.save_server_setting("llm_request_logging", "yes")
            .await
            .unwrap();
        assert!(
            !crate::dynamic_config::DynamicConfig::load_or_seed(&db, &config)
                .await
                .llm_request_logging
        );
    }

    /// 全新 DB：load_or_seed 默认开启，并把默认值 "true" 回写 DB。
    #[tokio::test]
    async fn test_llm_logging_load_or_seed_defaults_true_on_fresh_db() {
        let db = crate::db::Database::new(":memory:").await.unwrap();
        let config = crate::config::ServerConfig::default();

        let dc = crate::dynamic_config::DynamicConfig::load_or_seed(&db, &config).await;
        assert!(dc.llm_request_logging, "fresh DB must default to enabled");

        let stored = db
            .load_server_setting("llm_request_logging")
            .await
            .unwrap()
            .expect("seeded row should exist");
        assert_eq!(stored, "true");
    }
}
