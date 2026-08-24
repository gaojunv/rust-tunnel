//! Wiki 文档摄入后台任务：提取 → 分块 → LLM 结构化抽取 → 落库 → 事件。
//!
//! 放置在 `agent/wiki` 而非 `llm/rag`：Wiki 是 agent 工作台的第四类资产
//! （与 memory/skill 同级，挂 `AgentState`），摄入产出是结构化页面+图谱边+FTS，
//! 而非向量 shard；复用 `llm/rag/extractor` 与 `chunker` 仅为文本提取/分块能力，
//! 不触碰 `VectorStore`。放在 `agent/wiki` 使依赖单向，与 `agent/memory/distill.rs`
//! 的蒸馏管线同层，便于共享 `LlmState` / 设置回落语义。

use std::path::PathBuf;
use std::sync::Arc;

use futures_util::FutureExt;
use tokio::sync::{broadcast, Semaphore};

use crate::wiki::WikiEvent;
use crate::db::Database;
use crate::llm::LlmState;
use crate::llm::rag::chunker;
use crate::llm::rag::extractor::{self, FileType};

/// 单批次 LLM 输入的 chunk 数上限与 token 上限。
const MAX_CHUNKS_PER_BATCH: usize = 3;
const MAX_TOKENS_PER_BATCH: usize = 6000;
/// LLM 输出 content 单页上限（字符）。
const PAGE_CONTENT_MAX_CHARS: usize = 4000;
/// LLM 输出 title/summary 上限。
const TITLE_MAX_CHARS: usize = 64;
const SUMMARY_MAX_CHARS: usize = 200;

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

/// 启动 Wiki 文档摄入后台任务。
///
/// 流程：CAS pending→processing（发事件）→ 提取 → chunk → LLM 批量抽取 → 单事务落库 →
/// ready/failed 事件；`catch_unwind` 兜底 panic→failed（照 `rag/ingest.rs`）。
///
/// `sem` 为可选并发信号量：调用方注入 `Some(Arc<Semaphore(2)>)` 限流；`None` 不限。
#[allow(clippy::too_many_arguments)]
pub fn spawn_wiki_ingest(
    db: Database,
    llm: LlmState,
    wiki_id: String,
    doc_id: String,
    source_path: PathBuf,
    file_type: FileType,
    tx: broadcast::Sender<WikiEvent>,
    sem: Option<Arc<Semaphore>>,
) {
    tokio::spawn(async move {
        let _guard = match sem {
            Some(s) => Some(s.acquire_owned().await.ok()),
            None => None,
        };

        let emit = |status: &str, page_count: i64, err: Option<String>| {
            let _ = tx.send(WikiEvent {
                wiki_id: wiki_id.clone(),
                doc_id: doc_id.clone(),
                status: status.to_string(),
                page_count,
                error: err,
            });
        };

        let result = std::panic::AssertUnwindSafe(async {
            // CAS pending→processing：仅 pending 可抢占（reindex/first ingest 共用）。
            // upload 路径为新建 doc=pending，reindex 路径由 API 先 CAS pending，摄入在
            // 此再 pending→processing，二次 CAS 保证与并发 reindex 互斥。
            let moved = db
                .wiki_mark_doc_processing_if_pending(&doc_id)
                .await
                .unwrap_or(false);
            if !moved {
                // pending 为初始态：CAS 未命中说明 doc 不存在或已被抢占/在途，退出不误发事件。
                return;
            }
            let _ = db.wiki_update_status(&wiki_id, "processing").await;
            emit("processing", 0, None);

            match do_ingest(&db, &llm, &wiki_id, &doc_id, &source_path, file_type).await {
                Ok(count) => {
                    let _ = db
                        .wiki_update_doc_status(&doc_id, "ready", None)
                        .await;
                    let _ = db.wiki_update_status(&wiki_id, "ready").await;
                    emit("ready", count, None);
                }
                Err(e) => {
                    let _ = db
                        .wiki_update_doc_status(&doc_id, "failed", Some(&e))
                        .await;
                    let _ = db.wiki_update_status(&wiki_id, "failed").await;
                    emit("failed", 0, Some(e));
                }
            }
        })
        .catch_unwind()
        .await;

        if let Err(payload) = result {
            let msg = panic_message(&*payload);
            tracing::error!(wiki_id = %wiki_id, doc_id = %doc_id, panic = %msg, "wiki ingest task panicked");
            let _ = db
                .wiki_update_doc_status(&doc_id, "failed", Some(&msg))
                .await;
            let _ = db.wiki_update_status(&wiki_id, "failed").await;
            emit("failed", 0, Some(msg));
        }
    });
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_owned()
    }
}

async fn do_ingest(
    db: &Database,
    llm: &LlmState,
    wiki_id: &str,
    doc_id: &str,
    source_path: &std::path::Path,
    file_type: FileType,
) -> Result<i64, String> {
    let settings = db.memory_get_settings().await.map_err(|e| e.to_string())?;
    let model = settings.distill_model.trim().to_string();
    if model.is_empty() {
        return Err("LLM 未配置：请在 设置 → AI 记忆体 中配置 distill_model".to_string());
    }

    let bytes = tokio::fs::read(source_path)
        .await
        .map_err(|e| format!("read source file: {e}"))?;
    let content = tokio::task::spawn_blocking(move || extractor::extract(&bytes, file_type))
        .await
        .map_err(|e| format!("extract task: {e}"))?
        .map_err(|e| e.to_string())?;
    if content.trim().is_empty() {
        return Err("no text extracted from document".to_string());
    }
    let chunks = chunker::chunk_markdown(&content, 1200, 150);
    if chunks.is_empty() {
        return Err("empty content".to_string());
    }

    let batches = batch_chunks(&chunks);
    let mut all_pages: Vec<ExtractedPage> = Vec::new();
    let mut last_err: Option<String> = None;
    for batch in batches {
        let batch_text = batch
            .iter()
            .map(|c| c.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n---\n\n");
        let mut attempt = 0;
        let mut success = false;
        while attempt < 2 {
            match call_wiki_llm(llm, &model, &batch_text).await {
                Ok(raw) => match parse_extract_value(&raw) {
                    Ok(pages) => {
                        all_pages.extend(pages);
                        success = true;
                        break;
                    }
                    Err(e) => {
                        last_err = Some(e.clone());
                        attempt += 1;
                        if attempt >= 2 {
                            return Err(format!("LLM 返回 JSON 解析失败: {e}"));
                        }
                        tracing::warn!(wiki_id, doc_id, error = %e, attempt, "wiki LLM parse failed, retrying");
                    }
                },
                Err(e) => {
                    last_err = Some(e.clone());
                    attempt += 1;
                    if attempt >= 2 {
                        return Err(e);
                    }
                    tracing::warn!(wiki_id, doc_id, error = %e, attempt, "wiki LLM call failed, retrying");
                }
            }
        }
        if !success {
            if let Some(e) = last_err.take() {
                return Err(e);
            }
        }
    }

    // 空抽取（切片无可提炼内容）→ ready page_count=0（不置 failed，属正常完成）。
    if all_pages.is_empty() {
        db.wiki_clear_pages_by_doc(wiki_id, doc_id)
            .await
            .map_err(|e| e.to_string())?;
        return Ok(0);
    }

    db.wiki_clear_pages_by_doc(wiki_id, doc_id)
        .await
        .map_err(|e| e.to_string())?;

    let mut seen = std::collections::HashSet::new();
    let mut upserted = 0i64;
    for page in &all_pages {
        let Some(norm_ref) = crate::db::wiki::normalize_wiki_ref(&page.page_ref) else {
            tracing::warn!(wiki_id, doc_id, r = %page.page_ref, "wiki ingest: skip invalid ref");
            continue;
        };
        if !seen.insert(norm_ref.clone()) {
            // 同批次重复 ref，后者覆盖前者：DAO upsert 已处理覆盖
        }
        let title = truncate_chars(&page.title, TITLE_MAX_CHARS);
        let summary = truncate_chars(&page.summary, SUMMARY_MAX_CHARS);
        let content = truncate_chars(&page.content, PAGE_CONTENT_MAX_CHARS);
        match db
            .wiki_upsert_page(wiki_id, &norm_ref, &title, &summary, &content, false, Some(doc_id))
            .await
        {
            Ok(_) => upserted += 1,
            Err(e) => {
                tracing::warn!(wiki_id, doc_id, r = %norm_ref, error = %e, "wiki ingest: upsert page failed");
            }
        }
    }

    Ok(upserted)
}

fn batch_chunks(chunks: &[chunker::Chunk]) -> Vec<Vec<chunker::Chunk>> {
    let mut batches: Vec<Vec<chunker::Chunk>> = Vec::new();
    let mut cur: Vec<chunker::Chunk> = Vec::new();
    let mut cur_tokens = 0usize;
    for c in chunks {
        let tok = c.token_count;
        if !cur.is_empty()
            && (cur.len() >= MAX_CHUNKS_PER_BATCH || cur_tokens + tok > MAX_TOKENS_PER_BATCH)
        {
            batches.push(std::mem::take(&mut cur));
            cur_tokens = 0;
        }
        cur_tokens += tok;
        cur.push(c.clone());
        if cur.len() >= MAX_CHUNKS_PER_BATCH || cur_tokens >= MAX_TOKENS_PER_BATCH {
            batches.push(std::mem::take(&mut cur));
            cur_tokens = 0;
        }
    }
    if !cur.is_empty() {
        batches.push(cur);
    }
    batches
}

#[derive(Debug, Clone)]
struct ExtractedPage {
    page_ref: String,
    title: String,
    summary: String,
    content: String,
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

fn parse_extract_value(raw: &str) -> Result<Vec<ExtractedPage>, String> {
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
        let raw_ref = obj
            .get("ref")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
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
        pages.push(ExtractedPage {
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

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_code_fence_handles_json_fence() {
        let raw = "```json\n[{\"ref\":\"a\",\"title\":\"t\",\"summary\":\"s\",\"content\":\"c\"}]\n```";
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

    #[test]
    fn batch_chunks_respects_limits() {
        let chunks = vec![
            chunker::Chunk { heading_path: String::new(), content: "a".repeat(4000), token_count: 1000 },
            chunker::Chunk { heading_path: String::new(), content: "b".repeat(4000), token_count: 1000 },
            chunker::Chunk { heading_path: String::new(), content: "c".repeat(4000), token_count: 1000 },
            chunker::Chunk { heading_path: String::new(), content: "d".repeat(4000), token_count: 1000 },
        ];
        let batches = batch_chunks(&chunks);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), 3);
        assert_eq!(batches[1].len(), 1);
    }

    // ── 管线级测试：mock LLM 上游 + 真实 DAO/FTS ─────────────────

    use std::sync::atomic::{AtomicUsize, Ordering};
    use axum::response::IntoResponse as _;

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

    /// 测试基建：内存 DB + 注册 mock provider/model + distill_model 设置 +
    /// wiki/doc 行 + 落盘 md 原文。返回 (db, llm, wiki_id, doc_id, source_path, _tmp)。
    async fn pipeline_fixture(
        base_url: &str,
        doc_body: &str,
    ) -> (crate::db::Database, LlmState, String, String, PathBuf, tempfile::TempDir) {
        let db = crate::db::Database::new(":memory:").await.unwrap();
        let cipher = crate::llm::crypto::LlmCipher::from_master_key([7u8; 32]);
        let encrypted = cipher.encrypt("sk-test");
        db.llm_save_provider("p1", "Mock", "deepseek", base_url, &encrypted, None::<&str>, None::<&str>, true)
            .await
            .unwrap();
        db.llm_save_model("m1", "p1", "wiki-mock-model", "", "[]", true, None)
            .await
            .unwrap();
        let mut settings = db.memory_get_settings().await.unwrap();
        settings.distill_model = "wiki-mock-model".into();
        db.memory_upsert_settings(&settings).await.unwrap();
        let llm = LlmState::new(Some(db.clone()), Some(cipher));

        db.wiki_create("w1", "ops", "", "workspace", "c1", "ws1").await.unwrap();
        db.wiki_create_doc("d1", "w1", "ops.md", "md", "h1").await.unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("d1.md");
        std::fs::write(&path, doc_body).unwrap();
        (db, llm, "w1".into(), "d1".into(), path, tmp)
    }

    /// 收集事件直到 ready/failed（带超时），返回事件序列。
    async fn collect_events(
        rx: &mut broadcast::Receiver<WikiEvent>,
    ) -> Vec<WikiEvent> {
        let mut out = Vec::new();
        loop {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(15), rx.recv())
                .await
                .expect("event timeout")
                .expect("channel open");
            let terminal = ev.status == "ready" || ev.status == "failed";
            out.push(ev);
            if terminal {
                break;
            }
        }
        out
    }

    const TWO_PAGES: &str = r#"[{"ref":"deploy/checklist","title":"部署清单","summary":"生产部署前检查","content":"先确认 [[ops/backup]] 已完成再发布。","links":["ops/backup"]},{"ref":"ops/backup","title":"备份流程","summary":"每日备份","content":"备份到对象存储。","links":[]}]"#;

    #[tokio::test]
    async fn pipeline_ready_emits_events_and_upserts_pages_and_edges() {
        let (base, _hits) = start_mock_llm(TWO_PAGES, 200).await;
        let (db, llm, wiki, doc, path, _tmp) = pipeline_fixture(&base, "# 运维手册\n\n部署前先备份。").await;
        let (tx, mut rx) = broadcast::channel(8);
        spawn_wiki_ingest(db.clone(), llm, wiki.clone(), doc.clone(), path, FileType::Markdown, tx, None);
        let events = collect_events(&mut rx).await;

        assert_eq!(events.first().unwrap().status, "processing");
        let last = events.last().unwrap();
        assert_eq!(last.status, "ready");
        assert_eq!(last.page_count, 2);

        let page = db.wiki_get_page(&wiki, "deploy/checklist").await.unwrap().unwrap();
        assert!(page.content.contains("[[ops/backup]]"));
        assert_eq!(page.source_doc_id.as_deref(), Some(doc.as_str()));

        let graph = db.wiki_graph(&wiki).await.unwrap();
        assert_eq!(graph.nodes.len(), 2);
        assert_eq!(graph.edges.len(), 1, "[[ref]] 应建边");
        assert!(graph.edges[0].to.is_some(), "闭合链接应回填 dst page id");
        assert!(!graph.edges[0].dangling);

        let w = db.wiki_get(&wiki).await.unwrap().unwrap();
        assert_eq!(w.status, "ready");
        assert_eq!(w.page_count, 2);
        let d = db.wiki_get_doc(&doc).await.unwrap().unwrap();
        assert_eq!(d.status, "ready");
    }

    #[tokio::test]
    async fn pipeline_llm_500_marks_failed() {
        let (base, _hits) = start_mock_llm("", 500).await;
        let (db, llm, wiki, doc, path, _tmp) = pipeline_fixture(&base, "# 手册\n\n内容。").await;
        let (tx, mut rx) = broadcast::channel(8);
        spawn_wiki_ingest(db.clone(), llm, wiki.clone(), doc.clone(), path, FileType::Markdown, tx, None);
        let events = collect_events(&mut rx).await;

        assert_eq!(events.last().unwrap().status, "failed");
        let d = db.wiki_get_doc(&doc).await.unwrap().unwrap();
        assert_eq!(d.status, "failed");
        assert!(d.error.is_some());
    }

    #[tokio::test]
    async fn pipeline_bad_json_retries_then_fails() {
        let (base, hits) = start_mock_llm("这不是 JSON", 200).await;
        let (db, llm, wiki, doc, path, _tmp) = pipeline_fixture(&base, "# 手册\n\n内容。").await;
        let (tx, mut rx) = broadcast::channel(8);
        spawn_wiki_ingest(db.clone(), llm, wiki.clone(), doc.clone(), path, FileType::Markdown, tx, None);
        let events = collect_events(&mut rx).await;

        assert_eq!(events.last().unwrap().status, "failed");
        assert!(events.last().unwrap().error.as_deref().unwrap_or("").contains("JSON"));
        assert_eq!(hits.load(Ordering::SeqCst), 2, "解析失败应重试 1 次共 2 次调用");
    }

    #[tokio::test]
    async fn pipeline_empty_array_ready_with_zero_pages() {
        let (base, _hits) = start_mock_llm("[]", 200).await;
        let (db, llm, wiki, doc, path, _tmp) = pipeline_fixture(&base, "# 空\n\n无内容。").await;
        let (tx, mut rx) = broadcast::channel(8);
        spawn_wiki_ingest(db.clone(), llm, wiki.clone(), doc.clone(), path, FileType::Markdown, tx, None);
        let events = collect_events(&mut rx).await;

        let last = events.last().unwrap();
        assert_eq!(last.status, "ready");
        assert_eq!(last.page_count, 0);
        let w = db.wiki_get(&wiki).await.unwrap().unwrap();
        assert_eq!(w.page_count, 0);
    }

    #[tokio::test]
    async fn pipeline_locked_page_survives_reingest() {
        let (base, _hits) = start_mock_llm(TWO_PAGES, 200).await;
        let (db, llm, wiki, doc, path, _tmp) = pipeline_fixture(&base, "# 运维手册\n\n部署前先备份。").await;
        // 预置同 ref 的 locked 手动页
        db.wiki_upsert_page(&wiki, "deploy/checklist", "手动页", "手动摘要", "手动维护的内容", true, None)
            .await
            .unwrap();
        let (tx, mut rx) = broadcast::channel(8);
        spawn_wiki_ingest(db.clone(), llm, wiki.clone(), doc.clone(), path, FileType::Markdown, tx, None);
        let events = collect_events(&mut rx).await;
        assert_eq!(events.last().unwrap().status, "ready");

        let page = db.wiki_get_page(&wiki, "deploy/checklist").await.unwrap().unwrap();
        assert_eq!(page.content, "手动维护的内容", "locked 页不被 ingest 覆盖");
        assert_eq!(page.locked, 1);
    }

    #[tokio::test]
    async fn pipeline_missing_model_config_fails_fast() {
        let db = crate::db::Database::new(":memory:").await.unwrap();
        let llm = LlmState::new(Some(db.clone()), None); // distill_model 未配置
        db.wiki_create("w1", "ops", "", "workspace", "c1", "ws1").await.unwrap();
        db.wiki_create_doc("d1", "w1", "ops.md", "md", "h1").await.unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("d1.md");
        std::fs::write(&path, "# 手册\n\n内容。").unwrap();
        let (tx, mut rx) = broadcast::channel(8);
        spawn_wiki_ingest(db.clone(), llm, "w1".into(), "d1".into(), path, FileType::Markdown, tx, None);
        let events = collect_events(&mut rx).await;
        assert_eq!(events.last().unwrap().status, "failed");
        assert!(events.last().unwrap().error.as_deref().unwrap_or("").contains("distill_model"));
    }
}
