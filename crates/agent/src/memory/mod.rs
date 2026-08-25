//! AI 记忆体运行时：`MemoryState` 挂 `AgentState`，供蒸馏（distill）、注入（inject）、
//! remember 工具与后续管理 API 共享。仅 `rag` feature 编译（在 `agent/mod.rs` 挂载
//! 处用 `#[cfg(feature = "rag")]` 门控）。
//!
//! 向量约定：所有记忆共用单一全局 shard（`MEMORY_KB_ID = "memory"`），`ChunkPoint`
//! 的 id 与 doc_id 均取记忆 id，删除走 `delete_by_doc("memory", dim, memory_id)`。

pub mod distill;
pub mod inject;
pub mod remember;

use tokio::sync::broadcast;

use crate::db::memory::AgentMemorySettingsRecord;
use crate::db::Database;
use crate::llm::crypto::LlmCipher;
use crate::llm::rag::embedder::Embedder;
use crate::llm::rag::store::{ChunkPoint, ScoredPoint, VectorStore};
use crate::llm::LlmState;

/// 记忆向量的全局 kb_id 常量（单一 shard，落 `<data_dir>/rag/memory/`）。
/// ChunkPoint 的 id/doc_id 对齐 `agent_memories.id`。
pub const MEMORY_KB_ID: &str = "memory";

// ── 去重/容量常量（distill 与 remember 共用）────────────────────

/// 同作用域去重阈值（余弦相似度）。命中既有记忆且 score ≥ 阈值 → 更新；
/// 否则新建。蒸馏与 remember 工具统一。
pub const DEDUP_THRESHOLD: f32 = 0.90;
/// 去重检索的候选数（同作用域过滤前 over-fetch 的量）。
pub const DEDUP_TOP_K: usize = 3;
/// 单条记忆 content 上限（与 remember schema 的 ≤2048 一致）。
pub const MEMORY_CONTENT_MAX_CHARS: usize = 2048;
/// tags 数量上限。
pub const MAX_TAGS: usize = 8;
/// 单 tag 长度上限。
pub const TAG_MAX_CHARS: usize = 32;

// ── remember 工具 schema（runner 内置短路与 MCP tools/list 共用）────

/// remember 工具给模型的描述（OpenAI function schema 与 MCP 端点共用）。
pub const REMEMBER_TOOL_DESCRIPTION: &str = "Save a durable atomic fact about the machine environment, user preferences, or key project decisions for reuse in future sessions. Only save stable reusable knowledge — never credentials, API keys, or transient state.";

/// remember 工具的 parameters JSON schema。`tools.rs` 的 OpenAI function schema 直接
/// 内嵌此对象；MCP `tools/list` 的 `inputSchema` 同样取它（两者同构）。
#[must_use] 
pub fn remember_tool_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "content": {"type": "string", "description": "The atomic fact to remember (max 2048 chars)"},
            "scope": {"type": "string", "enum": ["workspace", "client", "global"], "description": "Visibility: workspace = this project only; client = all projects on this machine; global = everywhere (default workspace)"},
            "tags": {"type": "array", "items": {"type": "string"}, "description": "Optional tags (max 8, each max 32 chars)"}
        },
        "required": ["content"]
    })
}

/// 蒸馏/注入过程广播给前端 SSE 的事件。
#[derive(Debug, Clone, serde::Serialize)]
pub struct MemoryEvent {
    pub session_id: String,
    /// 判别字段：`distilled` / `failed` / `skipped` 等。
    pub status: String,
    pub facts_found: usize,
    /// 蒸馏产出的 Skill 条数（二期新增）。`#[serde(default)]`：旧前端/旧事件
    /// 缺字段时反序列化为 0（向后兼容）。
    #[serde(default)]
    pub skills_found: usize,
}

/// AI 记忆体运行时。`store` 与 `llm.rag_store` 是**同一** VectorStore 实例
/// （`server.rs` 构造时克隆 `LlmState.rag_store`，严禁 `VectorStore::new` 再造——
/// 双 EdgeShard 对同一目录各自 flush 会竞态 panic）。
#[derive(Clone)]
pub struct MemoryState {
    pub db: Database,
    pub store: VectorStore,
    pub cipher: Option<LlmCipher>,
    pub llm: LlmState,
    /// 蒸馏/注入事件广播（订阅者即后续的 SSE 端点）。
    pub events: broadcast::Sender<MemoryEvent>,
}

impl std::fmt::Debug for MemoryState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // LlmState 未实现 Debug；只暴露足以定位的字段。
        f.debug_struct("MemoryState")
            .field("store", &self.store)
            .field("cipher", &self.cipher.is_some())
            .finish_non_exhaustive()
    }
}

impl MemoryState {
    /// 构造运行时。`store` 必须是 `llm.rag_store` 的克隆（同一底层 shard 缓存）。
    #[must_use] 
    pub fn new(db: Database, store: VectorStore, cipher: Option<LlmCipher>, llm: LlmState) -> Self {
        // 容量 64：事件低频（会话结束蒸馏一次），广播满时仅丢旧事件，不阻塞调用方。
        let (events, _rx) = broadcast::channel(64);
        Self {
            db,
            store,
            cipher,
            llm,
            events,
        }
    }

    /// 读全局设置；DB 读取失败时返回默认（enabled=false）并告警——记忆功能视作
    /// 未配置，不影响主链路。
    pub async fn settings(&self) -> crate::db::memory::AgentMemorySettingsRecord {
        match self.db.memory_get_settings().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "memory settings read failed, treating as disabled");
                crate::db::memory::AgentMemorySettingsRecord::default_disabled()
            }
        }
    }

    /// 从设置构造 Embedder；总闸未开（enabled=0）或 emb 配置缺失 / API key 解密
    /// 为空时返回 None（调用方据此跳过向量化，蒸馏/注入/remember 均可安全短路）。
    pub async fn embedder(&self) -> Option<Embedder> {
        let s = self.settings().await;
        if s.enabled == 0 {
            return None;
        }
        if s.emb_base_url.is_empty() || s.emb_model.is_empty() {
            return None;
        }
        let key = crate::llm::crypto::decrypt_field(self.cipher.as_ref(), &s.emb_api_key)
            .unwrap_or_default();
        if key.is_empty() {
            return None;
        }
        Some(Embedder::new(&s.emb_base_url, &key, &s.emb_model))
    }

    /// 订阅记忆事件广播（SSE 端点用）。
    #[must_use] 
    pub fn subscribe(&self) -> broadcast::Receiver<MemoryEvent> {
        self.events.subscribe()
    }
}

// ── 共享工具（distill / inject / remember 共用）─────────────────

/// 作用域 → (scope_type, client_id, workspace_id) 三元组归一化。
/// - `global` → 全部空
/// - `client` → 客户端 id + 空 workspace
/// - `workspace`（默认）→ 客户端 id + 工作区 id
#[must_use] 
pub fn scope_coords(scope: &str, client_id: &str, workspace_id: &str) -> (String, String, String) {
    match scope {
        "global" => ("global".to_string(), String::new(), String::new()),
        "client" => ("client".to_string(), client_id.to_string(), String::new()),
        _ => (
            "workspace".to_string(),
            client_id.to_string(),
            workspace_id.to_string(),
        ),
    }
}

/// 检索注入的作用域判定：记忆行（scope_type/client_id/workspace_id）对当前
/// 会话（target_client/target_workspace）是否可见。global 恒可见；client 需
/// 客户端匹配；workspace 需客户端 + 工作区都匹配。
#[must_use] 
pub fn scope_ok(
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

/// 解析 tags JSON 数组字符串（`'["a","b"]'`）；坏 JSON 返回空。
#[must_use] 
pub fn parse_tags(raw: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(raw).unwrap_or_default()
}

/// tags 并集去重：旧 + 新，保留顺序，总量 ≤ `MAX_TAGS`（每项已由调用方保证 ≤
/// `TAG_MAX_CHARS`）。
#[must_use] 
pub fn merge_tags(existing: &[String], new: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in existing.iter().chain(new.iter()) {
        if out.len() >= MAX_TAGS {
            break;
        }
        if !out.contains(t) {
            out.push(t.clone());
        }
    }
    out
}

/// 在检索命中里找与目标作用域匹配、分数最高的既有记忆行。
async fn best_scope_match(
    memory: &MemoryState,
    hits: &[ScoredPoint],
    scope_type: &str,
    client_id: &str,
    workspace_id: &str,
) -> Option<(f32, String, crate::db::memory::AgentMemoryRecord)> {
    if hits.is_empty() {
        return None;
    }
    let ids: Vec<String> = hits.iter().map(|h| h.id.clone()).collect();
    let rows = memory.db.memory_get_by_ids(&ids).await.unwrap_or_default();
    hits.iter()
        .filter_map(|h| {
            let row = rows.iter().find(|r| r.id == h.id)?;
            if row.scope_type == scope_type
                && row.client_id == client_id
                && row.workspace_id == workspace_id
            {
                Some((h.score, row.clone()))
            } else {
                None
            }
        })
        .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(score, row)| (score, row.id.clone(), row))
}

/// 向量化 + 同作用域去重 upsert（distill 与 remember 共用）：
///
/// 1. `content` 向量化；
/// 2. 全 shard 检索 top-k 命中，过滤出同作用域（scope_type+client_id+workspace_id
///    全匹配）者；
/// 3. 最高分 ≥ `DEDUP_THRESHOLD` → 更新既有记忆（tags 并集、confidence 取 max、
///    content 以新为准，向量点同步覆盖）；
/// 4. 否则新建（id=uuid，`ChunkPoint` 的 id 与 doc_id 均取记忆 id）。
///
/// 返回记忆 id。embedding 失败 / 维度未配置 / DB 或向量写失败均返回 Err，由调用
/// 方决定降级语义（distill 静默跳过单条，remember 把错误喂回模型）。
#[allow(clippy::too_many_arguments)]
pub async fn upsert_memory_with_dedup(
    memory: &MemoryState,
    s: &AgentMemorySettingsRecord,
    emb: &Embedder,
    content: &str,
    scope_type: &str,
    client_id: &str,
    workspace_id: &str,
    tags: &[String],
    confidence: f64,
    source_session_id: &str,
    source_trigger: &str,
) -> Result<String, String> {
    let dim = s.emb_dimension;
    if dim <= 0 {
        return Err("memory embedding dimension not configured".into());
    }
    let vec = emb
        .embed_one(content)
        .await
        .map_err(|e| format!("embedding failed: {e}"))?;
    let hits = memory
        .store
        .search(MEMORY_KB_ID, dim as usize, &vec, DEDUP_TOP_K)
        .await;
    if let Some((score, id, existing)) =
        best_scope_match(memory, &hits, scope_type, client_id, workspace_id).await
    {
        if score >= DEDUP_THRESHOLD {
            let merged = merge_tags(&parse_tags(&existing.tags), tags);
            let tags_json = serde_json::to_string(&merged).unwrap_or_else(|_| "[]".into());
            let merged_conf = existing.confidence.max(confidence);
            memory
                .db
                .memory_update(&id, content, &tags_json, scope_type, merged_conf)
                .await
                .map_err(|e| format!("memory update failed: {e}"))?;
            // content 变了 → 向量点同步覆盖（id/doc_id 对齐记忆 id）
            memory
                .store
                .upsert(
                    MEMORY_KB_ID,
                    dim as usize,
                    vec![ChunkPoint {
                        id: id.clone(),
                        vector: vec,
                        doc_id: id.clone(),
                        seq: 0,
                        heading_path: String::new(),
                    }],
                )
                .await
                .map_err(|e| format!("vector upsert failed: {e}"))?;
            return Ok(id);
        }
    }
    // 新建
    let id = format!("{:032x}", rand::random::<u128>());
    let tags_json = serde_json::to_string(tags).unwrap_or_else(|_| "[]".into());
    memory
        .db
        .memory_insert(
            &id,
            content,
            scope_type,
            client_id,
            workspace_id,
            &tags_json,
            confidence,
            source_session_id,
            source_trigger,
            false,
        )
        .await
        .map_err(|e| format!("memory insert failed: {e}"))?;
    memory
        .store
        .upsert(
            MEMORY_KB_ID,
            dim as usize,
            vec![ChunkPoint {
                id: id.clone(),
                vector: vec,
                doc_id: id.clone(),
                seq: 0,
                heading_path: String::new(),
            }],
        )
        .await
        .map_err(|e| format!("vector upsert failed: {e}"))?;
    Ok(id)
}

/// 测试用 VectorStore（临时目录）。返回 `(TempDir, store)`：析构顺序 store 先、
/// TempDir 后（qdrant-edge 的 EdgeShard Drop 同步 flush，目录先删会 panic）。
#[cfg(all(test, feature = "rag"))]
#[must_use] 
pub fn test_store() -> (tempfile::TempDir, VectorStore) {
    let dir = tempfile::tempdir().expect("tempdir for memory test store");
    let store = VectorStore::new(dir.path());
    (dir, store)
}

/// 起一个返回固定 embedding 的本地 HTTP server（照抄 ingest.rs:231 mock 模式）。
/// 所有输入返回同一向量 → 检索 cosine=1.0，便于断言去重/阈值/作用域逻辑。
#[cfg(all(test, feature = "rag"))]
pub async fn mock_embedding_server(dim: usize) -> String {
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
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock embedding server");
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve mock embedding");
    });
    format!("http://{addr}")
}

/// 构造开启且 embedding 可达的 MemoryState（dim=8）。`base_url` 指向
/// [`mock_embedding_server`]。
#[cfg(all(test, feature = "rag"))]
pub async fn test_memory_with_embedding(base_url: &str) -> (Database, MemoryState) {
    let db = Database::new(":memory:").await.expect("in-memory db");
    let (_dir, store) = test_store();
    let llm = crate::llm::LlmState::new(None, None);
    let memory = MemoryState::new(db.clone(), store, None, llm);
    let mut s = db.memory_get_settings().await.unwrap();
    s.enabled = 1;
    s.emb_base_url = base_url.to_string();
    s.emb_api_key = "key".into();
    s.emb_model = "m".into();
    s.emb_dimension = 8;
    db.memory_upsert_settings(&s).await.unwrap();
    (db, memory)
}

#[cfg(all(test, feature = "rag"))]
mod tests {
    use super::*;

    #[test]
    fn merge_tags_unions_dedups_caps() {
        let out = merge_tags(&["a".into(), "b".into()], &["b".into(), "c".into()]);
        assert_eq!(out, vec!["a", "b", "c"]);
        let many = merge_tags(&(0..10).map(|i| format!("t{i}")).collect::<Vec<_>>(), &[]);
        assert_eq!(many.len(), MAX_TAGS, "tags 并集应封顶 MAX_TAGS");
    }

    #[test]
    fn scope_coords_normalizes() {
        assert_eq!(
            scope_coords("global", "c1", "w1"),
            ("global".to_string(), String::new(), String::new())
        );
        assert_eq!(
            scope_coords("client", "c1", "w1"),
            ("client".to_string(), "c1".to_string(), String::new())
        );
        assert_eq!(
            scope_coords("workspace", "c1", "w1"),
            ("workspace".to_string(), "c1".to_string(), "w1".to_string())
        );
        // 未知值回落 workspace
        assert_eq!(
            scope_coords("bogus", "c1", "w1"),
            ("workspace".to_string(), "c1".to_string(), "w1".to_string())
        );
    }

    #[tokio::test]
    async fn dedup_same_scope_updates_not_duplicates() {
        let base = mock_embedding_server(8).await;
        let (db, memory) = test_memory_with_embedding(&base).await;
        let s = db.memory_get_settings().await.unwrap();
        let emb = memory.embedder().await.expect("embedder configured");

        let id1 = upsert_memory_with_dedup(
            &memory,
            &s,
            &emb,
            "用户偏好简洁代码",
            "workspace",
            "c1",
            "w1",
            &["rust".into()],
            0.9,
            "s1",
            "distill",
        )
        .await
        .unwrap();
        // 同作用域近似内容（mock 同向量 → cosine=1.0 ≥ 0.9）→ 更新既有而非新建
        let id2 = upsert_memory_with_dedup(
            &memory,
            &s,
            &emb,
            "用户偏好简洁的代码风格",
            "workspace",
            "c1",
            "w1",
            &["clean".into()],
            1.0,
            "s1",
            "remember",
        )
        .await
        .unwrap();
        assert_eq!(id1, id2, "同作用域高相似应更新同一记忆");
        let row = db.memory_get_by_id(&id1).await.unwrap().unwrap();
        assert_eq!(row.content, "用户偏好简洁的代码风格", "content 以新为准");
        assert_eq!(row.confidence, 1.0, "confidence 取 max");
        let tags = parse_tags(&row.tags);
        assert!(
            tags.contains(&"rust".into()) && tags.contains(&"clean".into()),
            "tags 并集"
        );

        let all = db
            .memory_list(None, None, None, None, None, None, 100, 0)
            .await
            .unwrap();
        assert_eq!(all.len(), 1, "不应产生重复记忆");

        // 不同作用域 → 新建（不同 id）
        let id3 = upsert_memory_with_dedup(
            &memory,
            &s,
            &emb,
            "用户偏好简洁代码",
            "global",
            "",
            "",
            &[],
            0.9,
            "s1",
            "distill",
        )
        .await
        .unwrap();
        assert_ne!(id1, id3, "不同作用域应新建");
        let all = db
            .memory_list(None, None, None, None, None, None, 100, 0)
            .await
            .unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn dedup_emb_dimension_not_configured_is_err() {
        let base = mock_embedding_server(8).await;
        let (db, memory) = test_memory_with_embedding(&base).await;
        let emb = memory.embedder().await.unwrap();
        let mut s = db.memory_get_settings().await.unwrap();
        s.emb_dimension = 0; // 模拟维度未配置
        let err = upsert_memory_with_dedup(
            &memory,
            &s,
            &emb,
            "x",
            "workspace",
            "c1",
            "w1",
            &[],
            0.9,
            "s1",
            "distill",
        )
        .await
        .unwrap_err();
        assert!(err.contains("dimension"), "err: {err}");
    }
}
