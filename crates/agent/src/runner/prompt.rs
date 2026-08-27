//! 用户消息与 @引用文件内容组装（落库/进上下文的 user 消息合成）。

/// @引用个数上限。
pub const MAX_REFS: usize = 10;
/// 单个引用文件字节上限。
pub const MAX_REF_FILE_BYTES: usize = 50 * 1024;
/// 引用文件总字节上限。
pub const MAX_REFS_TOTAL_BYTES: usize = 200 * 1024;

/// 把用户消息与引用文件内容合成单条 user 消息（落库/进上下文的都是这条）。
#[must_use]
pub fn compose_user_message(
    content: &str,
    ref_files: &[(String, Result<String, String>)],
) -> String {
    use std::fmt::Write as _;
    if ref_files.is_empty() {
        return content.to_string();
    }
    let mut out = content.to_string();
    for (path, result) in ref_files {
        match result {
            Ok(text) => {
                let truncated = if text.len() > MAX_REF_FILE_BYTES {
                    let mut cut = MAX_REF_FILE_BYTES;
                    while !text.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    format!("{}\n[truncated]", &text[..cut])
                } else {
                    text.clone()
                };
                let _ = write!(out, "\n\n--- 引用文件: {path} ---\n```\n{truncated}\n```");
            }
            Err(_) => {
                let _ = write!(out, "\n\n[无法读取: {path}]");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compose_user_message_with_refs() {
        let msg = compose_user_message(
            "帮我重构",
            &[("src/main.rs".to_string(), Ok("fn main() {}".to_string()))],
        );
        assert!(msg.starts_with("帮我重构"));
        assert!(msg.contains("--- 引用文件: src/main.rs ---"));
        assert!(msg.contains("fn main() {}"));
    }

    #[test]
    fn test_compose_user_message_ref_failure_annotated() {
        let msg = compose_user_message(
            "看下这个",
            &[("missing.rs".to_string(), Err("not found".to_string()))],
        );
        assert!(msg.contains("[无法读取: missing.rs]"));
    }

    #[test]
    fn test_compose_user_message_no_refs_passthrough() {
        assert_eq!(compose_user_message("纯文本", &[]), "纯文本");
    }

    #[test]
    fn test_compose_user_message_file_truncated() {
        let big = "x".repeat(60 * 1024);
        let msg = compose_user_message("看", &[("big.rs".to_string(), Ok(big))]);
        assert!(msg.contains("[truncated]"));
    }

    // ── task 工具 / 子 agent 相关测试 ──────────────────────────
}
