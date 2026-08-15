//! 蒸馏器：会话结束（归档/删除/断线/idle）时把整会话渲染成文本，交给 LLM
//! 提炼原子事实，再逐条向量化 + 同作用域去重落库。
//!
//! 防重：`agent_sessions.distilled` 列的原子 CAS（`memory_mark_distilled_if_not`）
//! 保证多路并发触发（archive + delete + disconnect + idle）只有唯一赢家。
//!
//! 防蒸馏回环：蒸馏渲染时剥离 `<memory>...</memory>` 块与 @引用文件包装块，
//! 已注入的记忆不会被当成对话内容再次蒸馏。

use super::{
    upsert_memory_with_dedup, MemoryEvent, MemoryState, MAX_TAGS, MEMORY_CONTENT_MAX_CHARS,
    TAG_MAX_CHARS,
};
use crate::db::agent::AgentMessageRecord;
use crate::llm::{ChatCompletionRequest, ChatMessage, LlmState};

/// 蒸馏消息数下限：少于该条数的会话不蒸馏（太短无提炼价值）。
pub const MIN_DISTILL_MESSAGES: usize = 4;
/// 渲染时单条 tool 结果截断上限（字符）。
const TOOL_RESULT_TRUNCATE_CHARS: usize = 600;
/// 渲染总量上限（字节）。
const DISTILL_MAX_CHARS: usize = 32 * 1024;

/// 蒸馏输入快照：调用方先同步收集（会话元数据 + 消息）。delete 路径在删行前
/// 采集——FK 级联删会先删消息，异步蒸馏（spawn 后读 DB）会无料可用。
pub struct DistillSnapshot {
    pub session_id: String,
    pub client_id: String,
    pub workspace_id: String,
    /// 会话模型（`settings.distill_model` 未配置时的回落）。
    pub model: Option<String>,
    pub messages: Vec<AgentMessageRecord>,
}

/// 蒸馏触发（归档/断线/idle 用）：enabled 检查 → CAS → 快照 → 消息数下限 →
/// running 事件 → `tokio::spawn(do_distill)`。CAS 未命中（已蒸馏/会话不存在）
/// 直接返回，不阻塞调用方。
pub async fn trigger_distill(memory: &MemoryState, session_id: &str, trigger: &str) {
    if !memory_enabled(memory).await {
        return;
    }
    let ok = match memory.db.memory_mark_distilled_if_not(session_id).await {
        Ok(ok) => ok,
        Err(e) => {
            tracing::warn!(session_id, trigger, error = %e, "memory distill CAS failed");
            return;
        }
    };
    if !ok {
        return;
    }
    let Some(snapshot) = load_snapshot(memory, session_id).await else {
        return;
    };
    spawn_distill(memory, snapshot, trigger).await;
}

/// 蒸馏触发（delete 路径用）：调用方**先同步快照**再删行，然后调本函数。
///
/// CAS 语义与 [`trigger_distill`] 的差异：删行后 CAS 命中不了（行已不存在），
/// 但快照在手仍可蒸馏。判定：CAS 未命中时若会话行仍存在 → 已被其他路径标记
/// （跳过）；行不存在 → 删除路径的终态，快照可蒸馏。
pub async fn trigger_distill_with_snapshot(
    memory: &MemoryState,
    snapshot: DistillSnapshot,
    trigger: &str,
) {
    if !memory_enabled(memory).await {
        return;
    }
    let ok = match memory
        .db
        .memory_mark_distilled_if_not(&snapshot.session_id)
        .await
    {
        Ok(ok) => ok,
        Err(e) => {
            tracing::warn!(
                session_id = %snapshot.session_id,
                trigger,
                error = %e,
                "memory distill CAS failed"
            );
            return;
        }
    };
    if !ok {
        let session_exists = memory
            .db
            .agent_get_session(&snapshot.session_id)
            .await
            .ok()
            .flatten()
            .is_some();
        if session_exists {
            return; // 其他路径已赢 CAS（如归档），本路径让位
        }
    }
    if snapshot.messages.len() < MIN_DISTILL_MESSAGES {
        return;
    }
    spawn_distill(memory, snapshot, trigger).await;
}

/// 读蒸馏快照（会话 → workspace → 消息）。任一步失败/缺失返回 None。
pub async fn load_snapshot(memory: &MemoryState, session_id: &str) -> Option<DistillSnapshot> {
    let session = memory
        .db
        .agent_get_session(session_id)
        .await
        .ok()
        .flatten()?;
    let workspace = memory
        .db
        .agent_get_workspace(&session.workspace_id)
        .await
        .ok()
        .flatten()?;
    let messages = memory.db.agent_list_messages(session_id).await.ok()?;
    Some(DistillSnapshot {
        session_id: session_id.to_string(),
        client_id: workspace.client_id,
        workspace_id: session.workspace_id,
        model: session.model,
        messages,
    })
}

/// 广播 running 事件并后台执行蒸馏。事件发送失败静默忽略（无订阅者）。
async fn spawn_distill(memory: &MemoryState, snapshot: DistillSnapshot, trigger: &str) {
    let _ = memory.events.send(MemoryEvent {
        session_id: snapshot.session_id.clone(),
        status: "running".into(),
        facts_found: 0,
    });
    tokio::spawn(do_distill(memory.clone(), snapshot, trigger.to_string()));
}

/// 实际蒸馏：渲染 → LLM 非流式无 tools → JSON 解析 → 逐条 embed + 去重落库 →
/// done 事件。任何一步失败仅广播 failed 事件 + warn，不 panic、不阻断调用方。
async fn do_distill(memory: MemoryState, snapshot: DistillSnapshot, trigger: String) {
    let s = memory.settings().await;
    let rendered = render_distill_text(&snapshot.messages);
    if rendered.trim().is_empty() {
        return;
    }
    let model = if !s.distill_model.trim().is_empty() {
        s.distill_model.clone()
    } else {
        snapshot.model.clone().unwrap_or_default()
    };
    let text = match call_distill_llm(&memory.llm, &model, &rendered).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(session_id = %snapshot.session_id, error = %e, "distill LLM call failed");
            send_failed(&memory, &snapshot.session_id);
            return;
        }
    };
    let facts = match parse_facts(&text) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(session_id = %snapshot.session_id, error = %e, "distill parse failed");
            send_failed(&memory, &snapshot.session_id);
            return;
        }
    };
    // enabled 但 emb 配置缺失 → 静默降级（配置问题属预期路径，不广播 failed）。
    let Some(emb) = memory.embedder().await else {
        tracing::warn!(session_id = %snapshot.session_id, "distill skipped: embedding not configured");
        return;
    };
    let mut saved = 0usize;
    for fact in facts {
        let mut tags: Vec<String> = fact
            .tags
            .into_iter()
            .filter(|t| !t.trim().is_empty() && t.len() <= TAG_MAX_CHARS)
            .map(|t| t.trim().to_string())
            .collect();
        tags.truncate(MAX_TAGS);
        let (scope_type, client_id, workspace_id) =
            super::scope_coords(&fact.scope, &snapshot.client_id, &snapshot.workspace_id);
        match upsert_memory_with_dedup(
            &memory,
            &s,
            &emb,
            &fact.content,
            &scope_type,
            &client_id,
            &workspace_id,
            &tags,
            fact.confidence,
            &snapshot.session_id,
            &trigger,
        )
        .await
        {
            Ok(_) => saved += 1,
            Err(e) => {
                tracing::warn!(session_id = %snapshot.session_id, error = %e, "distill upsert memory failed");
            }
        }
    }
    let _ = memory.events.send(MemoryEvent {
        session_id: snapshot.session_id,
        status: "distilled".into(),
        facts_found: saved,
    });
}

fn send_failed(memory: &MemoryState, session_id: &str) {
    let _ = memory.events.send(MemoryEvent {
        session_id: session_id.to_string(),
        status: "failed".into(),
        facts_found: 0,
    });
}

/// 总闸 + CAS 前置 enabled 检查。
async fn memory_enabled(memory: &MemoryState) -> bool {
    memory.settings().await.enabled != 0
}

/// LLM 蒸馏 prompt：明确排除密钥/token/临时状态；scope 建议规则写入 prompt。
const DISTILL_PROMPT: &str = "你是会话记忆蒸馏器。从以下 AI 编程助手的会话记录中提炼可长期复用的\"原子事实\"，仅保留四类：机器环境（路径、语言版本、构建命令、依赖）、用户偏好（编码风格、命名习惯、工具偏好）、关键决策（架构选型、约定、选择的原因）、已知坑（易错点、踩坑教训）。

必须排除：
- 密钥、token、密码、API key、凭据（绝不记录）
- 临时状态（当前任务进度、一次性操作、调试中间值、本次会话的琐碎问答）
- 与可复用经验无关的寒暄、请求、闲聊

scope 建议规则（每条约 1-2 条短句，不展开）：
- 仅适用于本工作区/项目 → \"workspace\"
- 适用于该客户端机器上的所有项目 → \"client\"
- 适用于所有场景（用户通用偏好/通用知识）→ \"global\"

输出 JSON，不要代码围栏、不要多余文本：
{\"facts\": [{\"content\": \"原子事实（不超过 500 字）\", \"scope\": \"workspace\", \"tags\": [\"标签\"], \"confidence\": 0.9}]}
tags 最多 3 个；confidence 为 0 到 1 的数值，表示你对这条事实可复用性的把握。";

/// 非流式无 tools LLM 调用（抄 title.rs 的 resolve_with_failover +
/// build_upstream_body + execute_with_failover 模式）。
async fn call_distill_llm(llm: &LlmState, model: &str, rendered: &str) -> Result<String, String> {
    let chain = crate::llm::router::resolve_with_failover(llm, model)
        .await
        .map_err(|e| format!("model resolution failed: {e}"))?;
    let request = ChatCompletionRequest {
        model: model.to_string(),
        messages: vec![
            ChatMessage::text("system", DISTILL_PROMPT),
            ChatMessage::text("user", rendered),
        ],
        stream: false,
        max_tokens: None,
        temperature: None,
        top_p: None,
        tools: None,
        tool_choice: None,
        raw_body: None,
    };
    let req_body = crate::llm::upstream::build_upstream_body(&request);
    let outcome =
        crate::llm::upstream::execute_with_failover(&llm.breakers, &chain, &req_body, false).await;
    let resp = match outcome {
        crate::llm::upstream::FailoverOutcome::Success { resp, .. } => resp,
        crate::llm::upstream::FailoverOutcome::Exhausted { message, .. } => {
            return Err(format!("distill LLM unavailable: {message}"));
        }
    };
    let body_bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .map_err(|e| format!("failed to read distill response: {e}"))?;
    let body: serde_json::Value = serde_json::from_slice(&body_bytes)
        .map_err(|e| format!("invalid distill response JSON: {e}"))?;
    let raw = body
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    Ok(raw.to_string())
}

/// 一条待落库的蒸馏事实。
pub struct DistillFact {
    pub content: String,
    /// `workspace` | `client` | `global`（parse 阶段已归一化）。
    pub scope: String,
    pub tags: Vec<String>,
    pub confidence: f64,
}

/// 解析蒸馏 LLM 输出为事实列表。支持代码围栏（```json ... ```）、裸对象；坏
/// JSON / 缺 facts 数组 / 全部条目非法 → Err。单条非法（空 content/超长）跳过。
fn parse_facts(raw: &str) -> Result<Vec<DistillFact>, String> {
    let cleaned = strip_code_fence(raw);
    let value: serde_json::Value =
        serde_json::from_str(&cleaned).map_err(|e| format!("invalid distill JSON: {e}"))?;
    let arr = value
        .get("facts")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "distill JSON missing 'facts' array".to_string())?;
    let mut facts = Vec::new();
    for item in arr {
        let content = item
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if content.is_empty() || content.len() > MEMORY_CONTENT_MAX_CHARS {
            continue;
        }
        let scope = item
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("workspace");
        let scope = if matches!(scope, "global" | "client") {
            scope.to_string()
        } else {
            "workspace".to_string()
        };
        let tags: Vec<String> = item
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|t| t.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let confidence = item
            .get("confidence")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.8)
            .clamp(0.0, 1.0);
        facts.push(DistillFact {
            content: content.to_string(),
            scope,
            tags,
            confidence,
        });
    }
    if facts.is_empty() {
        return Err("no valid facts extracted".into());
    }
    Ok(facts)
}

/// 剥离 markdown 代码围栏（首行可带 json 标注；结尾围栏之后的杂文本丢弃）。
fn strip_code_fence(raw: &str) -> String {
    let s = raw.trim();
    if !s.starts_with("```") {
        return s.to_string();
    }
    let mut body = &s[3..];
    // 围栏首行语言标注（如 json）整行丢弃
    if let Some(nl) = body.find('\n') {
        body = &body[nl + 1..];
    }
    let end = body.find("```").unwrap_or(body.len());
    body[..end].trim().to_string()
}

/// 把消息列表渲染为蒸馏喂给 LLM 的纯文本（仿 compact.rs render_for_summary，
/// 差异：tool 结果截断 600 chars、总量 ≤32KB、剥离 `<memory>` 块与 @引用包装块）。
pub fn render_distill_text(messages: &[AgentMessageRecord]) -> String {
    let mut out = String::new();
    for m in messages {
        match (m.role.as_str(), m.tool_calls.as_deref()) {
            ("tool", _) => {
                let name = m.name.as_deref().unwrap_or("?");
                // M2 起 ACP 路径落库 content 为 JSON `{"text","status",...}`——
                // 蒸馏渲染只取 text（否则把 JSON 壳喂给蒸馏 LLM）；旧纯文本行
                // 原样使用（tool_result_text 返回 None）。
                let content = crate::agent::tool_result::tool_result_text(&m.content)
                    .unwrap_or_else(|| m.content.clone());
                let sanitized = sanitize_distill_content(&content);
                let truncated = truncate_chars(&sanitized, TOOL_RESULT_TRUNCATE_CHARS);
                out.push_str(&format!("tool({name}): {truncated}\n"));
            }
            (_, Some(calls)) if !calls.trim().is_empty() => {
                let parsed: Vec<serde_json::Value> =
                    serde_json::from_str(calls).unwrap_or_default();
                let names: Vec<&str> = parsed
                    .iter()
                    .filter_map(|c| c.pointer("/function/name").and_then(|n| n.as_str()))
                    .collect();
                out.push_str(&format!("assistant called tools: {}\n", names.join(", ")));
            }
            (role, _) => {
                let sanitized = sanitize_distill_content(&m.content);
                out.push_str(&format!("{role}: {sanitized}\n"));
            }
        }
    }
    truncate_chars(&out, DISTILL_MAX_CHARS)
}

/// 剥离 `<memory>...</memory>` 块与 inject_refs 的 `--- 引用文件:` 包装块
/// （防蒸馏回环：已注入的记忆/引用内容不当对话上下文再蒸馏）。
fn sanitize_distill_content(text: &str) -> String {
    let text = strip_block(text, "<memory>", "</memory>");
    strip_refs_wrapper(&text)
}

/// 移除 `open..close` 闭合块（不重叠）。open 存在但 close 缺失 → 删到末尾。
fn strip_block(text: &str, open: &str, close: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(open) {
        out.push_str(&rest[..start]);
        if let Some(rel) = rest[start..].find(close) {
            rest = &rest[start + rel + close.len()..];
        } else {
            rest = "";
        }
    }
    out.push_str(rest);
    out
}

/// 移除 `compose_user_message` 追加的引用文件块：
/// `--- 引用文件: {path} ---\n```\n{content}\n````
///
/// 状态机：看到 `--- 引用文件:` 头后跳过，直到第二个 ` ``` ` 行（开头一个、结尾
/// 一个）才恢复；连续多个引用块依序各处理一遍。
fn strip_refs_wrapper(text: &str) -> String {
    let mut out = String::new();
    let mut skipping = false;
    let mut fences_seen = 0usize;
    for line in text.lines() {
        if line.trim_start().starts_with("--- 引用文件:") {
            skipping = true;
            fences_seen = 0;
            continue;
        }
        if skipping {
            if line.trim() == "```" {
                fences_seen += 1;
                if fences_seen >= 2 {
                    skipping = false;
                }
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// 按字符边界截断到 max 字节并追加 `[truncated]` 标记；未超限原样返回。
fn truncate_chars(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut cut = max;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n[truncated]", &text[..cut])
}

#[cfg(all(test, feature = "rag"))]
mod tests {
    use super::*;
    use crate::db::Database;

    fn msg(role: &str, content: &str, kind: &str) -> AgentMessageRecord {
        AgentMessageRecord {
            id: format!("m-{}-{}", role, rand::random::<u64>()),
            session_id: "s1".into(),
            role: role.into(),
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            kind: kind.into(),
            parent_tool_call_id: None,
            created_at: String::new(),
        }
    }

    fn tool_msg(content: &str) -> AgentMessageRecord {
        AgentMessageRecord {
            id: format!("m-t-{}", rand::random::<u64>()),
            session_id: "s1".into(),
            role: "tool".into(),
            content: content.into(),
            tool_calls: None,
            tool_call_id: Some("c1".into()),
            name: Some("shell".into()),
            kind: "tool_result".into(),
            parent_tool_call_id: None,
            created_at: String::new(),
        }
    }

    fn tool_calls_msg() -> AgentMessageRecord {
        AgentMessageRecord {
            id: format!("m-tc-{}", rand::random::<u64>()),
            session_id: "s1".into(),
            role: "assistant".into(),
            content: String::new(),
            tool_calls: Some(
                r#"[{"id":"c1","type":"function","function":{"name":"shell"}}]"#.into(),
            ),
            tool_call_id: None,
            name: None,
            kind: "tool_calls".into(),
            parent_tool_call_id: None,
            created_at: String::new(),
        }
    }

    #[test]
    fn render_strips_memory_block() {
        let msgs = vec![
            msg("user", "帮我看看这个", "message"),
            msg(
                "assistant",
                "好的\n<memory>\n[记忆1] 历史事实\n</memory>\n继续",
                "message",
            ),
        ];
        let out = render_distill_text(&msgs);
        assert!(out.contains("帮我看看这个"));
        assert!(!out.contains("<memory>"));
        assert!(!out.contains("历史事实"));
        assert!(out.contains("继续"));
    }

    #[test]
    fn render_strips_refs_wrapper() {
        let msgs = vec![msg(
            "user",
            "看下这个\n\n--- 引用文件: src/main.rs ---\n```\nfn main() {}\n```\n后面还有",
            "message",
        )];
        let out = render_distill_text(&msgs);
        assert!(out.contains("看下这个"));
        assert!(!out.contains("--- 引用文件:"));
        assert!(!out.contains("fn main()"));
        assert!(out.contains("后面还有"));
    }

    #[test]
    fn render_strips_consecutive_refs_wrappers() {
        // 连续多个引用块：每个都剥离，后续正文保留
        let content = "改两处\n\n--- 引用文件: a.rs ---\n```\nA\n```\n--- 引用文件: b.rs ---\n```\nB\n```\n收尾";
        let out = render_distill_text(&[msg("user", content, "message")]);
        assert!(!out.contains("--- 引用文件:"));
        assert!(!out.contains("```"));
        assert!(out.contains("改两处"));
        assert!(out.contains("收尾"));
    }

    #[test]
    fn render_truncates_tool_result_at_600() {
        let big = "x".repeat(2000);
        let out = render_distill_text(&[tool_msg(&big)]);
        assert!(out.contains("tool(shell)"));
        assert!(out.contains("[truncated]"));
        assert!(out.len() < 1200);
    }

    #[test]
    fn render_total_capped_at_32kb() {
        let big = "y".repeat(40 * 1024);
        let msgs: Vec<AgentMessageRecord> =
            (0..8).map(|_| msg("assistant", &big, "message")).collect();
        let out = render_distill_text(&msgs);
        assert!(out.len() <= DISTILL_MAX_CHARS + 32);
        assert!(out.contains("[truncated]"));
    }

    #[test]
    fn render_shows_tool_call_names() {
        let out = render_distill_text(&[tool_calls_msg()]);
        assert!(out.contains("assistant called tools: shell"));
    }

    #[test]
    fn parse_facts_strips_fence_and_parses() {
        let raw = "```json\n{\"facts\": [{\"content\": \"用户偏好简洁代码\", \"scope\": \"workspace\", \"tags\": [\"rust\"], \"confidence\": 0.95}]}\n```";
        let facts = parse_facts(raw).unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].content, "用户偏好简洁代码");
        assert_eq!(facts[0].scope, "workspace");
        assert_eq!(facts[0].tags, vec!["rust"]);
        assert!((facts[0].confidence - 0.95).abs() < 1e-9);
    }

    #[test]
    fn parse_facts_bare_object() {
        let raw = "{\"facts\": [{\"content\": \"机器是 linux\", \"scope\": \"client\"}]}";
        let facts = parse_facts(raw).unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].scope, "client");
        assert_eq!(facts[0].confidence, 0.8, "缺省 confidence 0.8");
    }

    #[test]
    fn parse_facts_invalid_scope_falls_back_workspace() {
        let raw = r#"{"facts": [{"content": "x", "scope": "bogus"}]}"#;
        let facts = parse_facts(raw).unwrap();
        assert_eq!(facts[0].scope, "workspace");
    }

    #[test]
    fn parse_facts_bad_json_is_err() {
        assert!(parse_facts("not json at all").is_err());
        assert!(parse_facts(r#"{"facts": "nope"}"#).is_err());
        assert!(parse_facts(r#"{"other": []}"#).is_err());
    }

    #[test]
    fn parse_facts_skips_empty_and_oversized_entries() {
        // 全空 → Err
        assert!(parse_facts(r#"{"facts": [{"content": "  "}]}"#).is_err());
        assert!(parse_facts(r#"{"facts": []}"#).is_err());
        // 单条超长被跳过、合法条保留
        let big = "x".repeat(MEMORY_CONTENT_MAX_CHARS + 1);
        let raw = format!(r#"{{"facts": [{{"content": "{big}"}}, {{"content": "ok"}}]}}"#);
        let facts = parse_facts(&raw).unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].content, "ok");
    }

    #[tokio::test]
    async fn distill_failed_event_when_llm_unavailable() {
        // LlmState::new(None, None) 无 DB → resolve_with_failover 失败 → failed 事件。
        // 覆盖「LLM 失败静默降级 + 事件广播」路径。
        let db = Database::new(":memory:").await.unwrap();
        db.agent_create_workspace(
            "w1", "p", "nas", "host", "/p", None, None, "", None, None, None,
        )
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, Some("no-such-model"))
            .await
            .unwrap();
        for i in 0..5 {
            db.agent_add_message(&format!("m{i}"), "s1", "user", &format!("msg {i}"), None)
                .await
                .unwrap();
        }
        // enabled 开
        let mut settings = db.memory_get_settings().await.unwrap();
        settings.enabled = 1;
        db.memory_upsert_settings(&settings).await.unwrap();

        let (_dir, store) = super::super::test_store();
        let llm = LlmState::new(None, None);
        let memory = MemoryState::new(db.clone(), store, None, llm);
        let mut rx = memory.subscribe();

        trigger_distill(&memory, "s1", "archive").await;
        let ev = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("running event")
            .expect("channel closed");
        assert_eq!(ev.status, "running");
        let ev = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("failed event")
            .expect("channel closed");
        assert_eq!(ev.status, "failed");
    }
}
