use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;

use super::ApiState;

#[derive(Debug, Deserialize)]
pub struct StatsQueryParams {
    pub entity_type: Option<Vec<String>>,
    pub entity_id: Option<Vec<String>>,
    pub start: String,
    pub end: String,
}

#[derive(Debug, Deserialize)]
pub struct StatsStreamQuery {
    pub entity_type: Option<String>,
    pub token: Option<String>,
}

// GET /api/stats/query
// 注意：必须使用 axum_extra 的 Query（serde_html_form），axum 自带的 Query
// （serde_urlencoded）不支持重复的查询参数（entity_type=a&entity_type=b）。
pub async fn get_stats_query(
    State(state): State<ApiState>,
    axum_extra::extract::Query(params): axum_extra::extract::Query<StatsQueryParams>,
) -> impl IntoResponse {
    let start = match chrono::DateTime::parse_from_rfc3339(&params.start) {
        Ok(dt) => dt.with_timezone(&chrono::Utc),
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("Invalid start: {}", e)})),
            )
                .into_response()
        }
    };
    let end = match chrono::DateTime::parse_from_rfc3339(&params.end) {
        Ok(dt) => dt.with_timezone(&chrono::Utc),
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("Invalid end: {}", e)})),
            )
                .into_response()
        }
    };
    if (end - start) > chrono::Duration::days(7) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Range <= 7 days"})),
        )
            .into_response();
    }
    let entity_types = params.entity_type.unwrap_or_default();
    let entity_ids = params.entity_id.unwrap_or_default();
    let db = match state.server_state.get_db() {
        Some(db) => db,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "No DB"})),
            )
                .into_response()
        }
    };
    match db
        .query_stats_snapshots(&entity_types, &entity_ids, start, end)
        .await
    {
        Ok(snapshots) => Json(serde_json::json!({"snapshots": snapshots})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// GET /api/stats/summary
pub async fn get_stats_summary(State(state): State<ApiState>) -> impl IntoResponse {
    Json(state.server_state.stats_collector.get_summary()).into_response()
}

// GET /api/stats/stream
pub async fn sse_stats_stream(
    State(state): State<ApiState>,
    Query(params): Query<StatsStreamQuery>,
) -> impl IntoResponse {
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
                .body(axum::body::Body::from("Unauthorized"))
                .unwrap();
        }
    }
    let entity_type_filter = params.entity_type;
    let mut rx = state.server_state.stats_collector.subscribe();
    let stream = async_stream::stream! {
        loop {
            match tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv()).await {
                Ok(Ok(snapshot)) => {
                    if let Some(ref et) = entity_type_filter {
                        if snapshot.entity_type != *et { continue; }
                    }
                    let json = serde_json::to_string(&snapshot).unwrap_or_default();
                    yield Ok::<_, std::convert::Infallible>(
                        axum::response::sse::Event::default().event("snapshot").data(json),
                    );
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(n))) => {
                    yield Ok::<_, std::convert::Infallible>(
                        axum::response::sse::Event::default().event("sync").data(format!(r#"{{"lagged":{}}}"#, n)),
                    );
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => break,
                Err(_) => {
                    yield Ok::<_, std::convert::Infallible>(
                        axum::response::sse::Event::default().event("ping").data(""),
                    );
                }
            }
        }
    };
    axum::response::sse::Sse::new(stream)
        .keep_alive(
            axum::response::sse::KeepAlive::new().interval(std::time::Duration::from_secs(30)),
        )
        .into_response()
}