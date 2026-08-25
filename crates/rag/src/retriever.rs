//! 检索：查询向量 → top-K → 阈值过滤 → 组装注入文本。

use super::embedder::Embedder;
use super::store::VectorStore;
use rust_tunnel_common::crypto::decrypt_field;
use rust_tunnel_common::crypto::LlmCipher;
use rust_tunnel_persistence::rag::RagKnowledgeBaseRecord;
use rust_tunnel_persistence::Database;

#[derive(Debug, Clone)]
pub struct RetrievedChunk {
    pub heading_path: String,
    pub content: String,
    pub score: f32,
}

/// 丢弃低于阈值的命中。
#[must_use] 
pub fn filter_by_threshold(chunks: Vec<RetrievedChunk>, threshold: f64) -> Vec<RetrievedChunk> {
    chunks
        .into_iter()
        .filter(|c| f64::from(c.score) >= threshold)
        .collect()
}

/// 检索注入所需的 chunk（已按分数排序、阈值过滤）。
pub async fn retrieve(
    db: &Database,
    store: &VectorStore,
    cipher: Option<&LlmCipher>,
    kb: &RagKnowledgeBaseRecord,
    query_text: &str,
) -> Vec<RetrievedChunk> {
    // embedding 失败 → 空（降级）
    let api_key = decrypt_field(cipher, &kb.emb_api_key).unwrap_or_else(|_| kb.emb_api_key.clone());
    let embedder = Embedder::new(&kb.emb_base_url, &api_key, &kb.emb_model);
    let Ok(query_vec) = embedder.embed_one(query_text).await else {
        tracing::warn!("rag: embedding query failed for kb {}", kb.id);
        return Vec::new();
    };
    let hits = store
        .search(
            &kb.id,
            kb.emb_dimension as usize,
            &query_vec,
            kb.top_k as usize,
        )
        .await;
    if hits.is_empty() {
        return Vec::new();
    }
    let ids: Vec<String> = hits.iter().map(|h| h.id.clone()).collect();
    let Ok(chunks) = db.rag_get_chunks_by_ids(&ids).await else {
        tracing::warn!("rag: failed to load chunks for kb {}", kb.id);
        return Vec::new();
    };
    // 用 score 关联 chunk（point id == chunk id），保持分数排序
    let score_of = |id: &str| {
        hits.iter()
            .find(|h| h.id == id)
            .map_or(0.0, |h| h.score)
    };
    let mut out: Vec<RetrievedChunk> = chunks
        .into_iter()
        .map(|c| RetrievedChunk {
            heading_path: c.heading_path,
            content: c.content,
            score: score_of(&c.id),
        })
        .collect();
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    filter_by_threshold(out, kb.score_threshold)
}

/// 注入的 knowledge_base system 消息的近似 token 预算（chars/4 估算）。
/// 超过预算即丢弃后续 chunk —— 保留前 N 个**完整** chunk，不半截切断，
/// 保证 RAG 背景资料再大也不会撑爆上游模型上下文（顶层还有 top_k ≤ 20 约束）。
pub const MAX_SYSTEM_MESSAGE_TOKENS: usize = 8000;

/// 组装注入的 knowledge_base system 消息文本。
///
/// 按 `chars / 4` 近似累计 token，超过 [`MAX_SYSTEM_MESSAGE_TOKENS`] 后
/// 停止追加后续 chunk（只保留完整 chunk，绝不切断某个 chunk 的内容）。
#[must_use] 
pub fn build_system_message(chunks: &[RetrievedChunk]) -> String {
    let mut s = String::from(
        "<knowledge_base>\n以下是可参考的背景资料，请优先依据它们回答；若与问题无关可忽略。\n",
    );
    // 前缀本身的近似 token 数
    let mut approx_tokens = s.chars().count() / 4;
    for (i, c) in chunks.iter().enumerate() {
        let item = format!(
            "\n[资料{}] (来源：{})\n{}\n",
            i + 1,
            c.heading_path,
            c.content
        );
        approx_tokens += item.chars().count() / 4;
        if approx_tokens > MAX_SYSTEM_MESSAGE_TOKENS {
            break;
        }
        s.push_str(&item);
    }
    s.push_str("</knowledge_base>");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_below_threshold() {
        let pts = vec![
            RetrievedChunk {
                heading_path: "a".into(),
                content: "x".into(),
                score: 0.9,
            },
            RetrievedChunk {
                heading_path: "b".into(),
                content: "y".into(),
                score: 0.2,
            },
        ];
        let kept = filter_by_threshold(pts, 0.3);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].score, 0.9);
    }

    #[test]
    fn system_message_wraps_in_kb_tags() {
        let chunks = vec![RetrievedChunk {
            heading_path: "指南/安装".into(),
            content: "步骤".into(),
            score: 0.9,
        }];
        let msg = build_system_message(&chunks);
        assert!(msg.starts_with("<knowledge_base>"));
        assert!(msg.ends_with("</knowledge_base>"));
        assert!(msg.contains("指南/安装"));
        assert!(msg.contains("步骤"));
    }

    #[test]
    fn system_message_budget_includes_all_chunks_when_within_limit() {
        // 10 个 ~1000 字符的 chunk（≈250 tokens/chunk）→ 总预算内，全部保留。
        let chunks: Vec<RetrievedChunk> = (0..10)
            .map(|i| RetrievedChunk {
                heading_path: format!("h{i}"),
                content: format!("y{}", "x".repeat(1000)),
                score: 0.9,
            })
            .collect();
        let msg = build_system_message(&chunks);
        assert!(msg.contains(&chunks[0].content), "首 chunk 应保留");
        assert!(msg.contains(&chunks[9].content), "预算内尾 chunk 应保留");
        // 编号连续（无中间断档）
        assert!(msg.contains("[资料1]") && msg.contains("[资料10]"));
    }

    #[test]
    fn system_message_budget_truncates_keeps_complete_chunks() {
        // 40 个 ~1000 字符 chunk（≈250 tokens each）→ 远超 8000 token 预算。
        // 截断语义：保留前 N 个完整 chunk，不半截切断任何 chunk 的内容。
        let chunks: Vec<RetrievedChunk> = (0..40)
            .map(|i| RetrievedChunk {
                heading_path: format!("h{i}"),
                content: format!("z{i}-{}", "x".repeat(1000)),
                score: 0.9,
            })
            .collect();
        let msg = build_system_message(&chunks);
        assert!(msg.contains(&chunks[0].content), "首 chunk 应保留");
        assert!(
            !msg.contains(&chunks[39].content),
            "超预算的尾 chunk 应被截断"
        );
        assert!(
            msg.chars().count() / 4 <= MAX_SYSTEM_MESSAGE_TOKENS + 256,
            "结果近似 token 不应大幅超出预算"
        );
        assert!(msg.starts_with("<knowledge_base>"));
        assert!(msg.ends_with("</knowledge_base>"));
    }
}
