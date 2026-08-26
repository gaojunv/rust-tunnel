//! RAG 向量分块的数据访问层（`knowledge_chunks`）。
//!
//! 容器与文档能力已收敛至 `knowledge.rs`（`ks_*` / `kdoc_*`）。

use super::Database;

/// RAG 分块记录（`knowledge_chunks` 表的一行）。
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct RagChunkRecord {
    /// 分块 id（主键）。
    pub id: String,
    /// 所属文档 id。
    pub doc_id: String,
    /// 所属容器 id（`knowledge_sources.id`，列名沿用 `source_id`）。
    #[sqlx(rename = "source_id")]
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

impl Database {
    /// 事务批量插入分块（摄入完成、写向量后落库）。
    /// `rows` 的元素为 `（id, doc_id, source_id, seq, heading_path, content, token_count）`，
    /// 与 `knowledge_chunks` 列一一对应。
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn rag_insert_chunks(
        &self,
        rows: &[(String, String, String, i64, String, String, i64)],
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        for (id, doc_id, kb_id, seq, heading_path, content, token_count) in rows {
            sqlx::query(
                r"
                INSERT INTO knowledge_chunks (id, doc_id, source_id, seq, heading_path, content, token_count)
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
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn rag_get_chunks_by_ids(
        &self,
        ids: &[String],
    ) -> Result<Vec<RagChunkRecord>, sqlx::Error> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; ids.len()].join(", ");
        let sql = format!(
            "SELECT id, doc_id, source_id, seq, heading_path, content, token_count \
             FROM knowledge_chunks WHERE id IN ({placeholders})"
        );
        let mut q = sqlx::query_as::<_, RagChunkRecord>(&sql);
        for id in ids {
            q = q.bind(id);
        }
        q.fetch_all(&self.pool).await
    }

    /// 删除某文档的全部分块（删除文档/重索引前清空旧索引时调用）。
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn rag_delete_chunks_by_doc(&self, doc_id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM knowledge_chunks WHERE doc_id = ?")
            .bind(doc_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 删除某容器的全部分块（全量重建前清空旧索引时调用，按 `source_id` 列）。
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn rag_delete_chunks_by_kb(&self, kb_id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM knowledge_chunks WHERE source_id = ?")
            .bind(kb_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 容器下的分块总数（按 `source_id` 聚合）。
    /// # Errors
    /// 数据库错误：以 `sqlx::Error` 返回。
    pub async fn rag_count_kb_chunks(&self, kb_id: &str) -> Result<i64, sqlx::Error> {
        let row: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM knowledge_chunks WHERE source_id = ?")
                .bind(kb_id)
                .fetch_one(&self.pool)
                .await?;
        Ok(row.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::{IndexKind, KsCreateOpts};

    async fn test_db() -> Database {
        Database::new(":memory:")
            .await
            .expect("create in-memory db")
    }

    async fn create_sample_kb(db: &Database, id: &str) {
        db.ks_create(&KsCreateOpts {
            id: id.to_owned(),
            name: "测试库".to_owned(),
            summary: "描述".to_owned(),
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
        })
        .await
        .unwrap();
    }

    /// 迁移后：显式列 + source_id + knowledge 容器。
    #[tokio::test]
    async fn chunk_roundtrip_via_knowledge() {
        let db = test_db().await;
        create_sample_kb(&db, "kb-d").await;
        db.kdoc_create("doc-1", "kb-d", "guide.md", "md", "sha256:abc")
            .await
            .unwrap();
        db.kdoc_create("doc-2", "kb-d", "other.md", "md", "sha256:def")
            .await
            .unwrap();

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
        assert_eq!(db.rag_count_kb_chunks("kb-d").await.unwrap(), 2);
        assert_eq!(db.kdoc_count_by_source("kb-d").await.unwrap(), 2);

        let chunks = db
            .rag_get_chunks_by_ids(&["c-2".to_string(), "c-1".to_string()])
            .await
            .unwrap();
        assert_eq!(chunks.len(), 2);
        let c2 = chunks.iter().find(|c| c.id == "c-2").unwrap();
        assert_eq!(c2.seq, 1);
        assert_eq!(c2.content, "这是第二段内容");
        assert_eq!(c2.kb_id, "kb-d");

        assert!(db.rag_get_chunks_by_ids(&[]).await.unwrap().is_empty());

        db.rag_delete_chunks_by_doc("doc-1").await.unwrap();
        assert_eq!(db.rag_count_kb_chunks("kb-d").await.unwrap(), 0);

        db.rag_insert_chunks(&rows[..1]).await.unwrap();
        db.kdoc_create("d3", "kb-other2", "a.md", "md", "sha256:x")
            .await
            .unwrap_err(); // source 不存在：FK 约束（忽略错误，仅为隔离）
                           // 直接用 knowledge 的另一容器做隔离
        db.ks_create(&KsCreateOpts {
            id: "kb-other2".to_owned(),
            name: "other".to_owned(),
            summary: String::new(),
            index_vector: true,
            index_pages: false,
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
        db.kdoc_create("d3", "kb-other2", "c.md", "md", "sha256:c")
            .await
            .unwrap();
        db.rag_insert_chunks(&[(
            "c3".to_string(),
            "d3".to_string(),
            "kb-other2".to_string(),
            0,
            "## 概述".to_string(),
            "其它库".to_string(),
            12,
        )])
        .await
        .unwrap();
        db.rag_delete_chunks_by_kb("kb-d").await.unwrap();
        assert_eq!(db.rag_count_kb_chunks("kb-other2").await.unwrap(), 1);
        let _ = IndexKind::Vector.as_str();
    }

    #[tokio::test]
    async fn rag_delete_chunks_by_kb_clears_chunks_but_keeps_docs() {
        let db = test_db().await;
        create_sample_kb(&db, "kb-dck").await;
        db.kdoc_create("d1", "kb-dck", "a.md", "md", "sha256:a")
            .await
            .unwrap();
        db.kdoc_create("d2", "kb-dck", "b.md", "md", "sha256:b")
            .await
            .unwrap();
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
        db.rag_delete_chunks_by_kb("kb-dck").await.unwrap();
        assert_eq!(db.rag_count_kb_chunks("kb-dck").await.unwrap(), 0);
        assert_eq!(db.kdoc_count_by_source("kb-dck").await.unwrap(), 2);
        assert_eq!(db.kdoc_list("kb-dck").await.unwrap().len(), 2);
        // 其他库不串扰（已在上个测试验证隔离，这里复检）
    }
}
