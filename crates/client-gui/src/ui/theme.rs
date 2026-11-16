//! 远程会话窗口的视觉基础：主题令牌、会话级 `Style` 覆盖与常用 `Frame`。
//!
//! 会话窗口**跟随应用主题**（浅色/深色），所以这里从不硬编码 `dark_mode`：所有颜色都由
//! [`palette`] 依据 `style.visuals.dark_mode` 选择，调用方只需把 `&egui::Style` 传进来。
//!
//! 样式是**按视口作用域**生效的：eframe 的视口回调串行执行，[`scoped_session_style`] 在进入
//! 会话视口时保存当前 `Style`、切到 [`session_style`]，并在 `StyleGuard` 析构时原样恢复，
//! 因此主窗口的样式不受影响。

use std::sync::Arc;

/// 会话窗口配色令牌。深色/浅色各一套，由 [`palette`] 按当前 `Style` 选择。
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    /// 窗口/场景底色。
    pub bg: egui::Color32,
    /// 卡片、工具栏等表层底色。
    pub surface: egui::Color32,
    /// 比 `surface` 再抬升一层的控件底色。
    pub surface_raised: egui::Color32,
    /// 常规描边。
    pub stroke: egui::Color32,
    /// 强调描边（白色/黑色低透明度）。
    pub stroke_strong: egui::Color32,
    /// 主强调色。
    pub accent: egui::Color32,
    /// 主强调色的低透明度填充（选中态背景）。
    pub accent_dim: egui::Color32,
    /// 正文颜色。
    pub text: egui::Color32,
    /// 次要文本颜色。
    pub text_weak: egui::Color32,
    /// 警告/重连中。
    pub warn: egui::Color32,
    /// 错误/断开。
    pub err: egui::Color32,
    /// 阴影颜色（已带透明度）。
    pub shadow: egui::Color32,
}

/// 按 `style.visuals.dark_mode` 返回深色或浅色令牌。
#[must_use]
pub fn palette(style: &egui::Style) -> Palette {
    if style.visuals.dark_mode {
        Palette {
            bg: egui::Color32::from_rgb(0x17, 0x18, 0x1C),
            surface: egui::Color32::from_rgba_unmultiplied(0x22, 0x24, 0x2B, 245),
            surface_raised: egui::Color32::from_rgb(0x2A, 0x2D, 0x36),
            stroke: egui::Color32::from_rgb(0x3A, 0x3F, 0x4B),
            stroke_strong: egui::Color32::from_white_alpha(51),
            accent: egui::Color32::from_rgb(0x4C, 0x8D, 0xFF),
            accent_dim: egui::Color32::from_rgba_unmultiplied(0x4C, 0x8D, 0xFF, 41),
            text: egui::Color32::from_rgb(0xE6, 0xE8, 0xEE),
            text_weak: egui::Color32::from_rgb(0x9A, 0xA0, 0xAC),
            warn: egui::Color32::from_rgb(0xE5, 0xB4, 0x54),
            err: egui::Color32::from_rgb(0xE5, 0x67, 0x67),
            shadow: egui::Color32::from_black_alpha(102),
        }
    } else {
        Palette {
            bg: egui::Color32::from_rgb(0xF3, 0xF4, 0xF7),
            surface: egui::Color32::from_rgba_unmultiplied(0xFF, 0xFF, 0xFF, 235),
            surface_raised: egui::Color32::from_rgb(0xFF, 0xFF, 0xFF),
            stroke: egui::Color32::from_rgb(0xD7, 0xDA, 0xE2),
            stroke_strong: egui::Color32::from_black_alpha(38),
            accent: egui::Color32::from_rgb(0x2F, 0x6F, 0xEB),
            accent_dim: egui::Color32::from_rgba_unmultiplied(0x2F, 0x6F, 0xEB, 31),
            text: egui::Color32::from_rgb(0x1E, 0x21, 0x29),
            text_weak: egui::Color32::from_rgb(0x6A, 0x70, 0x80),
            warn: egui::Color32::from_rgb(0xB5, 0x7A, 0x12),
            err: egui::Color32::from_rgb(0xC9, 0x4A, 0x4A),
            shadow: egui::Color32::from_black_alpha(46),
        }
    }
}

/// 在 `base` 之上生成本次会话视口使用的 `Style`。
///
/// **只调外观，不碰主题**：`visuals.dark_mode` 原样保留，因此浅色应用得到浅色会话窗口。
#[must_use]
#[expect(
    clippy::too_many_lines,
    reason = "样式覆盖是一张平铺的令牌表：按段落拆函数会让「一处改了两边」更难发现"
)]
pub fn session_style(base: &egui::Style) -> egui::Style {
    let mut style = base.clone();
    let p = palette(&style);
    let dark = style.visuals.dark_mode;

    // 输入框等「凹陷」表面：深色比表层更暗，浅色比表层更亮。
    let extreme_bg = if dark {
        egui::Color32::from_rgb(0x1B, 0x1C, 0x21)
    } else {
        egui::Color32::from_rgb(0xFF, 0xFF, 0xFF)
    };
    let hovered_bg = if dark {
        egui::Color32::from_rgb(0x33, 0x37, 0x42)
    } else {
        egui::Color32::from_rgb(0xE6, 0xE8, 0xEE)
    };

    let visual = &mut style.visuals;
    visual.panel_fill = p.bg;
    visual.window_fill = p.surface;
    visual.window_stroke = egui::Stroke::new(1.0_f32, p.stroke);
    visual.extreme_bg_color = extreme_bg;
    visual.faint_bg_color = p.surface_raised;
    visual.selection.bg_fill = p.accent_dim;
    visual.selection.stroke = egui::Stroke::new(1.0_f32, p.accent);
    visual.window_corner_radius = egui::CornerRadius::same(12);
    visual.menu_corner_radius = egui::CornerRadius::same(10);
    visual.window_shadow = egui::Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 0,
        color: p.shadow,
    };
    visual.popup_shadow = egui::Shadow {
        offset: [0, 6],
        blur: 18,
        spread: 0,
        color: p.shadow,
    };

    let radius = egui::CornerRadius::same(8);
    let widgets = &mut visual.widgets;
    widgets.noninteractive = egui::style::WidgetVisuals {
        bg_fill: p.surface,
        weak_bg_fill: egui::Color32::TRANSPARENT,
        bg_stroke: egui::Stroke::new(1.0_f32, p.stroke),
        corner_radius: radius,
        fg_stroke: egui::Stroke::new(1.0_f32, p.text),
        expansion: 0.0,
    };
    widgets.inactive = egui::style::WidgetVisuals {
        bg_fill: p.surface_raised,
        weak_bg_fill: p.surface_raised,
        bg_stroke: egui::Stroke::new(1.0_f32, p.stroke),
        corner_radius: radius,
        fg_stroke: egui::Stroke::new(1.0_f32, p.text),
        expansion: 0.0,
    };
    widgets.hovered = egui::style::WidgetVisuals {
        bg_fill: hovered_bg,
        weak_bg_fill: hovered_bg,
        bg_stroke: egui::Stroke::new(1.0_f32, p.accent),
        corner_radius: radius,
        fg_stroke: egui::Stroke::new(1.0_f32, p.text),
        expansion: 1.0,
    };
    widgets.active = egui::style::WidgetVisuals {
        bg_fill: p.accent,
        // 选中项（`SelectableLabel`）画的是 `weak_bg_fill`：用低透明度强调色 + 强调描边。
        weak_bg_fill: p.accent_dim,
        bg_stroke: egui::Stroke::new(1.0_f32, p.accent),
        corner_radius: radius,
        fg_stroke: egui::Stroke::new(1.0_f32, p.text),
        expansion: 0.0,
    };

    let spacing = &mut style.spacing;
    spacing.item_spacing = egui::vec2(8.0, 6.0);
    spacing.button_padding = egui::vec2(10.0, 6.0);
    spacing.interact_size = egui::vec2(30.0, 26.0);
    spacing.window_margin = egui::Margin::symmetric(14, 12);
    spacing.combo_width = 150.0;
    spacing.indent = 16.0;
    spacing.scroll.bar_width = 8.0;
    spacing.scroll.bar_inner_margin = 4.0;
    spacing.scroll.bar_outer_margin = 2.0;

    let proportional = egui::FontFamily::Proportional;
    style.text_styles.insert(
        egui::TextStyle::Heading,
        egui::FontId::new(17.0, proportional.clone()),
    );
    style.text_styles.insert(
        egui::TextStyle::Body,
        egui::FontId::new(13.5, proportional.clone()),
    );
    style.text_styles.insert(
        egui::TextStyle::Button,
        egui::FontId::new(13.5, proportional.clone()),
    );
    style.text_styles.insert(
        egui::TextStyle::Small,
        egui::FontId::new(11.5, proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Monospace,
        egui::FontId::new(12.5, egui::FontFamily::Monospace),
    );
    style.animation_time = 0.12;

    style
}

/// 会话视口样式的作用域守卫：析构时把保存的 `Style` 原样写回。
///
/// eframe 的视口回调是**串行**执行的（同一时刻只有一个视口在跑），所以在会话视口的回调里
/// 开头调用 [`scoped_session_style`]、让守卫活到回调结束，就不会污染主窗口或其它视口。
pub struct StyleGuard {
    /// 共享的 `egui::Context`（恢复样式用）。
    ctx: egui::Context,
    /// 进入会话视口前的样式。
    prev: Arc<egui::Style>,
}

impl Drop for StyleGuard {
    fn drop(&mut self) {
        self.ctx.set_style(self.prev.clone());
    }
}

/// 保存当前样式、切换到 [`session_style`]，返回在作用域结束时恢复原样式的守卫。
#[must_use]
pub fn scoped_session_style(ctx: &egui::Context) -> StyleGuard {
    let prev = ctx.style();
    ctx.set_style(session_style(&prev));
    StyleGuard {
        ctx: ctx.clone(),
        prev,
    }
}

/// 卡片：表层底色 + 细描边 + 圆角 + 阴影。
pub fn card_frame(style: &egui::Style) -> egui::Frame {
    let p = palette(style);
    egui::Frame::new()
        .fill(p.surface)
        .stroke(egui::Stroke::new(1.0_f32, p.stroke))
        .corner_radius(egui::CornerRadius::same(12))
        .inner_margin(egui::Margin::symmetric(14, 12))
        .shadow(egui::Shadow {
            offset: [0, 6],
            blur: 20,
            spread: 0,
            color: p.shadow,
        })
}

/// 工具栏：与卡片同底色，但用 `stroke_strong` 描边、更薄的留白与更轻的投影。
pub fn toolbar_frame(style: &egui::Style) -> egui::Frame {
    let p = palette(style);
    egui::Frame::new()
        .fill(p.surface)
        .stroke(egui::Stroke::new(1.0_f32, p.stroke_strong))
        .corner_radius(egui::CornerRadius::same(12))
        .inner_margin(egui::Margin::symmetric(12, 7))
        .shadow(egui::Shadow {
            offset: [0, 4],
            blur: 16,
            spread: 0,
            color: p.shadow,
        })
}

/// 画面容器：场景底色，无描边（不抢视频画面的注意力）。
pub fn scene_frame(style: &egui::Style) -> egui::Frame {
    egui::Frame::new().fill(palette(style).bg)
}

/// 统计角标：**半透明**浮层底板（角标盖在画面上，不能把它挡成一块实心板）。
///
/// 与 [`card_frame`] 的区别只有三处，且都是为了「不挡画面、不吃点击」：
/// - 底色 alpha 降到 170（深色）/ 190（浅色），能透出底下的视频画面；
/// - 细描边 + 圆角 10、内边距 10×8（比卡片更紧凑）；
/// - **没有投影**——投影会在画面边缘投出一圈灰雾，而角标只是读数，不需要浮起感。
///
/// 角标本身必须挂在 `egui::Area::interactable(false)` 上才真正点击穿透（见
/// [`crate::ui::session_window::stats_overlay`]）；这里的半透明只解决视觉遮挡。
pub fn stats_frame(style: &egui::Style) -> egui::Frame {
    let p = palette(style);
    let fill = if style.visuals.dark_mode {
        egui::Color32::from_rgba_unmultiplied(0x22, 0x24, 0x2B, 170)
    } else {
        egui::Color32::from_rgba_unmultiplied(0xFF, 0xFF, 0xFF, 190)
    };
    egui::Frame::new()
        .fill(fill)
        .stroke(egui::Stroke::new(1.0_f32, p.stroke))
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::symmetric(10, 8))
}

#[cfg(test)]
mod tests {
    //! 纯样式测试：不构造 `egui::Context`（`scoped_session_style` 需要在真实上下文里调用）。

    use super::{card_frame, palette, session_style, stats_frame};
    use egui::{FontFamily, Style, TextStyle};

    /// 粗略感知亮度（0.0–1.0），只用于「文字与背景确实有对比」这一条断言。
    fn luminance(color: egui::Color32) -> f32 {
        (0.299 * f32::from(color.r()) + 0.587 * f32::from(color.g()) + 0.114 * f32::from(color.b()))
            / 255.0
    }

    #[test]
    fn session_style_keeps_dark_mode() {
        for dark in [true, false] {
            let mut base = Style::default();
            base.visuals.dark_mode = dark;
            assert_eq!(
                session_style(&base).visuals.dark_mode,
                dark,
                "会话样式不得翻转主题"
            );
        }
    }

    #[test]
    fn session_style_uses_rounded_corners() {
        let style = session_style(&Style::default());
        assert!(style.visuals.window_corner_radius.nw > 0);
        assert!(style.visuals.widgets.inactive.corner_radius.nw > 0);
        assert!(style.visuals.widgets.hovered.corner_radius.nw > 0);
    }

    #[test]
    fn dark_and_light_palettes_differ_with_contrast() {
        let mut dark_style = Style::default();
        dark_style.visuals.dark_mode = true;
        let mut light_style = Style::default();
        light_style.visuals.dark_mode = false;

        let dark = palette(&dark_style);
        let light = palette(&light_style);
        assert_ne!(dark.bg, light.bg);
        assert_ne!(dark.text, light.text);
        assert_ne!(dark.accent, light.accent);

        // 正文与底色必须保持足够对比（深色亮字、浅色暗字）。
        assert!((luminance(dark.text) - luminance(dark.bg)).abs() > 0.4);
        assert!((luminance(light.text) - luminance(light.bg)).abs() > 0.4);
    }

    #[test]
    fn session_text_styles_define_heading_and_body() {
        let style = session_style(&Style::default());
        let heading = style
            .text_styles
            .get(&TextStyle::Heading)
            .expect("缺少 Heading 字号");
        let body = style
            .text_styles
            .get(&TextStyle::Body)
            .expect("缺少 Body 字号");
        assert!(heading.size > body.size);
        assert_eq!(heading.family, FontFamily::Proportional);
        assert_eq!(body.family, FontFamily::Proportional);
    }

    /// 统计角标底板必须**半透明、无投影**：它盖在画面上，实心底板或投影都会挡住视频。
    #[test]
    fn stats_frame_is_translucent_without_shadow() {
        for dark in [true, false] {
            let mut base = Style::default();
            base.visuals.dark_mode = dark;
            let stats = stats_frame(&base);
            let card = card_frame(&base);
            let expected_alpha = if dark { 170 } else { 190 };
            assert_eq!(
                stats.fill.a(),
                expected_alpha,
                "角标底色 alpha 应可透出画面"
            );
            assert!(
                stats.fill.a() < card.fill.a(),
                "角标必须比实心卡片更透明（dark={dark}）"
            );
            assert_eq!(stats.shadow.blur, 0, "角标不得带投影");
            assert_eq!(stats.shadow.color.a(), 0, "角标投影必须完全透明");
            assert_eq!(stats.corner_radius, egui::CornerRadius::same(10));
        }
    }
}
