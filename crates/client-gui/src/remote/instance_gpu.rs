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
//! # 最关键的一条不变量：会话级锁存（区分「桌面阻塞」与「真实失败」）
//!
//! 编码器在**第一次编码**时按当时的采集设备创建：如果那一刻拿到的是 CPU 帧，
//! MFT 适配器就按 `gpu_input = false` 建好，之后再把 GPU 纹理交给它会命中
//! `MftAdapter::encode_frame` 的 `!self.gpu` 分支——那里会**永久禁用**进程级
//! GPU 路径。所以「本次会话能否用 GPU」必须在第一帧就定死。
//!
//! 但「第一帧没试成」有两种完全不同的原因，不能混为一谈：
//!
//! - **只是桌面不对**（会话建立时还在 `Winlogon`/`Screen-saver`，或者 worker 还没
//!   挂好、桌面未知）：**可恢复**。登录后回到 `Default` 桌面，[`SessionGpu::rearm`]
//!   把状态放回 `Unknown`，引擎经 `ScreenSource::gpu_capture_ready` 观察到后中途升级
//!   采集路径（编码任务在首张 GPU 帧上丢弃 CPU 输入编码器、重建并强制 IDR）。
//! - **真实 GPU 尝试失败 / 平台不支持 / 编码器吃不了 GPU 纹理**：**永久锁死**为
//!   `Disabled`，本会话绝不再尝试（编码器可能已经按 CPU 输入定型，赌一次就会命中
//!   上面那个永久禁用分支）。
//!
//! 一旦成功过 → `Enabled`：锁屏再解锁后继续用 GPU（编码器已经拿到设备，CPU 帧
//! 随时可编）。唯一允许「中途重新尝试」的除了 `DesktopBlocked`，就是已成功过的
//! `Enabled` 会话——那时的编码器是 GPU 设备建的，偶发失败退回 BitBlt 只是这一帧的事。

// 唯一的调用方是 Windows 专用的 `remote::win_hybrid`；非 Windows 构建里这些逻辑
// 只有测试用得到，故按 target 豁免 `dead_code`（Windows 构建上照常检查）。
#![cfg_attr(
    not(target_os = "windows"),
    allow(dead_code, reason = "只有 Windows 实例路径（win_hybrid）调用")
)]

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use super::desktop_follow::FollowDesktop;

/// `Unknown` 状态下 GPU 尝试失败的最大次数（含第 1 次）。
///
/// 登录后刚回到 `Default` 的第一拍最容易失败（WGC/DXGI 采集源还没稳定、桌面刚切换），
/// 一次失败就锁死会让整场会话永久退回 `BitBlt`——而引擎那时 `gpu_capture` 已是 `true`，
/// 不会再轮询 `gpu_capture_ready()`。
pub(crate) const GPU_RETRY_MAX_FAILURES: u32 = 3;
/// 两次 GPU 尝试之间的最短间隔（冷却期内直接走 `BitBlt`，不再去撞同一个失败）。
pub(crate) const GPU_RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// 「`Unknown` 状态下 GPU 尝试失败」的短重试预算（**纯逻辑，可单测**）。
///
/// 只给 [`SessionGpu::Unknown`] 用：真实不可用（平台禁用、编码器只能 CPU 输入）与
/// `Enabled` 会话的偶发失败都不走它——前者由调用方立即锁死，后者本帧回 `BitBlt`
/// 下一帧继续试。耗尽预算才把 `Unknown` 定格为 `Disabled`。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct GpuRetryBudget {
    /// 连续失败次数（成功一帧清零）。
    failures: u32,
    /// 冷却截止时刻：之前不再尝试。
    retry_at: Option<Instant>,
}

impl GpuRetryBudget {
    /// 全新预算（会话开始 / 桌面回到 `Default` 时）。
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 现在是否允许再试（冷却期内返回 `false`）。
    #[must_use]
    pub(crate) fn allowed(&self, now: Instant) -> bool {
        self.retry_at.is_none_or(|at| now >= at)
    }

    /// 记一次失败；返回 `true` 表示预算已耗尽（调用方应把 `Unknown` 锁成 `Disabled`）。
    pub(crate) fn note_failure(&mut self, now: Instant) -> bool {
        self.failures = self.failures.saturating_add(1);
        self.retry_at = Some(now + GPU_RETRY_INTERVAL);
        self.exhausted()
    }

    /// 成功一帧：清零（后续失败重新记账）。
    pub(crate) fn note_success(&mut self) {
        self.failures = 0;
        self.retry_at = None;
    }

    /// 桌面回到 `Default`（`rearm`）时重置。
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    /// 预算是否已耗尽。
    #[must_use]
    pub(crate) fn exhausted(&self) -> bool {
        self.failures >= GPU_RETRY_MAX_FAILURES
    }
}

/// 把采集尺寸按上限封顶（**整数、偶数对齐**，与实例 BitBlt 路径完全一致）。
///
/// 两条路径必须用同一个公式：否则同一块屏在 BitBlt 与安全桌面 DXGI 之间切换时会得到
/// 相差 1–2 像素的编码尺寸，触发编码器重建 + IDR（来回切换就是 IDR 风暴）。
/// 用**整数除法**（不是 `round`）与 `& !1` 对齐，是既有 BitBlt 行为，别改。
#[must_use]
pub(crate) fn clamp_dims(width: u32, height: u32, max: u32) -> (u32, u32) {
    if width <= max || max < 2 {
        return (width, height);
    }
    let height =
        u32::try_from(u64::from(height).saturating_mul(u64::from(max)) / u64::from(width.max(1)))
            .unwrap_or(height)
            .max(2)
            & !1;
    (max, height)
}

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
    /// 本次会话不再尝试 GPU（首次尝试失败 / 平台不支持 / 编码器吃不了 GPU 纹理）。
    /// **不可恢复**：这些原因都意味着编码器可能已按 CPU 输入定型。
    Disabled,
    /// **只是当前桌面不是 `Default` 而没试过**——可恢复。
    ///
    /// 与 `Disabled` 严格区分：登录完成后回到 `Default`，[`Self::rearm`] 会把它放回
    /// `Unknown`，引擎随即可以中途升级到 GPU 采集（见模块文档）。
    DesktopBlocked,
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
        if !capable {
            Self::Disabled
        } else if desktop == DesktopSlot::Default {
            Self::Unknown
        } else {
            // 平台有能力，但会话建立时人还没登录（Winlogon / 屏保 / 桌面未知）：
            // 只是没机会试，不是失败——登录后可以恢复。
            Self::DesktopBlocked
        }
    }

    /// 这一帧是否应该尝试 GPU。
    #[must_use]
    pub(crate) fn should_attempt(self, desktop: DesktopSlot) -> bool {
        desktop == DesktopSlot::Default && matches!(self, Self::Unknown | Self::Enabled)
    }

    /// 本帧本应尝试 GPU，但因为桌面不是 `Default`（或显示器 id 映射失败）而没试。
    ///
    /// 若还处于 `Unknown`，降级为 [`Self::DesktopBlocked`]：第一帧没拿到 GPU 纹理时
    /// 编码器会按 CPU 输入创建，但这条路径**不是失败**（桌面一回到 `Default` 就能
    /// [`Self::rearm`] 恢复），真正锁死的是 [`Self::on_gpu_failure`]。
    pub(crate) fn on_attempt_blocked(&mut self, desktop: DesktopSlot) {
        if *self == Self::Unknown && desktop != DesktopSlot::Default {
            *self = Self::DesktopBlocked;
        }
    }

    /// 桌面回到 `Default` 时调用（幂等）：把「只是没试过」的会话放回可尝试状态。
    ///
    /// 只对 [`Self::DesktopBlocked`] 生效——`Disabled`（真实失败/平台不支持）**绝不**
    /// 恢复，`Unknown`/`Enabled` 本来就是可尝试的。
    pub(crate) fn rearm(&mut self, desktop: DesktopSlot) {
        if *self == Self::DesktopBlocked && desktop == DesktopSlot::Default {
            *self = Self::Unknown;
        }
    }

    /// 一次成功的 GPU 采集。
    pub(crate) fn on_gpu_success(&mut self) {
        *self = Self::Enabled;
    }

    /// 一次失败的 GPU 采集。
    ///
    /// `Unknown` → `Disabled`（编码器还没拿到设备，不能赌下一次成功；这条**永久**，
    /// 不随桌面变化恢复）；`Enabled` → 保持 `Enabled`（编码器已经按 GPU 设备建好，
    /// 本帧退回 BitBlt，后续帧继续尝试）。
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
    /// `Disabled`（平台不支持 / 首次 GPU 尝试已失败）、`DesktopBlocked`（人还没登录，
    /// 补丁必然不可用）与非 `Default` 桌面一律 false。
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
    use super::SessionGpu::{DesktopBlocked, Disabled, Enabled, Unknown};
    use super::*;

    fn geom(x: i32, y: i32, width: u32, height: u32) -> Geometry {
        Geometry {
            x,
            y,
            width,
            height,
        }
    }

    /// 会话开始时锁屏：先只是「桌面阻塞」，登录回到 Default 后可以 rearm 恢复。
    #[test]
    fn locked_session_rearms_after_login() {
        let mut state = SessionGpu::begin(true, Secure);
        assert_eq!(state, DesktopBlocked);
        // 还在安全桌面上：不应尝试。
        assert!(!state.should_attempt(Secure));
        // 解锁后桌面变 Default，但还没 rearm —— 仍然不尝试。
        assert!(!state.should_attempt(Default));
        state.rearm(Secure);
        assert_eq!(state, DesktopBlocked, "非 Default 上的 rearm 必须无效");
        state.rearm(Default);
        assert_eq!(state, Unknown, "登录后回到 Default 应放回可尝试状态");
        assert!(state.should_attempt(Default));
    }

    /// rearm 是幂等的：已经 Unknown/Enabled 的会话不受影响。
    #[test]
    fn rearm_is_idempotent() {
        let mut unknown = Unknown;
        unknown.rearm(Default);
        assert_eq!(unknown, Unknown);
        let mut enabled = Enabled;
        enabled.rearm(Default);
        assert_eq!(enabled, Enabled);
        let mut blocked = DesktopBlocked;
        blocked.rearm(Default);
        blocked.rearm(Default);
        assert_eq!(blocked, Unknown);
    }

    /// 启动竞态：会话建立时桌面未知（`None` → `Secure`）也只算桌面阻塞，Default 出现后能恢复。
    #[test]
    fn startup_desktop_unknown_rearms_when_default_appears() {
        // `desktop_kind()` 还没发布时按安全桌面处理。
        let mut state = SessionGpu::begin(true, DesktopSlot::from_follow(None));
        assert_eq!(state, DesktopBlocked);
        state.rearm(DesktopSlot::from_follow(Some(FollowDesktop::Default)));
        assert_eq!(state, Unknown);
        assert!(state.should_attempt(Default));
    }

    /// 本应尝试但桌面不是 Default（例如正好跨在桌面切换上）：降级为桌面阻塞，可恢复。
    #[test]
    fn blocked_first_attempt_is_recoverable() {
        let mut state = SessionGpu::begin(true, Default);
        assert_eq!(state, Unknown);
        state.on_attempt_blocked(Secure);
        assert_eq!(state, DesktopBlocked);
        assert!(!state.should_attempt(Default));
        state.rearm(Default);
        assert_eq!(state, Unknown);
        assert!(state.should_attempt(Default));
    }

    /// 真实 GPU 失败：锁死为 Disabled，**任何 rearm 都不能恢复**（编码器可能已按 CPU 输入定型）。
    #[test]
    fn real_gpu_failure_never_rearms() {
        let mut state = SessionGpu::begin(true, Default);
        state.on_gpu_failure();
        assert_eq!(state, Disabled);
        state.rearm(Default);
        assert_eq!(state, Disabled, "真实失败绝不可恢复");
        assert!(!state.should_attempt(Default));
        state.on_attempt_blocked(Secure);
        assert_eq!(state, Disabled);
        state.rearm(Default);
        assert_eq!(state, Disabled);
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

    /// 平台不支持 / GPU 已被进程级禁用：一开始就是 Disabled，rearm 也不能复活。
    #[test]
    fn incapable_platform_starts_disabled() {
        let mut state = SessionGpu::begin(false, Default);
        assert_eq!(state, Disabled);
        assert!(!state.should_attempt(Default));
        assert_eq!(SessionGpu::begin(false, Secure), Disabled);
        state.rearm(Default);
        assert_eq!(state, Disabled);
    }

    /// 能力门（`supports_patches`）：`Unknown` 与 `Enabled` 在 Default 上都算可用
    /// ——引擎在 `supports_gpu_capture` 之后紧接着调用它，此时通常还没尝试第一帧。
    #[test]
    fn patch_capability_allows_unknown_and_enabled_on_default() {
        assert!(Unknown.patches_allowed(Default));
        assert!(Enabled.patches_allowed(Default));
        assert!(!Disabled.patches_allowed(Default));
        assert!(!DesktopBlocked.patches_allowed(Default));
        assert!(!Unknown.patches_allowed(Secure));
        assert!(!Enabled.patches_allowed(Secure));
        assert!(!Disabled.patches_allowed(Secure));
        assert!(!DesktopBlocked.patches_allowed(Secure));
    }

    /// 运行时门（`capture_patches`）：只有已成功过 GPU 的会话才真正调用采集。
    #[test]
    fn patch_runtime_requires_an_established_gpu_session() {
        assert!(!Unknown.patches_ready(Default));
        assert!(!Disabled.patches_ready(Default));
        assert!(!DesktopBlocked.patches_ready(Default));
        assert!(Enabled.patches_ready(Default));
        assert!(!Enabled.patches_ready(Secure));
    }

    /// 重试预算：连续 3 次失败才耗尽；期间有 500ms 冷却。
    #[test]
    fn retry_budget_locks_after_three_failures_with_cooldown() {
        let mut budget = GpuRetryBudget::new();
        let start = Instant::now();
        assert!(budget.allowed(start), "新预算应立即允许尝试");

        assert!(!budget.note_failure(start), "第 1 次失败后仍可重试");
        assert!(!budget.exhausted());
        assert!(
            !budget.allowed(start + Duration::from_millis(499)),
            "冷却期内不得重试"
        );
        assert!(
            budget.allowed(start + GPU_RETRY_INTERVAL),
            "冷却到期后应允许重试"
        );

        let second = start + GPU_RETRY_INTERVAL;
        assert!(!budget.note_failure(second), "第 2 次失败后仍可重试");
        let third = second + GPU_RETRY_INTERVAL;
        assert!(budget.note_failure(third), "第 3 次失败应耗尽预算");
        assert!(budget.exhausted());
    }

    /// 成功一帧或 rearm 都会清零，后续失败重新记账。
    #[test]
    fn retry_budget_clears_on_success_and_reset() {
        let mut budget = GpuRetryBudget::new();
        let start = Instant::now();
        assert!(!budget.note_failure(start));
        assert!(!budget.note_failure(start));
        budget.note_success();
        assert!(budget.allowed(start), "成功后冷却也一并清掉");
        // 成功后失败计数从零开始：仍要完整 3 次才耗尽。
        assert!(!budget.note_failure(start), "第 1 次失败后仍可重试");
        assert!(!budget.note_failure(start), "第 2 次失败后仍可重试");
        assert!(!budget.exhausted());
        assert!(budget.note_failure(start), "第 3 次失败应耗尽");
        assert!(budget.exhausted(), "连续 3 次失败后耗尽");
        budget.reset();
        assert!(!budget.exhausted(), "rearm 必须把已耗尽的预算恢复");
        assert!(budget.allowed(start));
    }

    /// 两条采集路径的封顶公式必须逐像素一致（否则切换会触发重建 + IDR）。
    #[test]
    fn clamp_dims_matches_the_bitblt_formula() {
        // 整数除法 + 偶数对齐：3440×1440 → 1920×802（不是 round 出来的 804）。
        assert_eq!(clamp_dims(3440, 1440, 1920), (1920, 802));
        assert_eq!(clamp_dims(2560, 1440, 1920), (1920, 1080));
        assert_eq!(clamp_dims(5120, 1440, 1920), (1920, 540));
        // 已经不超过上限：原样返回。
        assert_eq!(clamp_dims(1920, 1080, 1920), (1920, 1080));
        assert_eq!(clamp_dims(1280, 720, 1920), (1280, 720));
        // 奇数高度必须被对齐到偶数（编码器要求）。
        let (_, height) = clamp_dims(1000, 403, 500);
        assert_eq!(height % 2, 0, "封顶后高度必须偶数对齐");
        assert_eq!(clamp_dims(1000, 403, 500), (500, 200));
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
