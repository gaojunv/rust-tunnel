//! 事件/消息落库逻辑：把规范化 WS 帧与回合缓冲写入 SQLite（best-effort）。
//!
//! 落库不依赖 WS 连接存活——断线期间后台跑完的回合同样可追溯（评审修复：
//! persist 移出 ws_tx guard）。

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::db::Database;

use super::{SpawnedAgent, TurnSegment};

/// 把规范化 WS 帧落库（best-effort：失败仅记日志，不影响实时推送）。
///
/// - 文本/thought：按 session 缓冲在 `SpawnedAgent`，终态回调统一落一行；
/// - tool_call/tool_result/plan：到达即落；session_title 写回 sessions 表。
///
/// 落库不依赖 WS 连接存活——断线期间后台跑完的回合同样可追溯。
pub(super) async fn persist_acp_frame(
    db: &Database,
    sessions: &Arc<Mutex<HashMap<String, SpawnedAgent>>>,
    sid: &str,
    frame: &serde_json::Value,
) {
    match frame["type"].as_str().unwrap_or("") {
        "assistant_chunk" => {
            let mut map = sessions.lock().await;
            if let Some(a) = map.get_mut(sid) {
                let content = frame["content"].as_str().unwrap_or("");
                let is_thought = frame["thought"].as_bool().unwrap_or(false);
                // 子 agent 归属：父 Task 工具调用 id（主 agent 文本为 None）。
                let parent = frame["parent_tool_call_id"].as_str().map(str::to_string);
                // 同类型且同 parent 的相邻 chunk 合并进当前段（流式分段到达）；
                // 正文↔思考切换或归属变化（主/子 agent 文本交错）时开新段，保住
                // 交错顺序——flush 按此落库，刷新后顺序才与对话一致、归属正确。
                let appended = match a.turn_segments.last_mut() {
                    Some(last)
                        if last.thought == is_thought && last.parent_tool_call_id == parent =>
                    {
                        last.content.push_str(content);
                        true
                    }
                    _ => false,
                };
                if !appended {
                    a.turn_segments.push(TurnSegment {
                        thought: is_thought,
                        content: content.to_string(),
                        parent_tool_call_id: parent,
                    });
                }
            }
        }
        "tool_call" => {
            // 工具边界 flush：把此前缓冲的 assistant 文本/thought 先落库，再落
            // tool_call 行——DB rowid 顺序 = 对话顺序（文本出现在其调用的工具
            // 之前）。否则中途刷新时 DB 里缺当前工具之前的文本段，前端历史里
            // 这段文本消失（顺序乱）。终态 flush 只冲最后一段，行为不变。
            flush_acp_turn_buffers(db, sessions, sid).await;
            let mut call = serde_json::json!([{
                "id": frame["id"],
                "name": frame["name"],
                "arguments": frame.get("args").cloned().unwrap_or(serde_json::Value::Null),
                "tool_kind": frame["tool_kind"],
                "diffs": frame.get("diffs").cloned().unwrap_or(serde_json::Value::Null),
                "locations": frame.get("locations").cloned().unwrap_or(serde_json::Value::Null),
            }]);
            // 子 agent 归属：数组元素同步带 parent_tool_call_id（有值时），
            // 前端历史卡片据此归组。
            if let Some(parent) = frame.get("parent_tool_call_id") {
                call[0]["parent_tool_call_id"] = parent.clone();
            }
            let parent = frame["parent_tool_call_id"].as_str();
            let msg_id = format!("{:032x}", rand::random::<u128>());
            // upsert：同一 (session_id, tool_call_id) 收敛为一行，避免每个事件
            // 纯 INSERT 造成刷新后重复卡片。
            if let Err(e) = db
                .agent_upsert_tool_call(
                    &msg_id,
                    sid,
                    frame["id"].as_str().unwrap_or_default(),
                    frame["name"].as_str(),
                    &call.to_string(),
                    parent,
                )
                .await
            {
                tracing::warn!(session_id = %sid, "persist tool_call failed: {e}");
            }
        }
        "tool_result" => {
            let parent = frame["parent_tool_call_id"].as_str();
            let msg_id = format!("{:032x}", rand::random::<u128>());
            // M2 结构化落库：content 从纯文本改写为 JSON
            // `{"text","status","diffs"?,"locations"?}`（与前端 ChatStream 契约
            // 严格一致，见 agent/tool_result.rs）。status 落库修复「失败工具刷新后
            // 恒显 ✓」——旧实现只落 result 文本、status 丢失；diffs/locations 落库
            // 使刷新后 diff 展示不丢（此前仅 ToolCallUpdate 实时帧携带）。
            // 空占位语义保持：中间态（running/completed 且无任何产出）传 ""，upsert
            // 不覆盖已落库的真实结果；failed 等异常终态即使 text 为空也要落库。
            let content = crate::tool_result::tool_result_persist_content(
                frame["result"].as_str(),
                frame["status"].as_str(),
                frame.get("diffs"),
                frame.get("locations"),
            );
            // upsert：ToolCallUpdate 中间态（空 result）与终态按同一
            // (session_id, tool_call_id) 收敛，终态覆盖中间态空占位。
            if let Err(e) = db
                .agent_upsert_tool_result(
                    &msg_id,
                    sid,
                    frame["id"].as_str().unwrap_or_default(),
                    frame["name"].as_str(),
                    &content,
                    parent,
                )
                .await
            {
                tracing::warn!(session_id = %sid, "persist tool_result failed: {e}");
            }
        }
        "plan" => {
            // 同 tool_call：plan 前若有已缓冲文本（ACP 常先出 plan 再出正文，
            // 但顺序不定），先落库保证边界前文本不丢。
            flush_acp_turn_buffers(db, sessions, sid).await;
            let msg_id = format!("{:032x}", rand::random::<u128>());
            let entries = frame["entries"].to_string();
            if let Err(e) = db
                .agent_add_message_v2(&rust_tunnel_persistence::agent::AgentMessageOpts {
                    id: msg_id,
                    session_id: sid.to_owned(),
                    role: "assistant".to_owned(),
                    content: entries,
                    tool_calls: None,
                    tool_call_id: None,
                    name: Some("plan".to_owned()),
                    kind: "message".to_owned(),
                    parent_tool_call_id: None, // plan 不归属任何子 agent
                })
                .await
            {
                tracing::warn!(session_id = %sid, "persist plan failed: {e}");
            }
        }
        "session_title" => {
            if let Some(title) = frame["title"].as_str() {
                if let Err(e) = db.agent_update_session_title(sid, title).await {
                    tracing::warn!(session_id = %sid, "persist session title failed: {e}");
                }
            }
        }
        "usage" => {
            // 上下文用量快照：覆盖式写入 sessions 表（仅保留最近一次，不做历史累计），
            // 刷新后前端从 session DTO 恢复用量条。
            let used = frame["used"].as_i64();
            let size = frame["size"].as_i64();
            if let Err(e) = db.agent_update_session_context_usage(sid, used, size).await {
                tracing::warn!(session_id = %sid, "persist context usage failed: {e}");
            }
        }
        "attachment" => {
            // 多模态占位帧（image/audio/resource）：只落元信息（不透传 base64 数据），
            // 刷新后历史里以附件卡片回放。
            let msg_id = format!("{:032x}", rand::random::<u128>());
            if let Err(e) = db
                .agent_add_message_v2(&rust_tunnel_persistence::agent::AgentMessageOpts {
                    id: msg_id,
                    session_id: sid.to_owned(),
                    role: "assistant".to_owned(),
                    content: frame.to_string(),
                    tool_calls: None,
                    tool_call_id: None,
                    name: Some("attachment".to_owned()),
                    kind: "message".to_owned(),
                    parent_tool_call_id: frame["parent_tool_call_id"].as_str().map(str::to_owned),
                })
                .await
            {
                tracing::warn!(session_id = %sid, "persist attachment failed: {e}");
            }
        }
        _ => {}
    }
}

/// 回合终态：把缓冲的 assistant 输出片段按到达顺序各落一行并清空缓冲。
/// 取消/错误/断线终态同样落已有缓冲（用户能看到的那部分回合过程可追溯）。
/// 注意：必须按 `turn_segments` 顺序落库（思考 → 其后正文），不可先正文后思考
/// ——否则 DB rowid 顺序反了，刷新后历史里思考卡与回复顺序颠倒。
pub(super) async fn flush_acp_turn_buffers(
    db: &Database,
    sessions: &Arc<Mutex<HashMap<String, SpawnedAgent>>>,
    sid: &str,
) {
    let segments = {
        let mut map = sessions.lock().await;
        let Some(a) = map.get_mut(sid) else {
            return;
        };
        std::mem::take(&mut a.turn_segments)
    };
    for seg in segments {
        if seg.content.is_empty() {
            continue;
        }
        let name = seg.thought.then_some("thought");
        let msg_id = format!("{:032x}", rand::random::<u128>());
        if let Err(e) = db
            .agent_add_message_v2(&rust_tunnel_persistence::agent::AgentMessageOpts {
                id: msg_id,
                session_id: sid.to_owned(),
                role: "assistant".to_owned(),
                content: seg.content,
                tool_calls: None,
                tool_call_id: None,
                name: name.map(str::to_owned),
                kind: "message".to_owned(),
                parent_tool_call_id: seg.parent_tool_call_id,
            })
            .await
        {
            tracing::warn!(session_id = %sid, "persist turn text failed: {e}");
        }
    }
}
