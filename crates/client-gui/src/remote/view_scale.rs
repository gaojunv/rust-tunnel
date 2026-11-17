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
//! # DPI：远端像素 ≠ 逻辑点
//!
//! 本模块的输入是远端**像素**，输出是 egui **逻辑点**，两者相差 `pixels_per_point`。
//! 早期实现把它当成 1.0，于是 150% 缩放的屏幕上「100%」档实际是 1.5 倍的双线性放大，
//! 「适应窗口」也整体偏大——这是「所有字都发虚」的一层独立原因（与码率无关）。
//! 现在所有比例都以**物理像素**为基准，`Fit` 且**绝不放大**。
//!
//! 本模块只做**纯几何换算与档位定义**（可单测），不碰 egui 上下文。

use egui::Vec2;

/// 画面缩放档位。
///
/// **所有比例都是「远端像素 : 物理像素」**（不是逻辑点）：150% 缩放的屏幕上选 `100%`，
/// 1 个远端像素正好占 1 个物理像素，画面不会被 DPI 放大（历史上按逻辑点算，等于强制
/// 1.5 倍双线性放大，字整体发虚）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum ViewScale {
    /// 适应窗口：等比缩到可用区域（**只缩不放**，比窗口小时按 1:1 居中留白）。
    #[default]
    #[serde(rename = "fit")]
    Fit,
    /// 固定 50%。
    #[serde(rename = "p50")]
    P50,
    /// 固定 75%。
    #[serde(rename = "p75")]
    P75,
    /// 固定 100%：1 远端像素 = 1 物理像素（HiDPI 下会显得很小，这正是它的用途）。
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
    ///
    /// 档位里的数字是**物理像素**（用户看到的是「1920×1080」而不是「1280 点」），
    /// 所以按 `pixels_per_point` 折算：150% 屏幕上选 1920×1080 得到 1280×720 逻辑点，
    /// 窗口里正好能 1:1 放下一个 1080p 远端画面。
    #[must_use]
    pub fn inner_size(self, pixels_per_point: f32) -> Option<Vec2> {
        let ppp = dpi(pixels_per_point);
        let size = match self {
            Self::FitScreen => return None,
            Self::W1280x720 => Vec2::new(1280.0, 720.0),
            Self::W1600x900 => Vec2::new(1600.0, 900.0),
            Self::W1920x1080 => Vec2::new(1920.0, 1080.0),
            Self::W2560x1440 => Vec2::new(2560.0, 1440.0),
        };
        Some(size / ppp)
    }
}

/// 窗口最小内尺寸：再小工具条就会折成一团，画面也不剩多少。
pub const MIN_INNER: Vec2 = Vec2::new(640.0, 480.0);

/// 自适应开窗时，窗口最多占本机屏幕的这个比例（留出边距与任务栏）。
const SCREEN_FRACTION: f32 = 0.9;

/// 取有效 DPI 缩放（`pixels_per_point`），非法值一律按 1.0 处理。
///
/// 远端画面是**像素**，而 egui 的一切几何量都是**逻辑点**；两者相差一个
/// `pixels_per_point`。历史上这里把它当成 1.0（远端像素 = 逻辑点），于是：
/// - 150% 缩放的屏幕上，`100%` 档把 1920 宽的远端画到 2880 物理像素宽——**1.5 倍放大**；
/// - 「适应窗口」按逻辑点算比例，物理像素上得到的比例整体偏大。
///
/// 两条都会让文字整体发虚（双线性放大/单次采样缩小），且与编码码率无关。
#[must_use]
fn dpi(pixels_per_point: f32) -> f32 {
    if pixels_per_point.is_finite() && pixels_per_point > 0.0 {
        pixels_per_point
    } else {
        1.0
    }
}

/// 适应窗口：在可用区域内**等比铺满**，返回画面的绘制尺寸（至少 1×1，避免零尺寸退化）。
///
/// **以物理像素为基准**且**绝不放大**：`image` 是远端像素，`avail` 是逻辑点，比较前先把
/// `avail` 换算成物理像素。比例封顶 1.0——放大不会带来任何新细节，只会把边缘插值糊；
/// 窗口比远端大时画面居中留白，用户想要放大可以显式选 125% / 150%。
#[must_use]
#[expect(
    clippy::cast_precision_loss,
    reason = "画面像素尺寸有界（远小于 f32 精度上限）"
)]
pub fn fit_size(avail: Vec2, image: (u32, u32), pixels_per_point: f32) -> Vec2 {
    let ppp = dpi(pixels_per_point);
    let (w, h) = (image.0.max(1) as f32, image.1.max(1) as f32);
    let avail_px = avail * ppp;
    // 下界 0.01：可用区域退化为 0（窗口刚创建、面板还没布局）时仍给出一个可绘制的尺寸。
    let scale = (avail_px.x / w).min(avail_px.y / h).clamp(0.01, 1.0);
    Vec2::new((w * scale / ppp).max(1.0), (h * scale / ppp).max(1.0))
}

/// 固定缩放：远端原生像素 × 比例，再换回逻辑点（至少 1×1）。
///
/// `factor` 是**远端像素 : 物理像素**的比例，所以 `P100` 在 150% 缩放的屏幕上给出
/// `image / 1.5` 逻辑点 = `image` 物理像素——这才是注释里说的「1:1」。
#[must_use]
#[expect(
    clippy::cast_precision_loss,
    reason = "画面像素尺寸有界（远小于 f32 精度上限）"
)]
pub fn scaled_size(factor: f32, image: (u32, u32), pixels_per_point: f32) -> Vec2 {
    let ppp = dpi(pixels_per_point);
    let (w, h) = (image.0.max(1) as f32, image.1.max(1) as f32);
    Vec2::new((w * factor / ppp).max(1.0), (h * factor / ppp).max(1.0))
}

/// 按当前缩放档位算出画面的绘制尺寸（逻辑点）。
#[must_use]
pub fn display_size(
    scale: ViewScale,
    avail: Vec2,
    image: (u32, u32),
    pixels_per_point: f32,
) -> Vec2 {
    scale.factor().map_or_else(
        || fit_size(avail, image, pixels_per_point),
        |factor| scaled_size(factor, image, pixels_per_point),
    )
}

/// 自适应档的初始窗口尺寸：取「远端显示器尺寸」与「本机屏幕 × [`SCREEN_FRACTION`]」的较小者。
///
/// 远端分辨率是**物理像素**，窗口尺寸是逻辑点，故先除以 `pixels_per_point`：默认开窗就落
/// 在「画面 1:1 物理像素」的大小上（受屏幕比例约束）。历史上这里直接把远端像素当逻辑点，
/// 结果 150% 缩放下默认窗比远端大 1.5 倍，画面被双线性放大——一开窗就是糊的。
///
/// 下界是 [`MIN_INNER`]：远端屏幕比最小窗口还小时也不开得比工具条需要的更小。
#[must_use]
#[expect(
    clippy::cast_precision_loss,
    reason = "画面像素尺寸有界（远小于 f32 精度上限）"
)]
pub fn initial_window_size(remote: (u32, u32), monitor: Option<Vec2>, pixels_per_point: f32) -> Vec2 {
    let ppp = dpi(pixels_per_point);
    let want = Vec2::new(
        remote.0.max(1) as f32 / ppp,
        remote.1.max(1) as f32 / ppp,
    );
    let capped = monitor.map_or(want, |screen| want.min(screen * SCREEN_FRACTION));
    capped.max(MIN_INNER)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_preserves_aspect_ratio() {
        let size = fit_size(Vec2::new(800.0, 600.0), (1920, 1080), 1.0);
        let ratio = size.x / size.y;
        let expected = 1920.0 / 1080.0;
        assert!((ratio - expected).abs() < 1e-3, "{ratio} vs {expected}");
    }

    #[test]
    fn fit_fills_one_axis_exactly() {
        // 宽受限：结果宽度应当正好是可用宽度。
        let size = fit_size(Vec2::new(400.0, 1000.0), (1920, 1080), 1.0);
        assert!((size.x - 400.0).abs() < 1e-3);
    }

    /// 适应窗口**绝不放大**：窗口比远端大时按 1:1 物理像素画，多出来的地方留白。
    ///
    /// 放大只会把边缘插值糊掉（150% 缩放的屏幕上，旧的「远端像素 = 逻辑点」正是这么做的）。
    #[test]
    fn fit_never_upscales() {
        // 150% 屏幕：可用 2000×1500 点 = 3000×2250 物理像素，比 1920×1080 大。
        let size = fit_size(Vec2::new(2000.0, 1500.0), (1920, 1080), 1.5);
        // 1 远端像素 = 1 物理像素 = 1/1.5 逻辑点。
        assert!((size.x - 1280.0).abs() < 1e-3, "{size:?}");
        assert!((size.y - 720.0).abs() < 1e-3, "{size:?}");
    }

    /// 「适应窗口」的比例必须按**物理像素**算，而不是逻辑点。
    #[test]
    fn fit_compares_against_physical_pixels() {
        // 可用 1000 逻辑点 = 1500 物理像素；远端 3000 宽 → 物理比例 0.5。
        let size = fit_size(Vec2::new(1000.0, 2000.0), (3000, 1000), 1.5);
        // 1500 物理像素 = 1000 逻辑点，高度按比例 500 物理 = 333.33 逻辑点。
        assert!((size.x - 1000.0).abs() < 1e-3, "{size:?}");
        assert!((size.y - (500.0 / 1.5)).abs() < 1e-3, "{size:?}");
    }

    #[test]
    fn fit_survives_degenerate_inputs() {
        // 窗口尚未布局（可用区域为 0）或远端尺寸未知，都不能产生 0 或 NaN。
        for avail in [Vec2::ZERO, Vec2::new(0.0, 10.0), Vec2::new(-5.0, -5.0)] {
            let size = fit_size(avail, (0, 0), 1.5);
            assert!(size.x >= 1.0 && size.y >= 1.0, "{size:?}");
            assert!(size.x.is_finite() && size.y.is_finite(), "{size:?}");
        }
        // 非法 DPI 按 1.0 处理，不得产生 NaN。
        for ppp in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            let size = fit_size(Vec2::new(800.0, 600.0), (1920, 1080), ppp);
            assert!(size.x.is_finite() && size.y.is_finite(), "{ppp}: {size:?}");
        }
    }

    #[test]
    fn scaled_size_is_linear() {
        let size = scaled_size(0.5, (1920, 1080), 1.0);
        assert!((size.x - 960.0).abs() < 1e-3);
        assert!((size.y - 540.0).abs() < 1e-3);
    }

    /// `100%` 是「1 远端像素 : 1 **物理**像素」：150% 屏幕上 1920 宽的远端只占 1280 逻辑点。
    #[test]
    fn p100_is_one_remote_pixel_per_physical_pixel() {
        let size = scaled_size(1.0, (1920, 1080), 1.5);
        assert!((size.x - 1280.0).abs() < 1e-3, "{size:?}");
        assert!((size.y - 720.0).abs() < 1e-3, "{size:?}");
        // 100% DPI 下与原行为一致。
        let plain = scaled_size(1.0, (1920, 1080), 1.0);
        assert!((plain.x - 1920.0).abs() < 1e-3);
    }

    #[test]
    fn display_size_uses_fit_only_for_fit_variant() {
        let avail = Vec2::new(300.0, 300.0);
        let image = (1920, 1080);
        assert_eq!(
            display_size(ViewScale::Fit, avail, image, 1.0),
            fit_size(avail, image, 1.0)
        );
        // 固定档忽略可用区域：窗口再小也按比例画，溢出部分交给滚动区。
        let fixed = display_size(ViewScale::P100, avail, image, 1.0);
        assert!((fixed.x - 1920.0).abs() < 1e-3);
    }

    #[test]
    fn initial_window_size_is_capped_by_screen() {
        let size = initial_window_size((3840, 2160), Some(Vec2::new(1440.0, 900.0)), 1.0);
        assert!(size.x <= 1440.0 * SCREEN_FRACTION + 1e-3, "{size:?}");
        assert!(size.y <= 900.0 * SCREEN_FRACTION + 1e-3, "{size:?}");
    }

    /// 默认开窗按「画面 1:1 物理像素」折算成逻辑点，而不是把远端像素当逻辑点。
    #[test]
    fn initial_window_size_converts_pixels_to_points() {
        // 4K 远端、150% 屏幕：理想窗宽 2560 点（= 3840 物理像素）。
        let size = initial_window_size((3840, 2160), Some(Vec2::new(4000.0, 3000.0)), 1.5);
        assert!((size.x - 2560.0).abs() < 1e-3, "{size:?}");
        assert!((size.y - 1440.0).abs() < 1e-3, "{size:?}");
        // 1080p 远端在 150% 屏幕上只需 1280 点宽——正好是 1920 物理像素。
        let small = initial_window_size((1920, 1080), Some(Vec2::new(4000.0, 3000.0)), 1.5);
        assert!((small.x - 1280.0).abs() < 1e-3, "{small:?}");
    }

    #[test]
    fn initial_window_size_keeps_minimum_and_takes_remote_when_unknown_screen() {
        let small = initial_window_size((320, 200), Some(Vec2::new(1440.0, 900.0)), 1.0);
        assert_eq!(small, MIN_INNER);
        let unknown = initial_window_size((1600, 900), None, 1.0);
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
        assert!(WindowSize::FitScreen.inner_size(1.0).is_none());
        assert!(WindowSize::ALL
            .iter()
            .filter(|s| **s != WindowSize::FitScreen)
            .all(|s| s.inner_size(1.0).is_some()));
    }

    /// 固定窗口档位的数字是**物理像素**：150% 屏幕上 1920×1080 只占 1280×720 逻辑点。
    #[test]
    fn fixed_window_size_is_in_physical_pixels() {
        let size = WindowSize::W1920x1080.inner_size(1.5).unwrap();
        assert!((size.x - 1280.0).abs() < 1e-3, "{size:?}");
        assert!((size.y - 720.0).abs() < 1e-3, "{size:?}");
        // 100% DPI 下与标签一致。
        let plain = WindowSize::W1920x1080.inner_size(1.0).unwrap();
        assert!((plain.x - 1920.0).abs() < 1e-3);
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
