//! 实例模式（无人值守服务）GPU 采集的**纯逻辑策略**（无任何平台/Windows 依赖）。
//!
//! # 为什么单独抽出来
//!
//! 实例的采集要在两条路径间切换：`Default` 桌面走 WGC/DXGI + GPU 零拷贝，
//! `Winlogon` / `Screen-saver` 等安全桌面只能走 GDI `BitBlt`。切换判断、会话内
//! 的 GPU 可用性锁存、显示器 id 映射都与 Win32 无关，抽成纯逻辑后可以在**所有
//! 宿主**上单测——`crates/client-gui` 里 `#[cfg(target_os = "windows")]` 的代码
//! 在 macOS/Linux 上根本不参与编译。
//!
//! # 最关键的一条不变量：会话级锁存
//!
//! 编码器在**第一次编码**时按当时的采集设备创建：如果那一刻拿到的是 CPU 帧，
//! MFT 适配器就按 `gpu_input = false` 建好，之后再把 GPU 纹理交给它会命中
//! `MftAdapter::encode_frame` 的 `!self.gpu` 分支——那里会**永久禁用**进程级
//! GPU 路径。所以「本次会话能否用 GPU」必须在第一帧就定死：
//!
//! - 会话开始时不在 `Default`（锁屏/屏保）→ 整个会话都不再尝试 GPU；
//! - 第一次 GPU 尝试失败 → 整个会话都不再尝试 GPU；
//! - 一旦成功过 → 锁屏再解锁后继续用 GPU（编码器已经拿到设备，CPU 帧随时可编）。
//!
//! 唯一允许「中途重新尝试」的是**已经成功过**的会话：那时的编码器是 GPU 设备
//! 建的，偶发失败退回 BitBlt 只是这一帧的事。

// 唯一的调用方是 Windows 专用的 `remote::win_hybrid`；非 Windows 构建里这些逻辑
// 只有测试用得到，故按 target 豁免 `dead_code`（Windows 构建上照常检查）。
#![cfg_attr(
    not(target_os = "windows"),
    allow(dead_code, reason = "只有 Windows 实例路径（win_hybrid）调用")
)]

use std::collections::{HashMap, HashSet};

use super::desktop_follow::FollowDesktop;

/// 采集策略眼里的桌面类别。
///
/// **只有 `Default` 是 GPU 路径**；`None`（尚未挂载/探测失败）、`Winlogon`、
/// `Screen-saver`、`Unsupported` 一律按安全桌面处理——宁可多走一帧 BitBlt，
/// 也不要在安全桌面上尝试 WGC/DXGI（那里 `DXGI_ERROR_ACCESS_LOST` 是常态，
/// 白试只会浪费帧；实例用的严格版采集 API 不会因此永久禁用进程级 GPU）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DesktopSlot {
    /// `WinSta0\Default`：已登录、未锁屏，GPU 采集可用。
    Default,
    /// 安全桌面 / 未知桌面：只用 GDI `BitBlt`。
    Secure,
}

impl DesktopSlot {
    /// 由桌面跟随状态机的分类结果映射。
    #[must_use]
    pub(crate) fn from_follow(kind: Option<FollowDesktop>) -> Self {
        match kind {
            Some(FollowDesktop::Default) => Self::Default,
            _ => Self::Secure,
        }
    }
}

/// 本次会话的 GPU 可用性（见模块文档的锁存规则）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionGpu {
    /// 本次会话不再尝试 GPU（开始就不在 Default / 首次尝试失败 / 平台不支持）。
    Disabled,
    /// 允许尝试，但**还没有成功过**。第一次尝试的结果会把它定格成
    /// `Enabled` 或 `Disabled`。
    Unknown,
    /// 已经成功用 GPU 采过帧；编码器是按 GPU 设备创建的，锁屏/解锁后仍可继续用。
    Enabled,
}

impl SessionGpu {
    /// 会话开始（`ScreenSource::supports_gpu_capture`）时初始化。
    #[must_use]
    pub(crate) fn begin(capable: bool, desktop: DesktopSlot) -> Self {
        if capable && desktop == DesktopSlot::Default {
            Self::Unknown
        } else {
            Self::Disabled
        }
    }

    /// 这一帧是否应该尝试 GPU。
    #[must_use]
    pub(crate) fn should_attempt(self, desktop: DesktopSlot) -> bool {
        desktop == DesktopSlot::Default && matches!(self, Self::Unknown | Self::Enabled)
    }

    /// 本帧本应尝试 GPU，但因为桌面不是 `Default`（或显示器 id 映射失败）而没试。
    ///
    /// 若还处于 `Unknown`，必须立刻锁死为 `Disabled`：第一帧没拿到 GPU 纹理，
    /// 编码器就会按 CPU 输入创建，之后再交 GPU 帧会触发永久禁用。
    pub(crate) fn on_attempt_blocked(&mut self, desktop: DesktopSlot) {
        if *self == Self::Unknown && desktop != DesktopSlot::Default {
            *self = Self::Disabled;
        }
    }

    /// 一次成功的 GPU 采集。
    pub(crate) fn on_gpu_success(&mut self) {
        *self = Self::Enabled;
    }

    /// 一次失败的 GPU 采集。
    ///
    /// `Unknown` → `Disabled`（编码器还没拿到设备，不能赌下一次成功）；
    /// `Enabled` → 保持 `Enabled`（编码器已经按 GPU 设备建好，本帧退回 BitBlt，
    /// 后续帧继续尝试）。
    pub(crate) fn on_gpu_failure(&mut self) {
        if *self == Self::Unknown {
            *self = Self::Disabled;
        }
    }

    /// 会话级**能力**：本会话是否可能用上脏矩形补丁。
    ///
    /// 引擎在 `supports_gpu_capture()` 之后**紧接着**调用 `supports_patches()`，
    /// 此时通常还是 `Unknown`（Default 桌面、第一帧 GPU 尚未尝试）。所以这里
    /// `Unknown` 与 `Enabled` 都算「可能可用」；真正的运行时门槛在
    /// [`Self::patches_ready`]——补丁状态机要连续多拍低活跃才会进入，那时第一帧
    /// GPU 的结果早已把状态定格。
    ///
    /// `Disabled`（会话开始就锁屏 / 首次 GPU 尝试已失败）与非 `Default` 一律 false。
    #[must_use]
    pub(crate) fn patches_allowed(self, desktop: DesktopSlot) -> bool {
        desktop == DesktopSlot::Default && matches!(self, Self::Unknown | Self::Enabled)
    }

    /// 运行时**放行**：只有已经真正成功采过一帧 GPU、且当前在 `Default` 才允许
    /// 调用 `capture_patches`。
    ///
    /// `Unknown` 时第一帧还没建立 GPU 设备，必须先走全帧视频把会话状态定下来。
    #[must_use]
    pub(crate) fn patches_ready(self, desktop: DesktopSlot) -> bool {
        self == Self::Enabled && desktop == DesktopSlot::Default
    }
}

/// **最近一帧/一批**实际由哪个采集源产出。
///
/// `GpuSource` 同时覆盖 WGC/DXGI 的 GPU 纹理路径与它的 CPU 回读路径——两者都是
/// `WinVideoScreen`，区别由它自己的 `gpu_capture` 标志表达。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum LastSource {
    /// 最近一帧来自 GDI `BitBlt`（`WinScreen`）。
    #[default]
    BitBlt,
    /// 最近一帧来自 WGC/DXGI（`WinVideoScreen`）。
    GpuSource,
}

/// 遥测（`capture_info`）该按哪个源的读数上报。
///
/// 只有「最近一帧确实来自 GPU 源 **且** 当前仍在 `Default`」才报 GPU 源的后端；
/// 其余一律报 BitBlt。这样绝不会在 BitBlt 帧之后继续显示 WGC/DXGI。
#[must_use]
pub(crate) fn info_source(last: LastSource, desktop: DesktopSlot) -> LastSource {
    match (last, desktop) {
        (LastSource::GpuSource, DesktopSlot::Default) => LastSource::GpuSource,
        _ => LastSource::BitBlt,
    }
}

/// 一块显示器的几何（虚拟桌面坐标，物理像素）。
///
/// 与 `win-video` 的 `Geometry`、`win-platform` 的 `Monitor` 字段一一对应；
/// 纯逻辑模块不依赖它们，便于跨平台单测。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Geometry {
    /// 左边界。
    pub x: i32,
    /// 上边界。
    pub y: i32,
    /// 宽。
    pub width: u32,
    /// 高。
    pub height: u32,
}

/// 把**实例显示器 id** 映射到 **xcap 显示器 id**。
///
/// 实例的 `DisplayInfo.id` 来自 `win-platform::monitor::enumerate_monitors`
/// 的枚举下标，而 `WinVideoScreen` 内部用 xcap 的 id 查采集源。两套枚举都来自
/// `EnumDisplayMonitors`，顺序通常一致，但不能假设：几何匹配优先，枚举下标兜底
/// （DPI 感知模式不同可能让坐标取整略有差异）。**同一条候选只能被一个实例 id
/// 认领**——否则两套顺序不一致时，两个显示器会映射到同一块屏，控制器选其中一个
/// 会看到另一个显示器的画面。
///
/// 返回的 map 只包含能匹配上的 id；调用方遇到缺项必须退回 BitBlt。
#[must_use]
pub(crate) fn map_display_ids(
    monitors: &[(u32, Geometry)],
    candidates: &[(u32, Geometry)],
) -> HashMap<u32, u32> {
    let mut map = HashMap::with_capacity(monitors.len());
    let mut claimed = HashSet::new();
    for (index, (id, geometry)) in monitors.iter().enumerate() {
        let matched = candidates
            .iter()
            .find(|(candidate_id, candidate)| {
                candidate == geometry && !claimed.contains(candidate_id)
            })
            .or_else(|| {
                candidates
                    .get(index)
                    .filter(|(candidate_id, _)| !claimed.contains(candidate_id))
            });
        if let Some((candidate_id, _)) = matched {
            claimed.insert(*candidate_id);
            map.insert(*id, *candidate_id);
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::DesktopSlot::{Default, Secure};
    use super::SessionGpu::{Disabled, Enabled, Unknown};
    use super::*;

    fn geom(x: i32, y: i32, width: u32, height: u32) -> Geometry {
        Geometry {
            x,
            y,
            width,
            height,
        }
    }

    /// 会话开始时锁屏：整个会话都不再尝试 GPU（哪怕中途解锁）。
    #[test]
    fn locked_session_never_enables_gpu() {
        let mut state = SessionGpu::begin(true, Secure);
        assert_eq!(state, Disabled);
        // 解锁后桌面变 Default，也不能再尝试。
        assert!(!state.should_attempt(Default));
        state.on_attempt_blocked(Default);
        assert_eq!(state, Disabled);
    }

    /// 第一次尝试时桌面已经不在 Default：锁死为 Disabled，后续解锁也不改。
    #[test]
    fn blocked_first_attempt_latches_disabled() {
        let mut state = SessionGpu::begin(true, Default);
        assert_eq!(state, Unknown);
        state.on_attempt_blocked(Secure);
        assert_eq!(state, Disabled);
        assert!(!state.should_attempt(Default));
    }

    /// 第一次 GPU 尝试失败：锁死为 Disabled（编码器会按 CPU 输入创建）。
    #[test]
    fn first_gpu_failure_latches_disabled() {
        let mut state = SessionGpu::begin(true, Default);
        state.on_gpu_failure();
        assert_eq!(state, Disabled);
        assert!(!state.should_attempt(Default));
    }

    /// 成功过之后：锁屏不尝试、解锁后继续尝试，且锁屏不会把 Enabled 降级。
    #[test]
    fn enabled_survives_lock_and_unlock() {
        let mut state = SessionGpu::begin(true, Default);
        state.on_gpu_success();
        assert_eq!(state, Enabled);
        assert!(!state.should_attempt(Secure));
        state.on_attempt_blocked(Secure);
        assert_eq!(state, Enabled, "锁屏不能把已成功的会话降级");
        assert!(state.should_attempt(Default));
        // 已成功的会话里偶发一次失败：保持 Enabled，下一帧继续试。
        state.on_gpu_failure();
        assert_eq!(state, Enabled);
        assert!(state.should_attempt(Default));
    }

    /// 平台不支持 / GPU 已被进程级禁用：一开始就是 Disabled。
    #[test]
    fn incapable_platform_starts_disabled() {
        assert_eq!(SessionGpu::begin(false, Default), Disabled);
        assert!(!SessionGpu::begin(false, Default).should_attempt(Default));
    }

    /// 能力门（`supports_patches`）：`Unknown` 与 `Enabled` 在 Default 上都算可用
    /// ——引擎在 `supports_gpu_capture` 之后紧接着调用它，此时通常还没尝试第一帧。
    #[test]
    fn patch_capability_allows_unknown_and_enabled_on_default() {
        assert!(Unknown.patches_allowed(Default));
        assert!(Enabled.patches_allowed(Default));
        assert!(!Disabled.patches_allowed(Default));
        assert!(!Unknown.patches_allowed(Secure));
        assert!(!Enabled.patches_allowed(Secure));
        assert!(!Disabled.patches_allowed(Secure));
    }

    /// 运行时门（`capture_patches`）：只有已成功过 GPU 的会话才真正调用采集。
    #[test]
    fn patch_runtime_requires_an_established_gpu_session() {
        assert!(!Unknown.patches_ready(Default));
        assert!(!Disabled.patches_ready(Default));
        assert!(Enabled.patches_ready(Default));
        assert!(!Enabled.patches_ready(Secure));
    }

    /// 遥测只在「最近一帧来自 GPU 源 + 当前 Default」时报 GPU 源；BitBlt 之后
    /// 绝不能继续显示 WGC/DXGI，离开 Default 也立即按 BitBlt 报。
    #[test]
    fn telemetry_never_reports_gpu_after_bitblt() {
        assert_eq!(info_source(LastSource::BitBlt, Default), LastSource::BitBlt);
        assert_eq!(
            info_source(LastSource::GpuSource, Default),
            LastSource::GpuSource
        );
        assert_eq!(
            info_source(LastSource::GpuSource, Secure),
            LastSource::BitBlt
        );
        assert_eq!(info_source(LastSource::BitBlt, Secure), LastSource::BitBlt);
    }

    /// 几何匹配优先于枚举下标：两套枚举顺序不同也能对上。
    #[test]
    fn maps_by_geometry_even_when_order_differs() {
        let monitors = [(0, geom(-1920, 0, 1920, 1080)), (1, geom(0, 0, 2560, 1440))];
        // xcap 顺序相反。
        let candidates = [
            (42, geom(0, 0, 2560, 1440)),
            (7, geom(-1920, 0, 1920, 1080)),
        ];
        let map = map_display_ids(&monitors, &candidates);
        assert_eq!(map.get(&0), Some(&7));
        assert_eq!(map.get(&1), Some(&42));
    }

    /// 几何对不上（例如 DPI 感知差异让坐标取整不同）时按下标兜底。
    #[test]
    fn falls_back_to_enumeration_index() {
        let monitors = [(0, geom(0, 0, 3840, 2160))];
        let candidates = [(99, geom(0, 0, 2560, 1440))];
        let map = map_display_ids(&monitors, &candidates);
        assert_eq!(map.get(&0), Some(&99));
    }

    /// 候选列表比实例少时，多出来的实例显示器不进 map（调用方退回 BitBlt）。
    #[test]
    fn leaves_unmatched_displays_out() {
        let monitors = [(0, geom(0, 0, 1920, 1080)), (1, geom(1920, 0, 1920, 1080))];
        let candidates = [(5, geom(0, 0, 1920, 1080))];
        let map = map_display_ids(&monitors, &candidates);
        assert_eq!(map.len(), 1);
        assert_eq!(map.get(&0), Some(&5));
        assert!(!map.contains_key(&1));
    }

    /// 镜像显示器（几何相同）只有一条候选时，第二个实例 id 不能重复认领同一条
    /// 候选——否则控制器切到「显示器 2」看到的还是显示器 1 的画面。
    #[test]
    fn a_candidate_is_claimed_by_only_one_display() {
        let geometry = geom(0, 0, 1920, 1080);
        let monitors = [(0, geometry), (1, geometry)];
        let candidates = [(7, geometry)];
        let map = map_display_ids(&monitors, &candidates);
        assert_eq!(map.get(&0), Some(&7));
        assert!(
            !map.contains_key(&1),
            "同一条 xcap 候选不能被两个显示器共用"
        );
    }
}
