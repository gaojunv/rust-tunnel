//! 探针报告：**顺序敏感**的文本报告（可移植）。
//!
//! 阶段 0 的验收方式是「人在真机上跑一遍，把日志贴回来」，所以报告必须满足两个条件：
//! **顺序确定**（读起来和写代码时的调用顺序一致，便于对照）与**每行独立可 grep**
//! （字段值强制单行，换行被转义）。
//!
//! 这里刻意不用 `serde_json`：报告是给人读的，键值顺序与分组比机器可解析更重要，
//! 而且本 crate 要保持**零依赖**。

use std::fmt::Write as _;

/// 报告里的一项。顺序即 [`ProbeReport::render`] 的输出顺序。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportEntry {
    /// 分组标题，渲染成 `## 标题`。
    Section(String),
    /// 单行键值，渲染成 `键: 值`。
    Field {
        /// 键名（单行）。
        key: String,
        /// 值（单行；换行会被转义）。
        value: String,
    },
    /// 多行块，渲染成 `--- 标题 ---` 加正文原文。
    Block {
        /// 块标题（单行）。
        title: String,
        /// 正文，**原样输出**（不转义，用于贴字符画与错误详情）。
        body: String,
    },
}

/// 一份探针报告。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeReport {
    title: String,
    entries: Vec<ReportEntry>,
}

impl ProbeReport {
    /// 新建报告，`title` 会出现在首行横幅里。
    #[must_use]
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            entries: Vec::new(),
        }
    }

    /// 加一个分组标题。
    pub fn section(&mut self, name: impl Into<String>) -> &mut Self {
        self.entries.push(ReportEntry::Section(name.into()));
        self
    }

    /// 加一条单行键值。值接受任何 `Display`（探针报告里到处都是数字）。
    /// 键与值里的换行会被转义成字面量 `\n`，保证一行一项。
    pub fn field(&mut self, key: impl Into<String>, value: impl std::fmt::Display) -> &mut Self {
        self.entries.push(ReportEntry::Field {
            key: escape_line(&key.into()),
            value: escape_line(&value.to_string()),
        });
        self
    }

    /// 加一条可空键值：`None` 渲染成 `-`（比省略整项更好，缺项本身就是信息）。
    pub fn field_opt<T: std::fmt::Display>(
        &mut self,
        key: impl Into<String>,
        value: Option<T>,
    ) -> &mut Self {
        match value {
            Some(v) => self.field(key, v.to_string()),
            None => self.field(key, "-"),
        }
    }

    /// 加一条被测步骤的结果：`成功` 或失败详情。
    ///
    /// 探针里最常用的一项，「成功/失败」固定在行首，便于肉眼扫。
    pub fn step(
        &mut self,
        key: impl Into<String>,
        result: &Result<impl std::fmt::Debug, crate::WinError>,
    ) -> &mut Self {
        match result {
            Ok(v) => self.field(key, format!("成功 ({v:?})")),
            Err(err) => self.field(key, format!("失败 {err}")),
        }
    }

    /// 加一个多行块（字符画、错误详情、原始缓冲区 dump）。
    pub fn block(&mut self, title: impl Into<String>, body: impl Into<String>) -> &mut Self {
        self.entries.push(ReportEntry::Block {
            title: escape_line(&title.into()),
            body: body.into(),
        });
        self
    }

    /// 条目数（不含标题）。
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否一条都没加。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 渲染成可粘贴的文本（**以换行结尾**）。
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "===== {} =====", self.title);
        for entry in &self.entries {
            match entry {
                ReportEntry::Section(name) => {
                    let _ = writeln!(out, "\n## {name}");
                }
                ReportEntry::Field { key, value } => {
                    let _ = writeln!(out, "{key}: {value}");
                }
                ReportEntry::Block { title, body } => {
                    let _ = writeln!(out, "--- {title} ---");
                    let _ = writeln!(out, "{body}");
                }
            }
        }
        out
    }
}

/// 把值压成单行：换行与回车转义成字面量，制表符转空格。
fn escape_line(s: &str) -> String {
    if !s.contains(['\n', '\r', '\t']) {
        return s.to_owned();
    }
    s.replace('\r', "\\r")
        .replace('\n', "\\n")
        .replace('\t', " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_keeps_insertion_order_and_ends_with_newline() {
        let mut report = ProbeReport::new("探针");
        report
            .section("环境")
            .field("会话", 1)
            .field("完整性", "System")
            .section("注入")
            .field("SendInput", "3/3");
        let text = report.render();

        let expected =
            "===== 探针 =====\n\n## 环境\n会话: 1\n完整性: System\n\n## 注入\nSendInput: 3/3\n";
        assert_eq!(text, expected);
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn field_values_are_forced_to_one_line() {
        let mut report = ProbeReport::new("t");
        report.field("多行", "第一行\n第二行\ttab\r\n");
        let text = report.render();
        assert!(text.contains(r"多行: 第一行\n第二行 tab\r\n"), "{text}");
        // 除标题与尾随换行外只有这一行
        assert_eq!(text.lines().filter(|l| !l.is_empty()).count(), 2);
    }

    #[test]
    fn none_renders_as_dash_so_a_missing_reading_is_visible() {
        let mut report = ProbeReport::new("t");
        // 注意：`field_opt` 有 `impl Into<String>` 参数，故不能写 turbofish，只能靠 `None::<T>`。
        report.field_opt("桌面", None::<u32>);
        report.field_opt("会话", Some(1_u8));
        assert!(report.render().contains("桌面: -"));
        assert!(report.render().contains("会话: 1"));
    }

    #[test]
    fn blocks_keep_their_body_verbatim() {
        let mut report = ProbeReport::new("t");
        report.block("缩略图", "  @@\n  @@\n");
        let text = report.render();
        assert!(text.contains("--- 缩略图 ---\n  @@\n  @@\n"), "{text}");
    }

    #[test]
    fn step_marks_success_and_failure_distinctly() {
        let mut report = ProbeReport::new("t");
        report.step("SetThreadDesktop", &Ok::<u32, crate::WinError>(7));
        report.step(
            "OpenInputDesktop",
            &Err::<u32, crate::WinError>(crate::WinError::new("OpenInputDesktop", 5)),
        );
        let text = report.render();
        assert!(text.contains("SetThreadDesktop: 成功 (7)"), "{text}");
        assert!(
            text.contains("OpenInputDesktop: 失败 OpenInputDesktop 失败（Win32 5）"),
            "{text}"
        );
    }

    #[test]
    fn empty_report_still_has_a_title_and_is_flagged_empty() {
        let report = ProbeReport::new("空");
        assert!(report.is_empty());
        assert_eq!(report.len(), 0);
        assert_eq!(report.render(), "===== 空 =====\n");
    }
}
