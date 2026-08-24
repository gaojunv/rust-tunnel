//! 工具结果文本化：超长截断（头尾保留）与 [`AgentResult`] → 文本。

use rust_tunnel_common::AgentResult;

/// 工具结果落库/回填上限：300 行或 30KB（先到者），保护 DB 体积与 LLM 上下文。
const TOOL_RESULT_MAX_LINES: usize = 300;
const TOOL_RESULT_MAX_BYTES: usize = 30 * 1024;
/// head+tail 各保留的行数（300 行总量 = 前 150 + 后 150）。
const TOOL_RESULT_HEAD_LINES: usize = 150;
const TOOL_RESULT_TAIL_LINES: usize = 150;

pub(crate) fn truncate_tool_result(text: String) -> String {
    let total_lines = text.lines().count();
    if total_lines <= TOOL_RESULT_MAX_LINES && text.len() <= TOOL_RESULT_MAX_BYTES {
        return text;
    }
    // 字节级截断（优先）
    if text.len() > TOOL_RESULT_MAX_BYTES {
        let mut cut = TOOL_RESULT_MAX_BYTES;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        return format!("{}\n[... truncated, total {} bytes ...]", &text[..cut], text.len());
    }
    // 行级 head+tail 截断
    let lines: Vec<&str> = text.lines().collect();
    let head: String = lines[..TOOL_RESULT_HEAD_LINES.min(lines.len())].join("\n");
    let omitted = total_lines.saturating_sub(TOOL_RESULT_HEAD_LINES + TOOL_RESULT_TAIL_LINES);
    let tail_start = lines.len().saturating_sub(TOOL_RESULT_TAIL_LINES);
    let tail: String = lines[tail_start..].join("\n");
    format!("{head}\n[... truncated {omitted} lines ...]\n{tail}")
}

pub(crate) fn agent_result_to_text(result: &AgentResult) -> String {
    let text = match result {
        AgentResult::Shell {
            stdout,
            stderr,
            exit_code,
        } => format!("exit_code={exit_code}\nstdout:\n{stdout}\nstderr:\n{stderr}"),
        AgentResult::FileContent { content } => content.clone(),
        AgentResult::Success => "ok".to_string(),
        AgentResult::Error { message } => format!("error: {message}"),
        AgentResult::WriteOutcome {
            bytes_written,
            lines_added,
            lines_removed,
            diff,
            ..
        } => {
            let base = format!(
                "wrote: +{lines_added}/-{lines_removed} lines, {bytes_written} bytes"
            );
            if diff.len() <= 4096 {
                format!("{base}\n{diff}")
            } else {
                let changed = diff.lines().filter(|l| l.starts_with('+') || l.starts_with('-')).count();
                format!("{base}\n(diff omitted, {changed} changed lines)")
            }
        }
    };
    truncate_tool_result(text)
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_tool_result_by_lines() {
        let text = (0..400)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = truncate_tool_result(text);
        let lines: Vec<&str> = out.lines().collect();
        // head+tail: 前 150 + 标记行 + 后 150 = 301 行
        assert!(lines.len() <= TOOL_RESULT_MAX_LINES + 1); // +1 为 truncated 标记行
        assert!(out.contains("[... truncated"));
        assert!(out.contains("100")); // 省略 100 行
        // 尾部保留：最后一行应为 "line 399"
        assert!(out.contains("line 399"));
    }

    #[test]
    fn test_truncate_tool_result_by_bytes() {
        let text = "x".repeat(40 * 1024);
        let out = truncate_tool_result(text);
        assert!(out.len() < 35 * 1024);
        assert!(out.contains("[... truncated"));
    }

    #[test]
    fn test_truncate_tool_result_short_unchanged() {
        let text = "short output".to_string();
        assert_eq!(truncate_tool_result(text.clone()), text);
    }

    #[test]
    fn test_truncate_tool_result_multibyte_safe() {
        // 截断点落在 UTF-8 多字节序列中间不得 panic
        let text = "汉".repeat(15 * 1024); // ~45KB
        let out = truncate_tool_result(text);
        assert!(out.contains("[... truncated"));
    }

    #[test]
    fn test_write_outcome_short_diff() {
        let result = AgentResult::WriteOutcome {
            bytes_written: 42,
            lines_added: 3,
            lines_removed: 1,
            diff: "--- a\n+++ b\n@@ -1 +1 @@\n-old\n+new".to_string(),
            file_hash: "abc".to_string(),
        };
        let text = agent_result_to_text(&result);
        assert!(text.contains("+3/-1 lines"));
        assert!(text.contains("42 bytes"));
        assert!(text.contains("--- a"));
    }

    #[test]
    fn test_write_outcome_long_diff_omitted() {
        let diff = "x\n".repeat(5000);
        let result = AgentResult::WriteOutcome {
            bytes_written: 100,
            lines_added: 2500,
            lines_removed: 2500,
            diff,
            file_hash: "abc".to_string(),
        };
        let text = agent_result_to_text(&result);
        assert!(text.contains("diff omitted"));
        assert!(text.contains("changed lines"));
    }

    // ── stale hash 记录/清除 ──────────────────────────────

}
