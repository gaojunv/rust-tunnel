//! tool_result 行 content 的结构化格式（M2 契约，与前端 ChatStream 严格一致）。
//!
//! 2026-08 起 ACP 路径把 content 从纯文本改写为 JSON 字符串：
//! `{"text": string, "status": string, "diffs"?: [...], "locations"?: [...]}`
//! （空字段省略）。status 落库修复「失败工具刷新后恒显 ✓」（旧实现只落 result
//! 文本、status 丢失，前端无法区分失败）；diffs/locations 落库使刷新后 diff 展示
//! 不丢（此前只有 ToolCallUpdate 实时帧携带）。
//!
//! 存量旧行是纯文本，所有读取方（session load 重放、distill/compact 渲染）必须
//! 向后兼容：JSON 解析失败 / 非对象 / 缺 string text → 走旧路径原样使用。
//!
//! 落库语义（[`crate::tool_result::tool_result_persist_content`]）：status 为 running/completed（或缺省）
//! 且 text/diffs/locations 全空的中间态是「空占位」，传 "" 让 upsert 不覆盖已落库
//! 的真实结果；其余情况（含 failed 等异常终态）落结构化 JSON——即使 text 为空，
//! 异常终态也必须落库，前端才能把卡片打叉。
//!
//! 本文件从 `agent/tool_result.rs` 下移至此，属存储关注点（persistence）——
//! 消除 `persistence::db::agent` 对 `agent` 的依赖，两个纯函数为存储序列化逻辑。

use serde_json::Value;

/// 判断某帧字段是否携带非空数组（diffs/locations）。缺省 / null / 空数组视为无。
fn has_items(v: Option<&Value>) -> bool {
    v.and_then(Value::as_array).is_some_and(|a| !a.is_empty())
}

/// 组装结构化 content JSON（契约格式，空字段省略）。
fn tool_result_content_json(
    text: Option<&str>,
    status: Option<&str>,
    diffs: Option<&Value>,
    locations: Option<&Value>,
) -> String {
    let mut obj = serde_json::Map::new();
    obj.insert("text".into(), Value::String(text.unwrap_or("").to_string()));
    obj.insert(
        "status".into(),
        Value::String(status.unwrap_or("").to_string()),
    );
    if let Some(v) = diffs.filter(|v| has_items(Some(v))) {
        obj.insert("diffs".into(), v.clone());
    }
    if let Some(v) = locations.filter(|v| has_items(Some(v))) {
        obj.insert("locations".into(), v.clone());
    }
    serde_json::Value::Object(obj).to_string()
}

/// 由 tool_result WS 帧字段决定落库 content：空占位返回 ""（不覆盖已有真实结果），
/// 否则返回结构化 JSON。`status` 是 `frame["status"]`（ACP 缺省时为空串）。
#[must_use]
pub fn tool_result_persist_content(
    text: Option<&str>,
    status: Option<&str>,
    diffs: Option<&Value>,
    locations: Option<&Value>,
) -> String {
    let text = text.unwrap_or("");
    let status = status.unwrap_or("");
    let is_placeholder = text.is_empty()
        && !has_items(diffs)
        && !has_items(locations)
        && matches!(status, "running" | "completed" | "");
    if is_placeholder {
        String::new()
    } else {
        tool_result_content_json(Some(text), Some(status), diffs, locations)
    }
}

/// 从 tool_result content 提取 `text` 字段（新 JSON 格式）。旧纯文本行 / 非法
/// JSON / 非对象 / 缺 string text → None（调用方按旧路径原样使用）。
pub fn tool_result_text(content: &str) -> Option<String> {
    let v: Value = serde_json::from_str(content).ok()?;
    v.as_object()?.get("text")?.as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_empty_for_intermediate_state() {
        // 中间态（running、无任何产出）→ ""，upsert 不覆盖真实结果。
        let frame = serde_json::json!({"result": "", "status": "running"});
        assert_eq!(
            tool_result_persist_content(
                frame["result"].as_str(),
                frame["status"].as_str(),
                frame.get("diffs"),
                frame.get("locations"),
            ),
            ""
        );
    }

    #[test]
    fn placeholder_empty_for_completed_without_text() {
        // completed 但无产出（罕见）同样视为占位：不覆盖已落库结果。
        let frame = serde_json::json!({"result": "", "status": "completed"});
        assert_eq!(
            tool_result_persist_content(
                frame["result"].as_str(),
                frame["status"].as_str(),
                frame.get("diffs"),
                frame.get("locations"),
            ),
            ""
        );
    }

    #[test]
    fn failed_empty_text_persists_json() {
        // 异常终态：即使 text 为空也要落库（前端据此打叉，否则刷新后恒显 ✓）。
        let frame = serde_json::json!({"result": "", "status": "failed"});
        let content = tool_result_persist_content(
            frame["result"].as_str(),
            frame["status"].as_str(),
            frame.get("diffs"),
            frame.get("locations"),
        );
        let v: Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["text"], "");
        assert_eq!(v["status"], "failed");
    }

    #[test]
    fn completed_with_text_persists_json() {
        let frame = serde_json::json!({
            "result": "a.rs",
            "status": "completed",
            "diffs": [{"old": "x", "new": "y"}],
            "locations": [{"path": "a.rs", "line": 3}],
        });
        let content = tool_result_persist_content(
            frame["result"].as_str(),
            frame["status"].as_str(),
            frame.get("diffs"),
            frame.get("locations"),
        );
        let v: Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["text"], "a.rs");
        assert_eq!(v["status"], "completed");
        assert_eq!(v["diffs"][0]["new"], "y");
        assert_eq!(v["locations"][0]["line"], 3);
        // 空字段省略：locations 非空才写
        assert!(v.get("diffs").is_some());
        assert!(v.get("locations").is_some());
    }

    #[test]
    fn empty_fields_omitted() {
        // 无 diffs/locations 时 JSON 只含 text/status 两键。
        let frame = serde_json::json!({"result": "ok", "status": "completed"});
        let content = tool_result_persist_content(
            frame["result"].as_str(),
            frame["status"].as_str(),
            frame.get("diffs"),
            frame.get("locations"),
        );
        let v: Value = serde_json::from_str(&content).unwrap();
        assert!(v.get("diffs").is_none());
        assert!(v.get("locations").is_none());
    }

    #[test]
    fn extract_text_new_format() {
        let content = r#"{"text":"a.rs","status":"completed"}"#;
        assert_eq!(tool_result_text(content), Some("a.rs".to_string()));
    }

    #[test]
    fn extract_text_legacy_plain_text_is_none() {
        // 存量旧行：纯文本（含非 JSON 文本）→ None，走旧路径原样使用。
        assert_eq!(tool_result_text("a.rs"), None);
        assert_eq!(tool_result_text("not json {"), None);
    }

    #[test]
    fn extract_text_rejects_non_object_or_missing_text() {
        assert_eq!(tool_result_text(r#"["a.rs"]"#), None);
        assert_eq!(tool_result_text(r#"{"status":"failed"}"#), None);
        assert_eq!(tool_result_text(r#"{"text":42}"#), None);
    }
}
