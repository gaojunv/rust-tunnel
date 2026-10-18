//! 远程会话窗口的展示尺寸模型：**窗口尺寸**与**画面缩放**两个独立维度。
//!
//! # 为什么是两维
//!
//! 远端的分辨率是对方屏幕的原生像素，本端**改不了**——所谓「设置分辨率」只能落在展示层：
//!
//! - [`WindowSize`]：这个窗口开多大（自适应 / 固定档位）；
//! - [`ViewScale`]：画面按什么比例画（适应窗口 / 固定百分比）。
//!
//! 两者必须独立：选了 1920×1080 的窗口 + 150% 的缩放，画面会溢出窗口，由滚动区承载；
//! 而「适应窗口」在同一个窗口尺寸下又会把整屏塞进来。把它们合成一个「分辨率」下拉，
//! 用户就没法表达「窗口小一点但画面 1:1 像素对齐」这种常见诉求。
//!
//! 本模块只做**纯几何换算与档位定义**（可单测），不碰 egui 上下文。

use egui::Vec2;

/// 画面缩放档位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum ViewScale {
    /// 适应窗口：等比缩放到可用区域（可放大也可缩小）。
    #[default]
    #[serde(rename = "fit")]
    Fit,
    /// 固定 50%。
    #[serde(rename = "p50")]
    P50,
    /// 固定 75%。
    #[serde(rename = "p75")]
    P75,
    /// 固定 100%：与远端原生像素 1:1（HiDPI 下会显得很小，这是选择它的原因）。
    #[serde(rename = "p100")]
    P100,
    /// 固定 125%。
    #[serde(rename = "p125")]
    P125,
    /// 固定 150%。
    #[serde(rename = "p150")]
    P150,
}

impl ViewScale {
    /// 全部档位（UI 展示顺序）。
    pub const ALL: [Self; 6] = [
        Self::Fit,
        Self::P50,
        Self::P75,
        Self::P100,
        Self::P125,
        Self::P150,
    ];

    /// 中文标签。
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Fit => "适应窗口",
            Self::P50 => "50%",
            Self::P75 => "75%",
            Self::P100 => "100%",
            Self::P125 => "125%",
            Self::P150 => "150%",
        }
    }

    /// 固定缩放比；`Fit` 返回 `None`（比例由可用区域决定，见 [`fit_size`]）。
    #[must_use]
    pub fn factor(self) -> Option<f32> {
        match self {
            Self::Fit => None,
            Self::P50 => Some(0.5),
            Self::P75 => Some(0.75),
            Self::P100 => Some(1.0),
            Self::P125 => Some(1.25),
            Self::P150 => Some(1.5),
        }
    }
}

/// 窗口尺寸档位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum WindowSize {
    /// 自适应：按远端显示器尺寸开窗，并受本机屏幕约束（见 [`initial_window_size`]）。
    #[default]
    #[serde(rename = "fit")]
    FitScreen,
    /// 1280×720。
    #[serde(rename = "1280x720")]
    W1280x720,
    /// 1600×900。
    #[serde(rename = "1600x900")]
    W1600x900,
    /// 1920×1080。
    #[serde(rename = "1920x1080")]
    W1920x1080,
    /// 2560×1440。
    #[serde(rename = "2560x1440")]
    W2560x1440,
}

impl WindowSize {
    /// 全部档位（UI 展示顺序）。
    pub const ALL: [Self; 5] = [
        Self::FitScreen,
        Self::W1280x720,
        Self::W1600x900,
        Self::W1920x1080,
        Self::W2560x1440,
    ];

    /// 中文标签。
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::FitScreen => "自适应",
            Self::W1280x720 => "1280×720",
            Self::W1600x900 => "1600×900",
            Self::W1920x1080 => "1920×1080",
            Self::W2560x1440 => "2560×1440",
        }
    }

    /// 窗口内尺寸（逻辑点）；自适应档返回 `None`（由 [`initial_window_size`] 算）。
    #[must_use]
    pub fn inner_size(self) -> Option<Vec2> {
        match self {
            Self::FitScreen => None,
            Self::W1280x720 => Some(Vec2::new(1280.0, 720.0)),
            Self::W1600x900 => Some(Vec2::new(1600.0, 900.0)),
            Self::W1920x1080 => Some(Vec2::new(1920.0, 1080.0)),
            Self::W2560x1440 => Some(Vec2::new(2560.0, 1440.0)),
        }
    }
}

/// 窗口最小内尺寸：再小工具条就会折成一团，画面也不剩多少。
pub const MIN_INNER: Vec2 = Vec2::new(640.0, 480.0);

/// 工具条高度的**兜底值**（逻辑点）：首帧尚未实测到工具条矩形时，用它作热区高度与隐藏占位高度。
///
/// 这是 [`crate::remote::RemoteView::toolbar_height`] 的初值，也是隐藏时占位的下限——工具条
/// 折行后会更高，可见时每帧以实测值覆盖它。
pub const TOOLBAR_FALLBACK_HEIGHT: f32 = 44.0;

/// 自适应开窗时，窗口最多占本机屏幕的这个比例（留出边距与任务栏）。
const SCREEN_FRACTION: f32 = 0.9;

/// 适应窗口：在可用区域内**等比铺满**，返回画面的绘制尺寸（至少 1×1，避免零尺寸退化）。
#[must_use]
#[expect(
    clippy::cast_precision_loss,
    reason = "画面像素尺寸有界（远小于 f32 精度上限）"
)]
pub fn fit_size(avail: Vec2, image: (u32, u32)) -> Vec2 {
    let (w, h) = (image.0.max(1) as f32, image.1.max(1) as f32);
    // 下界 0.01：可用区域退化为 0（窗口刚创建、面板还没布局）时仍给出一个可绘制的尺寸。
    let scale = (avail.x / w).min(avail.y / h).max(0.01);
    Vec2::new((w * scale).max(1.0), (h * scale).max(1.0))
}

/// 固定缩放：远端原生像素 × 比例（至少 1×1）。
#[must_use]
#[expect(
    clippy::cast_precision_loss,
    reason = "画面像素尺寸有界（远小于 f32 精度上限）"
)]
pub fn scaled_size(factor: f32, image: (u32, u32)) -> Vec2 {
    let (w, h) = (image.0.max(1) as f32, image.1.max(1) as f32);
    Vec2::new((w * factor).max(1.0), (h * factor).max(1.0))
}

/// 按当前缩放档位算出画面的绘制尺寸。
#[must_use]
pub fn display_size(scale: ViewScale, avail: Vec2, image: (u32, u32)) -> Vec2 {
    scale
        .factor()
        .map_or_else(|| fit_size(avail, image), |factor| scaled_size(factor, image))
}

/// 自适应档的初始窗口尺寸：取「远端显示器尺寸」与「本机屏幕 × [`SCREEN_FRACTION`]」的较小者。
///
/// 远端分辨率是**原生像素**，这里按逻辑点用（不做 DPI 折算）：用户要的是「一开窗就看到整屏」，
/// 而不是与远端像素对齐——真正要 1:1 时用 [`ViewScale::P100`] 档。
///
/// 下界是 [`MIN_INNER`]：远端屏幕比最小窗口还小时也不开得比工具条需要的更小。
#[must_use]
#[expect(
    clippy::cast_precision_loss,
    reason = "画面像素尺寸有界（远小于 f32 精度上限）"
)]
pub fn initial_window_size(remote: (u32, u32), monitor: Option<Vec2>) -> Vec2 {
    let want = Vec2::new(remote.0.max(1) as f32, remote.1.max(1) as f32);
    let capped = monitor.map_or(want, |screen| want.min(screen * SCREEN_FRACTION));
    capped.max(MIN_INNER)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_preserves_aspect_ratio() {
        let size = fit_size(Vec2::new(800.0, 600.0), (1920, 1080));
        let ratio = size.x / size.y;
        let expected = 1920.0 / 1080.0;
        assert!((ratio - expected).abs() < 1e-3, "{ratio} vs {expected}");
    }

    #[test]
    fn fit_fills_one_axis_exactly() {
        // 宽受限：结果宽度应当正好是可用宽度。
        let size = fit_size(Vec2::new(400.0, 1000.0), (1920, 1080));
        assert!((size.x - 400.0).abs() < 1e-3);
    }

    #[test]
    fn fit_survives_degenerate_inputs() {
        // 窗口尚未布局（可用区域为 0）或远端尺寸未知，都不能产生 0 或 NaN。
        for avail in [Vec2::ZERO, Vec2::new(0.0, 10.0), Vec2::new(-5.0, -5.0)] {
            let size = fit_size(avail, (0, 0));
            assert!(size.x >= 1.0 && size.y >= 1.0, "{size:?}");
            assert!(size.x.is_finite() && size.y.is_finite(), "{size:?}");
        }
    }

    #[test]
    fn scaled_size_is_linear() {
        let size = scaled_size(0.5, (1920, 1080));
        assert!((size.x - 960.0).abs() < 1e-3);
        assert!((size.y - 540.0).abs() < 1e-3);
    }

    #[test]
    fn display_size_uses_fit_only_for_fit_variant() {
        let avail = Vec2::new(300.0, 300.0);
        let image = (1920, 1080);
        assert_eq!(display_size(ViewScale::Fit, avail, image), fit_size(avail, image));
        // 固定档忽略可用区域：窗口再小也按比例画，溢出部分交给滚动区。
        let fixed = display_size(ViewScale::P100, avail, image);
        assert!((fixed.x - 1920.0).abs() < 1e-3);
    }

    #[test]
    fn initial_window_size_is_capped_by_screen() {
        let size = initial_window_size((3840, 2160), Some(Vec2::new(1440.0, 900.0)));
        assert!(size.x <= 1440.0 * SCREEN_FRACTION + 1e-3, "{size:?}");
        assert!(size.y <= 900.0 * SCREEN_FRACTION + 1e-3, "{size:?}");
    }

    #[test]
    fn initial_window_size_keeps_minimum_and_takes_remote_when_unknown_screen() {
        let small = initial_window_size((320, 200), Some(Vec2::new(1440.0, 900.0)));
        assert_eq!(small, MIN_INNER);
        let unknown = initial_window_size((1600, 900), None);
        assert!((unknown.x - 1600.0).abs() < 1e-3);
    }

    #[test]
    fn every_variant_has_a_distinct_label() {
        let scales: std::collections::BTreeSet<_> = ViewScale::ALL.iter().map(|s| s.label()).collect();
        assert_eq!(scales.len(), ViewScale::ALL.len());
        let sizes: std::collections::BTreeSet<_> = WindowSize::ALL.iter().map(|s| s.label()).collect();
        assert_eq!(sizes.len(), WindowSize::ALL.len());
    }

    #[test]
    fn only_fit_has_no_fixed_factor() {
        assert!(ViewScale::Fit.factor().is_none());
        assert!(ViewScale::ALL
            .iter()
            .filter(|s| **s != ViewScale::Fit)
            .all(|s| s.factor().is_some_and(|f| f > 0.0)));
        assert!(WindowSize::FitScreen.inner_size().is_none());
        assert!(WindowSize::ALL
            .iter()
            .filter(|s| **s != WindowSize::FitScreen)
            .all(|s| s.inner_size().is_some()));
    }

    #[test]
    fn prefs_round_trip_through_toml() {
        // 这两个枚举要落盘到 gui.toml：改 `serde(rename)` 会静默让旧配置回落成默认值。
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Holder {
            // 与 `GuiPrefs` 一致：带 `default` 才允许旧配置文件缺这两个字段。
            #[serde(default)]
            scale: ViewScale,
            #[serde(default)]
            window: WindowSize,
        }
        let holder = Holder {
            scale: ViewScale::P125,
            window: WindowSize::W1600x900,
        };
        let text = toml::to_string(&holder).expect("序列化");
        assert!(text.contains("p125"), "{text}");
        assert!(text.contains("1600x900"), "{text}");
        let back: Holder = toml::from_str(&text).expect("反序列化");
        assert_eq!(back, holder);
        // 旧配置文件里没有这个键时落到默认档，而不是解析失败。
        let defaulted: Holder = toml::from_str("window = \"1920x1080\"").expect("缺字段的配置");
        assert_eq!(defaulted.scale, ViewScale::Fit);
        assert_eq!(defaulted.window, WindowSize::W1920x1080);
    }
}
