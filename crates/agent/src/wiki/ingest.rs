//! Wiki 页面抽取器：LLM 结构化抽取能力的唯一承载。
//!
//! 不再自建摄入流水线（提取→分块→批次→重试→落库→事件），这些策略已收敛至
//! `rust_tunnel_rag::ingest` 的统一双索引流水线；本模块只提供 prompt、LLM
//! 调用与 JSON 解析，经 [`LlmPageExtractor`] 注入流水线。

use crate::db::Database;
use crate::llm::LlmState;

/// Wiki 抽取 prompt：要求严格 JSON array。
const WIKI_EXTRACT_PROMPT: &str = r#"你是文档结构化抽取器。把以下 Markdown 切片提炼为结构化 Wiki 页面。

输出**严格 JSON 数组**，不要代码围栏、不要多余文本：
[{"ref":"路径式小写ASCII，如 deploy/prod-checklist","title":"标题≤64字符","summary":"摘要≤200字符","content":"Markdown 正文≤4000字符，保留 [[ref]] 内联链接","links":["ref"]}]

约束：
- ref 必须匹配 ^[a-z0-9][a-z0-9/_-]{0,127}$，小写 ASCII 路径式，禁 //、./、../
- title ≤64 字符，summary ≤200 字符，content ≤4000 字符且为 Markdown，内联链接用 [[ref]] 形式
- links 为本页正文中出现的 [[ref]] 列表（可空），仅作参考，以正文实际 [[ref]] 为准
- 若切片无可提炼内容，返回 []
- 只输出 JSON 数组，不要解释"#;

/// pages 索引的页面抽取器：把一批 Markdown 文本经 LLM 提炼为结构化页面。
///
/// 批次切分、失败重试、落库截断都由 `rust_tunnel_rag::ingest` 的统一流水线负责，
/// 本类型只关心 prompt、LLM 调用与 JSON 解析。
pub struct LlmPageExtractor {
    db: Database,
    llm: LlmState,
}

impl LlmPageExtractor {
    /// 同步构造。`distill_model` 在每次抽取时读取（本地 SQLite，成本可忽略），
    /// 未配置时由 `extract_pages` 返回 Err 走流水线统一的失败路径。
    #[must_use]
    pub fn new(db: Database, llm: LlmState) -> Self {
        Self { db, llm }
    }
}

#[async_trait::async_trait]
impl rust_tunnel_rag::ingest::PageExtractor for LlmPageExtractor {
    async fn extract_pages(
        &self,
        batch_text: &str,
    ) -> Result<Vec<rust_tunnel_rag::ingest::ExtractedPage>, String> {
        let settings = self
            .db
            .memory_get_settings()
            .await
            .map_err(|e| e.to_string())?;
        let model = settings.distill_model.trim().to_string();
        if model.is_empty() {
            return Err("LLM 未配置：请在 设置 → AI 记忆体 中配置 distill_model".to_string());
        }
        let raw = call_wiki_llm(&self.llm, &model, batch_text).await?;
        let pages = parse_extract_value(&raw)?;
        Ok(pages)
    }
}

async fn call_wiki_llm(llm: &LlmState, model: &str, batch_text: &str) -> Result<String, String> {
    let chain = crate::llm::router::resolve_with_failover(llm, model)
        .await
        .map_err(|e| format!("model resolution failed: {e}"))?;
    let request = crate::llm::ChatCompletionRequest {
        model: model.to_string(),
        messages: vec![
            crate::llm::ChatMessage::text("system", WIKI_EXTRACT_PROMPT),
            crate::llm::ChatMessage::text("user", batch_text),
        ],
        stream: false,
        max_tokens: None,
        temperature: None,
        top_p: None,
        tools: None,
        tool_choice: None,
        raw_body: None,
    };
    let req_body = crate::llm::upstream::build_upstream_body(&request);
    let outcome = crate::llm::upstream::execute_with_failover(
        &llm.upstream_client,
        &llm.breakers,
        &llm.known_failures,
        &chain,
        &req_body,
        false,
        None,
    )
    .await;
    let resp = match outcome {
        crate::llm::upstream::FailoverOutcome::Success { resp, .. } => resp,
        crate::llm::upstream::FailoverOutcome::Exhausted { message, .. } => {
            return Err(format!("wiki LLM unavailable: {message}"));
        }
    };
    let body_bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .map_err(|e| format!("failed to read wiki LLM response: {e}"))?;
    let body: serde_json::Value = serde_json::from_slice(&body_bytes)
        .map_err(|e| format!("invalid wiki LLM response JSON: {e}"))?;
    let raw = body
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    Ok(raw.to_string())
}

fn parse_extract_value(raw: &str) -> Result<Vec<rust_tunnel_rag::ingest::ExtractedPage>, String> {
    let cleaned = strip_code_fence(raw);
    let json_str = extract_json_array(&cleaned);
    let value: serde_json::Value =
        serde_json::from_str(&json_str).map_err(|e| format!("invalid wiki JSON: {e}"))?;
    let arr = value
        .as_array()
        .ok_or_else(|| "wiki LLM output must be JSON array".to_string())?;
    let mut pages = Vec::new();
    for item in arr {
        let obj = item
            .as_object()
            .ok_or_else(|| "wiki page must be object".to_string())?;
        let raw_ref = obj.get("ref").and_then(|v| v.as_str()).unwrap_or("").trim();
        if raw_ref.is_empty() {
            continue;
        }
        let Some(norm) = crate::db::wiki::normalize_wiki_ref(raw_ref) else {
            continue;
        };
        let title = obj
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let summary = obj
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let content = obj
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if content.is_empty() {
            continue;
        }
        pages.push(rust_tunnel_rag::ingest::ExtractedPage {
            page_ref: norm,
            title,
            summary,
            content,
        });
    }
    Ok(pages)
}

fn strip_code_fence(raw: &str) -> String {
    let s = raw.trim();
    if !s.starts_with("```") {
        return s.to_string();
    }
    let mut body = &s[3..];
    if let Some(nl) = body.find('\n') {
        body = &body[nl + 1..];
    }
    let end = body.find("```").unwrap_or(body.len());
    body[..end].trim().to_string()
}

fn extract_json_array(text: &str) -> String {
    match (text.find('['), text.rfind(']')) {
        (Some(start), Some(end)) if end > start => text[start..=end].to_string(),
        _ => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_code_fence_handles_json_fence() {
        let raw =
            "```json\n[{\"ref\":\"a\",\"title\":\"t\",\"summary\":\"s\",\"content\":\"c\"}]\n```";
        assert_eq!(
            strip_code_fence(raw),
            "[{\"ref\":\"a\",\"title\":\"t\",\"summary\":\"s\",\"content\":\"c\"}]"
        );
    }

    #[test]
    fn extract_json_array_from_wrapped_text() {
        let raw = "结果：\n[{\"ref\":\"a\"}]\n结束";
        let arr = extract_json_array(raw);
        assert_eq!(arr, "[{\"ref\":\"a\"}]");
    }

    #[test]
    fn parse_extract_value_valid() {
        let raw = r#"[{"ref":"deploy/prod","title":"部署","summary":"摘要","content":"内容 [[other/ref]]","links":["other/ref"]}]"#;
        let pages = parse_extract_value(raw).unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].page_ref, "deploy/prod");
        assert_eq!(pages[0].title, "部署");
    }

    #[test]
    fn parse_extract_value_empty_array() {
        let pages = parse_extract_value("[]").unwrap();
        assert!(pages.is_empty());
    }

    #[test]
    fn parse_extract_value_skips_invalid_ref() {
        let raw = r#"[{"ref":"BAD REF","title":"t","summary":"s","content":"c"}, {"ref":"good/ref","title":"t","summary":"s","content":"c"}]"#;
        let pages = parse_extract_value(raw).unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].page_ref, "good/ref");
    }

    // ── 管线级测试：经统一流水线 + mock LLM 上游 + 真实 DAO/FTS ─────────────────

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use axum::response::IntoResponse as _;
    use tokio::sync::broadcast;

    use crate::db::knowledge::{IndexKind, KsCreateOpts};
    use crate::llm::LlmState;
    use rust_tunnel_rag::extractor::FileType;
    use rust_tunnel_rag::ingest::{IngestOpts, KbEvent};
    use rust_tunnel_rag::store::VectorStore;

    /// 起一个 mock LLM 上游（`POST /v1/chat/completions`）。`content` 为
    /// choices[0].message.content 的原始字符串；`status` 非 200 时返回错误体。
    async fn start_mock_llm(content: &'static str, status: u16) -> (String, Arc<AtomicUsize>) {
        use axum::routing::post;
        use axum::Router;
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_task = hits.clone();
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move || {
                let hits = hits_task.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    if status == 200 {
                        axum::response::Json(serde_json::json!({
                            "id": "cmpl-wiki",
                            "object": "chat.completion",
                            "choices": [{
                                "index": 0,
                                "message": {"role": "assistant", "content": content},
                                "finish_reason": "stop"
                            }]
                        }))
                        .into_response()
                    } else {
                        axum::response::Response::builder()
                            .status(status)
                            .body(axum::body::Body::from("upstream error"))
                            .unwrap()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), hits)
    }

    /// TempDir 放前、store 放后：qdrant-edge 的 `EdgeShard` Drop 时同步 flush
    /// 并 `expect()`（目录已删会 panic），故 store 必须先于 TempDir 析构。
    fn tmp_store() -> (tempfile::TempDir, VectorStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = VectorStore::new(dir.path());
        (dir, store)
    }

    /// 测试基建：内存 DB + 注册 mock provider/model + distill_model 设置 +
    /// 容器/doc 行 + 落盘 md 原文。返回 (db, llm, source_id, doc_id, source_path, _tmp)。
    async fn pipeline_fixture(
        base_url: &str,
        doc_body: &str,
    ) -> (
        crate::db::Database,
        LlmState,
        String,
        String,
        PathBuf,
        tempfile::TempDir,
    ) {
        let db = crate::db::Database::new(":memory:").await.unwrap();
        let cipher = crate::llm::crypto::LlmCipher::from_master_key([7u8; 32]);
        let encrypted = cipher.encrypt("sk-test");
        db.llm_save_provider(
            "p1",
            "Mock",
            "deepseek",
            base_url,
            &encrypted,
            None::<&str>,
            None::<&str>,
            true,
        )
        .await
        .unwrap();
        db.llm_save_model("m1", "p1", "wiki-mock-model", "", "[]", true, None)
            .await
            .unwrap();
        let mut settings = db.memory_get_settings().await.unwrap();
        settings.distill_model = "wiki-mock-model".into();
        db.memory_upsert_settings(&settings).await.unwrap();
        let llm = LlmState::new(Some(db.clone()), Some(cipher));

        db.ks_create(&KsCreateOpts {
            id: "w1".into(),
            name: "ops".into(),
            summary: String::new(),
            index_vector: false,
            index_pages: true,
            scope_type: "workspace".into(),
            client_id: "c1".into(),
            workspace_id: "ws1".into(),
            emb_base_url: String::new(),
            emb_api_key: String::new(),
            emb_model: String::new(),
            emb_dimension: 0,
            top_k: 5,
            chunk_size: 512,
            chunk_overlap: 64,
            score_threshold: 0.3,
            enabled: true,
        })
        .await
        .unwrap();
        db.kdoc_create("d1", "w1", "ops.md", "md", "h1")
            .await
            .unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("d1.md");
        std::fs::write(&path, doc_body).unwrap();
        (db, llm, "w1".into(), "d1".into(), path, tmp)
    }

    /// 等下一条事件：同时容纳超时与 `Closed`（tx 被 move 进任务、结束后 sender 被 drop）。
    async fn next_kb_event(rx: &mut broadcast::Receiver<KbEvent>) -> Option<KbEvent> {
        match tokio::time::timeout(std::time::Duration::from_secs(15), rx.recv()).await {
            Ok(Ok(ev)) => Some(ev),
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => {
                // 低频测试不应 lag；若发生则继续等下一条
                None
            }
            // `Closed`（tx 被 move 进任务、结束后 sender 被 drop）与超时同走结束路径。
            Ok(Err(_)) | Err(_) => None,
        }
    }

    /// 收集事件直到 pages 终态（ready/failed），同时容纳 `Closed`/超时。
    async fn collect_pages_events(rx: &mut broadcast::Receiver<KbEvent>) -> Vec<KbEvent> {
        let mut out = Vec::new();
        loop {
            let Some(ev) = next_kb_event(rx).await else {
                break;
            };
            let is_pages = ev.kind == IndexKind::Pages;
            let terminal = ev.status == "ready" || ev.status == "failed";
            if is_pages {
                out.push(ev);
                if terminal {
                    break;
                }
            }
        }
        out
    }

    const TWO_PAGES: &str = r#"[{"ref":"deploy/checklist","title":"部署清单","summary":"生产部署前检查","content":"先确认 [[ops/backup]] 已完成再发布。","links":["ops/backup"]},{"ref":"ops/backup","title":"备份流程","summary":"每日备份","content":"备份到对象存储。","links":[]}]"#;

    #[tokio::test]
    async fn pipeline_ready_emits_events_and_upserts_pages_and_edges() {
        let (base, _hits) = start_mock_llm(TWO_PAGES, 200).await;
        let (db, llm, source_id, doc_id, path, _tmp) =
            pipeline_fixture(&base, "# 运维手册\n\n部署前先备份。").await;
        let source = db.ks_get(&source_id).await.unwrap().unwrap();
        let (_store_dir, store) = tmp_store();
        let (tx, mut rx) = broadcast::channel(32);
        rust_tunnel_rag::ingest::spawn_ingest(IngestOpts {
            db: db.clone(),
            store: store.clone(),
            cipher: None,
            source: source.clone(),
            doc_id: doc_id.clone(),
            source_path: path,
            file_type: FileType::Markdown,
            tx,
            vector_sem: None,
            pages_sem: None,
            page_extractor: Some(Arc::new(LlmPageExtractor::new(db.clone(), llm.clone()))),
            only: Some(IndexKind::Pages),
        });
        let events = collect_pages_events(&mut rx).await;

        assert!(
            events.iter().any(|e| e.status == "processing"),
            "应有 processing 事件: {events:?}"
        );
        let last = events.last().expect("应有终态事件");
        assert_eq!(last.status, "ready");
        assert_eq!(last.kind, IndexKind::Pages);
        assert_eq!(last.chunk_count, 2, "chunk_count 即页数");

        let page = db
            .wiki_get_page(&source_id, "deploy/checklist")
            .await
            .unwrap()
            .unwrap();
        assert!(page.content.contains("[[ops/backup]]"));
        assert_eq!(page.source_doc_id.as_deref(), Some(doc_id.as_str()));

        let graph = db.wiki_graph(&source_id).await.unwrap();
        assert_eq!(graph.nodes.len(), 2);
        assert_eq!(graph.edges.len(), 1, "[[ref]] 应建边");
        assert!(graph.edges[0].to.is_some(), "闭合链接应回填 dst page id");
        assert!(!graph.edges[0].dangling);

        let w = db.ks_get(&source_id).await.unwrap().unwrap();
        assert_eq!(w.status, "ready");
        assert_eq!(w.page_count, 2);
        let idx = db
            .kdoc_get_index(&doc_id, IndexKind::Pages)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(idx.status, "ready");
    }

    #[tokio::test]
    async fn pipeline_llm_500_marks_failed() {
        let (base, _hits) = start_mock_llm("", 500).await;
        let (db, llm, source_id, doc_id, path, _tmp) =
            pipeline_fixture(&base, "# 手册\n\n内容。").await;
        let source = db.ks_get(&source_id).await.unwrap().unwrap();
        let (_store_dir, store) = tmp_store();
        let (tx, mut rx) = broadcast::channel(32);
        rust_tunnel_rag::ingest::spawn_ingest(IngestOpts {
            db: db.clone(),
            store: store.clone(),
            cipher: None,
            source: source.clone(),
            doc_id: doc_id.clone(),
            source_path: path,
            file_type: FileType::Markdown,
            tx,
            vector_sem: None,
            pages_sem: None,
            page_extractor: Some(Arc::new(LlmPageExtractor::new(db.clone(), llm.clone()))),
            only: Some(IndexKind::Pages),
        });
        let events = collect_pages_events(&mut rx).await;

        assert_eq!(events.last().unwrap().status, "failed");
        assert_eq!(events.last().unwrap().kind, IndexKind::Pages);
        let idx = db
            .kdoc_get_index(&doc_id, IndexKind::Pages)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(idx.status, "failed");
        assert!(idx.error.is_some());
    }

    #[tokio::test]
    async fn pipeline_bad_json_retries_then_fails() {
        let (base, hits) = start_mock_llm("这不是 JSON", 200).await;
        let (db, llm, source_id, doc_id, path, _tmp) =
            pipeline_fixture(&base, "# 手册\n\n内容。").await;
        let source = db.ks_get(&source_id).await.unwrap().unwrap();
        let (_store_dir, store) = tmp_store();
        let (tx, mut rx) = broadcast::channel(32);
        rust_tunnel_rag::ingest::spawn_ingest(IngestOpts {
            db: db.clone(),
            store: store.clone(),
            cipher: None,
            source: source.clone(),
            doc_id: doc_id.clone(),
            source_path: path,
            file_type: FileType::Markdown,
            tx,
            vector_sem: None,
            pages_sem: None,
            page_extractor: Some(Arc::new(LlmPageExtractor::new(db.clone(), llm.clone()))),
            only: Some(IndexKind::Pages),
        });
        let events = collect_pages_events(&mut rx).await;

        assert_eq!(events.last().unwrap().status, "failed");
        assert!(events
            .last()
            .unwrap()
            .error
            .as_deref()
            .unwrap_or("")
            .contains("JSON"));
        // 流水线对每批次重试一次（BATCH_ATTEMPTS=2），本文档仅 1 批次
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "解析失败应重试 1 次共 2 次调用"
        );
    }

    #[tokio::test]
    async fn pipeline_empty_array_ready_with_zero_pages() {
        let (base, _hits) = start_mock_llm("[]", 200).await;
        let (db, llm, source_id, doc_id, path, _tmp) =
            pipeline_fixture(&base, "# 空\n\n无内容。").await;
        let source = db.ks_get(&source_id).await.unwrap().unwrap();
        let (_store_dir, store) = tmp_store();
        let (tx, mut rx) = broadcast::channel(32);
        rust_tunnel_rag::ingest::spawn_ingest(IngestOpts {
            db: db.clone(),
            store: store.clone(),
            cipher: None,
            source: source.clone(),
            doc_id: doc_id.clone(),
            source_path: path,
            file_type: FileType::Markdown,
            tx,
            vector_sem: None,
            pages_sem: None,
            page_extractor: Some(Arc::new(LlmPageExtractor::new(db.clone(), llm.clone()))),
            only: Some(IndexKind::Pages),
        });
        let events = collect_pages_events(&mut rx).await;

        let last = events.last().unwrap();
        assert_eq!(last.status, "ready");
        assert_eq!(last.chunk_count, 0);
        assert_eq!(last.kind, IndexKind::Pages);
        let w = db.ks_get(&source_id).await.unwrap().unwrap();
        assert_eq!(w.page_count, 0);
    }

    #[tokio::test]
    async fn pipeline_locked_page_survives_reingest() {
        let (base, _hits) = start_mock_llm(TWO_PAGES, 200).await;
        let (db, llm, source_id, doc_id, path, _tmp) =
            pipeline_fixture(&base, "# 运维手册\n\n部署前先备份。").await;
        // 预置同 ref 的 locked 手动页
        db.wiki_upsert_page(
            &source_id,
            "deploy/checklist",
            "手动页",
            "手动摘要",
            "手动维护的内容",
            true,
            None,
        )
        .await
        .unwrap();
        let source = db.ks_get(&source_id).await.unwrap().unwrap();
        let (_store_dir, store) = tmp_store();
        let (tx, mut rx) = broadcast::channel(32);
        rust_tunnel_rag::ingest::spawn_ingest(IngestOpts {
            db: db.clone(),
            store: store.clone(),
            cipher: None,
            source: source.clone(),
            doc_id: doc_id.clone(),
            source_path: path,
            file_type: FileType::Markdown,
            tx,
            vector_sem: None,
            pages_sem: None,
            page_extractor: Some(Arc::new(LlmPageExtractor::new(db.clone(), llm.clone()))),
            only: Some(IndexKind::Pages),
        });
        let events = collect_pages_events(&mut rx).await;
        assert_eq!(events.last().unwrap().status, "ready");

        let page = db
            .wiki_get_page(&source_id, "deploy/checklist")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(page.content, "手动维护的内容", "locked 页不被 ingest 覆盖");
        assert_eq!(page.locked, 1);
    }

    #[tokio::test]
    async fn pipeline_missing_model_config_fails_fast() {
        let db = crate::db::Database::new(":memory:").await.unwrap();
        let llm = LlmState::new(Some(db.clone()), None); // distill_model 未配置
        db.ks_create(&KsCreateOpts {
            id: "w1".into(),
            name: "ops".into(),
            summary: String::new(),
            index_vector: false,
            index_pages: true,
            scope_type: "workspace".into(),
            client_id: "c1".into(),
            workspace_id: "ws1".into(),
            emb_base_url: String::new(),
            emb_api_key: String::new(),
            emb_model: String::new(),
            emb_dimension: 0,
            top_k: 5,
            chunk_size: 512,
            chunk_overlap: 64,
            score_threshold: 0.3,
            enabled: true,
        })
        .await
        .unwrap();
        db.kdoc_create("d1", "w1", "ops.md", "md", "h1")
            .await
            .unwrap();
        let source = db.ks_get("w1").await.unwrap().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("d1.md");
        std::fs::write(&path, "# 手册\n\n内容。").unwrap();
        let (store_dir, store) = tmp_store();
        let (tx, mut rx) = broadcast::channel(32);
        rust_tunnel_rag::ingest::spawn_ingest(IngestOpts {
            db: db.clone(),
            store: store.clone(),
            cipher: None,
            source: source.clone(),
            doc_id: "d1".into(),
            source_path: path,
            file_type: FileType::Markdown,
            tx,
            vector_sem: None,
            pages_sem: None,
            page_extractor: Some(Arc::new(LlmPageExtractor::new(db.clone(), llm.clone()))),
            only: Some(IndexKind::Pages),
        });
        let events = collect_pages_events(&mut rx).await;
        assert_eq!(events.last().unwrap().status, "failed");
        assert!(events
            .last()
            .unwrap()
            .error
            .as_deref()
            .unwrap_or("")
            .contains("distill_model"));
        // `tmp` 与 `store_dir`/`store` 的析构顺序由声明顺序保证（store 先于 dir），
        // 无需手动 drop，保留至任务结束即可。
        let _ = (&tmp, &store_dir);
    }
}
