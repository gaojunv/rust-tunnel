//! 主题感知的小部件库：Fluent 风的导航项、卡片、开关、按钮、徽标、趋势线等。
//!
//! 所有组件都只读 `ui.style()/ui.visuals()`（经 [`palette`]），不持有状态，也不依赖具体页面。

#![allow(
    clippy::cast_precision_loss,
    reason = "UI 尺寸与采样点在 f32 范围内的精度损失不可感知"
)]

use egui::{
    Align, Color32, CornerRadius, FontFamily, FontId, Layout, Pos2, Rect, Response, RichText,
    Sense, Stroke, StrokeKind, Ui, Vec2,
};

use super::icons::{self, Icon};
use super::theme::{card_frame, palette, Palette};

/// 表单标签列的固定宽度。
pub const LABEL_WIDTH: f32 = 96.0;

/// 语义色调。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// 中性（灰）。
    Neutral,
    /// 强调（系统强调色）。
    Accent,
    /// 成功。
    Ok,
    /// 警告。
    Warn,
    /// 错误。
    Err,
}

/// 色调对应的（前景, 淡背景）颜色。
#[must_use]
pub fn tone_colors(p: &Palette, tone: Tone) -> (Color32, Color32) {
    let fg = match tone {
        Tone::Neutral => p.text_weak,
        Tone::Accent => p.accent,
        Tone::Ok => p.ok,
        Tone::Warn => p.warn,
        Tone::Err => p.err,
    };
    let bg = Color32::from_rgba_unmultiplied(fg.r(), fg.g(), fg.b(), if p.dark { 40 } else { 26 });
    (fg, bg)
}

/// 页面标题行：左侧标题 + 副标题，右侧动作区。
pub fn page_header(ui: &mut Ui, title: &str, subtitle: &str, right: impl FnOnce(&mut Ui)) {
    let p = palette(ui.style());
    // **必须给这一行固定高度**：`with_layout`/`allocate_new_ui` 会拿到父级剩余的全部高度，
    // 在垂直布局里直接用会把页头撑满整页（后续内容被推到窗口外）。这里先分配一条 54px 的
    // 横向带，左右两侧都在这条带内排布。
    let height = if subtitle.is_empty() { 36.0 } else { 54.0 };
    let width = ui.available_width();
    ui.allocate_ui_with_layout(
        Vec2::new(width, height),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new(title)
                        .font(icons::heading_font(26.0))
                        .color(p.text),
                );
                if !subtitle.is_empty() {
                    ui.add_space(2.0);
                    ui.label(RichText::new(subtitle).size(12.5).color(p.text_weak));
                }
            });
            right_row(ui, right);
        },
    );
    ui.add_space(14.0);
}

/// 在一行内右对齐一组控件，高度固定为 32。
///
/// **不要用裸 `ui.with_layout(...)` 做右对齐**：`with_layout` 会拿到父级剩余的全部高度，
/// 在卡片/页面的垂直布局里用会把整行撑到几百像素高（后续内容被推到窗口外）。
pub fn right_row<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    right_row_sized(ui, 32.0, add)
}

/// 同 [`right_row`]，可指定行高（工具条 26、日志行 18 等紧凑场景）。
pub fn right_row_sized<R>(ui: &mut Ui, height: f32, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.allocate_ui_with_layout(
        Vec2::new(ui.available_width().max(0.0), height),
        Layout::right_to_left(Align::Center),
        add,
    )
    .inner
}

/// 卡片容器。
pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    card_frame(ui.style()).show(ui, add).inner
}

/// 带标题的卡片。
pub fn card_titled<R>(
    ui: &mut Ui,
    title: &str,
    subtitle: Option<&str>,
    add: impl FnOnce(&mut Ui) -> R,
) -> R {
    card(ui, |ui| {
        card_header(ui, title, subtitle);
        ui.add_space(8.0);
        add(ui)
    })
}

/// 卡片标题行（`subtitle` 用弱色小字）。
pub fn card_header(ui: &mut Ui, title: &str, subtitle: Option<&str>) {
    let p = palette(ui.style());
    ui.label(
        RichText::new(title)
            .font(icons::heading_font(16.0))
            .color(p.text),
    );
    if let Some(subtitle) = subtitle {
        ui.add_space(2.0);
        ui.label(RichText::new(subtitle).size(12.0).color(p.text_weak));
    }
}

/// 左侧导航项（图标 + 文字）；返回点击响应。
pub fn nav_item(ui: &mut Ui, icon: Icon, label: &str, selected: bool) -> Response {
    let p = palette(ui.style());
    let width = ui.available_width().max(80.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 40.0), Sense::click());
    let row = rect.shrink2(Vec2::new(0.0, 2.0));
    if resp.hovered() && !selected {
        ui.painter()
            .rect_filled(row, CornerRadius::same(7), p.surface_alt);
    }
    if selected {
        ui.painter()
            .rect_filled(row, CornerRadius::same(7), p.accent_dim);
        let bar = Rect::from_min_size(
            Pos2::new(row.left() + 1.0, row.top() + 9.0),
            Vec2::new(3.0, row.height() - 18.0),
        );
        ui.painter()
            .rect_filled(bar, CornerRadius::same(2), p.accent);
    }
    let icon_color = if selected { p.accent } else { p.text_weak };
    let icon_rect = Rect::from_min_size(
        Pos2::new(rect.left() + 17.0, rect.center().y - 9.0),
        Vec2::splat(18.0),
    );
    icons::paint(ui.painter(), icon_rect, icon, icon_color, 1.6);
    let font = FontId::new(14.0, FontFamily::Proportional);
    ui.painter().text(
        Pos2::new(rect.left() + 45.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        font,
        p.text,
    );
    resp
}

/// 统计块：图标 + 标签 + 大数字 + 可选趋势线。
pub fn stat_tile(
    ui: &mut Ui,
    width: f32,
    icon: Icon,
    label: &str,
    value: &str,
    unit: &str,
    spark: Option<&[Option<f32>]>,
) {
    let p = palette(ui.style());
    // 卡片外框的总宽 = 内容 + 左右内边距(32) + 左右描边(1×2)。统计块要求「总宽正好等于
    // 传进来的 width」，所以内容宽度必须把这 34px 扣掉；否则每个卡片会比列宽多 2px，
    // 带趋势线的第一行与第二行就会互相挤开、左右边缘对不齐。
    let content_w = (width - 34.0).max(48.0);
    ui.allocate_ui_with_layout(
        Vec2::new(width, 108.0),
        Layout::top_down(Align::Min),
        |ui| {
            card_frame(ui.style()).show(ui, |ui| {
                ui.set_width(content_w);
                ui.horizontal(|ui| {
                    icons::icon(ui, icon, 15.0, p.text_weak);
                    ui.label(RichText::new(label).size(12.0).color(p.text_weak));
                });
                ui.add_space(1.0);
                ui.horizontal(|ui| {
                    // 窄卡片（2×2 档）用小一号数字，避免 `2h 14m` 把卡片撑出可视区。
                    let value_size = if width < 170.0 { 22.0 } else { 26.0 };
                    ui.label(
                        RichText::new(value)
                            .font(icons::heading_font(value_size))
                            .color(p.text),
                    );
                    if !unit.is_empty() {
                        ui.label(RichText::new(unit).size(12.0).color(p.text_weak));
                    }
                });
                ui.add_space(2.0);
                match spark {
                    Some(values) => {
                        // 趋势线固定在卡片内容宽度内，不按 `available_width()` 取宽：
                        // 后者在窄列里可能比内容宽度大，越界后会把相邻统计块挤开。
                        sparkline(ui, values, Vec2::new(content_w, 22.0), p.accent);
                    }
                    None => {
                        ui.add_space(22.0);
                    }
                }
            });
        },
    );
}

/// 迷你趋势线：只画数值序列（`None` 表示断点），带一层淡淡的面填充。
pub fn sparkline(ui: &mut Ui, values: &[Option<f32>], size: Vec2, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    if values.len() < 2 {
        return;
    }
    let finite: Vec<f32> = values.iter().flatten().copied().collect();
    if finite.len() < 2 {
        return;
    }
    let min = finite.iter().copied().fold(f32::INFINITY, f32::min);
    let mut max = finite.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    if (max - min).abs() < 1e-3 {
        max = min + 1.0;
    }
    let last = (values.len() - 1) as f32;
    let x_at = |i: usize| rect.left() + rect.width() * (i as f32) / last;
    let y_at = |v: f32| rect.bottom() - (v - min) / (max - min) * rect.height();
    let bottom = rect.bottom();

    let fill = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 38);
    let mut mesh = egui::epaint::Mesh::default();
    for i in 0..values.len() - 1 {
        let (Some(a), Some(b)) = (values[i], values[i + 1]) else {
            continue;
        };
        let (x0, x1) = (x_at(i), x_at(i + 1));
        let (y0, y1) = (y_at(a), y_at(b));
        let Ok(base) = u32::try_from(mesh.vertices.len()) else {
            break;
        };
        for pos in [
            Pos2::new(x0, y0),
            Pos2::new(x1, y1),
            Pos2::new(x1, bottom),
            Pos2::new(x0, bottom),
        ] {
            mesh.vertices.push(egui::epaint::Vertex {
                pos,
                uv: egui::epaint::WHITE_UV,
                color: fill,
            });
        }
        mesh.add_triangle(base, base + 1, base + 2);
        mesh.add_triangle(base, base + 2, base + 3);
    }
    if !mesh.vertices.is_empty() {
        ui.painter().add(egui::Shape::mesh(mesh));
    }
    for i in 0..values.len() - 1 {
        let (Some(a), Some(b)) = (values[i], values[i + 1]) else {
            continue;
        };
        ui.painter().line_segment(
            [Pos2::new(x_at(i), y_at(a)), Pos2::new(x_at(i + 1), y_at(b))],
            Stroke::new(1.4_f32, color),
        );
    }
}

/// Fluent 开关（独立控件）。
pub fn toggle(ui: &mut Ui, value: &mut bool) -> Response {
    let (rect, mut resp) = ui.allocate_exact_size(Vec2::new(40.0, 22.0), Sense::click());
    if resp.clicked() {
        *value = !*value;
        resp.mark_changed();
    }
    let p = palette(ui.style());
    paint_toggle(ui.painter(), rect, *value, resp.hovered(), &p);
    resp
}

/// 开关 + 标题/说明的整行（整行可点）。
pub fn switch_row(ui: &mut Ui, title: &str, subtitle: Option<&str>, value: &mut bool) -> bool {
    let p = palette(ui.style());
    let width = ui.available_width().max(120.0);
    let text_width = (width - 72.0).max(60.0);
    // 先量后分配：副标题必须换行（远程页的「自动锁屏」说明有两行），固定行高会把它裁掉。
    let (title_galley, sub_galley) = {
        let painter = ui.painter();
        let title_font = FontId::new(14.0, FontFamily::Proportional);
        let sub_font = FontId::new(11.5, FontFamily::Proportional);
        let title_galley = painter.layout_no_wrap(title.to_owned(), title_font, p.text);
        let sub_galley =
            subtitle.map(|text| painter.layout(text.to_owned(), sub_font, p.text_weak, text_width));
        (title_galley, sub_galley)
    };
    let title_h = title_galley.size().y;
    let sub_h = sub_galley.as_ref().map_or(0.0, |galley| galley.size().y);
    let height =
        (9.0 + title_h + if subtitle.is_some() { 3.0 + sub_h } else { 0.0 } + 9.0).max(44.0);
    let (rect, mut resp) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
    let row = rect.shrink2(Vec2::new(0.0, 1.0));
    if resp.hovered() {
        ui.painter()
            .rect_filled(row, CornerRadius::same(7), p.surface_alt);
    }
    let text_x = rect.left() + 10.0;
    let top = rect.top() + 9.0;
    ui.painter()
        .galley(Pos2::new(text_x, top), title_galley, p.text);
    if let Some(galley) = sub_galley {
        ui.painter()
            .galley(Pos2::new(text_x, top + title_h + 3.0), galley, p.text_weak);
    }
    let track = Rect::from_center_size(
        Pos2::new(rect.right() - 34.0, rect.center().y),
        Vec2::new(40.0, 22.0),
    );
    paint_toggle(ui.painter(), track, *value, resp.hovered(), &p);
    if resp.clicked() {
        *value = !*value;
        resp.mark_changed();
    }
    resp.changed()
}

fn paint_toggle(painter: &egui::Painter, rect: Rect, on: bool, hovered: bool, p: &Palette) {
    let track = if on {
        if hovered {
            p.accent_hover
        } else {
            p.accent
        }
    } else if hovered {
        if p.dark {
            Color32::from_rgb(0x6A, 0x6A, 0x6A)
        } else {
            Color32::from_rgb(0xB0, 0xB0, 0xB0)
        }
    } else if p.dark {
        Color32::from_rgb(0x5C, 0x5C, 0x5C)
    } else {
        Color32::from_rgb(0xC6, 0xC6, 0xC6)
    };
    painter.rect_filled(rect, CornerRadius::same(11), track);
    let knob_radius = 7.0;
    let knob_x = if on {
        rect.right() - 11.0
    } else {
        rect.left() + 11.0
    };
    painter.circle_filled(
        Pos2::new(knob_x, rect.center().y),
        knob_radius,
        if on { p.on_accent } else { Color32::WHITE },
    );
}

/// 按钮种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonKind {
    /// 强调色填充（主操作）。
    Primary,
    /// 描边（次操作）。
    Secondary,
    /// 无底（低权重）。
    Subtle,
    /// 破坏性操作用错误色。
    Danger,
}

/// 通用按钮：可选前置图标，悬停/按下态由自己绘制。
pub fn button(ui: &mut Ui, label: &str, icon: Option<Icon>, kind: ButtonKind) -> Response {
    button_sized(ui, label, icon, kind, 32.0, 14.0)
}

/// 指定高度与字号的按钮（会话工具条用更紧凑的一档）。
pub fn button_sized(
    ui: &mut Ui,
    label: &str,
    icon: Option<Icon>,
    kind: ButtonKind,
    height: f32,
    font_size: f32,
) -> Response {
    let p = palette(ui.style());
    let font = FontId::new(font_size, FontFamily::Proportional);
    let galley = ui.painter().layout_no_wrap(label.to_owned(), font, p.text);
    let icon_w = if icon.is_some() { 16.0 + 8.0 } else { 0.0 };
    let pad = if height <= 26.0 { 10.0 } else { 14.0 };
    let size = Vec2::new(galley.size().x + icon_w + pad * 2.0, height);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let hovered = resp.hovered();
    let (bg, stroke, fg) = match kind {
        ButtonKind::Primary => (
            if hovered { p.accent_hover } else { p.accent },
            Stroke::NONE,
            p.on_accent,
        ),
        ButtonKind::Secondary => (
            if hovered {
                p.surface_alt
            } else {
                p.surface_raised
            },
            Stroke::new(1.0_f32, if hovered { p.stroke_strong } else { p.stroke }),
            p.text,
        ),
        ButtonKind::Subtle => (
            if hovered {
                p.surface_alt
            } else {
                Color32::TRANSPARENT
            },
            Stroke::NONE,
            p.text,
        ),
        ButtonKind::Danger => (
            if hovered {
                Color32::from_rgba_unmultiplied(p.err.r(), p.err.g(), p.err.b(), 32)
            } else {
                Color32::TRANSPARENT
            },
            Stroke::NONE,
            p.err,
        ),
    };
    let radius = CornerRadius::same(6);
    if bg != Color32::TRANSPARENT {
        ui.painter().rect_filled(rect, radius, bg);
    }
    if stroke != Stroke::NONE {
        ui.painter()
            .rect_stroke(rect, radius, stroke, StrokeKind::Inside);
    }
    let content_w = galley.size().x + icon_w;
    let mut x = rect.center().x - content_w / 2.0;
    if let Some(icon) = icon {
        let icon_rect = Rect::from_min_size(Pos2::new(x, rect.center().y - 8.0), Vec2::splat(16.0));
        icons::paint(ui.painter(), icon_rect, icon, fg, 1.6);
        x += 24.0;
    }
    ui.painter().galley(
        Pos2::new(x, rect.center().y - galley.size().y / 2.0),
        galley,
        fg,
    );
    resp
}

/// 主操作按钮。
pub fn primary_button(ui: &mut Ui, label: &str) -> Response {
    button(ui, label, None, ButtonKind::Primary)
}

/// 次操作按钮。
pub fn secondary_button(ui: &mut Ui, label: &str) -> Response {
    button(ui, label, None, ButtonKind::Secondary)
}

/// 图标按钮（28×28，悬停出提示）。
pub fn icon_button(ui: &mut Ui, icon: Icon, tip: &str) -> Response {
    icon_button_colored(ui, icon, tip, None)
}

/// 可指定颜色的图标按钮（危险操作用错误色）。
pub fn icon_button_colored(ui: &mut Ui, icon: Icon, tip: &str, color: Option<Color32>) -> Response {
    let p = palette(ui.style());
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::click());
    if resp.hovered() {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(6), p.surface_alt);
    }
    let icon_rect = Rect::from_center_size(rect.center(), Vec2::splat(17.0));
    icons::paint(
        ui.painter(),
        icon_rect,
        icon,
        color.unwrap_or(if resp.hovered() { p.text } else { p.text_weak }),
        1.6,
    );
    resp.on_hover_text(tip)
}

/// 工具条图标按钮。
pub fn toolbar_icon(ui: &mut Ui, icon: Icon, tip: &str) -> Response {
    toolbar_icon_colored(ui, icon, tip, None)
}

/// 工具条图标按钮（可指定颜色）。
pub fn toolbar_icon_colored(
    ui: &mut Ui,
    icon: Icon,
    tip: &str,
    color: Option<Color32>,
) -> Response {
    let p = palette(ui.style());
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(26.0), Sense::click());
    if resp.hovered() {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(6), p.surface_alt);
    }
    let icon_rect = Rect::from_center_size(rect.center(), Vec2::splat(16.0));
    icons::paint(
        ui.painter(),
        icon_rect,
        icon,
        color.unwrap_or(if resp.hovered() { p.text } else { p.text_weak }),
        1.6,
    );
    resp.on_hover_text(tip)
}

/// 工具条切换按钮（文件/全屏这类开关）。
pub fn toolbar_toggle(ui: &mut Ui, icon: Icon, active: bool, tip: &str) -> Response {
    let p = palette(ui.style());
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(26.0), Sense::click());
    if active {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(6), p.accent_dim);
    } else if resp.hovered() {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(6), p.surface_alt);
    }
    let icon_rect = Rect::from_center_size(rect.center(), Vec2::splat(16.0));
    icons::paint(
        ui.painter(),
        icon_rect,
        icon,
        if active { p.accent } else { p.text_weak },
        1.6,
    );
    resp.on_hover_text(tip)
}

/// 分段控件（Fluent 的 pill 组）。
pub fn segmented<T: Copy + PartialEq>(ui: &mut Ui, current: T, options: &[(T, &str)]) -> Option<T> {
    let p = palette(ui.style());
    let font = FontId::new(13.0, FontFamily::Proportional);
    let galleys: Vec<_> = options
        .iter()
        .map(|(_, label)| {
            ui.painter()
                .layout_no_wrap((*label).to_owned(), font.clone(), p.text)
        })
        .collect();
    let pad = 12.0;
    let height = 32.0;
    // 每段之间留 2px：否则相邻选项的文字会连成一片（「全部信息警告…」），读不出边界。
    let gap = 2.0;
    let widths: Vec<f32> = galleys
        .iter()
        .map(|g| g.size().x + pad * 2.0 + gap)
        .collect();
    let total: f32 = widths.iter().sum::<f32>() - gap;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(total, height), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(8), p.surface_alt);
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(8),
        Stroke::new(1.0_f32, p.stroke),
        StrokeKind::Inside,
    );
    let mut x = rect.left();
    let mut picked = None;
    for (i, (value, label)) in options.iter().enumerate() {
        let seg = Rect::from_min_size(Pos2::new(x, rect.top()), Vec2::new(widths[i], height));
        let selected = *value == current;
        let resp = ui.interact(seg, ui.id().with(("segmented", i, *label)), Sense::click());
        if selected {
            let pill = seg.shrink2(Vec2::new(2.0, 3.0));
            ui.painter()
                .rect_filled(pill, CornerRadius::same(6), p.surface);
            ui.painter().rect_stroke(
                pill,
                CornerRadius::same(6),
                Stroke::new(1.0_f32, p.stroke),
                StrokeKind::Inside,
            );
        } else if resp.hovered() {
            ui.painter().rect_filled(
                seg.shrink2(Vec2::new(2.0, 3.0)),
                CornerRadius::same(6),
                p.surface_raised,
            );
        }
        let galley = galleys[i].clone();
        ui.painter().galley(
            Pos2::new(
                seg.center().x - galley.size().x / 2.0,
                seg.center().y - galley.size().y / 2.0,
            ),
            galley,
            if selected { p.text } else { p.text_weak },
        );
        if resp.clicked() && !selected {
            picked = Some(*value);
        }
        x += widths[i];
    }
    picked
}

/// 小号着色胶囊标签（非交互）。
pub fn pill(ui: &mut Ui, text: &str, fg: Color32, bg: Color32) {
    let font = egui::TextStyle::Small.resolve(ui.style());
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, fg);
    let padding = Vec2::new(8.0, 3.0);
    let (rect, _) = ui.allocate_exact_size(galley.size() + padding * 2.0, Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(7), bg);
    ui.painter().galley(rect.min + padding, galley, fg);
}

/// 语义徽标（可带图标）。
pub fn badge(ui: &mut Ui, text: &str, tone: Tone, icon: Option<Icon>) {
    let p = palette(ui.style());
    let (fg, bg) = tone_colors(&p, tone);
    let font = egui::TextStyle::Small.resolve(ui.style());
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, fg);
    let icon_w = if icon.is_some() { 15.0 } else { 0.0 };
    let pad = Vec2::new(8.0, 3.0);
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(
            galley.size().x + icon_w + pad.x * 2.0,
            galley.size().y + pad.y * 2.0,
        ),
        Sense::hover(),
    );
    ui.painter().rect_filled(rect, CornerRadius::same(7), bg);
    let mut x = rect.left() + pad.x;
    if let Some(icon) = icon {
        let icon_rect = Rect::from_min_size(Pos2::new(x, rect.center().y - 7.5), Vec2::splat(15.0));
        icons::paint(ui.painter(), icon_rect, icon, fg, 1.6);
        x += 15.0;
    }
    ui.painter().galley(
        Pos2::new(x, rect.center().y - galley.size().y / 2.0),
        galley,
        fg,
    );
}

/// 8px 状态圆点 + 文本。
pub fn status_dot(ui: &mut Ui, color: Color32, label: &str) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
        ui.painter().circle_filled(rect.center(), 4.0, color);
        ui.label(label);
    });
}

/// 信息条：淡色底 + 左侧图标 + 标题/正文。
pub fn info_bar(ui: &mut Ui, tone: Tone, title: Option<&str>, body: &str) {
    info_bar_custom(ui, tone, |ui| {
        let p = palette(ui.style());
        if let Some(title) = title {
            ui.label(
                RichText::new(title)
                    .font(icons::heading_font(13.5))
                    .color(p.text),
            );
            ui.add_space(2.0);
        }
        ui.label(RichText::new(body).size(12.0).color(p.text_weak));
    });
}

/// 自定义内容的信息条（图标按色调选择）。
pub fn info_bar_custom(ui: &mut Ui, tone: Tone, add: impl FnOnce(&mut Ui)) {
    let p = palette(ui.style());
    let (fg, bg) = tone_colors(&p, tone);
    let icon = match tone {
        Tone::Ok => Icon::Check,
        Tone::Warn | Tone::Err => Icon::Warning,
        Tone::Accent | Tone::Neutral => Icon::Info,
    };
    egui::Frame::new()
        .fill(bg)
        .stroke(Stroke::new(
            1.0_f32,
            Color32::from_rgba_unmultiplied(fg.r(), fg.g(), fg.b(), 70),
        ))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
                icons::paint(
                    ui.painter(),
                    Rect::from_center_size(rect.center(), Vec2::splat(18.0)),
                    icon,
                    fg,
                    1.6,
                );
                ui.add_space(2.0);
                ui.vertical(|ui| add(ui));
            });
        });
    ui.add_space(8.0);
}

/// 搜索框（左侧放大镜图标）。
pub fn search_field(ui: &mut Ui, text: &mut String, hint: &str) -> Response {
    let p = palette(ui.style());
    let height = 30.0;
    let width = ui.available_width().clamp(120.0, 260.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(6), p.surface_raised);
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(6),
        Stroke::new(1.0_f32, p.stroke),
        StrokeKind::Inside,
    );
    let icon_rect = Rect::from_center_size(
        Pos2::new(rect.left() + 16.0, rect.center().y),
        Vec2::splat(15.0),
    );
    icons::paint(ui.painter(), icon_rect, Icon::Search, p.text_weak, 1.6);
    let edit_rect = Rect::from_min_max(
        Pos2::new(rect.left() + 30.0, rect.top() + 3.0),
        Pos2::new(rect.right() - 6.0, rect.bottom() - 3.0),
    );
    ui.put(
        edit_rect,
        egui::TextEdit::singleline(text)
            .frame(false)
            .hint_text(hint)
            .desired_width(edit_rect.width()),
    )
}

/// 空状态：图标 + 标题 + 说明，垂直居中。
pub fn empty_state(ui: &mut Ui, icon: Icon, title: &str, hint: &str) {
    let p = palette(ui.style());
    ui.add_space(18.0);
    ui.vertical_centered(|ui| {
        icons::icon(ui, icon, 30.0, p.text_disabled);
        ui.add_space(6.0);
        ui.label(RichText::new(title).size(14.0).color(p.text_weak));
        if !hint.is_empty() {
            ui.label(RichText::new(hint).size(12.0).color(p.text_disabled));
        }
    });
    ui.add_space(18.0);
}

/// 水平分隔线。
pub fn divider(ui: &mut Ui) {
    let p = palette(ui.style());
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(ui.available_width().max(1.0), 1.0),
        Sense::hover(),
    );
    ui.painter().line_segment(
        [rect.left_center(), rect.right_center()],
        Stroke::new(1.0_f32, p.divider),
    );
}

/// 左侧固定宽度的小号弱色标签 + 右侧内容。
pub fn labeled_row(ui: &mut Ui, label: &str, body: impl FnOnce(&mut Ui)) {
    let p = palette(ui.style());
    ui.horizontal(|ui| {
        ui.add_sized(
            [LABEL_WIDTH, ui.spacing().interact_size.y],
            egui::Label::new(RichText::new(label).size(12.0).color(p.text_weak)),
        );
        body(ui);
    });
}

/// 键值行（用于摘要卡片）。
pub fn key_value(ui: &mut Ui, label: &str, value: &str, mono: bool) {
    let p = palette(ui.style());
    ui.horizontal(|ui| {
        ui.add_sized(
            [LABEL_WIDTH, 20.0],
            egui::Label::new(RichText::new(label).size(12.0).color(p.text_weak)),
        );
        let text = if mono {
            RichText::new(value).monospace().size(12.5).color(p.text)
        } else {
            RichText::new(value).size(13.0).color(p.text)
        };
        ui.label(text);
    });
}

/// 小节标题 + 内容 + 底部留白。
pub fn section(ui: &mut Ui, title: &str, body: impl FnOnce(&mut Ui)) {
    let p = palette(ui.style());
    ui.label(RichText::new(title).size(12.0).color(p.text_weak));
    ui.add_space(6.0);
    body(ui);
    ui.add_space(10.0);
}

/// 弹层标题行：左侧标题、右侧关闭图标；返回是否点了关闭。
pub fn modal_title(ui: &mut Ui, title: &str) -> bool {
    ui.horizontal(|ui| {
        let p = palette(ui.style());
        ui.label(
            RichText::new(title)
                .font(icons::heading_font(20.0))
                .color(p.text),
        );
        right_row(ui, |ui| icon_button(ui, Icon::Close, "关闭").clicked())
    })
    .inner
}

/// 可点击的胶囊按钮（会话工具条收起态：只显示延迟，点它展开）。
pub fn pill_button(ui: &mut Ui, text: &str, fg: Color32, tip: &str) -> Response {
    let p = palette(ui.style());
    let font = egui::TextStyle::Small.resolve(ui.style());
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, fg);
    let padding = Vec2::new(10.0, 5.0);
    let size = galley.size() + padding * 2.0;
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let bg = if resp.hovered() {
        p.surface_raised
    } else {
        p.surface
    };
    ui.painter().rect_filled(rect, CornerRadius::same(11), bg);
    ui.painter().galley(rect.min + padding, galley, fg);
    resp.on_hover_text(tip)
}

/// 主题按钮在导航栏里的形态：图标 + 标签，整行可点。
pub fn nav_theme_button(ui: &mut Ui, icon: Icon, label: &str, tip: &str) -> Response {
    let p = palette(ui.style());
    let width = ui.available_width().max(80.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 34.0), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(
            rect.shrink2(Vec2::new(0.0, 1.0)),
            CornerRadius::same(6),
            p.surface_alt,
        );
    }
    let icon_rect = Rect::from_min_size(
        Pos2::new(rect.left() + 17.0, rect.center().y - 8.0),
        Vec2::splat(16.0),
    );
    icons::paint(ui.painter(), icon_rect, icon, p.text_weak, 1.6);
    ui.painter().text(
        Pos2::new(rect.left() + 41.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        FontId::new(13.0, FontFamily::Proportional),
        p.text_weak,
    );
    resp.on_hover_text(tip)
}
