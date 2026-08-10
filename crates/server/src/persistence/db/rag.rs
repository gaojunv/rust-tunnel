//! RAG 知识库的数据访问层（向量本体在 qdrant-edge，此处存元数据与原文）。

use super::Database;

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct RagKnowledgeBaseRecord {
    pub id: String,
    pub name: String,
    pub description: String,
    pub emb_base_url: String,
    pub emb_api_key: String, // 加密存储
    pub emb_model: String,
    pub emb_dimension: i64,
    pub top_k: i64,
    pub chunk_size: i64,
    pub chunk_overlap: i64,
    pub score_threshold: f64,
    pub enabled: i32,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct RagDocumentRecord {
    pub id: String,
    pub kb_id: String,
    pub filename: String,
    pub file_type: String,
    pub content_hash: String,
    pub status: String,
    pub chunk_count: i64,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct RagChunkRecord {
    pub id: String,
    pub doc_id: String,
    pub kb_id: String,
    pub seq: i64,
    pub heading_path: String,
    pub content: String,
    pub token_count: i64,
}

impl Database {
    // ── Knowledge base CRUD ──────────────────────────────────────

    /// 创建知识库。emb_api_key 的加解密由调用方（mgmt api 层）用
    /// encrypt_field/decrypt_field 处理，本层只存取原始字符串。
    #[allow(clippy::too_many_arguments)]
    pub async fn rag_create_kb(
        &self,
        id: &str,
        name: &str,
        description: &str,
        emb_base_url: &str,
        emb_api_key: &str,
        emb_model: &str,
        emb_dimension: i64,
        top_k: i64,
        chunk_size: i64,
        chunk_overlap: i64,
        score_threshold: f64,
        enabled: bool,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO rag_knowledge_bases (
                id, name, description, emb_base_url, emb_api_key, emb_model,
                emb_dimension, top_k, chunk_size, chunk_overlap, score_threshold,
                enabled, created_at, updated_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, datetime('now'), datetime('now'))
            "#,
        )
        .bind(id)
        .bind(name)
        .bind(description)
        .bind(emb_base_url)
        .bind(emb_api_key)
        .bind(emb_model)
        .bind(emb_dimension)
        .bind(top_k)
        .bind(chunk_size)
        .bind(chunk_overlap)
        .bind(score_threshold)
        .bind(enabled as i32)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

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

    pub async fn rag_list_kbs(&self) -> Result<Vec<RagKnowledgeBaseRecord>, sqlx::Error> {
        sqlx::query_as::<_, RagKnowledgeBaseRecord>(
            "SELECT * FROM rag_knowledge_bases ORDER BY created_at",
        )
        .fetch_all(&self.pool)
        .await
    }

    /// 更新知识库的「名称 + 检索/分块参数」。emb 配置（base_url/api_key/model/dimension）
    /// 建库后锁定不可改（qdrant shard 维度固定），需要改动时删除重建。
    #[allow(clippy::too_many_arguments)]
    pub async fn rag_update_kb_params(
        &self,
        id: &str,
        name: &str,
        description: &str,
        top_k: i64,
        chunk_size: i64,
        chunk_overlap: i64,
        score_threshold: f64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            UPDATE rag_knowledge_bases
            SET name = ?, description = ?, top_k = ?, chunk_size = ?, chunk_overlap = ?,
                score_threshold = ?, updated_at = datetime('now')
            WHERE id = ?
            "#,
        )
        .bind(name)
        .bind(description)
        .bind(top_k)
        .bind(chunk_size)
        .bind(chunk_overlap)
        .bind(score_threshold)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn rag_toggle_kb(&self, id: &str, enabled: bool) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE rag_knowledge_bases SET enabled = ?, updated_at = datetime('now') WHERE id = ?",
        )
        .bind(enabled as i32)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

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
    pub async fn rag_create_document(
        &self,
        id: &str,
        kb_id: &str,
        filename: &str,
        content_hash: &str,
        file_type: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO rag_documents (id, kb_id, filename, file_type, content_hash, status, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, 'pending', datetime('now'), datetime('now'))
            "#,
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
    pub async fn backfill_rag_document_file_type(&self) -> Result<(), sqlx::Error> {
        sqlx::query(Self::BACKFILL_RAG_DOCUMENT_FILE_TYPE_SQL)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

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
    pub async fn rag_update_document_status(
        &self,
        doc_id: &str,
        status: &str,
        chunk_count: i64,
        error: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            UPDATE rag_documents
            SET status = ?, chunk_count = ?, error = ?, updated_at = datetime('now')
            WHERE id = ?
            "#,
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
    pub async fn rag_mark_document_pending_if_idle(
        &self,
        doc_id: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r#"
            UPDATE rag_documents
            SET status = 'pending', chunk_count = 0, error = NULL, updated_at = datetime('now')
            WHERE id = ? AND status NOT IN ('pending', 'processing')
            "#,
        )
        .bind(doc_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

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
    pub async fn rag_insert_chunks(
        &self,
        rows: &[(String, String, String, i64, String, String, i64)],
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        for (id, doc_id, kb_id, seq, heading_path, content, token_count) in rows {
            sqlx::query(
                r#"
                INSERT INTO rag_chunks (id, doc_id, kb_id, seq, heading_path, content, token_count)
                VALUES (?, ?, ?, ?, ?, ?, ?)
                "#,
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
    pub async fn rag_delete_chunks_by_doc(&self, doc_id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM rag_chunks WHERE doc_id = ?")
            .bind(doc_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ── Counts ───────────────────────────────────────────────────

    /// 知识库下的文档总数（含 pending/failed）。
    pub async fn rag_count_kb_docs(&self, kb_id: &str) -> Result<i64, sqlx::Error> {
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM rag_documents WHERE kb_id = ?")
            .bind(kb_id)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.0)
    }

    /// 知识库下的分块总数（冗余 kb_id 列按库聚合）。
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
            assert_eq!(row.0, 1, "table {} should exist", t);
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
        db.rag_create_kb(
            id,
            "测试库",
            "描述",
            "https://api.example.com",
            "sk-encrypted",
            "text-embedding-3-small",
            1536,
            5,
            512,
            64,
            0.3,
            true,
        )
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
            .rag_create_kb(
                "kb-1",
                "x",
                "",
                "https://api.example.com",
                "k",
                "m",
                10,
                5,
                512,
                64,
                0.3,
                true,
            )
            .await;
        assert!(dup.is_err(), "同 id 重复创建应冲突");

        // update params：emb 配置不变
        db.rag_update_kb_params("kb-1", "改名", "新描述", 8, 256, 32, 0.5)
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
        db.rag_create_kb(
            "kb1", "n", "", "http://x", "k", "m", 8, 5, 512, 64, 0.3, true,
        )
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
        db.rag_create_kb(
            "kb1", "n", "", "http://x", "k", "m", 8, 5, 512, 64, 0.3, true,
        )
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
    async fn migration_backfills_legacy_rows() {
        // 验证老数据回填规则：插入 file_type='' 的行（模拟迁移前的老数据），
        // 跑回填方法后应无条件为 'md'（旧版所有上传一律落盘 .md）。
        let db = Database::new(":memory:").await.unwrap();
        db.rag_create_kb(
            "kb2", "n", "", "http://x", "k", "m", 8, 5, 512, 64, 0.3, true,
        )
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
    async fn migration_backfill_sets_txt_legacy_rows_to_md() {
        // 回归（Finding #2）：旧版 .txt 上传同样落盘为 <doc_id>.md（原文不保留
        // 扩展名）。若按 filename 扩展名推导，notes.txt 会被回填成 'txt'，
        // reindex 找 .txt 原文 409、delete 孤儿化真实 .md——回填必须锁定为 'md'。
        let db = Database::new(":memory:").await.unwrap();
        db.rag_create_kb(
            "kb3", "n", "", "http://x", "k", "m", 8, 5, 512, 64, 0.3, true,
        )
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
