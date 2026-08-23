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
    pub fn new(db: Database, llm: LlmState) -> Self {
        // 容量 64：与 MemoryState / LlmState.rag_tx 一致，低频事件不阻塞调用方。
        let (events, _rx) = tokio::sync::broadcast::channel(64);
        Self { db, llm, events, ingest_sem: Arc::new(Semaphore::new(2)) }
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<WikiEvent> {
        self.events.subscribe()
    }
}

pub use crate::persistence::db::wiki::{normalize_wiki_ref, parse_wiki_links};
