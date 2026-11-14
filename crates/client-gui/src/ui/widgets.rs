//! 主题感知的会话窗口小部件：全部读取 `ui.style()/ui.visuals()`，不持有状态。

use egui::{Color32, RichText, Stroke, TextStyle, Ui};

use super::theme::palette;

/// 小节标题 + 内容 + 底部留白。
pub fn section(ui: &mut Ui, title: &str, body: impl FnOnce(&mut Ui)) {
    let color = palette(ui.style()).text_weak;
    ui.label(RichText::new(title).strong().small().color(color));
    ui.add_space(6.0);
    body(ui);
    ui.add_space(10.0);
}

/// 标签列宽度（`labeled_row` 的固定左栏）。
pub const LABEL_WIDTH: f32 = 84.0;

/// 左侧固定宽度的小号弱色标签 + 右侧内容。
pub fn labeled_row(ui: &mut Ui, label: &str, body: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.add_sized(
            [LABEL_WIDTH, ui.spacing().interact_size.y],
            egui::Label::new(RichText::new(label).weak().small()),
        );
        body(ui);
    });
}

/// 一组圆角胶囊式选项；只有点到**不同**选项时才返回 `Some(新值)`。
#[must_use]
pub fn segmented<T: Copy + PartialEq>(ui: &mut Ui, current: T, options: &[(T, &str)]) -> Option<T> {
    let mut picked = None;
    let p = palette(ui.style());
    egui::Frame::new()
        .fill(p.surface_raised)
        .stroke(Stroke::new(1.0_f32, p.stroke))
        .corner_radius(egui::CornerRadius::same(9))
        .inner_margin(egui::Margin::symmetric(3, 3))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.horizontal(|ui| {
                for &(value, label) in options {
                    let selected = value == current;
                    let text =
                        RichText::new(label).color(if selected { p.text } else { p.text_weak });
                    if ui.add(egui::SelectableLabel::new(selected, text)).clicked() && !selected {
                        picked = Some(value);
                    }
                }
            });
        });
    picked
}

/// 紧凑工具栏按钮（悬停提示）。
#[must_use]
pub fn toolbar_button(ui: &mut Ui, label: &str, tip: &str) -> egui::Response {
    compact_button(ui, label, palette(ui.style()).text).on_hover_text(tip)
}

/// 同 [`toolbar_button`]，但文字用错误色（用于「断开」这类破坏性操作）。
#[must_use]
pub fn danger_button(ui: &mut Ui, label: &str, tip: &str) -> egui::Response {
    compact_button(ui, label, palette(ui.style()).err).on_hover_text(tip)
}

/// 工具栏按钮的公共形状：小号字 + 较低的最小高度。
fn compact_button(ui: &mut Ui, label: &str, color: Color32) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(label).color(color).size(13.0))
            .min_size(egui::vec2(0.0, 24.0))
            .corner_radius(egui::CornerRadius::same(8)),
    )
}

/// 8px 状态圆点 + 文本。
pub fn status_dot(ui: &mut Ui, color: Color32, label: &str) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 4.0, color);
        ui.label(label);
    });
}

/// 小号着色胶囊标签（非交互）。
pub fn pill(ui: &mut Ui, text: &str, fg: Color32, bg: Color32) {
    let font = TextStyle::Small.resolve(ui.style());
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, fg);
    let padding = egui::vec2(8.0, 3.0);
    let (rect, _) = ui.allocate_exact_size(galley.size() + padding * 2.0, egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::same(8), bg);
    ui.painter().galley(rect.min + padding, galley, fg);
}

/// 弹层标题行：左侧标题、右侧「关闭」；返回是否点了关闭。
#[must_use]
pub fn modal_title(ui: &mut Ui, title: &str) -> bool {
    ui.horizontal(|ui| {
        ui.heading(title);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.button("关闭").clicked()
        })
        .inner
    })
    .inner
}
