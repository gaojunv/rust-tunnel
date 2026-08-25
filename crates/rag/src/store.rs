//! qdrant-edge 向量存储封装（Task 4）。
//!
//! 每个知识库一个 shard，目录 `<data_dir>/rag/<kb_id>/`，按需打开并缓存
//! （`Arc<Mutex<HashMap<kb_id, EdgeShard>>>`），维度在首次打开时建立。
//! 向量本体存于此；可检索原文与元数据在 SQLite（rag_chunks），此处 payload
//! 只存定位信息（id/doc_id/seq/heading_path）。
//!
//! 注意：`EdgeShard` 析构时会同步 flush（内部 `expect()`，不可在包装层拦截）。
//! 若 shard 目录已被删除（`delete_kb`）而仍有在途 `Arc` 克隆未析构，flush 会
//! 任务级 panic —— 调用方必须保证 `delete_kb` 与 upsert/search/delete_by_doc
//! 不并发操作同一 kb_id（见 `delete_kb` 文档）。
//!
//! qdrant-edge 的 `PointStruct::new` 要求 id 为 `ExtendedPointId`（u64 或
//! UUID），不接受任意字符串。因此 `ChunkPoint.id` 先映射为 PointId（数字/UUID
//! 字符串原样解析，其余 FNV-1a 哈希落为 NumId），原始 id 字符串同时存入
//! payload["id"]，检索时从 payload 原样返回 —— 保证 id 字符串无损往返。

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Mutex;

#[derive(Debug)]
pub enum StoreError {
    Qdrant(String),
    Io(std::io::Error),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Qdrant(m) => write!(f, "qdrant edge error: {m}"),
            Self::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl StoreError {
    /// 是否为 `open_shard` 的维度不匹配错误（缓存命中时校验产生）。
    /// `search` 用它区分降级路径：维度不匹配值得告警，其余失败静默降级。
    fn is_dim_mismatch(&self) -> bool {
        matches!(self, Self::Qdrant(m) if m.contains("dim mismatch"))
    }
}

/// 一个待写入的分块向量点。
#[derive(Debug, Clone)]
pub struct ChunkPoint {
    /// = rag_chunks.id（UUID）。
    pub id: String,
    pub vector: Vec<f32>,
    pub doc_id: String,
    pub seq: i64,
    pub heading_path: String,
}

/// 检索命中的点（只回传定位 id 与分数，原文按 id 回 SQLite 查）。
#[derive(Debug, Clone)]
pub struct ScoredPoint {
    pub id: String,
    pub score: f32,
}

/// 知识库向量存储：`<data_dir>/rag/<kb_id>/` 一个 EdgeShard，按需打开并缓存。
#[derive(Clone)]
pub struct VectorStore {
    data_dir: PathBuf,
    shards: Arc<Mutex<HashMap<String, Arc<qdrant_edge::EdgeShard>>>>,
}

// 手动实现 Debug（EdgeShard 不实现 Debug，且其内部状态在锁内）：只暴露 data_dir。
impl fmt::Debug for VectorStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VectorStore")
            .field("data_dir", &self.data_dir)
            .finish_non_exhaustive()
    }
}

impl VectorStore {
    #[must_use] 
    pub fn new(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            shards: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 数据根目录：`rag/<kb_id>/` 存向量 shard，`rag_docs/<kb_id>/` 存文档原文
    /// （管理 API 层读写原文用，见 `mgmt/api/rag.rs`）。
    #[must_use] 
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    fn shard_dir(&self, kb_id: &str) -> PathBuf {
        self.data_dir.join("rag").join(kb_id)
    }

    /// 打开（或取缓存的）shard；首次创建时用 `dim` 建立维度。
    ///
    /// 缓存命中时校验请求 `dim` 与 shard 实际维度一致：同一进程内换 embedding
    /// 模型重索引（同 kb 换 dim）时，若忽略此校验，后续 upsert 会在 qdrant-edge
    /// 内部 `assert_eq!("Vector size mismatch")` 处 panic。不一致返回可恢复的
    /// `StoreError`。注意 shard Drop 时会同步 flush 并 `expect()`。
    async fn open_shard(
        &self,
        kb_id: &str,
        dim: usize,
    ) -> Result<Arc<qdrant_edge::EdgeShard>, StoreError> {
        let mut cache = self.shards.lock().await;
        if let Some(s) = cache.get(kb_id) {
            let shard_dim = s.config().vectors.get("").map(|p| p.size);
            if shard_dim != Some(dim) {
                return Err(StoreError::Qdrant(format!(
                    "kb {kb_id} dim mismatch: shard has {shard_dim:?}, requested {dim}"
                )));
            }
            return Ok(s.clone());
        }
        let dir = self.shard_dir(kb_id);
        std::fs::create_dir_all(&dir)?;
        let config = qdrant_edge::EdgeConfigBuilder::new()
            .vector(
                "",
                qdrant_edge::EdgeVectorParamsBuilder::new(dim, qdrant_edge::Distance::Cosine)
                    .build(),
            )
            .build();
        let shard = qdrant_edge::EdgeShard::load(&dir, Some(config))
            .map_err(|e| StoreError::Qdrant(format!("{e}")))?;
        let shard = Arc::new(shard);
        cache.insert(kb_id.to_string(), shard.clone());
        Ok(shard)
    }

    /// 批量写入（或覆盖）分块向量。
    pub async fn upsert(
        &self,
        kb_id: &str,
        dim: usize,
        points: Vec<ChunkPoint>,
    ) -> Result<(), StoreError> {
        if points.is_empty() {
            return Ok(());
        }
        let shard = self.open_shard(kb_id, dim).await?;
        let pts: Vec<qdrant_edge::PointStructPersisted> = points
            .into_iter()
            .map(|p| {
                let point_id = store_point_id(&p.id);
                let payload = serde_json::json!({
                    "id": p.id,
                    "doc_id": p.doc_id,
                    "seq": p.seq,
                    "heading_path": p.heading_path,
                });
                qdrant_edge::PointStruct::new(point_id, p.vector, payload).into()
            })
            .collect();
        shard
            .update(qdrant_edge::UpdateOperation::PointOperation(
                qdrant_edge::PointOperations::UpsertPoints(
                    qdrant_edge::PointInsertOperations::PointsList(pts),
                ),
            ))
            .map_err(|e| StoreError::Qdrant(format!("{e}")))?;
        Ok(())
    }

    /// 向量检索，返回分数降序的命中。检索失败（shard 不存在等）降级为空，
    /// 由调用方决定；这是设计意图，不向上抛错。缓存命中但维度不匹配（同进程
    /// 换 dim 重索引）时同样降级为空，但以 `tracing::warn!` 记录以便排查。
    pub async fn search(
        &self,
        kb_id: &str,
        dim: usize,
        query: &[f32],
        top_k: usize,
    ) -> Vec<ScoredPoint> {
        let shard = match self.open_shard(kb_id, dim).await {
            Ok(s) => s,
            Err(e) if e.is_dim_mismatch() => {
                tracing::warn!(kb_id, dim, error = %e, "rag search skipped: kb dim mismatch");
                return Vec::new();
            }
            Err(_) => return Vec::new(),
        };
        let req = qdrant_edge::QueryRequest {
            prefetches: vec![],
            query: Some(qdrant_edge::ScoringQuery::Vector(
                qdrant_edge::QueryEnum::Nearest(qdrant_edge::NamedQuery::default_dense(
                    query.to_vec(),
                )),
            )),
            filter: None,
            score_threshold: None,
            limit: top_k,
            offset: 0,
            params: None,
            with_vector: qdrant_edge::WithVector::Bool(false),
            with_payload: qdrant_edge::WithPayloadInterface::Bool(true),
        };
        match shard.query(req) {
            Ok(res) => res
                .into_iter()
                .map(|s| ScoredPoint {
                    // 原始 id 字符串从 payload 回读，PointId 仅作内部定位
                    id: s
                        .payload
                        .as_ref()
                        .and_then(|p| p.0.get("id"))
                        .and_then(|v| v.as_str()).map_or_else(|| s.id.to_string(), str::to_string),
                    score: s.score,
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// 删除某文档在该知识库的所有分块（按 payload doc_id 过滤）。
    pub async fn delete_by_doc(
        &self,
        kb_id: &str,
        dim: usize,
        doc_id: &str,
    ) -> Result<(), StoreError> {
        let shard = self.open_shard(kb_id, dim).await?;
        let key = "doc_id"
            .parse::<qdrant_edge::JsonPath>()
            .map_err(|()| StoreError::Qdrant("invalid filter key 'doc_id'".into()))?;
        let filter = qdrant_edge::Filter::new_must(qdrant_edge::Condition::Field(
            qdrant_edge::FieldCondition::new_match(
                key,
                qdrant_edge::Match::new_value(qdrant_edge::ValueVariants::String(
                    doc_id.to_string(),
                )),
            ),
        ));
        shard
            .update(qdrant_edge::UpdateOperation::PointOperation(
                qdrant_edge::PointOperations::DeletePointsByFilter(filter),
            ))
            .map_err(|e| StoreError::Qdrant(format!("{e}")))?;
        Ok(())
    }

    /// 删除整个知识库：先从缓存移除（释放 shard 句柄），再删目录。
    ///
    /// # 并发约束
    ///
    /// 调用方必须保证删除时**无在途** `upsert`/`search`/`delete_by_doc`
    /// 操作同一 `kb_id`。`EdgeShard` 析构时同步 flush（内部 `expect()`，
    /// 包装层无法拦截）：若在途操作持有的 `Arc<EdgeShard>` 在目录删除后才
    /// 析构，flush 对已删目录失败会任务级 panic；flush 时 WAL 锁被并发
    /// update 持有时同样 panic。此约束由调用方（delete-KB API）串行化保证。
    pub async fn delete_kb(&self, kb_id: &str) -> Result<(), StoreError> {
        self.shards.lock().await.remove(kb_id);
        let dir = self.shard_dir(kb_id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        Ok(())
    }
}

/// 把 `ChunkPoint.id` 映射为 qdrant 点 id：数字/UUID 字符串原样解析，
/// 其余用 FNV-1a 哈希落为 NumId（确定性，幂等 upsert 可用）。
fn store_point_id(id: &str) -> qdrant_edge::PointId {
    match id.parse::<qdrant_edge::PointId>() {
        Ok(pid) => pid,
        Err(()) => qdrant_edge::PointId::NumId(fnv1a64(id)),
    }
}

fn fnv1a64(s: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for b in s.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TempDir 放前、store 放后：qadrant-edge 的 `EdgeShard` Drop 时同步 flush
    /// 并 `expect()`（目录已删会 panic），故 store 必须先于 TempDir 析构。
    fn tmp_store() -> (tempfile::TempDir, VectorStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = VectorStore::new(dir.path());
        (dir, store)
    }

    fn pt(id: &str, vec: Vec<f32>, doc: &str, seq: i64) -> ChunkPoint {
        ChunkPoint {
            id: id.into(),
            vector: vec,
            doc_id: doc.into(),
            seq,
            heading_path: "h".into(),
        }
    }

    #[tokio::test]
    async fn upsert_then_search_returns_nearest() {
        let (_d, store) = tmp_store();
        let dim = 4;
        store
            .upsert(
                "kb1",
                dim,
                vec![
                    pt("a", vec![1.0, 0.0, 0.0, 0.0], "doc1", 0),
                    pt("b", vec![0.0, 1.0, 0.0, 0.0], "doc1", 1),
                ],
            )
            .await
            .unwrap();
        let hits = store.search("kb1", dim, &[1.0, 0.0, 0.0, 0.0], 2).await;
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, "a", "最近邻应排第一");
        assert!(hits[0].score > hits[1].score);
    }

    #[tokio::test]
    async fn upsert_dim_mismatch_returns_err_instead_of_panic() {
        let (_d, store) = tmp_store();
        store
            .upsert("kb1", 4, vec![pt("a", vec![1.0, 0.0, 0.0, 0.0], "doc", 0)])
            .await
            .unwrap();
        // 同进程内同 kb 换 dim：open_shard 缓存命中校验应返回 Err，而非在
        // qdrant-edge 内部 assert_eq!("Vector size mismatch") 处 panic。
        let err = store
            .upsert("kb1", 8, vec![pt("b", vec![0.0; 8], "doc", 1)])
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("dim mismatch"),
            "expected dim mismatch err, got {err}"
        );
    }

    #[tokio::test]
    async fn delete_by_doc_removes_only_that_doc() {
        let (_d, store) = tmp_store();
        let dim = 4;
        store
            .upsert(
                "kb1",
                dim,
                vec![
                    pt("a", vec![1.0, 0.0, 0.0, 0.0], "doc1", 0),
                    pt("b", vec![0.9, 0.1, 0.0, 0.0], "doc2", 0),
                ],
            )
            .await
            .unwrap();
        store.delete_by_doc("kb1", dim, "doc1").await.unwrap();
        let hits = store.search("kb1", dim, &[1.0, 0.0, 0.0, 0.0], 10).await;
        assert!(hits.iter().all(|h| h.id != "a"));
        assert!(hits.iter().any(|h| h.id == "b"));
    }

    #[tokio::test]
    async fn search_empty_kb_returns_empty() {
        let (_d, store) = tmp_store();
        let hits = store.search("nope", 4, &[1.0, 0.0, 0.0, 0.0], 5).await;
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn delete_kb_removes_shard_dir() {
        let (d, store) = tmp_store();
        store
            .upsert("kbX", 4, vec![pt("a", vec![1.0, 0.0, 0.0, 0.0], "doc", 0)])
            .await
            .unwrap();
        assert!(d.path().join("rag/kbX").exists());
        store.delete_kb("kbX").await.unwrap();
        assert!(!d.path().join("rag/kbX").exists());
    }
}
