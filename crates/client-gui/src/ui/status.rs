//! 状态页：连接概览、延迟趋势与最近隧道。
//!
//! 页面只读 [`ClientStatus`] 与 [`StatusHistory`]，自身不持有状态：趋势采样由调用方每帧调用
//! [`StatusHistory::sample`] 推进，这里只负责把快照画成 Fluent 风的实色层次。

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "趋势点就是 f32：RTT 的 f64→f32、隧道数的 usize→f32 误差远小于一个像素，\
              这些转换属于采样类型定义本身"
)]
#![allow(
    clippy::module_name_repetitions,
    reason = "`StatusHistory` 是跨模块公开的类型名（主窗口持有一份），改成 `History` 会让调用处失去语境"
)]

use std::collections::VecDeque;
use std::time::{Duration, Instant, SystemTime};

use egui::{Align, Color32, Layout, RichText, Sense, Ui, Vec2};
use rust_tunnel_client::ClientStatus;

use crate::service_watch::{passive_notice, IdentityOwner};
use crate::tray::TrayState;

use super::icons::{self, Icon};
use super::theme::{palette, Palette};
use super::widgets::{self, badge, info_bar, page_header, stat_tile, Tone};

/// 趋势线容量：与页面上「近 60 秒」的文案一致（约 1 Hz × 60）。
const HISTORY_CAPACITY: usize = 60;

/// 采样周期。
const SAMPLE_PERIOD: Duration = Duration::from_secs(1);

/// 最近隧道一次展示的行数；超出部分在卡片内滚动查看。
const RECENT_TUNNEL_ROWS: usize = 8;

/// 统计块的最小可读宽度：低于它时 4 列会把文字挤成一团，退化成 2×2。
/// 统计块的最小可读宽度：低于它就把 4 列降级成 2×2。
///
/// 110 只够放下标签与两位数，`2h 14m` 这类在线时长会把卡片撑出可视区——默认 760 窗口
/// 扣掉导航与页边距后约 520，这里取 150 让它在默认尺寸下走 2×2，窗口拉宽后再回到 4 列。
const MIN_STAT_TILE_WIDTH: f32 = 150.0;

/// RTT 与活跃隧道数的秒级趋势（约 1 Hz、容量 60），供状态页画迷你趋势线。
///
/// `None` 是**断点**而不是数值 0：未连接时会写入，长时间没有新采样时也会补上。
/// 趋势线在断点处不连线，因此「断开」与「延迟 0 ms / 零隧道」在视觉上不会混为一谈。
pub struct StatusHistory {
    /// RTT 采样（毫秒）。
    rtt: VecDeque<Option<f32>>,
    /// 活跃隧道数采样。
    tunnels: VecDeque<Option<f32>>,
    /// 上一次采样的时刻。
    last_sample: Instant,
}

impl Default for StatusHistory {
    fn default() -> Self {
        Self {
            rtt: VecDeque::with_capacity(HISTORY_CAPACITY),
            tunnels: VecDeque::with_capacity(HISTORY_CAPACITY),
            last_sample: Instant::now(),
        }
    }
}

impl StatusHistory {
    /// 推进采样：距上次采样满一个周期才写入，并补齐期间漏掉的秒数（写作断点）。
    ///
    /// 调用方每帧调用即可。窗口被隐藏或进程挂起很久时**不逐秒补满**：超过容量的旧秒数
    /// 对「最近 60 秒」的语义已经没有展示价值，直接清空后只写当前值。
    pub fn sample(&mut self, status: &ClientStatus, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last_sample);
        if elapsed < SAMPLE_PERIOD {
            return;
        }
        let missed = usize::try_from(elapsed.as_secs()).unwrap_or(usize::MAX);
        if missed > HISTORY_CAPACITY {
            self.rtt.clear();
            self.tunnels.clear();
        } else {
            // 本次采样本身代表最近一个周期，之前每个漏掉的周期各补一个断点。
            for _ in 1..missed {
                self.push(None, None);
            }
        }
        let (rtt, tunnels) = live_values(status);
        self.push(rtt, tunnels);
        self.last_sample = now;
    }

    /// RTT 采样序列（毫秒）；`None` 为断点。
    #[must_use]
    pub fn rtt(&self) -> &[Option<f32>] {
        // `push` 每次写入后都会 `make_contiguous`，故第一段切片就是全部内容。
        self.rtt.as_slices().0
    }

    /// 活跃隧道数采样序列；`None` 为断点。
    #[must_use]
    pub fn tunnels(&self) -> &[Option<f32>] {
        // 理由同 `rtt()`。
        self.tunnels.as_slices().0
    }

    /// 写入一对采样并维持容量。
    fn push(&mut self, rtt: Option<f32>, tunnels: Option<f32>) {
        self.rtt.push_back(rtt);
        self.tunnels.push_back(tunnels);
        while self.rtt.len() > HISTORY_CAPACITY {
            self.rtt.pop_front();
        }
        while self.tunnels.len() > HISTORY_CAPACITY {
            self.tunnels.pop_front();
        }
        // `pop_front` 会让环形缓冲回绕，而 `rtt()`/`tunnels()` 只交第一段切片：
        // 每次写入后立刻摆平，保证它们始终是完整序列。
        self.rtt.make_contiguous();
        self.tunnels.make_contiguous();
    }
}

/// 当前时刻应写入趋势线的值；未连接时两侧都是断点。
fn live_values(status: &ClientStatus) -> (Option<f32>, Option<f32>) {
    if !status.connected {
        return (None, None);
    }
    (
        status.rtt_ms.map(|ms| ms as f32),
        Some(status.active_tunnels as f32),
    )
}

/// 状态页入口。
pub fn show(ui: &mut Ui, status: &ClientStatus, owner: IdentityOwner, history: &StatusHistory) {
    let state = TrayState::from_status_with_service(status, owner);
    egui::ScrollArea::vertical()
        .id_salt("status_page")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            page_header(ui, "状态", "连接与隧道概览", |ui| {
                badge(ui, state_label(state), state_tone(state), state_icon(state));
            });
            if owner.is_service() {
                info_bar(
                    ui,
                    Tone::Warn,
                    Some("Windows 服务正在承载客户端身份"),
                    passive_notice(),
                );
            }
            if let Some(error) = status.last_error.as_deref() {
                info_bar(ui, Tone::Err, Some("最近错误"), error);
            }
            hero_card(ui, status, state, history);
            ui.add_space(8.0);
            stat_row(ui, status, history);
            ui.add_space(8.0);
            tunnels_card(ui, status);
        });
}

/// 托盘态 → 状态文案（与托盘首行保持同一套说法）。
const fn state_label(state: TrayState) -> &'static str {
    match state {
        TrayState::Connected => "已连接",
        TrayState::Reconnecting => "重连中",
        TrayState::Offline => "离线",
        TrayState::ServiceOwned => "服务接管",
    }
}

/// 托盘态 → 语义色调：服务接管用警示色（原因由横幅解释）。
const fn state_tone(state: TrayState) -> Tone {
    match state {
        TrayState::Connected => Tone::Ok,
        TrayState::Reconnecting | TrayState::ServiceOwned => Tone::Warn,
        TrayState::Offline => Tone::Neutral,
    }
}

/// 托盘态 → 徽标图标。
const fn state_icon(state: TrayState) -> Option<Icon> {
    match state {
        TrayState::Connected => Some(Icon::Check),
        TrayState::Reconnecting => Some(Icon::Refresh),
        TrayState::Offline => None,
        TrayState::ServiceOwned => Some(Icon::Shield),
    }
}

/// 托盘态 → 状态圆点颜色。
fn state_color(p: &Palette, state: TrayState) -> Color32 {
    match state {
        TrayState::Connected => p.ok,
        TrayState::Reconnecting | TrayState::ServiceOwned => p.warn,
        TrayState::Offline => p.text_disabled,
    }
}

/// 主卡片：左侧大号连接状态 + 服务器/客户端信息，右侧大号延迟与 60 秒趋势线。
fn hero_card(ui: &mut Ui, status: &ClientStatus, state: TrayState, history: &StatusHistory) {
    widgets::card(ui, |ui| {
        let avail = ui.available_width();
        let spacing = ui.spacing().item_spacing.x;
        // 延迟列给足趋势线宽度：120 只够放下「12 ms」，线段会缩成一枚点。
        let right_w = (avail * 0.36).clamp(160.0, 240.0);
        let left_w = (avail - right_w - spacing).max(120.0);
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                Vec2::new(left_w, 96.0),
                Layout::top_down(Align::Min),
                |ui| {
                    ui.set_width(left_w);
                    hero_status(ui, state);
                    ui.add_space(8.0);
                    hero_server(ui, status);
                    ui.add_space(2.0);
                    hero_meta(ui, status);
                },
            );
            ui.allocate_ui_with_layout(
                Vec2::new(right_w, 96.0),
                Layout::top_down(Align::Max),
                |ui| {
                    ui.set_width(right_w);
                    hero_latency(ui, status, history, right_w);
                },
            );
        });
    });
}

/// 主卡片左侧第一行：14px 圆点 + 22px 状态文字。
fn hero_status(ui: &mut Ui, state: TrayState) {
    let p = palette(ui.style());
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
        ui.painter()
            .circle_filled(rect.center(), 6.0, state_color(&p, state));
        ui.label(
            RichText::new(state_label(state))
                .font(icons::heading_font(22.0))
                .color(p.text),
        );
    });
}

/// 主卡片左侧第二行：服务器地址（等宽弱色）。
fn hero_server(ui: &mut Ui, status: &ClientStatus) {
    let p = palette(ui.style());
    ui.horizontal(|ui| {
        icons::icon(ui, Icon::Server, 13.0, p.text_weak);
        let server = if status.server.is_empty() {
            "未配置服务器"
        } else {
            status.server.as_str()
        };
        ui.label(
            RichText::new(server)
                .monospace()
                .size(13.0)
                .color(p.text_weak),
        );
    });
}

/// 主卡片左侧第三行：客户端名 / 版本 / 映射规则条数（拿不到的部分直接省略）。
fn hero_meta(ui: &mut Ui, status: &ClientStatus) {
    let p = palette(ui.style());
    let mut meta = String::new();
    if !status.client_name.is_empty() {
        meta.push_str(&status.client_name);
    }
    if !status.version.is_empty() {
        if !meta.is_empty() {
            meta.push_str(" · ");
        }
        let version = status.version.trim_start_matches('v');
        meta.push('v');
        meta.push_str(version);
    }
    if let Some(summary) = status.mapping_summary.as_ref() {
        if !meta.is_empty() {
            meta.push_str(" · ");
        }
        meta.push_str("映射规则 ");
        meta.push_str(&summary.rules.len().to_string());
        meta.push_str(" 条");
    }
    if meta.is_empty() {
        return;
    }
    ui.label(RichText::new(meta).size(12.0).color(p.text_weak));
}

/// 主卡片右侧：32px 延迟数字 + 近 60 秒 RTT 趋势线；未知时显示「—」。
fn hero_latency(ui: &mut Ui, status: &ClientStatus, history: &StatusHistory, width: f32) {
    let p = palette(ui.style());
    ui.with_layout(Layout::top_down(Align::Max), |ui| {
        ui.set_width(width);
        ui.label(RichText::new("延迟").size(12.0).color(p.text_weak));
        ui.horizontal(|ui| {
            let (text, color) = match status.rtt_ms {
                Some(ms) => (format!("{ms:.0}"), p.text),
                None => ("—".to_owned(), p.text_disabled),
            };
            ui.label(
                RichText::new(text)
                    .font(icons::heading_font(32.0))
                    .color(color),
            );
            if status.rtt_ms.is_some() {
                ui.label(RichText::new("ms").size(13.0).color(p.text_weak));
            }
        });
        ui.add_space(4.0);
        ui.label(RichText::new("近 60 秒 RTT").size(11.5).color(p.text_weak));
        let spark_w = width.max(80.0);
        widgets::sparkline(ui, history.rtt(), Vec2::new(spark_w, 34.0), p.accent);
    });
}

/// 四个等宽统计块：延迟（趋势）、活跃隧道（趋势）、等待拨号、在线时长。
///
/// 正常宽度下一行 4 列；窗口被拖到最窄（480 减去左侧导航后不足）时退化成 2×2，
/// 否则 4 列会被压到读不出数字。
fn stat_row(ui: &mut Ui, status: &ClientStatus, history: &StatusHistory) {
    let gap = ui.spacing().item_spacing.x;
    let avail = ui.available_width();
    let columns = if avail >= 4.0 * MIN_STAT_TILE_WIDTH + 3.0 * gap {
        4.0
    } else {
        2.0
    };
    let width = ((avail - gap * (columns - 1.0)) / columns).max(MIN_STAT_TILE_WIDTH);
    let (rtt_value, rtt_unit) = match status.rtt_ms {
        Some(ms) => (format!("{ms:.0}"), "ms"),
        None => ("—".to_owned(), ""),
    };
    let uptime = status
        .connected_at
        .and_then(|at| at.elapsed().ok())
        .map_or_else(|| "—".to_owned(), format_uptime);
    ui.horizontal_wrapped(|ui| {
        stat_tile(
            ui,
            width,
            Icon::Gauge,
            "延迟",
            &rtt_value,
            rtt_unit,
            Some(history.rtt()),
        );
        stat_tile(
            ui,
            width,
            Icon::Route,
            "活跃隧道",
            &status.active_tunnels.to_string(),
            "条",
            Some(history.tunnels()),
        );
        stat_tile(
            ui,
            width,
            Icon::Refresh,
            "等待拨号",
            &status.pending_tunnels.to_string(),
            "条",
            None,
        );
        stat_tile(ui, width, Icon::Clock, "在线时长", &uptime, "", None);
    });
}

/// 在线时长：`42s` / `5m 12s` / `1h 2m` / `2d 3h`。
fn format_uptime(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m {}s", secs % 60);
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h {}m", mins % 60);
    }
    format!("{}d {}h", hours / 24, hours % 24)
}

/// 最近隧道卡片：标题 + 条数，逐行展示 id / 目标地址 / 相对时间。
fn tunnels_card(ui: &mut Ui, status: &ClientStatus) {
    let p = palette(ui.style());
    widgets::card(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("最近隧道")
                    .font(icons::heading_font(16.0))
                    .color(p.text),
            );
            widgets::right_row(ui, |ui| {
                let count = format!("{} 条", status.recent_tunnels.len());
                badge(ui, &count, Tone::Neutral, None);
            });
        });
        ui.add_space(8.0);
        if status.recent_tunnels.is_empty() {
            widgets::empty_state(
                ui,
                Icon::Route,
                "暂无隧道活动",
                "客户端发起或接受隧道后会显示在这里",
            );
            return;
        }
        // 只给视口 8 行高；更多的历史记录在卡片内滚动。行高 = 行内容 + 分隔线 + 上下间距。
        let row_h = 18.0 + 1.0 + ui.spacing().item_spacing.y * 2.0;
        let max_height = row_h * RECENT_TUNNEL_ROWS as f32;
        egui::ScrollArea::vertical()
            .id_salt("status_recent_tunnels")
            .max_height(max_height)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                let total = status.recent_tunnels.len();
                for (index, (id, addr, at)) in status.recent_tunnels.iter().enumerate() {
                    tunnel_row(ui, *id, addr, relative_time(*at));
                    if index + 1 < total {
                        widgets::divider(ui);
                    }
                }
            });
    });
}

/// 一条隧道记录：`#id`（弱色）+ 目标地址（等宽）+ 右对齐的相对时间。
fn tunnel_row(ui: &mut Ui, id: u64, addr: &str, when: String) {
    let p = palette(ui.style());
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("#{id}"))
                .monospace()
                .size(11.5)
                .color(p.text_weak),
        );
        ui.label(RichText::new(addr).monospace().size(12.5).color(p.text));
        widgets::right_row_sized(ui, 18.0, |ui| {
            ui.label(RichText::new(when).size(11.5).color(p.text_weak));
        });
    });
}

/// 相对时间（`3s 前` / `2m 前` / `1h 前` / `2d 前`）；系统时钟回拨时退化为「—」。
fn relative_time(at: SystemTime) -> String {
    let Ok(elapsed) = at.elapsed() else {
        return "—".to_owned();
    };
    let secs = elapsed.as_secs();
    // 逐档早退（而不是 if/else-if 链）：比较链上同一个值会命中 `comparison_chain`。
    if secs < 60 {
        return format!("{secs}s 前");
    }
    if secs < 3600 {
        return format!("{}m 前", secs / 60);
    }
    if secs < 86_400 {
        return format!("{}h 前", secs / 3600);
    }
    format!("{}d 前", secs / 86_400)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use rust_tunnel_client::ClientStatus;

    use super::{StatusHistory, HISTORY_CAPACITY, SAMPLE_PERIOD};

    fn connected(rtt_ms: f64, tunnels: usize) -> ClientStatus {
        let mut status = ClientStatus::new(
            "srv:3000".to_owned(),
            "home-nas".to_owned(),
            "0.18.6".to_owned(),
        );
        status.connected = true;
        status.rtt_ms = Some(rtt_ms);
        status.active_tunnels = tunnels;
        status
    }

    /// 逐点比较（不直接对 f32 用 `==`）。
    fn approx(values: &[Option<f32>], expected: &[Option<f32>]) -> bool {
        values.len() == expected.len()
            && values
                .iter()
                .zip(expected)
                .all(|(got, want)| match (got, want) {
                    (Some(got), Some(want)) => (*got - *want).abs() < f32::EPSILON,
                    (None, None) => true,
                    _ => false,
                })
    }

    #[test]
    fn samples_about_once_per_second() {
        let mut history = StatusHistory::default();
        let t0 = Instant::now();
        history.sample(&connected(12.0, 2), t0 + Duration::from_millis(400));
        assert!(history.rtt().is_empty(), "不足一个采样周期不应写入");

        history.sample(&connected(12.0, 2), t0 + SAMPLE_PERIOD);
        assert!(approx(history.rtt(), &[Some(12.0)]));
        assert!(approx(history.tunnels(), &[Some(2.0)]));
    }

    #[test]
    fn missed_seconds_become_none_breakpoints() {
        let mut history = StatusHistory::default();
        let t0 = Instant::now();
        history.sample(&connected(20.0, 1), t0 + SAMPLE_PERIOD);
        // 中间隔了整整一秒没有采样：趋势线应有一个断点，而不是把两点直接连起来。
        history.sample(&connected(21.0, 3), t0 + SAMPLE_PERIOD * 3);
        assert!(approx(history.rtt(), &[Some(20.0), None, Some(21.0)]));
        assert!(approx(history.tunnels(), &[Some(1.0), None, Some(3.0)]));
    }

    #[test]
    fn history_is_capped_and_stays_contiguous() {
        let mut history = StatusHistory::default();
        let t0 = Instant::now();
        for i in 1..=(HISTORY_CAPACITY + 5) {
            history.sample(&connected(10.0, 1), t0 + SAMPLE_PERIOD * i as u32);
        }
        // 环形缓冲回绕后仍必须交出完整切片（`rtt()` 只取第一段）。
        assert_eq!(history.rtt().len(), HISTORY_CAPACITY);
        assert_eq!(history.tunnels().len(), HISTORY_CAPACITY);
        assert!(history.rtt().iter().all(Option::is_some));
    }

    #[test]
    fn long_pause_clears_instead_of_backfilling() {
        let mut history = StatusHistory::default();
        let t0 = Instant::now();
        history.sample(&connected(10.0, 1), t0 + SAMPLE_PERIOD);
        let far = t0 + SAMPLE_PERIOD * (HISTORY_CAPACITY as u32 + 2);
        history.sample(&connected(11.0, 2), far);
        assert!(
            approx(history.rtt(), &[Some(11.0)]),
            "挂起超过容量后只保留当前值"
        );
    }

    #[test]
    fn disconnected_samples_are_breakpoints() {
        let mut history = StatusHistory::default();
        let t0 = Instant::now();
        let mut offline = connected(5.0, 4);
        offline.connected = false;
        history.sample(&offline, t0 + SAMPLE_PERIOD);
        assert!(approx(history.rtt(), &[None]));
        assert!(approx(history.tunnels(), &[None]));
    }
}
