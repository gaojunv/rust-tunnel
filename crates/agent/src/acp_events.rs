//! Map ACP session updates to the existing WebSocket event JSON shapes
//! (assistant_chunk / tool_call / tool_result / done), so the frontend
//! needs no changes for the ACP path.
//!
//! 纯函数、无 I/O：单独文件便于单测（用 `serde_json::from_value` 构造
//! ACP crate 的 fixture，避免手写嵌套结构）。

use agent_client_protocol::schema::v1::{
    AvailableCommandsUpdate, ContentBlock, Meta, Plan, SessionUpdate, ToolCallContent,
    ToolCallLocation, ToolCallStatus, ToolKind,
};
use agent_client_protocol::schema::MaybeUndefined;

/// 把一个 ACP update 映射为现有 WS 帧；无需推送的更新返回 None。
///
/// 帧形状对齐 `src/server/agent/runner.rs` 现有 WS 协议（并扩展了卡片化字段）：
/// - `assistant_chunk` → `{"type", "content", "thought"?}`
/// - `tool_call`       → `{"type", "id", "name", "status", "args"?, "tool_kind", "diffs"?, "locations"?}`
/// - `tool_call_update`→ `{"type": "tool_result", "id", "name"?, "status", "result"?, "tool_kind"?, "diffs"?, "locations"?}`
/// - `plan`            → `{"type", "entries": [{content, status}]}`
/// - `session_info_update` → `{"type": "session_title", "title"}`
/// - `usage_update`    → `{"type": "usage", "used", "size"}`
/// - `current_mode_update` → `{"type": "current_mode_update", "mode_id"}`
/// - `config_option_update` → `{"type": "config_option_update", "options"}`
///
/// 子 agent 归属（opt-in 的 "nested subagent transcripts" 约定，见
/// <https://github.com/zed-industries/claude-code-acp> ）：事件 `_meta.claudeCode`
/// 携带 `parentToolUseId`/`subagent` 时，tool_call / tool_result / assistant_chunk
/// 帧额外输出 `parent_tool_call_id`（有值才输出）与 `is_subagent`（仅 true 时
/// 输出），前端据此按父子关系分组渲染。无 `_meta` 的事件字段缺省，完全无感降级。
// 对 9 个 ACP 帧变体的扁平派发，共享大量局部状态，拆分会把相关逻辑散到多个签名里反而降低可读性。
#[allow(clippy::too_many_lines, reason = "对 9 个 ACP 帧变体的扁平派发，拆分会把相关逻辑散到多个签名里反而降低可读性")]
#[allow(clippy::match_same_arms, reason = "UserMessageChunk 显式分支为文档目的，虽与通配分支同为 None，但中间夹着关键业务分支，合并会掩盖意图")]
pub fn map_update(update: &SessionUpdate) -> Option<serde_json::Value> {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => {
            map_text_chunk(&chunk.content, chunk.meta.as_ref(), false)
        }
        SessionUpdate::AgentThoughtChunk(chunk) => {
            map_text_chunk(&chunk.content, chunk.meta.as_ref(), true)
        }
        SessionUpdate::UserMessageChunk(_) => None, // 用户消息前端自渲染，避免重复
        SessionUpdate::ToolCall(tc) => {
            let mut frame = serde_json::json!({
                "type": "tool_call",
                "id": tc.tool_call_id.to_string(),
                "name": tc.title,
                "status": status_str(Some(tc.status)),
                "tool_kind": kind_str(tc.kind),
            });
            if let Some(args) = &tc.raw_input {
                frame["args"] = serde_json::Value::String(encode_raw(args));
            }
            let mut diffs = extract_diffs(&tc.content);
            // claude-code Edit 形态：diff 在 raw_input 的 old_string/new_string
            if diffs.is_empty() {
                if let Some(raw) = &tc.raw_input {
                    diffs = extract_raw_edit_diff(raw);
                }
            }
            if !diffs.is_empty() {
                frame["diffs"] = serde_json::Value::Array(diffs);
            }
            let locations = extract_locations(&tc.locations);
            if !locations.is_empty() {
                frame["locations"] = serde_json::Value::Array(locations);
            }
            apply_claude_code_meta(&mut frame, tc.meta.as_ref());
            Some(frame)
        }
        SessionUpdate::ToolCallUpdate(upd) => {
            // ACP 的 ToolCallUpdate 常精简（claude-code 常省略 status，只带
            // raw_output）。status 缺失但结果已产出（raw_output/content 携带
            // 信息）→ completed，不能沿用 ToolCall 的 status_str（None→running），
            // 否则已完成工具的前端卡片永远转圈（Bug 3）。真·中间状态更新
            // （无结果、只改 title/content 为空）仍按 running 处理。
            // 例外：subagent 父卡（is_subagent=true 且无 parentToolUseId，即 Task
            // 工具调用自身的进度快照）的 status 缺失更新常带部分输出（子 agent
            // 的中间结果/文本），has_result ≠ 执行完成——误判 completed 会让前端
            // 在子 agent 未执行完时就打勾（问题②）。子 agent 内部工具
            // （parentToolUseId 有值）与普通工具保持原 heuristic（不回归 Bug 3）。
            let status = if let Some(st) = upd.fields.status {
                status_str(Some(st))
            } else {
                let has_result = upd.fields.raw_output.is_some()
                    || upd.fields.content.as_ref().is_some_and(|c| !c.is_empty());
                if has_result {
                    let (parent, is_subagent) = claude_code_meta(upd.meta.as_ref());
                    if is_subagent && parent.is_none() {
                        "running"
                    } else {
                        "completed"
                    }
                } else {
                    "running"
                }
            };
            let mut frame = serde_json::json!({
                "type": "tool_result",
                "id": upd.tool_call_id.to_string(),
                "status": status,
            });
            if let Some(title) = &upd.fields.title {
                frame["name"] = serde_json::Value::String(title.clone());
            }
            if let Some(kind) = upd.fields.kind {
                frame["tool_kind"] = serde_json::Value::String(kind_str(kind).into());
            }
            // claude-code 的 ToolCall 首帧 rawInput 常是 {}（参数尚未到达），真正的
            // 命令/路径经后续 ToolCallUpdate.rawInput 才到达——映射为 args 让前端卡片
            // 实时补出操作内容（首帧 args 是 {} 占位时也要被本帧覆盖）。
            if let Some(input) = &upd.fields.raw_input {
                frame["args"] = serde_json::Value::String(encode_raw(input));
            }
            if let Some(output) = &upd.fields.raw_output {
                frame["result"] = serde_json::Value::String(encode_raw(output));
            }
            let mut diffs = upd
                .fields
                .content
                .as_deref()
                .map(extract_diffs)
                .unwrap_or_default();
            // Edit 兜底：diff 在 raw_input 的 old_string/new_string（同 ToolCall 路径）
            if diffs.is_empty() {
                if let Some(raw) = &upd.fields.raw_input {
                    diffs = extract_raw_edit_diff(raw);
                }
            }
            if !diffs.is_empty() {
                frame["diffs"] = serde_json::Value::Array(diffs);
            }
            if let Some(locations) = &upd.fields.locations {
                let locations = extract_locations(locations);
                if !locations.is_empty() {
                    frame["locations"] = serde_json::Value::Array(locations);
                }
            }
            apply_claude_code_meta(&mut frame, upd.meta.as_ref());
            Some(frame)
        }
        SessionUpdate::Plan(plan) => Some(serde_json::json!({
            "type": "plan",
            "entries": plan_entries_json(plan),
        })),
        SessionUpdate::SessionInfoUpdate(info) => {
            let MaybeUndefined::Value(title) = &info.title else {
                return None; // Undefined（未携带）与 Null（清除）都不产生标题帧
            };
            Some(serde_json::json!({"type": "session_title", "title": title}))
        }
        SessionUpdate::UsageUpdate(usage) => Some(serde_json::json!({
            "type": "usage",
            "used": usage.used,
            "size": usage.size,
        })),
        SessionUpdate::CurrentModeUpdate(mode) => Some(serde_json::json!({
            "type": "current_mode_update",
            "mode_id": mode.current_mode_id.to_string(),
        })),
        SessionUpdate::ConfigOptionUpdate(upd) => Some(serde_json::json!({
            "type": "config_option_update",
            "options": upd.config_options,
        })),
        SessionUpdate::AvailableCommandsUpdate(upd) => Some(map_available_commands_update(upd)),
        _ => None,
    }
}

/// 把 ACP 的 raw 输入/输出（`serde_json::Value`）编码成 WS 帧里的字符串。
///
/// 前端契约 `args?: string` / `result?: string`（`frontend/src/types/index.ts`，
/// 前端对 args 做 `JSON.parse`）。raw 值是 JSON 对象时序列化为 JSON 文本；
/// 本身已是字符串则直接透传，避免双重编码。
fn encode_raw(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// 文本 chunk（assistant 正文 / thought）→ assistant_chunk 帧；非文本块
/// （image/audio/resource/resource_link）→ attachment 占位帧，不再静默丢弃。
///
/// `chunk_meta` 是 chunk 级 `_meta`（`ContentChunk.meta`）；`TextContent` 内部还有
/// content 级 `_meta`（`TextContent.meta`）。子 agent 文本归属两级都查，优先 chunk 级。
fn map_text_chunk(
    content: &ContentBlock,
    chunk_meta: Option<&Meta>,
    thought: bool,
) -> Option<serde_json::Value> {
    let ContentBlock::Text(text) = content else {
        return map_attachment_chunk(content);
    };
    if text.text.is_empty() {
        return None;
    }
    let mut frame = serde_json::json!({"type": "assistant_chunk", "content": text.text});
    if thought {
        frame["thought"] = serde_json::Value::Bool(true);
    }
    let (parent, is_subagent) = claude_code_meta_two(chunk_meta, text.meta.as_ref());
    if let Some(parent) = parent {
        frame["parent_tool_call_id"] = serde_json::Value::String(parent);
    }
    if is_subagent {
        frame["is_subagent"] = serde_json::Value::Bool(true);
    }
    Some(frame)
}

/// 非文本内容块 → attachment 占位帧：
/// `{"type":"attachment","media_kind":"image|audio|resource","name","uri"?,"mime"?}`。
/// 正文数据（base64 等）不透传——占位卡只表达"这里有一个附件"，避免大 payload
/// 刷屏 WS 与控制通道。
fn map_attachment_chunk(content: &ContentBlock) -> Option<serde_json::Value> {
    use agent_client_protocol::schema::v1::EmbeddedResourceResource;
    let (media_kind, name, uri, mime) = match content {
        ContentBlock::Image(img) => (
            "image",
            // 图片无文件名，用 uri 末段或 mime 兜底
            img.uri
                .as_deref()
                .and_then(|u| u.rsplit('/').next().filter(|s| !s.is_empty()))
                .unwrap_or("image")
                .to_string(),
            img.uri.clone(),
            Some(img.mime_type.clone()),
        ),
        ContentBlock::Audio(audio) => (
            "audio",
            "audio".to_string(),
            None,
            Some(audio.mime_type.clone()),
        ),
        ContentBlock::ResourceLink(link) => (
            "resource",
            link.name.clone(),
            Some(link.uri.clone()),
            link.mime_type.clone(),
        ),
        ContentBlock::Resource(res) => match &res.resource {
            EmbeddedResourceResource::TextResourceContents(t) => (
                "resource",
                t.uri
                    .rsplit('/')
                    .next()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("resource")
                    .to_string(),
                Some(t.uri.clone()),
                t.mime_type.clone(),
            ),
            EmbeddedResourceResource::BlobResourceContents(b) => (
                "resource",
                b.uri
                    .rsplit('/')
                    .next()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("resource")
                    .to_string(),
                Some(b.uri.clone()),
                b.mime_type.clone(),
            ),
            // schema 标注 non_exhaustive：未来新增资源类型安全降级为无占位帧
            _ => return None,
        },
        ContentBlock::Text(_) | _ => return None,
    };
    let mut frame = serde_json::json!({
        "type": "attachment",
        "media_kind": media_kind,
        "name": name,
    });
    if let Some(u) = uri {
        frame["uri"] = serde_json::Value::String(u);
    }
    if let Some(m) = mime {
        frame["mime"] = serde_json::Value::String(m);
    }
    Some(frame)
}

/// claude-code-acp 的 opt-in 子 agent 约定（https://github.com/zed-industries/claude-code-acp）：
/// 事件 `_meta.claudeCode` 携带
/// - `parentToolUseId`（字符串）：发起本事件的 Task/Agent 工具调用的 toolCallId
/// - `subagent`（布尔）：本事件由子 agent 产生（Task 工具调用自身）
///
/// 返回 `(parent_tool_call_id, is_subagent)`；缺省时均为 `(None, false)`。
fn claude_code_meta(meta: Option<&Meta>) -> (Option<String>, bool) {
    let Some(meta) = meta else {
        return (None, false);
    };
    let claude_code = meta.get("claudeCode");
    let parent = claude_code
        .and_then(|v| v.get("parentToolUseId"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let is_subagent = claude_code
        .and_then(|v| v.get("subagent"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    (parent, is_subagent)
}

/// 两级 `_meta` 的 claudeCode 字段：primary（chunk 级）有任一字段时采用，否则
/// 回退 fallback（content 级）。真实事件里两级不会同时携带不同归属，先到先得即可。
fn claude_code_meta_two(primary: Option<&Meta>, fallback: Option<&Meta>) -> (Option<String>, bool) {
    let parsed = claude_code_meta(primary);
    if parsed.0.is_some() || parsed.1 {
        return parsed;
    }
    claude_code_meta(fallback)
}

/// 把 claude-code `_meta` 的父归属写入 WS 帧：`parent_tool_call_id` 有值才输出，
/// `is_subagent` 仅 true 时输出——缺省帧字段不出现，对不支持 `_meta` 的引擎无感降级。
fn apply_claude_code_meta(frame: &mut serde_json::Value, meta: Option<&Meta>) {
    let (parent, is_subagent) = claude_code_meta(meta);
    if let Some(parent) = parent {
        frame["parent_tool_call_id"] = serde_json::Value::String(parent);
    }
    if is_subagent {
        frame["is_subagent"] = serde_json::Value::Bool(true);
    }
}

/// ACP `ToolKind` → 帧字符串（前端按此选图标/详情渲染）。
fn kind_str(kind: ToolKind) -> &'static str {
    match kind {
        ToolKind::Read => "read",
        ToolKind::Edit => "edit",
        ToolKind::Delete => "delete",
        ToolKind::Move => "move",
        ToolKind::Search => "search",
        ToolKind::Execute => "execute",
        ToolKind::Think => "think",
        ToolKind::Fetch => "fetch",
        ToolKind::SwitchMode => "switch_mode",
        _ => "other", // Other + non_exhaustive 未来变体
    }
}

/// `content[].Diff` → 规范化 diff JSON：`{path, old_text, new_text}`。
fn extract_diffs(content: &[ToolCallContent]) -> Vec<serde_json::Value> {
    content
        .iter()
        .filter_map(|c| match c {
            ToolCallContent::Diff(d) => Some(serde_json::json!({
                "path": d.path.display().to_string(),
                "old_text": d.old_text,
                "new_text": d.new_text,
            })),
            _ => None,
        })
        .collect()
}

/// `locations[]` → 规范化 JSON：`{path, line?}`。
fn extract_locations(locations: &[ToolCallLocation]) -> Vec<serde_json::Value> {
    locations
        .iter()
        .map(|l| serde_json::json!({"path": l.path.display().to_string(), "line": l.line}))
        .collect()
}

/// claude-code Edit 形态兜底：raw_input 含 `file_path`+`old_string`+`new_string`
/// 时合成单条 diff（old/new 是补丁片段而非完整文件，前端按上下文片段渲染）。
fn extract_raw_edit_diff(raw: &serde_json::Value) -> Vec<serde_json::Value> {
    let Some(obj) = raw.as_object() else {
        return Vec::new();
    };
    let (Some(path), Some(old), Some(new)) = (
        obj.get("file_path").and_then(|v| v.as_str()),
        obj.get("old_string").and_then(|v| v.as_str()),
        obj.get("new_string").and_then(|v| v.as_str()),
    ) else {
        return Vec::new();
    };
    vec![serde_json::json!({"path": path, "old_text": old, "new_text": new})]
}

/// ACP Plan → 前端条目 JSON（含 priority 透传）。
///
/// 状态/优先级字符串通过 serde 序列化取 snake_case 名（`InProgress` → `in_progress`、
/// `High` → `high`），与前端既有 plan 渲染保持一致。
fn plan_entries_json(plan: &Plan) -> Vec<serde_json::Value> {
    plan.entries
        .iter()
        .map(|e| {
            let status = serde_json::to_value(&e.status)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default();
            let priority = serde_json::to_value(&e.priority)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default();
            serde_json::json!({
                "content": e.content,
                "priority": priority,
                "status": status,
            })
        })
        .collect()
}

/// 把 ACP AvailableCommandsUpdate 映射为 WS 帧：`{"type":"available_commands","commands":[...]}`。
///
/// 命令列表在 `SpawnedAgent` 中缓存，新 WS 连接建立时补发一次（后加入的标签页也能
/// 拿到）。
fn map_available_commands_update(upd: &AvailableCommandsUpdate) -> serde_json::Value {
    let commands: Vec<serde_json::Value> = upd
        .available_commands
        .iter()
        .map(|cmd| {
            serde_json::json!({
                "name": cmd.name,
                "description": cmd.description,
            })
        })
        .collect();
    serde_json::json!({
        "type": "available_commands",
        "commands": commands,
    })
}

/// ACP `ToolCallStatus` → WS 帧里的字符串状态。
/// Pending/InProgress 对前端都是"运行中"。
fn status_str(status: Option<ToolCallStatus>) -> &'static str {
    match status {
        Some(ToolCallStatus::Completed) => "completed",
        Some(ToolCallStatus::Failed) => "failed",
        _ => "running",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 从 JSON fixture 反序列化一个 SessionUpdate（ACP crate 类型均可
    /// serde 反序列化，字段名以 crate 的 serde 注解为准）。
    fn update(v: serde_json::Value) -> SessionUpdate {
        serde_json::from_value(v).expect("fixture should deserialize")
    }

    #[test]
    fn test_map_agent_message_chunk() {
        let u = update(serde_json::json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "hello"}
        }));
        let frame = map_update(&u).expect("text chunk should map");
        assert_eq!(frame["type"], "assistant_chunk");
        assert_eq!(frame["content"], "hello");
    }

    #[test]
    fn test_map_empty_text_chunk_returns_none() {
        let u = update(serde_json::json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": ""}
        }));
        assert!(map_update(&u).is_none(), "empty text should be dropped");
    }

    #[test]
    fn test_map_tool_call() {
        let u = update(serde_json::json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call_1",
            "title": "shell",
            "status": "in_progress",
            "rawInput": {"cmd": "ls"}
        }));
        let frame = map_update(&u).expect("tool_call should map");
        assert_eq!(frame["type"], "tool_call");
        assert_eq!(frame["id"], "call_1");
        assert_eq!(frame["name"], "shell");
        assert_eq!(frame["status"], "running");
        // raw 值是 JSON 对象 → args 序列化为 JSON 文本字符串（前端 JSON.parse）
        assert_eq!(frame["args"], "{\"cmd\":\"ls\"}");
    }

    #[test]
    fn test_map_tool_call_string_args_passthrough() {
        // raw 值已是字符串（如 shell 命令）→ 直接透传，不双重编码
        let u = update(serde_json::json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call_1",
            "title": "shell",
            "status": "completed",
            "rawInput": "ls -la"
        }));
        let frame = map_update(&u).expect("tool_call should map");
        assert_eq!(frame["args"], "ls -la");
    }

    #[test]
    fn test_map_tool_call_update_without_status_is_completed() {
        // ACP 的 ToolCallUpdate 常精简（claude-code 常省略 status 字段，只带
        // raw_output）。status 缺失但结果已产出的工具应视为 completed，
        // 不能默认映射为 running——否则前端卡片永远转圈（Bug 3）。
        let u = update(serde_json::json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call_1",
            "rawOutput": "a.rs"
        }));
        let frame = map_update(&u).expect("tool_call_update should map");
        assert_eq!(frame["type"], "tool_result");
        assert_eq!(frame["status"], "completed");
        assert_eq!(frame["result"], "a.rs");
    }

    #[test]
    fn test_map_tool_call_update_subagent_parent_without_status_is_running() {
        // 问题②回归：subagent 父卡（is_subagent=true、无 parentToolUseId，即 Task
        // 工具调用自身的进度快照）的 ToolCallUpdate 常省略 status 但带部分输出
        // （子 agent 中间结果）——has_result ≠ 执行完成，必须映射 running，
        // 否则前端在子 agent 未执行完时就打勾。
        let u = update(serde_json::json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "task_1",
            "rawOutput": "子 agent 的部分输出",
            "_meta": {"claudeCode": {"subagent": true}}
        }));
        let frame = map_update(&u).expect("tool_call_update should map");
        assert_eq!(frame["type"], "tool_result");
        assert_eq!(frame["status"], "running");
        assert_eq!(frame["result"], "子 agent 的部分输出");
    }

    #[test]
    fn test_map_tool_call_update_subagent_child_without_status_is_completed() {
        // 子 agent 内部工具（parentToolUseId 有值）保持 has_result→completed，
        // 不回归 Bug 3（已完成工具永远转圈）。
        let u = update(serde_json::json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "sub_tool_1",
            "rawOutput": "a.rs",
            "_meta": {"claudeCode": {"parentToolUseId": "task_1", "subagent": true}}
        }));
        let frame = map_update(&u).expect("tool_call_update should map");
        assert_eq!(frame["type"], "tool_result");
        assert_eq!(frame["status"], "completed");
        assert_eq!(frame["result"], "a.rs");
    }

    #[test]
    fn test_map_tool_call_update_completed() {
        let u = update(serde_json::json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call_1",
            "status": "completed",
            "rawOutput": "a.rs"
        }));
        let frame = map_update(&u).expect("tool_call_update should map");
        assert_eq!(frame["type"], "tool_result");
        assert_eq!(frame["id"], "call_1");
        assert_eq!(frame["status"], "completed");
        // raw 值是字符串 → result 直接透传
        assert_eq!(frame["result"], "a.rs");
    }

    #[test]
    fn test_map_tool_call_update_object_result_serialized() {
        // raw 值是 JSON 对象（如文件内容结构化输出）→ result 序列化为 JSON 文本
        let u = update(serde_json::json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call_1",
            "status": "completed",
            "rawOutput": {"path": "/tmp/a.rs", "size": 42}
        }));
        let frame = map_update(&u).expect("tool_call_update should map");
        assert_eq!(frame["result"], "{\"path\":\"/tmp/a.rs\",\"size\":42}");
    }

    #[test]
    fn test_map_tool_call_update_failed() {
        let u = update(serde_json::json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call_2",
            "status": "failed"
        }));
        let frame = map_update(&u).expect("failed update should map");
        assert_eq!(frame["status"], "failed");
    }

    #[test]
    fn test_map_current_mode_update() {
        let u = update(serde_json::json!({
            "sessionUpdate": "current_mode_update",
            "currentModeId": "plan"
        }));
        let frame = map_update(&u).expect("current_mode_update should map");
        assert_eq!(frame["type"], "current_mode_update");
        assert_eq!(frame["mode_id"], "plan");
    }

    #[test]
    fn test_map_config_option_update() {
        let u = update(serde_json::json!({
            "sessionUpdate": "config_option_update",
            "configOptions": [{
                "id": "mode",
                "name": "Mode",
                "category": "mode",
                "type": "select",
                "currentValue": "plan",
                "options": [
                    {"value": "default", "name": "Default"},
                    {"value": "plan", "name": "Plan"}
                ]
            }]
        }));
        let frame = map_update(&u).expect("config_option_update should map");
        assert_eq!(frame["type"], "config_option_update");
        assert_eq!(frame["options"][0]["id"], "mode");
        assert_eq!(frame["options"][0]["currentValue"], "plan");
        assert_eq!(frame["options"][0]["options"][1]["value"], "plan");
    }

    #[test]
    fn test_map_irrelevant_update_returns_none() {
        // user_message_chunk 仍属无需推送的更新（前端自渲染用户消息）
        let u = update(serde_json::json!({
            "sessionUpdate": "user_message_chunk",
            "content": {"type": "text", "text": "hi"}
        }));
        assert!(map_update(&u).is_none());
    }

    #[test]
    fn test_status_str() {
        assert_eq!(status_str(Some(ToolCallStatus::Completed)), "completed");
        assert_eq!(status_str(Some(ToolCallStatus::Failed)), "failed");
        assert_eq!(status_str(Some(ToolCallStatus::Pending)), "running");
        assert_eq!(status_str(Some(ToolCallStatus::InProgress)), "running");
        assert_eq!(status_str(None), "running");
    }

    #[test]
    fn test_map_tool_call_with_kind_diffs_locations() {
        let u = update(serde_json::json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call_1",
            "title": "Edit src/main.rs",
            "kind": "edit",
            "status": "in_progress",
            "content": [
                {"type": "diff", "path": "/w/src/main.rs",
                 "oldText": "fn main() {}", "newText": "fn main() { run(); }"},
                {"type": "content", "content": {"type": "text", "text": "忽略我"}}
            ],
            "locations": [{"path": "/w/src/main.rs", "line": 3}],
            "rawInput": {"file_path": "src/main.rs"}
        }));
        let frame = map_update(&u).expect("tool_call should map");
        assert_eq!(frame["tool_kind"], "edit");
        assert_eq!(
            frame["diffs"],
            serde_json::json!([{"path": "/w/src/main.rs",
                "old_text": "fn main() {}", "new_text": "fn main() { run(); }"}])
        );
        assert_eq!(
            frame["locations"],
            serde_json::json!([{"path": "/w/src/main.rs", "line": 3}])
        );
    }

    #[test]
    fn test_map_tool_call_diff_from_raw_input() {
        // claude-code Edit 形态：无 content，diff 在 raw_input 的 old_string/new_string
        let u = update(serde_json::json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call_2",
            "title": "Edit",
            "kind": "edit",
            "rawInput": {"file_path": "src/lib.rs", "old_string": "a", "new_string": "b"}
        }));
        let frame = map_update(&u).expect("tool_call should map");
        assert_eq!(
            frame["diffs"],
            serde_json::json!([{"path": "src/lib.rs", "old_text": "a", "new_text": "b"}])
        );
    }

    #[test]
    fn test_map_tool_call_kind_defaults_other() {
        let u = update(serde_json::json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call_3",
            "title": "mystery"
        }));
        let frame = map_update(&u).expect("tool_call should map");
        assert_eq!(frame["tool_kind"], "other");
        assert!(frame.get("diffs").is_none());
        assert!(frame.get("locations").is_none());
    }

    #[test]
    fn test_map_tool_call_update_with_kind_and_content_diff() {
        let u = update(serde_json::json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call_4",
            "status": "completed",
            "kind": "edit",
            "content": [{"type": "diff", "path": "/w/a.rs", "newText": "new"}],
            "rawOutput": "ok"
        }));
        let frame = map_update(&u).expect("tool_call_update should map");
        assert_eq!(frame["tool_kind"], "edit");
        assert_eq!(
            frame["diffs"],
            serde_json::json!([{"path": "/w/a.rs", "old_text": null, "new_text": "new"}])
        );
    }

    #[test]
    fn test_map_tool_call_update_carries_raw_input_as_args() {
        // 实测 claude-code-acp 形态（0.66.0）：ToolCall 首帧 rawInput={}、title 是
        // 占位词（"Terminal"），真正的参数经 ToolCallUpdate.rawInput 才到达。
        // update 帧必须携带 args，前端卡片才能实时补出操作内容。
        let u = update(serde_json::json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call_5",
            "rawInput": {"command": "echo hello", "description": "Print hello"},
            "title": "echo hello",
            "kind": "execute"
        }));
        let frame = map_update(&u).expect("tool_call_update should map");
        assert_eq!(frame["type"], "tool_result");
        assert_eq!(
            frame["args"],
            "{\"command\":\"echo hello\",\"description\":\"Print hello\"}"
        );
        assert_eq!(frame["name"], "echo hello");
        // 无 status/raw_output/非空 content → 仍是 running（中间状态，不误判 completed）
        assert_eq!(frame["status"], "running");
    }

    #[test]
    fn test_map_tool_call_update_diff_from_raw_input() {
        // claude-code Edit 形态：update 无 content，diff 在 rawInput 的 old_string/new_string
        let u = update(serde_json::json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call_6",
            "rawInput": {"file_path": "src/lib.rs", "old_string": "a", "new_string": "b"}
        }));
        let frame = map_update(&u).expect("tool_call_update should map");
        assert_eq!(
            frame["diffs"],
            serde_json::json!([{"path": "src/lib.rs", "old_text": "a", "new_text": "b"}])
        );
    }

    #[test]
    fn test_map_thought_chunk() {
        let u = update(serde_json::json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": {"type": "text", "text": "想想…"}
        }));
        let frame = map_update(&u).expect("thought chunk should map");
        assert_eq!(frame["type"], "assistant_chunk");
        assert_eq!(frame["content"], "想想…");
        assert_eq!(frame["thought"], true);
    }

    #[test]
    fn test_map_user_message_chunk_ignored() {
        let u = update(serde_json::json!({
            "sessionUpdate": "user_message_chunk",
            "content": {"type": "text", "text": "用户原文"}
        }));
        assert!(map_update(&u).is_none());
    }

    #[test]
    fn test_map_plan() {
        let u = update(serde_json::json!({
            "sessionUpdate": "plan",
            "entries": [
                {"content": "读代码", "priority": "high", "status": "completed"},
                {"content": "改实现", "priority": "medium", "status": "in_progress"},
                {"content": "跑测试", "priority": "low", "status": "pending"}
            ]
        }));
        let frame = map_update(&u).expect("plan should map");
        assert_eq!(frame["type"], "plan");
        assert_eq!(
            frame["entries"],
            serde_json::json!([
                {"content": "读代码", "priority": "high", "status": "completed"},
                {"content": "改实现", "priority": "medium", "status": "in_progress"},
                {"content": "跑测试", "priority": "low", "status": "pending"}
            ])
        );
    }

    #[test]
    fn test_map_available_commands_update() {
        let u = update(serde_json::json!({
            "sessionUpdate": "available_commands_update",
            "availableCommands": [
                {"name": "create_plan", "description": "Create a plan"},
                {"name": "research_codebase", "description": ""}
            ]
        }));
        let frame = map_update(&u).expect("available_commands_update should map");
        assert_eq!(frame["type"], "available_commands");
        assert_eq!(frame["commands"][0]["name"], "create_plan");
        assert_eq!(frame["commands"][0]["description"], "Create a plan");
        assert_eq!(frame["commands"][1]["name"], "research_codebase");
        assert_eq!(frame["commands"][1]["description"], "");
    }

    #[test]
    fn test_map_available_commands_update_empty_list() {
        let u = update(serde_json::json!({
            "sessionUpdate": "available_commands_update",
            "availableCommands": []
        }));
        let frame = map_update(&u).expect("empty available_commands_update should map");
        assert_eq!(frame["type"], "available_commands");
        assert_eq!(frame["commands"], serde_json::json!([]));
    }

    #[test]
    fn test_map_session_info_title() {
        let u = update(serde_json::json!({
            "sessionUpdate": "session_info_update",
            "title": "修复登录 bug"
        }));
        let frame = map_update(&u).expect("session info should map");
        assert_eq!(frame["type"], "session_title");
        assert_eq!(frame["title"], "修复登录 bug");
    }

    #[test]
    fn test_map_usage_update() {
        let u = update(serde_json::json!({
            "sessionUpdate": "usage_update",
            "used": 1234,
            "size": 200_000
        }));
        let frame = map_update(&u).expect("usage should map");
        assert_eq!(frame["type"], "usage");
        assert_eq!(frame["used"], 1234);
        assert_eq!(frame["size"], 200_000);
    }

    #[test]
    fn test_map_image_chunk_to_attachment_frame() {
        let u = update(serde_json::json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {
                "type": "image",
                "data": "iVBORw0KGgo=",
                "mimeType": "image/png",
                "uri": "https://example.com/pic.png"
            }
        }));
        let frame = map_update(&u).expect("image chunk 应映射为 attachment 占位帧");
        assert_eq!(frame["type"], "attachment");
        assert_eq!(frame["media_kind"], "image");
        assert_eq!(frame["uri"], "https://example.com/pic.png");
        assert_eq!(frame["mime"], "image/png");
    }

    #[test]
    fn test_map_resource_link_chunk_to_attachment_frame() {
        let u = update(serde_json::json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": {
                "type": "resource_link",
                "name": "readme.md",
                "uri": "file:///home/x/readme.md"
            }
        }));
        let frame = map_update(&u).expect("resource_link chunk 应映射为 attachment 占位帧");
        assert_eq!(frame["type"], "attachment");
        assert_eq!(frame["media_kind"], "resource");
        assert_eq!(frame["name"], "readme.md");
        assert_eq!(frame["uri"], "file:///home/x/readme.md");
    }

    // ── claude-code subagent-transcript `_meta` 透传 ──────────────

    fn claude_code_meta(parent: &str) -> serde_json::Value {
        serde_json::json!({
            "claudeCode": {
                "parentToolUseId": parent,
                "subagent": true,
            }
        })
    }

    #[test]
    fn test_map_tool_call_carries_subagent_meta() {
        // Task/Agent 工具调用自身：is_subagent=true，无 parentToolUseId
        let u = update(serde_json::json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "task_1",
            "title": "Agent 子任务",
            "status": "in_progress",
            "rawInput": {"prompt": "查一下"},
            "_meta": {"claudeCode": {"subagent": true}}
        }));
        let frame = map_update(&u).expect("tool_call should map");
        assert_eq!(frame["type"], "tool_call");
        assert_eq!(frame["is_subagent"], true);
        assert!(
            frame.get("parent_tool_call_id").is_none(),
            "Task 调用自身无父归属"
        );
    }

    #[test]
    fn test_map_tool_call_carries_parent_meta() {
        // 子 agent 内的工具调用：parentToolUseId = 发起它的 Task 的 toolCallId
        let u = update(serde_json::json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "sub_tool_1",
            "title": "shell",
            "status": "in_progress",
            "rawInput": {"cmd": "ls"},
            "_meta": claude_code_meta("task_1")
        }));
        let frame = map_update(&u).expect("tool_call should map");
        assert_eq!(frame["type"], "tool_call");
        assert_eq!(frame["parent_tool_call_id"], "task_1");
        assert_eq!(frame["is_subagent"], true);
        // 原有字段不受影响
        assert_eq!(frame["id"], "sub_tool_1");
        assert_eq!(frame["name"], "shell");
    }

    #[test]
    fn test_map_tool_call_without_meta_has_no_parent_fields() {
        // 向后兼容：无 `_meta` 的事件帧不出现新增字段
        let u = update(serde_json::json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call_1",
            "title": "shell",
            "status": "completed"
        }));
        let frame = map_update(&u).expect("tool_call should map");
        assert!(frame.get("parent_tool_call_id").is_none());
        assert!(frame.get("is_subagent").is_none());
    }

    #[test]
    fn test_map_tool_call_update_carries_parent_meta() {
        let u = update(serde_json::json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "sub_tool_1",
            "status": "completed",
            "rawOutput": "a.rs",
            "_meta": claude_code_meta("task_1")
        }));
        let frame = map_update(&u).expect("tool_call_update should map");
        assert_eq!(frame["type"], "tool_result");
        assert_eq!(frame["parent_tool_call_id"], "task_1");
        assert_eq!(frame["is_subagent"], true);
        assert_eq!(frame["result"], "a.rs");
    }

    #[test]
    fn test_map_text_chunk_carries_parent_meta_chunk_level() {
        // chunk 级 `_meta`（ContentChunk.meta）：子 agent 文本归属
        let u = update(serde_json::json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "子 agent 的回复"},
            "_meta": claude_code_meta("task_1")
        }));
        let frame = map_update(&u).expect("text chunk should map");
        assert_eq!(frame["type"], "assistant_chunk");
        assert_eq!(frame["content"], "子 agent 的回复");
        assert_eq!(frame["parent_tool_call_id"], "task_1");
    }

    #[test]
    fn test_map_text_chunk_carries_parent_meta_content_level() {
        // content 级 `_meta`（TextContent.meta）兜底：chunk 级缺失时归属仍透传
        let u = update(serde_json::json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": {
                "type": "text",
                "text": "子 agent 的思考",
                "_meta": {"claudeCode": {"parentToolUseId": "task_1"}}
            }
        }));
        let frame = map_update(&u).expect("thought chunk should map");
        assert_eq!(frame["type"], "assistant_chunk");
        assert_eq!(frame["thought"], true);
        assert_eq!(frame["parent_tool_call_id"], "task_1");
    }

    #[test]
    fn test_map_text_chunk_without_meta_has_no_parent_fields() {
        let u = update(serde_json::json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "主 agent 文本"}
        }));
        let frame = map_update(&u).expect("text chunk should map");
        assert!(frame.get("parent_tool_call_id").is_none());
        assert!(frame.get("is_subagent").is_none());
    }
}
