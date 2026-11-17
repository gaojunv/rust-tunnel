//! 日志页：运行日志的等级/关键词过滤、暂停自动滚动、复制与清空。
//!
//! 过滤是**纯视图逻辑**：`LogBuffer` 的环形缓冲本身不被改写，这里每帧只取最近 500 条
//! 做一次快照过滤。等级过滤按**类别**匹配（选「警告」就只看 WARN，
//! 选「全部」连 TRACE 与未知等级一起看），不是「最低等级」阈值——分段控件上那五个标签
//! 是并列的类别，用户点哪个就只看哪个。
//!
//! 复制走 [`egui::Context::copy_text`]（egui 只提供写剪贴板，没有读 API），复制的是
//! **当前过滤结果**，不是整个缓冲；缓冲为空或过滤后为空时不碰剪贴板，避免把用户剪贴板
//! 里原有的内容清成空串。

use std::sync::Arc;

use egui::{Align, FontFamily, FontId, Layout, RichText, Ui};
use rust_tunnel_client::LogBuffer;
use rust_tunnel_common::protocol::ClientLogEntry;

use crate::ui::icons::Icon;
use crate::ui::theme::{palette, toolbar_frame};
use crate::ui::widgets::{self, Tone};

/// 每帧从环形缓冲拉取的最大条数。
const RECENT_LIMIT: usize = 500;

/// 日志等级过滤（分段控件的取值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LevelFilter {
    /// 全部等级（含 TRACE 与未知等级）。
    #[default]
    All,
    /// 仅 INFO。
    Info,
    /// 仅 WARN。
    Warn,
    /// 仅 ERROR。
    Error,
    /// 仅 DEBUG。
    Debug,
}

impl LevelFilter {
    /// 分段控件上显示的标签。
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "全部",
            Self::Info => "信息",
            Self::Warn => "警告",
            Self::Error => "错误",
            Self::Debug => "调试",
        }
    }

    /// 该过滤是否接受 `level`（大小写不敏感；[`LevelFilter::All`] 接受一切）。
    #[must_use]
    pub fn matches(self, level: &str) -> bool {
        match self {
            Self::All => true,
            Self::Info => level.eq_ignore_ascii_case("INFO"),
            Self::Warn => level.eq_ignore_ascii_case("WARN"),
            Self::Error => level.eq_ignore_ascii_case("ERROR"),
            Self::Debug => level.eq_ignore_ascii_case("DEBUG"),
        }
    }
}

/// 日志页的持久 UI 状态（由 `GuiApp` 跨帧持有）。
#[allow(
    clippy::module_name_repetitions,
    reason = "公开名称由协调方统一约定，改名会与并行改动的调用点冲突"
)]
#[derive(Debug, Clone, Default)]
pub struct LogsState {
    /// 当前等级过滤。
    pub level: LevelFilter,
    /// 关键字：在 target 与 message 上做大小写不敏感的包含匹配。
    pub query: String,
    /// 暂停自动滚动：为真时不再把滚动条钉在底部，方便向上翻阅历史。
    pub paused: bool,
}

/// 渲染日志页：页头动作、等级/关键词工具条与日志列表。
///
/// `log_buffer` 以 `&Arc` 传入（调用方持有共享缓冲）；本函数只读它，唯一会改动缓冲的
/// 是用户点击「清空」。
pub fn show(ui: &mut Ui, log_buffer: &Arc<LogBuffer>, state: &mut LogsState) {
    let total = log_buffer.len();
    let recent = log_buffer.recent(RECENT_LIMIT);

    let mut copy_clicked = false;
    let mut clear_clicked = false;
    widgets::page_header(
        ui,
        "日志",
        "控制 / 隧道 / 远程控制的运行日志",
        |ui| {
            let p = palette(ui.style());
            if widgets::icon_button_colored(ui, Icon::Trash, "清空日志", Some(p.err)).clicked()
            {
                clear_clicked = true;
            }
            if widgets::icon_button(ui, Icon::Copy, "复制当前过滤结果").clicked() {
                copy_clicked = true;
            }
            widgets::badge(ui, &format!("{total} 条"), Tone::Neutral, None);
        },
    );

    // 复制发生在页头、早于工具条，因此这里用**当前**状态重算一次过滤结果（点击复制这一帧
    // 用户不可能同时在输入框里改关键词，所以不存在与列表不一致的问题）。
    if copy_clicked {
        let filtered = filter_entries(&recent, state.level, &state.query);
        let text = copy_text(&filtered);
        if !text.is_empty() {
            ui.ctx().copy_text(text);
        }
    }

    level_toolbar(ui, state);
    ui.add_space(10.0);

    // 工具条可能在本帧改掉等级/关键词（分段点选、输入），所以列表在工具条**之后**重算，
    // 保证点一下「错误」这一帧就换结果，而不是等下一次输入事件。
    let filtered = filter_entries(&recent, state.level, &state.query);
    log_card(ui, &recent, &filtered, state.paused);

    // 清空放在最后：本帧列表仍按清空前的快照画完，下一帧（已请求重绘）自然变空，
    // 不会出现「点了清空却整块闪一下」的观感。
    if clear_clicked {
        log_buffer.clear();
        ui.ctx().request_repaint();
    }
}

/// 工具条卡片：等级分段 + 关键词搜索 + 暂停自动滚动开关。
fn level_toolbar(ui: &mut Ui, state: &mut LogsState) {
    let p = palette(ui.style());
    let options = [
        (LevelFilter::All, LevelFilter::All.label()),
        (LevelFilter::Info, LevelFilter::Info.label()),
        (LevelFilter::Warn, LevelFilter::Warn.label()),
        (LevelFilter::Error, LevelFilter::Error.label()),
        (LevelFilter::Debug, LevelFilter::Debug.label()),
    ];
    toolbar_frame(ui.style()).show(ui, |ui| {
        // 用 `horizontal_wrapped`：默认 760 窗口里分段控件 + 搜索框 + 暂停开关刚好在
        // 临界宽度，窄一点时让暂停开关自动换到下一行，而不是把右边挤出可视区。
        ui.horizontal_wrapped(|ui| {
            if let Some(picked) = widgets::segmented(ui, state.level, &options) {
                state.level = picked;
            }
            ui.add_space(6.0);
            widgets::search_field(ui, &mut state.query, "搜索日志内容");
            ui.add_space(6.0);
            widgets::toggle(ui, &mut state.paused);
            ui.label(RichText::new("暂停自动滚动").size(12.0).color(p.text_weak));
        });
    });
}

/// 日志卡片：空状态或可滚动的日志列表。
fn log_card(ui: &mut Ui, recent: &[ClientLogEntry], filtered: &[&ClientLogEntry], paused: bool) {
    widgets::card(ui, |ui| {
        if recent.is_empty() {
            widgets::empty_state(
                ui,
                Icon::List,
                "暂无日志",
                "连接后自动采集，含控制/隧道/agent 动态",
            );
            return;
        }
        if filtered.is_empty() {
            widgets::empty_state(ui, Icon::Search, "没有匹配的日志", "调整等级或关键词");
            return;
        }
        let max_height = (ui.available_height() - 6.0).max(160.0);
        egui::ScrollArea::vertical()
            .stick_to_bottom(!paused)
            .auto_shrink([false, false])
            .max_height(max_height)
            .show(ui, |ui| {
                for (index, entry) in filtered.iter().copied().enumerate() {
                    if index > 0 {
                        ui.add_space(4.0);
                    }
                    log_row(ui, entry);
                }
            });
    });
}

/// 单条日志：左侧时间 + 等级徽标，右侧 target 与可换行的 message。
fn log_row(ui: &mut Ui, entry: &ClientLogEntry) {
    let p = palette(ui.style());
    ui.horizontal_top(|ui| {
        ui.add_sized(
            [54.0, 16.0],
            egui::Label::new(
                RichText::new(format_time(entry.timestamp))
                    .font(mono(11.0))
                    .color(p.text_weak),
            ),
        );
        // 等级徽标给固定宽度：ERROR 比 INFO 宽，不固定的话右侧 target/message 的左边界
        // 会逐行参差。
        ui.allocate_ui_with_layout(
            egui::vec2(58.0, 20.0),
            Layout::left_to_right(Align::Center),
            |ui| {
                widgets::badge(ui, &entry.level, level_tone(&entry.level), None);
            },
        );
        ui.add_space(2.0);
        ui.vertical(|ui| {
            ui.set_width(ui.available_width().max(1.0));
            ui.add(
                egui::Label::new(
                    RichText::new(entry.target.as_str())
                        .font(mono(11.0))
                        .color(p.text_weak),
                )
                .truncate(),
            );
            ui.add(
                egui::Label::new(
                    RichText::new(entry.message.as_str())
                        .font(mono(12.0))
                        .color(p.text),
                )
                .wrap(),
            );
        });
    });
}

/// 等级到语义色调的映射（ERROR/WARN/INFO；DEBUG 与 TRACE/未知一律中性）。
fn level_tone(level: &str) -> Tone {
    if level.eq_ignore_ascii_case("ERROR") {
        Tone::Err
    } else if level.eq_ignore_ascii_case("WARN") {
        Tone::Warn
    } else if level.eq_ignore_ascii_case("INFO") {
        Tone::Accent
    } else {
        Tone::Neutral
    }
}

/// 微秒时间戳 → 当天内的 `HH:MM:SS`（UTC；不引时区依赖，与原先的显示口径一致）。
fn format_time(timestamp_us: i64) -> String {
    let secs = timestamp_us.max(0) / 1_000_000;
    let h = (secs / 3600) % 24;
    let m = (secs / 60) % 60;
    let s = secs % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

/// 过滤：先按等级类别，再按关键词（target 或 message 大小写不敏感包含）。
fn filter_entries<'a>(
    entries: &'a [ClientLogEntry],
    level: LevelFilter,
    query: &str,
) -> Vec<&'a ClientLogEntry> {
    let needle = query.trim().to_lowercase();
    entries
        .iter()
        .filter(|entry| level.matches(&entry.level))
        .filter(|entry| {
            needle.is_empty()
                || entry.target.to_lowercase().contains(&needle)
                || entry.message.to_lowercase().contains(&needle)
        })
        .collect()
}

/// 把过滤结果拼成可粘贴的纯文本（每行 `HH:MM:SS LEVEL target message`）。
fn copy_text(entries: &[&ClientLogEntry]) -> String {
    entries
        .iter()
        .copied()
        .map(format_entry)
        .collect::<Vec<_>>()
        .join("\n")
}

/// 单条日志的复制文本。
fn format_entry(entry: &ClientLogEntry) -> String {
    let level = entry.level.as_str();
    let target = entry.target.as_str();
    let message = entry.message.as_str();
    format!(
        "{} {level} {target} {message}",
        format_time(entry.timestamp)
    )
}

/// 等宽字体句柄（日志走等宽才能对齐列）。
fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}

#[cfg(test)]
mod tests {
    use super::{filter_entries, LevelFilter};
    use rust_tunnel_common::protocol::ClientLogEntry;

    fn entry(level: &str, target: &str, message: &str) -> ClientLogEntry {
        ClientLogEntry {
            timestamp: 0,
            level: level.to_owned(),
            target: target.to_owned(),
            message: message.to_owned(),
        }
    }

    /// 等级过滤是**类别**匹配：选「警告」只看 WARN，不看 ERROR；「全部」接受一切。
    #[test]
    fn level_filter_matches_each_category_exactly() {
        assert!(LevelFilter::All.matches("ERROR"));
        assert!(LevelFilter::All.matches("TRACE"));
        assert!(LevelFilter::All.matches(""));

        assert!(LevelFilter::Error.matches("ERROR"));
        assert!(LevelFilter::Error.matches("error"));
        assert!(!LevelFilter::Error.matches("WARN"));

        assert!(LevelFilter::Warn.matches("WARN"));
        assert!(!LevelFilter::Warn.matches("ERROR"));

        assert!(LevelFilter::Info.matches("INFO"));
        assert!(!LevelFilter::Info.matches("DEBUG"));

        assert!(LevelFilter::Debug.matches("debug"));
        assert!(!LevelFilter::Debug.matches("TRACE"));
    }

    #[test]
    fn level_filter_labels_are_distinct() {
        let mut labels = [
            LevelFilter::All,
            LevelFilter::Info,
            LevelFilter::Warn,
            LevelFilter::Error,
            LevelFilter::Debug,
        ]
        .map(LevelFilter::label)
        .to_vec();
        let count = labels.len();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), count);
    }

    #[test]
    fn filter_applies_level_then_query() {
        let entries = [
            entry("INFO", "rust_tunnel_client::control", "已连接到服务器"),
            entry("WARN", "rust_tunnel_client::control", "心跳超时，准备重连"),
            entry("ERROR", "rust_tunnel_client::proxy", "目标拨号失败"),
            entry("DEBUG", "rust_tunnel_client::logs", "flush batch size=32"),
        ];

        // 仅等级
        let level_only = filter_entries(&entries, LevelFilter::Warn, "");
        assert_eq!(level_only.len(), 1);
        assert_eq!(level_only[0].message, "心跳超时，准备重连");

        // 仅关键词：target 命中、大小写不敏感
        let query_only = filter_entries(&entries, LevelFilter::All, "CONTROL");
        assert_eq!(query_only.len(), 2, "关键词应大小写不敏感地匹配 target");

        // 等级 + 关键词同时生效
        let combined = filter_entries(&entries, LevelFilter::Info, "服务器");
        assert_eq!(combined.len(), 1);
        assert_eq!(combined[0].level, "INFO");

        // 等级与关键词的交集为空时结果为空
        let none = filter_entries(&entries, LevelFilter::Error, "心跳");
        assert!(none.is_empty(), "等级与关键词必须同时满足");
    }

    #[test]
    fn query_matches_message_and_is_trimmed() {
        let entries = [entry("INFO", "a::b", "Connection established")];
        assert_eq!(
            filter_entries(&entries, LevelFilter::All, "  ESTABLISHED ").len(),
            1,
            "关键词应去首尾空白并大小写不敏感地匹配 message"
        );
        assert_eq!(
            filter_entries(&entries, LevelFilter::All, "target").len(),
            0
        );
    }
}
