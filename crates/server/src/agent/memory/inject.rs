//! 注入器：会话开始（首回合）检索一次记忆 → 组装 `<memory>` 块注入上下文。
//!
//! - runner 路径：注入 `rt.messages[0]`（system 单条），缓存 `rt.memory_block`。
//! - ACP 路径：缓存到 `SpawnedAgent.memory_block`，`prompt_inner` 发送前把块
//!   prepend 到 user content 头部。
//!
//! 单次检索并缓存（对齐 runner.rs agents_md 缓存先例）：每会话只 embedding 一次
//! 查询，避免每回合都打一次向量检索。

use super::{MemoryState, MEMORY_KB_ID};
use crate::db::memory::AgentMemoryRecord;

/// 检索某会话可注入的记忆块（作用域 global ∪ client 匹配 ∪ workspace 匹配）。
///
/// enabled 关闭 / embedding 失败 / 维度未配置 / 无命中 / 过滤后为空 → `None`
/// （静默降级，不阻断回合）。返回的块以 `<memory>` 开头、`</memory>` 结尾，
/// 蒸馏渲染会剥离（防回环）。
pub async fn retrieve_for_session(
    memory: &MemoryState,
    client_id: &str,
    workspace_id: &str,
    query_text: &str,
) -> Option<String> {
    let s = memory.settings().await;
    if s.enabled == 0 {
        return None;
    }
    let query = query_text.trim();
    if query.is_empty() {
        return None;
    }
    let emb = memory.embedder().await?;
    let Ok(query_vec) = emb.embed_one(query).await else {
        return None;
    };
    let dim = s.emb_dimension;
    if dim <= 0 {
        return None;
    }
    // over-fetch：单一全局 shard 不支持 payload 过滤，取 top_k×8（上限 50）后
    // 在 SQL 侧做作用域过滤，避免同作用域记忆因排到 top_k 之外被漏掉。
    let over = (s.top_k as usize).saturating_mul(8).clamp(1, 50);
    let hits = memory
        .store
        .search(MEMORY_KB_ID, dim as usize, &query_vec, over)
        .await;
    if hits.is_empty() {
        return None;
    }
    let ids: Vec<String> = hits.iter().map(|h| h.id.clone()).collect();
    let rows = memory.db.memory_get_by_ids(&ids).await.ok()?;
    let mut candidates: Vec<(f32, AgentMemoryRecord)> = rows
        .into_iter()
        .filter_map(|r| {
            if !super::scope_ok(
                &r.scope_type,
                &r.client_id,
                &r.workspace_id,
                client_id,
                workspace_id,
            ) {
                return None;
            }
            let score = hits
                .iter()
                .find(|h| h.id == r.id)
                .map(|h| h.score)
                .unwrap_or(0.0);
            Some((score, r))
        })
        .collect();
    if candidates.is_empty() {
        return None;
    }
    // pinned 优先，同 pinned 再按 score 降序。
    candidates.sort_by(|a, b| {
        b.1.pinned
            .cmp(&a.1.pinned)
            .then_with(|| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal))
    });
    // 阈值过滤：pinned（且 pin_always_inject=1）恒注入；其余需 ≥ score_threshold。
    let pin_always = s.pin_always_inject != 0;
    let threshold = s.score_threshold as f32;
    let items: Vec<AgentMemoryRecord> = candidates
        .into_iter()
        .filter(|(score, r)| (pin_always && r.pinned == 1) || *score >= threshold)
        .map(|(_, r)| r)
        .collect();
    if items.is_empty() {
        return None;
    }
    let block = build_memory_block(&items, s.inject_budget_tokens as usize)?;
    // 命中回写（仅实际注入的条目）。
    let hit_ids: Vec<String> = items.iter().map(|r| r.id.clone()).collect();
    let _ = memory.db.memory_bump_hits(&hit_ids).await;
    Some(block)
}

/// 组装 `<memory>` 注入块。预算按 `chars / 4` ≈ tokens 近似（同 retriever.rs 的
/// `MAX_SYSTEM_MESSAGE_TOKENS` 惯例）；超预算停止追加后续条目——只保留完整条目，
/// 绝不半截切断某条记忆的内容。无条目 → None。
pub fn build_memory_block(items: &[AgentMemoryRecord], budget_tokens: usize) -> Option<String> {
    let mut s = String::from("<memory>\n以下是来自历史会话的记忆，可能与当前任务相关：\n");
    let mut approx_tokens = s.chars().count() / 4;
    let mut added = 0usize;
    for (i, m) in items.iter().enumerate() {
        let scope_label = match m.scope_type.as_str() {
            "global" => "全局",
            "client" => "客户端",
            _ => "工作区",
        };
        let item = format!(
            "\n[记忆{}] ({scope_label}，置信度 {:.2})\n{}\n",
            i + 1,
            m.confidence,
            m.content
        );
        approx_tokens += item.chars().count() / 4;
        // 至少保留首条：预算极小（如 1 token）时也注入一条，不返回空。
        if approx_tokens > budget_tokens && added > 0 {
            break;
        }
        s.push_str(&item);
        added += 1;
    }
    if added == 0 {
        return None;
    }
    s.push_str("</memory>");
    Some(s)
}

#[cfg(all(test, feature = "rag"))]
mod tests {
    use super::*;
    use crate::db::Database;

    async fn memory_state() -> (Database, super::super::MemoryState) {
        let db = Database::new(":memory:").await.unwrap();
        let (_dir, store) = super::super::test_store();
        let llm = crate::llm::LlmState::new(None, None);
        let memory = super::super::MemoryState::new(db.clone(), store, None, llm);
        // 开启并配好 embedding（dim=8）
        let mut s = db.memory_get_settings().await.unwrap();
        s.enabled = 1;
        s.emb_base_url = "http://localhost:1/v1".into(); // 不可达 → embed 失败路径
        s.emb_api_key = "key".into();
        s.emb_model = "m".into();
        s.emb_dimension = 8;
        db.memory_upsert_settings(&s).await.unwrap();
        (db, memory)
    }

    fn record(id: &str, scope: &str, client: &str, ws: &str, pinned: bool, content: &str) -> AgentMemoryRecord {
        AgentMemoryRecord {
            id: id.into(),
            content: content.into(),
            scope_type: scope.into(),
            client_id: client.into(),
            workspace_id: ws.into(),
            tags: "[]".into(),
            confidence: 0.8,
            source_session_id: "s0".into(),
            source_trigger: "manual".into(),
            pinned: pinned as i32,
            hit_count: 0,
            last_hit_at: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn block_wraps_in_memory_tags() {
        let block = build_memory_block(&[record("m1", "workspace", "c1", "w1", false, "事实")], 1500).unwrap();
        assert!(block.starts_with("<memory>"));
        assert!(block.ends_with("</memory>"));
        assert!(block.contains("事实"));
        assert!(block.contains("工作区"));
    }

    #[test]
    fn block_budget_keeps_complete_entries() {
        // 40 条 ~500 字符记忆 → 超出 1500 token 预算 → 只保留前若干完整条目
        let items: Vec<AgentMemoryRecord> = (0..40)
            .map(|i| record(&format!("m{i}"), "global", "", "", false, &format!("z{i}-{}", "x".repeat(500))))
            .collect();
        let block = build_memory_block(&items, 1500).unwrap();
        assert!(block.contains(&items[0].content));
        assert!(
            !block.contains(&items[39].content),
            "超预算条目应被整条截断"
        );
        assert!(
            block.chars().count() / 4 <= 1500 + 256,
            "结果近似 token 不应大幅超出预算"
        );
        // 不半截：块内任意一条记忆内容要么完整出现、要么完全不出现
        for item in &items {
            if block.contains(&item.content) {
                assert!(item.content.len() <= 520, "未截断的条目应完整");
            }
        }
    }

    #[test]
    fn block_empty_items_is_none() {
        assert!(build_memory_block(&[], 1500).is_none());
    }

    #[test]
    fn block_tiny_budget_keeps_first() {
        // 预算 1 token：至少注入首条，不返回空
        let items = [record("m1", "global", "", "", false, "首条")];
        let block = build_memory_block(&items, 1).unwrap();
        assert!(block.contains("首条"));
    }

    #[tokio::test]
    async fn retrieve_disabled_returns_none() {
        let (db, memory) = memory_state().await;
        let mut s = db.memory_get_settings().await.unwrap();
        s.enabled = 0;
        db.memory_upsert_settings(&s).await.unwrap();
        assert!(retrieve_for_session(&memory, "c1", "w1", "q").await.is_none());
    }

    #[tokio::test]
    async fn retrieve_embed_failure_degrades_none() {
        // base_url 不可达 → embed 失败 → None（静默降级，不 panic、不报错）
        let (_db, memory) = memory_state().await;
        assert!(retrieve_for_session(&memory, "c1", "w1", "查询").await.is_none());
    }

    /// 直接 seed：SQLite 行 + 向量点（固定向量 0.1，dim=8）。
    async fn seed_memory(
        memory: &super::super::MemoryState,
        id: &str,
        scope: &str,
        client: &str,
        ws: &str,
        pinned: bool,
        content: &str,
    ) {
        memory
            .db
            .memory_insert(id, content, scope, client, ws, "[]", 0.8, "", "manual", pinned)
            .await
            .unwrap();
        memory
            .store
            .upsert(
                super::super::MEMORY_KB_ID,
                8,
                vec![crate::llm::rag::store::ChunkPoint {
                    id: id.into(),
                    vector: vec![0.1f32; 8],
                    doc_id: id.into(),
                    seq: 0,
                    heading_path: String::new(),
                }],
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn retrieve_scope_filter_and_order() {
        let base = super::super::mock_embedding_server(8).await;
        let (_db, memory) = super::super::test_memory_with_embedding(&base).await;
        seed_memory(&memory, "g1", "global", "", "", false, "全局事实").await;
        seed_memory(&memory, "cl1", "client", "c1", "", false, "客户端事实").await;
        seed_memory(&memory, "w1a", "workspace", "c1", "w1", false, "工作区事实A").await;
        seed_memory(&memory, "w1p", "workspace", "c1", "w1", true, "工作区置顶").await;
        seed_memory(&memory, "w2", "workspace", "c1", "w2", false, "别的工作区").await;

        // 默认阈值 0.4：全部命中（cosine=1.0）→ 作用域过滤为主
        let block = retrieve_for_session(&memory, "c1", "w1", "查询").await.unwrap();
        assert!(block.contains("全局事实"), "global 恒可见");
        assert!(block.contains("客户端事实"), "client 匹配可见");
        assert!(block.contains("工作区事实A"), "workspace 匹配可见");
        assert!(block.contains("工作区置顶"));
        assert!(!block.contains("别的工作区"), "其他 workspace 应被作用域过滤");
        // pinned 优先：置顶排在第一个 [记忆1]
        assert!(
            block.find("工作区置顶").unwrap() < block.find("全局事实").unwrap(),
            "pinned 应排最前"
        );
    }

    #[tokio::test]
    async fn retrieve_pinned_bypasses_high_threshold() {
        let base = super::super::mock_embedding_server(8).await;
        let (db, memory) = super::super::test_memory_with_embedding(&base).await;
        seed_memory(&memory, "w1a", "workspace", "c1", "w1", false, "工作区事实A").await;
        seed_memory(&memory, "w1p", "workspace", "c1", "w1", true, "工作区置顶").await;
        // 高阈值 1.5：所有 unpinned（cosine=1.0 < 1.5）被过滤；pinned 恒注入
        let mut s = db.memory_get_settings().await.unwrap();
        s.score_threshold = 1.5;
        db.memory_upsert_settings(&s).await.unwrap();

        let block = retrieve_for_session(&memory, "c1", "w1", "查询").await.unwrap();
        assert!(block.contains("工作区置顶"), "pinned 应绕过阈值恒注入");
        assert!(!block.contains("工作区事实A"), "未 pinned 低于阈值应被过滤");

        // 关闭 pin_always_inject → pinned 也需 ≥ 阈值 → 全过滤 → None
        let mut s = db.memory_get_settings().await.unwrap();
        s.pin_always_inject = 0;
        db.memory_upsert_settings(&s).await.unwrap();
        assert!(
            retrieve_for_session(&memory, "c1", "w1", "查询").await.is_none(),
            "pin_always_inject=0 时 pinned 也按阈值过滤"
        );
    }

    #[tokio::test]
    async fn retrieve_bump_hits_only_injected() {
        let base = super::super::mock_embedding_server(8).await;
        let (db, memory) = super::super::test_memory_with_embedding(&base).await;
        seed_memory(&memory, "m1", "workspace", "c1", "w1", false, "命中事实").await;
        // 其他作用域：不注入 → 不 bump
        seed_memory(&memory, "m2", "workspace", "c1", "w2", false, "别的").await;

        let block = retrieve_for_session(&memory, "c1", "w1", "查询").await.unwrap();
        assert!(block.contains("命中事实"));
        assert_eq!(db.memory_get_by_id("m1").await.unwrap().unwrap().hit_count, 1);
        assert_eq!(db.memory_get_by_id("m2").await.unwrap().unwrap().hit_count, 0);
    }

    #[test]
    fn scope_filter_rules() {
        // 覆盖作用域过滤的判定逻辑：global / client / workspace 各自命中条件。
        let c1 = "c1";
        let w1 = "w1";
        let w2 = "w2";
        // global 恒命中
        assert!(super::super::scope_ok("global", "", "", c1, w1));
        // client 需 client_id 匹配（workspace 忽略）
        assert!(super::super::scope_ok("client", c1, "", c1, w1));
        assert!(!super::super::scope_ok("client", "other", "", c1, w1));
        // workspace 需 client_id + workspace_id 都匹配
        assert!(super::super::scope_ok("workspace", c1, w1, c1, w1));
        assert!(!super::super::scope_ok("workspace", c1, w2, c1, w1));
        assert!(!super::super::scope_ok("workspace", "other", w1, c1, w1));
    }
}
