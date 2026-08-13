//! Agent workbench session 相关 handler：session CRUD + 模型/标题/归档 + 全局默认模型。
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};

use crate::agent::session::DEFAULT_MODEL_KEY;
use crate::mgmt::api::ApiState;

use super::dto::{
    CreateSessionRequest, DefaultModelResponse, UpdateSessionModelRequest, UpdateSessionRequest,
};
use super::new_id;

/// GET /api/agent/default-model — 读全局默认模型（未设置返回空串）。
pub async fn get_default_model(State(state): State<ApiState>) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match agent.db.load_server_setting(DEFAULT_MODEL_KEY).await {
        Ok(Some(m)) => Json(DefaultModelResponse { model: m }).into_response(),
        Ok(None) => Json(DefaultModelResponse {
            model: String::new(),
        })
        .into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// PUT /api/agent/default-model — 写全局默认模型（空串清除）。
pub async fn put_default_model(
    State(state): State<ApiState>,
    Json(body): Json<UpdateSessionModelRequest>,
) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match agent
        .db
        .save_server_setting(DEFAULT_MODEL_KEY, body.model.trim())
        .await
    {
        Ok(()) => StatusCode::OK.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub async fn list_sessions(
    State(state): State<ApiState>,
    Path(workspace_id): Path<String>,
) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match agent.db.agent_list_sessions(&workspace_id).await {
        Ok(list) => Json(list).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub async fn create_session(
    State(state): State<ApiState>,
    Path(workspace_id): Path<String>,
    Json(body): Json<CreateSessionRequest>,
) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match agent.db.agent_get_workspace(&workspace_id).await {
        Ok(Some(_)) => {}
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
    let id = new_id();
    match agent
        .db
        .agent_create_session(
            &id,
            &workspace_id,
            body.title.as_deref(),
            body.model.as_deref(),
        )
        .await
    {
        Ok(()) => match agent.db.agent_get_session(&id).await {
            Ok(Some(s)) => Json(s).into_response(),
            _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub async fn update_session(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateSessionRequest>,
) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match agent.db.agent_update_session_title(&id, &body.title).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub async fn update_session_model(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateSessionModelRequest>,
) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    // 会话不存在返回 404
    match agent.db.agent_get_session(&id).await {
        Ok(Some(_)) => {}
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
    let model = body.model.trim();
    let model = if model.is_empty() { None } else { Some(model) };
    match agent.db.agent_update_session_model(&id, model).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub async fn archive_session(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    // 归档即终结：杀掉该 session 的 ACP agent 进程（不存在则 no-op）。
    // 用 kill（保留 agent 侧持久化会话数据）——归档后重开会话可 resume 恢复。
    if let Some(bridge) = agent.acp_bridge.as_ref() {
        bridge.kill(&id).await;
    }
    match agent.db.agent_archive_session(&id).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub async fn delete_session(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    // 删除即终结：先发 ACP session/delete 让 agent 清理客户端持久化会话文件，
    // 再杀掉进程（不存在则 no-op）。
    if let Some(bridge) = agent.acp_bridge.as_ref() {
        bridge.kill_and_delete(&id).await;
    }
    match agent.db.agent_delete_session(&id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub async fn list_messages(
    State(state): State<ApiState>,
    Path(session_id): Path<String>,
) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match agent.db.agent_get_session(&session_id).await {
        Ok(Some(_)) => {}
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
    match agent.db.agent_list_messages(&session_id).await {
        Ok(list) => Json(list).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthConfig;
    use crate::control::ServerState;
    use crate::db::Database;
    use std::sync::Arc;

    async fn test_state() -> (ApiState, Database) {
        let db = Database::new(":memory:").await.unwrap();
        let server_state = ServerState::with_db(db.clone());
        (
            ApiState {
                server_state,
                auth_config: Arc::new(AuthConfig::new(None, None)),
                log_store: None,
            },
            db,
        )
    }

    #[tokio::test]
    async fn test_session_lifecycle() {
        let (state, db) = test_state().await;
        db.agent_create_workspace(
            "w1", "p", "nas", "host", "/p", None, None, "", None, None, None,
        )
        .await
        .unwrap();

        let resp = create_session(
            State(state.clone()),
            Path("w1".to_string()),
            Json(CreateSessionRequest {
                title: Some("t".into()),
                model: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = list_sessions(State(state.clone()), Path("w1".to_string()))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = list_messages(State(state), Path("s-nonexistent".to_string()))
            .await
            .into_response();
        // 消息列表对不存在的会话返回 404
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_update_session_model_endpoint() {
        let (state, db) = test_state().await;
        db.agent_create_workspace(
            "w1", "p", "nas", "host", "/p", None, None, "", None, None, None,
        )
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();

        // 设置模型
        let resp = update_session_model(
            State(state.clone()),
            Path("s1".to_string()),
            Json(UpdateSessionModelRequest {
                model: "claude-opus-5".into(),
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let s = db.agent_get_session("s1").await.unwrap().unwrap();
        assert_eq!(s.model.as_deref(), Some("claude-opus-5"));

        // 空串清除
        let resp = update_session_model(
            State(state.clone()),
            Path("s1".to_string()),
            Json(UpdateSessionModelRequest { model: "  ".into() }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(db
            .agent_get_session("s1")
            .await
            .unwrap()
            .unwrap()
            .model
            .is_none());

        // 不存在的会话 → 404
        let resp = update_session_model(
            State(state),
            Path("nope".to_string()),
            Json(UpdateSessionModelRequest { model: "x".into() }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_default_model_roundtrip() {
        let (state, _db) = test_state().await;

        // 未设置 → 空串
        let resp = get_default_model(State(state.clone()))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["model"], "");

        // 写入
        let resp = put_default_model(
            State(state.clone()),
            Json(UpdateSessionModelRequest {
                model: "deepseek-chat".into(),
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);

        // 读回
        let resp = get_default_model(State(state.clone()))
            .await
            .into_response();
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["model"], "deepseek-chat");

        // 空串清除 → 读回空串
        let resp = put_default_model(
            State(state.clone()),
            Json(UpdateSessionModelRequest { model: "".into() }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = get_default_model(State(state.clone()))
            .await
            .into_response();
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["model"], "");
    }
}
