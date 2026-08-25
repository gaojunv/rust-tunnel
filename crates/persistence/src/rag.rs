//! RAG 知识库的数据访问层（向量本体在 qdrant-edge，此处存元数据与原文）。

use super::Database;

/// RAG 知识库记录（rag_knowledge_bases 表的一行）。
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct RagKnowledgeBaseRecord {
    /// 知识库 id（主键）。
    pub id: String,
    /// 知识库名称。
    pub name: String,
    /// 知识库描述。
    pub description: String,
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
    /// 是否启用（1 启用，0 禁用）。
    pub enabled: i32,
    /// 创建时间。
    pub created_at: String,
    /// 更新时间。
    pub updated_at: String,
}

/// RAG 文档记录（rag_documents 表的一行）。
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct RagDocumentRecord {
    /// 文档 id（主键）。
    pub id: String,
    /// 所属知识库 id。
    pub kb_id: String,
    /// 源文件名。
    pub filename: String,
    /// 文件类型（如 md/pdf）。
    pub file_type: String,
    /// 内容哈希。
    pub content_hash: String,
    /// 处理状态（pending/processing/ready/failed）。
    pub status: String,
    /// 分块数量。
    pub chunk_count: i64,
    /// 错误信息，成功时为 None。
    pub error: Option<String>,
    /// 创建时间。
    pub created_at: String,
    /// 更新时间。
    pub updated_at: String,
}

/// RAG 分块记录（rag_chunks 表的一行）。
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct RagChunkRecord {
    /// 分块 id（主键）。
    pub id: String,
    /// 所属文档 id。
    pub doc_id: String,
    /// 所属知识库 id（冗余列，便于按库聚合）。
    pub kb_id: String,
    /// 分块序号。
    pub seq: i64,
    /// 标题路径（如 "## 概述 > ### 背景"）。
    pub heading_path: String,
    /// 分块文本内容。
    pub content: String,
    /// token 数量。
    pub token_count: i64,
}

/// `rag_create_kb` 参数包：知识库创建的全部字段（12 项）。
#[derive(Debug, Clone, Default)]
pub struct RagCreateKbOpts {
    /// 知识库 id。
    pub id: String,
    /// 知识库名称。
    pub name: String,
    /// 知识库描述。
    pub description: String,
    /// Embedding 服务地址。
    pub emb_base_url: String,
    /// Embedding 服务密钥（已加密密文）。
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
    /// 是否启用。
    pub enabled: bool,
}

/// `rag_update_kb_params` 参数包：知识库参数更新的全部字段（10 项）。
#[derive(Debug, Clone, Default)]
pub struct RagUpdateKbParamsOpts {
    /// 知识库名称。
    pub name: String,
    /// 知识库描述。
    pub description: String,
    /// 检索返回条数。
    pub top_k: i64,
    /// 分块大小（token）。
    pub chunk_size: i64,
    /// 分块重叠大小（token）。
    pub chunk_overlap: i64,
    /// 检索分数阈值。
    pub score_threshold: f64,
}

/// `rag_update_kb_full` 参数包：知识库完整更新的全部字段（9 项）。
#[derive(Debug, Clone, Default)]
pub struct RagUpdateKbFullOpts {
    /// 知识库名称。
    pub name: String,
    /// 知识库描述。
    pub description: String,
    /// 检索返回条数。
    pub top_k: i64,
    /// 分块大小（token）。
    pub chunk_size: i64,
    /// 分块重叠大小（token）。
    pub chunk_overlap: i64,
    /// 检索分数阈值。
    pub score_threshold: f64,
    /// Embedding 服务地址。
    pub emb_base_url: String,
    /// Embedding 服务密钥（已加密密文）。
    pub emb_api_key: String,
    /// Embedding 模型名。
    pub emb_model: String,
    /// 向量维度。
    pub emb_dimension: i64,
}

impl Database {
    // ── Knowledge base CRUD ──────────────────────────────────────

    /// 创建知识库。emb_api_key 的加解密由调用方（mgmt api 层）用
    /// encrypt_field/decrypt_field 处理，本层只存取原始字符串。
    /// # Errors
    ///
    /// 数据库连接不可用、约束冲突或 SQL 执行失败时返回 `sqlx::Error`。
    pub async fn rag_create_kb(&self, opts: &RagCreateKbOpts) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"
            INSERT INTO rag_knowledge_bases (
                id, name, description, emb_base_url, emb_api_key, emb_model,
                emb_dimension, top_k, chunk_size, chunk_overlap, score_threshold,
                enabled, created_at, updated_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, datetime('now'), datetime('now'))
            ",
        )
        .bind(&opts.id)
        .bind(&opts.name)
        .bind(&opts.description)
        .bind(&opts.emb_base_url)
        .bind(&opts.emb_api_key)
        .bind(&opts.emb_model)
        .bind(opts.emb_dimension)
        .bind(opts.top_k)
        .bind(opts.chunk_size)
        .bind(opts.chunk_overlap)
        .bind(opts.score_threshold)
        .bind(i32::from(opts.enabled))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 按 id 查询知识库，不存在返回 None。
    /// # Errors
    ///
    /// 数据库连接不可用、SQL 执行失败或结果反序列化失败时返回 `sqlx::Error`。
    pub async fn rag_get_kb(
        &self,
        id: &str,
    ) -> Result<Option<RagKnowledgeBaseRecord>, sqlx::Error> {
        sqlx::query_as::<_, RagKnowledgeBaseRecord>(
            "SELECT * FROM rag_knowledge_bases WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
    }

    /// 列出全部知识库，按创建时间排序。
    /// # Errors
    ///
    /// 数据库连接不可用、SQL 执行失败或结果反序列化失败时返回 `sqlx::Error`。
    pub async fn rag_list_kbs(&self) -> Result<Vec<RagKnowledgeBaseRecord>, sqlx::Error> {
        sqlx::query_as::<_, RagKnowledgeBaseRecord>(
            "SELECT * FROM rag_knowledge_bases ORDER BY created_at",
        )
        .fetch_all(&self.pool)
        .await
    }

    /// 更新知识库的「名称 + 检索/分块参数」。emb 配置（base_url/api_key/model/dimension）
    /// 建库后锁定不可改（qdrant shard 维度固定），需要改动时删除重建。
    /// # Errors
    ///
    /// 数据库连接不可用或 SQL 执行失败时返回 `sqlx::Error`。
    pub async fn rag_update_kb_params(
        &self,
        id: &str,
        opts: &RagUpdateKbParamsOpts,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"
            UPDATE rag_knowledge_bases
            SET name = ?, description = ?, top_k = ?, chunk_size = ?, chunk_overlap = ?,
                score_threshold = ?, updated_at = datetime('now')
            WHERE id = ?
            ",
        )
        .bind(&opts.name)
        .bind(&opts.description)
        .bind(opts.top_k)
        .bind(opts.chunk_size)
        .bind(opts.chunk_overlap)
        .bind(opts.score_threshold)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 更新知识库的「名称 + 检索/分块参数 + emb 配置」。emb_api_key 入参为已加密密文
    /// （调用方用 `encrypt_field` 处理），本层只存取原始字符串。用于编辑 KB 时全量保存
    /// （含可选的 emb 配置变更，与建库时同口径）。区别于 `rag_update_kb_params`——后者
    /// 不碰 emb 列（历史锁定语义），本方法提供可编辑 emb 的能力。
    /// # Errors
    ///
    /// 数据库连接不可用或 SQL 执行失败时返回 `sqlx::Error`。
    pub async fn rag_update_kb_full(
        &self,
        id: &str,
        opts: &RagUpdateKbFullOpts,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"
            UPDATE rag_knowledge_bases
            SET name = ?, description = ?, top_k = ?, chunk_size = ?, chunk_overlap = ?,
                score_threshold = ?, emb_base_url = ?, emb_api_key = ?, emb_model = ?,
                emb_dimension = ?, updated_at = datetime('now')
            WHERE id = ?
            ",
        )
        .bind(&opts.name)
        .bind(&opts.description)
        .bind(opts.top_k)
        .bind(opts.chunk_size)
        .bind(opts.chunk_overlap)
        .bind(opts.score_threshold)
        .bind(&opts.emb_base_url)
        .bind(&opts.emb_api_key)
        .bind(&opts.emb_model)
        .bind(opts.emb_dimension)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 切换知识库启用状态。
    /// # Errors
    ///
    /// 数据库连接不可用或 SQL 执行失败时返回 `sqlx::Error`。
    pub async fn rag_toggle_kb(&self, id: &str, enabled: bool) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE rag_knowledge_bases SET enabled = ?, updated_at = datetime('now') WHERE id = ?",
        )
        .bind(i32::from(enabled))
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 删除知识库（文档与分块经 FK 级联删除）。
    /// # Errors
    ///
    /// 数据库连接不可用或 SQL 执行失败时返回 `sqlx::Error`。
    pub async fn rag_delete_kb(&self, id: &str) -> Result<(), sqlx::Error> {
        // 文档/分块经 FK ON DELETE CASCADE 级联删除
        sqlx::query("DELETE FROM rag_knowledge_bases WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ── Document CRUD ────────────────────────────────────────────

    /// 创建文档记录，status 初始为 'pending'（异步摄入开始前的占位）。
    /// # Errors
    ///
    /// 数据库连接不可用、约束冲突或 SQL 执行失败时返回 `sqlx::Error`。
    pub async fn rag_create_document(
        &self,
        id: &str,
        kb_id: &str,
        filename: &str,
        content_hash: &str,
        file_type: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"
            INSERT INTO rag_documents (id, kb_id, filename, file_type, content_hash, status, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, 'pending', datetime('now'), datetime('now'))
            ",
        )
        .bind(id)
        .bind(kb_id)
        .bind(filename)
        .bind(file_type)
        .bind(content_hash)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 测试/维护用：回填空 file_type 为 'md'（老数据落盘一律 .md，见 schema.rs
    /// `BACKFILL_RAG_DOCUMENT_FILE_TYPE_SQL` 注释）。
    /// # Errors
    ///
    /// 数据库连接不可用或 SQL 执行失败时返回 `sqlx::Error`。
    pub async fn backfill_rag_document_file_type(&self) -> Result<(), sqlx::Error> {
        sqlx::query(Self::BACKFILL_RAG_DOCUMENT_FILE_TYPE_SQL)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 按 id 查询文档，不存在返回 None。
    /// # Errors
    ///
    /// 数据库连接不可用、SQL 执行失败或结果反序列化失败时返回 `sqlx::Error`。
    pub async fn rag_get_document(
        &self,
        id: &str,
    ) -> Result<Option<RagDocumentRecord>, sqlx::Error> {
        // 显式列：`SELECT *` 会在 ALTER TABLE 追加 file_type 后命中 sqlx 语句缓存中的
        // 旧列元数据，与 RagDocumentRecord 的 FromRow 错位导致越界 panic（同 2201ba6
        // llm_api_keys 的修法）。
        sqlx::query_as::<_, RagDocumentRecord>(
            "SELECT id, kb_id, filename, file_type, content_hash, status, chunk_count, \
             error, created_at, updated_at FROM rag_documents WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
    }

    /// 列出某知识库下的全部文档，按创建时间排序。
    /// # Errors
    ///
    /// 数据库连接不可用、SQL 执行失败或结果反序列化失败时返回 `sqlx::Error`。
    pub async fn rag_list_documents(
        &self,
        kb_id: &str,
    ) -> Result<Vec<RagDocumentRecord>, sqlx::Error> {
        // 同上：显式列避免语句缓存旧元数据与 FromRow 错位。
        sqlx::query_as::<_, RagDocumentRecord>(
            "SELECT id, kb_id, filename, file_type, content_hash, status, chunk_count, \
             error, created_at, updated_at FROM rag_documents WHERE kb_id = ? ORDER BY created_at",
        )
        .bind(kb_id)
        .fetch_all(&self.pool)
        .await
    }

    /// 更新文档状态。`error` 传 None 清空失败原因（例如重索引成功时）。
    /// # Errors
    ///
    /// 数据库连接不可用或 SQL 执行失败时返回 `sqlx::Error`。
    pub async fn rag_update_document_status(
        &self,
        doc_id: &str,
        status: &str,
        chunk_count: i64,
        error: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"
            UPDATE rag_documents
            SET status = ?, chunk_count = ?, error = ?, updated_at = datetime('now')
            WHERE id = ?
            ",
        )
        .bind(status)
        .bind(chunk_count)
        .bind(error)
        .bind(doc_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 原子 CAS：仅当文档处于 ready/failed（空闲态）时置回 pending 并清零 chunk_count。
    /// 返回 true = 抢占成功；false = 正在 pending/processing（在途），调用方应拒绝重索引。
    /// 解决 reindex 端点 check-then-act 竞态：两个并发请求只有一个能抢到。
    /// # Errors
    ///
    /// 数据库连接不可用或 SQL 执行失败时返回 `sqlx::Error`。
    pub async fn rag_mark_document_pending_if_idle(
        &self,
        doc_id: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"
            UPDATE rag_documents
            SET status = 'pending', chunk_count = 0, error = NULL, updated_at = datetime('now')
            WHERE id = ? AND status NOT IN ('pending', 'processing')
            ",
        )
        .bind(doc_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// 启动对账：把上次运行遗留的 pending/processing 文档统一复位为 failed。
    /// 服务器若在摄入中途崩溃/panic，这些 doc 永远停在在途态、UI 永久卡住；
    /// 启动时（API 服务开启前）复位后前端可感知失败并重试（reindex/上传走
    /// `rag_mark_document_pending_if_idle` CAS 抢占）。返回被复位的行数。
    /// # Errors
    ///
    /// 数据库连接不可用或 SQL 执行失败时返回 `sqlx::Error`。
    pub async fn rag_fail_inflight_documents(&self, error: &str) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            r"
            UPDATE rag_documents
            SET status = 'failed', chunk_count = 0, error = ?, updated_at = datetime('now')
            WHERE status IN ('pending', 'processing')
            ",
        )
        .bind(error)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// 删除文档（分块经 FK 级联删除）。
    /// # Errors
    ///
    /// 数据库连接不可用或 SQL 执行失败时返回 `sqlx::Error`。
    pub async fn rag_delete_document(&self, id: &str) -> Result<(), sqlx::Error> {
        // 分块经 FK ON DELETE CASCADE 级联删除
        sqlx::query("DELETE FROM rag_documents WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ── Chunks ───────────────────────────────────────────────────

    /// 事务批量插入分块（摄入完成、写向量后落库）。
    /// `rows` 的元素为元组 `(id, doc_id, kb_id, seq, heading_path, content, token_count)`，
    /// 与 rag_chunks 表列一一对应。
    /// # Errors
    ///
    /// 数据库连接不可用、约束冲突或 SQL 执行失败时返回 `sqlx::Error`。
    pub async fn rag_insert_chunks(
        &self,
        rows: &[(String, String, String, i64, String, String, i64)],
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        for (id, doc_id, kb_id, seq, heading_path, content, token_count) in rows {
            sqlx::query(
                r"
                INSERT INTO rag_chunks (id, doc_id, kb_id, seq, heading_path, content, token_count)
                VALUES (?, ?, ?, ?, ?, ?, ?)
                ",
            )
            .bind(id)
            .bind(doc_id)
            .bind(kb_id)
            .bind(seq)
            .bind(heading_path)
            .bind(content)
            .bind(token_count)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// 按 id 批量取分块（检索命中后回填 content/heading_path）。
    /// 空列表直接返回空，避免生成非法 `IN ()` 语句。
    /// # Errors
    ///
    /// 数据库连接不可用、SQL 执行失败或结果反序列化失败时返回 `sqlx::Error`。
    pub async fn rag_get_chunks_by_ids(
        &self,
        ids: &[String],
    ) -> Result<Vec<RagChunkRecord>, sqlx::Error> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; ids.len()].join(", ");
        let sql = format!("SELECT * FROM rag_chunks WHERE id IN ({placeholders})");
        let mut q = sqlx::query_as::<_, RagChunkRecord>(&sql);
        for id in ids {
            q = q.bind(id);
        }
        q.fetch_all(&self.pool).await
    }

    /// 删除某文档的全部分块（删除文档/重索引前清空旧索引时调用）。
    /// # Errors
    ///
    /// 数据库连接不可用或 SQL 执行失败时返回 `sqlx::Error`。
    pub async fn rag_delete_chunks_by_doc(&self, doc_id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM rag_chunks WHERE doc_id = ?")
            .bind(doc_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 删除某知识库的全部分块（全量重建前清空旧索引时调用）。文档/向量本体在别处清理，
    /// 本方法只清 SQLite 的 chunk 元数据行（按 `kb_id` 列聚合删除，不触碰文档行）。
    /// # Errors
    ///
    /// 数据库连接不可用或 SQL 执行失败时返回 `sqlx::Error`。
    pub async fn rag_delete_chunks_by_kb(&self, kb_id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM rag_chunks WHERE kb_id = ?")
            .bind(kb_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ── Counts ───────────────────────────────────────────────────

    /// 知识库下的文档总数（含 pending/failed）。
    /// # Errors
    ///
    /// 数据库连接不可用、SQL 执行失败或结果反序列化失败时返回 `sqlx::Error`。
    pub async fn rag_count_kb_docs(&self, kb_id: &str) -> Result<i64, sqlx::Error> {
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM rag_documents WHERE kb_id = ?")
            .bind(kb_id)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.0)
    }

    /// 知识库下的分块总数（冗余 kb_id 列按库聚合）。
    /// # Errors
    ///
    /// 数据库连接不可用、SQL 执行失败或结果反序列化失败时返回 `sqlx::Error`。
    pub async fn rag_count_kb_chunks(&self, kb_id: &str) -> Result<i64, sqlx::Error> {
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM rag_chunks WHERE kb_id = ?")
            .bind(kb_id)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_db() -> Database {
        // 内存数据库；Database::new 内部自动初始化完整 schema（含本次新增的 RAG 表）
        Database::new(":memory:")
            .await
            .expect("create in-memory db")
    }

    #[tokio::test]
    async fn schema_creates_rag_tables() {
        let db = test_db().await;
        // 三张表存在
        for t in ["rag_knowledge_bases", "rag_documents", "rag_chunks"] {
            let row: (i64,) =
                sqlx::query_as("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?")
                    .bind(t)
                    .fetch_one(db.pool())
                    .await
                    .unwrap();
            assert_eq!(row.0, 1, "table {t} should exist");
        }
        // llm_api_keys 有 kb_id 列
        let row: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM pragma_table_info('llm_api_keys') WHERE name='kb_id'",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(row.0, 1);
        // llm_usage_logs 有 rag_chunks_injected 列
        let row: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM pragma_table_info('llm_usage_logs') WHERE name='rag_chunks_injected'",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(row.0, 1);
    }

    #[tokio::test]
    async fn schema_migration_is_idempotent() {
        let db = test_db().await;
        // 在同一连接池上再次执行幂等迁移（模拟旧库升级后重启）不报错。
        // initialize_schema 是 Database 的私有静态方法，此处直接重跑两个 ALTER 验证幂等。
        for sql in [
            "ALTER TABLE llm_api_keys ADD COLUMN kb_id TEXT REFERENCES rag_knowledge_bases(id) ON DELETE SET NULL",
            "ALTER TABLE llm_usage_logs ADD COLUMN rag_chunks_injected INTEGER",
        ] {
            // 已存在列时应报 duplicate column，迁移函数须吞掉该错误
            let res = sqlx::query(sql).execute(db.pool()).await;
            let dominated = match &res {
                Ok(_) => true,
                Err(e) => e.to_string().contains("duplicate column"),
            };
            assert!(
                dominated,
                "idempotent migration should tolerate duplicate column: {:?}",
                res.err()
            );
        }
    }

    // ── DAO round-trip ───────────────────────────────────────────

    /// 构造一个最小可用的知识库 record 字段集。
    async fn create_sample_kb(db: &Database, id: &str) {
        db.rag_create_kb(&RagCreateKbOpts {
            id: id.to_owned(),
            name: "测试库".to_owned(),
            description: "描述".to_owned(),
            emb_base_url: "https://api.example.com".to_owned(),
            emb_api_key: "sk-encrypted".to_owned(),
            emb_model: "text-embedding-3-small".to_owned(),
            emb_dimension: 1536,
            top_k: 5,
            chunk_size: 512,
            chunk_overlap: 64,
            score_threshold: 0.3,
            enabled: true,
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn kb_crud_roundtrip() {
        let db = test_db().await;

        create_sample_kb(&db, "kb-1").await;
        // get
        let kb = db.rag_get_kb("kb-1").await.unwrap().unwrap();
        assert_eq!(kb.name, "测试库");
        assert_eq!(kb.emb_dimension, 1536);
        assert_eq!(kb.enabled, 1);

        // 重复创建同一 id 覆盖（upsert 语义不要求，此处应报 primary key 冲突）
        let dup = db
            .rag_create_kb(&RagCreateKbOpts {
                id: "kb-1".to_owned(),
                name: "x".to_owned(),
                description: String::new(),
                emb_base_url: "https://api.example.com".to_owned(),
                emb_api_key: "k".to_owned(),
                emb_model: "m".to_owned(),
                emb_dimension: 10,
                top_k: 5,
                chunk_size: 512,
                chunk_overlap: 64,
                score_threshold: 0.3,
                enabled: true,
            })
            .await;
        assert!(dup.is_err(), "同 id 重复创建应冲突");

        // update params：emb 配置不变
        db.rag_update_kb_params(
            "kb-1",
            &RagUpdateKbParamsOpts {
                name: "改名".to_owned(),
                description: "新描述".to_owned(),
                top_k: 8,
                chunk_size: 256,
                chunk_overlap: 32,
                score_threshold: 0.5,
            },
        )
        .await
        .unwrap();
        let kb = db.rag_get_kb("kb-1").await.unwrap().unwrap();
        assert_eq!(kb.name, "改名");
        assert_eq!(kb.description, "新描述");
        assert_eq!(kb.top_k, 8);
        assert_eq!(kb.chunk_size, 256);
        assert_eq!(kb.chunk_overlap, 32);
        assert!((kb.score_threshold - 0.5).abs() < 1e-9);
        assert_eq!(
            kb.emb_base_url, "https://api.example.com",
            "emb 配置不可被 update_params 改"
        );

        // list
        create_sample_kb(&db, "kb-2").await;
        let kbs = db.rag_list_kbs().await.unwrap();
        assert_eq!(kbs.len(), 2);

        // toggle 启停
        db.rag_toggle_kb("kb-1", false).await.unwrap();
        assert_eq!(db.rag_get_kb("kb-1").await.unwrap().unwrap().enabled, 0);

        // count 空库
        assert_eq!(db.rag_count_kb_docs("kb-1").await.unwrap(), 0);
        assert_eq!(db.rag_count_kb_chunks("kb-1").await.unwrap(), 0);

        // delete
        db.rag_delete_kb("kb-1").await.unwrap();
        assert!(db.rag_get_kb("kb-1").await.unwrap().is_none());
        assert_eq!(db.rag_list_kbs().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn document_and_chunk_roundtrip() {
        let db = test_db().await;
        create_sample_kb(&db, "kb-d").await;

        // document
        db.rag_create_document("doc-1", "kb-d", "guide.md", "sha256:abc", "md")
            .await
            .unwrap();
        let doc = db.rag_get_document("doc-1").await.unwrap().unwrap();
        assert_eq!(doc.status, "pending");
        assert_eq!(doc.kb_id, "kb-d");
        assert_eq!(doc.chunk_count, 0);
        assert!(doc.error.is_none());

        // list
        db.rag_create_document("doc-2", "kb-d", "other.md", "sha256:def", "md")
            .await
            .unwrap();
        let docs = db.rag_list_documents("kb-d").await.unwrap();
        assert_eq!(docs.len(), 2);
        // 其它库不串扰
        create_sample_kb(&db, "kb-other").await;
        assert!(db.rag_list_documents("kb-other").await.unwrap().is_empty());

        // 批量插入分块（事务）
        let rows = vec![
            (
                "c-1".to_string(),
                "doc-1".to_string(),
                "kb-d".to_string(),
                0,
                "## 概述".to_string(),
                "这是第一段内容".to_string(),
                12,
            ),
            (
                "c-2".to_string(),
                "doc-1".to_string(),
                "kb-d".to_string(),
                1,
                "## 安装".to_string(),
                "这是第二段内容".to_string(),
                10,
            ),
        ];
        db.rag_insert_chunks(&rows).await.unwrap();

        // count
        assert_eq!(db.rag_count_kb_docs("kb-d").await.unwrap(), 2);
        assert_eq!(db.rag_count_kb_chunks("kb-d").await.unwrap(), 2);

        // 按 id 批量取（乱序验证 IN 子句）
        let chunks = db
            .rag_get_chunks_by_ids(&["c-2".to_string(), "c-1".to_string()])
            .await
            .unwrap();
        assert_eq!(chunks.len(), 2);
        let c2 = chunks.iter().find(|c| c.id == "c-2").unwrap();
        assert_eq!(c2.seq, 1);
        assert_eq!(c2.content, "这是第二段内容");

        // 空 ids
        assert!(db.rag_get_chunks_by_ids(&[]).await.unwrap().is_empty());

        // 更新文档状态
        db.rag_update_document_status("doc-1", "ready", 2, None)
            .await
            .unwrap();
        let doc = db.rag_get_document("doc-1").await.unwrap().unwrap();
        assert_eq!(doc.status, "ready");
        assert_eq!(doc.chunk_count, 2);

        // 失败状态带 error
        db.rag_update_document_status("doc-1", "failed", 0, Some("embedding timeout"))
            .await
            .unwrap();
        let doc = db.rag_get_document("doc-1").await.unwrap().unwrap();
        assert_eq!(doc.status, "failed");
        assert_eq!(doc.error.as_deref(), Some("embedding timeout"));

        // 按文档删分块
        db.rag_delete_chunks_by_doc("doc-1").await.unwrap();
        assert_eq!(db.rag_count_kb_chunks("kb-d").await.unwrap(), 0);
        assert!(db
            .rag_get_chunks_by_ids(&["c-1".to_string()])
            .await
            .unwrap()
            .is_empty());

        // 删除文档
        db.rag_delete_document("doc-1").await.unwrap();
        assert!(db.rag_get_document("doc-1").await.unwrap().is_none());

        // 删除知识库级联删文档
        db.rag_delete_kb("kb-d").await.unwrap();
        assert!(db.rag_list_documents("kb-d").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn document_roundtrip_carries_file_type() {
        let db = Database::new(":memory:").await.unwrap();
        db.rag_create_kb(&RagCreateKbOpts {
            id: "kb1".to_owned(),
            name: "n".to_owned(),
            description: String::new(),
            emb_base_url: "http://x".to_owned(),
            emb_api_key: "k".to_owned(),
            emb_model: "m".to_owned(),
            emb_dimension: 8,
            top_k: 5,
            chunk_size: 512,
            chunk_overlap: 64,
            score_threshold: 0.3,
            enabled: true,
        })
        .await
        .unwrap();
        db.rag_create_document("d1", "kb1", "a.pdf", "sha256:x", "pdf")
            .await
            .unwrap();
        let doc = db.rag_get_document("d1").await.unwrap().unwrap();
        assert_eq!(doc.file_type, "pdf");
    }

    #[tokio::test]
    async fn mark_pending_if_idle_cas() {
        let db = Database::new(":memory:").await.unwrap();
        db.rag_create_kb(&RagCreateKbOpts {
            id: "kb1".to_owned(),
            name: "n".to_owned(),
            description: String::new(),
            emb_base_url: "http://x".to_owned(),
            emb_api_key: "k".to_owned(),
            emb_model: "m".to_owned(),
            emb_dimension: 8,
            top_k: 5,
            chunk_size: 512,
            chunk_overlap: 64,
            score_threshold: 0.3,
            enabled: true,
        })
        .await
        .unwrap();
        db.rag_create_document("d1", "kb1", "a.md", "sha256:x", "md")
            .await
            .unwrap();

        // ready → pending 成功（返回 true）
        db.rag_update_document_status("d1", "ready", 5, None)
            .await
            .unwrap();
        assert!(db.rag_mark_document_pending_if_idle("d1").await.unwrap());
        let doc = db.rag_get_document("d1").await.unwrap().unwrap();
        assert_eq!(doc.status, "pending");
        assert_eq!(doc.chunk_count, 0);

        // 再次 CAS（已是 pending）→ 失败（返回 false），状态不变
        assert!(!db.rag_mark_document_pending_if_idle("d1").await.unwrap());
        let doc = db.rag_get_document("d1").await.unwrap().unwrap();
        assert_eq!(doc.status, "pending");

        // processing → 失败
        db.rag_update_document_status("d1", "processing", 0, None)
            .await
            .unwrap();
        assert!(!db.rag_mark_document_pending_if_idle("d1").await.unwrap());
        let doc = db.rag_get_document("d1").await.unwrap().unwrap();
        assert_eq!(doc.status, "processing");

        // failed → 成功
        db.rag_update_document_status("d1", "failed", 0, Some("boom"))
            .await
            .unwrap();
        assert!(db.rag_mark_document_pending_if_idle("d1").await.unwrap());
        let doc = db.rag_get_document("d1").await.unwrap().unwrap();
        assert_eq!(doc.status, "pending");
        assert!(doc.error.is_none());
    }

    #[tokio::test]
    async fn reconcile_fails_stale_inflight_docs() {
        let db = Database::new(":memory:").await.unwrap();
        db.rag_create_kb(&RagCreateKbOpts {
            id: "kb1".to_owned(),
            name: "n".to_owned(),
            description: String::new(),
            emb_base_url: "http://x".to_owned(),
            emb_api_key: "k".to_owned(),
            emb_model: "m".to_owned(),
            emb_dimension: 8,
            top_k: 5,
            chunk_size: 512,
            chunk_overlap: 64,
            score_threshold: 0.3,
            enabled: true,
        })
        .await
        .unwrap();
        db.rag_create_document("d-pending", "kb1", "a.md", "sha256:a", "md")
            .await
            .unwrap();
        db.rag_create_document("d-processing", "kb1", "b.md", "sha256:b", "md")
            .await
            .unwrap();
        db.rag_create_document("d-ready", "kb1", "c.md", "sha256:c", "md")
            .await
            .unwrap();
        db.rag_create_document("d-failed", "kb1", "e.md", "sha256:e", "md")
            .await
            .unwrap();
        // 构造非初始状态：processing/ready/failed 与各自存量数据
        db.rag_update_document_status("d-processing", "processing", 0, None)
            .await
            .unwrap();
        db.rag_update_document_status("d-ready", "ready", 3, None)
            .await
            .unwrap();
        db.rag_update_document_status("d-failed", "failed", 0, Some("old error"))
            .await
            .unwrap();

        // 只复位在途（pending/processing）行
        let reset = db
            .rag_fail_inflight_documents("interrupted by server restart")
            .await
            .unwrap();
        assert_eq!(reset, 2);

        // pending/processing → failed，带对账错误信息，chunk_count 清零
        for id in ["d-pending", "d-processing"] {
            let doc = db.rag_get_document(id).await.unwrap().unwrap();
            assert_eq!(doc.status, "failed", "{id} 应被复位为 failed");
            assert_eq!(
                doc.error.as_deref(),
                Some("interrupted by server restart"),
                "{id} 应带对账错误信息"
            );
            assert_eq!(doc.chunk_count, 0, "{id} chunk_count 应清零");
        }
        // 终态不受影响：ready 保留 chunk_count，failed 保留原 error
        let ready = db.rag_get_document("d-ready").await.unwrap().unwrap();
        assert_eq!(ready.status, "ready");
        assert_eq!(ready.chunk_count, 3);
        let failed = db.rag_get_document("d-failed").await.unwrap().unwrap();
        assert_eq!(failed.status, "failed");
        assert_eq!(
            failed.error.as_deref(),
            Some("old error"),
            "既有 failed 的 error 不应被对账覆盖"
        );
    }

    #[tokio::test]
    async fn migration_backfills_legacy_rows() {
        // 验证老数据回填规则：插入 file_type='' 的行（模拟迁移前的老数据），
        // 跑回填方法后应无条件为 'md'（旧版所有上传一律落盘 .md）。
        let db = Database::new(":memory:").await.unwrap();
        db.rag_create_kb(&RagCreateKbOpts {
            id: "kb2".to_owned(),
            name: "n".to_owned(),
            description: String::new(),
            emb_base_url: "http://x".to_owned(),
            emb_api_key: "k".to_owned(),
            emb_model: "m".to_owned(),
            emb_dimension: 8,
            top_k: 5,
            chunk_size: 512,
            chunk_overlap: 64,
            score_threshold: 0.3,
            enabled: true,
        })
        .await
        .unwrap();
        db.rag_create_document("legacy", "kb2", "old.md", "sha256:y", "")
            .await
            .unwrap();
        db.backfill_rag_document_file_type().await.unwrap();
        let doc = db.rag_get_document("legacy").await.unwrap().unwrap();
        assert_eq!(doc.file_type, "md");
    }

    #[tokio::test]
    async fn rag_update_kb_full_updates_emb_columns() {
        let db = test_db().await;
        create_sample_kb(&db, "kb-full").await;

        // 初始 emb 配置
        let kb = db.rag_get_kb("kb-full").await.unwrap().unwrap();
        assert_eq!(kb.emb_base_url, "https://api.example.com");
        assert_eq!(kb.emb_model, "text-embedding-3-small");
        assert_eq!(kb.emb_dimension, 1536);

        // 全量更新（含改名 + 改 emb 配置；api_key 已是密文形式）
        db.rag_update_kb_full(
            "kb-full",
            &RagUpdateKbFullOpts {
                name: "改名库".to_owned(),
                description: "新描述".to_owned(),
                top_k: 8,
                chunk_size: 256,
                chunk_overlap: 32,
                score_threshold: 0.5,
                emb_base_url: "https://new.example.com".to_owned(),
                emb_api_key: "enc:v1:newcipher".to_owned(),
                emb_model: "new-model".to_owned(),
                emb_dimension: 768,
            },
        )
        .await
        .unwrap();

        let kb = db.rag_get_kb("kb-full").await.unwrap().unwrap();
        // emb 列被正确更新
        assert_eq!(kb.emb_base_url, "https://new.example.com");
        assert_eq!(
            kb.emb_api_key, "enc:v1:newcipher",
            "api key 应是入参密文原样落库"
        );
        assert_eq!(kb.emb_model, "new-model");
        assert_eq!(kb.emb_dimension, 768);
        // 普通检索参数也同步更新（rag_update_kb_params 同口径）
        assert_eq!(kb.name, "改名库");
        assert_eq!(kb.description, "新描述");
        assert_eq!(kb.top_k, 8);
        assert_eq!(kb.chunk_size, 256);
        assert_eq!(kb.chunk_overlap, 32);
        assert!((kb.score_threshold - 0.5).abs() < 1e-9);
    }

    #[tokio::test]
    async fn rag_delete_chunks_by_kb_clears_chunks_but_keeps_docs() {
        let db = test_db().await;
        create_sample_kb(&db, "kb-dck").await;
        db.rag_create_document("d1", "kb-dck", "a.md", "sha256:a", "md")
            .await
            .unwrap();
        db.rag_create_document("d2", "kb-dck", "b.md", "sha256:b", "md")
            .await
            .unwrap();

        // 插入 d1 的分块
        db.rag_insert_chunks(&[(
            "c1".to_string(),
            "d1".to_string(),
            "kb-dck".to_string(),
            0,
            "## 概述".to_string(),
            "第一段".to_string(),
            12,
        )])
        .await
        .unwrap();
        assert_eq!(db.rag_count_kb_chunks("kb-dck").await.unwrap(), 1);

        // 按 kb 清空分块：分块归零，但文档行保留
        db.rag_delete_chunks_by_kb("kb-dck").await.unwrap();
        assert_eq!(db.rag_count_kb_chunks("kb-dck").await.unwrap(), 0);
        assert_eq!(db.rag_count_kb_docs("kb-dck").await.unwrap(), 2);
        assert_eq!(db.rag_list_documents("kb-dck").await.unwrap().len(), 2);

        // 其它库不串扰
        create_sample_kb(&db, "kb-other2").await;
        db.rag_create_document("d3", "kb-other2", "c.md", "sha256:c", "md")
            .await
            .unwrap();
        db.rag_insert_chunks(&[(
            "c2".to_string(),
            "d3".to_string(),
            "kb-other2".to_string(),
            0,
            "## 概述".to_string(),
            "其它库".to_string(),
            12,
        )])
        .await
        .unwrap();
        db.rag_delete_chunks_by_kb("kb-dck").await.unwrap();
        assert_eq!(db.rag_count_kb_chunks("kb-other2").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn migration_backfill_sets_txt_legacy_rows_to_md() {
        // 回归（Finding #2）：旧版 .txt 上传同样落盘为 <doc_id>.md（原文不保留
        // 扩展名）。若按 filename 扩展名推导，notes.txt 会被回填成 'txt'，
        // reindex 找 .txt 原文 409、delete 孤儿化真实 .md——回填必须锁定为 'md'。
        let db = Database::new(":memory:").await.unwrap();
        db.rag_create_kb(&RagCreateKbOpts {
            id: "kb3".to_owned(),
            name: "n".to_owned(),
            description: String::new(),
            emb_base_url: "http://x".to_owned(),
            emb_api_key: "k".to_owned(),
            emb_model: "m".to_owned(),
            emb_dimension: 8,
            top_k: 5,
            chunk_size: 512,
            chunk_overlap: 64,
            score_threshold: 0.3,
            enabled: true,
        })
        .await
        .unwrap();
        db.rag_create_document("legacy-txt", "kb3", "notes.txt", "sha256:z", "")
            .await
            .unwrap();
        db.backfill_rag_document_file_type().await.unwrap();
        let doc = db.rag_get_document("legacy-txt").await.unwrap().unwrap();
        assert_eq!(doc.file_type, "md");
    }
}
