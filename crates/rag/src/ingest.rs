//! 统一文档摄入后台任务：**提取一次** → 双索引分叉（向量 / pages）→ per-kind
//! 状态落库 + SSE 事件。
//!
//! 一篇文档可同时进两个索引（容器的 `index_vector` / `index_pages` 开关决定）。
//! 两侧唯一共享的步骤是文本提取——PDF/OOXML 解析是流水线里最贵的一步，做两遍
//! 纯属浪费。**分块不共享**：向量侧用容器配置的 `chunk_size`/`chunk_overlap`
//! （为检索精度调的，默认 512/64），pages 侧固定 [`PAGES_CHUNK_SIZE`]/
//! [`PAGES_CHUNK_OVERLAP`]（LLM 结构化抽取需要更大上下文，切太碎会让批次数量和
//! LLM 调用成本成倍上升）。这两个参数语义不同，强行统一会牺牲一侧。
//!
//! pages 侧的页面抽取需要调 LLM，而本 crate 不依赖 llm crate（llm 依赖 rag，
//! 反向依赖会成环）。故抽取能力经 [`PageExtractor`] trait 由调用方注入。

use super::{
    chunker,
    embedder::Embedder,
    extractor::{self, FileType},
    store::{ChunkPoint, VectorStore},
};
use futures_util::FutureExt;
use rust_tunnel_common::crypto::{decrypt_field, LlmCipher};
use rust_tunnel_persistence::knowledge::{IndexKind, KnowledgeSourceRecord};
use rust_tunnel_persistence::Database;
use std::sync::Arc;
use tokio::sync::broadcast;

/// 索引项状态变更事件（SSE 推送给前端）。
///
/// `kind` 区分是哪个索引在变——同一篇文档的两个索引各自独立推进，前端按
/// `(doc_id, kind)` 对账。`chunk_count` 是通用的"索引项数"：向量侧为分块数，
/// pages 侧为页数。
#[derive(Debug, Clone, serde::Serialize)]
pub struct KbEvent {
    /// 文档 ID。
    pub doc_id: String,
    /// 所属容器 ID。
    pub kb_id: String,
    /// 变更的索引种类。
    pub kind: IndexKind,
    /// 索引项状态：`processing` / `ready` / `failed`。
    pub status: String,
    /// 索引项数：向量侧为分块数，pages 侧为页数。
    pub chunk_count: i64,
    /// 失败原因（`status != "failed"` 时为 `None`）。
    pub error: Option<String>,
}

/// pages 索引的抽取产物：一个结构化页面。
///
/// `page_ref` 为**已归一化**的引用（调用方在实现 [`PageExtractor`] 时应先过
/// `normalize_wiki_ref`）；本模块落库前会再归一化一次并丢弃非法 ref。
#[derive(Debug, Clone)]
pub struct ExtractedPage {
    /// 页面引用（kebab-case 标识，容器内唯一）。
    pub page_ref: String,
    /// 页面标题。
    pub title: String,
    /// 一句话摘要。
    pub summary: String,
    /// 页面正文（Markdown）。
    pub content: String,
}

/// pages 索引的页面抽取器：把一批 Markdown 文本提炼为结构化页面。
///
/// 实现方（`agent` crate 的 `LlmPageExtractor`）负责 prompt、LLM 调用与 JSON
/// 解析；批次切分、失败重试与落库截断由本模块的流水线统一处理，使这些策略
/// 可在一处测试。
#[async_trait::async_trait]
pub trait PageExtractor: Send + Sync {
    /// 抽取单批次文本的页面。返回 `Err` 表示本批次失败（流水线会重试一次）。
    async fn extract_pages(&self, batch_text: &str) -> Result<Vec<ExtractedPage>, String>;
}

/// 单批次 LLM 输入的 chunk 数上限与 token 上限。
const MAX_CHUNKS_PER_BATCH: usize = 3;
const MAX_TOKENS_PER_BATCH: usize = 6000;
/// 单批次抽取失败后的总尝试次数（1 次重试）。
const BATCH_ATTEMPTS: usize = 2;
/// pages 落库前的字段上限（字符）。
const PAGE_CONTENT_MAX_CHARS: usize = 4000;
const TITLE_MAX_CHARS: usize = 64;
const SUMMARY_MAX_CHARS: usize = 200;
/// pages 侧的分块参数：LLM 抽取需要比向量检索更大的上下文窗口，故不用容器的
/// `chunk_size`/`chunk_overlap`（见模块文档）。
const PAGES_CHUNK_SIZE: usize = 1200;
const PAGES_CHUNK_OVERLAP: usize = 150;

/// 摄入任务参数。
///
/// 字段多是本质的（跨三个子系统的基础设施 + 两侧限流 + 索引选择），用 Opts
/// 结构体而非长参数列表。
pub struct IngestOpts {
    /// 数据库句柄（文档/索引项状态、chunks、pages 落库）。
    pub db: Database,
    /// 向量 shard 句柄（仅向量侧使用）。
    pub store: VectorStore,
    /// 字段解密器（解容器的 emb API key，仅向量侧使用）。
    pub cipher: Option<LlmCipher>,
    /// 容器记录：`index_vector`/`index_pages` 决定跑哪些索引，emb\_\* 供向量侧使用。
    pub source: KnowledgeSourceRecord,
    /// 待摄入文档 ID。
    pub doc_id: String,
    /// 已落盘的原始文件路径（`<data_dir>/knowledge_docs/<source_id>/<doc_id>.<ext>`）。
    pub source_path: std::path::PathBuf,
    /// 文件类型，决定用哪个提取器。
    pub file_type: FileType,
    /// 状态事件出口。
    pub tx: broadcast::Sender<KbEvent>,
    /// 向量侧限流：全量重建时注入，护住远端 embedding 服务。`None` 不限。
    pub vector_sem: Option<Arc<tokio::sync::Semaphore>>,
    /// pages 侧限流：LLM 调用远比 embedding 重，独立且应更窄。`None` 不限。
    pub pages_sem: Option<Arc<tokio::sync::Semaphore>>,
    /// pages 抽取器。容器开了 pages 索引却为 `None` 时该侧置 failed
    /// （这是装配错误，不该静默跳过让文档永远停在 pending）。
    pub page_extractor: Option<Arc<dyn PageExtractor>>,
    /// 只跑指定索引（单侧 reindex）。`None` = 跑容器启用的全部索引。
    pub only: Option<IndexKind>,
}

/// 按容器开关与 `only` 求要跑的索引集合。
fn planned_kinds(source: &KnowledgeSourceRecord, only: Option<IndexKind>) -> Vec<IndexKind> {
    let mut v = Vec::with_capacity(2);
    if source.index_vector != 0 {
        v.push(IndexKind::Vector);
    }
    if source.index_pages != 0 {
        v.push(IndexKind::Pages);
    }
    if let Some(k) = only {
        v.retain(|x| *x == k);
    }
    v
}

/// 发一条索引状态事件。接收端关闭（无订阅者）时静默丢弃——事件是旁路通知，
/// 状态的真相源是 DB。
fn emit(
    tx: &broadcast::Sender<KbEvent>,
    doc_id: &str,
    source_id: &str,
    kind: IndexKind,
    status: &str,
    count: i64,
    error: Option<String>,
) {
    let _ = tx.send(KbEvent {
        doc_id: doc_id.to_owned(),
        kb_id: source_id.to_owned(),
        kind,
        status: status.to_owned(),
        chunk_count: count,
        error,
    });
}

/// 收尾单个索引：落库状态 + 发事件；pages 侧顺带同步容器级 status。
async fn finish_kind(
    db: &Database,
    tx: &broadcast::Sender<KbEvent>,
    source_id: &str,
    doc_id: &str,
    kind: IndexKind,
    result: Result<i64, String>,
) {
    match result {
        Ok(count) => {
            let _ = db
                .kdoc_update_index_status(doc_id, kind, "ready", count, None)
                .await;
            sync_source_status(db, source_id, kind, "ready").await;
            emit(tx, doc_id, source_id, kind, "ready", count, None);
        }
        Err(e) => {
            let _ = db
                .kdoc_update_index_status(doc_id, kind, "failed", 0, Some(&e))
                .await;
            sync_source_status(db, source_id, kind, "failed").await;
            emit(tx, doc_id, source_id, kind, "failed", 0, Some(e));
        }
    }
}

/// 容器级 status 只由 pages 侧维护（Wiki 列表显示容器状态；向量侧的 KB 列表
/// 无此语义）。两侧都写会造成并发双写互相覆盖，故此处按 kind 分流。
async fn sync_source_status(db: &Database, source_id: &str, kind: IndexKind, status: &str) {
    if kind == IndexKind::Pages {
        let _ = db.ks_update_status(source_id, status).await;
    }
}

/// 启动文档摄入后台任务。
///
/// 流程：
/// 1. 按容器开关 + `only` 求要跑的索引集合，空则直接返回（不发事件）。
/// 2. 逐 kind 原子 CAS `pending→processing`；未抢到的剔除（文档不存在、或已被
///    并发 reindex 抢占）。全部落空则返回。
/// 3. 提取一次（双索引共享）。失败 → 所有抢到的 kind 置 failed。
/// 4. 双侧**并发**跑各自流水线（互不依赖），各自在分支内部取自己的 permit。
///
/// 关于 permit 位置：限流的目标是远端服务（embedding / LLM），提取是本地 CPU/IO，
/// 故 permit 在提取之后、各侧分支内部获取。代价是排队等 permit 期间文档已显示
/// `processing` 而非 `pending`（此前单索引实现里 permit 在最外层，排队时仍是
/// pending）。这是双索引共享提取的必要取舍：两侧 permit 数量不同，若在最外层
/// 取两个 permit，窄的 pages 侧会拖住宽的向量侧。
///
/// 整个任务体包在 `catch_unwind` 里：解析器/embedder 任一步 panic 都不能让任务
/// 静默死亡、文档永久卡 processing。
pub fn spawn_ingest(opts: IngestOpts) {
    tokio::spawn(async move {
        let IngestOpts {
            db,
            store,
            cipher,
            source,
            doc_id,
            source_path,
            file_type,
            tx,
            vector_sem,
            pages_sem,
            page_extractor,
            only,
        } = opts;

        let planned = planned_kinds(&source, only);
        if planned.is_empty() {
            return;
        }

        // 逐 kind CAS 抢占：pending 是索引行的初始态，也是 reindex 前 API 置回的
        // 状态。CAS 未命中说明索引行不存在（容器未开该索引 / 文档已删）或已被
        // 抢占/在途 —— 不误发事件、不参与后续。
        let mut kinds = Vec::with_capacity(planned.len());
        for k in planned {
            if db
                .kdoc_mark_processing_if_pending(&doc_id, k)
                .await
                .unwrap_or(false)
            {
                kinds.push(k);
            }
        }
        if kinds.is_empty() {
            return;
        }

        if kinds.contains(&IndexKind::Pages) {
            let _ = db.ks_update_status(&source.id, "processing").await;
        }
        for k in &kinds {
            emit(&tx, &doc_id, &source.id, *k, "processing", 0, None);
        }

        let result = std::panic::AssertUnwindSafe(async {
            // 提取一次（双索引共享）。
            let content = match read_and_extract(&source_path, file_type).await {
                Ok(c) => c,
                Err(e) => {
                    for k in &kinds {
                        finish_kind(&db, &tx, &source.id, &doc_id, *k, Err(e.clone())).await;
                    }
                    return;
                }
            };

            let run_vector = kinds.contains(&IndexKind::Vector);
            let run_pages = kinds.contains(&IndexKind::Pages);

            // 双分叉并发：两个索引互不依赖，串行只会让总时长变成两侧之和
            // （pages 侧 LLM 调用可达数十秒）。
            let vector_side = async {
                if !run_vector {
                    return;
                }
                let _permit = match &vector_sem {
                    Some(s) => s.clone().acquire_owned().await.ok(),
                    None => None,
                };
                let r =
                    ingest_vector(&db, &store, cipher.as_ref(), &source, &doc_id, &content).await;
                finish_kind(&db, &tx, &source.id, &doc_id, IndexKind::Vector, r).await;
            };

            let pages_side = async {
                if !run_pages {
                    return;
                }
                let _permit = match &pages_sem {
                    Some(s) => s.clone().acquire_owned().await.ok(),
                    None => None,
                };
                let r = match page_extractor.as_ref() {
                    Some(ex) => ingest_pages(&db, ex.as_ref(), &source.id, &doc_id, &content).await,
                    // 容器开了 pages 索引但调用方没注入抽取器：装配错误，明确
                    // 报失败（静默跳过会让文档永远停在 processing）。
                    None => Err("pages extractor unavailable".to_owned()),
                };
                finish_kind(&db, &tx, &source.id, &doc_id, IndexKind::Pages, r).await;
            };

            tokio::join!(vector_side, pages_side);
        })
        .catch_unwind()
        .await;

        // panic 兜底：与流水线返回 Err 同语义 —— 所有在途 kind 置 failed，
        // 文档不卡 processing、前端能感知失败。
        if let Err(payload) = result {
            let msg = panic_message(&*payload);
            tracing::error!(doc_id = %doc_id, source_id = %source.id, panic = %msg, "knowledge ingest task panicked");
            for k in &kinds {
                finish_kind(&db, &tx, &source.id, &doc_id, *k, Err(msg.clone())).await;
            }
        }
    });
}

/// 从 `catch_unwind` 的 panic payload 提取人类可读消息（`&str` 或 `String`）。
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_owned()
    }
}

/// 读原文 + 提取文本（双索引共享的唯一昂贵步骤）。
async fn read_and_extract(
    source_path: &std::path::Path,
    file_type: FileType,
) -> Result<String, String> {
    let bytes = tokio::fs::read(source_path)
        .await
        .map_err(|e| format!("read source file: {e}"))?;
    // CPU 密集的解析放阻塞池，避免卡住 tokio worker（PDF 解析大文件可达数百 ms）。
    let content = tokio::task::spawn_blocking(move || extractor::extract(&bytes, file_type))
        .await
        .map_err(|e| format!("extract task: {e}"))?
        .map_err(|e| e.to_string())?;
    if content.trim().is_empty() {
        return Err("no text extracted from document".to_string());
    }
    Ok(content)
}

/// 向量侧：分块 → embedding → 写 shard → 落库，返回分块数。
async fn ingest_vector(
    db: &Database,
    store: &VectorStore,
    cipher: Option<&LlmCipher>,
    kb: &KnowledgeSourceRecord,
    doc_id: &str,
    content: &str,
) -> Result<i64, String> {
    let chunks = chunker::chunk_markdown(
        content,
        usize::try_from(kb.chunk_size).unwrap_or(usize::MAX),
        usize::try_from(kb.chunk_overlap).unwrap_or(usize::MAX),
    );
    if chunks.is_empty() {
        return Err("empty content".to_string());
    }
    let api_key = decrypt_field(cipher, &kb.emb_api_key).map_err(|e| e.clone())?;
    let embedder = Embedder::new(&kb.emb_base_url, &api_key, &kb.emb_model);
    let texts: Vec<String> = chunks.iter().map(|c| c.content.clone()).collect();
    let vectors = embedder.embed(&texts).await.map_err(|e| e.to_string())?;
    if vectors.len() != texts.len() {
        return Err(format!(
            "embedding count mismatch: sent {}, got {}",
            texts.len(),
            vectors.len()
        ));
    }

    // 写向量 + 元数据
    let mut points = Vec::with_capacity(chunks.len());
    let mut rows = Vec::with_capacity(chunks.len());
    for (i, (c, v)) in chunks.iter().zip(vectors).enumerate() {
        let cid = uuid::Uuid::new_v4().to_string();
        points.push(ChunkPoint {
            id: cid.clone(),
            vector: v,
            doc_id: doc_id.to_string(),
            seq: i64::try_from(i).unwrap_or(i64::MAX),
            heading_path: c.heading_path.clone(),
        });
        rows.push((
            cid,
            doc_id.to_string(),
            kb.id.clone(),
            i64::try_from(i).unwrap_or(i64::MAX),
            c.heading_path.clone(),
            c.content.clone(),
            i64::try_from(c.token_count).unwrap_or(i64::MAX),
        ));
    }
    store
        .upsert(
            &kb.id,
            usize::try_from(kb.emb_dimension).unwrap_or(usize::MAX),
            points,
        )
        .await
        .map_err(|e| e.to_string())?;
    if let Err(e) = db.rag_insert_chunks(&rows).await {
        // 回滚本任务刚写入的向量：rag_insert_chunks 失败意味着元数据未落库
        // （FK 失败——doc 在摄入中途被删、或库被软关后的竞态），不清理则这些
        // 向量永久残留（chunk id 不在 knowledge_chunks 中，检索不可见，纯磁盘泄漏）。
        // best-effort：失败仅 warn，DB 仍是源，不影响错误上报。
        if let Err(se) = store
            .delete_by_doc(
                &kb.id,
                usize::try_from(kb.emb_dimension).unwrap_or(usize::MAX),
                doc_id,
            )
            .await
        {
            tracing::warn!(source_id = %kb.id, doc_id, error = %se, "knowledge ingest: vector rollback failed");
        }
        return Err(e.to_string());
    }
    Ok(i64::try_from(chunks.len()).unwrap_or(i64::MAX))
}

/// pages 侧：分块 → 批次抽取（经注入的 [`PageExtractor`]）→ 清旧页 → upsert，
/// 返回页数。
async fn ingest_pages(
    db: &Database,
    extractor: &dyn PageExtractor,
    source_id: &str,
    doc_id: &str,
    content: &str,
) -> Result<i64, String> {
    let chunks = chunker::chunk_markdown(content, PAGES_CHUNK_SIZE, PAGES_CHUNK_OVERLAP);
    if chunks.is_empty() {
        return Err("empty content".to_string());
    }

    let mut all_pages: Vec<ExtractedPage> = Vec::new();
    for batch in batch_chunks(&chunks) {
        let batch_text = batch
            .iter()
            .map(|c| c.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n---\n\n");
        let mut last_err = String::new();
        let mut ok = false;
        for attempt in 0..BATCH_ATTEMPTS {
            match extractor.extract_pages(&batch_text).await {
                Ok(pages) => {
                    all_pages.extend(pages);
                    ok = true;
                    break;
                }
                Err(e) => {
                    tracing::warn!(source_id, doc_id, error = %e, attempt, "pages extract failed, retrying");
                    last_err = e;
                }
            }
        }
        // 一个批次用尽重试仍失败 → 整篇失败。部分成功会让页面集合残缺却报
        // ready，用户无从知道少了什么。
        if !ok {
            return Err(last_err);
        }
    }

    // 先清本文档的旧页（reindex 语义：全量替换），再 upsert。空抽取（切片无可
    // 提炼内容）→ 清完直接返回 0，属正常完成而非失败。
    db.wiki_clear_pages_by_doc(source_id, doc_id)
        .await
        .map_err(|e| e.to_string())?;
    if all_pages.is_empty() {
        return Ok(0);
    }

    let mut upserted = 0i64;
    for page in &all_pages {
        let Some(norm_ref) = rust_tunnel_persistence::wiki::normalize_wiki_ref(&page.page_ref)
        else {
            tracing::warn!(source_id, doc_id, r = %page.page_ref, "pages ingest: skip invalid ref");
            continue;
        };
        // 同批次重复 ref 由 DAO 的 upsert 覆盖处理（后者胜）。
        let title = truncate_chars(&page.title, TITLE_MAX_CHARS);
        let summary = truncate_chars(&page.summary, SUMMARY_MAX_CHARS);
        let body = truncate_chars(&page.content, PAGE_CONTENT_MAX_CHARS);
        match db
            .wiki_upsert_page(
                source_id,
                &norm_ref,
                &title,
                &summary,
                &body,
                false,
                Some(doc_id),
            )
            .await
        {
            Ok(_) => upserted += 1,
            Err(e) => {
                tracing::warn!(source_id, doc_id, r = %norm_ref, error = %e, "pages ingest: upsert page failed");
            }
        }
    }

    Ok(upserted)
}

/// 把 chunk 序列打包成 LLM 批次：受 [`MAX_CHUNKS_PER_BATCH`] 与
/// [`MAX_TOKENS_PER_BATCH`] 双重约束。
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

/// 按**字符**截断（非字节）：避免切在 UTF-8 码点中间。
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use tokio::sync::broadcast;

    use crate::extractor::pdf::make_empty_page_pdf;
    use rust_tunnel_persistence::knowledge::{IndexKind, KnowledgeSourceRecord, KsCreateOpts};
    use rust_tunnel_persistence::Database;

    use super::*;

    /// 把文本写入临时源文件，返回 (TempDir, 路径)。TempDir 需活到任务结束。
    fn write_source(content: &str, ext: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("doc.{ext}"));
        std::fs::write(&path, content).unwrap();
        (dir, path)
    }

    /// 字节版 write_source：用于二进制 fixture（PDF/OOXML）。
    fn write_source_bytes(bytes: &[u8], ext: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("doc.{ext}"));
        std::fs::write(&path, bytes).unwrap();
        (dir, path)
    }

    /// TempDir 放前、store 放后：qadrant-edge 的 `EdgeShard` Drop 时同步 flush
    /// 并 `expect()`（目录已删会 panic），故 store 必须先于 TempDir 析构。
    fn tmp_store() -> (tempfile::TempDir, VectorStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = VectorStore::new(dir.path());
        (dir, store)
    }

    /// 建一个指向给定 embedding base 的容器（仅向量索引）并返回完整 record。
    async fn create_kb(
        db: &Database,
        id: &str,
        emb_base_url: &str,
        dim: i64,
    ) -> KnowledgeSourceRecord {
        create_source(db, id, emb_base_url, dim, true, false).await
    }

    /// 建容器，可选开启两侧索引。
    async fn create_source(
        db: &Database,
        id: &str,
        emb_base_url: &str,
        dim: i64,
        index_vector: bool,
        index_pages: bool,
    ) -> KnowledgeSourceRecord {
        db.ks_create(&KsCreateOpts {
            id: id.to_owned(),
            name: format!("测试库-{id}"),
            summary: "描述".to_owned(),
            index_vector,
            index_pages,
            scope_type: "global".to_owned(),
            client_id: String::new(),
            workspace_id: String::new(),
            emb_base_url: emb_base_url.to_owned(),
            emb_api_key: "sk-plain".to_owned(),
            emb_model: "test-model".to_owned(),
            emb_dimension: dim,
            top_k: 5,
            chunk_size: 512,
            chunk_overlap: 64,
            score_threshold: 0.3,
            enabled: true,
        })
        .await
        .unwrap();
        db.ks_get(id).await.unwrap().unwrap()
    }

    /// 组装一份最小 `IngestOpts`（向量侧，无限流、无 pages 抽取器）。
    fn vector_opts(
        db: &Database,
        store: &VectorStore,
        kb: &KnowledgeSourceRecord,
        doc_id: &str,
        src: std::path::PathBuf,
        file_type: FileType,
        tx: broadcast::Sender<KbEvent>,
    ) -> IngestOpts {
        IngestOpts {
            db: db.clone(),
            store: store.clone(),
            cipher: None,
            source: kb.clone(),
            doc_id: doc_id.to_owned(),
            source_path: src,
            file_type,
            tx,
            vector_sem: None,
            pages_sem: None,
            page_extractor: None,
            only: None,
        }
    }

    /// 固定返回预设页面的 mock 抽取器，记录调用次数。
    struct MockPages {
        pages: Vec<ExtractedPage>,
        calls: Arc<AtomicUsize>,
        /// 前 N 次调用返回 Err（测重试）。
        fail_first: usize,
    }

    #[async_trait::async_trait]
    impl PageExtractor for MockPages {
        async fn extract_pages(&self, _batch_text: &str) -> Result<Vec<ExtractedPage>, String> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n < self.fail_first {
                return Err("mock extractor transient failure".to_owned());
            }
            Ok(self.pages.clone())
        }
    }

    fn mock_pages(
        pages: Vec<ExtractedPage>,
        fail_first: usize,
    ) -> (Arc<dyn PageExtractor>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let ex = MockPages {
            pages,
            calls: calls.clone(),
            fail_first,
        };
        (Arc::new(ex), calls)
    }

    fn page(r: &str, title: &str, content: &str) -> ExtractedPage {
        ExtractedPage {
            page_ref: r.to_owned(),
            title: title.to_owned(),
            summary: "摘要".to_owned(),
            content: content.to_owned(),
        }
    }

    /// 起一个返回固定 embedding 的本地 HTTP server，返回 base_url。
    async fn mock_embedding_server(dim: usize) -> String {
        use axum::extract::Json;
        use axum::routing::post;
        use axum::Router;
        use serde_json::{json, Value};
        let app = Router::new().route(
            "/embeddings",
            post(move |body: Json<Value>| async move {
                let n = body["input"].as_array().map_or(1, std::vec::Vec::len);
                let data: Vec<_> = (0..n)
                    .map(|i| {
                        json!({
                            "index": i,
                            "embedding": vec![0.1f32; dim],
                            "object": "embedding"
                        })
                    })
                    .collect();
                Json(json!({"object": "list", "data": data}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    /// 起一个返回固定 embedding 的本地 HTTP server，但 data 比 input 少一条
    /// （模拟服务商部分结果，触发 count mismatch 校验）。
    async fn mock_embedding_server_short(dim: usize) -> String {
        use axum::extract::Json;
        use axum::routing::post;
        use axum::Router;
        use serde_json::{json, Value};
        let app = Router::new().route(
            "/embeddings",
            post(move |body: Json<Value>| async move {
                let n = body["input"].as_array().map_or(1, std::vec::Vec::len);
                let data: Vec<_> = (0..n.saturating_sub(1))
                    .map(|i| {
                        json!({
                            "index": i,
                            "embedding": vec![0.1f32; dim],
                            "object": "embedding"
                        })
                    })
                    .collect();
                Json(json!({"object": "list", "data": data}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    /// 等待下一条摄入事件（10s 超时，避免测试无限挂起）。
    async fn next_event(rx: &mut broadcast::Receiver<KbEvent>) -> KbEvent {
        tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("timeout waiting for ingest event")
            .expect("broadcast channel closed")
    }

    /// 收事件直到某 kind 到达终态，返回该 kind 的终态事件。**仅用于单索引
    /// 场景**：会丢弃其他 kind 的事件，双索引下用 [`wait_terminals`]。
    async fn wait_terminal(rx: &mut broadcast::Receiver<KbEvent>, kind: IndexKind) -> KbEvent {
        loop {
            let ev = next_event(rx).await;
            if ev.kind == kind && (ev.status == "ready" || ev.status == "failed") {
                return ev;
            }
        }
    }

    /// 收事件直到给定各 kind 都到终态，返回 kind → 终态事件。
    ///
    /// 双索引下两侧并发推进，终态事件到达顺序不确定；逐个 `wait_terminal`
    /// 会把先到的另一侧终态事件丢掉，之后永远等不到它（channel 关闭）。
    async fn wait_terminals(
        rx: &mut broadcast::Receiver<KbEvent>,
        kinds: &[IndexKind],
    ) -> std::collections::HashMap<IndexKind, KbEvent> {
        let mut out = std::collections::HashMap::new();
        while out.len() < kinds.len() {
            let ev = next_event(rx).await;
            if kinds.contains(&ev.kind) && (ev.status == "ready" || ev.status == "failed") {
                out.insert(ev.kind, ev);
            }
        }
        out
    }

    #[tokio::test]
    async fn ingest_produces_ready_doc_with_chunks() {
        let db = Database::new(":memory:").await.expect("in-memory db");
        let base = mock_embedding_server(8).await;
        let kb = create_kb(&db, "kb-1", &base, 8).await;
        let doc_id = "doc-1".to_string();
        db.kdoc_create(&doc_id, &kb.id, "guide.md", "md", "sha256:abc")
            .await
            .unwrap();
        let content =
            "# 使用指南\n\n## 安装\n\n运行 rust-tunnel-server。\n\n## 配置\n\n编辑 config.toml。\n"
                .to_string();

        let (_d, store) = tmp_store();
        let (tx, mut rx) = broadcast::channel(16);
        let (_sd, src) = write_source(&content, "md");
        spawn_ingest(vector_opts(
            &db,
            &store,
            &kb,
            &doc_id,
            src,
            FileType::Markdown,
            tx,
        ));

        // 事件序列：processing → ready，均带 kind=vector
        let s1 = next_event(&mut rx).await;
        assert_eq!(s1.status, "processing");
        assert_eq!(s1.doc_id, doc_id);
        assert_eq!(s1.kind, IndexKind::Vector);
        let s2 = next_event(&mut rx).await;
        assert_eq!(s2.status, "ready");
        assert_eq!(s2.kind, IndexKind::Vector);
        assert!(s2.error.is_none());

        // db 状态与分块落库
        let idx = db
            .kdoc_get_index(&doc_id, IndexKind::Vector)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(idx.status, "ready");
        assert!(idx.item_count > 0);
        assert!(idx.error.is_none());
        assert_eq!(s2.chunk_count, idx.item_count);
        assert_eq!(
            db.rag_count_kb_chunks(&kb.id).await.unwrap(),
            idx.item_count
        );

        // 向量已写入：同 kb search 能命中
        let query = [1.0f32; 8];
        let hits = store
            .search(
                &kb.id,
                usize::try_from(kb.emb_dimension).unwrap_or(usize::MAX),
                &query,
                5,
            )
            .await;
        assert_eq!(
            i64::try_from(hits.len()).unwrap_or(i64::MAX),
            idx.item_count
        );
        assert!(!hits.is_empty());
    }

    #[tokio::test]
    async fn ingest_failure_marks_doc_failed() {
        let db = Database::new(":memory:").await.expect("in-memory db");
        // emb_base_url 指向不可达地址（127.0.0.1:1 → connection refused）
        let kb = create_kb(&db, "kb-fail", "http://127.0.0.1:1", 8).await;
        let doc_id = "doc-fail".to_string();
        db.kdoc_create(&doc_id, &kb.id, "x.md", "md", "sha256:x")
            .await
            .unwrap();

        let (_d, store) = tmp_store();
        let (tx, mut rx) = broadcast::channel(16);
        let (_sd, src) = write_source("some content", "md");
        spawn_ingest(vector_opts(
            &db,
            &store,
            &kb,
            &doc_id,
            src,
            FileType::Markdown,
            tx,
        ));

        let s1 = next_event(&mut rx).await;
        assert_eq!(s1.status, "processing");
        let s2 = next_event(&mut rx).await;
        assert_eq!(s2.status, "failed");
        assert!(s2.error.as_deref().is_some_and(|e| !e.is_empty()));

        let idx = db
            .kdoc_get_index(&doc_id, IndexKind::Vector)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(idx.status, "failed");
        assert!(idx.error.as_deref().is_some_and(|e| !e.is_empty()));
    }

    #[tokio::test]
    async fn embed_count_mismatch_marks_doc_failed() {
        let db = Database::new(":memory:").await.expect("in-memory db");
        // mock 返回的向量比输入文本少一条 → 报 count mismatch，
        // 而非静默丢弃尾部 chunk 后仍报 ready。
        let base = mock_embedding_server_short(8).await;
        let kb = create_kb(&db, "kb-short", &base, 8).await;
        let doc_id = "doc-short".to_string();
        db.kdoc_create(&doc_id, &kb.id, "y.md", "md", "sha256:y")
            .await
            .unwrap();
        // 内容需产生至少 2 个 chunk，使 mock 的 n-1 返回值非空
        // （否则会先触发 embedder 的 EmptyResponse，而非本校验）。
        let content = "# A\n\n段落一。\n\n## B\n\n段落二。\n".to_string();

        let (_d, store) = tmp_store();
        let (tx, mut rx) = broadcast::channel(16);
        let (_sd, src) = write_source(&content, "md");
        spawn_ingest(vector_opts(
            &db,
            &store,
            &kb,
            &doc_id,
            src,
            FileType::Markdown,
            tx,
        ));

        let s1 = next_event(&mut rx).await;
        assert_eq!(s1.status, "processing");
        let s2 = next_event(&mut rx).await;
        assert_eq!(s2.status, "failed");
        let err = s2.error.as_deref().expect("failed event has error");
        assert!(
            err.contains("count mismatch"),
            "error should mention count mismatch: {err}"
        );

        let idx = db
            .kdoc_get_index(&doc_id, IndexKind::Vector)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(idx.status, "failed");
        assert!(idx
            .error
            .as_deref()
            .is_some_and(|e| e.contains("count mismatch")));
        // 尾部 chunk 未入索引：count 保持 0
        assert_eq!(idx.item_count, 0);
        assert_eq!(db.rag_count_kb_chunks(&kb.id).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn ingest_insert_failure_rolls_back_vectors() {
        let db = Database::new(":memory:").await.expect("in-memory db");
        let base = mock_embedding_server(8).await;
        let kb = create_kb(&db, "kb-rollback", &base, 8).await;
        let (_d, store) = tmp_store();

        // 不创建 doc 行：rag_insert_chunks 因 FK（doc_id 不存在）失败。
        // 这是「doc 在摄入中途被删 / insert 落库失败」的确定性模拟 ——
        // upsert 已写入向量，随后 insert 失败，必须回滚本次向量防孤儿残留。
        let content = "# 使用指南\n\n## 安装\n\n运行 rust-tunnel-server。\n".to_string();
        let res = ingest_vector(&db, &store, None, &kb, "ghost-doc", &content).await;
        assert!(res.is_err(), "FK 失败应使摄入失败: {res:?}");

        // 向量已回滚：同 kb search 应为空（不留孤儿向量），分块也未落库。
        let hits = store
            .search(
                &kb.id,
                usize::try_from(kb.emb_dimension).unwrap_or(usize::MAX),
                &[1.0f32; 8],
                10,
            )
            .await;
        assert!(hits.is_empty(), "insert 失败后应回滚本次写入的向量");
        assert_eq!(db.rag_count_kb_chunks(&kb.id).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn ingest_extract_failure_marks_doc_failed() {
        let db = Database::new(":memory:").await.expect("in-memory db");
        let base = mock_embedding_server(8).await;
        let kb = create_kb(&db, "kb-scan", &base, 8).await;
        let doc_id = "doc-scan".to_string();
        db.kdoc_create(&doc_id, &kb.id, "scan.pdf", "pdf", "sha256:x")
            .await
            .unwrap();
        // 无文本层 PDF（复用 extractor::pdf 测试用空页 PDF 构造）。
        let (_sd, src) = write_source_bytes(&make_empty_page_pdf(), "pdf");
        let (_d, store) = tmp_store();
        let (tx, mut rx) = broadcast::channel(16);
        spawn_ingest(vector_opts(
            &db,
            &store,
            &kb,
            &doc_id,
            src,
            FileType::Pdf,
            tx,
        ));

        let s1 = next_event(&mut rx).await;
        assert_eq!(s1.status, "processing");
        let s2 = next_event(&mut rx).await;
        assert_eq!(s2.status, "failed");
        assert!(
            s2.error
                .as_deref()
                .is_some_and(|e| e.contains("no text layer")),
            "error should mention no text layer: {:?}",
            s2.error
        );

        let idx = db
            .kdoc_get_index(&doc_id, IndexKind::Vector)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(idx.status, "failed");
        assert!(idx
            .error
            .as_deref()
            .is_some_and(|e| e.contains("no text layer")));
        assert_eq!(db.rag_count_kb_chunks(&kb.id).await.unwrap(), 0);
    }

    #[test]
    fn panic_message_extracts_readable_payload() {
        let str_payload: Box<dyn std::any::Any + Send> = Box::new("boom");
        assert_eq!(panic_message(&*str_payload), "boom");
        let string_payload: Box<dyn std::any::Any + Send> = Box::new("boom".to_string());
        assert_eq!(panic_message(&*string_payload), "boom");
        let other: Box<dyn std::any::Any + Send> = Box::new(42i32);
        assert_eq!(panic_message(&*other), "unknown panic");
    }

    #[test]
    fn batch_chunks_respects_limits() {
        let chunks = vec![
            chunker::Chunk {
                heading_path: String::new(),
                content: "a".repeat(4000),
                token_count: 1000,
            },
            chunker::Chunk {
                heading_path: String::new(),
                content: "b".repeat(4000),
                token_count: 1000,
            },
            chunker::Chunk {
                heading_path: String::new(),
                content: "c".repeat(4000),
                token_count: 1000,
            },
            chunker::Chunk {
                heading_path: String::new(),
                content: "d".repeat(4000),
                token_count: 1000,
            },
        ];
        let batches = batch_chunks(&chunks);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), 3);
        assert_eq!(batches[1].len(), 1);
    }

    #[test]
    fn truncate_chars_cuts_on_char_boundary() {
        // 中文字符按码点截断，不切坏 UTF-8（按字节截断会 panic 或产生乱码）
        assert_eq!(truncate_chars("一二三四五", 3), "一二三");
        assert_eq!(truncate_chars("abc", 10), "abc");
    }

    #[tokio::test]
    async fn planned_kinds_honors_switches_and_only() {
        // 用真容器记录而非手工字面量：字段一多，字面量在 schema 变更时是最先
        // 腐烂的东西，而 planned_kinds 只关心两个开关。
        let db = Database::new(":memory:").await.expect("in-memory db");
        let mut src = create_source(&db, "src-plan", "", 0, true, true).await;
        assert_eq!(
            planned_kinds(&src, None),
            vec![IndexKind::Vector, IndexKind::Pages]
        );
        assert_eq!(
            planned_kinds(&src, Some(IndexKind::Pages)),
            vec![IndexKind::Pages]
        );

        src.index_pages = 0;
        assert_eq!(planned_kinds(&src, None), vec![IndexKind::Vector]);
        // only 指向未启用的索引 → 空集（不能越过容器开关跑索引）
        assert!(planned_kinds(&src, Some(IndexKind::Pages)).is_empty());

        src.index_vector = 0;
        assert!(planned_kinds(&src, None).is_empty());
    }

    // ── 双索引分叉 ─────────────────────────────────────────────────

    #[tokio::test]
    async fn dual_index_runs_both_sides_from_one_extraction() {
        let db = Database::new(":memory:").await.expect("in-memory db");
        let base = mock_embedding_server(8).await;
        let src_rec = create_source(&db, "src-dual", &base, 8, true, true).await;
        let doc_id = "doc-dual".to_string();
        db.kdoc_create(&doc_id, &src_rec.id, "ops.md", "md", "sha256:d")
            .await
            .unwrap();

        let (ex, calls) = mock_pages(
            vec![
                page("deploy/checklist", "部署清单", "先看 [[ops/backup]]。"),
                page("ops/backup", "备份", "备份到对象存储。"),
            ],
            0,
        );
        let (_d, store) = tmp_store();
        let (tx, mut rx) = broadcast::channel(32);
        let (_sd, path) = write_source("# 运维手册\n\n## 部署\n\n先备份。\n", "md");
        spawn_ingest(IngestOpts {
            db: db.clone(),
            store: store.clone(),
            cipher: None,
            source: src_rec.clone(),
            doc_id: doc_id.clone(),
            source_path: path,
            file_type: FileType::Markdown,
            tx,
            vector_sem: None,
            pages_sem: None,
            page_extractor: Some(ex),
            only: None,
        });

        // 两侧各自到达 ready
        let term = wait_terminals(&mut rx, &[IndexKind::Vector, IndexKind::Pages]).await;
        let v = &term[&IndexKind::Vector];
        let p = &term[&IndexKind::Pages];
        assert_eq!(v.status, "ready", "vector 侧应 ready: {:?}", v.error);
        assert!(v.chunk_count > 0);
        assert_eq!(p.status, "ready", "pages 侧应 ready: {:?}", p.error);
        assert_eq!(p.chunk_count, 2);

        // per-kind 状态各自落库
        let vi = db
            .kdoc_get_index(&doc_id, IndexKind::Vector)
            .await
            .unwrap()
            .unwrap();
        let pi = db
            .kdoc_get_index(&doc_id, IndexKind::Pages)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(vi.status, "ready");
        assert_eq!(pi.status, "ready");
        assert_eq!(pi.item_count, 2);

        // 页面与图谱边落库；容器 page_count 由 DAO 维护
        let pg = db
            .wiki_get_page(&src_rec.id, "deploy/checklist")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pg.source_doc_id.as_deref(), Some(doc_id.as_str()));
        let graph = db.wiki_graph(&src_rec.id).await.unwrap();
        assert_eq!(graph.nodes.len(), 2);
        assert_eq!(graph.edges.len(), 1, "[[ref]] 应建边");
        let container = db.ks_get(&src_rec.id).await.unwrap().unwrap();
        assert_eq!(container.page_count, 2);

        // 提取只做一次：抽取器按批次调用，本文档只有 1 批
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn one_side_failure_does_not_affect_the_other() {
        let db = Database::new(":memory:").await.expect("in-memory db");
        // 向量侧指向不可达 embedding → 必失败；pages 侧 mock 正常 → 必成功。
        let src_rec = create_source(&db, "src-split", "http://127.0.0.1:1", 8, true, true).await;
        let doc_id = "doc-split".to_string();
        db.kdoc_create(&doc_id, &src_rec.id, "ops.md", "md", "sha256:s")
            .await
            .unwrap();

        let (ex, _calls) = mock_pages(vec![page("ops/note", "笔记", "正文。")], 0);
        let (_d, store) = tmp_store();
        let (tx, mut rx) = broadcast::channel(32);
        let (_sd, path) = write_source("# 手册\n\n内容。\n", "md");
        spawn_ingest(IngestOpts {
            db: db.clone(),
            store: store.clone(),
            cipher: None,
            source: src_rec.clone(),
            doc_id: doc_id.clone(),
            source_path: path,
            file_type: FileType::Markdown,
            tx,
            vector_sem: None,
            pages_sem: None,
            page_extractor: Some(ex),
            only: None,
        });

        let term = wait_terminals(&mut rx, &[IndexKind::Vector, IndexKind::Pages]).await;
        let v = &term[&IndexKind::Vector];
        let p = &term[&IndexKind::Pages];
        assert_eq!(v.status, "failed");
        assert_eq!(p.status, "ready", "一侧失败不应波及另一侧: {:?}", p.error);
        assert_eq!(p.chunk_count, 1);
    }

    #[tokio::test]
    async fn only_restricts_to_single_index() {
        let db = Database::new(":memory:").await.expect("in-memory db");
        let base = mock_embedding_server(8).await;
        let src_rec = create_source(&db, "src-only", &base, 8, true, true).await;
        let doc_id = "doc-only".to_string();
        db.kdoc_create(&doc_id, &src_rec.id, "ops.md", "md", "sha256:o")
            .await
            .unwrap();

        let (ex, calls) = mock_pages(vec![page("a/b", "t", "c")], 0);
        let (_d, store) = tmp_store();
        let (tx, mut rx) = broadcast::channel(32);
        let (_sd, path) = write_source("# 手册\n\n内容。\n", "md");
        spawn_ingest(IngestOpts {
            db: db.clone(),
            store: store.clone(),
            cipher: None,
            source: src_rec.clone(),
            doc_id: doc_id.clone(),
            source_path: path,
            file_type: FileType::Markdown,
            tx,
            vector_sem: None,
            pages_sem: None,
            page_extractor: Some(ex),
            only: Some(IndexKind::Vector),
        });

        let v = wait_terminal(&mut rx, IndexKind::Vector).await;
        assert_eq!(v.status, "ready");
        // pages 侧未被触碰：抽取器零调用，索引行仍是初始 pending
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let pi = db
            .kdoc_get_index(&doc_id, IndexKind::Pages)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pi.status, "pending", "only=vector 不应推进 pages 索引");
    }

    #[tokio::test]
    async fn pages_without_extractor_marks_failed() {
        let db = Database::new(":memory:").await.expect("in-memory db");
        let src_rec = create_source(&db, "src-noex", "", 0, false, true).await;
        let doc_id = "doc-noex".to_string();
        db.kdoc_create(&doc_id, &src_rec.id, "ops.md", "md", "sha256:n")
            .await
            .unwrap();

        let (_d, store) = tmp_store();
        let (tx, mut rx) = broadcast::channel(16);
        let (_sd, path) = write_source("# 手册\n\n内容。\n", "md");
        spawn_ingest(IngestOpts {
            db: db.clone(),
            store: store.clone(),
            cipher: None,
            source: src_rec.clone(),
            doc_id: doc_id.clone(),
            source_path: path,
            file_type: FileType::Markdown,
            tx,
            vector_sem: None,
            pages_sem: None,
            page_extractor: None,
            only: None,
        });

        let p = wait_terminal(&mut rx, IndexKind::Pages).await;
        assert_eq!(p.status, "failed", "缺抽取器应明确失败，不能卡 processing");
        assert!(p
            .error
            .as_deref()
            .is_some_and(|e| e.contains("extractor unavailable")));
        let pi = db
            .kdoc_get_index(&doc_id, IndexKind::Pages)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pi.status, "failed");
    }

    #[tokio::test]
    async fn pages_batch_retries_once_then_succeeds() {
        let db = Database::new(":memory:").await.expect("in-memory db");
        let src_rec = create_source(&db, "src-retry", "", 0, false, true).await;
        let doc_id = "doc-retry".to_string();
        db.kdoc_create(&doc_id, &src_rec.id, "ops.md", "md", "sha256:r")
            .await
            .unwrap();

        // 首次调用失败、第二次成功 → 整体 ready
        let (ex, calls) = mock_pages(vec![page("a/b", "t", "c")], 1);
        let (_d, store) = tmp_store();
        let (tx, mut rx) = broadcast::channel(16);
        let (_sd, path) = write_source("# 手册\n\n内容。\n", "md");
        spawn_ingest(IngestOpts {
            db: db.clone(),
            store: store.clone(),
            cipher: None,
            source: src_rec.clone(),
            doc_id: doc_id.clone(),
            source_path: path,
            file_type: FileType::Markdown,
            tx,
            vector_sem: None,
            pages_sem: None,
            page_extractor: Some(ex),
            only: None,
        });

        let p = wait_terminal(&mut rx, IndexKind::Pages).await;
        assert_eq!(p.status, "ready", "重试后应成功: {:?}", p.error);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "应重试 1 次共 2 次调用");
    }

    #[tokio::test]
    async fn pages_batch_exhausts_retries_and_fails() {
        let db = Database::new(":memory:").await.expect("in-memory db");
        let src_rec = create_source(&db, "src-exh", "", 0, false, true).await;
        let doc_id = "doc-exh".to_string();
        db.kdoc_create(&doc_id, &src_rec.id, "ops.md", "md", "sha256:e")
            .await
            .unwrap();

        // 永远失败 → 用尽重试后整篇 failed（不报部分成功的 ready）
        let (ex, calls) = mock_pages(vec![page("a/b", "t", "c")], usize::MAX);
        let (_d, store) = tmp_store();
        let (tx, mut rx) = broadcast::channel(16);
        let (_sd, path) = write_source("# 手册\n\n内容。\n", "md");
        spawn_ingest(IngestOpts {
            db: db.clone(),
            store: store.clone(),
            cipher: None,
            source: src_rec.clone(),
            doc_id: doc_id.clone(),
            source_path: path,
            file_type: FileType::Markdown,
            tx,
            vector_sem: None,
            pages_sem: None,
            page_extractor: Some(ex),
            only: None,
        });

        let p = wait_terminal(&mut rx, IndexKind::Pages).await;
        assert_eq!(p.status, "failed");
        assert_eq!(calls.load(Ordering::SeqCst), BATCH_ATTEMPTS);
        let container = db.ks_get(&src_rec.id).await.unwrap().unwrap();
        assert_eq!(container.status, "failed", "pages 侧应同步容器状态");
    }

    #[tokio::test]
    async fn pages_empty_extraction_is_ready_with_zero() {
        let db = Database::new(":memory:").await.expect("in-memory db");
        let src_rec = create_source(&db, "src-empty", "", 0, false, true).await;
        let doc_id = "doc-empty".to_string();
        db.kdoc_create(&doc_id, &src_rec.id, "ops.md", "md", "sha256:z")
            .await
            .unwrap();

        // 抽取器返回空数组：切片无可提炼内容属正常完成，不是失败
        let (ex, _calls) = mock_pages(vec![], 0);
        let (_d, store) = tmp_store();
        let (tx, mut rx) = broadcast::channel(16);
        let (_sd, path) = write_source("# 空\n\n无内容。\n", "md");
        spawn_ingest(IngestOpts {
            db: db.clone(),
            store: store.clone(),
            cipher: None,
            source: src_rec.clone(),
            doc_id: doc_id.clone(),
            source_path: path,
            file_type: FileType::Markdown,
            tx,
            vector_sem: None,
            pages_sem: None,
            page_extractor: Some(ex),
            only: None,
        });

        let p = wait_terminal(&mut rx, IndexKind::Pages).await;
        assert_eq!(p.status, "ready");
        assert_eq!(p.chunk_count, 0);
        let container = db.ks_get(&src_rec.id).await.unwrap().unwrap();
        assert_eq!(container.page_count, 0);
        assert_eq!(container.status, "ready");
    }

    #[tokio::test]
    async fn pages_reingest_replaces_old_pages_and_skips_locked() {
        let db = Database::new(":memory:").await.expect("in-memory db");
        let src_rec = create_source(&db, "src-re", "", 0, false, true).await;
        let doc_id = "doc-re".to_string();
        db.kdoc_create(&doc_id, &src_rec.id, "ops.md", "md", "sha256:re")
            .await
            .unwrap();
        // 预置同 ref 的 locked 手动页：ingest 不得覆盖
        db.wiki_upsert_page(
            &src_rec.id,
            "deploy/checklist",
            "手动页",
            "手动摘要",
            "手动维护的内容",
            true,
            None,
        )
        .await
        .unwrap();

        let (ex, _calls) = mock_pages(
            vec![
                page("deploy/checklist", "自动页", "自动内容"),
                page("ops/backup", "备份", "正文"),
            ],
            0,
        );
        let (_d, store) = tmp_store();
        let (tx, mut rx) = broadcast::channel(16);
        let (_sd, path) = write_source("# 手册\n\n内容。\n", "md");
        spawn_ingest(IngestOpts {
            db: db.clone(),
            store: store.clone(),
            cipher: None,
            source: src_rec.clone(),
            doc_id: doc_id.clone(),
            source_path: path,
            file_type: FileType::Markdown,
            tx,
            vector_sem: None,
            pages_sem: None,
            page_extractor: Some(ex),
            only: None,
        });

        let p = wait_terminal(&mut rx, IndexKind::Pages).await;
        assert_eq!(p.status, "ready", "{:?}", p.error);

        let pg = db
            .wiki_get_page(&src_rec.id, "deploy/checklist")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pg.content, "手动维护的内容", "locked 页不被 ingest 覆盖");
        assert_eq!(pg.locked, 1);
    }

    #[tokio::test]
    async fn extract_failure_fails_all_planned_kinds() {
        let db = Database::new(":memory:").await.expect("in-memory db");
        let base = mock_embedding_server(8).await;
        let src_rec = create_source(&db, "src-ext", &base, 8, true, true).await;
        let doc_id = "doc-ext".to_string();
        db.kdoc_create(&doc_id, &src_rec.id, "scan.pdf", "pdf", "sha256:p")
            .await
            .unwrap();

        // 无文本层 PDF：提取阶段失败 → 两个索引都应置 failed（共享步骤失败）
        let (ex, calls) = mock_pages(vec![page("a/b", "t", "c")], 0);
        let (_sd, path) = write_source_bytes(&make_empty_page_pdf(), "pdf");
        let (_d, store) = tmp_store();
        let (tx, mut rx) = broadcast::channel(16);
        spawn_ingest(IngestOpts {
            db: db.clone(),
            store: store.clone(),
            cipher: None,
            source: src_rec.clone(),
            doc_id: doc_id.clone(),
            source_path: path,
            file_type: FileType::Pdf,
            tx,
            vector_sem: None,
            pages_sem: None,
            page_extractor: Some(ex),
            only: None,
        });

        let term = wait_terminals(&mut rx, &[IndexKind::Vector, IndexKind::Pages]).await;
        let v = &term[&IndexKind::Vector];
        let p = &term[&IndexKind::Pages];
        assert_eq!(v.status, "failed");
        assert_eq!(p.status, "failed");
        assert!(v
            .error
            .as_deref()
            .is_some_and(|e| e.contains("no text layer")));
        assert!(p
            .error
            .as_deref()
            .is_some_and(|e| e.contains("no text layer")));
        // 提取失败发生在分叉之前，抽取器不该被调用
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn cas_prevents_double_ingest_of_same_index() {
        let db = Database::new(":memory:").await.expect("in-memory db");
        let base = mock_embedding_server(8).await;
        let kb = create_kb(&db, "kb-cas", &base, 8).await;
        let doc_id = "doc-cas".to_string();
        db.kdoc_create(&doc_id, &kb.id, "g.md", "md", "sha256:c")
            .await
            .unwrap();
        // 手工置 processing：模拟已有在途摄入
        db.kdoc_update_index_status(&doc_id, IndexKind::Vector, "processing", 0, None)
            .await
            .unwrap();

        let (_d, store) = tmp_store();
        let (tx, mut rx) = broadcast::channel(16);
        let (_sd, src) = write_source("# A\n\n内容。\n", "md");
        spawn_ingest(vector_opts(
            &db,
            &store,
            &kb,
            &doc_id,
            src,
            FileType::Markdown,
            tx,
        ));

        // CAS 未命中 → 任务静默退出，不发任何事件、不改状态。任务结束后 tx
        // 被 drop，recv 返回 Closed —— 超时与 Closed 都表示"没有事件"，
        // 收到任何事件才是失败。
        let got = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await;
        assert!(
            matches!(got, Err(_) | Ok(Err(broadcast::error::RecvError::Closed))),
            "CAS 未抢到时不应发事件: {got:?}"
        );
        let idx = db
            .kdoc_get_index(&doc_id, IndexKind::Vector)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(idx.status, "processing", "状态应保持在途，未被抢占");
        assert_eq!(db.rag_count_kb_chunks(&kb.id).await.unwrap(), 0);
    }
}
