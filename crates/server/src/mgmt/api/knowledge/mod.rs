//! 统一知识容器 API（双索引容器 + 文档 + 页面 + 检索 + SSE）。
//!
//! 路由挂在 `/api/knowledge`，除 SSE 端点外均受 JWT 保护（SSE 的 token 走 query 参数，
//! 参照 `/api/logs/stream`）。向量本体在 `qdrant-edge` shard，元数据、原文与页面在
//! `SQLite`，两个索引（`vector` / `pages`）共享同一套容器与文档表。
//!
//! - 旧 `rag`（`/api/llm/kb*`）与旧 `agent/wiki`（`/api/agent/wiki*`）已合并至此；
//!   前端重定向由主会话另行处理，本模块只提供新路由。
//! - `MULTIPART_BODY_LIMIT` 沿用旧 `rag` 的 `20MB + 64KB`，旧 Wiki 侧此前**没有**
//!   `DefaultBodyLimit`（2MB 上传 bug 根源之一），统一后 pages 侧自动获得同样保护。
//! - 运行时 `KnowledgeRuntime` 复用 `LlmState.rag_tx` 统一事件通道与 `WikiState.ingest_sem`
//!   同一限流池（LLM 总并发 2，新建池等于翻倍）。
//! - 子模块按 handler 聚类：`sources`（容器 CRUD）、`docs`（文档）、`pages`（页面/图谱/搜索）、
//!   `query_events`（query / test-embedding / SSE）。

mod docs;
mod pages;
mod query_events;
mod sources;

pub use docs::{delete_doc, get_doc, list_docs, reindex_doc, upload_doc};
pub use pages::{delete_page, get_graph, get_page, list_pages, put_page, search_all, search_knowledge};
pub use query_events::{query_knowledge, sse_knowledge_events, test_embedding};
pub use sources::{create_source, delete_source, get_source, list_sources, patch_source, update_source};

use std::sync::Arc;

use axum::{
    extract::DefaultBodyLimit,
    http::StatusCode,
    routing::{get, post},
    Router,
};
use tokio::sync::{broadcast, Semaphore};

use crate::db::Database;
use crate::llm::crypto::LlmCipher;
use crate::llm::rag::ingest::KbEvent;
use crate::llm::rag::store::VectorStore;
use crate::llm::LlmState;
use crate::mgmt::api::ApiState;

/// multipart 请求体总长上限：取最大单文件上限（二进制 20MB）+ 开销。
/// 字面量与 `FileType::max_bytes` 的二进制值保持一致（const 上下文无法调用
/// 非 const fn，故写字面量并注明对应关系）。
/// 旧 Wiki 路由此前没有此层，2MB 以上文件会落入 axum 默认 2MB 限制；统一后
/// pages 侧自动获得同样的 20MB 保护。
pub(crate) const MULTIPART_BODY_LIMIT: usize = 20 * 1024 * 1024 + 64 * 1024;

/// 统一知识运行时（从 `LlmState` / `WikiState` 克隆，避免长持锁）。
/// `pub(crate)`：`docs::reindex_source_doc` 等需此类型。
pub(crate) struct KnowledgeRuntime {
    pub(crate) db: Database,
    pub(crate) store: VectorStore,
    pub(crate) cipher: Option<LlmCipher>,
    pub(crate) tx: broadcast::Sender<KbEvent>,
    /// 完整 `LlmState`（pages 抽取器 `LlmPageExtractor::new(db, llm)` 需要）。
    pub(crate) llm: LlmState,
    /// pages 侧限流池：**复用** `WikiState.ingest_sem` 同一池（LLM 总并发 2），
    /// 新建池等于把限额翻倍。
    pub(crate) pages_sem: Arc<Semaphore>,
}

/// 取当前 LLM 运行时状态（未初始化时为 `None` → 请求失败）。
pub(crate) async fn llm_state(state: &ApiState) -> Option<Arc<LlmState>> {
    state.server_state.llm_state.read().await.as_ref().cloned()
}

/// 组装统一知识运行时；LLM 未初始化 / 数据库缺失 / Wiki 未注入时返回可直出的错误。
/// 错误形态沿用旧 `rag_rt`（503 / 500），`pages_sem` 复用 `wiki_runtime` 的同一池。
pub(crate) async fn knowledge_rt(
    state: &ApiState,
) -> Result<KnowledgeRuntime, (StatusCode, String)> {
    let llm = llm_state(state).await.ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "LLM gateway not initialized".to_string(),
        )
    })?;
    let db = llm
        .db
        .clone()
        .ok_or_else(|| (StatusCode::INTERNAL_SERVER_ERROR, "no database".to_string()))?;
    // 复用 Wiki 的 LLM 并发池（2），语义是「LLM 调用总并发 2」。
    let wiki = crate::mgmt::api::agent::wiki_runtime(state)
        .map_err(|(c, m)| (c, m))?;
    Ok(KnowledgeRuntime {
        db,
        store: llm.rag_store.clone(),
        cipher: llm.cipher.clone(),
        tx: llm.rag_tx.clone(),
        llm: (*llm).clone(),
        pages_sem: wiki.ingest_sem.clone(),
    })
}

/// 公开路由（SSE 事件流，`?token=` 认证）。
pub fn public_router() -> Router<ApiState> {
    Router::new().route("/api/knowledge/events", get(sse_knowledge_events))
}

/// 受保护路由（需 JWT）。
pub fn protected_router() -> Router<ApiState> {
    // 仅 `/api/knowledge/:id/docs` 的 POST 需要放宽 body 限制（20MB + 开销），
    // 其他路由保持 axum 默认 2MB。此处把该路由隔离到子 Router 再 merge，
    // 避免把 20MB 放宽泄露到全量路由（旧 Wiki 无此层是 2MB bug 根源）。
    let docs_router = Router::new()
        .route("/api/knowledge/:id/docs", get(list_docs).post(upload_doc))
        .layer(DefaultBodyLimit::max(MULTIPART_BODY_LIMIT));
    Router::new()
        .route("/api/knowledge", get(list_sources).post(create_source))
        .route(
            "/api/knowledge/:id",
            get(get_source)
                .put(update_source)
                .patch(patch_source)
                .delete(delete_source),
        )
        .route(
            "/api/knowledge/:id/docs/:doc_id",
            get(get_doc).delete(delete_doc),
        )
        .route(
            "/api/knowledge/:id/docs/:doc_id/reindex",
            post(reindex_doc),
        )
        .route("/api/knowledge/test-embedding", post(test_embedding))
        .route("/api/knowledge/:id/query", post(query_knowledge))
        .route("/api/knowledge/:id/pages", get(list_pages))
        .route(
            "/api/knowledge/:id/pages/*ref",
            get(get_page).put(put_page).delete(delete_page),
        )
        .route("/api/knowledge/:id/graph", get(get_graph))
        .route("/api/knowledge/:id/search", get(search_knowledge))
        .route("/api/knowledge/search", get(search_all))
        .merge(docs_router)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::extract::DefaultBodyLimit;
    use axum::http::{header, Method, Request, StatusCode as HttpStatus};
    use axum::routing::{get, post};
    use axum::Router;
    use serde_json::{json, Value};
    use std::time::Duration;
    use tower::ServiceExt;

    use crate::auth::AuthConfig;
    use crate::control_plane::ServerState;
    use crate::db::Database;

    /// 构造 ApiState：内存 DB + 指定 RAG 数据目录 + 固定测试主密钥（字段加密可用），
    /// 并注入 Wiki 运行时（`KnowledgeRuntime` 的 `pages_sem` 复用同一池）。
    async fn test_api_state(rag_dir: &std::path::Path) -> ApiState {
        let db = Database::new(":memory:").await.expect("in-memory db");
        let mut server_state = ServerState::with_db(db);
        server_state
            .init_llm_state(
                server_state.db().cloned(),
                Some([42u8; 32]),
                rag_dir,
                std::sync::Arc::new(tokio::sync::RwLock::new(
                    crate::dynamic_config::DynamicConfig::default_for_llm(),
                )),
            )
            .await;
        // 注入 Wiki 运行时（与生产 `bin/server.rs` 同路径：复用同一 DB/LLM，pages_sem=2）
        {
            let llm = server_state
                .llm_state
                .read()
                .await
                .as_ref()
                .expect("llm state initialized")
                .clone();
            let db_clone = server_state.db().cloned().expect("db present");
            let wiki = crate::agent::wiki::WikiState::new(db_clone, (*llm).clone());
            let agent = server_state
                .agent_state
                .take()
                .expect("agent state")
                .with_wiki(wiki);
            server_state.agent_state = Some(agent);
        }
        // 注入 Memory 运行时（wiki 测试中的 settings 依赖）
        {
            let llm = server_state
                .llm_state
                .read()
                .await
                .as_ref()
                .expect("llm state initialized")
                .clone();
            let mem = crate::agent::memory::MemoryState::new(
                server_state.db().cloned().expect("db present"),
                llm.rag_store.clone(),
                llm.cipher.clone(),
                (*llm).clone(),
            );
            let agent = server_state.agent_state.take().expect("agent state").with_memory(mem);
            server_state.agent_state = Some(agent);
        }
        ApiState {
            server_state,
            auth_config: Arc::new(AuthConfig::new(None, None)),
            log_store: None,
        }
    }

    /// 覆盖本模块全部路由的测试 `Router`（免 JWT）。
    fn test_router(state: ApiState) -> Router {
        let public = Router::new().route("/api/knowledge/events", get(super::sse_knowledge_events));
        let protected = Router::new()
            .route("/api/knowledge", get(super::list_sources).post(super::create_source))
            .route(
                "/api/knowledge/:id",
                get(super::get_source)
                    .put(super::update_source)
                    .patch(super::patch_source)
                    .delete(super::delete_source),
            )
            .route(
                "/api/knowledge/:id/docs",
                get(super::list_docs).post(super::upload_doc),
            )
            .layer(DefaultBodyLimit::max(super::MULTIPART_BODY_LIMIT))
            .route(
                "/api/knowledge/:id/docs/:doc_id",
                get(super::get_doc).delete(super::delete_doc),
            )
            .route(
                "/api/knowledge/:id/docs/:doc_id/reindex",
                post(super::reindex_doc),
            )
            .route("/api/knowledge/test-embedding", post(super::test_embedding))
            .route("/api/knowledge/:id/query", post(super::query_knowledge))
            .route("/api/knowledge/:id/pages", get(super::list_pages))
            .route(
                "/api/knowledge/:id/pages/*ref",
                get(super::get_page).put(super::put_page).delete(super::delete_page),
            )
            .route("/api/knowledge/:id/graph", get(super::get_graph))
            .route("/api/knowledge/:id/search", get(super::search_knowledge))
            .route("/api/knowledge/search", get(super::search_all));
        public.merge(protected).with_state(state)
    }

    /// 仅用于 wiki SSE headers 测试的 Router（复用 knowledge 的 wiki 注入态）。
    fn wiki_test_router(state: ApiState) -> Router {
        super::protected_router()
            .merge(super::public_router())
            .merge(crate::mgmt::api::agent::memory::protected_router())
            .merge(crate::mgmt::api::agent::memory::public_router())
            .with_state(state)
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

    fn multipart_body(boundary: &str, filename: &str, content: &str) -> String {
        format!(
            "--{boundary}\r\n\
             Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n\
             Content-Type: text/plain\r\n\r\n\
             {content}\r\n\
             --{boundary}--\r\n"
        )
    }

    fn multipart_body_bytes(boundary: &str, filename: &str, content: &[u8]) -> Vec<u8> {
        let mut v = format!(
            "--{boundary}\r\n\
             Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n\
             Content-Type: text/plain\r\n\r\n"
        )
        .into_bytes();
        v.extend_from_slice(content);
        v.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        v
    }

    fn multipart_upload_request(
        kb_id: &str,
        boundary: &str,
        filename: &str,
        content: &[u8],
    ) -> Request<Body> {
        let body = multipart_body_bytes(boundary, filename, content);
        Request::builder()
            .method(Method::POST)
            .uri(format!("/api/knowledge/{kb_id}/docs"))
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body))
            .expect("build multipart request")
    }

    async fn mock_embedding_server(dim: usize) -> String {
        use axum::extract::Json as J;
        use serde_json::Value as V;
        let app = Router::new().route(
            "/embeddings",
            post(move |body: J<V>| async move {
                let n = body["input"].as_array().map_or(1, Vec::len);
                let data: Vec<_> = (0..n)
                    .map(|i| {
                        json!({
                            "index": i,
                            "embedding": vec![0.1f32; dim],
                            "object": "embedding"
                        })
                    })
                    .collect();
                J(json!({ "object": "list", "data": data }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("mock server");
        });
        format!("http://{addr}")
    }

    async fn create_kb(app: &Router, base: &str) -> String {
        let req = json_request(
            Method::POST,
            "/api/knowledge".to_string(),
            &json!({
                "name": "测试知识库",
                "description": "集成测试用",
                "index_vector": true,
                "emb_base_url": base,
                "emb_api_key": "sk-test",
                "emb_model": "test-model",
                "emb_dimension": 8,
                "top_k": 5,
            }),
        );
        let (status, body) = call(app, req).await;
        assert_eq!(status, HttpStatus::CREATED, "create kb: {body}");
        body["id"].as_str().expect("kb id").to_string()
    }

    async fn wait_doc_ready(app: &Router, kb_id: &str, doc_id: &str) -> i64 {
        for _ in 0..50 {
            let (status, body) = call(
                app,
                json_request(
                    Method::GET,
                    format!("/api/knowledge/{kb_id}/docs/{doc_id}"),
                    &json!(null),
                ),
            )
            .await;
            assert_eq!(status, HttpStatus::OK);
            // 统一视图：vector 侧状态在 body["vector"]["status"]
            let v_status = body.get("vector").and_then(|v| v.get("status")).and_then(|s| s.as_str()).unwrap_or("");
            if v_status == "ready" {
                return body
                    .get("vector")
                    .and_then(|v| v.get("chunk_count"))
                    .and_then(|c| c.as_i64())
                    .unwrap_or(0);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("doc {doc_id} did not become ready");
    }

    #[tokio::test]
    async fn test_embedding_returns_dimension() {
        let base = mock_embedding_server(8).await;
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;
        let app = test_router(state);

        let req = json_request(
            Method::POST,
            "/api/knowledge/test-embedding".to_string(),
            &json!({ "base_url": base, "api_key": "sk-test", "model": "test-model" }),
        );
        let (status, body) = call(&app, req).await;
        assert_eq!(status, HttpStatus::OK, "test-embedding: {body}");
        assert_eq!(body["dimension"].as_i64(), Some(8));
        assert!(body["latency_ms"].is_u64());
    }

    #[tokio::test]
    async fn test_embedding_fails_on_unreachable_service() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;
        let app = test_router(state);

        let req = json_request(
            Method::POST,
            "/api/knowledge/test-embedding".to_string(),
            &json!({ "base_url": "http://127.0.0.1:1", "api_key": "k", "model": "m" }),
        );
        let (status, _body) = call(&app, req).await;
        assert_eq!(status, HttpStatus::BAD_GATEWAY);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn kb_crud_flow_with_docs_and_query() {
        let base = mock_embedding_server(8).await;
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;

        let db = state.server_state.db().unwrap().clone();
        let tx = {
            let guard = state.server_state.llm_state.read().await;
            guard.as_ref().unwrap().rag_tx.clone()
        };
        let mut rx = tx.subscribe();
        let app = test_router(state);

        let kb_id = create_kb(&app, &base).await;
        assert!(!kb_id.is_empty());

        let stored = db.ks_get(&kb_id).await.unwrap().unwrap();
        assert!(
            stored.emb_api_key.starts_with("enc:v1:"),
            "emb_api_key should be encrypted, got: {}",
            stored.emb_api_key
        );

        let (status, body) = call(
            &app,
            json_request(Method::GET, "/api/knowledge".to_string(), &json!(null)),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        let kbs = body["sources"].as_array().unwrap();
        assert!(kbs.iter().any(|k| k["id"] == json!(kb_id)));

        let (status, body) = call(
            &app,
            json_request(Method::GET, format!("/api/knowledge/{kb_id}"), &json!(null)),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        assert_eq!(body["name"], json!("测试知识库"));
        assert_eq!(body["emb_api_key"], json!(""), "api key must not be echoed");

        let (status, body) = call(
            &app,
            json_request(
                Method::PUT,
                format!("/api/knowledge/{kb_id}"),
                &json!({ "name": "改名", "description": "d2", "top_k": 8, "chunk_size": 256, "chunk_overlap": 32, "score_threshold": 0.5 }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK, "update kb: {body}");
        let updated = db.ks_get(&kb_id).await.unwrap().unwrap();
        assert_eq!(updated.top_k, 8);
        assert_eq!(
            updated.emb_base_url, stored.emb_base_url,
            "PUT 不带 emb 字段时配置不变"
        );

        let (status, _body) = call(
            &app,
            json_request(
                Method::PATCH,
                format!("/api/knowledge/{kb_id}"),
                &json!({ "enabled": false }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        assert_eq!(db.ks_get(&kb_id).await.unwrap().unwrap().enabled, 0);
        let (status, _body) = call(
            &app,
            json_request(
                Method::PATCH,
                format!("/api/knowledge/{kb_id}"),
                &json!({ "enabled": true }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);

        let content =
            "# 使用指南\n\n## 安装\n\n运行 rust-tunnel-server。\n\n## 配置\n\n编辑 config.toml。\n"
                .to_string();
        let boundary = "test-boundary-42";
        let upload_body = multipart_body(boundary, "guide.md", &content);
        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/api/knowledge/{kb_id}/docs"))
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(upload_body))
            .expect("build multipart request");
        let (status, body) = call(&app, req).await;
        assert_eq!(status, HttpStatus::CREATED, "upload doc: {body}");
        let doc_status = body["vector"]["status"].as_str().expect("doc status");
        assert!(
            doc_status == "pending" || doc_status == "processing",
            "doc status should be pending or processing, got {doc_status}"
        );
        assert_eq!(body["filename"], json!("guide.md"));
        assert!(body["content_hash"]
            .as_str()
            .unwrap()
            .starts_with("sha256:"));
        let doc_id = body["id"].as_str().expect("doc id").to_string();

        let source_path = dir
            .path()
            .join("knowledge_docs")
            .join(&kb_id)
            .join(format!("{doc_id}.md"));
        assert!(
            source_path.exists(),
            "original doc file should be persisted on upload"
        );

        let ev1 = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("first event timeout")
            .expect("channel closed");
        assert_eq!(ev1.status, "processing");
        assert_eq!(ev1.doc_id, doc_id);

        let mut doc_ready = false;
        for _ in 0..50 {
            let (status, body) = call(
                &app,
                json_request(
                    Method::GET,
                    format!("/api/knowledge/{kb_id}/docs/{doc_id}"),
                    &json!(null),
                ),
            )
            .await;
            assert_eq!(status, HttpStatus::OK);
            if body["vector"]["status"] == json!("ready") {
                doc_ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(doc_ready, "doc should become ready");
        let ev2 = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("ready event timeout")
            .expect("channel closed");
        assert_eq!(ev2.status, "ready");
        assert!(ev2.error.is_none());

        let (status, body) = call(
            &app,
            json_request(
                Method::POST,
                format!("/api/knowledge/{kb_id}/query"),
                &json!({ "text": "怎么安装?" }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK, "query: {body}");
        let chunks = body["chunks"].as_array().unwrap();
        assert!(!chunks.is_empty(), "query should hit ingested chunks");
        assert!(chunks[0]["score"].as_f64().unwrap() >= 0.3);

        let (status, body) = call(
            &app,
            json_request(
                Method::GET,
                format!("/api/knowledge/{kb_id}/docs"),
                &json!(null),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        assert_eq!(body["documents"].as_array().unwrap().len(), 1);

        let (status, _body) = call(
            &app,
            json_request(
                Method::DELETE,
                format!("/api/knowledge/{kb_id}/docs/{doc_id}"),
                &json!(null),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        let (status, _body) = call(
            &app,
            json_request(
                Method::GET,
                format!("/api/knowledge/{kb_id}/docs/{doc_id}"),
                &json!(null),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::NOT_FOUND);
        assert!(
            !source_path.exists(),
            "doc source file should be removed on delete"
        );

        let (status, _body) = call(
            &app,
            json_request(Method::DELETE, format!("/api/knowledge/{kb_id}"), &json!(null)),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        let (status, _body) = call(
            &app,
            json_request(Method::GET, format!("/api/knowledge/{kb_id}"), &json!(null)),
        )
        .await;
        assert_eq!(status, HttpStatus::NOT_FOUND);
        assert!(
            !dir.path().join("rag").join(&kb_id).exists(),
            "kb shard dir should be removed"
        );
        assert!(
            !dir.path().join("knowledge_docs").join(&kb_id).exists(),
            "kb doc source dir should be removed"
        );
        assert!(db.ks_get(&kb_id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn upload_rejects_bad_extension_and_oversize() {
        let base = mock_embedding_server(8).await;
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;
        let app = test_router(state);
        let kb_id = create_kb(&app, &base).await;

        let boundary = "b-bad";
        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/api/knowledge/{kb_id}/docs"))
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(multipart_body(boundary, "evil.exe", "x")))
            .expect("build request");
        let (status, _body) = call(&app, req).await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);

        let big = "x".repeat(2 * 1024 * 1024 + 1);
        let boundary = "b-big";
        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/api/knowledge/{kb_id}/docs"))
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(multipart_body(boundary, "big.md", &big)))
            .expect("build request");
        let (status, body_text) = call_raw(&app, req).await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);
        assert!(
            body_text.contains("file too large"),
            "oversize should return custom message, got: {body_text}"
        );

        let boundary = "b-nofile";
        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/api/knowledge/{kb_id}/docs"))
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"note\"\r\n\r\nhi\r\n--{boundary}--\r\n"
            )))
            .expect("build request");
        let (status, _body) = call(&app, req).await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);
    }

    #[tokio::test]
    async fn create_kb_validates_input() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;
        let app = test_router(state);

        let (status, _body) = call(
            &app,
            json_request(
                Method::POST,
                "/api/knowledge".to_string(),
                &json!({ "name": "", "index_vector": true, "emb_base_url": "http://x", "emb_model": "m", "emb_dimension": 8 }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);

        let (status, body_text) = call_raw(
            &app,
            json_request(
                Method::POST,
                "/api/knowledge".to_string(),
                &json!({ "name": "n", "index_vector": true, "emb_base_url": "http://x", "emb_model": "m" }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);
        assert!(
            body_text.contains("emb_dimension"),
            "缺 dimension 应提示 emb_dimension, got: {body_text}"
        );

        let (status, body_text) = call_raw(
            &app,
            json_request(
                Method::POST,
                "/api/knowledge".to_string(),
                &json!({ "name": "n", "index_vector": true }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);
        assert!(
            body_text.contains("embedding is not configured"),
            "缺 embedding 且全局未配置应 400, got: {body_text}"
        );

        let (status, _body) = call(
            &app,
            json_request(
                Method::POST,
                "/api/knowledge".to_string(),
                &json!({ "name": "n", "index_vector": true, "emb_base_url": "http://x", "emb_model": "m", "emb_dimension": 8, "chunk_size": 128, "chunk_overlap": 200 }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);

        let boundary = "b-404";
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/knowledge/no-such-kb/docs")
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(multipart_body(boundary, "a.md", "hi")))
            .expect("build request");
        let (status, _body) = call(&app, req).await;
        assert_eq!(status, HttpStatus::NOT_FOUND);
    }

    #[tokio::test]
    async fn create_kb_falls_back_to_global_embedding() {
        let base = mock_embedding_server(8).await;
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;
        let db = state.server_state.db().unwrap().clone();
        let app = test_router(state);

        let mut s = crate::db::memory::AgentMemorySettingsRecord::default_disabled();
        s.enabled = 1;
        s.emb_base_url = base.clone();
        s.emb_api_key = "sk-global".to_string();
        s.emb_model = "global-model".to_string();
        s.emb_dimension = 8;
        db.memory_upsert_settings(&s).await.unwrap();

        let (status, body) = call(
            &app,
            json_request(
                Method::POST,
                "/api/knowledge".to_string(),
                &json!({ "name": "全局库", "index_vector": true, "top_k": 5 }),
            ),
        )
        .await;
        assert_eq!(
            status,
            HttpStatus::CREATED,
            "create with global embedding: {body}"
        );
        let kb_id = body["id"].as_str().expect("kb id").to_string();

        let stored = db.ks_get(&kb_id).await.unwrap().unwrap();
        assert_eq!(stored.emb_base_url, base);
        assert_eq!(stored.emb_model, "global-model");
        assert_eq!(stored.emb_dimension, 8);
        assert!(
            stored.emb_api_key.starts_with("enc:v1:"),
            "global api key should be encrypted, got: {}",
            stored.emb_api_key
        );
    }

    #[tokio::test]
    async fn upload_rejects_non_utf8_file() {
        let base = mock_embedding_server(8).await;
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;
        let app = test_router(state);
        let kb_id = create_kb(&app, &base).await;

        let boundary = "b-utf8";
        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/api/knowledge/{kb_id}/docs"))
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(multipart_body_bytes(
                boundary,
                "bad.md",
                &[0xffu8, 0xfe, 0xfd],
            )))
            .expect("build request");
        let (status, body_text) = call_raw(&app, req).await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);
        assert!(
            body_text.contains("UTF-8"),
            "non-utf8 should return UTF-8 message, got: {body_text}"
        );
    }

    #[tokio::test]
    async fn upload_rejects_legacy_image_and_mismatched_binary() {
        let base = mock_embedding_server(8).await;
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;
        let app = test_router(state);
        let kb_id = create_kb(&app, &base).await;

        let (status, body_text) = call_raw(
            &app,
            multipart_upload_request(&kb_id, "b-legacy", "legacy.doc", b"x"),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);
        assert!(
            body_text.contains("legacy Office format '.doc' is not supported"),
            "got: {body_text}"
        );

        let (status, body_text) = call_raw(
            &app,
            multipart_upload_request(&kb_id, "b-img", "photo.png", b"x"),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);
        assert!(
            body_text.contains("image files are not supported"),
            "got: {body_text}"
        );

        let (status, body_text) = call_raw(
            &app,
            multipart_upload_request(&kb_id, "b-pdf", "fake.pdf", b"not a pdf at all"),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);
        assert!(body_text.contains("not a PDF file"), "got: {body_text}");

        let (status, body_text) = call_raw(
            &app,
            multipart_upload_request(&kb_id, "b-noext", "README", b"x"),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);
        assert!(
            body_text.contains("unsupported file type"),
            "got: {body_text}"
        );
    }

    #[tokio::test]
    async fn reindex_doc_rebuilds_index() {
        let base = mock_embedding_server(8).await;
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;
        let db = state.server_state.db().unwrap().clone();
        let app = test_router(state);
        let kb_id = create_kb(&app, &base).await;

        let content =
            "# 使用指南\n\n## 安装\n\n运行 rust-tunnel-server。\n\n## 配置\n\n编辑 config.toml。\n"
                .to_string();
        let boundary = "b-reindex";
        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/api/knowledge/{kb_id}/docs"))
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(multipart_body(boundary, "guide.md", &content)))
            .expect("build request");
        let (status, body) = call(&app, req).await;
        assert_eq!(status, HttpStatus::CREATED, "upload: {body}");
        let doc_id = body["id"].as_str().expect("doc id").to_string();
        let source_path = dir
            .path()
            .join("knowledge_docs")
            .join(&kb_id)
            .join(format!("{doc_id}.md"));
        assert!(
            source_path.exists(),
            "original doc file should be persisted"
        );
        assert_eq!(
            std::fs::read_to_string(&source_path).unwrap(),
            content,
            "persisted source should match upload"
        );

        let first = wait_doc_ready(&app, &kb_id, &doc_id).await;
        assert!(first > 0);
        assert_eq!(db.rag_count_kb_chunks(&kb_id).await.unwrap(), first);

        let (status, body) = call(
            &app,
            json_request(
                Method::POST,
                format!("/api/knowledge/{kb_id}/docs/{doc_id}/reindex"),
                &json!(null),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK, "reindex: {body}");
        let st = body["vector"]["status"].as_str().unwrap_or("");
        assert!(
            st == "pending" || st == "processing",
            "reindex immediate status should be pending/processing, got {st}"
        );

        let second = wait_doc_ready(&app, &kb_id, &doc_id).await;
        assert!(second > 0);
        assert_eq!(
            second, first,
            "reindex should rebuild the same number of chunks"
        );
        assert_eq!(
            db.rag_count_kb_chunks(&kb_id).await.unwrap(),
            second,
            "old chunks must be removed before re-ingest"
        );

        let (status, body) = call(
            &app,
            json_request(
                Method::POST,
                format!("/api/knowledge/{kb_id}/query"),
                &json!({ "text": "怎么安装?" }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK, "query after reindex: {body}");
        assert!(
            !body["chunks"].as_array().unwrap().is_empty(),
            "query should hit rebuilt chunks"
        );
    }

    #[tokio::test]
    async fn create_and_update_enforce_top_k_cap() {
        let base = mock_embedding_server(8).await;
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;
        let app = test_router(state);

        let req = json_request(
            Method::POST,
            "/api/knowledge".to_string(),
            &json!({
                "name": "边界库",
                "index_vector": true,
                "emb_base_url": base,
                "emb_model": "m",
                "emb_dimension": 8,
                "top_k": 20,
            }),
        );
        let (status, body) = call(&app, req).await;
        assert_eq!(status, HttpStatus::CREATED, "top_k=20 应允许: {body}");
        let kb_id = body["id"].as_str().expect("kb id").to_string();

        let req = json_request(
            Method::POST,
            "/api/knowledge".to_string(),
            &json!({
                "name": "超限库",
                "index_vector": true,
                "emb_base_url": base,
                "emb_model": "m",
                "emb_dimension": 8,
                "top_k": 21,
            }),
        );
        let (status, body_text) = call_raw(&app, req).await;
        assert_eq!(status, HttpStatus::BAD_REQUEST, "top_k=21 应 400");
        assert!(
            body_text.contains("top_k must be 1-20"),
            "错误消息应提示 1-20, got: {body_text}"
        );

        let (status, body_text) = call_raw(
            &app,
            json_request(
                Method::PUT,
                format!("/api/knowledge/{kb_id}"),
                &json!({ "name": "改名", "top_k": 21, "chunk_size": 512, "chunk_overlap": 64 }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST, "update top_k=21 应 400");
        assert!(body_text.contains("top_k must be 1-20"));
    }

    #[tokio::test]
    async fn patch_kb_rejects_non_boolean_enabled() {
        let base = mock_embedding_server(8).await;
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;
        let db = state.server_state.db().unwrap().clone();
        let app = test_router(state);
        let kb_id = create_kb(&app, &base).await;

        let (status, _body) = call(
            &app,
            json_request(
                Method::PATCH,
                format!("/api/knowledge/{kb_id}"),
                &json!({ "enabled": "yes" }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);
        assert_eq!(db.ks_get(&kb_id).await.unwrap().unwrap().enabled, 1);
    }

    #[tokio::test]
    async fn patch_kb_rejects_missing_enabled_and_preserves_state() {
        let base = mock_embedding_server(8).await;
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;
        let db = state.server_state.db().unwrap().clone();
        let app = test_router(state);
        let kb_id = create_kb(&app, &base).await;
        assert_eq!(db.ks_get(&kb_id).await.unwrap().unwrap().enabled, 1);

        let (status, _body) = call(
            &app,
            json_request(Method::PATCH, format!("/api/knowledge/{kb_id}"), &json!({})),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);
        assert_eq!(
            db.ks_get(&kb_id).await.unwrap().unwrap().enabled,
            1,
            "空 PATCH 不得把 KB 静默禁用"
        );

        let (status, _body) = call(
            &app,
            json_request(
                Method::PATCH,
                format!("/api/knowledge/{kb_id}"),
                &json!({ "enabled": 1 }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);
        assert_eq!(db.ks_get(&kb_id).await.unwrap().unwrap().enabled, 1);
    }

    #[tokio::test]
    async fn upload_to_disabled_kb_rejected() {
        let base = mock_embedding_server(8).await;
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;
        let db = state.server_state.db().unwrap().clone();
        let app = test_router(state);
        let kb_id = create_kb(&app, &base).await;

        db.ks_set_enabled(&kb_id, false).await.unwrap();

        let boundary = "b-disabled";
        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/api/knowledge/{kb_id}/docs"))
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(multipart_body(boundary, "a.md", "hi")))
            .expect("build request");
        let (status, body_text) = call_raw(&app, req).await;
        assert_eq!(status, HttpStatus::CONFLICT);
        assert!(
            body_text.contains("knowledge base is disabled"),
            "禁用 KB 上传应提示 disabled, got: {body_text}"
        );
        assert!(db.kdoc_list(&kb_id).await.unwrap().is_empty());
        assert!(!dir.path().join("knowledge_docs").join(&kb_id).exists());
    }

    #[tokio::test]
    async fn reindex_rejected_while_doc_processing() {
        let base = mock_embedding_server(8).await;
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;
        let db = state.server_state.db().unwrap().clone();
        let app = test_router(state);
        let kb_id = create_kb(&app, &base).await;

        let doc_id = uuid::Uuid::new_v4().to_string();
        db.kdoc_create(&doc_id, &kb_id, "busy.md", "md", "sha256:x")
            .await
            .unwrap();
        db.kdoc_update_index_status(&doc_id, rust_tunnel_persistence::knowledge::IndexKind::Vector, "processing", 0, None)
            .await
            .unwrap();
        let source_path = dir
            .path()
            .join("knowledge_docs")
            .join(&kb_id)
            .join(format!("{doc_id}.md"));
        tokio::fs::create_dir_all(source_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&source_path, "# busy").await.unwrap();

        let (status, body_text) = call_raw(
            &app,
            json_request(
                Method::POST,
                format!("/api/knowledge/{kb_id}/docs/{doc_id}/reindex"),
                &json!(null),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::CONFLICT);
        assert!(
            body_text.contains("document is being processed, retry later"),
            "processing 中 reindex 应 409, got: {body_text}"
        );
        db.kdoc_update_index_status(&doc_id, rust_tunnel_persistence::knowledge::IndexKind::Vector, "pending", 0, None)
            .await
            .unwrap();
        let (status, _body) = call(
            &app,
            json_request(
                Method::POST,
                format!("/api/knowledge/{kb_id}/docs/{doc_id}/reindex"),
                &json!(null),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::CONFLICT);
    }

    async fn upload_guide_and_wait(app: &Router, kb_id: &str) -> (String, i64) {
        let content =
            "# 使用指南\n\n## 安装\n\n运行 rust-tunnel-server。\n\n## 配置\n\n编辑 config.toml。\n";
        let boundary = "b-emb";
        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/api/knowledge/{kb_id}/docs"))
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(multipart_body(boundary, "guide.md", content)))
            .expect("build multipart request");
        let (status, body) = call(app, req).await;
        assert_eq!(status, HttpStatus::CREATED, "upload: {body}");
        let doc_id = body["id"].as_str().expect("doc id").to_string();
        let chunks = wait_doc_ready(app, kb_id, &doc_id).await;
        assert!(chunks > 0, "doc should ingest to >0 chunks");
        (doc_id, chunks)
    }

    #[tokio::test]
    async fn update_kb_emb_change_triggers_full_rebuild() {
        let base8 = mock_embedding_server(8).await;
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;
        let db = state.server_state.db().unwrap().clone();
        let app = test_router(state);
        let kb_id = create_kb(&app, &base8).await;
        let (doc_id, first) = upload_guide_and_wait(&app, &kb_id).await;
        let before = db.ks_get(&kb_id).await.unwrap().unwrap();

        let base16 = mock_embedding_server(16).await;
        let (status, body) = call(
            &app,
            json_request(
                Method::PUT,
                format!("/api/knowledge/{kb_id}"),
                &json!({
                    "name": "测试知识库",
                    "emb_base_url": base16,
                    "emb_model": "new-model",
                    "emb_dimension": 16,
                    "emb_api_key": "",
                }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK, "update emb: {body}");
        assert_eq!(body["reindexed"].as_i64(), Some(1), "应重建 1 篇文档");
        assert_eq!(body["missing_source"].as_i64(), Some(0));

        let after = db.ks_get(&kb_id).await.unwrap().unwrap();
        assert_eq!(after.emb_base_url, base16);
        assert_eq!(after.emb_model, "new-model");
        assert_eq!(after.emb_dimension, 16);
        assert_eq!(
            after.emb_api_key, before.emb_api_key,
            "api_key 留空应保留旧密文"
        );
        assert_eq!(after.enabled, 1, "重建完成后应恢复启用");

        let second = wait_doc_ready(&app, &kb_id, &doc_id).await;
        assert_eq!(second, first, "重建后 chunk 数应一致");

        let (status, body) = call(
            &app,
            json_request(
                Method::POST,
                format!("/api/knowledge/{kb_id}/query"),
                &json!({ "text": "怎么安装?" }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK, "query after rebuild: {body}");
        assert!(
            !body["chunks"].as_array().unwrap().is_empty(),
            "rebuilt index should be searchable"
        );
    }

    #[tokio::test]
    async fn update_kb_api_key_only_does_not_rebuild() {
        let base = mock_embedding_server(8).await;
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;
        let db = state.server_state.db().unwrap().clone();
        let app = test_router(state);
        let kb_id = create_kb(&app, &base).await;
        let (doc_id, chunks) = upload_guide_and_wait(&app, &kb_id).await;
        let before = db.ks_get(&kb_id).await.unwrap().unwrap();

        let (status, body) = call(
            &app,
            json_request(
                Method::PUT,
                format!("/api/knowledge/{kb_id}"),
                &json!({ "name": "测试知识库", "emb_api_key": "sk-rotated" }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK, "rotate key: {body}");
        assert!(
            body.get("reindexed").is_none(),
            "仅换密钥不应触发重建: {body}"
        );

        let after = db.ks_get(&kb_id).await.unwrap().unwrap();
        assert_ne!(after.emb_api_key, before.emb_api_key, "密钥密文应已替换");
        assert_eq!(after.emb_base_url, before.emb_base_url);
        assert_eq!(after.emb_model, before.emb_model);
        assert_eq!(after.emb_dimension, before.emb_dimension);

        let idx = db
            .kdoc_get_index(&doc_id, rust_tunnel_persistence::knowledge::IndexKind::Vector)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(idx.status, "ready");
        assert_eq!(db.rag_count_kb_chunks(&kb_id).await.unwrap(), chunks);
    }

    #[tokio::test]
    async fn update_kb_validates_emb_fields() {
        let base = mock_embedding_server(8).await;
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;
        let app = test_router(state);
        let kb_id = create_kb(&app, &base).await;

        let (status, body_text) = call_raw(
            &app,
            json_request(
                Method::PUT,
                format!("/api/knowledge/{kb_id}"),
                &json!({ "name": "n", "emb_base_url": "" }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);
        assert!(body_text.contains("emb_base_url"), "got: {body_text}");

        let (status, body_text) = call_raw(
            &app,
            json_request(
                Method::PUT,
                format!("/api/knowledge/{kb_id}"),
                &json!({ "name": "n", "emb_dimension": 0 }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::BAD_REQUEST);
        assert!(body_text.contains("emb_dimension"), "got: {body_text}");
    }

    #[tokio::test]
    async fn test_embedding_with_kb_id_uses_stored_key() {
        let base = mock_embedding_server(8).await;
        let dir = tempfile::tempdir().unwrap();
        let state = test_api_state(dir.path()).await;
        let app = test_router(state);
        let kb_id = create_kb(&app, &base).await;

        let (status, body) = call(
            &app,
            json_request(
                Method::POST,
                "/api/knowledge/test-embedding".to_string(),
                &json!({ "base_url": base, "api_key": "", "model": "test-model", "kb_id": kb_id }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK, "test with kb_id: {body}");
        assert_eq!(body["dimension"].as_i64(), Some(8));

        let (status, _body) = call(
            &app,
            json_request(
                Method::POST,
                "/api/knowledge/test-embedding".to_string(),
                &json!({ "base_url": base, "api_key": "", "model": "m", "kb_id": "no-such-kb" }),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::NOT_FOUND);
    }

    // ── Wiki 迁移测试（原 agent/wiki.rs） ──────────────────────────

    #[tokio::test]
    async fn wiki_container_crud_and_manual_page_and_search_and_settings() {
        let dir = tempfile::tempdir().unwrap();
        let app = wiki_test_router(test_api_state(dir.path()).await);

        let (status, body) = call(
            &app,
            json_request(Method::POST, "/api/knowledge".to_string(), &json!({"name": "my-wiki","summary":"desc","scope_type":"workspace","client_id":"c1","workspace_id":"w1","index_pages": true})),
        )
        .await;
        assert_eq!(status, HttpStatus::CREATED, "create wiki: {body}");
        let wiki_id = body["id"].as_str().unwrap().to_string();

        let (status, body) = call(
            &app,
            json_request(
                Method::GET,
                "/api/knowledge?scope=workspace&workspace_id=w1&index_kind=pages".to_string(),
                &json!(null),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        // 统一 list 键名为 sources
        assert_eq!(body["sources"].as_array().unwrap().len(), 1);
        assert_eq!(body["total"], json!(1));

        let (status, _) = call(
            &app,
            json_request(
                Method::GET,
                format!("/api/knowledge/{wiki_id}"),
                &json!(null),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);

        let (status, body) = call(
            &app,
            json_request(
                Method::PUT,
                format!("/api/knowledge/{wiki_id}/pages/deploy/prod"),
                &json!({"title":"部署","summary":"摘要","content":"内容 [[other/page]]"}),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK, "put page: {body}");
        assert_eq!(body["ref"], json!("deploy/prod"));

        let (status, body) = call(
            &app,
            json_request(
                Method::GET,
                format!("/api/knowledge/{wiki_id}/pages/deploy/prod"),
                &json!(null),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        assert_eq!(body["content"], json!("内容 [[other/page]]"));

        let (status, body) = call(
            &app,
            json_request(
                Method::GET,
                format!("/api/knowledge/{wiki_id}/pages"),
                &json!(null),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        assert_eq!(body["pages"].as_array().unwrap().len(), 1);

        let (status, body) = call(
            &app,
            json_request(
                Method::GET,
                format!("/api/knowledge/{wiki_id}/graph"),
                &json!(null),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        assert_eq!(body["nodes"].as_array().unwrap().len(), 1);

        let req = Request::builder()
            .method(Method::GET)
            .uri(format!("/api/knowledge/{wiki_id}/search?q=部署"))
            .body(Body::empty())
            .unwrap();
        let (status, body) = call(&app, req).await;
        assert_eq!(status, HttpStatus::OK, "search: {body}");
        assert!(body["hits"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["ref"] == json!("deploy/prod")));

        let (status, _) = call(
            &app,
            json_request(
                Method::DELETE,
                format!("/api/knowledge/{wiki_id}/pages/deploy/prod"),
                &json!(null),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);

        let (status, body) = call(
            &app,
            json_request(
                Method::GET,
                "/api/agent/memory/settings".to_string(),
                &json!(null),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        assert_eq!(body["wiki_enabled"], json!(true));
        assert_eq!(body["wiki_list_max"], json!(20));

        let (status, body) = call(
            &app,
            json_request(
                Method::PUT,
                "/api/agent/memory/settings".to_string(),
                &json!({"wiki_enabled": false, "wiki_list_max": 8}),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK, "put settings: {body}");
        assert_eq!(body["wiki_enabled"], json!(false));
        assert_eq!(body["wiki_list_max"], json!(8));

        let (status, body) = call(
            &app,
            json_request(
                Method::PUT,
                "/api/agent/memory/settings".to_string(),
                &json!({"top_k": 16}),
            ),
        )
        .await;
        assert_eq!(status, HttpStatus::OK);
        assert_eq!(
            body["wiki_enabled"],
            json!(false),
            "wiki_enabled 不应被重置"
        );
        assert_eq!(body["wiki_list_max"], json!(8), "wiki_list_max 不应被重置");
    }

    #[tokio::test]
    async fn wiki_sse_events_stream_headers() {
        let dir = tempfile::tempdir().unwrap();
        let app = wiki_test_router(test_api_state(dir.path()).await);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/knowledge/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), HttpStatus::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/event-stream"
        );
    }
}
