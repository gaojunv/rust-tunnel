//! 兼容模式：把工具调用历史改写成纯文本，适配不支持多轮 tool calling 的上游。
//!
//! 背景：部分 OpenAI 兼容网关（如 opencode Console Go）只支持单轮工具调用，
//! 一旦请求历史里出现 `assistant.tool_calls` 或 `role="tool"` 消息就返回 400。
//! 本模块在发送前把这两类结构降级为文本。
//!
//! # 协议
//!
//! ## v2（当前）
//!
//! 消息改写采用 XML 风格标签，对模型推理分布干扰更小：
//!
//! - 工具调用：`<tool_call>\n{"name":"...","arguments":{...}}\n</tool_call>`
//! - 工具结果：`<tool_result name="...">\n...\n</tool_result>`
//!
//! 标签体内的 JSON 使用与原生 `tool_calls` 相同的 schema，避免信息丢失。
//!
//! ## 解析失败策略
//!
//! `TagScanner` 增量解析器检测到未闭合标签或非法 JSON 后产出 `Discarded` 事件，
//! 该片段**不回退为普通文本**——标记已明确表明模型意图调用工具，结构错误应
//! 被安全剥离而非泄露给客户端。`log_discard()` 以 `tracing::warn!` 记录片段
//! 前 100 字符用于诊断。
//!
//! ## 旧格式兼容
//!
//! `[调用工具 name] {args}` 与 `[工具结果 name]` 格式仍被 `TagScanner` 识别，
//! 用于历史会话回环兼容。新输出不再产生此格式。

use serde_json::{json, Value};

use super::ChatMessage;

/// 判断 provider 的 extra_config 是否开启工具历史兼容模式。
/// extra_config 是 JSON 字符串，含 `"compat_tool_history": true` 即开启。
#[must_use]
pub fn compat_tool_history_enabled(extra_config: Option<&str>) -> bool {
    extra_config
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .and_then(|v| v.get("compat_tool_history").cloned())
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

// ── v2 协议常量 ───────────────────────────────────────────────

/// 伪工具调用标签（模型训练分布内的 XML 风格，模仿成功率高）。
const TOOL_CALL_OPEN: &str = "<tool_call>";
const TOOL_CALL_CLOSE: &str = "</tool_call>";
const RESULT_OPEN_PREFIX: &str = "<tool_result name=\"";
const RESULT_OPEN_CLOSE: &str = "\">";
const RESULT_CLOSE: &str = "</tool_result>";
/// 旧协议标记（历史会话回环兼容）。
const LEGACY_CALL_OPEN: &str = "[调用工具 ";
const LEGACY_RESULT_OPEN: &str = "[工具结果 ";

/// compat 模式开启时注入的 system 引导（messages 末尾）。
pub const TOOL_CALL_GUIDANCE: &str = "你可以通过两种方式调用工具：使用原生 tool_calls 功能，\
或在回复中输出 <tool_call>\n{\"name\": \"工具名\", \"arguments\": {...}}\n</tool_call> \
格式的文本。两种方式都会被正确执行。可用工具见 tools 声明。";

/// 在 messages 末尾注入伪工具调用引导（幂等：末尾已是引导则跳过）。
pub fn inject_tool_call_guidance(messages: &mut Vec<ChatMessage>) {
    if let Some(last) = messages.last() {
        if last.role == "system" && last.content.as_deref() == Some(TOOL_CALL_GUIDANCE) {
            return;
        }
    }
    messages.push(ChatMessage::text("system", TOOL_CALL_GUIDANCE));
}

/// 把消息列表里的工具调用结构改写为纯文本（原位替换）。
/// 返回是否有改写发生（用于诊断日志）。
pub fn rewrite_tool_history(messages: &mut [ChatMessage]) -> bool {
    let mut rewritten = false;

    // 第一遍：收集 tool_call_id → 函数名映射，用于给 tool 结果消息标注来源工具。
    let mut id_to_name: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for m in messages.iter() {
        if let Some(calls) = &m.tool_calls {
            for c in calls {
                let id = c.get("id").and_then(Value::as_str).unwrap_or("");
                let name = c
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if !id.is_empty() {
                    id_to_name.insert(id.to_string(), name.to_string());
                }
            }
        }
    }

    for m in messages.iter_mut() {
        // ── assistant.tool_calls → 标签文本 ──
        if let Some(calls) = m.tool_calls.take() {
            rewritten = true;
            let mut parts: Vec<String> = Vec::new();
            if let Some(text) = &m.content {
                if !text.is_empty() {
                    parts.push(text.clone());
                }
            }
            for c in &calls {
                let name = c
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let args = c
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(Value::as_str)
                    .unwrap_or("{}");
                parts.push(format!(
                    "{TOOL_CALL_OPEN}\n{}\n{TOOL_CALL_CLOSE}",
                    json!({"name": name, "arguments": serde_json::from_str::<Value>(args).unwrap_or_else(|_| json!({}))})
                ));
            }
            m.content = Some(parts.join("\n"));
        }

        // ── role="tool" → user 标签文本 ──
        if m.role == "tool" {
            rewritten = true;
            let tool_name = m
                .tool_call_id
                .as_deref()
                .and_then(|id| id_to_name.get(id))
                .cloned()
                .unwrap_or_else(|| "unknown".to_string());
            let content = m.content.take().unwrap_or_default();
            m.role = "user".to_string();
            m.tool_call_id = None;
            m.content = Some(format!(
                "{RESULT_OPEN_PREFIX}{tool_name}{RESULT_OPEN_CLOSE}\n{content}\n{RESULT_CLOSE}"
            ));
        }
    }

    rewritten
}

// ── v2 增量解析器 ─────────────────────────────────────────────

/// 解析器产出的增量事件。
#[derive(Debug)]
pub enum ScanEvent {
    /// 可即时透传的普通文本
    Text(String),
    /// 完整解析出的 OpenAI tool_call 对象
    ToolCall(Value),
    /// 检测到标记但解析失败被剥离的片段（仅用于调用方记日志/断言，绝不输出给客户端）
    Discarded(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanState {
    Text,
    InToolCall,
    InToolResult,
    InLegacyCall,
    InLegacyResult,
}

/// 伪工具调用增量解析器。
///
/// 逐段喂入文本，即时产出 Text 事件；检测到 `<tool_call>` 或旧格式
/// `[调用工具 ` 标记后进入累积态，闭合标签 + JSON 合法才产出 ToolCall；
/// 解析失败产出 Discarded（剥离，不回退为文本）。
pub struct TagScanner {
    state: ScanState,
    /// Text 态：确认安全的文本缓冲；标记态：标记之后的累积内容
    buf: String,
    calls: Vec<Value>,
    idx: u32,
}

/// 所有标记前缀（前缀互不相含，按长度降序尝试即可）。
const MARKERS: &[(&str, ScanState)] = &[
    (TOOL_CALL_OPEN, ScanState::InToolCall),
    (RESULT_OPEN_PREFIX, ScanState::InToolResult),
    (LEGACY_CALL_OPEN, ScanState::InLegacyCall),
    (LEGACY_RESULT_OPEN, ScanState::InLegacyResult),
];

/// 最长标记前缀长度（`<tool_result name="` = 18 字节）。
const MAX_MARKER_LEN: usize = 18;

impl Default for TagScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl TagScanner {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: ScanState::Text,
            buf: String::new(),
            calls: Vec::new(),
            idx: 0,
        }
    }

    #[must_use]
    pub fn has_tool_calls(&self) -> bool {
        !self.calls.is_empty()
    }

    /// 取走已解析的 tool_calls（与 finish 互斥：finish 消耗 self）。
    pub fn take_tool_calls(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.calls)
    }

    /// 喂入一段文本，产出本段解析出的事件。
    pub fn push(&mut self, text: &str) -> Vec<ScanEvent> {
        let mut events = Vec::new();
        self.buf.push_str(text);
        loop {
            match self.state {
                ScanState::Text => {
                    let (emit, next) = self.scan_text();
                    if let Some(t) = emit {
                        events.push(ScanEvent::Text(t));
                    }
                    match next {
                        Some(st) => self.state = st,
                        None => break, // 剩余是悬挂前缀，等更多输入
                    }
                }
                ScanState::InToolCall => {
                    let Some(pos) = self.buf.find(TOOL_CALL_CLOSE) else {
                        break;
                    };
                    let body = self.buf[..pos].to_string();
                    self.buf.drain(..pos + TOOL_CALL_CLOSE.len());
                    self.state = ScanState::Text;
                    self.emit_call(&body, &mut events);
                }
                ScanState::InToolResult => {
                    let Some(pos) = self.buf.find(RESULT_CLOSE) else {
                        break;
                    };
                    self.buf.drain(..pos + RESULT_CLOSE.len());
                    self.state = ScanState::Text;
                    // 工具结果标签内容（模型正常不会输出）静默剥离
                }
                ScanState::InLegacyCall => match scan_legacy_call(&self.buf) {
                    LegacyScan::Incomplete => break,
                    LegacyScan::Done {
                        name,
                        args,
                        consumed,
                    } => {
                        self.buf.drain(..consumed);
                        self.state = ScanState::Text;
                        self.push_call(&name, &args, &mut events);
                    }
                    LegacyScan::Invalid { consumed } => {
                        let frag = self.buf[..consumed].to_string();
                        self.buf.drain(..consumed);
                        self.state = ScanState::Text;
                        log_discard("legacy call parse failed", &frag);
                        events.push(ScanEvent::Discarded(frag));
                    }
                },
                ScanState::InLegacyResult => {
                    let Some(pos) = self.buf.find(']') else { break };
                    self.buf.drain(..=pos);
                    self.state = ScanState::Text;
                    // 同 InToolResult：剥离
                }
            }
        }
        events
    }

    /// 流结束：把残留缓冲清算为最终事件。
    #[must_use]
    pub fn finish(mut self) -> Vec<ScanEvent> {
        let mut events = Vec::new();
        if self.buf.is_empty() {
            return events;
        }
        match self.state {
            ScanState::Text => {
                // 悬挂前缀属于普通文本，原样放行
                events.push(ScanEvent::Text(std::mem::take(&mut self.buf)));
            }
            ScanState::InToolCall | ScanState::InLegacyCall => {
                let frag = std::mem::take(&mut self.buf);
                log_discard("unclosed tool call at stream end", &frag);
                events.push(ScanEvent::Discarded(frag));
            }
            ScanState::InToolResult | ScanState::InLegacyResult => {
                self.buf.clear(); // 静默剥离
            }
        }
        events
    }

    /// Text 态扫描：返回 (可放行文本, 下一个状态)。
    /// None 表示剩余缓冲是悬挂前缀，等待更多输入。
    fn scan_text(&mut self) -> (Option<String>, Option<ScanState>) {
        // 找最早的完整标记
        let mut best: Option<(usize, &str, ScanState)> = None;
        for (marker, st) in MARKERS {
            if let Some(pos) = self.buf.find(marker) {
                if best.as_ref().is_none_or(|(bp, _, _)| pos < *bp) {
                    best = Some((pos, marker, *st));
                }
            }
        }
        if let Some((pos, marker, st)) = best {
            let emit = self.buf[..pos].to_string();
            self.buf.drain(..pos + marker.len());
            if emit.is_empty() {
                return (None, Some(st));
            }
            // emit 位于完整标记之前，find 已保证其中无完整标记；
            // 若有半截前缀贴着完整标记，属下一圈循环的事，直接放行。
            return (Some(emit), Some(st));
        }

        // 无完整标记：保留末尾可能是前缀的尾巴（按字符边界截断）
        let keep = dangling_prefix_len(&self.buf);
        let split_at = self.buf.len() - keep;
        if split_at == 0 {
            return (None, None); // 整段都是悬挂前缀
        }
        let emit: String = self.buf.drain(..split_at).collect();
        (Some(emit), None)
    }

    /// 解析 <tool_call> 体内 JSON 并产出事件。
    fn emit_call(&mut self, body: &str, events: &mut Vec<ScanEvent>) {
        if let Some((name, args)) = parse_tool_call_body(body) {
            self.push_call(&name, &args, events)
        } else {
            log_discard("tool_call JSON parse failed", body);
            events.push(ScanEvent::Discarded(body.to_string()));
        }
    }

    fn push_call(&mut self, name: &str, arguments: &str, events: &mut Vec<ScanEvent>) {
        self.idx += 1;
        let call = json!({
            "id": format!("call_{}", self.idx),
            "type": "function",
            "function": {
                "name": name.trim(),
                "arguments": arguments,
            },
        });
        self.calls.push(call.clone());
        events.push(ScanEvent::ToolCall(call));
    }
}

/// 计算 buf 末尾与某个标记前缀匹配的长度（悬挂前缀）。
/// 首字符豁免：若 buf 只剩 1 个字符且恰为 '<' 或 '['，不视为悬挂（直接放行），
/// 避免单字符普通文本被无限扣留。
fn dangling_prefix_len(buf: &str) -> usize {
    let mut best = 0;
    for (marker, _) in MARKERS {
        let mlen = marker.len().min(MAX_MARKER_LEN);
        // Walk backwards from the end of buf, only at char boundaries.
        let mut byte_pos = buf.len();
        for ch in buf.chars().rev() {
            let ch_len = ch.len_utf8();
            byte_pos = byte_pos.saturating_sub(ch_len);
            let k = buf.len() - byte_pos;
            if k > mlen {
                break;
            }
            if marker.starts_with(&buf[byte_pos..]) {
                best = best.max(k);
            }
        }
    }
    if buf.len() == 1 && best == 1 {
        return 0; // 单字符 '<' / '[' 直接放行
    }
    best
}

/// 解析 <tool_call> 体：{"name": "...", "arguments": {...}}，容忍代码围栏与空白。
/// 返回 (name, arguments 的 JSON 字符串)。
fn parse_tool_call_body(body: &str) -> Option<(String, String)> {
    let mut b = body.trim();
    // 容忍 ```json / ``` 围栏
    if let Some(rest) = b.strip_prefix("```json") {
        b = rest;
    } else if let Some(rest) = b.strip_prefix("```") {
        b = rest;
    }
    if let Some(rest) = b.strip_suffix("```") {
        b = rest;
    }
    let b = b.trim();
    let v: Value = serde_json::from_str(b).ok()?;
    let name = v.get("name")?.as_str()?.to_string();
    let args = match v.get("arguments") {
        Some(Value::String(s)) => s.clone(),
        Some(other) => serde_json::to_string(other).ok()?,
        None => "{}".to_string(),
    };
    Some((name, args))
}

/// 旧格式 `[调用工具 name] {args}` 的增量扫描结果。
enum LegacyScan {
    Incomplete,
    Done {
        name: String,
        args: String,
        consumed: usize,
    },
    Invalid {
        consumed: usize,
    },
}

/// 在 buf 开头尝试解析旧格式调用（buf 已位于 "[调用工具 " 之后）。
fn scan_legacy_call(buf: &str) -> LegacyScan {
    // name 到 ']' 为止
    let Some(bracket) = buf.find(']') else {
        return LegacyScan::Incomplete;
    };
    let name = buf[..bracket].trim().to_string();
    let after = &buf[bracket + 1..];
    let skip = after.len() - after.trim_start().len();
    let rest = &after[skip..];
    if rest.is_empty() {
        return LegacyScan::Incomplete;
    }
    if !rest.starts_with('{') {
        if rest.starts_with('\n') || rest.len() < 2 {
            return LegacyScan::Incomplete; // 参数可能还在路上
        }
        // 明确不是 JSON 参数：判失败，消耗 name 部分 + 已见内容
        let raw_consumed = bracket + 1 + skip + rest.len().min(64);
        let consumed = buf.floor_char_boundary(raw_consumed);
        return LegacyScan::Invalid { consumed };
    }
    // brace 匹配（容忍字符串内转义）
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for (i, c) in rest.char_indices() {
        if esc {
            esc = false;
            continue;
        }
        match c {
            '\\' if in_str => esc = true,
            '"' => in_str = !in_str,
            '{' if !in_str => depth += 1,
            '}' if !in_str => {
                depth -= 1;
                if depth == 0 {
                    let args = &rest[..=i];
                    if serde_json::from_str::<Value>(args).is_ok() {
                        return LegacyScan::Done {
                            name,
                            args: args.to_string(),
                            consumed: bracket + 1 + skip + i + 1,
                        };
                    }
                    return LegacyScan::Invalid {
                        consumed: bracket + 1 + skip + i + 1,
                    };
                }
            }
            _ => {}
        }
    }
    LegacyScan::Incomplete
}

/// 剥离片段的告警日志（含前 100 字符摘要）。
fn log_discard(reason: &str, frag: &str) {
    let summary: String = frag.chars().take(100).collect();
    tracing::warn!(
        target: "llm_compat",
        reason,
        fragment = %summary,
        "pseudo tool call fragment discarded"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn disabled_when_no_extra_config() {
        assert!(!compat_tool_history_enabled(None));
        assert!(!compat_tool_history_enabled(Some("")));
        assert!(!compat_tool_history_enabled(Some("not json")));
        assert!(!compat_tool_history_enabled(Some("{}")));
        assert!(!compat_tool_history_enabled(Some(
            r#"{"compat_tool_history": false}"#
        )));
    }

    #[test]
    fn enabled_when_flag_true() {
        assert!(compat_tool_history_enabled(Some(
            r#"{"compat_tool_history": true}"#
        )));
    }

    #[test]
    fn assistant_tool_calls_become_text() {
        let mut msgs = vec![ChatMessage {
            role: "assistant".into(),
            reasoning_content: None,
            content: None,
            tool_calls: Some(vec![json!({
                "id": "call_1",
                "type": "function",
                "function": {"name": "Bash", "arguments": "{\"command\":\"ls\"}"}
            })]),
            tool_call_id: None,
            name: None,
        }];
        assert!(rewrite_tool_history(&mut msgs));
        assert_eq!(msgs[0].role, "assistant");
        assert!(msgs[0].tool_calls.is_none());
        let c = msgs[0].content.as_deref().unwrap();
        assert!(c.contains("Bash"), "应含工具名: {c}");
        assert!(c.contains("ls"), "应含参数: {c}");
        assert!(c.contains("<tool_call>"), "应含 v2 标签: {c}");
    }

    #[test]
    fn tool_role_becomes_user_with_tool_name() {
        let mut msgs = vec![
            ChatMessage {
                role: "assistant".into(),
                reasoning_content: None,
                content: None,
                tool_calls: Some(vec![json!({
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{}"}
                })]),
                tool_call_id: None,
                name: None,
            },
            ChatMessage {
                role: "tool".into(),
                reasoning_content: None,
                content: Some("晴 25度".into()),
                tool_calls: None,
                tool_call_id: Some("call_1".into()),
                name: None,
            },
        ];
        assert!(rewrite_tool_history(&mut msgs));
        assert_eq!(msgs[1].role, "user");
        assert!(msgs[1].tool_call_id.is_none());
        let c = msgs[1].content.as_deref().unwrap();
        assert!(c.contains("get_weather"), "应标注来源工具: {c}");
        assert!(c.contains("晴 25度"), "应保留结果内容: {c}");
    }

    #[test]
    fn plain_messages_untouched() {
        let mut msgs = vec![
            ChatMessage::text("system", "sys"),
            ChatMessage::text("user", "hi"),
            ChatMessage::text("assistant", "hello"),
        ];
        assert!(!rewrite_tool_history(&mut msgs));
        assert_eq!(msgs[0].content.as_deref(), Some("sys"));
        assert_eq!(msgs[2].content.as_deref(), Some("hello"));
    }

    #[test]
    fn assistant_text_and_tool_calls_merge() {
        // assistant 同时有正文和 tool_calls 时，两者都保留
        let mut msgs = vec![ChatMessage {
            role: "assistant".into(),
            reasoning_content: None,
            content: Some("我先查一下".into()),
            tool_calls: Some(vec![json!({
                "id": "c1",
                "type": "function",
                "function": {"name": "Read", "arguments": "{\"path\":\"/a\"}"}
            })]),
            tool_call_id: None,
            name: None,
        }];
        rewrite_tool_history(&mut msgs);
        let c = msgs[0].content.as_deref().unwrap();
        assert!(c.contains("我先查一下"));
        assert!(c.contains("Read"));
    }

    #[test]
    fn rewritten_output_has_no_tool_structure() {
        // 序列化后不得再出现 tool_calls / tool_call_id / role:"tool"
        let mut msgs = vec![
            ChatMessage {
                role: "assistant".into(),
                reasoning_content: None,
                content: None,
                tool_calls: Some(vec![json!({
                    "id": "c1", "type": "function",
                    "function": {"name": "Bash", "arguments": "{}"}
                })]),
                tool_call_id: None,
                name: None,
            },
            ChatMessage {
                role: "tool".into(),
                reasoning_content: None,
                content: Some("out".into()),
                tool_calls: None,
                tool_call_id: Some("c1".into()),
                name: None,
            },
        ];
        rewrite_tool_history(&mut msgs);
        let s = serde_json::to_string(&msgs).unwrap();
        assert!(!s.contains("tool_calls"), "{s}");
        assert!(!s.contains("tool_call_id"), "{s}");
        assert!(!s.contains("\"tool\""), "{s}");
    }

    // ── v2 协议：标签格式改写 ──────────────────────────────────

    #[test]
    fn rewrite_v2_assistant_tool_calls_become_tags() {
        let mut msgs = vec![ChatMessage {
            role: "assistant".into(),
            reasoning_content: None,
            content: None,
            tool_calls: Some(vec![json!({
                "id": "call_1", "type": "function",
                "function": {"name": "Bash", "arguments": "{\"command\":\"ls\"}"}
            })]),
            tool_call_id: None,
            name: None,
        }];
        assert!(rewrite_tool_history(&mut msgs));
        let c = msgs[0].content.as_deref().unwrap();
        assert!(c.contains("<tool_call>"), "应含起始标签: {c}");
        assert!(c.contains("</tool_call>"), "应含结束标签: {c}");
        assert!(c.contains(r#""name":"Bash""#), "应含工具名: {c}");
        assert!(c.contains(r#""arguments":"#), "应含 arguments: {c}");
        assert!(!c.contains("[调用工具"), "不应再有旧格式: {c}");
        assert!(msgs[0].tool_calls.is_none());
    }

    #[test]
    fn rewrite_v2_tool_role_becomes_tool_result_tag() {
        let mut msgs = vec![
            ChatMessage {
                role: "assistant".into(),
                reasoning_content: None,
                content: None,
                tool_calls: Some(vec![json!({
                    "id": "call_1", "type": "function",
                    "function": {"name": "Read", "arguments": "{}"}
                })]),
                tool_call_id: None,
                name: None,
            },
            ChatMessage {
                role: "tool".into(),
                reasoning_content: None,
                content: Some("file content".into()),
                tool_calls: None,
                tool_call_id: Some("call_1".into()),
                name: None,
            },
        ];
        assert!(rewrite_tool_history(&mut msgs));
        assert_eq!(msgs[1].role, "user");
        let c = msgs[1].content.as_deref().unwrap();
        assert!(
            c.contains("<tool_result name=\"Read\">"),
            "应含工具结果标签: {c}"
        );
        assert!(c.contains("file content"), "应保留结果内容: {c}");
        assert!(c.contains("</tool_result>"), "应含结束标签: {c}");
        assert!(!c.contains("[工具结果"), "不应再有旧格式: {c}");
    }

    #[test]
    fn rewrite_v2_content_before_tool_call_preserved() {
        let mut msgs = vec![ChatMessage {
            role: "assistant".into(),
            reasoning_content: None,
            content: Some("我先看下".into()),
            tool_calls: Some(vec![json!({
                "id": "c1", "type": "function",
                "function": {"name": "Bash", "arguments": "{}"}
            })]),
            tool_call_id: None,
            name: None,
        }];
        rewrite_tool_history(&mut msgs);
        let c = msgs[0].content.as_deref().unwrap();
        assert!(c.starts_with("我先看下"), "正文应在标签前: {c}");
        assert!(c.contains("<tool_call>"));
    }

    // ── v2：引导注入 ──────────────────────────────────────────

    #[test]
    fn guidance_injected_at_end() {
        let mut msgs = vec![
            ChatMessage::text("system", "sys"),
            ChatMessage::text("user", "hi"),
        ];
        inject_tool_call_guidance(&mut msgs);
        let last = msgs.last().unwrap();
        assert_eq!(last.role, "system");
        assert!(last.content.as_deref().unwrap().contains("<tool_call>"));
        assert!(last.content.as_deref().unwrap().contains("tool_calls"));
    }

    #[test]
    fn guidance_injection_idempotent() {
        let mut msgs = vec![ChatMessage::text("user", "hi")];
        inject_tool_call_guidance(&mut msgs);
        let n = msgs.len();
        inject_tool_call_guidance(&mut msgs);
        assert_eq!(msgs.len(), n, "重复注入不应增加消息");
    }

    // ── v2：TagScanner 新格式解析 ─────────────────────────────

    #[test]
    fn scanner_single_tool_call_tag() {
        let mut s = TagScanner::new();
        let events = s.push(
            "<tool_call>\n{\"name\":\"Bash\",\"arguments\":{\"command\":\"ls\"}}\n</tool_call>",
        );
        let calls: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ScanEvent::ToolCall(v) => Some(v),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "Bash");
        assert_eq!(calls[0]["id"], "call_1");
        let args: Value =
            serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["command"], "ls");
        assert!(s.has_tool_calls());
    }

    #[test]
    fn scanner_text_before_and_after() {
        let mut s = TagScanner::new();
        let events =
            s.push("先执行\n<tool_call>{\"name\":\"Bash\",\"arguments\":{}}</tool_call>\n完成");
        let texts: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                ScanEvent::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert!(texts.iter().any(|t| t.contains("先执行")), "{texts:?}");
        assert!(texts.iter().any(|t| t.contains("完成")), "{texts:?}");
    }

    #[test]
    fn scanner_tag_split_across_chunks() {
        let mut s = TagScanner::new();
        let mut events = Vec::new();
        // 起始标签被切成三片喂入
        events.extend(s.push("前置<to"));
        events.extend(s.push("ol_call"));
        events.extend(s.push(">{\"name\":\"A\",\"arguments\":{}}</tool_call>"));
        let calls: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ScanEvent::ToolCall(v) => Some(v),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 1, "跨 chunk 标签应被识别: {events:?}");
        // 切断前的"前置"文本应立即输出
        let first_text = events.iter().find_map(|e| match e {
            ScanEvent::Text(t) => Some(t.as_str()),
            _ => None,
        });
        assert_eq!(first_text, Some("前置"));
    }

    #[test]
    fn scanner_json_with_code_fence_tolerated() {
        let mut s = TagScanner::new();
        let events =
            s.push("<tool_call>\n```json\n{\"name\":\"Bash\",\"arguments\":{}}\n```\n</tool_call>");
        let calls: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ScanEvent::ToolCall(v) => Some(v),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 1, "应容忍代码围栏: {events:?}");
    }

    #[test]
    fn scanner_broken_json_discarded_not_leaked() {
        let mut s = TagScanner::new();
        let events = s.push("<tool_call>\n{bad json\n</tool_call>");
        assert!(
            events.iter().any(|e| matches!(e, ScanEvent::Discarded(_))),
            "坏 JSON 应产生 Discarded: {events:?}"
        );
        for e in &events {
            if let ScanEvent::Text(t) = e {
                assert!(!t.contains("<tool_call>"), "泄漏: {t}");
                assert!(!t.contains("bad json"), "泄漏: {t}");
            }
        }
    }

    #[test]
    fn scanner_unclosed_tag_discarded_on_finish() {
        let mut s = TagScanner::new();
        s.push("<tool_call>\n{\"name\":\"Bash\"");
        let events = s.finish();
        assert!(
            events.iter().any(|e| matches!(e, ScanEvent::Discarded(_))),
            "未闭合标签应 Discarded: {events:?}"
        );
        for e in &events {
            if let ScanEvent::Text(t) = e {
                assert!(!t.contains("tool_call"), "泄漏: {t}");
            }
        }
    }

    #[test]
    fn scanner_trailing_partial_prefix_flushed_on_finish() {
        let mut s = TagScanner::new();
        let push_events = s.push("正常文本<tool_");
        let finish_events = s.finish();
        // push() emits safe text immediately; finish() flushes the trailing
        // prefix that was being held back as a potential marker start.
        let push_text: String = push_events
            .iter()
            .filter_map(|e| match e {
                ScanEvent::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert!(
            push_text.contains("正常文本"),
            "安全文本应即时输出: {push_text}"
        );
        let finish_text: String = finish_events
            .iter()
            .filter_map(|e| match e {
                ScanEvent::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert!(
            finish_text.contains("<tool_"),
            "悬挂前缀应在 finish 时放行: {finish_text}"
        );
        let all_text = push_text + &finish_text;
        assert!(
            all_text.contains("正常文本<tool_"),
            "结合 push+finish 应有完整文本: {all_text}"
        );
    }

    // ── v2：旧格式回环兼容 ────────────────────────────────────

    #[test]
    fn scanner_legacy_format_still_parsed() {
        let mut s = TagScanner::new();
        let events = s.push("[调用工具 Bash] {\"command\":\"ls\"}");
        let calls: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ScanEvent::ToolCall(v) => Some(v),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 1, "旧格式仍应识别: {events:?}");
        assert_eq!(calls[0]["function"]["name"], "Bash");
    }

    #[test]
    fn scanner_legacy_broken_discarded() {
        let mut s = TagScanner::new();
        let events = s.push("[调用工具 Bash] {invalid}");
        assert!(
            events.iter().any(|e| matches!(e, ScanEvent::Discarded(_))),
            "旧格式坏 JSON 应 Discarded（不再原文回退）: {events:?}"
        );
    }

    #[test]
    fn scanner_multiple_calls_unique_ids() {
        let mut s = TagScanner::new();
        let events = s.push(
            "<tool_call>{\"name\":\"A\",\"arguments\":{}}</tool_call>\n\
             [调用工具 B] {}\n\
             <tool_call>{\"name\":\"C\",\"arguments\":{}}</tool_call>",
        );
        let calls: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ScanEvent::ToolCall(v) => Some(v.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0]["id"], "call_1");
        assert_eq!(calls[1]["id"], "call_2");
        assert_eq!(calls[2]["id"], "call_3");
    }

    // ── v2：往返还原 ──────────────────────────────────────────

    #[test]
    fn v2_roundtrip_rewrite_then_scan() {
        let mut msgs = vec![ChatMessage {
            role: "assistant".into(),
            reasoning_content: None,
            content: Some("执行".into()),
            tool_calls: Some(vec![json!({
                "id": "x", "type": "function",
                "function": {"name": "Bash", "arguments": "{\"command\":\"ls\"}"}
            })]),
            tool_call_id: None,
            name: None,
        }];
        rewrite_tool_history(&mut msgs);
        let text = msgs[0].content.clone().unwrap();
        let mut s = TagScanner::new();
        let mut events = s.push(&text);
        events.extend(s.finish());
        let calls: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ScanEvent::ToolCall(v) => Some(v),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "Bash");
        let args: Value =
            serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["command"], "ls");
    }

    // ── take_tool_calls ───────────────────────────────────────

    #[test]
    fn take_tool_calls_returns_and_clears() {
        let mut s = TagScanner::new();
        s.push("<tool_call>{\"name\":\"Bash\",\"arguments\":{}}</tool_call>");
        assert!(s.has_tool_calls());
        let calls = s.take_tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "Bash");
        let calls2 = s.take_tool_calls();
        assert!(calls2.is_empty());
    }

    // ── 旧格式 Invalid UTF-8 边界安全 ─────────────────────────

    #[test]
    fn scanner_legacy_invalid_utf8_safe_consumed() {
        // 输入："[调用工具 Bash] " + 30 个中文字符（每个 3 字节）。
        // 旧代码中 rest.len().min(64) 会落在多字节字符中间，导致 buf[..consumed] panic。
        let garbage = "文".repeat(30);
        let input = format!("[调用工具 Bash] {garbage}");
        let mut s = TagScanner::new();
        let events = s.push(&input);
        // 不应 panic，应产出 Discarded 事件
        assert!(
            events.iter().any(|e| matches!(e, ScanEvent::Discarded(_))),
            "should produce Discarded, got: {events:?}"
        );
        // 任何 Text 事件不应包含旧的标记前缀
        for e in &events {
            if let ScanEvent::Text(t) = e {
                assert!(!t.contains("[调用工具"), "Discarded text leaked: {t}");
            }
        }
    }
}
