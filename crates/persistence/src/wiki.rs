//! Wiki 页面/边/FTS 数据访问层（`knowledge_pages` 系）。
//!
//! 容器/文档能力已收敛至 `knowledge.rs`（`ks_*` / `kdoc_*`）；本模块只保留
//! 页面与图谱相关，以及三個不碰表的纯函数。

use std::collections::HashSet;

use super::Database;

// ── 链接解析工具 ────────────────────────────────────────────────

/// 归一化 wiki `ref`：`trim` + `lowercase` + 校验后返回 `Some(normalized)`，
/// 非法返回 `None`。规范：`^[a-z0-9][a-z0-9/_-]{0,127}$`，禁 `//`、`./`、`../`，
/// 长度 ≤128。
#[must_use]
pub fn normalize_wiki_ref(raw: &str) -> Option<String> {
    let s = raw.trim().to_lowercase();
    if s.is_empty() || s.len() > 128 {
        return None;
    }
    if s.contains("//") || s.contains("./") || s.contains("../") {
        return None;
    }
    let bytes = s.as_bytes();
    if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit() {
        return None;
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '/' || c == '_' || c == '-')
    {
        return None;
    }
    Some(s)
}

/// 从 `content` 中抽取内联链接 `[[ref]]`，归一化并去重后返回。
#[must_use]
pub fn parse_wiki_links(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut rest = content;
    while let Some(start) = rest.find("[[") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("]]") else {
            break;
        };
        let raw = &after[..end];
        if let Some(norm) = normalize_wiki_ref(raw) {
            if seen.insert(norm.clone()) {
                out.push(norm);
            }
        }
        rest = &after[end + 2..];
    }
    out
}

// ── Scope 判定（容器级，对齐 `agent/memory::scope_ok`）────────────

/// 容器行对当前会话是否可见。`global` 恒可见；`client` 需客户端匹配；
/// `workspace` 需客户端 + 工作区都匹配。
#[must_use]
pub fn wiki_scope_ok(
    scope_type: &str,
    client_id: &str,
    workspace_id: &str,
    target_client: &str,
    target_workspace: &str,
) -> bool {
    scope_type == "global"
        || (scope_type == "client" && client_id == target_client)
        || (scope_type == "workspace"
            && client_id == target_client
            && workspace_id == target_workspace)
}

// ── 记录类型 ─────────────────────────────────────────────────────

/// Wiki 页面记录（`knowledge_pages` 表的一行，含正文）。
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct AgentWikiPageRecord {
    /// 页面 id（主键）。
    pub id: String,
    /// 所属容器 id（`knowledge_pages.source_id`，序列化仍为 `wiki_id` 以兼容旧 API）。
    #[sqlx(rename = "source_id")]
    pub wiki_id: String,
    /// 页面引用路径（`ref` 列）。
    #[sqlx(rename = "ref")]
    pub page_ref: String,
    /// 页面标题。
    pub title: String,
    /// 页面摘要。
    pub summary: String,
    /// 页面正文（Markdown）。
    pub content: String,
    /// 是否锁定（1 锁定，0 未锁定）。
    pub locked: i64,
    /// 来源文档 id，手动页为 None。
    pub source_doc_id: Option<String>,
    /// 被引用/使用次数。
    pub use_count: i64,
    /// 最后使用时间，未使用为 None。
    pub last_used_at: Option<String>,
    /// 创建时间。
    pub created_at: String,
    /// 更新时间。
    pub updated_at: String,
}

/// 页面列表/摘要视图（不含 `content`）。
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct AgentWikiPageSummary {
    /// 页面 id（主键）。
    pub id: String,
    /// 所属容器 id。
    #[sqlx(rename = "source_id")]
    pub wiki_id: String,
    /// 页面引用路径。
    #[sqlx(rename = "ref")]
    pub page_ref: String,
    /// 页面标题。
    pub title: String,
    /// 页面摘要。
    pub summary: String,
    /// 是否锁定（1 锁定，0 未锁定）。
    pub locked: i64,
    /// 来源文档 id，手动页为 None。
    pub source_doc_id: Option<String>,
    /// 被引用次数。
    pub use_count: i64,
    /// 最后使用时间，未使用为 None。
    pub last_used_at: Option<String>,
    /// 创建时间。
    pub created_at: String,
    /// 更新时间。
    pub updated_at: String,
}

/// Wiki 边记录（`knowledge_page_edges` 表的一行）。
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct AgentWikiEdgeRecord {
    /// 所属容器 id。
    #[sqlx(rename = "source_id")]
    pub wiki_id: String,
    /// 源页面 id。
    pub src_page_id: String,
    /// 源页面引用路径。
    pub src_ref: String,
    /// 目标引用路径（可能悬空）。
    pub dst_ref: String,
    /// 目标页面 id，悬空时为 None。
    pub dst_page_id: Option<String>,
}

/// FTS5 检索命中（`rank` 为 `bm25` 负分或 LIKE 回退的占位）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct WikiSearchHit {
    /// 命中页面 id。
    pub page_id: String,
    /// 所属容器 id。
    pub wiki_id: String,
    #[serde(rename = "ref")]
    /// 页面引用路径。
    pub page_ref: String,
    /// 页面标题。
    pub title: String,
    /// 页面摘要。
    pub summary: String,
    /// 高亮片段（FTS snippet 或 summary 回退）。
    pub snippet: String,
    /// 排序分数（bm25 负分或 LIKE 占位 0.0）。
    pub rank: f64,
}

/// Graph 响应：`nodes` 为页面摘要，`edges` 为有向边（含悬空标记）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct WikiGraph {
    /// 图节点（页面摘要列表）。
    pub nodes: Vec<AgentWikiPageSummary>,
    /// 图边列表。
    pub edges: Vec<WikiGraphEdge>,
}

/// Wiki 图边（Graph 响应中的一条有向边）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct WikiGraphEdge {
    /// 源页面 id。
    pub from: String,
    /// 源页面引用路径。
    pub from_ref: String,
    /// 目标页面 id，悬空时为 None。
    pub to: Option<String>,
    /// 目标引用路径。
    pub to_ref: String,
    /// 是否为悬空边。
    pub dangling: bool,
}

// ── 页面：upsert（正表+FTS+边 同事务）、delete、列表、bump_use ───────

impl Database {
    /// 页面 upsert：`locked=1` 的页不被覆盖；同事务同步 FTS 与边。
    /// `source_doc_id` 可空（手动页为 `None`）。
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)] // 保留：单调用点方法，Opts 化成本高
    pub async fn wiki_upsert_page(
        &self,
        wiki_id: &str,
        page_ref: &str,
        title: &str,
        summary: &str,
        content: &str,
        locked: bool,
        source_doc_id: Option<&str>,
    ) -> Result<String, sqlx::Error> {
        let existing: Option<AgentWikiPageRecord> = sqlx::query_as(
            "SELECT id, source_id, ref, title, summary, content, locked, source_doc_id, use_count, last_used_at, created_at, updated_at \
             FROM knowledge_pages WHERE source_id = ? AND ref = ?",
        )
        .bind(wiki_id)
        .bind(page_ref)
        .fetch_optional(&self.pool)
        .await?;
        if let Some(ref row) = existing {
            if row.locked != 0 && !locked {
                return Ok(row.id.clone());
            }
        }

        let old_rowid: Option<(i64,)> = if let Some(ref row) = existing {
            sqlx::query_as("SELECT rowid FROM knowledge_pages WHERE id = ?")
                .bind(&row.id)
                .fetch_optional(&self.pool)
                .await?
        } else {
            None
        };

        let page_id = existing.as_ref().map_or_else(
            || format!("{:032x}", rand::random::<u128>()),
            |r| r.id.clone(),
        );

        let links = parse_wiki_links(content);

        let mut tx = self.pool.begin().await?;

        if existing.is_some() {
            sqlx::query(
                r"UPDATE knowledge_pages
                   SET title = ?, summary = ?, content = ?, locked = ?, source_doc_id = ?, updated_at = datetime('now')
                   WHERE id = ?",
            )
            .bind(title)
            .bind(summary)
            .bind(content)
            .bind(i64::from(locked))
            .bind(source_doc_id)
            .bind(&page_id)
            .execute(&mut *tx)
            .await?;
            if let Some((rid,)) = old_rowid {
                sqlx::query("DELETE FROM knowledge_pages_fts WHERE rowid = ?")
                    .bind(rid)
                    .execute(&mut *tx)
                    .await?;
            }
            sqlx::query("DELETE FROM knowledge_page_edges WHERE src_page_id = ?")
                .bind(&page_id)
                .execute(&mut *tx)
                .await?;
        } else {
            sqlx::query(
                r"INSERT INTO knowledge_pages (id, source_id, ref, title, summary, content, locked, source_doc_id)
                   VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&page_id)
            .bind(wiki_id)
            .bind(page_ref)
            .bind(title)
            .bind(summary)
            .bind(content)
            .bind(i64::from(locked))
            .bind(source_doc_id)
            .execute(&mut *tx)
            .await?;
        }

        let rowid: (i64,) = sqlx::query_as("SELECT rowid FROM knowledge_pages WHERE id = ?")
            .bind(&page_id)
            .fetch_one(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO knowledge_pages_fts (rowid, ref, title, summary, content) VALUES (?, ?, ?, ?, ?)")
            .bind(rowid.0)
            .bind(page_ref)
            .bind(title)
            .bind(summary)
            .bind(content)
            .execute(&mut *tx)
            .await?;

        for dst_ref in &links {
            let dst: Option<(String,)> =
                sqlx::query_as("SELECT id FROM knowledge_pages WHERE source_id = ? AND ref = ?")
                    .bind(wiki_id)
                    .bind(dst_ref)
                    .fetch_optional(&mut *tx)
                    .await?;
            sqlx::query(
                "INSERT OR REPLACE INTO knowledge_page_edges (source_id, src_page_id, src_ref, dst_ref, dst_page_id) \
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(wiki_id)
            .bind(&page_id)
            .bind(page_ref)
            .bind(dst_ref)
            .bind(dst.map(|(id,)| id))
            .execute(&mut *tx)
            .await?;
        }

        sqlx::query(
            "UPDATE knowledge_page_edges SET dst_page_id = ? WHERE source_id = ? AND dst_ref = ? AND dst_page_id IS NULL",
        )
        .bind(&page_id)
        .bind(wiki_id)
        .bind(page_ref)
        .execute(&mut *tx)
        .await?;

        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM knowledge_pages WHERE source_id = ?")
                .bind(wiki_id)
                .fetch_one(&mut *tx)
                .await?;
        sqlx::query(
            "UPDATE knowledge_sources SET page_count = ?, version = version + 1, updated_at = datetime('now') WHERE id = ?",
        )
        .bind(count.0)
        .bind(wiki_id)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(page_id)
    }
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn wiki_get_page(
        &self,
        wiki_id: &str,
        page_ref: &str,
    ) -> Result<Option<AgentWikiPageRecord>, sqlx::Error> {
        sqlx::query_as::<_, AgentWikiPageRecord>(
            "SELECT id, source_id, ref, title, summary, content, locked, source_doc_id, use_count, last_used_at, created_at, updated_at \
             FROM knowledge_pages WHERE source_id = ? AND ref = ?",
        )
        .bind(wiki_id)
        .bind(page_ref)
        .fetch_optional(&self.pool)
        .await
    }
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn wiki_get_page_by_id(
        &self,
        id: &str,
    ) -> Result<Option<AgentWikiPageRecord>, sqlx::Error> {
        sqlx::query_as::<_, AgentWikiPageRecord>(
            "SELECT id, source_id, ref, title, summary, content, locked, source_doc_id, use_count, last_used_at, created_at, updated_at \
             FROM knowledge_pages WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
    }

    /// 删除页面：同事务清 FTS、出边与入边悬空化。
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn wiki_delete_page(
        &self,
        wiki_id: &str,
        page_ref: &str,
    ) -> Result<bool, sqlx::Error> {
        let existing: Option<AgentWikiPageRecord> = sqlx::query_as(
            "SELECT id, source_id, ref, title, summary, content, locked, source_doc_id, use_count, last_used_at, created_at, updated_at \
             FROM knowledge_pages WHERE source_id = ? AND ref = ?",
        )
        .bind(wiki_id)
        .bind(page_ref)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = existing else {
            return Ok(false);
        };
        let rowid: Option<(i64,)> =
            sqlx::query_as("SELECT rowid FROM knowledge_pages WHERE id = ?")
                .bind(&row.id)
                .fetch_optional(&self.pool)
                .await?;
        let mut tx = self.pool.begin().await?;
        if let Some((rid,)) = rowid {
            sqlx::query("DELETE FROM knowledge_pages_fts WHERE rowid = ?")
                .bind(rid)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("UPDATE knowledge_page_edges SET dst_page_id = NULL WHERE dst_page_id = ?")
            .bind(&row.id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM knowledge_page_edges WHERE src_page_id = ?")
            .bind(&row.id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM knowledge_pages WHERE id = ?")
            .bind(&row.id)
            .execute(&mut *tx)
            .await?;
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM knowledge_pages WHERE source_id = ?")
                .bind(wiki_id)
                .fetch_one(&mut *tx)
                .await?;
        sqlx::query("UPDATE knowledge_sources SET page_count = ?, version = version + 1, updated_at = datetime('now') WHERE id = ?")
            .bind(count.0)
            .bind(wiki_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// 清空某文档抽取的非 locked 页（reindex 用：清旧页+FTS+边）。
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn wiki_clear_pages_by_doc(
        &self,
        wiki_id: &str,
        doc_id: &str,
    ) -> Result<u64, sqlx::Error> {
        let rows: Vec<AgentWikiPageRecord> = sqlx::query_as(
            "SELECT id, source_id, ref, title, summary, content, locked, source_doc_id, use_count, last_used_at, created_at, updated_at \
             FROM knowledge_pages WHERE source_id = ? AND source_doc_id = ? AND locked = 0",
        )
        .bind(wiki_id)
        .bind(doc_id)
        .fetch_all(&self.pool)
        .await?;
        let mut deleted = 0u64;
        for row in rows {
            let rowid: Option<(i64,)> =
                sqlx::query_as("SELECT rowid FROM knowledge_pages WHERE id = ?")
                    .bind(&row.id)
                    .fetch_optional(&self.pool)
                    .await?;
            let mut tx = self.pool.begin().await?;
            if let Some((rid,)) = rowid {
                sqlx::query("DELETE FROM knowledge_pages_fts WHERE rowid = ?")
                    .bind(rid)
                    .execute(&mut *tx)
                    .await?;
            }
            sqlx::query("UPDATE knowledge_page_edges SET dst_page_id = NULL WHERE dst_page_id = ?")
                .bind(&row.id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM knowledge_page_edges WHERE src_page_id = ?")
                .bind(&row.id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM knowledge_pages WHERE id = ?")
                .bind(&row.id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            deleted += 1;
        }
        if deleted > 0 {
            let count: (i64,) =
                sqlx::query_as("SELECT COUNT(*) FROM knowledge_pages WHERE source_id = ?")
                    .bind(wiki_id)
                    .fetch_one(&self.pool)
                    .await?;
            sqlx::query("UPDATE knowledge_sources SET page_count = ?, version = version + 1, updated_at = datetime('now') WHERE id = ?")
                .bind(count.0)
                .bind(wiki_id)
                .execute(&self.pool)
                .await?;
        }
        Ok(deleted)
    }

    /// 页面列表（q/ref 前缀/locked 过滤+分页，摘要视图不含 content）。
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn wiki_list_pages(
        &self,
        wiki_id: &str,
        q: Option<&str>,
        ref_prefix: Option<&str>,
        locked: Option<bool>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<AgentWikiPageSummary>, sqlx::Error> {
        let mut qb = sqlx::QueryBuilder::new(
            "SELECT id, source_id, ref, title, summary, locked, source_doc_id, use_count, last_used_at, created_at, updated_at \
             FROM knowledge_pages WHERE source_id = ",
        );
        qb.push_bind(wiki_id);
        if let Some(q) = q.filter(|q| !q.is_empty()) {
            qb.push(" AND (title LIKE ")
                .push_bind(format!("%{q}%"))
                .push(" OR summary LIKE ")
                .push_bind(format!("%{q}%"))
                .push(" OR ref LIKE ")
                .push_bind(format!("%{q}%"))
                .push(")");
        }
        if let Some(prefix) = ref_prefix.filter(|p| !p.is_empty()) {
            qb.push(" AND ref LIKE ").push_bind(format!("{prefix}%"));
        }
        if let Some(locked) = locked {
            qb.push(" AND locked = ").push_bind(i64::from(locked));
        }
        qb.push(" ORDER BY updated_at DESC");
        qb.push(" LIMIT ")
            .push_bind(limit)
            .push(" OFFSET ")
            .push_bind(offset);
        qb.build_query_as::<AgentWikiPageSummary>()
            .fetch_all(&self.pool)
            .await
    }
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn wiki_bump_use(&self, page_id: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE knowledge_pages SET use_count = use_count + 1, last_used_at = datetime('now') WHERE id = ?",
        )
        .bind(page_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ── Graph ──────────────────────────────────────────────────────
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn wiki_graph(&self, wiki_id: &str) -> Result<WikiGraph, sqlx::Error> {
        let nodes: Vec<AgentWikiPageSummary> = sqlx::query_as(
            "SELECT id, source_id, ref, title, summary, locked, source_doc_id, use_count, last_used_at, created_at, updated_at \
             FROM knowledge_pages WHERE source_id = ? ORDER BY ref",
        )
        .bind(wiki_id)
        .fetch_all(&self.pool)
        .await?;
        let edges_raw: Vec<AgentWikiEdgeRecord> =
            sqlx::query_as("SELECT source_id, src_page_id, src_ref, dst_ref, dst_page_id FROM knowledge_page_edges WHERE source_id = ?")
                .bind(wiki_id)
                .fetch_all(&self.pool)
                .await?;
        let edges = edges_raw
            .into_iter()
            .map(|e| WikiGraphEdge {
                from: e.src_page_id,
                from_ref: e.src_ref,
                to: e.dst_page_id.clone(),
                to_ref: e.dst_ref,
                dangling: e.dst_page_id.is_none(),
            })
            .collect();
        Ok(WikiGraph { nodes, edges })
    }

    // ── FTS5 两段式检索 ──────────────────────────────────────────

    /// 转义 FTS5 MATCH 特殊字符（`"`、`*`、`-`、`:`），防 syntax error。
    fn escape_fts_query(q: &str) -> String {
        q.replace('"', "\"\"").replace(['*', ':', '-'], " ")
    }

    /// 判断是否触发 LIKE 回退：任一词 <3 字符则直接 LIKE（trigram <3 零命中）。
    fn needs_like_fallback(q: &str) -> bool {
        q.split_whitespace().any(|w| w.chars().count() < 3)
    }

    /// 两段式检索：在 `visible_wiki_ids` 范围内检索（空表示全局）。
    /// 1) 短词直接 LIKE；2) 否则 FTS MATCH + bm25，零命中回退 LIKE。
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn wiki_search(
        &self,
        visible_wiki_ids: &[String],
        query: &str,
        limit: i64,
    ) -> Result<Vec<WikiSearchHit>, sqlx::Error> {
        let q = query.trim();
        if q.is_empty() {
            return Ok(Vec::new());
        }
        if Self::needs_like_fallback(q) {
            return self.wiki_search_like(visible_wiki_ids, q, limit).await;
        }
        let escaped = Self::escape_fts_query(q);
        let hits = self
            .wiki_search_fts(visible_wiki_ids, &escaped, q, limit)
            .await;
        match hits {
            Ok(v) if !v.is_empty() => Ok(v),
            Ok(_) => self.wiki_search_like(visible_wiki_ids, q, limit).await,
            Err(e) if e.to_string().contains("syntax error") => {
                self.wiki_search_like(visible_wiki_ids, q, limit).await
            }
            Err(e) => Err(e),
        }
    }

    async fn wiki_search_like(
        &self,
        visible_wiki_ids: &[String],
        q: &str,
        limit: i64,
    ) -> Result<Vec<WikiSearchHit>, sqlx::Error> {
        let like = format!("%{q}%");
        if visible_wiki_ids.is_empty() {
            let rows: Vec<AgentWikiPageRecord> = sqlx::query_as(
                "SELECT id, source_id, ref, title, summary, content, locked, source_doc_id, use_count, last_used_at, created_at, updated_at \
                 FROM knowledge_pages WHERE title LIKE ? OR summary LIKE ? OR content LIKE ? OR ref LIKE ? LIMIT ?",
            )
            .bind(&like)
            .bind(&like)
            .bind(&like)
            .bind(&like)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
            return Ok(rows
                .into_iter()
                .map(|r| WikiSearchHit {
                    page_id: r.id,
                    wiki_id: r.wiki_id,
                    page_ref: r.page_ref,
                    title: r.title.clone(),
                    summary: r.summary.clone(),
                    snippet: r.summary.clone(),
                    rank: 0.0,
                })
                .collect());
        }
        let mut qb = sqlx::QueryBuilder::new(
            "SELECT id, source_id, ref, title, summary, content, locked, source_doc_id, use_count, last_used_at, created_at, updated_at \
             FROM knowledge_pages WHERE source_id IN (",
        );
        let mut sep = qb.separated(", ");
        for id in visible_wiki_ids {
            sep.push_bind(id);
        }
        qb.push(") AND (title LIKE ");
        qb.push_bind(like.clone());
        qb.push(" OR summary LIKE ").push_bind(like.clone());
        qb.push(" OR content LIKE ").push_bind(like.clone());
        qb.push(" OR ref LIKE ").push_bind(like);
        qb.push(") LIMIT ").push_bind(limit);
        let rows: Vec<AgentWikiPageRecord> = qb
            .build_query_as::<AgentWikiPageRecord>()
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| WikiSearchHit {
                page_id: r.id,
                wiki_id: r.wiki_id,
                page_ref: r.page_ref,
                title: r.title.clone(),
                summary: r.summary.clone(),
                snippet: r.summary.clone(),
                rank: 0.0,
            })
            .collect())
    }

    async fn wiki_search_fts(
        &self,
        visible_wiki_ids: &[String],
        escaped: &str,
        _original: &str,
        limit: i64,
    ) -> Result<Vec<WikiSearchHit>, sqlx::Error> {
        if visible_wiki_ids.is_empty() {
            #[allow(clippy::type_complexity)]
            let rows: Vec<(i64, String, String, String, String, String, String, f64, String)> = sqlx::query_as(
                "SELECT p.rowid, p.id, p.source_id, p.ref, p.title, p.summary, p.content, \
                 bm25(knowledge_pages_fts, 2.0, 1.0, 1.0, 0.5) AS rank, \
                 snippet(knowledge_pages_fts, 3, '<mark>', '</mark>', '…', 10) AS snippet \
                 FROM knowledge_pages_fts JOIN knowledge_pages p ON p.rowid = knowledge_pages_fts.rowid \
                 WHERE knowledge_pages_fts MATCH ? ORDER BY rank LIMIT ?",
            )
            .bind(escaped)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
            return Ok(rows
                .into_iter()
                .map(
                    |(_, id, wiki_id, r, title, summary, _content, rank, snippet)| WikiSearchHit {
                        page_id: id,
                        wiki_id,
                        page_ref: r,
                        title,
                        summary,
                        snippet,
                        rank,
                    },
                )
                .collect());
        }
        let mut qb = sqlx::QueryBuilder::new(
            "SELECT p.rowid, p.id, p.source_id, p.ref, p.title, p.summary, p.content, \
             bm25(knowledge_pages_fts, 2.0, 1.0, 1.0, 0.5) AS rank, \
             snippet(knowledge_pages_fts, 3, '<mark>', '</mark>', '…', 10) AS snippet \
             FROM knowledge_pages_fts JOIN knowledge_pages p ON p.rowid = knowledge_pages_fts.rowid \
             WHERE knowledge_pages_fts MATCH ",
        );
        qb.push_bind(escaped);
        qb.push(" AND p.source_id IN (");
        let mut sep = qb.separated(", ");
        for id in visible_wiki_ids {
            sep.push_bind(id);
        }
        qb.push(") ORDER BY rank LIMIT ").push_bind(limit);
        #[allow(clippy::type_complexity)]
        let rows: Vec<(
            i64,
            String,
            String,
            String,
            String,
            String,
            String,
            f64,
            String,
        )> = qb
            .build_query_as::<(
                i64,
                String,
                String,
                String,
                String,
                String,
                String,
                f64,
                String,
            )>()
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(
                |(_, id, wiki_id, r, title, summary, _content, rank, snippet)| {
                    let snippet = if snippet.is_empty() {
                        summary.clone()
                    } else {
                        snippet
                    };
                    WikiSearchHit {
                        page_id: id,
                        wiki_id,
                        page_ref: r,
                        title,
                        summary,
                        snippet,
                        rank,
                    }
                },
            )
            .collect())
    }
}

// ── Tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::{IndexKind, KsCreateOpts};

    fn pages_opts(id: &str, name: &str) -> KsCreateOpts {
        KsCreateOpts {
            id: id.to_owned(),
            name: name.to_owned(),
            summary: "summary".to_owned(),
            index_vector: false,
            index_pages: true,
            scope_type: "global".to_owned(),
            client_id: String::new(),
            workspace_id: String::new(),
            emb_base_url: String::new(),
            emb_api_key: String::new(),
            emb_model: String::new(),
            emb_dimension: 0,
            top_k: 5,
            chunk_size: 512,
            chunk_overlap: 64,
            score_threshold: 0.3,
            enabled: true,
        }
    }

    #[test]
    fn normalize_wiki_ref_ok() {
        assert_eq!(
            normalize_wiki_ref("Deploy/Prod-Checklist"),
            Some("deploy/prod-checklist".into())
        );
        assert_eq!(normalize_wiki_ref("  a_b-1  "), Some("a_b-1".into()));
        assert_eq!(normalize_wiki_ref("a"), Some("a".into()));
    }

    #[test]
    fn normalize_wiki_ref_rejects() {
        assert_eq!(normalize_wiki_ref(""), None);
        assert_eq!(normalize_wiki_ref("  "), None);
        assert_eq!(normalize_wiki_ref("-abc"), None);
        assert_eq!(normalize_wiki_ref("/abc"), None);
        assert_eq!(normalize_wiki_ref("a//b"), None);
        assert_eq!(normalize_wiki_ref("a/./b"), None);
        assert_eq!(normalize_wiki_ref("a/../b"), None);
        assert_eq!(normalize_wiki_ref("a b"), None);
        assert_eq!(normalize_wiki_ref("中文"), None);
        let long = "a".repeat(129);
        assert_eq!(normalize_wiki_ref(&long), None);
        assert_eq!(normalize_wiki_ref(&"a".repeat(128)), Some("a".repeat(128)));
    }

    #[test]
    fn parse_wiki_links_dedup() {
        let c = "看 [[deploy/prod]] 和 [[deploy/prod]] 还有 [[missing-ref]]";
        let links = parse_wiki_links(c);
        assert_eq!(links, vec!["deploy/prod", "missing-ref"]);
    }

    #[test]
    fn parse_wiki_links_empty() {
        assert!(parse_wiki_links("no links here").is_empty());
        assert!(parse_wiki_links("[[bad ref]]").is_empty());
    }

    #[tokio::test]
    async fn wiki_page_upsert_and_locked_and_fts_sync() {
        let db = Database::new(":memory:").await.unwrap();
        db.ks_create(&pages_opts("w1", "wiki")).await.unwrap();

        db.kdoc_create("doc1", "w1", "a.md", "md", "sha256:x")
            .await
            .unwrap();
        db.kdoc_create("doc2", "w1", "b.md", "md", "sha256:y")
            .await
            .unwrap();
        db.wiki_upsert_page(
            "w1",
            "deploy/prod",
            "部署",
            "摘要",
            "内容含 [[other/ref]]",
            false,
            Some("doc1"),
        )
        .await
        .unwrap();
        let hits = db.wiki_search(&["w1".into()], "部署", 10).await.unwrap();
        assert!(!hits.is_empty(), "FTS/LIKE 应命中");

        db.wiki_upsert_page(
            "w1",
            "deploy/prod",
            "部署-手动",
            "摘要2",
            "手动内容",
            true,
            None,
        )
        .await
        .unwrap();
        let p = db.wiki_get_page("w1", "deploy/prod").await.unwrap().unwrap();
        assert_eq!(p.title, "部署-手动");
        assert_eq!(p.locked, 1);

        db.wiki_upsert_page(
            "w1",
            "deploy/prod",
            "尝试覆盖",
            "x",
            "x",
            false,
            Some("doc2"),
        )
        .await
        .unwrap();
        let p = db.wiki_get_page("w1", "deploy/prod").await.unwrap().unwrap();
        assert_eq!(p.title, "部署-手动", "locked 页不应被 ingest 覆盖");

        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM knowledge_pages WHERE source_id = 'w1'")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(count, 1);

        let fts_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM knowledge_pages_fts")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(fts_count, count, "fts 命中数应等于正表页数");

        db.wiki_delete_page("w1", "deploy/prod").await.unwrap();
        let fts_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM knowledge_pages_fts")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(fts_count, 0);
        assert!(db.wiki_get_page("w1", "deploy/prod").await.unwrap().is_none());
        let _ = IndexKind::Pages.as_str();
    }

    #[tokio::test]
    async fn wiki_search_like_fallback_and_escape() {
        let db = Database::new(":memory:").await.unwrap();
        db.ks_create(&pages_opts("w1", "wiki")).await.unwrap();
        db.wiki_upsert_page("w1", "p1", "部署文档", "摘要", "这里是部署相关内容", false, None)
            .await
            .unwrap();
        db.wiki_upsert_page("w1", "p2", "其他", "摘要", "完全不相关的内容", false, None)
            .await
            .unwrap();

        let hits = db.wiki_search(&["w1".into()], "部署", 10).await.unwrap();
        assert!(hits.iter().any(|h| h.page_ref == "p1"), "2字查询应 LIKE 回退命中");

        let hits = db.wiki_search(&["w1".into()], "部署相关", 10).await.unwrap();
        assert!(hits.iter().any(|h| h.page_ref == "p1"));

        let hits = db.wiki_search(&["w1".into()], "\"*-:\"", 10).await.unwrap();
        let _ = hits;

        let hits = db
            .wiki_search(&["w1".into()], "不存在的词xyz", 10)
            .await
            .unwrap();
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn wiki_graph_dangling() {
        let db = Database::new(":memory:").await.unwrap();
        db.ks_create(&pages_opts("w1", "wiki")).await.unwrap();
        db.wiki_upsert_page("w1", "a", "A", "", "link to [[b]] and [[missing]]", false, None)
            .await
            .unwrap();
        db.wiki_upsert_page("w1", "b", "B", "", "no links", false, None)
            .await
            .unwrap();

        let g = db.wiki_graph("w1").await.unwrap();
        assert_eq!(g.nodes.len(), 2);
        let to_b = g.edges.iter().find(|e| e.to_ref == "b").unwrap();
        assert!(!to_b.dangling);
        assert!(to_b.to.is_some());
        let missing = g.edges.iter().find(|e| e.to_ref == "missing").unwrap();
        assert!(missing.dangling);
        assert!(missing.to.is_none());
    }

    #[tokio::test]
    async fn wiki_page_list_filters_and_bump() {
        let db = Database::new(":memory:").await.unwrap();
        db.ks_create(&pages_opts("w1", "wiki")).await.unwrap();
        db.wiki_upsert_page("w1", "deploy/a", "A", "s", "c", false, None)
            .await
            .unwrap();
        db.wiki_upsert_page("w1", "deploy/b", "B", "s", "c", true, None)
            .await
            .unwrap();
        db.wiki_upsert_page("w1", "other/c", "C", "s", "c", false, None)
            .await
            .unwrap();

        let list = db
            .wiki_list_pages("w1", None, Some("deploy/"), None, 10, 0)
            .await
            .unwrap();
        assert_eq!(list.len(), 2);
        let locked = db
            .wiki_list_pages("w1", None, None, Some(true), 10, 0)
            .await
            .unwrap();
        assert_eq!(locked.len(), 1);
        assert_eq!(locked[0].page_ref, "deploy/b");

        let p = db.wiki_get_page("w1", "deploy/a").await.unwrap().unwrap();
        db.wiki_bump_use(&p.id).await.unwrap();
        let p2 = db.wiki_get_page_by_id(&p.id).await.unwrap().unwrap();
        assert_eq!(p2.use_count, 1);
        assert!(p2.last_used_at.is_some());
    }

    #[tokio::test]
    async fn wiki_fts_rowid_coupling() {
        let db = Database::new(":memory:").await.unwrap();
        db.ks_create(&pages_opts("w1", "wiki")).await.unwrap();
        db.wiki_upsert_page("w1", "r1", "t", "s", "hello world unique123", false, None)
            .await
            .unwrap();
        // 更新同一 ref 应正确替换 FTS 行（旧 rowid 删除，新 rowid 插入）
        db.wiki_upsert_page("w1", "r1", "t2", "s2", "hello world unique123 updated", false, None)
            .await
            .unwrap();
        let hits = db.wiki_search(&["w1".into()], "unique123", 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "t2");
        let fts_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM knowledge_pages_fts")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        let page_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM knowledge_pages WHERE source_id = 'w1'")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(fts_count, page_count, "FTS 行数应与主表一致，无幽灵行");
    }

    #[tokio::test]
    async fn wiki_clear_pages_by_doc_keeps_locked() {
        let db = Database::new(":memory:").await.unwrap();
        db.ks_create(&pages_opts("w1", "wiki")).await.unwrap();
        db.kdoc_create("doc1", "w1", "a.md", "md", "sha256:x")
            .await
            .unwrap();
        db.wiki_upsert_page("w1", "a/p1", "P1", "s", "c", false, Some("doc1"))
            .await
            .unwrap();
        db.wiki_upsert_page("w1", "a/p2", "P2", "s", "c", true, Some("doc1"))
            .await
            .unwrap();
        let n = db.wiki_clear_pages_by_doc("w1", "doc1").await.unwrap();
        assert_eq!(n, 1, "仅非 locked 页被清理");
        assert!(db.wiki_get_page("w1", "a/p1").await.unwrap().is_none());
        assert!(db.wiki_get_page("w1", "a/p2").await.unwrap().is_some());
        let fts_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM knowledge_pages_fts")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(fts_count, 1);
    }
}
