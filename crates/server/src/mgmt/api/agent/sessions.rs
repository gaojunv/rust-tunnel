//! Agent workbench session 相关 handler：session CRUD + 模型/标题/归档 + 全局默认模型。
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};

use crate::agent::session::DEFAULT_MODEL_KEY;
use crate::mgmt::api::ApiState;

use super::dto::{
    CreateSessionRequest, DefaultModelResponse, ListMessagesParams, ListMessagesResponse,
    UpdateSessionModelRequest, UpdateSessionRequest,
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
    // kill 内部 flush 回合缓冲后触发记忆蒸馏（ACP 路径）；runner（非 ACP）会话
    // 由下方 handler 触发。CAS 防重，两路并发唯一赢家。
    if let Some(bridge) = agent.acp_bridge.as_ref() {
        bridge.kill(&id).await;
    }
    // AI 记忆蒸馏（归档触发，runner 路径兜底）：agent_archive_session 之前调用，
    // 保证会话行存在、快照可读。CAS 未命中（ACP 路径已赢/已蒸馏）直接返回。
    #[cfg(feature = "rag")]
    if let Some(memory) = agent.memory.as_ref() {
        crate::agent::memory::distill::trigger_distill(memory, &id, "archive").await;
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
    // AI 记忆蒸馏（删除触发）：**先同步快照**再删行——FK 级联删会先删消息，
    // 异步蒸馏（spawn 后读 DB）会无料可用。快照在删行前采集，蒸馏在删行后触发。
    #[cfg(feature = "rag")]
    let distill_snapshot = if let Some(memory) = agent.memory.as_ref() {
        crate::agent::memory::distill::load_snapshot(memory, &id).await
    } else {
        None
    };
    // 蒸馏 CAS 预留：kill_and_delete → kill 内部会触发归档蒸馏（此时行还在、
    // 会 CAS 赢）；不预留则删行后 trigger_distill_with_snapshot（行已不在）还会
    // 再蒸馏一次，同一内容双跑有产生重复记忆的竞态。此处先抢占标记，kill 的
    // 触发因 CAS 未命中（行存在且已标记）让位——删除路径独占蒸馏权。
    #[cfg(feature = "rag")]
    if let Some(memory) = agent.memory.as_ref() {
        let _ = memory.db.memory_mark_distilled_if_not(&id).await;
    }
    // 删除即终结：先发 ACP session/delete 让 agent 清理客户端持久化会话文件，
    // 再杀掉进程（不存在则 no-op）。
    if let Some(bridge) = agent.acp_bridge.as_ref() {
        bridge.kill_and_delete(&id).await;
    }
    match agent.db.agent_delete_session(&id).await {
        Ok(()) => {
            // 删行后触发蒸馏：CAS 命中不了（行已不存在）但快照在手仍可蒸馏；
            // 其他路径已赢 CAS（归档已蒸馏）时让位。
            #[cfg(feature = "rag")]
            if let Some(snapshot) = distill_snapshot {
                if let Some(memory) = agent.memory.as_ref() {
                    crate::agent::memory::distill::trigger_distill_with_snapshot(
                        memory,
                        snapshot,
                        "delete",
                    )
                    .await;
                }
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// GET /api/agent/sessions/:id/messages — 分页返回会话消息（默认最近 200 条）。
///
/// query 参数：
/// - `limit`（可选，默认 200，clamp 到 [1, 500]）
/// - `before`（可选，消息 id 游标）：返回该消息更早的最近 limit 条（游标不含）
///
/// 响应体：`{ "messages": [...], "has_more": bool }`。`has_more` 表示游标（或最旧
/// 一条）之前是否还有更早的消息。排序与 `agent_list_messages` 一致（rowid 升序）。
pub async fn list_messages(
    State(state): State<ApiState>,
    Path(session_id): Path<String>,
    Query(params): Query<ListMessagesParams>,
) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match agent.db.agent_get_session(&session_id).await {
        Ok(Some(_)) => {}
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
    let limit = params.limit.unwrap_or(200).clamp(1, 500);
    match agent
        .db
        .agent_list_messages_page(&session_id, params.before.as_deref(), limit)
        .await
    {
        Ok((messages, has_more)) => Json(ListMessagesResponse { messages, has_more }).into_response(),
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

        let resp = list_messages(
            State(state),
            Path("s-nonexistent".to_string()),
            Query(ListMessagesParams::default()),
        )
        .await
        .into_response();
        // 消息列表对不存在的会话返回 404
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// 解析 list_messages 响应体 → (messages, has_more)。
    async fn parse_messages(
        resp: impl IntoResponse,
    ) -> (Vec<serde_json::Value>, bool) {
        let resp = resp.into_response();
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let msgs = json["messages"].as_array().cloned().unwrap_or_default();
        let has_more = json["has_more"].as_bool().unwrap_or(false);
        (msgs, has_more)
    }

    /// 分页 API 语义：无参取最近 N 条、before 翻页、before 不存在、空会话、
    /// limit 超总量。
    #[tokio::test]
    async fn test_list_messages_pagination_api() {
        let (state, db) = test_state().await;
        db.agent_create_workspace(
            "w1", "p", "nas", "host", "/p", None, None, "", None, None, None,
        )
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();
        for i in 0..250 {
            db.agent_add_message(&format!("m{i}"), "s1", "user", &format!("msg {i}"), None)
                .await
                .unwrap();
        }

        // 无参 → 最近 200 条（m50..m249）、has_more=true
        let (msgs, has_more) = parse_messages(
            list_messages(
                State(state.clone()),
                Path("s1".to_string()),
                Query(ListMessagesParams::default()),
            )
            .await,
        )
        .await;
        assert!(has_more);
        assert_eq!(msgs.len(), 200);
        assert_eq!(msgs[0]["id"], "m50");
        assert_eq!(msgs[199]["id"], "m249");

        // before=m50 翻页 → m50 之前的 50 条（m0..m49）、has_more=false
        let (msgs, has_more) = parse_messages(
            list_messages(
                State(state.clone()),
                Path("s1".to_string()),
                Query(ListMessagesParams {
                    limit: None,
                    before: Some("m50".into()),
                }),
            )
            .await,
        )
        .await;
        assert!(!has_more);
        assert_eq!(msgs.len(), 50);
        assert_eq!(msgs[0]["id"], "m0");
        assert_eq!(msgs[49]["id"], "m49");

        // before 指向不存在的 id → 空 + has_more=false
        let (msgs, has_more) = parse_messages(
            list_messages(
                State(state.clone()),
                Path("s1".to_string()),
                Query(ListMessagesParams {
                    limit: None,
                    before: Some("no-such".into()),
                }),
            )
            .await,
        )
        .await;
        assert!(msgs.is_empty());
        assert!(!has_more);

        // 空会话 → 空 + has_more=false
        db.agent_create_session("s2", "w1", None, None)
            .await
            .unwrap();
        let (msgs, has_more) = parse_messages(
            list_messages(
                State(state.clone()),
                Path("s2".to_string()),
                Query(ListMessagesParams::default()),
            )
            .await,
        )
        .await;
        assert!(msgs.is_empty());
        assert!(!has_more);

        // limit 超过总量：limit=999 但只有 250 条 → 全量 250 + has_more=false
        let (msgs, has_more) = parse_messages(
            list_messages(
                State(state),
                Path("s1".to_string()),
                Query(ListMessagesParams {
                    limit: Some(999),
                    before: None,
                }),
            )
            .await,
        )
        .await;
        assert_eq!(msgs.len(), 250);
        assert!(!has_more);
    }

    /// limit 上限 clamp：limit=999 在 600 条会话上 clamp 到 500。
    #[tokio::test]
    async fn test_list_messages_limit_clamped_to_500() {
        let (state, db) = test_state().await;
        db.agent_create_workspace(
            "w1", "p", "nas", "host", "/p", None, None, "", None, None, None,
        )
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();
        for i in 0..600 {
            db.agent_add_message(&format!("m{i}"), "s1", "user", &format!("msg {i}"), None)
                .await
                .unwrap();
        }

        let (msgs, has_more) = parse_messages(
            list_messages(
                State(state),
                Path("s1".to_string()),
                Query(ListMessagesParams {
                    limit: Some(999),
                    before: None,
                }),
            )
            .await,
        )
        .await;
        assert_eq!(msgs.len(), 500, "limit 999 必须 clamp 到 500");
        assert_eq!(msgs[0]["id"], "m100", "取最近 500 条");
        assert!(has_more, "600 条 > 500 仍有更早");
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
