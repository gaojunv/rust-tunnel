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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memory::AgentMemoryRecord;
    use crate::test_helpers::{in_memory_db, seed_workspace_and_session};

    fn record(
        id: &str,
        scope: &str,
        client: &str,
        ws: &str,
        pinned: bool,
        content: &str,
    ) -> AgentMemoryRecord {
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

    // ── build_memory_block ────────────────────────────────

    #[test]
    fn build_memory_block_empty_returns_none() {
        assert!(build_memory_block(&[], 1500).is_none());
        assert!(build_memory_block(&[], 0).is_none());
        assert!(build_memory_block(&[], 1).is_none());
    }

    #[test]
    fn build_memory_block_single_returns_some_concatenated() {
        let items = vec![record("m1", "workspace", "c1", "w1", false, "事实A")];
        let block = build_memory_block(&items, 1500).unwrap();
        assert!(block.starts_with("<memory>"));
        assert!(block.ends_with("</memory>"));
        assert!(block.contains("事实A"));
        assert!(block.contains("[记忆1]"));
        // 多条拼接
        let items2 = vec![
            record("m1", "global", "", "", false, "第一条"),
            record("m2", "client", "c1", "", false, "第二条"),
        ];
        let block2 = build_memory_block(&items2, 1500).unwrap();
        assert!(block2.contains("第一条"));
        assert!(block2.contains("第二条"));
        assert!(block2.contains("[记忆1]"));
        assert!(block2.contains("[记忆2]"));
    }

    #[test]
    fn build_memory_block_budget_clipping_keeps_complete_entries() {
        // 40 条 ~500 字符记忆 → 超出 1500 token 预算 → 只保留前若干完整条目
        let items: Vec<AgentMemoryRecord> = (0..40)
            .map(|i| {
                record(
                    &format!("m{i}"),
                    "global",
                    "",
                    "",
                    false,
                    &format!("z{i}-{}", "x".repeat(500)),
                )
            })
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
        for item in &items {
            if block.contains(&item.content) {
                assert!(item.content.len() <= 520, "未截断的条目应完整");
            }
        }
    }

    #[test]
    fn build_memory_block_tiny_budget_keeps_first() {
        let items = [record("m1", "global", "", "", false, "首条")];
        let block = build_memory_block(&items, 1).unwrap();
        assert!(block.contains("首条"));
        // 预算 0 也保留首条（added==0 时不触发 break）
        let block0 = build_memory_block(&items, 0).unwrap();
        assert!(block0.contains("首条"));
    }

    #[test]
    fn build_memory_block_record_fields_serialized() {
        // scope 映射
        let g = record("m1", "global", "", "", false, "global内容");
        let c = record("m2", "client", "c1", "", false, "client内容");
        let w = record("m3", "workspace", "c1", "w1", false, "workspace内容");
        let bg = build_memory_block(&[g], 1500).unwrap();
        assert!(bg.contains("全局"), "global 应映射为 全局");
        assert!(bg.contains("global内容"));
        let bc = build_memory_block(&[c], 1500).unwrap();
        assert!(bc.contains("客户端"), "client 应映射为 客户端");
        assert!(bc.contains("client内容"));
        let bw = build_memory_block(&[w], 1500).unwrap();
        assert!(bw.contains("工作区"), "workspace 应映射为 工作区");
        assert!(bw.contains("workspace内容"));

        // confidence 保留两位小数
        let mut r = record("m4", "global", "", "", false, "conf");
        r.confidence = 0.85;
        let b = build_memory_block(&[r.clone()], 1500).unwrap();
        assert!(b.contains("0.85"), "confidence 应格式化为 0.85");
        r.confidence = 1.0;
        let b2 = build_memory_block(&[r], 1500).unwrap();
        assert!(b2.contains("1.00"));

        // 内容原样保留，包含特殊字符
        let special = record("m5", "global", "", "", false, "特殊字符：\n换行\t制表");
        let bs = build_memory_block(&[special], 1500).unwrap();
        assert!(bs.contains("特殊字符：\n换行\t制表"));

        // <memory> 包裹
        let r2 = record("m6", "global", "", "", false, "x");
        let full = build_memory_block(&[r2], 1500).unwrap();
        assert!(full.starts_with("<memory>"));
        assert!(full.ends_with("</memory>"));
    }

    #[tokio::test]
    async fn build_memory_block_with_seeded_helpers() {
        // 演示使用 test_helpers 构造数据（满足任务要求）
        let db = in_memory_db().await;
        let sess_id = seed_workspace_and_session(&db).await;
        assert_eq!(sess_id, "sess-test");
        // 同时验证 DB 中 workspace/session 已创建
        let ws: Option<String> =
            sqlx::query_scalar("SELECT id FROM agent_workspaces WHERE id='ws-test'")
                .fetch_optional(&db.pool)
                .await
                .unwrap();
        assert_eq!(ws.as_deref(), Some("ws-test"));
        // build_memory_block 本身不依赖 DB，但此处展示 helpers 可用
        let rec = record(
            "m1",
            "workspace",
            "test-client",
            "ws-test",
            false,
            "seeded helper 事实",
        );
        let block = build_memory_block(&[rec], 1500).unwrap();
        assert!(block.contains("seeded helper 事实"));
    }

    // ── retrieve_for_session（脱离 LLM/Embedder 的 early-return 路径）──
    // 注意 VectorStore/TempDir 析构顺序：TempDir 必须比 VectorStore/MemoryState 活得久，
    // 否则 EdgeShard Drop 时 flush 已删目录会 panic。所有含向量检索的用例显式绑定
    // (_dir, store) 且 _dir 声明在 MemoryState 之前（Rust 逆序析构保证 _dir 最后 drop）。

    #[tokio::test]
    #[cfg(feature = "rag")]
    async fn retrieve_disabled_returns_none() {
        let db = in_memory_db().await;
        let _sess = seed_workspace_and_session(&db).await;
        // 正确顺序：_dir 先声明，store 次之，memory 最后 → drop 时 memory/store/_dir
        let (_dir, store) = super::super::test_store();
        let llm = crate::llm::LlmState::new(None, None);
        let memory = super::super::MemoryState::new(db.clone(), store, None, llm);
        // 默认 enabled=0
        assert!(
            retrieve_for_session(&memory, "test-client", "ws-test", "hello")
                .await
                .is_none()
        );
        // 显式开启后再关闭
        let mut s = db.memory_get_settings().await.unwrap();
        s.enabled = 1;
        s.emb_base_url = "http://localhost:1".into();
        s.emb_api_key = "key".into();
        s.emb_model = "m".into();
        s.emb_dimension = 8;
        db.memory_upsert_settings(&s).await.unwrap();
        let mut s2 = db.memory_get_settings().await.unwrap();
        s2.enabled = 0;
        db.memory_upsert_settings(&s2).await.unwrap();
        assert!(retrieve_for_session(&memory, "test-client", "ws-test", "q")
            .await
            .is_none());
    }

    #[tokio::test]
    #[cfg(feature = "rag")]
    async fn retrieve_empty_query_returns_none() {
        let db = in_memory_db().await;
        let _sess = seed_workspace_and_session(&db).await;
        let (_dir, store) = super::super::test_store();
        let llm = crate::llm::LlmState::new(None, None);
        let memory = super::super::MemoryState::new(db.clone(), store, None, llm);
        let mut s = db.memory_get_settings().await.unwrap();
        s.enabled = 1;
        s.emb_base_url = "http://localhost:1".into();
        s.emb_api_key = "key".into();
        s.emb_model = "m".into();
        s.emb_dimension = 8;
        db.memory_upsert_settings(&s).await.unwrap();
        assert!(retrieve_for_session(&memory, "test-client", "ws-test", "")
            .await
            .is_none());
        assert!(
            retrieve_for_session(&memory, "test-client", "ws-test", "   ")
                .await
                .is_none()
        );
        assert!(
            retrieve_for_session(&memory, "test-client", "ws-test", "\n\t ")
                .await
                .is_none()
        );
    }

    #[tokio::test]
    #[cfg(feature = "rag")]
    async fn retrieve_embed_failure_degrades_none() {
        let db = in_memory_db().await;
        let _sess = seed_workspace_and_session(&db).await;
        let (_dir, store) = super::super::test_store();
        let llm = crate::llm::LlmState::new(None, None);
        let memory = super::super::MemoryState::new(db.clone(), store, None, llm);
        let mut s = db.memory_get_settings().await.unwrap();
        s.enabled = 1;
        s.emb_base_url = "http://localhost:1/v1".into(); // 不可达
        s.emb_api_key = "key".into();
        s.emb_model = "m".into();
        s.emb_dimension = 8;
        db.memory_upsert_settings(&s).await.unwrap();
        assert!(
            retrieve_for_session(&memory, "test-client", "ws-test", "查询")
                .await
                .is_none()
        );
    }

    #[tokio::test]
    #[cfg(feature = "rag")]
    async fn retrieve_zero_dimension_returns_none() {
        let db = in_memory_db().await;
        let _sess = seed_workspace_and_session(&db).await;
        let base = super::super::mock_embedding_server(8).await;
        // _dir 先于 store/memory 声明，保证最后 drop
        let (_dir, store) = super::super::test_store();
        let llm = crate::llm::LlmState::new(None, None);
        let memory = super::super::MemoryState::new(db.clone(), store, None, llm);
        let mut s = db.memory_get_settings().await.unwrap();
        s.enabled = 1;
        s.emb_base_url = base;
        s.emb_api_key = "key".into();
        s.emb_model = "m".into();
        s.emb_dimension = 0; // 未配置
        db.memory_upsert_settings(&s).await.unwrap();
        assert!(
            retrieve_for_session(&memory, "test-client", "ws-test", "hello")
                .await
                .is_none()
        );
    }

    #[cfg(feature = "rag")]
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
            .memory_insert(
                id, content, scope, client, ws, "[]", 0.8, "", "manual", pinned,
            )
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
    #[cfg(feature = "rag")]
    async fn retrieve_scope_filter_and_order_with_helpers() {
        let db = in_memory_db().await;
        let _sess = seed_workspace_and_session(&db).await;
        db.agent_create_workspace(
            "ws2",
            "ws2-name",
            "test-client",
            "host",
            "/tmp",
            None,
            None,
            "",
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        let base = super::super::mock_embedding_server(8).await;
        let (_dir, store) = super::super::test_store();
        let llm = crate::llm::LlmState::new(None, None);
        let memory = super::super::MemoryState::new(db.clone(), store, None, llm);
        let mut s = db.memory_get_settings().await.unwrap();
        s.enabled = 1;
        s.emb_base_url = base;
        s.emb_api_key = "key".into();
        s.emb_model = "m".into();
        s.emb_dimension = 8;
        db.memory_upsert_settings(&s).await.unwrap();

        seed_memory(&memory, "g1", "global", "", "", false, "全局事实").await;
        seed_memory(
            &memory,
            "cl1",
            "client",
            "test-client",
            "",
            false,
            "客户端事实",
        )
        .await;
        seed_memory(
            &memory,
            "w1a",
            "workspace",
            "test-client",
            "ws-test",
            false,
            "工作区事实A",
        )
        .await;
        seed_memory(
            &memory,
            "w1p",
            "workspace",
            "test-client",
            "ws-test",
            true,
            "工作区置顶",
        )
        .await;
        seed_memory(
            &memory,
            "w2",
            "workspace",
            "test-client",
            "ws2",
            false,
            "别的工作区",
        )
        .await;

        let block = retrieve_for_session(&memory, "test-client", "ws-test", "查询")
            .await
            .unwrap();
        assert!(block.contains("全局事实"), "global 恒可见");
        assert!(block.contains("客户端事实"), "client 匹配可见");
        assert!(block.contains("工作区事实A"), "workspace 匹配可见");
        assert!(block.contains("工作区置顶"));
        assert!(
            !block.contains("别的工作区"),
            "其他 workspace 应被作用域过滤"
        );
        assert!(
            block.find("工作区置顶").unwrap() < block.find("全局事实").unwrap(),
            "pinned 应排最前"
        );
    }

    #[tokio::test]
    #[cfg(feature = "rag")]
    async fn retrieve_pinned_bypasses_high_threshold() {
        let db = in_memory_db().await;
        let _ = seed_workspace_and_session(&db).await;
        let base = super::super::mock_embedding_server(8).await;
        let (_dir, store) = super::super::test_store();
        let llm = crate::llm::LlmState::new(None, None);
        let memory = super::super::MemoryState::new(db.clone(), store, None, llm);
        let mut s = db.memory_get_settings().await.unwrap();
        s.enabled = 1;
        s.emb_base_url = base;
        s.emb_api_key = "key".into();
        s.emb_model = "m".into();
        s.emb_dimension = 8;
        db.memory_upsert_settings(&s).await.unwrap();

        seed_memory(
            &memory,
            "w1a",
            "workspace",
            "test-client",
            "ws-test",
            false,
            "工作区事实A",
        )
        .await;
        seed_memory(
            &memory,
            "w1p",
            "workspace",
            "test-client",
            "ws-test",
            true,
            "工作区置顶",
        )
        .await;
        let mut s = db.memory_get_settings().await.unwrap();
        s.score_threshold = 1.5;
        db.memory_upsert_settings(&s).await.unwrap();

        let block = retrieve_for_session(&memory, "test-client", "ws-test", "查询")
            .await
            .unwrap();
        assert!(block.contains("工作区置顶"), "pinned 应绕过阈值恒注入");
        assert!(!block.contains("工作区事实A"), "未 pinned 低于阈值应被过滤");

        let mut s = db.memory_get_settings().await.unwrap();
        s.pin_always_inject = 0;
        db.memory_upsert_settings(&s).await.unwrap();
        assert!(
            retrieve_for_session(&memory, "test-client", "ws-test", "查询")
                .await
                .is_none(),
            "pin_always_inject=0 时 pinned 也按阈值过滤"
        );
    }

    #[tokio::test]
    #[cfg(feature = "rag")]
    async fn retrieve_bump_hits_only_injected() {
        let db = in_memory_db().await;
        let _ = seed_workspace_and_session(&db).await;
        let base = super::super::mock_embedding_server(8).await;
        let (_dir, store) = super::super::test_store();
        let llm = crate::llm::LlmState::new(None, None);
        let memory = super::super::MemoryState::new(db.clone(), store, None, llm);
        let mut s = db.memory_get_settings().await.unwrap();
        s.enabled = 1;
        s.emb_base_url = base;
        s.emb_api_key = "key".into();
        s.emb_model = "m".into();
        s.emb_dimension = 8;
        db.memory_upsert_settings(&s).await.unwrap();

        seed_memory(
            &memory,
            "m1",
            "workspace",
            "test-client",
            "ws-test",
            false,
            "命中事实",
        )
        .await;
        seed_memory(
            &memory,
            "m2",
            "workspace",
            "test-client",
            "ws2",
            false,
            "别的",
        )
        .await;
        db.agent_create_workspace(
            "ws2",
            "ws2-name",
            "test-client",
            "host",
            "/tmp",
            None,
            None,
            "",
            None,
            None,
            None,
            None,
        )
        .await
        .ok();

        let block = retrieve_for_session(&memory, "test-client", "ws-test", "查询")
            .await
            .unwrap();
        assert!(block.contains("命中事实"));
        assert_eq!(
            db.memory_get_by_id("m1").await.unwrap().unwrap().hit_count,
            1
        );
        assert_eq!(
            db.memory_get_by_id("m2").await.unwrap().unwrap().hit_count,
            0
        );
    }
}
