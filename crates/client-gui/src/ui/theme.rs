//! 应用主题：Win11 Fluent 风格的令牌、全局 `Style` 与常用 `Frame`。
//!
//! 设计约束（决定了这里的形状）：
//! - **实色层次，不做 Mica**：用背景 / 卡片 / 抬升面 / 输入框四层色阶表达层次。透明交换链
//!   在 wgpu/glow 两条后端上行为不一，截图与录屏还可能发黑，收益不抵风险。
//! - **强调色跟随系统**（见 [`crate::ui::accent`]），深浅两套各自把亮度钳到可读区间：
//!   深色主题下用户选的深色强调色会被提亮，浅色主题下的浅色强调色会被压暗，否则
//!   「强调色底 + 白字」会读不出来。
//! - **会话窗口跟随应用主题**：这里从不硬编码 `dark_mode`，所有颜色都由 [`palette`] 依据
//!   `style.visuals.dark_mode` 选择。会话视口仍用 [`scoped_session_style`] 做作用域覆盖
//!   （eframe 的视口回调串行执行，守卫析构时原样恢复，因此不会污染主窗口）。

use std::sync::{Arc, OnceLock};

/// 会话窗口配色令牌。深色/浅色各一套，由 [`palette`] 按当前 `Style` 选择。
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    /// 窗口/场景底色。
    pub bg: egui::Color32,
    /// 卡片、工具栏等表层底色。
    pub surface: egui::Color32,
    /// 比 `surface` 再抬升一层的控件底色（按钮、输入框）。
    pub surface_raised: egui::Color32,
    /// 悬停/选中行的浅色填充。
    pub surface_alt: egui::Color32,
    /// 常规描边。
    pub stroke: egui::Color32,
    /// 强调描边（白色/黑色低透明度）。
    pub stroke_strong: egui::Color32,
    /// 分隔线（比 `stroke` 更弱）。
    pub divider: egui::Color32,
    /// 主强调色。
    pub accent: egui::Color32,
    /// 强调色悬停态。
    pub accent_hover: egui::Color32,
    /// 主强调色的低透明度填充（选中态背景）。
    pub accent_dim: egui::Color32,
    /// 强调色底上的文字色（按亮度自动选黑/白）。
    pub on_accent: egui::Color32,
    /// 正文颜色。
    pub text: egui::Color32,
    /// 次要文本颜色。
    pub text_weak: egui::Color32,
    /// 禁用文本颜色。
    pub text_disabled: egui::Color32,
    /// 成功/在线。
    pub ok: egui::Color32,
    /// 警告/重连中。
    pub warn: egui::Color32,
    /// 错误/断开。
    pub err: egui::Color32,
    /// 阴影颜色（已带透明度）。
    pub shadow: egui::Color32,
    /// 当前是否深色。
    pub dark: bool,
}

/// Fluent 蓝的回退值：浅色 `#0067C0`、深色 `#60CDFF`。
const FALLBACK_ACCENT_LIGHT: [u8; 3] = [0x00, 0x67, 0xC0];
const FALLBACK_ACCENT_DARK: [u8; 3] = [0x60, 0xCD, 0xFF];

/// 系统强调色只解析一次（`palette` 每帧都会调用）。
fn accent_hint() -> Option<[u8; 3]> {
    static HINT: OnceLock<Option<[u8; 3]>> = OnceLock::new();
    *HINT.get_or_init(crate::ui::accent::system_accent_rgb)
}

/// 粗略感知亮度（0.0–1.0）。
#[must_use]
pub fn luminance(color: egui::Color32) -> f32 {
    (0.299 * f32::from(color.r()) + 0.587 * f32::from(color.g()) + 0.114 * f32::from(color.b()))
        / 255.0
}

fn rgb([r, g, b]: [u8; 3]) -> egui::Color32 {
    egui::Color32::from_rgb(r, g, b)
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "颜色通道是 0..=255 的定点数，四舍五入后落在 u8 范围内"
)]
fn mix(from: [u8; 3], to: [u8; 3], t: f32) -> [u8; 3] {
    let t = t.clamp(0.0, 1.0);
    let lerp = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8;
    [
        lerp(from[0], to[0]),
        lerp(from[1], to[1]),
        lerp(from[2], to[2]),
    ]
}

/// 把颜色亮度钳进 `[lo, hi]`：暗了往白色混，亮了按比例压暗。
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "颜色通道是 0..=255 的定点数，四舍五入后落在 u8 范围内"
)]
fn clamp_luminance(rgb_in: [u8; 3], lo: f32, hi: f32) -> [u8; 3] {
    let lum = luminance(rgb(rgb_in));
    if lum < lo {
        let t = (lo - lum) / (1.0 - lum).max(f32::EPSILON);
        mix(rgb_in, [255, 255, 255], t)
    } else if lum > hi {
        let scale = hi / lum.max(f32::EPSILON);
        [
            (f32::from(rgb_in[0]) * scale).round() as u8,
            (f32::from(rgb_in[1]) * scale).round() as u8,
            (f32::from(rgb_in[2]) * scale).round() as u8,
        ]
    } else {
        rgb_in
    }
}

/// 解析当前模式的强调色（含系统覆盖与亮度钳制）。
fn accent_for(dark: bool) -> [u8; 3] {
    match (accent_hint(), dark) {
        (Some(raw), true) => clamp_luminance(raw, 0.55, 0.82),
        (Some(raw), false) => clamp_luminance(raw, 0.22, 0.58),
        (None, true) => FALLBACK_ACCENT_DARK,
        (None, false) => FALLBACK_ACCENT_LIGHT,
    }
}

/// 按 `style.visuals.dark_mode` 返回深色或浅色令牌。
#[must_use]
pub fn palette(style: &egui::Style) -> Palette {
    let dark = style.visuals.dark_mode;
    let accent_rgb = accent_for(dark);
    let accent = rgb(accent_rgb);
    let on_accent = if luminance(accent) > 0.55 {
        egui::Color32::from_rgb(0x0A, 0x0A, 0x0A)
    } else {
        egui::Color32::WHITE
    };
    let accent_hover = rgb(if dark {
        mix(accent_rgb, [255, 255, 255], 0.14)
    } else {
        mix(accent_rgb, [0, 0, 0], 0.12)
    });
    if dark {
        Palette {
            bg: egui::Color32::from_rgb(0x20, 0x20, 0x20),
            surface: egui::Color32::from_rgb(0x27, 0x27, 0x27),
            surface_raised: egui::Color32::from_rgb(0x2D, 0x2D, 0x2D),
            surface_alt: egui::Color32::from_rgb(0x33, 0x33, 0x33),
            stroke: egui::Color32::from_rgb(0x3A, 0x3A, 0x3A),
            stroke_strong: egui::Color32::from_white_alpha(38),
            divider: egui::Color32::from_white_alpha(22),
            accent,
            accent_hover,
            accent_dim: egui::Color32::from_rgba_unmultiplied(
                accent.r(),
                accent.g(),
                accent.b(),
                46,
            ),
            on_accent,
            text: egui::Color32::from_rgb(0xFF, 0xFF, 0xFF),
            text_weak: egui::Color32::from_rgb(0xB9, 0xB9, 0xB9),
            text_disabled: egui::Color32::from_rgb(0x7A, 0x7A, 0x7A),
            ok: egui::Color32::from_rgb(0x6C, 0xCB, 0x5F),
            warn: egui::Color32::from_rgb(0xF0, 0xC0, 0x5A),
            err: egui::Color32::from_rgb(0xFF, 0x99, 0xA4),
            shadow: egui::Color32::from_black_alpha(140),
            dark,
        }
    } else {
        Palette {
            bg: egui::Color32::from_rgb(0xF3, 0xF3, 0xF3),
            surface: egui::Color32::from_rgb(0xFF, 0xFF, 0xFF),
            surface_raised: egui::Color32::from_rgb(0xFB, 0xFB, 0xFB),
            surface_alt: egui::Color32::from_rgb(0xEF, 0xEF, 0xEF),
            stroke: egui::Color32::from_rgb(0xE1, 0xE1, 0xE1),
            stroke_strong: egui::Color32::from_black_alpha(36),
            divider: egui::Color32::from_black_alpha(22),
            accent,
            accent_hover,
            accent_dim: egui::Color32::from_rgba_unmultiplied(
                accent.r(),
                accent.g(),
                accent.b(),
                30,
            ),
            on_accent,
            text: egui::Color32::from_rgb(0x1B, 0x1B, 0x1B),
            text_weak: egui::Color32::from_rgb(0x5E, 0x5E, 0x5E),
            text_disabled: egui::Color32::from_rgb(0xA0, 0xA0, 0xA0),
            ok: egui::Color32::from_rgb(0x0F, 0x7B, 0x0F),
            warn: egui::Color32::from_rgb(0x9D, 0x5D, 0x00),
            err: egui::Color32::from_rgb(0xC4, 0x2B, 0x1C),
            shadow: egui::Color32::from_black_alpha(26),
            dark,
        }
    }
}

/// 在 `base` 之上生成本次会话视口使用的 `Style`（会话窗口与主窗口同一套令牌）。
#[must_use]
pub fn session_style(base: &egui::Style) -> egui::Style {
    fluent_style(base)
}

/// 主窗口的全局 `Style`：Fluent 的控件形状、字号与间距。
#[must_use]
#[expect(
    clippy::too_many_lines,
    reason = "样式覆盖是一张平铺的令牌表：按段落拆函数会让「一处改了两边」更难发现"
)]
pub fn fluent_style(base: &egui::Style) -> egui::Style {
    let mut style = base.clone();
    let p = palette(&style);
    let dark = style.visuals.dark_mode;

    let input_bg = if dark {
        egui::Color32::from_rgb(0x1B, 0x1B, 0x1B)
    } else {
        egui::Color32::WHITE
    };
    let hover_bg = if dark {
        egui::Color32::from_rgb(0x38, 0x38, 0x38)
    } else {
        egui::Color32::from_rgb(0xF5, 0xF5, 0xF5)
    };

    let visual = &mut style.visuals;
    visual.panel_fill = p.bg;
    visual.window_fill = p.surface;
    visual.window_stroke = egui::Stroke::new(1.0_f32, p.stroke);
    visual.extreme_bg_color = input_bg;
    visual.faint_bg_color = p.surface_alt;
    visual.code_bg_color = p.surface_raised;
    visual.selection.bg_fill = p.accent_dim;
    visual.selection.stroke = egui::Stroke::new(1.0_f32, p.accent);
    visual.hyperlink_color = p.accent;
    visual.warn_fg_color = p.warn;
    visual.error_fg_color = p.err;
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

    let radius = egui::CornerRadius::same(6);
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
        bg_fill: hover_bg,
        weak_bg_fill: hover_bg,
        bg_stroke: egui::Stroke::new(1.0_f32, p.stroke_strong),
        corner_radius: radius,
        fg_stroke: egui::Stroke::new(1.0_f32, p.text),
        expansion: 0.0,
    };
    widgets.active = egui::style::WidgetVisuals {
        bg_fill: p.accent_dim,
        // 选中项（`SelectableLabel`）画的是 `weak_bg_fill`：用低透明度强调色 + 强调描边。
        weak_bg_fill: p.accent_dim,
        bg_stroke: egui::Stroke::new(1.0_f32, p.accent),
        corner_radius: radius,
        fg_stroke: egui::Stroke::new(1.0_f32, p.text),
        expansion: 0.0,
    };

    let spacing = &mut style.spacing;
    spacing.item_spacing = egui::vec2(8.0, 8.0);
    spacing.button_padding = egui::vec2(12.0, 6.0);
    spacing.interact_size = egui::vec2(32.0, 30.0);
    spacing.window_margin = egui::Margin::symmetric(14, 12);
    spacing.combo_width = 150.0;
    spacing.indent = 16.0;
    spacing.scroll.bar_width = 8.0;
    spacing.scroll.bar_inner_margin = 4.0;
    spacing.scroll.bar_outer_margin = 2.0;

    let proportional = egui::FontFamily::Proportional;
    let heading = crate::fonts::heading_family();
    style
        .text_styles
        .insert(egui::TextStyle::Heading, egui::FontId::new(20.0, heading));
    style.text_styles.insert(
        egui::TextStyle::Body,
        egui::FontId::new(14.0, proportional.clone()),
    );
    style.text_styles.insert(
        egui::TextStyle::Button,
        egui::FontId::new(14.0, proportional.clone()),
    );
    style.text_styles.insert(
        egui::TextStyle::Small,
        egui::FontId::new(11.5, proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Monospace,
        egui::FontId::new(12.5, egui::FontFamily::Monospace),
    );
    style.animation_time = 0.10;

    style
}

/// 把 Fluent 样式写到所有主题（浅色/深色各一份）上；启动与切换主题时调用。
pub fn apply_global_style(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        *style = fluent_style(style);
    });
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

/// 卡片：表层底色 + 细描边 + 圆角 + 极轻的投影。
pub fn card_frame(style: &egui::Style) -> egui::Frame {
    let p = palette(style);
    egui::Frame::new()
        .fill(p.surface)
        .stroke(egui::Stroke::new(1.0_f32, p.stroke))
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::symmetric(16, 14))
        .shadow(egui::Shadow {
            offset: [0, 1],
            blur: 4,
            spread: 0,
            color: if p.dark {
                egui::Color32::from_black_alpha(60)
            } else {
                egui::Color32::from_black_alpha(12)
            },
        })
}

/// 左侧导航栏底板：比窗口底色略深/略浅一层，与内容区靠分隔线分开。
pub fn nav_frame(style: &egui::Style) -> egui::Frame {
    let p = palette(style);
    egui::Frame::new()
        .fill(if p.dark {
            egui::Color32::from_rgb(0x1B, 0x1B, 0x1B)
        } else {
            egui::Color32::from_rgb(0xF9, 0xF9, 0xF9)
        })
        .inner_margin(egui::Margin::symmetric(8, 10))
}

/// 页面主体：背景色 + 统一内边距。
pub fn page_frame(style: &egui::Style) -> egui::Frame {
    egui::Frame::new()
        .fill(palette(style).bg)
        .inner_margin(egui::Margin::symmetric(22, 18))
}

/// 工具栏：与卡片同底色，但用 `stroke_strong` 描边、更薄的留白与更轻的投影。
pub fn toolbar_frame(style: &egui::Style) -> egui::Frame {
    let p = palette(style);
    egui::Frame::new()
        .fill(p.surface)
        .stroke(egui::Stroke::new(1.0_f32, p.stroke_strong))
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::symmetric(12, 7))
        .shadow(egui::Shadow {
            offset: [0, 4],
            blur: 16,
            spread: 0,
            color: p.shadow,
        })
}

/// 会话顶部的**贴顶**工具条：与 [`toolbar_frame`] 同色同边，但上缘两角为直角、下缘圆角，
/// 读起来像从窗口上沿垂下来的一条面板（收起态换成一枚同样贴顶的小胶囊，见
/// [`crate::ui::widgets::pill_button`]）。
pub fn toolbar_frame_top(style: &egui::Style) -> egui::Frame {
    let p = palette(style);
    egui::Frame::new()
        .fill(p.surface)
        .stroke(egui::Stroke::new(1.0_f32, p.stroke_strong))
        .corner_radius(egui::CornerRadius {
            nw: 0,
            ne: 0,
            sw: 10,
            se: 10,
        })
        .inner_margin(egui::Margin::symmetric(12, 6))
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
        egui::Color32::from_rgba_unmultiplied(0x27, 0x27, 0x27, 170)
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

    use super::{card_frame, fluent_style, luminance, palette, session_style, stats_frame};
    use egui::{FontFamily, Style, TextStyle};

    fn style(dark: bool) -> Style {
        let mut base = Style::default();
        base.visuals.dark_mode = dark;
        base
    }

    #[test]
    fn session_style_keeps_dark_mode() {
        for dark in [true, false] {
            assert_eq!(
                session_style(&style(dark)).visuals.dark_mode,
                dark,
                "会话样式不得翻转主题"
            );
        }
    }

    #[test]
    fn fluent_style_uses_rounded_corners() {
        let style = fluent_style(&Style::default());
        assert!(style.visuals.window_corner_radius.nw > 0);
        assert!(style.visuals.widgets.inactive.corner_radius.nw > 0);
        assert!(style.visuals.widgets.hovered.corner_radius.nw > 0);
    }

    #[test]
    fn dark_and_light_palettes_differ_with_contrast() {
        let dark = palette(&style(true));
        let light = palette(&style(false));
        assert_ne!(dark.bg, light.bg);
        assert_ne!(dark.text, light.text);
        assert_ne!(dark.accent, light.accent);

        // 正文与底色必须保持足够对比（深色亮字、浅色暗字）。
        assert!((luminance(dark.text) - luminance(dark.bg)).abs() > 0.4);
        assert!((luminance(light.text) - luminance(light.bg)).abs() > 0.4);
    }

    /// 强调色底上的文字必须真的读得出来（系统强调色可能是浅黄或深蓝）。
    #[test]
    fn accent_and_on_accent_keep_contrast() {
        for dark in [true, false] {
            let p = palette(&style(dark));
            assert!(
                (luminance(p.accent) - luminance(p.on_accent)).abs() > 0.35,
                "dark={dark} 时强调色与文字色对比不足"
            );
        }
    }

    /// 三层表面必须可区分（背景 / 卡片 / 抬升面），否则层次感会消失。
    #[test]
    fn surface_layers_are_distinct() {
        for dark in [true, false] {
            let p = palette(&style(dark));
            assert_ne!(p.bg, p.surface, "dark={dark}");
            assert_ne!(p.surface, p.surface_raised, "dark={dark}");
        }
    }

    #[test]
    fn session_text_styles_define_heading_and_body() {
        let style = fluent_style(&Style::default());
        let heading = style
            .text_styles
            .get(&TextStyle::Heading)
            .expect("缺少 Heading 字号");
        let body = style
            .text_styles
            .get(&TextStyle::Body)
            .expect("缺少 Body 字号");
        assert!(heading.size > body.size);
        assert_eq!(body.family, FontFamily::Proportional);
    }

    /// 统计角标底板必须**半透明、无投影**：它盖在画面上，实心底板或投影都会挡住视频。
    #[test]
    fn stats_frame_is_translucent_without_shadow() {
        for dark in [true, false] {
            let base = style(dark);
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
