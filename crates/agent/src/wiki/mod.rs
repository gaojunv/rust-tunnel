//! Wiki 运行时：`WikiState` 挂 `AgentState`，与 `MemoryState` 并列。
//! 仅 `rag` feature 编译。对齐 `agent/memory` 的挂载与广播形态。

#[cfg(feature = "rag")]
pub mod ingest;

use std::sync::Arc;
use tokio::sync::Semaphore;

use crate::db::Database;
use crate::llm::LlmState;

/// Wiki 摄入事件（与 `MemoryEvent` 同构，SSE 推给前端）。
#[cfg(feature = "rag")]
#[derive(Debug, Clone, serde::Serialize)]
pub struct WikiEvent {
    pub wiki_id: String,
    pub doc_id: String,
    pub status: String,
    pub page_count: i64,
    pub error: Option<String>,
}

/// Wiki 运行时：挂 `AgentState`（同 `MemoryState`），供 ingest/API/SSE 共享。
///
/// 不触碰向量（零 `VectorStore` 依赖）；`LlmState` / `Database` 与 memory 共用
/// 同一实例，`wiki_tx` 与 `MemoryState.events` 并列为独立广播。
#[cfg(feature = "rag")]
#[derive(Clone)]
pub struct WikiState {
    pub db: Database,
    pub llm: LlmState,
    /// 摄入状态事件广播（订阅者即 `/api/agent/wiki/events`）。
    pub events: tokio::sync::broadcast::Sender<WikiEvent>,
    /// LLM 并发限流：`Semaphore(2)`（对齐计划与 RAG `Semaphore(4)` 语义）。
    pub ingest_sem: Arc<Semaphore>,
}

#[cfg(feature = "rag")]
impl std::fmt::Debug for WikiState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WikiState")
            .field("db", &"Database{..}")
            .field("llm_init", &self.llm.db.is_some())
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "rag")]
impl WikiState {
    #[must_use]
    pub fn new(db: Database, llm: LlmState) -> Self {
        // 容量 64：与 MemoryState / LlmState.rag_tx 一致，低频事件不阻塞调用方。
        let (events, _rx) = tokio::sync::broadcast::channel(64);
        Self {
            db,
            llm,
            events,
            ingest_sem: Arc::new(Semaphore::new(2)),
        }
    }

    #[must_use]
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<WikiEvent> {
        self.events.subscribe()
    }
}

pub use crate::db::wiki::{normalize_wiki_ref, parse_wiki_links};

// ── 清单注入与工具短路（仅 rag）────────────────────────────────────

#[cfg(feature = "rag")]
use crate::db::wiki::AgentWikiRecord;

/// 清单块硬上限（字符，≈1K tokens），不暴露 UI。
#[cfg(feature = "rag")]
pub const WIKI_LIST_MAX_CHARS: usize = 4096;

/// 归一化 Wiki 容器名：trim + to_lowercase（大小写不敏感，作用域去重与工具寻址都基于此）。
#[cfg(feature = "rag")]
#[must_use]
pub fn normalize_wiki_name(name: &str) -> String {
    name.trim().to_lowercase()
}

/// 清单注入：wiki_enabled 关闭 / 无可见容器 → None。纯 SQL + 字符串拼接，零 embedding 依赖。
#[cfg(feature = "rag")]
pub async fn retrieve_wiki_list_for_session(
    wiki_state: &WikiState,
    client_id: &str,
    workspace_id: &str,
) -> Option<String> {
    let s = wiki_state.db.memory_get_settings().await.ok()?;
    if s.wiki_enabled == 0 {
        return None;
    }
    let max = usize::try_from(s.wiki_list_max.clamp(1, 50)).unwrap_or(20);
    let rows = wiki_state
        .db
        .wiki_visible_wikis(client_id, workspace_id, i64::try_from(max).unwrap_or(50))
        .await
        .unwrap_or_default();
    if rows.is_empty() {
        return None;
    }
    build_wiki_list_block(&rows, max, WIKI_LIST_MAX_CHARS)
}

/// 组装 `<wikis>...</wikis>` 清单块：按 page_count DESC 排序，预算（条数上限 +
/// 字符上限）内只保留完整行，绝不半截。无条目 → None。
#[cfg(feature = "rag")]
#[must_use]
pub fn build_wiki_list_block(
    items: &[AgentWikiRecord],
    max_items: usize,
    max_chars: usize,
) -> Option<String> {
    let mut sorted: Vec<&AgentWikiRecord> = items.iter().collect();
    sorted.sort_by_key(|w| std::cmp::Reverse(w.page_count));
    let mut s = String::from("<wikis>\n以下是本工作区可用的 Wiki 知识库清单（name + 摘要 + 页数）。需要时先调用 wiki_search 按关键词搜索，再用 wiki_read 按 ref 拉取全文：\n");
    let mut added = 0usize;
    for wiki in sorted {
        if added >= max_items {
            break;
        }
        let summary = if wiki.summary.trim().is_empty() {
            "无摘要".to_string()
        } else {
            wiki.summary.clone()
        };
        let item = format!("- {}: {} ({} 页)\n", wiki.name, summary, wiki.page_count);
        if s.len() + item.len() > max_chars && added > 0 {
            break;
        }
        s.push_str(&item);
        added += 1;
    }
    if added == 0 {
        return None;
    }
    s.push_str("</wikis>");
    Some(s)
}

/// 容器名解析 helper：normalize 后按 workspace > client > global 逐级查找。
#[cfg(feature = "rag")]
async fn resolve_wiki_by_name(
    wiki_state: &WikiState,
    name: &str,
    client_id: &str,
    workspace_id: &str,
) -> Option<AgentWikiRecord> {
    let normalized = normalize_wiki_name(name);
    if normalized.is_empty() {
        return None;
    }
    for scope in ["workspace", "client", "global"] {
        let (scope_type, cid, wid) = crate::memory::scope_coords(scope, client_id, workspace_id);
        if let Ok(Some(row)) = wiki_state
            .db
            .wiki_get_by_name_scope_ci(&normalized, &scope_type, &cid, &wid)
            .await
        {
            return Some(row);
        }
    }
    None
}

/// wiki_search 工具短路：`{query, limit?, wiki?}` → 可见容器内 BM25+LIKE 检索，命中渲染为紧凑文本。
///
/// # Errors
/// 参数非法 / wiki 禁用 / 数据库读取失败时返回错误文本喂回模型。
#[cfg(feature = "rag")]
pub async fn wiki_search_from_agent(
    wiki_state: &WikiState,
    client_id: &str,
    workspace_id: &str,
    args_json: &str,
) -> Result<String, String> {
    use std::fmt::Write as _;
    let args: serde_json::Value =
        serde_json::from_str(args_json).map_err(|e| format!("invalid arguments: {e}"))?;
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .map_or("", str::trim);
    if query.is_empty() {
        return Err("wiki_search requires non-empty 'query'".into());
    }
    if query.len() > 500 {
        return Err("query too long (>500 chars)".into());
    }
    let limit = args
        .get("limit")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(5);
    if !(1..=20).contains(&limit) {
        return Err("limit must be 1-20".into());
    }
    let wiki_name = args
        .get("wiki")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let s = wiki_state
        .db
        .memory_get_settings()
        .await
        .map_err(|e| format!("settings read failed: {e}"))?;
    if s.wiki_enabled == 0 {
        return Err("wiki is disabled".into());
    }
    let visible_ids: Vec<String> = if let Some(name) = wiki_name {
        if name.len() > 64 {
            return Err("wiki name too long (>64)".into());
        }
        let wiki = resolve_wiki_by_name(wiki_state, name, client_id, workspace_id)
            .await
            .ok_or_else(|| format!("wiki not found: {name}"))?;
        vec![wiki.id]
    } else {
        wiki_state
            .db
            .wiki_visible_ids(client_id, workspace_id)
            .await
            .map_err(|e| format!("wiki lookup failed: {e}"))?
    };
    if visible_ids.is_empty() {
        return Ok("no visible wikis".to_string());
    }
    let hits = wiki_state
        .db
        .wiki_search(&visible_ids, query, i64::try_from(limit).unwrap_or(20))
        .await
        .map_err(|e| format!("wiki search failed: {e}"))?;
    if hits.is_empty() {
        return Ok(format!("no results for \"{query}\""));
    }
    let mut out = format!(
        "wiki_search results for \"{query}\" ({} hits):\n",
        hits.len()
    );
    for hit in hits {
        let _ = writeln!(
            out,
            "- ref: {} | title: {} | summary: {} | snippet: {}",
            hit.page_ref, hit.title, hit.summary, hit.snippet
        );
    }
    Ok(out)
}

/// wiki_read 工具短路：`{wiki, refs[]}` → 批量取全文，命中 bump_use。
///
/// # Errors
/// 参数非法 / wiki 禁用 / 数据库读取失败时返回错误文本喂回模型。
#[cfg(feature = "rag")]
pub async fn wiki_read_from_agent(
    wiki_state: &WikiState,
    client_id: &str,
    workspace_id: &str,
    args_json: &str,
) -> Result<String, String> {
    use std::fmt::Write as _;
    let args: serde_json::Value =
        serde_json::from_str(args_json).map_err(|e| format!("invalid arguments: {e}"))?;
    let wiki_name = args
        .get("wiki")
        .and_then(|v| v.as_str())
        .map_or("", str::trim);
    if wiki_name.is_empty() {
        return Err("wiki_read requires non-empty 'wiki'".into());
    }
    if wiki_name.len() > 64 {
        return Err("wiki name too long (>64)".into());
    }
    let refs_val = args
        .get("refs")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "wiki_read requires array 'refs'".to_string())?;
    if refs_val.is_empty() || refs_val.len() > 10 {
        return Err("refs must be 1-10".into());
    }
    let mut refs_norm = Vec::with_capacity(refs_val.len());
    for v in refs_val {
        let s = v
            .as_str()
            .ok_or_else(|| "refs must be strings".to_string())?
            .trim();
        if s.is_empty() {
            return Err("ref must not be empty".into());
        }
        let norm = normalize_wiki_ref(s).ok_or_else(|| format!("invalid ref: {s}"))?;
        refs_norm.push(norm);
    }
    let wiki = resolve_wiki_by_name(wiki_state, wiki_name, client_id, workspace_id)
        .await
        .ok_or_else(|| format!("wiki not found: {wiki_name}"))?;
    let s = wiki_state
        .db
        .memory_get_settings()
        .await
        .map_err(|e| format!("settings read failed: {e}"))?;
    if s.wiki_enabled == 0 {
        return Err("wiki is disabled".into());
    }
    let mut out = String::new();
    for r in refs_norm {
        let page = wiki_state
            .db
            .wiki_get_page(&wiki.id, &r)
            .await
            .map_err(|e| format!("wiki read failed: {e}"))?;
        if let Some(p) = page {
            let _ = wiki_state.db.wiki_bump_use(&p.id).await;
            let _ = write!(
                out,
                "## {} - {}\n{}\n\n{}\n\n---\n",
                p.page_ref, p.title, p.summary, p.content
            );
        } else {
            let _ = writeln!(out, "ref not found: {r}\n---");
        }
    }
    Ok(out)
}

#[cfg(all(test, feature = "rag"))]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::llm::LlmState;

    fn record(id: &str, name: &str, summary: &str, page_count: i64) -> AgentWikiRecord {
        AgentWikiRecord {
            id: id.into(),
            name: name.into(),
            summary: summary.into(),
            status: "ready".into(),
            version: 1,
            page_count,
            scope_type: "workspace".into(),
            client_id: "c1".into(),
            workspace_id: "w1".into(),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    async fn wiki_state() -> (Database, WikiState) {
        let db = Database::new(":memory:").await.unwrap();
        let llm = LlmState::new(None, None);
        let wiki = WikiState::new(db.clone(), llm);
        (db, wiki)
    }

    #[test]
    fn build_block_sorts_and_respects_budget() {
        let items = vec![
            record("a", "alpha", "摘要 a", 1),
            record("b", "beta", "摘要 b", 5),
            record("c", "gamma", "摘要 c", 3),
        ];
        let block = build_wiki_list_block(&items, 10, WIKI_LIST_MAX_CHARS).unwrap();
        assert!(block.starts_with("<wikis>"));
        assert!(block.ends_with("</wikis>"));
        assert!(block.find("beta").unwrap() < block.find("alpha").unwrap());
        assert!(block.contains("gamma"));
        assert!(block.contains("5 页"));

        let one = build_wiki_list_block(&items, 1, WIKI_LIST_MAX_CHARS).unwrap();
        assert!(one.contains("beta"));
        assert!(!one.contains("alpha"));

        let tiny = build_wiki_list_block(&items, 10, 5).unwrap();
        assert!(tiny.contains("beta"));
        assert!(tiny
            .lines()
            .all(|l| !l.ends_with(':') || l.starts_with('-')));

        assert!(build_wiki_list_block(&[], 10, WIKI_LIST_MAX_CHARS).is_none());
    }

    #[tokio::test]
    async fn retrieve_gated_and_scope_visible() {
        let (db, wiki) = wiki_state().await;
        // 默认 wiki_enabled=1，但无容器 → None
        assert!(retrieve_wiki_list_for_session(&wiki, "c1", "w1")
            .await
            .is_none());

        db.wiki_create("g1", "global-wiki", "全局", "global", "", "")
            .await
            .unwrap();
        db.wiki_create("w1a", "ws-wiki", "工作区", "workspace", "c1", "w1")
            .await
            .unwrap();
        db.wiki_create("w2", "other-ws", "其他", "workspace", "c1", "w2")
            .await
            .unwrap();
        // page_count 影响排序
        db.wiki_upsert_page("g1", "p1", "t", "s", "c", false, None)
            .await
            .unwrap();
        db.wiki_upsert_page("g1", "p2", "t", "s", "c", false, None)
            .await
            .unwrap();
        db.wiki_upsert_page("w1a", "p1", "t", "s", "c", false, None)
            .await
            .unwrap();

        let block = retrieve_wiki_list_for_session(&wiki, "c1", "w1")
            .await
            .unwrap();
        assert!(block.starts_with("<wikis>"));
        assert!(block.contains("global-wiki"));
        assert!(block.contains("ws-wiki"));
        assert!(!block.contains("other-ws"), "其他 workspace 不可见");
        assert!(block.find("global-wiki").unwrap() < block.find("ws-wiki").unwrap());

        // wiki_enabled 关闭 → None
        let mut s = db.memory_get_settings().await.unwrap();
        s.wiki_enabled = 0;
        db.memory_upsert_settings(&s).await.unwrap();
        assert!(retrieve_wiki_list_for_session(&wiki, "c1", "w1")
            .await
            .is_none());

        // 重新开启，wiki_list_max 限制
        s.wiki_enabled = 1;
        s.wiki_list_max = 1;
        db.memory_upsert_settings(&s).await.unwrap();
        let block = retrieve_wiki_list_for_session(&wiki, "c1", "w1")
            .await
            .unwrap();
        let lines: Vec<&str> = block.lines().filter(|l| l.starts_with("- ")).collect();
        assert_eq!(lines.len(), 1);
    }

    #[tokio::test]
    async fn search_param_validation_and_e2e() {
        let (db, wiki) = wiki_state().await;
        db.wiki_create("w1", "my-wiki", "desc", "workspace", "c1", "w1")
            .await
            .unwrap();
        db.wiki_upsert_page(
            "w1",
            "deploy/prod",
            "部署",
            "摘要",
            "内容含部署相关",
            false,
            None,
        )
        .await
        .unwrap();
        db.wiki_upsert_page("w1", "other/page", "其他", "摘要", "无关", false, None)
            .await
            .unwrap();

        assert!(wiki_search_from_agent(&wiki, "c1", "w1", r#"{"limit":5}"#)
            .await
            .is_err());
        assert!(
            wiki_search_from_agent(&wiki, "c1", "w1", r#"{"query":"  "}"#)
                .await
                .is_err()
        );
        assert!(
            wiki_search_from_agent(&wiki, "c1", "w1", r#"{"query":"部署","limit":0}"#)
                .await
                .is_err()
        );
        assert!(
            wiki_search_from_agent(&wiki, "c1", "w1", r#"{"query":"部署","limit":30}"#)
                .await
                .is_err()
        );
        assert!(
            wiki_search_from_agent(&wiki, "c1", "w1", r#"{"query":"部署","wiki":"ghost"}"#)
                .await
                .unwrap_err()
                .contains("not found")
        );

        let out = wiki_search_from_agent(&wiki, "c1", "w1", r#"{"query":"部署"}"#)
            .await
            .unwrap();
        assert!(out.contains("deploy/prod"), "out: {out}");
        assert!(out.contains("部署"));

        let out = wiki_search_from_agent(&wiki, "c1", "w1", r#"{"query":"部署","wiki":"my-wiki"}"#)
            .await
            .unwrap();
        assert!(out.contains("deploy/prod"));

        let out = wiki_search_from_agent(&wiki, "c1", "w1", r#"{"query":"部署","wiki":"My-Wiki"}"#)
            .await
            .unwrap();
        assert!(out.contains("deploy/prod"));

        db.wiki_create("g1", "same-name", "全局", "global", "", "")
            .await
            .unwrap();
        db.wiki_create("c1w", "same-name", "客户端", "client", "c1", "")
            .await
            .unwrap();
        db.wiki_create("w1b", "same-name", "工作区", "workspace", "c1", "w1")
            .await
            .unwrap();
        db.wiki_upsert_page("w1b", "ws/page", "ws", "s", "ws content", false, None)
            .await
            .unwrap();
        db.wiki_upsert_page("c1w", "cl/page", "cl", "s", "cl content", false, None)
            .await
            .unwrap();
        db.wiki_upsert_page("g1", "gl/page", "gl", "s", "gl content", false, None)
            .await
            .unwrap();
        let out = wiki_search_from_agent(
            &wiki,
            "c1",
            "w1",
            r#"{"query":"ws content","wiki":"same-name"}"#,
        )
        .await
        .unwrap();
        assert!(out.contains("ws/page"));
        let out = wiki_search_from_agent(
            &wiki,
            "c1",
            "w2",
            r#"{"query":"cl content","wiki":"same-name"}"#,
        )
        .await
        .unwrap();
        assert!(out.contains("cl/page"));
        let out = wiki_search_from_agent(
            &wiki,
            "other",
            "w9",
            r#"{"query":"gl content","wiki":"same-name"}"#,
        )
        .await
        .unwrap();
        assert!(out.contains("gl/page"));

        let mut s = db.memory_get_settings().await.unwrap();
        s.wiki_enabled = 0;
        db.memory_upsert_settings(&s).await.unwrap();
        assert!(
            wiki_search_from_agent(&wiki, "c1", "w1", r#"{"query":"部署"}"#)
                .await
                .unwrap_err()
                .contains("disabled")
        );
    }

    #[tokio::test]
    async fn read_param_validation_and_bump() {
        let (db, wiki) = wiki_state().await;
        db.wiki_create("w1", "my-wiki", "", "workspace", "c1", "w1")
            .await
            .unwrap();
        db.wiki_upsert_page("w1", "a/b", "A", "sum A", "content A", false, None)
            .await
            .unwrap();
        db.wiki_upsert_page("w1", "c/d", "C", "sum C", "content C", false, None)
            .await
            .unwrap();

        assert!(
            wiki_read_from_agent(&wiki, "c1", "w1", r#"{"refs":["a/b"]}"#)
                .await
                .is_err()
        );
        assert!(
            wiki_read_from_agent(&wiki, "c1", "w1", r#"{"wiki":"my-wiki","refs":[]}"#)
                .await
                .is_err()
        );
        let many: Vec<String> = (0..11).map(|i| format!("r{i}")).collect();
        let args = serde_json::json!({"wiki":"my-wiki","refs": many}).to_string();
        assert!(wiki_read_from_agent(&wiki, "c1", "w1", &args)
            .await
            .is_err());
        assert!(wiki_read_from_agent(
            &wiki,
            "c1",
            "w1",
            r#"{"wiki":"my-wiki","refs":["bad ref"]}"#
        )
        .await
        .is_err());
        assert!(
            wiki_read_from_agent(&wiki, "c1", "w1", r#"{"wiki":"ghost","refs":["a/b"]}"#)
                .await
                .unwrap_err()
                .contains("not found")
        );

        let out = wiki_read_from_agent(&wiki, "c1", "w1", r#"{"wiki":"my-wiki","refs":["a/b"]}"#)
            .await
            .unwrap();
        assert!(out.contains("content A"));
        assert!(out.contains("a/b"));
        let p = db.wiki_get_page("w1", "a/b").await.unwrap().unwrap();
        assert_eq!(p.use_count, 1);
        let _ = wiki_read_from_agent(
            &wiki,
            "c1",
            "w1",
            r#"{"wiki":"my-wiki","refs":["a/b","c/d"]}"#,
        )
        .await
        .unwrap();
        let p = db.wiki_get_page("w1", "a/b").await.unwrap().unwrap();
        assert_eq!(p.use_count, 2);
        let p2 = db.wiki_get_page("w1", "c/d").await.unwrap().unwrap();
        assert_eq!(p2.use_count, 1);

        let out = wiki_read_from_agent(
            &wiki,
            "c1",
            "w1",
            r#"{"wiki":"my-wiki","refs":["a/b","missing/ref"]}"#,
        )
        .await
        .unwrap();
        assert!(out.contains("content A"));
        assert!(out.contains("not found: missing/ref"));

        let mut s = db.memory_get_settings().await.unwrap();
        s.wiki_enabled = 0;
        db.memory_upsert_settings(&s).await.unwrap();
        assert!(
            wiki_read_from_agent(&wiki, "c1", "w1", r#"{"wiki":"my-wiki","refs":["a/b"]}"#)
                .await
                .unwrap_err()
                .contains("disabled")
        );
    }
}
