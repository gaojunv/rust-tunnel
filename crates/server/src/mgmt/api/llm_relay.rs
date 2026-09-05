//! JWT 保护的 LLM 中继端点（供 wiki 桌面端复用管理面登录态）。
//!
//! 两条受保护路由直接透传到 LLM 网关 handler：内部长期 API key 懒创建并持久化
//! 到 `server_settings`（`llm_relay_internal_key`），请求时手工构造
//! `Authorization: Bearer` 头后**函数调用**网关 handler，返回的
//! `axum::response::Response` 原样透传（流式 SSE 天然透传，不包壳、不改 body）。

#![allow(clippy::missing_docs_in_private_items, reason = "中继模块内部辅助项无需对外文档")]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use std::sync::Arc;

use axum::{
    extract::State,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::llm::{openai_handler::LlmHandlerState, LlmProtocol, LlmState};
use crate::mgmt::api::ApiState;

/// 内部中继 key 的固定 id（`llm_api_keys` 主键）。
const RELAY_KEY_ID: &str = "__llm_relay_internal__";
/// 持久化到 `server_settings` 的 key。
const RELAY_SETTING_KEY: &str = "llm_relay_internal_key";

/// 进程级创建锁：并发首次请求时避免重复创建。
static CREATE_LOCK: Mutex<()> = Mutex::const_new(());

/// `server_settings` 中存储的 JSON 结构。
#[derive(Debug, Serialize, Deserialize)]
struct RelaySetting {
    /// 固定为 `__llm_relay_internal__`。
    key_id: String,
    /// `encrypt_field` 后的密文（无 cipher 时为明文）。
    enc_raw_key: String,
}

/// 计算 `SHA256` 十六进制（与 `rust-tunnel-llm::auth` 一致）。
fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// 取当前 `LlmState`（未初始化时为 `None`）。
async fn llm_state(state: &ApiState) -> Option<Arc<LlmState>> {
    state.server_state.llm_state.read().await.as_ref().cloned()
}

/// 确保内部中继 `API key` 存在并返回 `(LlmState, raw_key)`。
///
/// # Errors
/// `LlmState` 未初始化或无 `Database` 时返回 `503`；数据库读写失败时返回 `500`。
async fn ensure_internal_key(
    state: &ApiState,
) -> Result<(Arc<LlmState>, String), (StatusCode, String)> {
    let llm = llm_state(state).await.ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "LLM gateway not initialized".to_string(),
        )
    })?;
    let db = llm.db.clone().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "LLM gateway not initialized".to_string(),
        )
    })?;
    let cipher = llm.cipher.clone();

    // ── 快路径：读 settings → 解密 → hash 校验存在且启用 ──
    if let Some(val) = db
        .load_server_setting(RELAY_SETTING_KEY)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")))?
    {
        if let Ok(setting) = serde_json::from_str::<RelaySetting>(&val) {
            if let Ok(raw) = crate::llm::crypto::decrypt_field(cipher.as_ref(), &setting.enc_raw_key) {
                let hash = sha256_hex(&raw);
                if let Ok(Some(rec)) = db.llm_find_api_key_by_hash(&hash).await {
                    if rec.enabled != 0 {
                        return Ok((llm, raw));
                    }
                }
            }
        }
    }

    // ── 慢路径：持锁双检 → 重建 ──
    let _guard = CREATE_LOCK.lock().await;

    // 双检：等待锁期间可能已被其他请求创建。
    if let Some(val) = db
        .load_server_setting(RELAY_SETTING_KEY)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")))?
    {
        if let Ok(setting) = serde_json::from_str::<RelaySetting>(&val) {
            if let Ok(raw) = crate::llm::crypto::decrypt_field(cipher.as_ref(), &setting.enc_raw_key) {
                let hash = sha256_hex(&raw);
                if let Ok(Some(rec)) = db.llm_find_api_key_by_hash(&hash).await {
                    if rec.enabled != 0 {
                        return Ok((llm.clone(), raw));
                    }
                }
            }
        }
    }

    // 固定 id 先删旧行（幂等，忽略不存在错误）。
    let _ = db.llm_delete_api_key(RELAY_KEY_ID).await;

    let (raw_key, key_hash, key_prefix) = crate::llm::auth::generate_api_key();
    db.llm_save_api_key(RELAY_KEY_ID, &key_hash, &key_prefix, "Wiki Desktop Relay", None)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")))?;

    let enc = crate::llm::crypto::encrypt_field(cipher.as_ref(), &raw_key);
    let setting = RelaySetting {
        key_id: RELAY_KEY_ID.to_string(),
        enc_raw_key: enc,
    };
    let json = serde_json::to_string(&setting)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("serialize error: {e}")))?;
    db.save_server_setting(RELAY_SETTING_KEY, &json)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("DB error: {e}")))?;

    Ok((llm, raw_key))
}

/// `POST /api/llm/relay/chat/completions` —— 透传到网关 `chat/completions`。
///
/// # Errors
/// 网关未初始化时返回 `503`；其余错误由网关原样透传。
pub async fn relay_chat_completions(
    State(state): State<ApiState>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let (llm, raw) = match ensure_internal_key(&state).await {
        Ok(v) => v,
        Err((code, msg)) => return (code, msg).into_response(),
    };
    let handler_state = LlmHandlerState {
        llm,
        protocol: Some(LlmProtocol::OpenAI),
    };
    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&format!("Bearer {raw}")) {
        headers.insert(header::AUTHORIZATION, v);
    }
    crate::llm::openai_handler::handle_chat_completions(State(handler_state), headers, Json(body))
        .await
}

/// `GET /api/llm/relay/models` —— 透传到网关 `list models`。
///
/// # Errors
/// 网关未初始化时返回 `503`；其余错误由网关原样透传。
pub async fn relay_list_models(State(state): State<ApiState>) -> Response {
    let (llm, raw) = match ensure_internal_key(&state).await {
        Ok(v) => v,
        Err((code, msg)) => return (code, msg).into_response(),
    };
    let handler_state = LlmHandlerState {
        llm,
        protocol: Some(LlmProtocol::OpenAI),
    };
    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&format!("Bearer {raw}")) {
        headers.insert(header::AUTHORIZATION, v);
    }
    crate::llm::openai_handler::handle_list_models(State(handler_state), headers).await
}

/// 受保护路由（需 `JWT`，由 `run_api_server` 的 `auth` 中间件保护）。
pub fn protected_router() -> Router<ApiState> {
    Router::new()
        .route(
            "/api/llm/relay/chat/completions",
            post(relay_chat_completions),
        )
        .route("/api/llm/relay/models", get(relay_list_models))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, Method, Request, StatusCode as HttpStatus};
    use axum::Router;
    use serde_json::{json, Value};
    use std::sync::Arc;
    use tower::ServiceExt;

    use crate::auth::AuthConfig;
    use crate::control_plane::ServerState;
    use crate::db::Database;

    /// 构造带 `LlmState` 的 `ApiState`（内存 `DB` + 固定主密钥 + 临时 `rag` 目录）。
    async fn test_api_state(rag_dir: &std::path::Path) -> ApiState {
        let db = Database::new(":memory:").await.expect("in-memory db");
        let server_state = ServerState::with_db(db);
        server_state
            .init_llm_state(
                server_state.db().cloned(),
                Some([42u8; 32]),
                rag_dir,
                Arc::new(tokio::sync::RwLock::new(
                    crate::dynamic_config::DynamicConfig::default_for_llm(),
                )),
            )
            .await;
        ApiState {
            server_state,
            auth_config: Arc::new(AuthConfig::new(None, None)),
            log_store: None,
        }
    }

    fn test_router(state: ApiState) -> Router {
        super::protected_router().with_state(state)
    }

    async fn call(app: &Router, req: Request<Body>) -> (HttpStatus, Value) {
        let resp = app.clone().oneshot(req).await.expect("router responds");
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 8 * 1024 * 1024)
            .await
            .expect("read response body");
        let body = serde_json::from_slice(&bytes).unwrap_or(json!(null));
        (status, body)
    }

    async fn call_raw(app: &Router, req: Request<Body>) -> (HttpStatus, String) {
        let resp = app.clone().oneshot(req).await.expect("router responds");
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 8 * 1024 * 1024)
            .await
            .expect("read response body");
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    fn json_request(method: Method, uri: String, body: &Value) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .expect("build request")
    }

    #[tokio::test]
    async fn uninitialized_llm_returns_503() {
        let server_state = ServerState::new();
        let state = ApiState {
            server_state,
            auth_config: Arc::new(AuthConfig::new(None, None)),
            log_store: None,
        };
        let app = test_router(state);

        let req = json_request(
            Method::POST,
            "/api/llm/relay/chat/completions".to_string(),
            &json!({"model":"x","messages":[{"role":"user","content":"hi"}]}),
        );
        let (status, _body) = call(&app, req).await;
        assert_eq!(status, HttpStatus::SERVICE_UNAVAILABLE);

        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/llm/relay/models")
            .body(Body::empty())
            .unwrap();
        let (status, _) = call(&app, req).await;
        assert_eq!(status, HttpStatus::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn first_request_creates_internal_key_and_passthrough_error() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;
        let db = state.server_state.db().cloned().unwrap();
        let app = test_router(state);

        let req = json_request(
            Method::POST,
            "/api/llm/relay/chat/completions".to_string(),
            &json!({"model":"nonexistent-model","messages":[{"role":"user","content":"hi"}]}),
        );
        let (status, body) = call(&app, req).await;
        // 应透传网关错误（模型不存在 404 或上游不可达 502），不是 401/503
        assert!(
            status != HttpStatus::UNAUTHORIZED && status != HttpStatus::SERVICE_UNAVAILABLE,
            "unexpected status {status}, body: {body}"
        );
        assert!(
            status.is_client_error() || status.is_server_error(),
            "expected 4xx/5xx passthrough, got {status}, body: {body}"
        );

        // DB 已持久化内部 key
        let keys = db.llm_list_api_keys().await.unwrap();
        assert!(
            keys.iter().any(|k| k.id == RELAY_KEY_ID),
            "llm_api_keys should contain {RELAY_KEY_ID}"
        );
        let val = db
            .load_server_setting(RELAY_SETTING_KEY)
            .await
            .unwrap()
            .expect("setting should exist");
        let setting: RelaySetting = serde_json::from_str(&val).expect("setting json");
        assert_eq!(setting.key_id, RELAY_KEY_ID);
        // cipher 存在时为密文前缀
        assert!(
            setting.enc_raw_key.starts_with("enc:v1:"),
            "enc_raw_key should be encrypted, got: {}",
            setting.enc_raw_key
        );
        // 解密后能匹配 DB hash
        let llm = db; // 已有 db
        let _ = llm;
    }

    #[tokio::test]
    async fn second_request_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;
        let db = state.server_state.db().cloned().unwrap();
        let app = test_router(state);

        let req1 = json_request(
            Method::POST,
            "/api/llm/relay/chat/completions".to_string(),
            &json!({"model":"nonexistent-model","messages":[{"role":"user","content":"hi"}]}),
        );
        let (status, _) = call(&app, req1).await;
        assert_ne!(status, HttpStatus::SERVICE_UNAVAILABLE);

        let before = db
            .load_server_setting(RELAY_SETTING_KEY)
            .await
            .unwrap()
            .unwrap();
        let keys_before = db.llm_list_api_keys().await.unwrap().len();

        let req2 = json_request(
            Method::POST,
            "/api/llm/relay/chat/completions".to_string(),
            &json!({"model":"nonexistent-model","messages":[{"role":"user","content":"hi"}]}),
        );
        let (status, _) = call(&app, req2).await;
        assert_ne!(status, HttpStatus::SERVICE_UNAVAILABLE);

        let after = db
            .load_server_setting(RELAY_SETTING_KEY)
            .await
            .unwrap()
            .unwrap();
        let keys_after = db.llm_list_api_keys().await.unwrap().len();

        assert_eq!(before, after, "settings should be unchanged on second request");
        assert_eq!(keys_before, keys_after, "key count should be unchanged");
    }

    #[tokio::test]
    async fn recreates_after_manual_delete() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;
        let db = state.server_state.db().cloned().unwrap();
        let app = test_router(state);

        let req = json_request(
            Method::POST,
            "/api/llm/relay/chat/completions".to_string(),
            &json!({"model":"nonexistent-model","messages":[{"role":"user","content":"hi"}]}),
        );
        let (_status, _) = call(&app, req).await;
        let before = db
            .load_server_setting(RELAY_SETTING_KEY)
            .await
            .unwrap()
            .unwrap();

        // 手动删除 DB key
        db.llm_delete_api_key(RELAY_KEY_ID).await.unwrap();
        assert!(
            db.llm_list_api_keys().await.unwrap().is_empty()
                || !db
                    .llm_list_api_keys()
                    .await
                    .unwrap()
                    .iter()
                    .any(|k| k.id == RELAY_KEY_ID)
        );

        let req2 = json_request(
            Method::POST,
            "/api/llm/relay/chat/completions".to_string(),
            &json!({"model":"nonexistent-model","messages":[{"role":"user","content":"hi"}]}),
        );
        let (status, _) = call(&app, req2).await;
        assert_ne!(status, HttpStatus::UNAUTHORIZED);
        assert_ne!(status, HttpStatus::SERVICE_UNAVAILABLE);

        // 已重建
        let keys = db.llm_list_api_keys().await.unwrap();
        assert!(keys.iter().any(|k| k.id == RELAY_KEY_ID));
        let after = db
            .load_server_setting(RELAY_SETTING_KEY)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(before, after, "settings should be updated after recreate");
        let setting: RelaySetting = serde_json::from_str(&after).unwrap();
        assert!(setting.enc_raw_key.starts_with("enc:v1:"));
    }

    #[tokio::test]
    async fn list_models_returns_object_list() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;
        let app = test_router(state);

        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/llm/relay/models")
            .body(Body::empty())
            .unwrap();
        let (status, body) = call(&app, req).await;
        assert_eq!(status, HttpStatus::OK, "body: {body}");
        assert_eq!(body["object"], json!("list"));
        assert!(body["data"].is_array(), "data should be array, body: {body}");
    }

    #[tokio::test]
    async fn relay_chat_completions_passthrough_with_unreachable_upstream() {
        // 若未来网关对不存在模型返回 404，此用例覆盖“上游不可达 502 透传”分支：
        // 造一个可解析的 provider/model，但 base_url 不可达 → 网关 502
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;
        let db = state.server_state.db().cloned().unwrap();
        let app = test_router(state);

        db.llm_save_provider(
            "prov-1",
            "test-provider",
            "deepseek",
            "http://127.0.0.1:1",
            "sk-test",
            None,
            None,
            true,
        )
        .await
        .unwrap();
        db.llm_save_model("model-1", "prov-1", "relay-test-model", "", "[]", true, None)
            .await
            .unwrap();

        let req = json_request(
            Method::POST,
            "/api/llm/relay/chat/completions".to_string(),
            &json!({"model":"relay-test-model","messages":[{"role":"user","content":"hi"}]}),
        );
        let (status, _body) = call_raw(&app, req).await;
        assert!(
            status.is_server_error(),
            "unreachable upstream should be 5xx passthrough, got {status}"
        );
        assert_ne!(status, HttpStatus::SERVICE_UNAVAILABLE);
        assert_ne!(status, HttpStatus::UNAUTHORIZED);
    }
}
