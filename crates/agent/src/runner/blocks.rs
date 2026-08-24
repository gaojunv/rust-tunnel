//! 系统提示词块操作：角色/plan 块移除与插入、子代理 frame 父标记注入。

use crate::session::SessionRuntime;

/// 若 WS 帧需要 parent_tool_call_id，注入到帧 JSON 中。
pub(crate) fn with_parent(frame: &mut serde_json::Value, rt: &SessionRuntime) {
    if let Some(ref id) = rt.parent_tool_call_id {
        frame["parent_tool_call_id"] = serde_json::Value::String(id.clone());
    }
}

/// 主会话角色系统提示块的 tag（块以该前缀开始）。
pub(crate) const ROLE_BLOCK_TAG: &str = "\n\n---\n\n# Role: ";

/// 按 tag 移除 system 消息中的一个动态块：tag 起点截到下一个块分隔符
/// （`\n\n---\n\n`）之前；自身 tag 含分隔符前缀，搜索后续分隔符必须从
/// tag 结束之后开始，否则匹配到自身导致移除失效。块后无其他块时截到结尾。
pub(crate) fn remove_tagged_block(content: &str, tag: &str) -> String {
    let Some(pos) = content.find(tag) else {
        return content.to_string();
    };
    let before = content[..pos].trim_end().to_string();
    let rest_start = pos + tag.len();
    match content[rest_start..].find("\n\n---\n\n") {
        Some(p) => format!("{before}{}", &content[rest_start + p..]),
        None => before,
    }
}

/// 把块插到锚点 tag 之前（锚点不存在则追加到末尾）。
pub(crate) fn insert_block_before(content: &str, anchor_tag: &str, block: &str) -> String {
    if let Some(pos) = content.find(anchor_tag) {
        format!("{}{}{}", content[..pos].trim_end(), block, &content[pos..])
    } else {
        format!("{content}{block}")
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_remove_tagged_block_middle() {
        // 角色块在中间：前后块都保留，tag 自身的分隔符前缀不得被误判为下一个块。
        let content = "base\n\n---\n\n# Role: explore\n只读\n提示词\n\n---\n\n# Plan Mode\nplan 内容";
        let out = remove_tagged_block(content, ROLE_BLOCK_TAG);
        assert_eq!(out, "base\n\n---\n\n# Plan Mode\nplan 内容");
    }

    #[test]
    fn test_remove_tagged_block_at_end_and_absent() {
        let content = "base\n\n---\n\n# Role: a\n提示词";
        assert_eq!(remove_tagged_block(content, ROLE_BLOCK_TAG), "base");
        // tag 不存在：原样返回
        assert_eq!(remove_tagged_block("base", ROLE_BLOCK_TAG), "base");
    }

    #[test]
    fn test_remove_tagged_block_self_match_trap() {
        // 回归：在 tag 起点处搜索分隔符会匹配到 tag 自身前缀（tag 以分隔符开头），
        // 必须从 tag 结束之后开始搜，否则移除失效（原样返回）。
        let content = "base\n\n---\n\n# Role: a\n提示词\n\n---\n\n### Available Sub-Agent Roles\n- x";
        let out = remove_tagged_block(content, ROLE_BLOCK_TAG);
        assert!(!out.contains("# Role:"), "角色块应被移除: {out}");
        assert!(out.contains("### Available Sub-Agent Roles"));
    }

    #[test]
    fn test_insert_block_before_anchor() {
        let content = "base\n\n---\n\n# Plan Mode\nplan";
        let out = insert_block_before(content, "\n\n---\n\n# Plan Mode\n", "\n\n---\n\n# Role: r\n提示词");
        assert_eq!(out, "base\n\n---\n\n# Role: r\n提示词\n\n---\n\n# Plan Mode\nplan");
        // 锚点不存在 → 追加
        assert_eq!(insert_block_before("base", "\n\n---\n\n# Plan Mode\n", "\n\n---\n\n# Role: r\n提示词"), "base\n\n---\n\n# Role: r\n提示词");
    }

}
