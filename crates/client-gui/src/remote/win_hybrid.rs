//! 实例（`--instance`）的**混合采集**：`Default` 桌面走 WGC/DXGI + GPU 零拷贝，
//! 安全桌面（`Winlogon` / `Screen-saver`）与任何失败场景回退 GDI `BitBlt`。
//!
//! # 为什么需要混合
//!
//! 实例必须跟随输入桌面：锁屏/登录界面在 `WinSta0\Winlogon`，屏保在
//! `WinSta0\Screen-saver`，那里只有 GDI `BitBlt` 可靠（WGC/DXGI 在安全桌面上会
//! 拿不到帧或 `DXGI_ERROR_ACCESS_LOST`）。但如果因此**全程**用 BitBlt，普通桌面
//! 也要付 4K 全屏 GDI 回读（实测约 40 ms/帧）、CPU 帧上传编码的代价。
//!
//! 这里按**当前 worker 实际挂着的桌面**分流：`Default` → `WinVideoScreen`
//! （WGC 优先、DXGI 回退、GPU 纹理直通编码器），其余 → `WinScreen` 的 BitBlt。
//!
//! # 三条硬约束
//!
//! 1. **安全桌面绝不尝试 GPU**。那里 WGC/DXGI 拿不到帧或返回
//!    `DXGI_ERROR_ACCESS_LOST`；实例走的是严格版采集 API（失败不写进程级禁用），
//!    但白试只会浪费帧并污染遥测。
//! 2. **会话级锁存**（见 [`super::instance_gpu`]）：编码器在第一次编码时按当时的
//!    采集设备定型。第一帧没拿到 GPU 纹理，本会话就再也不能交 GPU 帧。
//! 3. **如实上报**：`capture_info()` 只报**最近一帧真正用过**的后端；BitBlt 帧
//!    之后不能继续显示 WGC/DXGI 或 `gpu_capture = true`。
//!
//! # 显示器 id 的两套体系
//!
//! 实例的 `DisplayInfo.id` 来自 `win-platform::monitor::enumerate_monitors` 的
//! 枚举下标（输入注入也用它），而 `WinVideoScreen` 内部按 xcap 的 id 查采集源。
//! 本模块用**几何匹配 + 下标兜底**把前者翻译成后者（[`instance_gpu::map_display_ids`]），
//! 并且对外的 `displays()` / `RawFrame.display` / `CapturedPatches.display` 一律
//! 保持实例 id——绝不能把 xcap id 漏给引擎。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use rust_tunnel_remote::screen::{CaptureInfo, CapturedPatches};
use rust_tunnel_remote::{DisplayInfo, RawFrame, RemoteError, RemoteResult, ScreenSource};

use super::desktop_follow::FollowDesktop;
use super::instance_gpu::{self, DesktopSlot, Geometry, LastSource, SessionGpu};
use super::win_desktop::{Shared, WinScreen};
use super::win_video::WinVideoScreen;

/// xcap 显示器 id 映射的刷新周期。
///
/// 与 `WinVideoScreen::SOURCE_RECHECK_INTERVAL` 同为 2 秒：插拔显示器/改分辨率
/// 是低频事件，2 秒的迟滞足够，又不会让每帧都枚举一遍显示器。
const XCAP_MAP_TTL: Duration = Duration::from_secs(2);

/// GPU 采集失败日志的限频间隔（失败路径可能每帧触发，不能刷屏）。
const ERROR_LOG_INTERVAL: Duration = Duration::from_secs(5);

/// 第一帧显示器 id 映射失败时的短重试次数。
///
/// 前两次**返回错误**而不是交 CPU 帧：编码器还没创建，让引擎跳过这一帧、下一帧
/// 重试；一旦交了 CPU 帧，编码器就会按 CPU 输入定型，本会话再也回不到 GPU
/// （见 [`SessionGpu`] 的锁存规则）。超过上限才 latch 成 `Disabled` 并回退 BitBlt。
const MAP_RETRY_LIMIT: u32 = 3;

/// 混合采集源（实例模式）。
pub(super) struct HybridScreen {
    /// 安全桌面 / 所有回退场景的 GDI `BitBlt` 采集源。
    bitblt: Arc<dyn ScreenSource>,
    /// 桌面跟随 worker 的共享状态（当前桌面、显示器几何）。
    io: Arc<Shared>,
    /// `Default` 桌面的 WGC/DXGI + GPU 采集源（不预打开设备）。
    gpu: WinVideoScreen,
    /// 会话锁存、桌面切换与 id 映射缓存。
    state: Mutex<HybridState>,
}

/// 跨采集调用的可变状态（`gpu` 本身的方法都只要 `&self`）。
struct HybridState {
    /// 本次会话的 GPU 可用性（见 [`SessionGpu`]）。
    session: SessionGpu,
    /// 最近一次观察到的 worker 桌面（只用于检测切换）。
    last_desktop: Option<FollowDesktop>,
    /// 实例显示器 id → xcap id。
    xcap_ids: HashMap<u32, u32>,
    /// 上次重建 `xcap_ids` 的时刻。
    xcap_checked: Option<Instant>,
    /// 上次重建映射时 win-platform 的显示器数量（数量变化立即重建）。
    xcap_monitor_count: usize,
    /// 上次 GPU 失败日志的时刻（限频）。
    last_error_log: Option<Instant>,
    /// 连续「显示器 id 映射不到 xcap」的次数（见 [`MAP_RETRY_LIMIT`]）。
    map_failures: u32,
    /// **最近一帧/一批**实际由哪个源产出。`capture_info()` 据此避免虚报后端。
    last_source: LastSource,
}

impl Default for HybridState {
    fn default() -> Self {
        Self {
            // 保守初值：`supports_patches` 若在 `supports_gpu_capture` 之前被调用，
            // 不能默认允许 GPU。
            session: SessionGpu::Disabled,
            last_desktop: None,
            xcap_ids: HashMap::new(),
            xcap_checked: None,
            xcap_monitor_count: 0,
            last_error_log: None,
            map_failures: 0,
            last_source: LastSource::BitBlt,
        }
    }
}

/// 构造实例的混合采集源。
///
/// `io` 必须已经启动监管线程（[`super::win_desktop::screen_and_input`] 创建）；
/// 本函数只持有句柄，不阻塞。
pub(super) fn screen(io: Arc<Shared>) -> Arc<dyn ScreenSource> {
    let bitblt: Arc<dyn ScreenSource> = Arc::new(WinScreen::new(Arc::clone(&io)));
    Arc::new(HybridScreen {
        bitblt,
        io,
        gpu: super::win_video::screen(),
        state: Mutex::new(HybridState::default()),
    })
}

/// 编译期断言：引擎要求 `Arc<dyn ScreenSource>`，它隐含 `Send + Sync + 'static`。
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<HybridScreen>();
};

impl HybridScreen {
    /// 加锁（容忍中毒：中毒说明某个线程 panic 过，但状态本身仍可用）。
    fn lock(&self) -> MutexGuard<'_, HybridState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 当前 worker 桌面及其采集策略分类。
    fn current(&self) -> (Option<FollowDesktop>, DesktopSlot) {
        let kind = self.io.desktop_kind();
        (kind, DesktopSlot::from_follow(kind))
    }

    /// 观察桌面变化；离开 `Default` 时释放 WGC/DXGI 采集源。
    ///
    /// **会话锁存不在这里改**：已经 `Enabled` 的会话要活过「锁屏 → 解锁」，而
    /// 第一帧就加密的决定由 [`SessionGpu::on_attempt_blocked`] 负责。
    fn observe_desktop(&self, state: &mut HybridState, kind: Option<FollowDesktop>) {
        if state.last_desktop == kind {
            return;
        }
        let previous = state.last_desktop;
        state.last_desktop = kind;
        match (previous, kind) {
            (Some(FollowDesktop::Default), other) => {
                // 离开 Default：WGC 捕获项与 DXGI duplication 绑定在当时的桌面/输出上，
                // 留着只会在解锁回来时先失败一次。
                self.gpu.reset_capture_sources();
                tracing::info!(
                    desktop = other.map_or("Unknown", FollowDesktop::as_str),
                    "输入桌面离开 Default，实例采集回退 GDI BitBlt"
                );
            }
            (_, Some(FollowDesktop::Default)) => {
                tracing::info!("输入桌面回到 Default，实例采集启用 WGC/DXGI（采集源懒惰重开）");
            }
            _ => {}
        }
    }

    /// 走 GDI `BitBlt`，并把「最近一帧是 BitBlt」记下来供遥测使用。
    ///
    /// **不要持着 `state` 锁调用**：`WinScreen::capture` 是阻塞的（正常约 40 ms，
    /// worker 卡住时最长等 8 s），持锁会让 1 Hz 心搏里的 `capture_info()` 一起卡住。
    /// 锁只在写 `last_source` 时短暂持有——此时记的是「最近一个**完成**的帧」，
    /// 语义仍然正确。
    fn blit(&self, display: u32) -> RemoteResult<RawFrame> {
        let result = self.bitblt.capture(display);
        self.lock().last_source = LastSource::BitBlt;
        result
    }

    /// 实例显示器 id → xcap id（带缓存）。
    ///
    /// 拿不到 win-platform 的几何缓存、xcap 枚举失败、或该 id 匹配不上时返回
    /// `None`——调用方必须回退 BitBlt，绝不能猜一个别的显示器。
    ///
    /// worker 还没发布几何缓存时（进程刚起、会话来得很快）在**当前线程**补一次
    /// `win-platform` 的枚举：GPU 路径只在输入桌面为 `Default` 时调用，而本线程
    /// 就在进程默认桌面上，枚举结果与 worker 发布的一致。这样不会因为第一帧抢在
    /// 缓存之前就把整个会话锁死成 CPU（见 [`SessionGpu`] 的锁存规则）。
    fn xcap_id_with(&self, state: &mut HybridState, display: u32) -> Option<u32> {
        let monitors = self
            .io
            .cached_monitors()
            .filter(|monitors| !monitors.is_empty())
            .or_else(|| {
                rust_tunnel_win_platform::monitor::enumerate_monitors()
                    .ok()
                    .filter(|monitors| !monitors.is_empty())
            })?;
        // 只按 TTL 与「显示器数量变化」重建；**不要**按「这次查询的 id 不在 map 里」
        // 重建——某个 id 真的匹配不上时，那会变成每帧枚举一次显示器。
        // 新建/热插的显示器最多等一个 TTL。
        let needs_refresh = state
            .xcap_checked
            .is_none_or(|at| at.elapsed() >= XCAP_MAP_TTL)
            || state.xcap_monitor_count != monitors.len();
        if needs_refresh {
            let candidates = enumerate_xcap();
            let monitors: Vec<(u32, Geometry)> = monitors
                .iter()
                .map(|monitor| {
                    (
                        monitor.id,
                        Geometry {
                            x: monitor.origin_x,
                            y: monitor.origin_y,
                            width: monitor.width,
                            height: monitor.height,
                        },
                    )
                })
                .collect();
            state.xcap_ids = instance_gpu::map_display_ids(&monitors, &candidates);
            state.xcap_monitor_count = monitors.len();
            // 枚举失败（空表）不缓存：下一帧立刻重试，别让一次瞬时失败把整个
            // 会话锁成 CPU（调用方还有 [`MAP_RETRY_LIMIT`] 的短重试）。
            state.xcap_checked = (!candidates.is_empty()).then(Instant::now);
        }
        state.xcap_ids.get(&display).copied()
    }
}

/// xcap 的 `(id, 几何)` 列表；失败时返回空表（GPU 路径本拍不可用，回退 BitBlt）。
fn enumerate_xcap() -> Vec<(u32, Geometry)> {
    match xcap::Monitor::all() {
        Ok(monitors) => monitors
            .iter()
            .filter_map(|monitor| {
                let id = monitor.id().ok()?;
                Some((
                    id,
                    Geometry {
                        x: monitor.x().ok()?,
                        y: monitor.y().ok()?,
                        width: monitor.width().ok()?,
                        height: monitor.height().ok()?,
                    },
                ))
            })
            .collect(),
        Err(err) => {
            tracing::debug!(error = %err, "xcap 枚举显示器失败，实例 GPU 采集暂不可用");
            Vec::new()
        }
    }
}

impl ScreenSource for HybridScreen {
    /// 实例 id 的显示器列表（**只读 win-platform 的缓存**，绝不返回 xcap id）。
    fn displays(&self) -> Vec<DisplayInfo> {
        self.bitblt.displays()
    }

    /// 编码宽度上限只对 GPU 源有意义（BitBlt 路径固定按采集尺寸，不做 4K）。
    fn set_max_encode_width(&self, width: u32) {
        self.gpu.set_max_encode_width(width);
    }

    /// CPU 采集路径（VP9 会话、或未启用 GPU 的 H.264 会话）。
    ///
    /// `Default` 上也优先用 WGC/DXGI 的 CPU 回读（比 GDI 快一个数量级），但**不**
    /// 走 GPU 纹理路径——这条路径不承诺零拷贝，失败也不该影响进程级 GPU 状态。
    // 参数名刻意用 `display_id` 而不是 `display`：`tracing` 的宏展开里有 `display`
    // 这个函数名，宏参数里直接写 `display` 会被它盖住（见
    // `win_desktop::events_to_steps` 的同款注释）。
    fn capture(&self, display_id: u32) -> RemoteResult<RawFrame> {
        // 进程级 GPU 路径被禁用（环境变量 `RUST_TUNNEL_DISABLE_GPU_CAPTURE` 或一次
        // 真实失败）时连 WGC 的 CPU 回读也不走——现场需要一个**确定**的 BitBlt 兜底。
        if !self.gpu.supports_gpu_capture() {
            return self.blit(display_id);
        }
        let (kind, slot) = self.current();
        // 只在**决定用哪个源**时持锁；真正的采集（可能 40 ms 以上）在锁外做，
        // 否则 1 Hz 心搏里的 `capture_info()` 会被一次采集拖住。
        let mapped = {
            let mut state = self.lock();
            self.observe_desktop(&mut state, kind);
            if slot == DesktopSlot::Default {
                let mapped = self.xcap_id_with(&mut state, display_id);
                if mapped.is_none() {
                    tracing::debug!(display_id, "实例显示器 id 无法映射到 xcap，回退 GDI BitBlt");
                }
                mapped
            } else {
                None
            }
        };
        let Some(mapped) = mapped else {
            return self.blit(display_id);
        };
        match self.gpu.capture(mapped) {
            Ok(mut frame) => {
                // 对引擎必须始终是实例 id（输入/关键帧请求都用它）。
                frame.display = display_id;
                self.lock().last_source = LastSource::GpuSource;
                Ok(frame)
            }
            Err(err) => {
                tracing::debug!(
                    error = %err,
                    display_id,
                    "WGC/DXGI CPU 采集失败，回退 GDI BitBlt"
                );
                self.blit(display_id)
            }
        }
    }

    /// 会话开始时的 GPU 能力判定（引擎每会话调用一次）。
    ///
    /// 只有「平台支持 **且** 当前在 `Default`」才返回 true；返回 true 时把会话
    /// 状态置为 `Unknown`，等第一帧的实际结果把它定格。
    fn supports_gpu_capture(&self) -> bool {
        let (kind, slot) = self.current();
        let capable = self.gpu.supports_gpu_capture();
        let mut state = self.lock();
        self.observe_desktop(&mut state, kind);
        state.session = SessionGpu::begin(capable, slot);
        state.map_failures = 0;
        capable && slot == DesktopSlot::Default
    }

    /// GPU 纹理采集（H.264 + 会话允许时由引擎调用）。
    fn capture_gpu(&self, display_id: u32) -> RemoteResult<RawFrame> {
        let (kind, slot) = self.current();
        let mapped = {
            let mut state = self.lock();
            self.observe_desktop(&mut state, kind);
            // 进程级 GPU 路径可能在会话中途被禁用（环境变量/一次真实失败）。那时
            // 编码器下一次重建会按 CPU 输入创建，绝不能再交 GPU 帧。
            if !self.gpu.supports_gpu_capture() {
                if state.session != SessionGpu::Disabled {
                    tracing::info!("GPU 采集路径已被进程级禁用，实例本会话回退 GDI BitBlt");
                }
                state.session = SessionGpu::Disabled;
            }
            // 编码器工厂可能只能建 CPU 输入适配器（非 Intel 采集设备 /
            // `new_with_device` 失败）。它会把 hint 置 false；本会话从这一帧起
            // 改交 CPU 帧，否则 `MftAdapter::encode_frame` 的 `!self.gpu` 会拒绝。
            if !super::win_video::encode_gpu_input_available() {
                if state.session != SessionGpu::Disabled {
                    tracing::info!("编码器只能接受 CPU 输入，实例本会话停止交 GPU 帧");
                }
                state.session = SessionGpu::Disabled;
            }
            if state.session.should_attempt(slot) {
                if let Some(mapped) = self.xcap_id_with(&mut state, display_id) {
                    state.map_failures = 0;
                    Some(mapped)
                } else {
                    state.map_failures = state.map_failures.saturating_add(1);
                    if state.session == SessionGpu::Unknown && state.map_failures < MAP_RETRY_LIMIT
                    {
                        // 第一帧还没交出去：返回错误让引擎跳过这一帧、下一帧重试。
                        // 绝不能交 CPU 帧——那会让编码器按 CPU 输入定型，本会话再也
                        // 回不到 GPU。
                        return Err(RemoteError::Platform(
                            "显示器 id 映射暂不可用，下一帧重试".to_owned(),
                        ));
                    }
                    // 重试耗尽（或已经 Enabled）：latch 成 CPU / 本帧回退 BitBlt。
                    if state.session == SessionGpu::Unknown {
                        state.session = SessionGpu::Disabled;
                    }
                    tracing::debug!(
                        display_id,
                        "实例显示器 id 无法映射到 xcap，GPU 采集回退 GDI BitBlt"
                    );
                    None
                }
            } else {
                // 本会话已被锁死为 CPU（或当前在安全桌面）：绝不能交 GPU 帧。
                state.session.on_attempt_blocked(slot);
                None
            }
        };
        let Some(mapped) = mapped else {
            return self.blit(display_id);
        };
        // 严格版：失败不写进程级禁用状态、也不自行回退 xcap——回退目标是 BitBlt。
        // **不持锁**做实际的 GPU 采集（可能几十 ms），见 `blit` 的同款说明。
        match self.gpu.capture_gpu_strict(mapped) {
            Ok(mut frame) => {
                frame.display = display_id;
                let mut state = self.lock();
                state.session.on_gpu_success();
                state.last_error_log = None;
                state.last_source = LastSource::GpuSource;
                Ok(frame)
            }
            Err(err) => {
                let log = {
                    let mut state = self.lock();
                    state.session.on_gpu_failure();
                    let log = state
                        .last_error_log
                        .is_none_or(|at| at.elapsed() >= ERROR_LOG_INTERVAL);
                    if log {
                        state.last_error_log = Some(Instant::now());
                    }
                    log
                };
                if log {
                    tracing::warn!(
                        error = %err,
                        display_id,
                        "实例 GPU 采集失败，本帧回退 GDI BitBlt"
                    );
                }
                self.blit(display_id)
            }
        }
    }

    /// 脏矩形补丁只在「本会话已成功用过 GPU 且当前在 Default」时开放。
    fn supports_patches(&self) -> bool {
        let (kind, slot) = self.current();
        let mut state = self.lock();
        self.observe_desktop(&mut state, kind);
        let allowed = state.session.patches_allowed(slot);
        drop(state);
        allowed && self.gpu.supports_patches()
    }

    /// 脏矩形补丁采集；不适用时返回 `Ok(None)` 让引擎回退全帧。
    fn capture_patches(
        &self,
        display_id: u32,
        scale: f32,
        max_width: u32,
    ) -> RemoteResult<Option<CapturedPatches>> {
        let (kind, slot) = self.current();
        let mut state = self.lock();
        self.observe_desktop(&mut state, kind);
        // 能力门（`supports_patches`）允许 `Unknown`，但真正调用采集必须等第一帧
        // GPU 成功把会话定格成 `Enabled`——否则补丁会跑在还没建立的 GPU 设备上。
        // 进程级 GPU 被禁用时同样立刻停掉补丁（编码器可能已按 CPU 输入重建）。
        if !state.session.patches_ready(slot) || !self.gpu.supports_patches() {
            return Ok(None);
        }
        let Some(mapped) = self.xcap_id_with(&mut state, display_id) else {
            tracing::debug!(display_id, "实例显示器 id 无法映射到 xcap，补丁采集跳过");
            return Ok(None);
        };
        drop(state);
        match self.gpu.capture_patches(mapped, scale, max_width) {
            Ok(Some(mut patches)) => {
                // 与整帧路径同理：对引擎只暴露实例 id。
                patches.display = display_id;
                self.lock().last_source = LastSource::GpuSource;
                Ok(Some(patches))
            }
            Ok(None) => Ok(None),
            Err(err) => {
                tracing::debug!(
                    error = %err,
                    display_id,
                    "GPU 补丁采集失败，回退全帧"
                );
                Ok(None)
            }
        }
    }

    /// 遥测口径：只报**最近一帧真正用过**的后端。
    ///
    /// GPU 源（WGC/DXGI，含它的 CPU 回读路径）返回 `WinVideoScreen` 的完整信息
    /// ——它的 `gpu_capture` 标志本身只表示「最近一帧是否真的走了 GPU 纹理」。
    /// BitBlt 路径返回 BitBlt，但仍带上 `gpu_disabled` / 原因，方便现场判断
    /// 「为什么没用上 GPU」。**绝不会**在 BitBlt 帧之后继续显示 WGC/DXGI。
    ///
    /// 安全桌面上 `gpu_disabled` 通常是 `false`——那表示「本桌面不适用 GPU」，
    /// 不是「GPU 健康」；判据是 `backend` 字段。
    ///
    /// **本方法必须保持非阻塞**：它由 1 Hz 心搏调用，而 `observe_desktop` 在桌面
    /// 切换那一拍会去清 WGC/DXGI 采集源（持源锁等一次在途采集）。桌面切换的清理由
    /// 下一条采集路径负责。
    fn capture_info(&self) -> CaptureInfo {
        let (_, slot) = self.current();
        let last_source = self.lock().last_source;
        let gpu = self.gpu.capture_info();
        if instance_gpu::info_source(last_source, slot) == LastSource::GpuSource {
            return gpu;
        }
        let mut info = self.bitblt.capture_info();
        info.gpu_disabled = gpu.gpu_disabled;
        info.gpu_disabled_reason = gpu.gpu_disabled_reason;
        // 保留 GPU 源的累计计数：引擎按秒求差，切到 BitBlt 时若报 0，切回 GPU 的
        // 那一秒会把整个累计值当成一秒增量（「回退 N/s」尖峰）。
        info.fallbacks = gpu.fallbacks;
        info.no_new_frame = gpu.no_new_frame;
        info.open_fallbacks = gpu.open_fallbacks;
        info
    }
}
