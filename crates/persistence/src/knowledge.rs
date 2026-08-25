//! 统一知识容器 + 文档 DAO（双索引模型）。
//!
//! 容器（`knowledge_sources`）统一 `rag_knowledge_bases` 与 `agent_wikis`；文档
//! （`knowledge_docs`）统一两者文档；`knowledge_doc_index` 以 `kind` 区分 per-doc
//! 索引状态（`vector` / `pages`）。本模块收敛容器 CRUD、文档 CRUD 与状态机。

use super::Database;

/// 索引种类，与 `knowledge_doc_index.kind` 的 DB 字符串一一对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IndexKind {
    /// 向量索引（`vector`）。
    Vector,
    /// Pages 索引（`pages`）。
    Pages,
}

impl IndexKind {
    /// 转 DB 字符串（`"vector"` / `"pages"`）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Vector => "vector",
            Self::Pages => "pages",
        }
    }

    /// 从 DB 字符串解析，大小写敏感。
    #[must_use]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "vector" => Some(Self::Vector),
            "pages" => Some(Self::Pages),
            _ => None,
        }
    }
}

impl std::fmt::Display for IndexKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 统一容器记录（`knowledge_sources` 表的一行）。
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct KnowledgeSourceRecord {
    /// 容器 id（主键）。
    pub id: String,
    /// 容器名称。
    pub name: String,
    /// 容器简介（原 RAG 侧 `description` 映射为 `summary`）。
    pub summary: String,
    /// 是否启用向量索引（1/0）。
    pub index_vector: i64,
    /// 是否启用 Pages 索引（1/0）。
    pub index_pages: i64,
    /// 作用域类型（`global` / `client` / `workspace`）。
    pub scope_type: String,
    /// 归属客户端 id。
    pub client_id: String,
    /// 归属工作区 id。
    pub workspace_id: String,
    /// Embedding 服务地址。
    pub emb_base_url: String,
    /// Embedding 服务密钥（加密存储）。
    pub emb_api_key: String,
    /// Embedding 模型名。
    pub emb_model: String,
    /// 向量维度。
    pub emb_dimension: i64,
    /// 检索返回条数。
    pub top_k: i64,
    /// 分块大小（token）。
    pub chunk_size: i64,
    /// 分块重叠大小（token）。
    pub chunk_overlap: i64,
    /// 检索分数阈值。
    pub score_threshold: f64,
    /// 容器状态（`draft`/`pending`/`processing`/`ready`/`failed`）。
    pub status: String,
    /// 版本号。
    pub version: i64,
    /// 页面数量（pages 侧）。
    pub page_count: i64,
    /// 是否启用（1/0）。
    pub enabled: i64,
    /// 创建时间。
    pub created_at: String,
    /// 更新时间。
    pub updated_at: String,
}

/// 统一文档记录（`knowledge_docs` 表的一行，**不含**索引状态）。
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct KnowledgeDocRecord {
    /// 文档 id（主键）。
    pub id: String,
    /// 所属容器 id。
    pub source_id: String,
    /// 源文件名。
    pub filename: String,
    /// 文件类型。
    pub file_type: String,
    /// 内容哈希。
    pub content_hash: String,
    /// 创建时间。
    pub created_at: String,
    /// 更新时间。
    pub updated_at: String,
}

/// 索引状态记录（`knowledge_doc_index` 的一行，`PK = (doc_id, kind)`）。
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct KnowledgeDocIndexRecord {
    /// 文档 id。
    pub doc_id: String,
    /// 索引种类（`vector` / `pages`）。
    pub kind: String,
    /// 处理状态。
    pub status: String,
    /// 条目数（vector 为 chunk_count，pages 为 page 数）。
    pub item_count: i64,
    /// 错误信息。
    pub error: Option<String>,
    /// 更新时间。
    pub updated_at: String,
}

/// `ks_create` 参数包：统一容器的创建字段。
#[derive(Debug, Clone, Default)]
pub struct KsCreateOpts {
    /// 容器 id。
    pub id: String,
    /// 容器名称。
    pub name: String,
    /// 容器简介。
    pub summary: String,
    /// 是否启用向量索引。
    pub index_vector: bool,
    /// 是否启用 Pages 索引。
    pub index_pages: bool,
    /// 作用域类型。
    pub scope_type: String,
    /// 归属客户端 id。
    pub client_id: String,
    /// 归属工作区 id。
    pub workspace_id: String,
    /// Embedding 服务地址。
    pub emb_base_url: String,
    /// Embedding 服务密钥（已加密）。
    pub emb_api_key: String,
    /// Embedding 模型名。
    pub emb_model: String,
    /// 向量维度。
    pub emb_dimension: i64,
    /// 检索返回条数。
    pub top_k: i64,
    /// 分块大小。
    pub chunk_size: i64,
    /// 分块重叠大小。
    pub chunk_overlap: i64,
    /// 检索分数阈值。
    pub score_threshold: f64,
    /// 是否启用。
    pub enabled: bool,
}

/// `ks_update` 参数包：统一容器的更新字段（均为可选，部分更新）。
#[derive(Debug, Clone, Default)]
pub struct KsUpdateOpts {
    /// 容器名称。
    pub name: Option<String>,
    /// 容器简介。
    pub summary: Option<String>,
    /// 向量开关。
    pub index_vector: Option<bool>,
    /// Pages 开关。
    pub index_pages: Option<bool>,
    /// 作用域类型。
    pub scope_type: Option<String>,
    /// 归属客户端 id。
    pub client_id: Option<String>,
    /// 归属工作区 id。
    pub workspace_id: Option<String>,
    /// Embedding 服务地址。
    pub emb_base_url: Option<String>,
    /// Embedding 服务密钥。
    pub emb_api_key: Option<String>,
    /// Embedding 模型名。
    pub emb_model: Option<String>,
    /// 向量维度。
    pub emb_dimension: Option<i64>,
    /// 检索返回条数。
    pub top_k: Option<i64>,
    /// 分块大小。
    pub chunk_size: Option<i64>,
    /// 分块重叠大小。
    pub chunk_overlap: Option<i64>,
    /// 检索分数阈值。
    pub score_threshold: Option<f64>,
    /// 启用态。
    pub enabled: Option<bool>,
}

/// 容器列表/计数过滤条件（scope / kind / enabled 等统一表达）。
#[derive(Debug, Clone, Default)]
pub struct KsListFilter {
    /// 作用域类型精确过滤。
    pub scope_type: Option<String>,
    /// 客户端 id 精确过滤。
    pub client_id: Option<String>,
    /// 工作区 id 精确过滤。
    pub workspace_id: Option<String>,
    /// 名称/简介模糊搜索。
    pub q: Option<String>,
    /// 状态精确过滤。
    pub status: Option<String>,
    /// 索引种类过滤（`Vector` 仅 `index_vector=1`，`Pages` 仅 `index_pages=1`）。
    pub index_kind: Option<IndexKind>,
    /// 启用态过滤。
    pub enabled: Option<bool>,
}

impl Database {
    // ── 容器 CRUD ───────────────────────────────────────────────

    /// 建容器。
    ///
    /// Pages 索引开启时（`index_pages = 1`）同 scope 内 `name` 唯一（部分唯一索引
    /// `WHERE index_pages = 1`），冲突时返回 `UNIQUE` 错误由调用方映射为 409。
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn ks_create(&self, opts: &KsCreateOpts) -> Result<(), sqlx::Error> {
        // 归一 scope 默认值（与旧 wiki 侧一致）
        let scope = if opts.scope_type.is_empty() {
            "global".to_string()
        } else {
            opts.scope_type.clone()
        };
        sqlx::query(
            r"
            INSERT INTO knowledge_sources (
                id, name, summary, index_vector, index_pages,
                scope_type, client_id, workspace_id,
                emb_base_url, emb_api_key, emb_model, emb_dimension,
                top_k, chunk_size, chunk_overlap, score_threshold,
                enabled, created_at, updated_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, datetime('now'), datetime('now'))
            ",
        )
        .bind(&opts.id)
        .bind(&opts.name)
        .bind(&opts.summary)
        .bind(i64::from(opts.index_vector))
        .bind(i64::from(opts.index_pages))
        .bind(&scope)
        .bind(&opts.client_id)
        .bind(&opts.workspace_id)
        .bind(&opts.emb_base_url)
        .bind(&opts.emb_api_key)
        .bind(&opts.emb_model)
        .bind(opts.emb_dimension)
        .bind(opts.top_k)
        .bind(opts.chunk_size)
        .bind(opts.chunk_overlap)
        .bind(opts.score_threshold)
        .bind(i64::from(opts.enabled))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 按 id 查询容器。
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn ks_get(&self, id: &str) -> Result<Option<KnowledgeSourceRecord>, sqlx::Error> {
        sqlx::query_as::<_, KnowledgeSourceRecord>(
            "SELECT id, name, summary, index_vector, index_pages, scope_type, client_id, workspace_id, \
             emb_base_url, emb_api_key, emb_model, emb_dimension, top_k, chunk_size, chunk_overlap, \
             score_threshold, status, version, page_count, enabled, created_at, updated_at \
             FROM knowledge_sources WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
    }

    /// 容器列表（过滤 + 分页，按 `updated_at DESC`）。
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn ks_list(
        &self,
        filter: &KsListFilter,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<KnowledgeSourceRecord>, sqlx::Error> {
        let mut qb = sqlx::QueryBuilder::new(
            "SELECT id, name, summary, index_vector, index_pages, scope_type, client_id, workspace_id, \
             emb_base_url, emb_api_key, emb_model, emb_dimension, top_k, chunk_size, chunk_overlap, \
             score_threshold, status, version, page_count, enabled, created_at, updated_at \
             FROM knowledge_sources WHERE 1=1",
        );
        if let Some(s) = filter.scope_type.as_deref().filter(|s| !s.is_empty()) {
            qb.push(" AND scope_type = ").push_bind(s);
        }
        if let Some(c) = filter.client_id.as_deref().filter(|c| !c.is_empty()) {
            qb.push(" AND client_id = ").push_bind(c);
        }
        if let Some(w) = filter.workspace_id.as_deref().filter(|w| !w.is_empty()) {
            qb.push(" AND workspace_id = ").push_bind(w);
        }
        if let Some(q) = filter.q.as_deref().filter(|q| !q.is_empty()) {
            qb.push(" AND (name LIKE ").push_bind(format!("%{q}%"));
            qb.push(" OR summary LIKE ").push_bind(format!("%{q}%")).push(")");
        }
        if let Some(st) = filter.status.as_deref().filter(|s| !s.is_empty()) {
            qb.push(" AND status = ").push_bind(st);
        }
        if let Some(kind) = filter.index_kind {
            match kind {
                IndexKind::Vector => qb.push(" AND index_vector = 1"),
                IndexKind::Pages => qb.push(" AND index_pages = 1"),
            };
        }
        if let Some(enabled) = filter.enabled {
            qb.push(" AND enabled = ").push_bind(i64::from(enabled));
        }
        qb.push(" ORDER BY updated_at DESC");
        qb.push(" LIMIT ").push_bind(limit);
        qb.push(" OFFSET ").push_bind(offset);
        qb.build_query_as::<KnowledgeSourceRecord>()
            .fetch_all(&self.pool)
            .await
    }

    /// 容器计数（与 `ks_list` 同过滤语义，不含分页）。
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn ks_count(&self, filter: &KsListFilter) -> Result<i64, sqlx::Error> {
        let mut qb = sqlx::QueryBuilder::new("SELECT COUNT(*) FROM knowledge_sources WHERE 1=1");
        if let Some(s) = filter.scope_type.as_deref().filter(|s| !s.is_empty()) {
            qb.push(" AND scope_type = ").push_bind(s);
        }
        if let Some(c) = filter.client_id.as_deref().filter(|c| !c.is_empty()) {
            qb.push(" AND client_id = ").push_bind(c);
        }
        if let Some(w) = filter.workspace_id.as_deref().filter(|w| !w.is_empty()) {
            qb.push(" AND workspace_id = ").push_bind(w);
        }
        if let Some(q) = filter.q.as_deref().filter(|q| !q.is_empty()) {
            qb.push(" AND (name LIKE ").push_bind(format!("%{q}%"));
            qb.push(" OR summary LIKE ").push_bind(format!("%{q}%")).push(")");
        }
        if let Some(st) = filter.status.as_deref().filter(|s| !s.is_empty()) {
            qb.push(" AND status = ").push_bind(st);
        }
        if let Some(kind) = filter.index_kind {
            match kind {
                IndexKind::Vector => qb.push(" AND index_vector = 1"),
                IndexKind::Pages => qb.push(" AND index_pages = 1"),
            };
        }
        if let Some(enabled) = filter.enabled {
            qb.push(" AND enabled = ").push_bind(i64::from(enabled));
        }
        qb.build_query_scalar::<i64>().fetch_one(&self.pool).await
    }

    /// 更新容器的可变字段（`KsUpdateOpts` 中 `Some` 的字段才更新）。
    ///
    /// `index_pages` 置 1 且改名时可能触发部分唯一冲突（同 scope 同名，返回 `UNIQUE`）。
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn ks_update(&self, id: &str, opts: &KsUpdateOpts) -> Result<(), sqlx::Error> {
        // 动态 SET：仅更新 Some 字段。若无字段需更新则 no-op。
        let mut sets: Vec<String> = Vec::new();
        // 用 QueryBuilder 动态绑定，避免字符串插值。
        let mut qb = sqlx::QueryBuilder::new("UPDATE knowledge_sources SET ");
        let mut first = true;
        let mut push_set = |qb: &mut sqlx::QueryBuilder<'_, sqlx::Sqlite>, clause: &str| {
            if !first {
                qb.push(", ");
            }
            qb.push(clause);
            first = false;
        };
        if let Some(v) = opts.name.as_deref() {
            push_set(&mut qb, "name = ");
            qb.push_bind(v);
            sets.push("name".to_string());
        }
        if let Some(v) = opts.summary.as_deref() {
            push_set(&mut qb, "summary = ");
            qb.push_bind(v);
        }
        if let Some(v) = opts.index_vector {
            push_set(&mut qb, "index_vector = ");
            qb.push_bind(i64::from(v));
        }
        if let Some(v) = opts.index_pages {
            push_set(&mut qb, "index_pages = ");
            qb.push_bind(i64::from(v));
        }
        if let Some(v) = opts.scope_type.as_deref() {
            push_set(&mut qb, "scope_type = ");
            qb.push_bind(v);
        }
        if let Some(v) = opts.client_id.as_deref() {
            push_set(&mut qb, "client_id = ");
            qb.push_bind(v);
        }
        if let Some(v) = opts.workspace_id.as_deref() {
            push_set(&mut qb, "workspace_id = ");
            qb.push_bind(v);
        }
        if let Some(v) = opts.emb_base_url.as_deref() {
            push_set(&mut qb, "emb_base_url = ");
            qb.push_bind(v);
        }
        if let Some(v) = opts.emb_api_key.as_deref() {
            push_set(&mut qb, "emb_api_key = ");
            qb.push_bind(v);
        }
        if let Some(v) = opts.emb_model.as_deref() {
            push_set(&mut qb, "emb_model = ");
            qb.push_bind(v);
        }
        if let Some(v) = opts.emb_dimension {
            push_set(&mut qb, "emb_dimension = ");
            qb.push_bind(v);
        }
        if let Some(v) = opts.top_k {
            push_set(&mut qb, "top_k = ");
            qb.push_bind(v);
        }
        if let Some(v) = opts.chunk_size {
            push_set(&mut qb, "chunk_size = ");
            qb.push_bind(v);
        }
        if let Some(v) = opts.chunk_overlap {
            push_set(&mut qb, "chunk_overlap = ");
            qb.push_bind(v);
        }
        if let Some(v) = opts.score_threshold {
            push_set(&mut qb, "score_threshold = ");
            qb.push_bind(v);
        }
        if let Some(v) = opts.enabled {
            push_set(&mut qb, "enabled = ");
            qb.push_bind(i64::from(v));
        }
        if first {
            // 无更新字段
            return Ok(());
        }
        qb.push(", updated_at = datetime('now') WHERE id = ");
        qb.push_bind(id);
        qb.build().execute(&self.pool).await?;
        let _ = sets;
        Ok(())
    }

    /// 切换启用态。
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn ks_set_enabled(&self, id: &str, enabled: bool) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE knowledge_sources SET enabled = ?, updated_at = datetime('now') WHERE id = ?")
            .bind(i64::from(enabled))
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 更新容器状态（`draft`/`pending`/`processing`/`ready`/`failed`）。
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn ks_update_status(&self, id: &str, status: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE knowledge_sources SET status = ?, updated_at = datetime('now') WHERE id = ?")
            .bind(status)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 删除容器（文档经 FK 级联，`knowledge_pages_fts` 需显式清理）。
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn ks_delete(&self, id: &str) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        // FTS 无 FK，须按 rowid 显式清理，且必须与容器删除同事务——否则中途失败会留下
        // 两种不一致：FTS 行已删而 page 仍在（该 page 静默搜不到），或 page 经级联删除
        // 而 FTS 行残留（孤儿行占住该 rowid，SQLite 复用 rowid 时新 page 插 FTS 会冲突）。
        // 同文件的 wiki_delete_page / wiki_clear_pages_by_doc 本就在事务内，此处对齐。
        let rowids: Vec<(i64,)> =
            sqlx::query_as("SELECT rowid FROM knowledge_pages WHERE source_id = ?")
                .bind(id)
                .fetch_all(&mut *tx)
                .await?;
        for (rid,) in rowids {
            sqlx::query("DELETE FROM knowledge_pages_fts WHERE rowid = ?")
                .bind(rid)
                .execute(&mut *tx)
                .await?;
        }
        // docs / doc_index / chunks / pages / edges 经 FK ON DELETE CASCADE 清理
        // （级联在事务内正常生效；只有 PRAGMA foreign_keys 的**修改**在事务内是 no-op）
        sqlx::query("DELETE FROM knowledge_sources WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// 按名称+作用域精确查询（`name` 大小写敏感）。
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn ks_get_by_name_scope(
        &self,
        name: &str,
        scope_type: &str,
        client_id: &str,
        workspace_id: &str,
    ) -> Result<Option<KnowledgeSourceRecord>, sqlx::Error> {
        sqlx::query_as::<_, KnowledgeSourceRecord>(
            "SELECT id, name, summary, index_vector, index_pages, scope_type, client_id, workspace_id, \
             emb_base_url, emb_api_key, emb_model, emb_dimension, top_k, chunk_size, chunk_overlap, \
             score_threshold, status, version, page_count, enabled, created_at, updated_at \
             FROM knowledge_sources WHERE name = ? AND scope_type = ? AND client_id = ? AND workspace_id = ?",
        )
        .bind(name)
        .bind(scope_type)
        .bind(client_id)
        .bind(workspace_id)
        .fetch_optional(&self.pool)
        .await
    }

    /// 大小写不敏感的同作用域名称查找（ASCII 不区分大小写）。
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn ks_get_by_name_scope_ci(
        &self,
        name_lower: &str,
        scope_type: &str,
        client_id: &str,
        workspace_id: &str,
    ) -> Result<Option<KnowledgeSourceRecord>, sqlx::Error> {
        if let Some(row) = self
            .ks_get_by_name_scope(name_lower, scope_type, client_id, workspace_id)
            .await?
        {
            return Ok(Some(row));
        }
        sqlx::query_as::<_, KnowledgeSourceRecord>(
            "SELECT id, name, summary, index_vector, index_pages, scope_type, client_id, workspace_id, \
             emb_base_url, emb_api_key, emb_model, emb_dimension, top_k, chunk_size, chunk_overlap, \
             score_threshold, status, version, page_count, enabled, created_at, updated_at \
             FROM knowledge_sources WHERE lower(name) = lower(?1) \
             AND scope_type = ?2 AND client_id = ?3 AND workspace_id = ?4",
        )
        .bind(name_lower)
        .bind(scope_type)
        .bind(client_id)
        .bind(workspace_id)
        .fetch_optional(&self.pool)
        .await
    }

    /// 对账：把 `pending`/`processing` 的容器置为 `failed`。
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn ks_fail_inflight(&self, error: &str) -> Result<u64, sqlx::Error> {
        let _ = error;
        let r = sqlx::query(
            "UPDATE knowledge_sources SET status = 'failed', updated_at = datetime('now') \
             WHERE status IN ('pending','processing')",
        )
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// 可见容器 id 列表（scope 可见性，pages 侧；对齐旧 `wiki_visible_ids` 语义，
    /// 过滤 `index_pages = 1` 以区分 vector 容器）。
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn ks_visible_ids(
        &self,
        client_id: &str,
        workspace_id: &str,
    ) -> Result<Vec<String>, sqlx::Error> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT id FROM knowledge_sources \
             WHERE index_pages = 1 AND (scope_type = 'global' \
             OR (scope_type = 'client' AND client_id = ?) \
             OR (scope_type = 'workspace' AND client_id = ? AND workspace_id = ?))",
        )
        .bind(client_id)
        .bind(client_id)
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    /// 可见容器完整记录（`page_count DESC`，`index_pages = 1`）。
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn ks_visible_sources(
        &self,
        client_id: &str,
        workspace_id: &str,
        limit: i64,
    ) -> Result<Vec<KnowledgeSourceRecord>, sqlx::Error> {
        sqlx::query_as::<_, KnowledgeSourceRecord>(
            "SELECT id, name, summary, index_vector, index_pages, scope_type, client_id, workspace_id, \
             emb_base_url, emb_api_key, emb_model, emb_dimension, top_k, chunk_size, chunk_overlap, \
             score_threshold, status, version, page_count, enabled, created_at, updated_at \
             FROM knowledge_sources \
             WHERE index_pages = 1 AND (scope_type = 'global' \
             OR (scope_type = 'client' AND client_id = ?) \
             OR (scope_type = 'workspace' AND client_id = ? AND workspace_id = ?)) \
             ORDER BY page_count DESC LIMIT ?",
        )
        .bind(client_id)
        .bind(client_id)
        .bind(workspace_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
    }

    /// 容器下的文档总数（含各状态）。
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn kdoc_count_by_source(&self, source_id: &str) -> Result<i64, sqlx::Error> {
        let row: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM knowledge_docs WHERE source_id = ?")
                .bind(source_id)
                .fetch_one(&self.pool)
                .await?;
        Ok(row.0)
    }

    // ── 文档 CRUD + 索引状态 ───────────────────────────────────────

    /// 创建文档：插 `knowledge_docs`，并为所属容器已启用的每个 `kind` 插 `pending` 索引行。
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn kdoc_create(
        &self,
        id: &str,
        source_id: &str,
        filename: &str,
        file_type: &str,
        content_hash: &str,
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO knowledge_docs (id, source_id, filename, file_type, content_hash, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, datetime('now'), datetime('now'))",
        )
        .bind(id)
        .bind(source_id)
        .bind(filename)
        .bind(file_type)
        .bind(content_hash)
        .execute(&mut *tx)
        .await?;
        // 为容器已启用的 kind 插 pending 行（事务内读取 flags）
        let src: Option<(i64, i64)> =
            sqlx::query_as("SELECT index_vector, index_pages FROM knowledge_sources WHERE id = ?")
                .bind(source_id)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some((iv, ip)) = src {
            if iv != 0 {
                sqlx::query(
                    "INSERT OR IGNORE INTO knowledge_doc_index (doc_id, kind, status, item_count, error, updated_at) \
                     VALUES (?, 'vector', 'pending', 0, NULL, datetime('now'))",
                )
                .bind(id)
                .execute(&mut *tx)
                .await?;
            }
            if ip != 0 {
                sqlx::query(
                    "INSERT OR IGNORE INTO knowledge_doc_index (doc_id, kind, status, item_count, error, updated_at) \
                     VALUES (?, 'pages', 'pending', 0, NULL, datetime('now'))",
                )
                .bind(id)
                .execute(&mut *tx)
                .await?;
            }
            // 两开关均为 0 时**不插任何索引行**：文档只存档、不进摄入管线。
            // 不要在这里"兜底"补一个 vector 行——那会把「用户关掉了所有索引」静默
            // 变成「做向量化」（该容器 emb_* 大概率为空，摄入必然失败，文档永久
            // failed），并制造 index_vector=0 却有 vector 索引行的不一致状态。
            // 若要禁止这种配置，该在 API 层校验，不是在 DAO 层猜测意图。
        }
        tx.commit().await?;
        Ok(())
    }

    /// 按 id 查询文档。
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn kdoc_get(&self, id: &str) -> Result<Option<KnowledgeDocRecord>, sqlx::Error> {
        sqlx::query_as::<_, KnowledgeDocRecord>(
            "SELECT id, source_id, filename, file_type, content_hash, created_at, updated_at \
             FROM knowledge_docs WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
    }

    /// 列出某容器下的全部文档，按创建时间排序。
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn kdoc_list(
        &self,
        source_id: &str,
    ) -> Result<Vec<KnowledgeDocRecord>, sqlx::Error> {
        sqlx::query_as::<_, KnowledgeDocRecord>(
            "SELECT id, source_id, filename, file_type, content_hash, created_at, updated_at \
             FROM knowledge_docs WHERE source_id = ? ORDER BY created_at",
        )
        .bind(source_id)
        .fetch_all(&self.pool)
        .await
    }

    /// 删除文档（`knowledge_chunks` / `knowledge_pages.source_doc_id` 经 FK 联动；`knowledge_doc_index` 级联）。
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn kdoc_delete(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM knowledge_docs WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 查询某文档的 per-kind 索引状态。
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn kdoc_get_index(
        &self,
        doc_id: &str,
        kind: IndexKind,
    ) -> Result<Option<KnowledgeDocIndexRecord>, sqlx::Error> {
        sqlx::query_as::<_, KnowledgeDocIndexRecord>(
            "SELECT doc_id, kind, status, item_count, error, updated_at \
             FROM knowledge_doc_index WHERE doc_id = ? AND kind = ?",
        )
        .bind(doc_id)
        .bind(kind.as_str())
        .fetch_optional(&self.pool)
        .await
    }

    /// 列出某文档的全部索引状态（多 kind 时最多 2 行）。
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn kdoc_list_indexes(
        &self,
        doc_id: &str,
    ) -> Result<Vec<KnowledgeDocIndexRecord>, sqlx::Error> {
        sqlx::query_as::<_, KnowledgeDocIndexRecord>(
            "SELECT doc_id, kind, status, item_count, error, updated_at \
             FROM knowledge_doc_index WHERE doc_id = ? ORDER BY kind",
        )
        .bind(doc_id)
        .fetch_all(&self.pool)
        .await
    }

    /// 更新 per-kind 索引状态。
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn kdoc_update_index_status(
        &self,
        doc_id: &str,
        kind: IndexKind,
        status: &str,
        item_count: i64,
        error: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE knowledge_doc_index SET status = ?, item_count = ?, error = ?, updated_at = datetime('now') \
             WHERE doc_id = ? AND kind = ?",
        )
        .bind(status)
        .bind(item_count)
        .bind(error)
        .bind(doc_id)
        .bind(kind.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 原子 CAS：仅当 `status NOT IN ('pending','processing')` 时置回 `pending`（空闲态可重入）。
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn kdoc_mark_pending_if_idle(
        &self,
        doc_id: &str,
        kind: IndexKind,
    ) -> Result<bool, sqlx::Error> {
        let r = sqlx::query(
            "UPDATE knowledge_doc_index SET status = 'pending', item_count = 0, error = NULL, updated_at = datetime('now') \
             WHERE doc_id = ? AND kind = ? AND status NOT IN ('pending','processing')",
        )
        .bind(doc_id)
        .bind(kind.as_str())
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() > 0)
    }

    /// 原子 CAS：仅当 `status = 'pending'` 时置为 `processing`。
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn kdoc_mark_processing_if_pending(
        &self,
        doc_id: &str,
        kind: IndexKind,
    ) -> Result<bool, sqlx::Error> {
        let r = sqlx::query(
            "UPDATE knowledge_doc_index SET status = 'processing', updated_at = datetime('now') \
             WHERE doc_id = ? AND kind = ? AND status = 'pending'",
        )
        .bind(doc_id)
        .bind(kind.as_str())
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() > 0)
    }

    /// 对账：把 `pending`/`processing` 的索引行统一复位为 `failed`（按 `kind`）。
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn kdoc_fail_inflight(
        &self,
        kind: IndexKind,
        error: &str,
    ) -> Result<u64, sqlx::Error> {
        let r = sqlx::query(
            "UPDATE knowledge_doc_index SET status = 'failed', item_count = 0, error = ?, updated_at = datetime('now') \
             WHERE kind = ? AND status IN ('pending','processing')",
        )
        .bind(error)
        .bind(kind.as_str())
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// 对账：按 `kind` 可选过滤（`None` 则复位全部 kind）。
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn kdoc_fail_inflight_opt(
        &self,
        kind: Option<IndexKind>,
        error: &str,
    ) -> Result<u64, sqlx::Error> {
        if let Some(k) = kind {
            return self.kdoc_fail_inflight(k, error).await;
        }
        let r = sqlx::query(
            "UPDATE knowledge_doc_index SET status = 'failed', item_count = 0, error = ?, updated_at = datetime('now') \
             WHERE status IN ('pending','processing')",
        )
        .bind(error)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// 回填空 `file_type` 为 `'md'`（老数据落盘一律 `.md`）。
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn kdoc_backfill_file_type(&self) -> Result<(), sqlx::Error> {
        sqlx::query(crate::Database::BACKFILL_KNOWLEDGE_DOC_FILE_TYPE_SQL)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_db() -> Database {
        Database::new(":memory:").await.expect("create in-memory db")
    }

    fn vec_opts(id: &str, name: &str) -> KsCreateOpts {
        KsCreateOpts {
            id: id.to_owned(),
            name: name.to_owned(),
            summary: "desc".to_owned(),
            index_vector: true,
            index_pages: false,
            scope_type: "global".to_owned(),
            client_id: String::new(),
            workspace_id: String::new(),
            emb_base_url: "https://api.example.com".to_owned(),
            emb_api_key: "sk-encrypted".to_owned(),
            emb_model: "text-embedding-3-small".to_owned(),
            emb_dimension: 1536,
            top_k: 5,
            chunk_size: 512,
            chunk_overlap: 64,
            score_threshold: 0.3,
            enabled: true,
        }
    }

    fn pages_opts(id: &str, name: &str, scope: &str, client: &str, ws: &str) -> KsCreateOpts {
        KsCreateOpts {
            id: id.to_owned(),
            name: name.to_owned(),
            summary: "summary".to_owned(),
            index_vector: false,
            index_pages: true,
            scope_type: scope.to_owned(),
            client_id: client.to_owned(),
            workspace_id: ws.to_owned(),
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

    #[tokio::test]
    async fn ks_crud_roundtrip_vector() {
        let db = test_db().await;
        db.ks_create(&vec_opts("ks-1", "测试库")).await.unwrap();
        let ks = db.ks_get("ks-1").await.unwrap().unwrap();
        assert_eq!(ks.name, "测试库");
        assert_eq!(ks.index_vector, 1);
        assert_eq!(ks.index_pages, 0);
        assert_eq!(ks.emb_dimension, 1536);
        assert_eq!(ks.enabled, 1);

        let dup = db.ks_create(&vec_opts("ks-1", "x")).await;
        assert!(dup.is_err(), "同 id 重复创建应冲突");

        db.ks_update(
            "ks-1",
            &KsUpdateOpts {
                name: Some("改名".to_owned()),
                summary: Some("新描述".to_owned()),
                top_k: Some(8),
                chunk_size: Some(256),
                chunk_overlap: Some(32),
                score_threshold: Some(0.5),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let ks = db.ks_get("ks-1").await.unwrap().unwrap();
        assert_eq!(ks.name, "改名");
        assert_eq!(ks.summary, "新描述");
        assert_eq!(ks.top_k, 8);
        assert_eq!(ks.emb_base_url, "https://api.example.com", "emb 配置未被覆盖");

        db.ks_create(&vec_opts("ks-2", "库2")).await.unwrap();
        let filter = KsListFilter::default();
        let list = db.ks_list(&filter, 10, 0).await.unwrap();
        assert_eq!(list.len(), 2);

        db.ks_set_enabled("ks-1", false).await.unwrap();
        assert_eq!(db.ks_get("ks-1").await.unwrap().unwrap().enabled, 0);

        assert_eq!(db.kdoc_count_by_source("ks-1").await.unwrap(), 0);

        db.ks_delete("ks-1").await.unwrap();
        assert!(db.ks_get("ks-1").await.unwrap().is_none());
        assert_eq!(db.ks_list(&filter, 10, 0).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn ks_vector_does_not_enforce_name_uniqueness() {
        let db = test_db().await;
        db.ks_create(&vec_opts("v1", "同名")).await.unwrap();
        db.ks_create(&vec_opts("v2", "同名")).await.unwrap();
        let list = db.ks_list(&KsListFilter::default(), 10, 0).await.unwrap();
        assert_eq!(list.len(), 2, "vector 侧同名应允许");
    }

    #[tokio::test]
    async fn ks_pages_enforces_partial_unique() {
        let db = test_db().await;
        db.ks_create(&pages_opts("w1", "my-wiki", "workspace", "c1", "ws1"))
            .await
            .unwrap();
        let dup = db
            .ks_create(&pages_opts("w2", "my-wiki", "workspace", "c1", "ws1"))
            .await;
        assert!(dup.is_err(), "pages 侧同名同 scope 应唯一冲突");
        assert!(dup.unwrap_err().to_string().contains("UNIQUE"));

        db.ks_create(&pages_opts("w3", "my-wiki", "global", "", ""))
            .await
            .unwrap();

        // 同名 pages 更新触发冲突
        db.ks_create(&pages_opts("w4", "other", "workspace", "c1", "ws1"))
            .await
            .unwrap();
        let conflict = db
            .ks_update(
                "w4",
                &KsUpdateOpts {
                    name: Some("my-wiki".to_owned()),
                    ..Default::default()
                },
            )
            .await;
        assert!(conflict.is_err(), "同 scope 改名应触发部分唯一冲突");
        assert!(conflict.unwrap_err().to_string().contains("UNIQUE"));
        // 显式校验：改名后同 scope 查询应命中原名或冲突
        let list = db
            .ks_list(
                &KsListFilter {
                    scope_type: Some("workspace".to_owned()),
                    ..Default::default()
                },
                10,
                0,
            )
            .await
            .unwrap();
        assert!(list.len() >= 2);
    }

    #[tokio::test]
    async fn ks_list_filter_by_kind_and_enabled_and_q() {
        let db = test_db().await;
        db.ks_create(&vec_opts("v1", "alpha")).await.unwrap();
        db.ks_create(&pages_opts("p1", "beta", "global", "", ""))
            .await
            .unwrap();
        db.ks_create(&KsCreateOpts {
            id: "both".to_owned(),
            name: "gamma".to_owned(),
            summary: String::new(),
            index_vector: true,
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
        })
        .await
        .unwrap();
        db.ks_set_enabled("p1", false).await.unwrap();

        let vec_filter = KsListFilter {
            index_kind: Some(IndexKind::Vector),
            ..Default::default()
        };
        let vec_list = db.ks_list(&vec_filter, 10, 0).await.unwrap();
        assert_eq!(vec_list.len(), 2, "vector 过滤应命中 v1 + both");
        let pages_filter = KsListFilter {
            index_kind: Some(IndexKind::Pages),
            ..Default::default()
        };
        let pages_list = db.ks_list(&pages_filter, 10, 0).await.unwrap();
        assert_eq!(pages_list.len(), 2, "pages 过滤应命中 p1 + both");

        let enabled_filter = KsListFilter {
            enabled: Some(true),
            ..Default::default()
        };
        assert_eq!(db.ks_count(&enabled_filter).await.unwrap(), 2);
        let q_filter = KsListFilter {
            q: Some("alpha".to_owned()),
            ..Default::default()
        };
        assert_eq!(db.ks_list(&q_filter, 10, 0).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn ks_visible_scope_filter() {
        let db = test_db().await;
        db.ks_create(&pages_opts("g1", "global-wiki", "global", "", ""))
            .await
            .unwrap();
        db.ks_create(&pages_opts("c1", "client-wiki", "client", "c1", ""))
            .await
            .unwrap();
        db.ks_create(&pages_opts("w1", "ws-wiki", "workspace", "c1", "w1"))
            .await
            .unwrap();
        db.ks_create(&pages_opts("w2", "other-ws", "workspace", "c1", "w2"))
            .await
            .unwrap();
        // vector 容器不应出现在 pages 可见性里
        db.ks_create(&vec_opts("v1", "vec-only")).await.unwrap();

        let ids = db.ks_visible_ids("c1", "w1").await.unwrap();
        assert!(ids.contains(&"g1".to_string()));
        assert!(ids.contains(&"c1".to_string()));
        assert!(ids.contains(&"w1".to_string()));
        assert!(!ids.contains(&"w2".to_string()));
        assert!(!ids.contains(&"v1".to_string()), "vector 容器不应在 pages 可见性中");

        let vis = db.ks_visible_sources("c1", "w1", 10).await.unwrap();
        assert_eq!(vis.len(), 3);
    }

    #[tokio::test]
    async fn ks_get_by_name_scope_and_ci() {
        let db = test_db().await;
        db.ks_create(&pages_opts("w1", "MyWiki", "global", "", ""))
            .await
            .unwrap();
        let hit = db
            .ks_get_by_name_scope("MyWiki", "global", "", "")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(hit.id, "w1");
        let ci = db
            .ks_get_by_name_scope_ci("mywiki", "global", "", "")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ci.id, "w1");
        assert!(db
            .ks_get_by_name_scope("notfound", "global", "", "")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn ks_status_and_fail_inflight() {
        let db = test_db().await;
        db.ks_create(&pages_opts("w1", "wiki", "global", "", ""))
            .await
            .unwrap();
        db.ks_update_status("w1", "pending").await.unwrap();
        assert_eq!(db.ks_get("w1").await.unwrap().unwrap().status, "pending");
        db.ks_update_status("w1", "processing").await.unwrap();
        let n = db.ks_fail_inflight("restart").await.unwrap();
        assert_eq!(n, 1);
        assert_eq!(db.ks_get("w1").await.unwrap().unwrap().status, "failed");
        // 已是 failed 不再复位
        assert_eq!(db.ks_fail_inflight("x").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn dual_index_switch_and_per_kind_independent_status() {
        let db = test_db().await;
        db.ks_create(&KsCreateOpts {
            id: "ks-both".to_owned(),
            name: "both".to_owned(),
            summary: String::new(),
            index_vector: true,
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
        })
        .await
        .unwrap();
        db.kdoc_create("doc1", "ks-both", "a.md", "md", "sha256:x")
            .await
            .unwrap();
        let idxs = db.kdoc_list_indexes("doc1").await.unwrap();
        assert_eq!(idxs.len(), 2, "双索引容器应为文档创建两个 pending 行");
        assert!(idxs.iter().any(|r| r.kind == "vector" && r.status == "pending"));
        assert!(idxs.iter().any(|r| r.kind == "pages" && r.status == "pending"));

        // per-kind 独立：vector ready，pages 仍 pending
        db.kdoc_update_index_status("doc1", IndexKind::Vector, "ready", 5, None)
            .await
            .unwrap();
        let v = db.kdoc_get_index("doc1", IndexKind::Vector).await.unwrap().unwrap();
        assert_eq!(v.status, "ready");
        assert_eq!(v.item_count, 5);
        let p = db.kdoc_get_index("doc1", IndexKind::Pages).await.unwrap().unwrap();
        assert_eq!(p.status, "pending", "pages 侧不应被 vector 更新影响");
        assert_eq!(p.item_count, 0);

        // CAS 互不干扰：vector 为 ready 时可抢回 pending，pages 为 pending 时不可抢
        assert!(db
            .kdoc_mark_pending_if_idle("doc1", IndexKind::Vector)
            .await
            .unwrap());
        assert!(!db
            .kdoc_mark_pending_if_idle("doc1", IndexKind::Pages)
            .await
            .unwrap());

        // pages 的 pending -> processing
        assert!(db
            .kdoc_mark_processing_if_pending("doc1", IndexKind::Pages)
            .await
            .unwrap());
        assert_eq!(
            db.kdoc_get_index("doc1", IndexKind::Pages)
                .await
                .unwrap()
                .unwrap()
                .status,
            "processing"
        );
        // vector 此时为 pending（刚抢回），CAS processing 应成功
        assert!(db
            .kdoc_mark_processing_if_pending("doc1", IndexKind::Vector)
            .await
            .unwrap());

        // 对账：仅复位 vector 的 inflight
        db.kdoc_update_index_status("doc1", IndexKind::Vector, "processing", 0, None)
            .await
            .unwrap();
        db.kdoc_update_index_status("doc1", IndexKind::Pages, "ready", 2, None)
            .await
            .unwrap();
        let n = db
            .kdoc_fail_inflight(IndexKind::Vector, "crash")
            .await
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            db.kdoc_get_index("doc1", IndexKind::Vector)
                .await
                .unwrap()
                .unwrap()
                .status,
            "failed"
        );
        assert_eq!(
            db.kdoc_get_index("doc1", IndexKind::Pages)
                .await
                .unwrap()
                .unwrap()
                .status,
            "ready",
            "pages ready 不应被 vector 对账影响"
        );
    }

    #[tokio::test]
    async fn kdoc_crud_and_counts_and_cascade() {
        let db = test_db().await;
        db.ks_create(&vec_opts("ks1", "kb")).await.unwrap();
        db.kdoc_create("d1", "ks1", "a.md", "md", "sha256:a")
            .await
            .unwrap();
        db.kdoc_create("d2", "ks1", "b.md", "md", "sha256:b")
            .await
            .unwrap();
        assert_eq!(db.kdoc_count_by_source("ks1").await.unwrap(), 2);
        assert_eq!(db.kdoc_list("ks1").await.unwrap().len(), 2);
        assert_eq!(db.kdoc_list("missing").await.unwrap().len(), 0);

        // 级联：删容器应删文档及索引
        // 先插入一个 vector pending 索引校验存在
        assert!(db
            .kdoc_get_index("d1", IndexKind::Vector)
            .await
            .unwrap()
            .is_some());
        db.ks_delete("ks1").await.unwrap();
        assert!(db.ks_get("ks1").await.unwrap().is_none());
        assert!(db.kdoc_get("d1").await.unwrap().is_none());
        assert!(db
            .kdoc_get_index("d1", IndexKind::Vector)
            .await
            .unwrap()
            .is_none());
        assert_eq!(db.kdoc_count_by_source("ks1").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn kdoc_mark_pending_cas_and_reconcile() {
        let db = test_db().await;
        db.ks_create(&vec_opts("ks1", "n")).await.unwrap();
        db.kdoc_create("d1", "ks1", "a.md", "md", "sha256:x")
            .await
            .unwrap();
        // pending -> 不能抢
        assert!(!db
            .kdoc_mark_pending_if_idle("d1", IndexKind::Vector)
            .await
            .unwrap());
        db.kdoc_update_index_status("d1", IndexKind::Vector, "ready", 5, None)
            .await
            .unwrap();
        assert!(db
            .kdoc_mark_pending_if_idle("d1", IndexKind::Vector)
            .await
            .unwrap());
        let idx = db
            .kdoc_get_index("d1", IndexKind::Vector)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(idx.status, "pending");
        assert_eq!(idx.item_count, 0);

        // processing -> 不能抢
        db.kdoc_update_index_status("d1", IndexKind::Vector, "processing", 0, None)
            .await
            .unwrap();
        assert!(!db
            .kdoc_mark_pending_if_idle("d1", IndexKind::Vector)
            .await
            .unwrap());

        // failed -> 可抢
        db.kdoc_update_index_status("d1", IndexKind::Vector, "failed", 0, Some("boom"))
            .await
            .unwrap();
        assert!(db
            .kdoc_mark_pending_if_idle("d1", IndexKind::Vector)
            .await
            .unwrap());
        assert!(db
            .kdoc_get_index("d1", IndexKind::Vector)
            .await
            .unwrap()
            .unwrap()
            .error
            .is_none());

        // 对账：pending/processing -> failed
        db.kdoc_create("d2", "ks1", "b.md", "md", "sha256:b")
            .await
            .unwrap();
        db.kdoc_update_index_status("d2", IndexKind::Vector, "processing", 0, None)
            .await
            .unwrap();
        db.kdoc_create("d3", "ks1", "c.md", "md", "sha256:c")
            .await
            .unwrap();
        db.kdoc_update_index_status("d3", IndexKind::Vector, "ready", 3, None)
            .await
            .unwrap();
        let n = db
            .kdoc_fail_inflight(IndexKind::Vector, "interrupted")
            .await
            .unwrap();
        assert_eq!(n, 2, "d1 pending + d2 processing -> 2");
        // 精确：d1 已是 pending，d2 processing，d3 ready -> 应复位 2
        // 上面 d1 在 failed->pending 抢成功后为 pending，所以共 2
        // 但我们刚把 d1 设为 pending，未改 d2，所以共 2
        // 容忍实现为实际 2，断言放宽
        assert!(n >= 2);
        for id in ["d1", "d2"] {
            let r = db
                .kdoc_get_index(id, IndexKind::Vector)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(r.status, "failed");
            assert_eq!(r.error.as_deref(), Some("interrupted"));
        }
        assert_eq!(
            db.kdoc_get_index("d3", IndexKind::Vector)
                .await
                .unwrap()
                .unwrap()
                .status,
            "ready"
        );
    }

    #[tokio::test]
    async fn kdoc_backfill_file_type() {
        let db = test_db().await;
        db.ks_create(&vec_opts("kb2", "n")).await.unwrap();
        db.kdoc_create("legacy", "kb2", "old.md", "", "sha256:y")
            .await
            .unwrap();
        db.kdoc_backfill_file_type().await.unwrap();
        let doc = db.kdoc_get("legacy").await.unwrap().unwrap();
        assert_eq!(doc.file_type, "md");

        db.kdoc_create("legacy-txt", "kb2", "notes.txt", "", "sha256:z")
            .await
            .unwrap();
        db.kdoc_backfill_file_type().await.unwrap();
        let doc = db.kdoc_get("legacy-txt").await.unwrap().unwrap();
        assert_eq!(doc.file_type, "md");
    }

    #[tokio::test]
    async fn ks_delete_cleans_fts_and_cascades() {
        let db = test_db().await;
        db.ks_create(&pages_opts("w1", "wiki", "global", "", ""))
            .await
            .unwrap();
        db.kdoc_create("doc1", "w1", "a.md", "md", "sha256:x")
            .await
            .unwrap();
        // 建一页（走 wiki DAO，验证 FTS 级联清理）
        db.wiki_upsert_page("w1", "a/b", "T", "S", "hello world", false, Some("doc1"))
            .await
            .unwrap();
        let fts_before: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM knowledge_pages_fts")
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(fts_before, 1);
        db.ks_delete("w1").await.unwrap();
        let fts_after: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM knowledge_pages_fts")
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(fts_after, 0, "容器删除应同步清理 FTS");
        assert!(db.kdoc_get("doc1").await.unwrap().is_none());
        assert!(db
            .wiki_get_page("w1", "a/b")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn kdoc_mark_processing_if_pending() {
        let db = test_db().await;
        db.ks_create(&pages_opts("w1", "wiki", "global", "", ""))
            .await
            .unwrap();
        db.kdoc_create("d1", "w1", "a.md", "md", "sha256:x")
            .await
            .unwrap();
        // pending -> processing 成功
        assert!(db
            .kdoc_mark_processing_if_pending("d1", IndexKind::Pages)
            .await
            .unwrap());
        assert_eq!(
            db.kdoc_get_index("d1", IndexKind::Pages)
                .await
                .unwrap()
                .unwrap()
                .status,
            "processing"
        );
        // 再次 processing -> 失败
        assert!(!db
            .kdoc_mark_processing_if_pending("d1", IndexKind::Pages)
            .await
            .unwrap());
        db.kdoc_update_index_status("d1", IndexKind::Pages, "ready", 1, None)
            .await
            .unwrap();
        assert!(!db
            .kdoc_mark_processing_if_pending("d1", IndexKind::Pages)
            .await
            .unwrap());
    }

    /// 两开关全关的容器：文档只落 `knowledge_docs`，**不产生任何索引行**。
    /// 此处曾有个「兜底插 vector」分支，会让用户关掉全部索引后上传的文档仍被送去
    /// 向量化（这类容器 emb_* 为空，摄入必然失败 → 文档永久 failed），并制造
    /// `index_vector=0` 却存在 vector 索引行的不一致状态。
    /// 反向断言（开 vector 则有 1 行）用来防止「把插入整段删掉」也能让测试通过。
    #[tokio::test]
    async fn kdoc_create_with_no_index_enabled_produces_no_index_rows() {
        let db = test_db().await;
        let mut off = vec_opts("ks-off", "无索引容器");
        off.index_vector = false;
        off.index_pages = false;
        db.ks_create(&off).await.unwrap();
        db.kdoc_create("d-off", "ks-off", "notes.md", "md", "hash-off")
            .await
            .unwrap();

        assert!(
            db.kdoc_get("d-off").await.unwrap().is_some(),
            "文档本体应已落库"
        );
        let idx = db.kdoc_list_indexes("d-off").await.unwrap();
        assert!(idx.is_empty(), "两开关全关时不应产生索引行，实际: {idx:?}");

        db.ks_create(&vec_opts("ks-on", "有索引容器")).await.unwrap();
        db.kdoc_create("d-on", "ks-on", "notes.md", "md", "hash-on")
            .await
            .unwrap();
        let on_idx = db.kdoc_list_indexes("d-on").await.unwrap();
        assert_eq!(on_idx.len(), 1, "开 vector 的容器应有 1 行索引");
        assert_eq!(on_idx[0].kind, IndexKind::Vector.as_str());
    }
}
