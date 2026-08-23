//! Wiki 数据访问层：容器 / 文档 / 页面 / 边 / FTS5。
//!
//! 与 `memory`/`skills` 同层，对齐既有中文注释与 `clippy::pedantic` 风格。
//! 不触碰向量（FTS5 trigram 零 embedding 依赖），页面与边/FTS 同事务同步。

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
        // 显式归一化：禁非法 ref 与空串，长度/字符集由 normalize 统一把关
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

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct AgentWikiRecord {
    pub id: String,
    pub name: String,
    pub summary: String,
    pub status: String,
    pub version: i64,
    pub page_count: i64,
    pub scope_type: String,
    pub client_id: String,
    pub workspace_id: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct AgentWikiDocRecord {
    pub id: String,
    pub wiki_id: String,
    pub filename: String,
    pub file_type: String,
    pub content_hash: String,
    pub status: String,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct AgentWikiPageRecord {
    pub id: String,
    pub wiki_id: String,
    #[sqlx(rename = "ref")]
    pub page_ref: String,
    pub title: String,
    pub summary: String,
    pub content: String,
    pub locked: i64,
    pub source_doc_id: Option<String>,
    pub use_count: i64,
    pub last_used_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// 页面列表/摘要视图（不含 `content`，对齐 `AgentSkillSummary` 节省流量）。
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct AgentWikiPageSummary {
    pub id: String,
    pub wiki_id: String,
    #[sqlx(rename = "ref")]
    pub page_ref: String,
    pub title: String,
    pub summary: String,
    pub locked: i64,
    pub source_doc_id: Option<String>,
    pub use_count: i64,
    pub last_used_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct AgentWikiEdgeRecord {
    pub wiki_id: String,
    pub src_page_id: String,
    pub src_ref: String,
    pub dst_ref: String,
    pub dst_page_id: Option<String>,
}

/// FTS5 检索命中（`rank` 为 `bm25` 负分或 LIKE 回退的占位）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct WikiSearchHit {
    pub page_id: String,
    pub wiki_id: String,
    #[serde(rename = "ref")]
    pub page_ref: String,
    pub title: String,
    pub summary: String,
    pub snippet: String,
    pub rank: f64,
}

/// Graph 响应：`nodes` 为页面摘要，`edges` 为有向边（含悬空标记）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct WikiGraph {
    pub nodes: Vec<AgentWikiPageSummary>,
    pub edges: Vec<WikiGraphEdge>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct WikiGraphEdge {
    pub from: String,
    pub from_ref: String,
    pub to: Option<String>,
    pub to_ref: String,
    pub dangling: bool,
}

// ── 容器 CRUD ───────────────────────────────────────────────────

impl Database {
/// # Errors
/// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn wiki_create(
        &self,
        id: &str,
        name: &str,
        summary: &str,
        scope_type: &str,
        client_id: &str,
        workspace_id: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO agent_wikis (id, name, summary, scope_type, client_id, workspace_id)
               VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(name)
        .bind(summary)
        .bind(scope_type)
        .bind(client_id)
        .bind(workspace_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
/// # Errors
/// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn wiki_get(&self, id: &str) -> Result<Option<AgentWikiRecord>, sqlx::Error> {
        sqlx::query_as::<_, AgentWikiRecord>("SELECT * FROM agent_wikis WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
    }
/// # Errors
/// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn wiki_get_by_name_scope(
        &self,
        name: &str,
        scope_type: &str,
        client_id: &str,
        workspace_id: &str,
    ) -> Result<Option<AgentWikiRecord>, sqlx::Error> {
        sqlx::query_as::<_, AgentWikiRecord>(
            "SELECT * FROM agent_wikis WHERE name = ? AND scope_type = ? AND client_id = ? AND workspace_id = ?",
        )
        .bind(name)
        .bind(scope_type)
        .bind(client_id)
        .bind(workspace_id)
        .fetch_optional(&self.pool)
        .await
    }
/// # Errors
/// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn wiki_update(
        &self,
        id: &str,
        name: &str,
        summary: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE agent_wikis SET name = ?, summary = ?, updated_at = datetime('now') WHERE id = ?",
        )
        .bind(name)
        .bind(summary)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
/// # Errors
/// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn wiki_delete(&self, id: &str) -> Result<(), sqlx::Error> {
        // Pages/Docs/Edges 经 FK CASCADE 清理；FTS 残留需显式清理（rowid 无 FK）
        let page_ids: Vec<(String,)> =
            sqlx::query_as("SELECT id FROM agent_wiki_pages WHERE wiki_id = ?")
                .bind(id)
                .fetch_all(&self.pool)
                .await?;
        for (pid,) in &page_ids {
            // 取 rowid 删 FTS
            let rowid: Option<(i64,)> =
                sqlx::query_as("SELECT rowid FROM agent_wiki_pages WHERE id = ?")
                    .bind(pid)
                    .fetch_optional(&self.pool)
                    .await?;
            if let Some((rid,)) = rowid {
                let _ = sqlx::query("DELETE FROM agent_wiki_pages_fts WHERE rowid = ?")
                    .bind(rid)
                    .execute(&self.pool)
                    .await;
            }
        }
        sqlx::query("DELETE FROM agent_wikis WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 列表（作用域 / q / status 过滤 + 分页）。`scope_type` 精确过滤；空串不过滤。
/// # Errors
/// 数据库错误：以 `sqlx::Error` 返回。
    #[allow(clippy::too_many_arguments)]
    pub async fn wiki_list(
        &self,
        scope_type: Option<&str>,
        client_id: Option<&str>,
        workspace_id: Option<&str>,
        q: Option<&str>,
        status: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<AgentWikiRecord>, sqlx::Error> {
        let mut qb = sqlx::QueryBuilder::new("SELECT * FROM agent_wikis WHERE 1=1");
        if let Some(s) = scope_type.filter(|s| !s.is_empty()) {
            qb.push(" AND scope_type = ").push_bind(s);
        }
        if let Some(c) = client_id.filter(|c| !c.is_empty()) {
            qb.push(" AND client_id = ").push_bind(c);
        }
        if let Some(w) = workspace_id.filter(|w| !w.is_empty()) {
            qb.push(" AND workspace_id = ").push_bind(w);
        }
        if let Some(q) = q.filter(|q| !q.is_empty()) {
            qb.push(" AND (name LIKE ")
                .push_bind(format!("%{q}%"))
                .push(" OR summary LIKE ")
                .push_bind(format!("%{q}%"))
                .push(")");
        }
        if let Some(st) = status.filter(|s| !s.is_empty()) {
            qb.push(" AND status = ").push_bind(st);
        }
        qb.push(" ORDER BY updated_at DESC");
        qb.push(" LIMIT ").push_bind(limit).push(" OFFSET ").push_bind(offset);
        qb.build_query_as::<AgentWikiRecord>()
            .fetch_all(&self.pool)
            .await
    }
/// # Errors
/// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn wiki_count(
        &self,
        scope_type: Option<&str>,
        client_id: Option<&str>,
        workspace_id: Option<&str>,
        q: Option<&str>,
        status: Option<&str>,
    ) -> Result<i64, sqlx::Error> {
        let mut qb = sqlx::QueryBuilder::new("SELECT COUNT(*) FROM agent_wikis WHERE 1=1");
        if let Some(s) = scope_type.filter(|s| !s.is_empty()) {
            qb.push(" AND scope_type = ").push_bind(s);
        }
        if let Some(c) = client_id.filter(|c| !c.is_empty()) {
            qb.push(" AND client_id = ").push_bind(c);
        }
        if let Some(w) = workspace_id.filter(|w| !w.is_empty()) {
            qb.push(" AND workspace_id = ").push_bind(w);
        }
        if let Some(q) = q.filter(|q| !q.is_empty()) {
            qb.push(" AND (name LIKE ")
                .push_bind(format!("%{q}%"))
                .push(" OR summary LIKE ")
                .push_bind(format!("%{q}%"))
                .push(")");
        }
        if let Some(st) = status.filter(|s| !s.is_empty()) {
            qb.push(" AND status = ").push_bind(st);
        }
        qb.build_query_scalar::<i64>().fetch_one(&self.pool).await
    }

    /// 对账：把 `pending`/`processing` 的 wiki 置为 `failed`（启动时调用）。
/// # Errors
/// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn wiki_fail_inflight(&self, error: &str) -> Result<u64, sqlx::Error> {
        let _ = error; // 容器无 error 列，仅复位状态（参数保留与 docs 对账签名一致）
        let r = sqlx::query(
            "UPDATE agent_wikis SET status = 'failed', updated_at = datetime('now') WHERE status IN ('pending','processing')",
        )
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// 容器状态更新（摄入管线用：processing/ready/failed）。
/// # Errors
/// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn wiki_update_status(&self, id: &str, status: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE agent_wikis SET status = ?, updated_at = datetime('now') WHERE id = ?")
            .bind(status)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 原子 CAS：仅当 `pending` → `processing` 可抢占（摄入入口）。
/// # Errors
/// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn wiki_mark_doc_processing_if_pending(&self, id: &str) -> Result<bool, sqlx::Error> {
        let r = sqlx::query(
            "UPDATE agent_wiki_docs SET status = 'processing', updated_at = datetime('now') WHERE id = ? AND status = 'pending'",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() > 0)
    }

    // ── 文档 CRUD ────────────────────────────────────────────────
/// # Errors
/// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn wiki_create_doc(
        &self,
        id: &str,
        wiki_id: &str,
        filename: &str,
        file_type: &str,
        content_hash: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO agent_wiki_docs (id, wiki_id, filename, file_type, content_hash) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(wiki_id)
        .bind(filename)
        .bind(file_type)
        .bind(content_hash)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
/// # Errors
/// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn wiki_get_doc(&self, id: &str) -> Result<Option<AgentWikiDocRecord>, sqlx::Error> {
        sqlx::query_as::<_, AgentWikiDocRecord>("SELECT * FROM agent_wiki_docs WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
    }
/// # Errors
/// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn wiki_list_docs(&self, wiki_id: &str) -> Result<Vec<AgentWikiDocRecord>, sqlx::Error> {
        sqlx::query_as::<_, AgentWikiDocRecord>(
            "SELECT * FROM agent_wiki_docs WHERE wiki_id = ? ORDER BY created_at",
        )
        .bind(wiki_id)
        .fetch_all(&self.pool)
        .await
    }
/// # Errors
/// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn wiki_delete_doc(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM agent_wiki_docs WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        // source_doc_id SET NULL 由 FK 负责，无需额外处理
        Ok(())
    }
/// # Errors
/// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn wiki_update_doc_status(
        &self,
        id: &str,
        status: &str,
        error: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE agent_wiki_docs SET status = ?, error = ?, updated_at = datetime('now') WHERE id = ?",
        )
        .bind(status)
        .bind(error)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// CAS：仅当 `pending`→`processing` 或 `ready`/`failed`→`pending` 的空闲态可抢占。
/// # Errors
/// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn wiki_mark_doc_pending_if_idle(&self, id: &str) -> Result<bool, sqlx::Error> {
        let r = sqlx::query(
            "UPDATE agent_wiki_docs SET status = 'pending', error = NULL, updated_at = datetime('now') \
             WHERE id = ? AND status NOT IN ('pending','processing')",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() > 0)
    }
/// # Errors
/// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn wiki_fail_inflight_docs(&self, error: &str) -> Result<u64, sqlx::Error> {
        let r = sqlx::query(
            "UPDATE agent_wiki_docs SET status = 'failed', error = ?, updated_at = datetime('now') \
             WHERE status IN ('pending','processing')",
        )
        .bind(error)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    // ── 页面：upsert（正表+FTS+边 同事务）、delete、列表、bump_use ───────

    /// 页面 upsert：`locked=1` 的页不被覆盖；同事务同步 FTS 与边。
    /// `source_doc_id` 可空（手动页为 `None`）。
/// # Errors
/// 数据库错误：以 `sqlx::Error` 返回。
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
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
        // 检查是否 locked
        let existing: Option<AgentWikiPageRecord> = sqlx::query_as(
            "SELECT * FROM agent_wiki_pages WHERE wiki_id = ? AND ref = ?",
        )
        .bind(wiki_id)
        .bind(page_ref)
        .fetch_optional(&self.pool)
        .await?;
        if let Some(ref row) = existing {
            if row.locked != 0 && !locked {
                // ingest 不覆盖 locked 页：静默跳过，返回既有 id
                return Ok(row.id.clone());
            }
        }

        // 取旧 rowid（FTS 删除用）
        let old_rowid: Option<(i64,)> = if let Some(ref row) = existing {
            sqlx::query_as("SELECT rowid FROM agent_wiki_pages WHERE id = ?")
                .bind(&row.id)
                .fetch_optional(&self.pool)
                .await?
        } else {
            None
        };

        let page_id = existing
            .as_ref()
            .map_or_else(|| format!("{:032x}", rand::random::<u128>()), |r| r.id.clone());

        let links = parse_wiki_links(content);

        let mut tx = self.pool.begin().await?;

        if existing.is_some() {
            sqlx::query(
                r"UPDATE agent_wiki_pages
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
                sqlx::query("DELETE FROM agent_wiki_pages_fts WHERE rowid = ?")
                    .bind(rid)
                    .execute(&mut *tx)
                    .await?;
            }
            // 删旧边（重建）
            sqlx::query("DELETE FROM agent_wiki_edges WHERE src_page_id = ?")
                .bind(&page_id)
                .execute(&mut *tx)
                .await?;
        } else {
            sqlx::query(
                r"INSERT INTO agent_wiki_pages (id, wiki_id, ref, title, summary, content, locked, source_doc_id)
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

        // 取 rowid 插 FTS
        let rowid: (i64,) = sqlx::query_as("SELECT rowid FROM agent_wiki_pages WHERE id = ?")
            .bind(&page_id)
            .fetch_one(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO agent_wiki_pages_fts (rowid, ref, title, summary, content) VALUES (?, ?, ?, ?, ?)")
            .bind(rowid.0)
            .bind(page_ref)
            .bind(title)
            .bind(summary)
            .bind(content)
            .execute(&mut *tx)
            .await?;

        // 插边（dst_page_id 回填：同 wiki 下 ref 存在则填 id，否则 NULL 悬空）
        for dst_ref in &links {
            let dst: Option<(String,)> =
                sqlx::query_as("SELECT id FROM agent_wiki_pages WHERE wiki_id = ? AND ref = ?")
                    .bind(wiki_id)
                    .bind(dst_ref)
                    .fetch_optional(&mut *tx)
                    .await?;
            sqlx::query(
                "INSERT OR REPLACE INTO agent_wiki_edges (wiki_id, src_page_id, src_ref, dst_ref, dst_page_id) \
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

        // 入边回填：其它页指向本页的悬空边，现在可解析了
        sqlx::query(
            "UPDATE agent_wiki_edges SET dst_page_id = ? WHERE wiki_id = ? AND dst_ref = ? AND dst_page_id IS NULL",
        )
        .bind(&page_id)
        .bind(wiki_id)
        .bind(page_ref)
        .execute(&mut *tx)
        .await?;

        // 更新容器 page_count/version
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM agent_wiki_pages WHERE wiki_id = ?")
            .bind(wiki_id)
            .fetch_one(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE agent_wikis SET page_count = ?, version = version + 1, updated_at = datetime('now') WHERE id = ?",
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
            "SELECT * FROM agent_wiki_pages WHERE wiki_id = ? AND ref = ?",
        )
        .bind(wiki_id)
        .bind(page_ref)
        .fetch_optional(&self.pool)
        .await
    }
/// # Errors
/// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn wiki_get_page_by_id(&self, id: &str) -> Result<Option<AgentWikiPageRecord>, sqlx::Error> {
        sqlx::query_as::<_, AgentWikiPageRecord>("SELECT * FROM agent_wiki_pages WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
    }

    /// 删除页面：同事务清 FTS、出边与入边悬空化。
/// # Errors
/// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn wiki_delete_page(&self, wiki_id: &str, page_ref: &str) -> Result<bool, sqlx::Error> {
        let existing: Option<AgentWikiPageRecord> = sqlx::query_as(
            "SELECT * FROM agent_wiki_pages WHERE wiki_id = ? AND ref = ?",
        )
        .bind(wiki_id)
        .bind(page_ref)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = existing else {
            return Ok(false);
        };
        let rowid: Option<(i64,)> =
            sqlx::query_as("SELECT rowid FROM agent_wiki_pages WHERE id = ?")
                .bind(&row.id)
                .fetch_optional(&self.pool)
                .await?;
        let mut tx = self.pool.begin().await?;
        if let Some((rid,)) = rowid {
            sqlx::query("DELETE FROM agent_wiki_pages_fts WHERE rowid = ?")
                .bind(rid)
                .execute(&mut *tx)
                .await?;
        }
        // 入边悬空化（dst_page_id 置空，保留虚线）
        sqlx::query("UPDATE agent_wiki_edges SET dst_page_id = NULL WHERE dst_page_id = ?")
            .bind(&row.id)
            .execute(&mut *tx)
            .await?;
        // 出边随 FK CASCADE 清理，但显式删一次更稳妥
        sqlx::query("DELETE FROM agent_wiki_edges WHERE src_page_id = ?")
            .bind(&row.id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM agent_wiki_pages WHERE id = ?")
            .bind(&row.id)
            .execute(&mut *tx)
            .await?;
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM agent_wiki_pages WHERE wiki_id = ?")
            .bind(wiki_id)
            .fetch_one(&mut *tx)
            .await?;
        sqlx::query("UPDATE agent_wikis SET page_count = ?, version = version + 1, updated_at = datetime('now') WHERE id = ?")
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
    pub async fn wiki_clear_pages_by_doc(&self, wiki_id: &str, doc_id: &str) -> Result<u64, sqlx::Error> {
        let rows: Vec<AgentWikiPageRecord> = sqlx::query_as(
            "SELECT * FROM agent_wiki_pages WHERE wiki_id = ? AND source_doc_id = ? AND locked = 0",
        )
        .bind(wiki_id)
        .bind(doc_id)
        .fetch_all(&self.pool)
        .await?;
        let mut deleted = 0u64;
        for row in rows {
            let rowid: Option<(i64,)> =
                sqlx::query_as("SELECT rowid FROM agent_wiki_pages WHERE id = ?")
                    .bind(&row.id)
                    .fetch_optional(&self.pool)
                    .await?;
            let mut tx = self.pool.begin().await?;
            if let Some((rid,)) = rowid {
                sqlx::query("DELETE FROM agent_wiki_pages_fts WHERE rowid = ?")
                    .bind(rid)
                    .execute(&mut *tx)
                    .await?;
            }
            sqlx::query("UPDATE agent_wiki_edges SET dst_page_id = NULL WHERE dst_page_id = ?")
                .bind(&row.id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM agent_wiki_edges WHERE src_page_id = ?")
                .bind(&row.id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM agent_wiki_pages WHERE id = ?")
                .bind(&row.id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            deleted += 1;
        }
        if deleted > 0 {
            let count: (i64,) =
                sqlx::query_as("SELECT COUNT(*) FROM agent_wiki_pages WHERE wiki_id = ?")
                    .bind(wiki_id)
                    .fetch_one(&self.pool)
                    .await?;
            sqlx::query("UPDATE agent_wikis SET page_count = ?, version = version + 1, updated_at = datetime('now') WHERE id = ?")
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
            "SELECT id, wiki_id, ref, title, summary, locked, source_doc_id, use_count, last_used_at, created_at, updated_at \
             FROM agent_wiki_pages WHERE wiki_id = ",
        );
        qb.push_bind(wiki_id);
        if let Some(q) = q.filter(|q| !q.is_empty()) {
            qb.push(" AND (title LIKE ").push_bind(format!("%{q}%"))
                .push(" OR summary LIKE ").push_bind(format!("%{q}%"))
                .push(" OR ref LIKE ").push_bind(format!("%{q}%"))
                .push(")");
        }
        if let Some(prefix) = ref_prefix.filter(|p| !p.is_empty()) {
            qb.push(" AND ref LIKE ").push_bind(format!("{prefix}%"));
        }
        if let Some(locked) = locked {
            qb.push(" AND locked = ").push_bind(i64::from(locked));
        }
        qb.push(" ORDER BY updated_at DESC");
        qb.push(" LIMIT ").push_bind(limit).push(" OFFSET ").push_bind(offset);
        qb.build_query_as::<AgentWikiPageSummary>()
            .fetch_all(&self.pool)
            .await
    }
/// # Errors
/// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn wiki_bump_use(&self, page_id: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE agent_wiki_pages SET use_count = use_count + 1, last_used_at = datetime('now') WHERE id = ?",
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
            "SELECT id, wiki_id, ref, title, summary, locked, source_doc_id, use_count, last_used_at, created_at, updated_at \
             FROM agent_wiki_pages WHERE wiki_id = ? ORDER BY ref",
        )
        .bind(wiki_id)
        .fetch_all(&self.pool)
        .await?;
        let edges_raw: Vec<AgentWikiEdgeRecord> =
            sqlx::query_as("SELECT * FROM agent_wiki_edges WHERE wiki_id = ?")
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
        // 短词或特殊场景：直接 LIKE
        if Self::needs_like_fallback(q) {
            return self.wiki_search_like(visible_wiki_ids, q, limit).await;
        }
        // FTS 路径
        let escaped = Self::escape_fts_query(q);
        let hits = self.wiki_search_fts(visible_wiki_ids, &escaped, q, limit).await;
        match hits {
            Ok(v) if !v.is_empty() => Ok(v),
            Ok(_) => self.wiki_search_like(visible_wiki_ids, q, limit).await,
            Err(e) if e.to_string().contains("syntax error") => {
                // MATCH 语法错误（如全特殊字符）→ LIKE 回退
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
        // 全量 LIKE：不走 FTS，snippet 用 summary 回退
        if visible_wiki_ids.is_empty() {
            let rows: Vec<AgentWikiPageRecord> = sqlx::query_as(
                "SELECT * FROM agent_wiki_pages WHERE title LIKE ? OR summary LIKE ? OR content LIKE ? OR ref LIKE ? LIMIT ?",
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
        // 指定 wiki 集合：IN 过滤
        let mut qb = sqlx::QueryBuilder::new(
            "SELECT * FROM agent_wiki_pages WHERE wiki_id IN (",
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
        // 权重：ref 2.0, title 1.0, summary 1.0, content 0.5
        // snippet 取 content 列（第 3 列，0-indexed 为 3）高亮
        if visible_wiki_ids.is_empty() {
            #[allow(clippy::type_complexity)]
            let rows: Vec<(i64, String, String, String, String, String, String, f64, String)> = sqlx::query_as(
                "SELECT p.rowid, p.id, p.wiki_id, p.ref, p.title, p.summary, p.content, \
                 bm25(agent_wiki_pages_fts, 2.0, 1.0, 1.0, 0.5) AS rank, \
                 snippet(agent_wiki_pages_fts, 3, '<mark>', '</mark>', '…', 10) AS snippet \
                 FROM agent_wiki_pages_fts JOIN agent_wiki_pages p ON p.rowid = agent_wiki_pages_fts.rowid \
                 WHERE agent_wiki_pages_fts MATCH ? ORDER BY rank LIMIT ?",
            )
            .bind(escaped)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
            return Ok(rows
                .into_iter()
                .map(|(_, id, wiki_id, r, title, summary, _content, rank, snippet)| WikiSearchHit {
                    page_id: id,
                    wiki_id,
                    page_ref: r,
                    title,
                    summary,
                    snippet,
                    rank,
                })
                .collect());
        }
        let mut qb = sqlx::QueryBuilder::new(
            "SELECT p.rowid, p.id, p.wiki_id, p.ref, p.title, p.summary, p.content, \
             bm25(agent_wiki_pages_fts, 2.0, 1.0, 1.0, 0.5) AS rank, \
             snippet(agent_wiki_pages_fts, 3, '<mark>', '</mark>', '…', 10) AS snippet \
             FROM agent_wiki_pages_fts JOIN agent_wiki_pages p ON p.rowid = agent_wiki_pages_fts.rowid \
             WHERE agent_wiki_pages_fts MATCH ",
        );
        qb.push_bind(escaped);
        qb.push(" AND p.wiki_id IN (");
        let mut sep = qb.separated(", ");
        for id in visible_wiki_ids {
            sep.push_bind(id);
        }
        qb.push(") ORDER BY rank LIMIT ").push_bind(limit);
        #[allow(clippy::type_complexity)]
        let rows: Vec<(i64, String, String, String, String, String, String, f64, String)> = qb
            .build_query_as::<(i64, String, String, String, String, String, String, f64, String)>()
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|(_, id, wiki_id, r, title, summary, _content, rank, snippet)| {
                let snippet = if snippet.is_empty() { summary.clone() } else { snippet };
                WikiSearchHit {
                    page_id: id,
                    wiki_id,
                    page_ref: r,
                    title,
                    summary,
                    snippet,
                    rank,
                }
            })
            .collect())
    }

    /// 可见 wiki 列表（scope 过滤，对齐 `skill_injectable` 的可见性语义）。
/// # Errors
/// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn wiki_visible_ids(
        &self,
        client_id: &str,
        workspace_id: &str,
    ) -> Result<Vec<String>, sqlx::Error> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT id FROM agent_wikis WHERE scope_type = 'global' \
             OR (scope_type = 'client' AND client_id = ?) \
             OR (scope_type = 'workspace' AND client_id = ? AND workspace_id = ?)",
        )
        .bind(client_id)
        .bind(client_id)
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    /// 可见 wiki 完整记录（清单注入用）：scope 过滤 + page_count DESC + limit。
    /// 与 [`Self::wiki_visible_ids`] 同一可见性语义，返回整行（name/summary/
    /// page_count 供 `<wikis>` 清单渲染）。
/// # Errors
/// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn wiki_visible_wikis(
        &self,
        client_id: &str,
        workspace_id: &str,
        limit: i64,
    ) -> Result<Vec<AgentWikiRecord>, sqlx::Error> {
        sqlx::query_as::<_, AgentWikiRecord>(
            "SELECT * FROM agent_wikis WHERE scope_type = 'global' \
             OR (scope_type = 'client' AND client_id = ?) \
             OR (scope_type = 'workspace' AND client_id = ? AND workspace_id = ?) \
             ORDER BY page_count DESC LIMIT ?",
        )
        .bind(client_id)
        .bind(client_id)
        .bind(workspace_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
    }

    /// 大小写不敏感的同作用域容器名查找（agent 工具寻址用）。SQLite LIKE 对
    /// ASCII 不区分大小写，中文不受影响；`name` 必须已由调用方 normalize。
/// # Errors
/// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn wiki_get_by_name_scope_ci(
        &self,
        name_lower: &str,
        scope_type: &str,
        client_id: &str,
        workspace_id: &str,
    ) -> Result<Option<AgentWikiRecord>, sqlx::Error> {
        // 精确命中优先（绝大多数场景 name 本就小写），再回退大小写不敏感匹配。
        if let Some(row) = self
            .wiki_get_by_name_scope(name_lower, scope_type, client_id, workspace_id)
            .await?
        {
            return Ok(Some(row));
        }
        sqlx::query_as::<_, AgentWikiRecord>(
            "SELECT * FROM agent_wikis WHERE lower(name) = lower(?1) \
             AND scope_type = ?2 AND client_id = ?3 AND workspace_id = ?4",
        )
        .bind(name_lower)
        .bind(scope_type)
        .bind(client_id)
        .bind(workspace_id)
        .fetch_optional(&self.pool)
        .await
    }
}

// ── Tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_wiki_ref_ok() {
        assert_eq!(normalize_wiki_ref("Deploy/Prod-Checklist"), Some("deploy/prod-checklist".into()));
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
    async fn wiki_container_crud_and_unique() {
        let db = Database::new(":memory:").await.unwrap();
        db.wiki_create("w1", "my-wiki", "desc", "workspace", "c1", "ws1").await.unwrap();
        let w = db.wiki_get("w1").await.unwrap().unwrap();
        assert_eq!(w.name, "my-wiki");
        assert_eq!(w.scope_type, "workspace");
        assert_eq!(w.page_count, 0);
        assert_eq!(w.version, 1);

        // 同名同 scope 冲突
        let dup = db.wiki_create("w2", "my-wiki", "", "workspace", "c1", "ws1").await;
        assert!(dup.is_err(), "同名同 scope 应唯一冲突");

        // 异 scope 可同名
        db.wiki_create("w3", "my-wiki", "", "global", "", "").await.unwrap();

        db.wiki_update("w1", "renamed", "new summary").await.unwrap();
        let w = db.wiki_get("w1").await.unwrap().unwrap();
        assert_eq!(w.name, "renamed");

        let list = db.wiki_list(None, None, None, None, None, 10, 0).await.unwrap();
        assert_eq!(list.len(), 2);

        let filtered = db.wiki_list(Some("global"), None, None, None, None, 10, 0).await.unwrap();
        assert_eq!(filtered.len(), 1);

        db.wiki_delete("w1").await.unwrap();
        assert!(db.wiki_get("w1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn wiki_page_upsert_and_locked_and_fts_sync() {
        let db = Database::new(":memory:").await.unwrap();
        db.wiki_create("w1", "wiki", "", "global", "", "").await.unwrap();

        db.wiki_create_doc("doc1", "w1", "a.md", "md", "sha256:x").await.unwrap();
        db.wiki_create_doc("doc2", "w1", "b.md", "md", "sha256:y").await.unwrap();
        // ingest 页（非 locked）
        db.wiki_upsert_page("w1", "deploy/prod", "部署", "摘要", "内容含 [[other/ref]]", false, Some("doc1"))
            .await.unwrap();
        // FTS 应命中
        let hits = db.wiki_search(&["w1".into()], "部署", 10).await.unwrap();
        assert!(!hits.is_empty(), "FTS/LIKE 应命中");

        // 手动页（locked=1）覆盖
        db.wiki_upsert_page("w1", "deploy/prod", "部署-手动", "摘要2", "手动内容", true, None)
            .await.unwrap();
        let p = db.wiki_get_page("w1", "deploy/prod").await.unwrap().unwrap();
        assert_eq!(p.title, "部署-手动");
        assert_eq!(p.locked, 1);

        // ingest 再次尝试覆盖 locked 页：应被保护
        db.wiki_upsert_page("w1", "deploy/prod", "尝试覆盖", "x", "x", false, Some("doc2"))
            .await.unwrap();
        let p = db.wiki_get_page("w1", "deploy/prod").await.unwrap().unwrap();
        assert_eq!(p.title, "部署-手动", "locked 页不应被 ingest 覆盖");

        // UNIQUE(wiki_id, ref) 冲突走 upsert，不会重复行
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_wiki_pages WHERE wiki_id = 'w1'")
            .fetch_one(&db.pool).await.unwrap();
        assert_eq!(count, 1);

        // FTS 同步一致性：fts 命中数 == 正表页数
        let fts_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_wiki_pages_fts")
            .fetch_one(&db.pool).await.unwrap();
        assert_eq!(fts_count, count, "fts 命中数应等于正表页数");

        // 删除后一致性
        db.wiki_delete_page("w1", "deploy/prod").await.unwrap();
        let fts_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_wiki_pages_fts")
            .fetch_one(&db.pool).await.unwrap();
        assert_eq!(fts_count, 0);
        assert!(db.wiki_get_page("w1", "deploy/prod").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn wiki_search_like_fallback_and_escape() {
        let db = Database::new(":memory:").await.unwrap();
        db.wiki_create("w1", "wiki", "", "global", "", "").await.unwrap();
        db.wiki_upsert_page("w1", "p1", "部署文档", "摘要", "这里是部署相关内容", false, None).await.unwrap();
        db.wiki_upsert_page("w1", "p2", "其他", "摘要", "完全不相关的内容", false, None).await.unwrap();

        // 2 字中文：trigram LIKE 回退应命中
        let hits = db.wiki_search(&["w1".into()], "部署", 10).await.unwrap();
        assert!(hits.iter().any(|h| h.page_ref == "p1"), "2字查询应 LIKE 回退命中");

        // 3+ 字 FTS 命中（若 trigram 支持）
        let hits = db.wiki_search(&["w1".into()], "部署相关", 10).await.unwrap();
        // 至少 LIKE 回退也能命中
        assert!(hits.iter().any(|h| h.page_ref == "p1"));

        // 特殊字符转义不报错
        let hits = db.wiki_search(&["w1".into()], "\"*-:\"", 10).await.unwrap();
        // 不应 panic，返回空或 LIKE 结果均可
        let _ = hits;

        // MATCH 零命中也回退 LIKE
        let hits = db.wiki_search(&["w1".into()], "不存在的词xyz", 10).await.unwrap();
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn wiki_graph_dangling() {
        let db = Database::new(":memory:").await.unwrap();
        db.wiki_create("w1", "wiki", "", "global", "", "").await.unwrap();
        db.wiki_upsert_page("w1", "a", "A", "", "link to [[b]] and [[missing]]", false, None).await.unwrap();
        db.wiki_upsert_page("w1", "b", "B", "", "no links", false, None).await.unwrap();

        let g = db.wiki_graph("w1").await.unwrap();
        assert_eq!(g.nodes.len(), 2);
        // a→b 已解析，a→missing 悬空
        let to_b = g.edges.iter().find(|e| e.to_ref == "b").unwrap();
        assert!(!to_b.dangling);
        assert!(to_b.to.is_some());
        let missing = g.edges.iter().find(|e| e.to_ref == "missing").unwrap();
        assert!(missing.dangling);
        assert!(missing.to.is_none());
    }

    #[tokio::test]
    async fn wiki_visible_ids_scope_filter() {
        let db = Database::new(":memory:").await.unwrap();
        db.wiki_create("g1", "global-wiki", "", "global", "", "").await.unwrap();
        db.wiki_create("c1", "client-wiki", "", "client", "c1", "").await.unwrap();
        db.wiki_create("w1", "ws-wiki", "", "workspace", "c1", "w1").await.unwrap();
        db.wiki_create("w2", "other-ws", "", "workspace", "c1", "w2").await.unwrap();

        let ids = db.wiki_visible_ids("c1", "w1").await.unwrap();
        assert!(ids.contains(&"g1".to_string()));
        assert!(ids.contains(&"c1".to_string()));
        assert!(ids.contains(&"w1".to_string()));
        assert!(!ids.contains(&"w2".to_string()));
    }

    #[tokio::test]
    async fn wiki_doc_cas_and_inflight() {
        let db = Database::new(":memory:").await.unwrap();
        db.wiki_create("w1", "wiki", "", "global", "", "").await.unwrap();
        db.wiki_create_doc("d1", "w1", "a.md", "md", "sha256:x").await.unwrap();

        // pending 状态：CAS 不抢
        assert!(!db.wiki_mark_doc_pending_if_idle("d1").await.unwrap());
        db.wiki_update_doc_status("d1", "ready", None).await.unwrap();
        assert!(db.wiki_mark_doc_pending_if_idle("d1").await.unwrap());
        assert_eq!(db.wiki_get_doc("d1").await.unwrap().unwrap().status, "pending");

        // 对账
        let n = db.wiki_fail_inflight_docs("restart").await.unwrap();
        assert_eq!(n, 1);
        assert_eq!(db.wiki_get_doc("d1").await.unwrap().unwrap().status, "failed");
    }

    #[tokio::test]
    async fn wiki_page_list_filters_and_bump() {
        let db = Database::new(":memory:").await.unwrap();
        db.wiki_create("w1", "wiki", "", "global", "", "").await.unwrap();
        db.wiki_upsert_page("w1", "deploy/a", "A", "s", "c", false, None).await.unwrap();
        db.wiki_upsert_page("w1", "deploy/b", "B", "s", "c", true, None).await.unwrap();
        db.wiki_upsert_page("w1", "other/c", "C", "s", "c", false, None).await.unwrap();

        let list = db.wiki_list_pages("w1", None, Some("deploy/"), None, 10, 0).await.unwrap();
        assert_eq!(list.len(), 2);
        let locked = db.wiki_list_pages("w1", None, None, Some(true), 10, 0).await.unwrap();
        assert_eq!(locked.len(), 1);
        assert_eq!(locked[0].page_ref, "deploy/b");

        let p = db.wiki_get_page("w1", "deploy/a").await.unwrap().unwrap();
        db.wiki_bump_use(&p.id).await.unwrap();
        let p2 = db.wiki_get_page_by_id(&p.id).await.unwrap().unwrap();
        assert_eq!(p2.use_count, 1);
        assert!(p2.last_used_at.is_some());
    }
}
