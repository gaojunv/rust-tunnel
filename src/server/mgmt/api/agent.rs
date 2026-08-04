//! Agent workbench REST + WebSocket handlers.
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;

use crate::server::agent::session::SessionRuntime;
use crate::server::auth::validate_token;
use crate::server::llm::ChatMessage;

use super::ApiState;

#[derive(Debug, Deserialize)]
pub struct CreateWorkspaceRequest {
    pub name: String,
    pub client_id: String,
    pub runtime_type: String, // "host" | "docker"
    pub root_path: String,
    pub docker_image: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateWorkspaceRequest {
    pub name: String,
    pub root_path: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateSessionRequest {
    pub title: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateSessionRequest {
    pub title: String,
}

/// Generate a random hex id (32 hex chars, 128-bit).
fn new_id() -> String {
    format!("{:032x}", rand::random::<u128>())
}

#[derive(Debug, Deserialize)]
pub struct AgentWsQuery {
    pub session_id: String,
    pub token: Option<String>,
}

/// GET /api/agent/ws?session_id=xxx&token=<jwt>
/// Public route; JWT validated from query param (browser WebSocket can't set headers).
pub async fn agent_ws(
    State(state): State<ApiState>,
    Query(params): Query<AgentWsQuery>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    if state.auth_config.is_enabled() {
        let token = params.token.as_deref().unwrap_or("");
        if token.is_empty() || validate_token(token, &state.auth_config.jwt_secret).is_err() {
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }
    ws.on_upgrade(move |socket| handle_agent_socket(state, socket, params.session_id))
        .into_response()
}

/// Extract the user-message content from a WebSocket message; returns None for
/// non-text, unparseable, or non-`user_message` frames.
fn parse_user_message(msg: Message) -> Option<String> {
    let Message::Text(text) = msg else {
        return None;
    };
    let Ok(body) = serde_json::from_str::<serde_json::Value>(&text) else {
        return None;
    };
    if body.get("type").and_then(|t| t.as_str()) != Some("user_message") {
        return None;
    }
    body.get("content").and_then(|c| c.as_str()).map(str::to_string)
}

async fn handle_agent_socket(state: ApiState, socket: WebSocket, session_id: String) {
    let (mut ws_sink, mut ws_stream) = socket.split();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<serde_json::Value>(64);

    // 推送任务：event_rx → WebSocket。对端断开导致 send 失败时不再 break，而是
    // 继续 drain event_rx——runner 内部仍是阻塞式 send().await，但只要接收端持续
    // 消费，64 槽 channel 就不会填满，runner 永不阻塞。所有发送方 drop 后
    // recv() 返回 None，任务自然结束；外层循环退出后仍由 push_task.abort() 兜底。
    let push_task = tokio::spawn(async move {
        let mut sink_alive = true;
        while let Some(ev) = event_rx.recv().await {
            let text = serde_json::to_string(&ev).unwrap_or_default();
            if sink_alive && ws_sink.send(Message::Text(text)).await.is_err() {
                sink_alive = false;
            }
        }
    });

    // Live per-connection runtime cache. Reloading from the DB between turns is
    // protocol-invalid: assistant tool_calls messages are never persisted and
    // tool rows lack tool_call_id, so a DB replay yields a message sequence the
    // OpenAI API rejects (400). Instead, load once on the first user message
    // (which is persisted first), then append user text onto the in-memory
    // messages for later turns — the runner's in-memory path is valid. A fresh
    // WebSocket connection does reload, which may lose prior tool-call
    // structure; that is the accepted MVP tradeoff.
    let mut rt_cache: Option<SessionRuntime> = None;
    // 回合进行中对端又发来的用户消息：最多缓冲一条，当前 turn 结束后优先处理。
    let mut pending: Option<String> = None;

    loop {
        // 优先消费缓冲的 pending 消息；否则从 socket 读取下一条。
        let content = if let Some(p) = pending.take() {
            p
        } else {
            let msg = match ws_stream.next().await {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                Some(Ok(m)) => m,
            };
            match parse_user_message(msg) {
                Some(c) => c,
                None => continue,
            }
        };

        let (agent, llm) = match (
            state.server_state.agent_state.clone(),
            state
                .server_state
                .proxy_state
                .llm_state
                .read()
                .await
                .as_ref()
                .cloned(),
        ) {
            (Some(a), Some(l)) => (a, l),
            _ => {
                let _ = event_tx
                    .send(serde_json::json!({"type": "error", "message": "agent or LLM gateway not initialized"}))
                    .await;
                continue;
            }
        };

        // 持久化 user 消息（保持会话历史完整，供 Web 端与重连后的首轮恢复）。
        let msg_id = format!("{:032x}", rand::random::<u128>());
        let _ = agent
            .db
            .agent_add_message(&msg_id, &session_id, "user", &content, None)
            .await;

        // 首个用户消息：从 DB 重建运行时（含刚写入的 user 消息）；后续消息直接追加到内存 messages。
        let rt = match rt_cache.as_mut() {
            Some(rt) => {
                rt.messages.push(ChatMessage::text("user", content));
                rt
            }
            None => {
                let mut loaded = match SessionRuntime::load(&agent.db, &session_id, "").await {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = event_tx
                            .send(serde_json::json!({"type": "error", "message": e}))
                            .await;
                        continue;
                    }
                };
                // 模型为空 → 取 LLM 网关第一个可用模型
                if loaded.model.is_empty() {
                    if let Ok(models) =
                        crate::server::llm::router::list_available_models(&llm).await
                    {
                        if let Some(first) = models.first() {
                            if let Some(name) = first.get("id").and_then(|v| v.as_str()) {
                                loaded.model = name.to_string();
                            }
                        }
                    }
                    if loaded.model.is_empty() {
                        let _ = event_tx
                            .send(serde_json::json!({"type": "error", "message": "no LLM model configured"}))
                            .await;
                        continue;
                    }
                }
                rt_cache = Some(loaded);
                rt_cache.as_mut().expect("rt_cache just assigned")
            }
        };

        // 每个用户消息串行运行一个 agent turn。turn 期间持续观察 ws_stream：
        // 对端断开则丢弃 turn future（取消该回合）并退出外层循环，避免连接任务
        // 永久挂起（read 循环不再 poll ws_stream 导致 close 永远不可见）；若 turn
        // 期间对端又发来 user_message，缓冲到 pending，turn 结束后优先处理。
        let turn = crate::server::agent::runner::run_agent_turn(
            agent.clone(),
            llm.clone(),
            rt,
            event_tx.clone(),
        );
        tokio::pin!(turn);
        let result = loop {
            tokio::select! {
                r = &mut turn => break Some(r),
                msg = ws_stream.next() => match msg {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break None,
                    Some(Ok(m)) => {
                        if let Some(c) = parse_user_message(m) {
                            pending.get_or_insert(c);
                        }
                    }
                },
            }
        };
        let Some(result) = result else {
            break;
        };
        // 仅 Exhausted 场景 runner 会发终态 error 帧；其余 Err 路径需要在此兜底，
        // 否则浏览器会一直等待。Exhausted 场景会重复一个 error 帧，MVP 下无害。
        if let Err(e) = result {
            let _ = event_tx
                .send(serde_json::json!({"type": "error", "message": e}))
                .await;
        }
    }

    push_task.abort();
}

pub async fn list_workspaces(State(state): State<ApiState>) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match agent.db.agent_list_workspaces().await {
        Ok(list) => Json(list).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub async fn create_workspace(
    State(state): State<ApiState>,
    Json(body): Json<CreateWorkspaceRequest>,
) -> impl IntoResponse {
    if body.runtime_type != "host" && body.runtime_type != "docker" {
        return (
            StatusCode::BAD_REQUEST,
            "runtime_type must be 'host' or 'docker'",
        )
            .into_response();
    }
    if body.runtime_type == "docker" && body.docker_image.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            "docker_image required for docker runtime",
        )
            .into_response();
    }
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let id = new_id();
    match agent
        .db
        .agent_create_workspace(
            &id,
            &body.name,
            &body.client_id,
            &body.runtime_type,
            &body.root_path,
            body.docker_image.as_deref(),
        )
        .await
    {
        Ok(()) => match agent.db.agent_get_workspace(&id).await {
            Ok(Some(ws)) => Json(ws).into_response(),
            _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub async fn get_workspace(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match agent.db.agent_get_workspace(&id).await {
        Ok(Some(ws)) => Json(ws).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub async fn update_workspace(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateWorkspaceRequest>,
) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match agent
        .db
        .agent_update_workspace(&id, &body.name, &body.root_path)
        .await
    {
        Ok(()) => get_workspace(State(state), Path(id)).await.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub async fn delete_workspace(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match agent.db.agent_delete_workspace(&id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
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

pub async fn archive_session(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(agent) = &state.server_state.agent_state else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
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
    use crate::server::auth::AuthConfig;
    use crate::server::control::ServerState;
    use crate::server::db::Database;
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
    async fn test_create_and_list_workspaces() {
        let (state, _db) = test_state().await;
        let resp = create_workspace(
            State(state.clone()),
            Json(CreateWorkspaceRequest {
                name: "proj".into(),
                client_id: "nas".into(),
                runtime_type: "host".into(),
                root_path: "/home/u/proj".into(),
                docker_image: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = list_workspaces(State(state)).await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_create_workspace_rejects_bad_runtime_type() {
        let (state, _db) = test_state().await;
        let resp = create_workspace(
            State(state),
            Json(CreateWorkspaceRequest {
                name: "x".into(),
                client_id: "nas".into(),
                runtime_type: "kubernetes".into(),
                root_path: "/p".into(),
                docker_image: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_session_lifecycle() {
        let (state, db) = test_state().await;
        db.agent_create_workspace("w1", "p", "nas", "host", "/p", None)
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
}
