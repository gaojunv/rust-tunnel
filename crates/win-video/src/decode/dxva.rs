//! DXVA/D3D11 硬件解码的 D3D11 侧支持。
//!
//! # 为什么需要设备管理器
//!
//! `MSH264DecoderMFT` 只有在收到 `MFT_MESSAGE_SET_D3D_MANAGER`（且此前已用
//! `MFCreateDXGIDeviceManager` + `ResetDevice` 绑定好 D3D11 设备）时才会走
//! DXVA 硬件解码，输出 `IMFDXGIBuffer` 而不是系统内存 NV12。少了这一步，
//! 解码完全落在 CPU 上——这正是 Intel HD 630 控制端 CPU 占用高的原因。
//!
//! # 为什么还要 staging 拷贝
//!
//! DXGI 输出是 GPU 纹理，CPU 读不了；本模块把它 `CopySubresourceRegion` 到
//! 一块 `D3D11_USAGE_STAGING` 的 NV12 纹理，再 `Map` 拿到 `RowPitch`。解码本身
//! 已由 GPU 完成，这里剩下的只是一次 NV12 回读 + BT.709→RGBA 转换。
//!
//! # 适配器选择
//!
//! 优先 Intel 核显（用户场景 HD 630，且多数笔记本的显示输出挂在核显上），
//! 没有核显时退回任意非软件适配器。设备必须用
//! `D3D11_CREATE_DEVICE_VIDEO_SUPPORT`（见 [`crate::d3d11::create`]）。

use super::{decode_err, elapsed_us, mf_api};
use crate::d3d11::{self, D3d11};
use crate::WinVideoError;
use std::ffi::c_void;
use std::ptr;
use windows::core::Interface as _;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Query, ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_FLAG_DO_NOT_WAIT, D3D11_MAP_READ, D3D11_QUERY_DESC, D3D11_QUERY_EVENT,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11_ASYNC_GETDATA_DONOTFLUSH,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::DXGI_ERROR_WAS_STILL_DRAWING;
use windows::Win32::Media::MediaFoundation::{
    IMFDXGIBuffer, IMFDXGIDeviceManager, IMFTransform, MFT_MESSAGE_SET_D3D_MANAGER,
};

/// `Map` 的等待方式（诊断开关，默认 = 生产行为）。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum MapMode {
    /// `D3D11_MAP_READ` 直接阻塞（当前生产行为）。
    #[default]
    Blocking,
    /// `D3D11_MAP_FLAG_DO_NOT_WAIT` 轮询 + `ID3D11Query(D3D11_QUERY_EVENT)` 显式等待。
    ///
    /// 用来区分「`Map` 内部的驱动等待」与「等待 GPU 完成」本身：若两者耗时一致，
    /// 说明卡的是 GPU 完成时间而不是 `Map` 的实现。
    PollEvent,
}

/// 回读预算的默认上限（微秒）：100 ms。
///
/// 正常回读是 1–10 ms（p95 也在 10 ms 量级），只有真机那种「GPU 空闲后拷贝
/// 要 400+ ms 才完成」的病态情况才会撞到它。撞到就放弃这一帧，而不是把解码任务
/// 卡满 445 ms（后者会把控制器的 4 帧解码队列塞满，触发整条自锁闭环）。
pub const DEFAULT_READBACK_BUDGET_US: u64 = 100_000;

/// 回读预算的环境变量（毫秒，`0` = 关闭上限 = 旧行为）。
pub const READBACK_BUDGET_ENV: &str = "RUST_TUNNEL_READBACK_BUDGET_MS";

/// 回读预算的硬上限（微秒，5 s）。
///
/// 预算会用来算 `Instant + Duration`；环境变量给出荒谬值时先夹住，避免溢出 panic。
pub const MAX_READBACK_BUDGET_US: u64 = 5_000_000;

/// 解析回读预算（毫秒字符串 → 微秒）：缺省/非法用默认，`0` 表示不设上限。
pub(crate) fn parse_readback_budget_us(raw: Option<&str>) -> u64 {
    match raw.and_then(|value| value.trim().parse::<u64>().ok()) {
        Some(ms) => ms.saturating_mul(1000).min(MAX_READBACK_BUDGET_US),
        None => DEFAULT_READBACK_BUDGET_US,
    }
}

/// 进程当前的回读预算（微秒）；`0` = 不设上限。
pub(crate) fn readback_budget_us_from_env() -> u64 {
    parse_readback_budget_us(std::env::var(READBACK_BUDGET_ENV).ok().as_deref())
}

/// 拷贝前的预热方式（诊断开关，默认 [`WarmUp::None`]）。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum WarmUp {
    /// 不预热（当前生产行为）。
    #[default]
    None,
    /// 正式拷贝前先发一个 16x16 的 `CopySubresourceRegion` + `Flush`。
    ///
    /// 用来判断「设备空闲后第一条命令的唤醒耗时」能否被一条**异步**小命令吃掉：
    /// 预热命令不等结果，正式拷贝紧跟其后。
    ///
    /// **真机实测：无效**（停顿照旧），保留它是为了让探针能复现这个对照。
    OnDemandCopy,
}

/// DXVA 回读的诊断选项。
///
/// 默认值里除了 [`Self::readback_budget_us`] 是新增的**有界等待**之外，其余字段
/// 都逐字节等同于本项目一直以来的行为；`decode_stall_probe` 用它在真机上逐项 A/B。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadbackOptions {
    /// `CopySubresourceRegion` 之前先 `ID3D11DeviceContext::Flush`。
    pub flush_before_copy: bool,
    /// `Map` 的等待方式。
    pub map_mode: MapMode,
    /// 每帧重建 staging 纹理（而不是按尺寸复用）。
    pub recreate_staging: bool,
    /// 拷贝前的预热方式。
    pub warm_up: WarmUp,
    /// 回读（拷贝 + `Map`）的时间预算，微秒；`0` = 不设上限（旧行为）。
    ///
    /// 超预算时**放弃这一帧**（`decode` 返回 `Ok(None)`），而不是继续阻塞。
    pub readback_budget_us: u64,
}

impl ReadbackOptions {
    /// 生产默认：行为不变 + 回读预算（可由 [`READBACK_BUDGET_ENV`] 覆盖）。
    #[must_use]
    pub fn production() -> Self {
        Self {
            readback_budget_us: readback_budget_us_from_env(),
            ..Self::default()
        }
    }

    /// 不设回读预算（等价于本项目修复前的行为，探针用）。
    #[must_use]
    pub fn unbounded() -> Self {
        Self {
            readback_budget_us: 0,
            ..Self::default()
        }
    }
}

impl Default for ReadbackOptions {
    fn default() -> Self {
        Self {
            flush_before_copy: false,
            map_mode: MapMode::Blocking,
            recreate_staging: false,
            warm_up: WarmUp::None,
            readback_budget_us: DEFAULT_READBACK_BUDGET_US,
        }
    }
}

/// 一次回读的等待策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReadbackPolicy {
    /// 阻塞等到 GPU 完成（旧行为）。
    Blocking,
    /// 有界等待：超预算就放弃这一帧（`decode` 返回 `Ok(None)`）。
    Bounded,
}

/// 决定这一次 `Map` 用哪种等待策略（纯函数，便于单测锁住）。
///
/// 规则：
/// 1. **含 IDR 的访问单元永远阻塞**——补丁模式（协议 v6 `RegionUpdate`）的基准帧
///    就是那张 IDR：控制端若把它跳掉，`FrameSink::on_frame` 不会发生，合成缓冲还
///    停在旧基准上，而对端随后发来的补丁会盖到旧画面上，**一直错到下一次 IDR**。
///    这条优先于用户配置（预算关掉本来也是阻塞，语义一致）。
/// 2. 预算为 0（用户关闭）→ 阻塞。
/// 3. 距上一次成功回读的间隔未知（会话第一帧 / 刚重建）或超过预算一半 → 有界；
///    否则阻塞（健康路径零额外开销）。
pub(super) fn readback_policy(
    budget_us: u64,
    has_idr: bool,
    idle_us: Option<u64>,
) -> ReadbackPolicy {
    if has_idr || budget_us == 0 {
        return ReadbackPolicy::Blocking;
    }
    match idle_us {
        Some(idle) if idle <= budget_us / 2 => ReadbackPolicy::Blocking,
        _ => ReadbackPolicy::Bounded,
    }
}

/// DXVA staging 回读内部的三段耗时（微秒）。
///
/// 真机上出现过「`readback_us` ≈ 445 ms、而 `submit`/`output`/`color` 都正常」的
/// 停顿，光看 [`crate::decode::DecodePhaseTimings::readback_us`] 无法判断卡在取纹理、
/// GPU 拷贝还是 `Map` 的同步等待上。三段分别计时，语义与
/// [`crate::decode::DecodePhaseTimings`] 一样是**同一次 `decode()` 内累加**。
///
/// `get_resource_us + copy_us + map_us` 近似等于 `readback_us`（差额是 `GetDesc`、
/// `GetSubresourceIndex` 与 `Unmap` 等零碎开销）。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReadbackBreakdown {
    /// `IMFDXGIBuffer::GetResource`：从 MFT 输出缓冲取出底层 `ID3D11Texture2D`。
    pub get_resource_us: u64,
    /// `ID3D11DeviceContext::CopySubresourceRegion`：DXGI 纹理 → staging 纹理。
    ///
    /// 这个调用本身是异步提交，但驱动在目标是不可写共享的 STAGING 纹理时可能
    /// 在这里就同步一次（见探针 `decode_stall_probe`）。
    pub copy_us: u64,
    /// `ID3D11DeviceContext::Map`：等待 GPU 完成并取到 CPU 指针（**同步等待点**）。
    pub map_us: u64,
}

impl ReadbackBreakdown {
    /// 三段之和（微秒）。
    #[must_use]
    pub fn total_us(self) -> u64 {
        self.get_resource_us
            .saturating_add(self.copy_us)
            .saturating_add(self.map_us)
    }

    /// 耗时最大的那一段（全为 0 时返回 [`ReadbackPhase::None`]）。
    ///
    /// 相等时按 `Map` > `Copy` > `GetResource` 的顺序取——`Map` 是唯一的同步点，
    /// 判读停顿原因时宁可指向它。
    #[must_use]
    pub fn dominant(self) -> ReadbackPhase {
        let segments = [
            (ReadbackPhase::Map, self.map_us),
            (ReadbackPhase::Copy, self.copy_us),
            (ReadbackPhase::GetResource, self.get_resource_us),
        ];
        let mut best = (ReadbackPhase::None, 0u64);
        for (phase, value) in segments {
            if value > best.1 {
                best = (phase, value);
            }
        }
        best.0
    }

    /// 把另一份读数累加进来（一次 `decode()` 可能产出多帧）。
    pub(super) fn accumulate(&mut self, other: Self) {
        self.get_resource_us = self.get_resource_us.saturating_add(other.get_resource_us);
        self.copy_us = self.copy_us.saturating_add(other.copy_us);
        self.map_us = self.map_us.saturating_add(other.map_us);
    }
}

/// [`ReadbackBreakdown`] 里耗时最大的子段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadbackPhase {
    /// 三段全为 0（软件路径，或本次没有回读）。
    None,
    /// `IMFDXGIBuffer::GetResource`。
    GetResource,
    /// `CopySubresourceRegion`。
    Copy,
    /// `Map`（GPU 同步等待点）。
    Map,
}

impl ReadbackPhase {
    /// 稳定的短名称（探针/日志用）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "—",
            Self::GetResource => "GetResource",
            Self::Copy => "Copy",
            Self::Map => "Map",
        }
    }
}

/// DXVA 解码所需的 D3D11 设备、设备管理器与可复用的 staging 纹理。
pub(super) struct HardwareContext {
    /// D3D11 设备 / 立即上下文 / 适配器信息。
    device: D3d11,
    /// MFT 通过 `MFT_MESSAGE_SET_D3D_MANAGER` 引用的设备管理器。
    ///
    /// 只被持有、从不直接读取：MFT 内部保存了它的指针，必须活到 MFT 销毁，
    /// 提前释放会让后续解码访问悬空设备管理器。
    _manager: IMFDXGIDeviceManager,
    /// 从 DXGI 纹理回读用的 staging 纹理（按编码尺寸复用）。
    staging: Option<ID3D11Texture2D>,
    /// `staging` 对应的编码尺寸。
    staging_size: Option<(u32, u32)>,
    /// 回读诊断选项（默认 = 生产行为）。
    options: ReadbackOptions,
    /// 上一次**成功回读**（`Map` + `Unmap`）结束的时刻。
    ///
    /// 只用于判断「这一次 `Map` 值不值得先走有界等待」：真机停顿只在设备空闲
    /// 一段时间后出现，而连续逐帧解码（间隔几十毫秒）从来不停顿。
    last_readback_end: Option<std::time::Instant>,
}

impl std::fmt::Debug for HardwareContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HardwareContext")
            .field("adapter", &self.device.adapter.name)
            .field("staging_size", &self.staging_size)
            .finish_non_exhaustive()
    }
}

/// 选一块适合 DXVA 的硬件适配器并建设备：Intel 核显优先，否则任意非软件适配器。
pub(super) fn create_device() -> Result<D3d11, WinVideoError> {
    let adapters = d3d11::adapters()?;
    let chosen = adapters
        .iter()
        .find(|adapter| !adapter.software && adapter.vendor_id == d3d11::VENDOR_INTEL)
        .or_else(|| adapters.iter().find(|adapter| !adapter.software))
        .ok_or_else(|| {
            WinVideoError::Decode("没有可用的硬件适配器（DXVA 需要 D3D11）".to_owned())
        })?;
    let device = d3d11::create(Some(chosen.index))?;
    // 多线程保护已由 `d3d11::create` 统一开启（`VIDEO_SUPPORT` 的要求）；这里再调一次
    // 是幂等的，只为把「此前已开启 / 本次开启」写进日志，行为与之前完全一致。
    d3d11::enable_multithread_protection(&device.device);
    tracing::debug!(adapter = %device.adapter.name, "DXVA 解码设备已创建");
    Ok(device)
}

impl HardwareContext {
    /// 创建设备管理器并把它交给 MFT；必须在设置输入/输出类型之前调用。
    pub(super) fn new(device: D3d11, transform: &IMFTransform) -> Result<Self, WinVideoError> {
        let manager = create_device_manager(&device)?;
        // SAFETY: `manager` 是有效的 COM 接口，`transform` 是有效 MFT；
        // `MFT_MESSAGE_SET_D3D_MANAGER` 的 ulParam 约定为设备管理器的裸指针。
        // manager 随后由本结构体持有，生命周期覆盖 MFT 的使用期。
        unsafe { transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize) }
            .map_err(|err| decode_err("MFT_MESSAGE_SET_D3D_MANAGER", &err))?;
        Ok(Self {
            device,
            _manager: manager,
            staging: None,
            staging_size: None,
            options: ReadbackOptions::default(),
            last_readback_end: None,
        })
    }

    /// 设置回读诊断选项（只影响本进程后续的 `decode`，默认值等于原有行为）。
    pub(super) fn set_options(&mut self, options: ReadbackOptions) {
        self.options = options;
    }

    /// 实际绑定的 D3D11 适配器名。
    #[must_use]
    pub(super) fn adapter_name(&self) -> &str {
        &self.device.adapter.name
    }

    /// 把 DXGI 输出纹理拷到 staging、映射后执行 `convert`，最后保证解映射。
    ///
    /// `convert` 收到的是 NV12 连续内存指针与 `RowPitch`（字节）；UV 平面位于
    /// `pitch * coded_height` 之后，由调用方的转换函数按该约定读取。
    ///
    /// 返回 `Ok(None)` 表示**本次回读超出预算被放弃**：此时没有解映射（从未映射），
    /// staging 上那次拷贝仍由驱动排队完成，下一帧的拷贝排在它后面。
    ///
    /// `force_block` 为真时**不允许**走有界等待（含 IDR 的访问单元必须回读到），
    /// 判定见 [`readback_policy`]。
    pub(super) fn with_mapped<T>(
        &mut self,
        coded_width: u32,
        coded_height: u32,
        buffer: &IMFDXGIBuffer,
        breakdown: &mut ReadbackBreakdown,
        force_block: bool,
        convert: impl FnOnce(*const u8, usize) -> Result<T, WinVideoError>,
    ) -> Result<Option<T>, WinVideoError> {
        let get_started = std::time::Instant::now();
        let texture = Self::dxgi_texture(buffer);
        breakdown.get_resource_us = breakdown
            .get_resource_us
            .saturating_add(elapsed_us(get_started));
        let texture = texture?;
        // SAFETY: `buffer` 有效，返回本栈帧内的 subresource 索引。
        let subresource = unsafe { buffer.GetSubresourceIndex() }
            .map_err(|err| decode_err("IMFDXGIBuffer::GetSubresourceIndex", &err))?;

        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `texture` 有效；`desc` 指向本栈帧。
        unsafe { texture.GetDesc(&raw mut desc) };
        if desc.Format != DXGI_FORMAT_NV12 {
            return Err(WinVideoError::Decode(format!(
                "DXGI 解码输出不是 NV12（{:?}）",
                desc.Format
            )));
        }
        if desc.Width != coded_width || desc.Height != coded_height {
            return Err(WinVideoError::Decode(format!(
                "DXGI 解码输出 {}x{} 与协商的编码尺寸 {coded_width}x{coded_height} 不一致",
                desc.Width, desc.Height
            )));
        }

        let staging = self.ensure_staging(coded_width, coded_height)?;
        if self.options.warm_up == WarmUp::OnDemandCopy {
            // 16x16 的预热拷贝：只提交、不等结果，随后 Flush 推给驱动。
            let region = windows::Win32::Graphics::Direct3D11::D3D11_BOX {
                left: 0,
                top: 0,
                front: 0,
                right: 16,
                bottom: 16,
                back: 1,
            };
            // SAFETY: 同一个设备上的两块同格式纹理；box 落在源纹理内。
            unsafe {
                self.device.context.CopySubresourceRegion(
                    &staging,
                    0,
                    0,
                    0,
                    0,
                    &texture,
                    subresource,
                    Some(&raw const region),
                );
                self.device.context.Flush();
            }
        }
        if self.options.flush_before_copy {
            // SAFETY: 立即上下文有效；`Flush` 只是把已提交的命令推给驱动。
            unsafe { self.device.context.Flush() };
        }
        // SAFETY: 源纹理来自本设备管理器绑定的同一 D3D11 设备；staging 是同设备的
        // STAGING 纹理，尺寸/格式一致；`None` 表示复制整个子资源。
        let copy_started = std::time::Instant::now();
        unsafe {
            self.device.context.CopySubresourceRegion(
                &staging,
                0,
                0,
                0,
                0,
                &texture,
                subresource,
                None,
            );
        }
        breakdown.copy_us = breakdown.copy_us.saturating_add(elapsed_us(copy_started));

        let map_started = std::time::Instant::now();
        let budget_us = self.options.readback_budget_us;
        let mapped = match self.options.map_mode {
            // 诊断模式：轮询 + 事件等待，本来就不会跳过。
            MapMode::PollEvent => self.map_poll_event(&staging),
            MapMode::Blocking => {
                let idle_us = self.last_readback_end.map(|ended| {
                    u64::try_from(ended.elapsed().as_micros()).unwrap_or(u64::MAX)
                });
                match readback_policy(budget_us, force_block, idle_us) {
                    ReadbackPolicy::Blocking => self.map_blocking(&staging),
                    ReadbackPolicy::Bounded => self.map_with_budget(&staging, budget_us),
                }
            }
        };
        breakdown.map_us = breakdown.map_us.saturating_add(elapsed_us(map_started));
        let Some(mapped) = mapped? else {
            // 预算内没等到 GPU 完成：放弃这一帧（调用方据此 `decode` 返回 None）。
            return Ok(None);
        };

        let pitch = usize::try_from(mapped.RowPitch)
            .map_err(|_| WinVideoError::Decode("DXGI RowPitch 无法转为 usize".to_owned()))?;
        let converted = convert(mapped.pData.cast::<u8>(), pitch);
        // SAFETY: 与上面的 Map 严格配对；无论转换成败都必须解映射。
        unsafe { self.device.context.Unmap(&staging, 0) };
        self.last_readback_end = Some(std::time::Instant::now());
        if self.options.recreate_staging {
            // 诊断开关：让下一帧走「新建 staging 纹理」的路径。
            self.staging = None;
            self.staging_size = None;
        }
        converted.map(Some)
    }

    /// 生产行为：`D3D11_MAP_READ` 直接阻塞到 GPU 完成。
    ///
    /// 是否改走 [`Self::map_with_budget`] 由 [`readback_policy`] 决定（IDR 永远走这里）。
    fn map_blocking(
        &self,
        staging: &ID3D11Texture2D,
    ) -> Result<Option<D3D11_MAPPED_SUBRESOURCE>, WinVideoError> {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: `staging` 是 STAGING 纹理且带 CPU_ACCESS_READ；`mapped` 指向本栈帧。
        unsafe {
            self.device
                .context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&raw mut mapped))
        }
        .map_err(|err| decode_err("ID3D11DeviceContext::Map", &err))?;
        Ok(Some(mapped))
    }

    /// 有界 `Map`：`DO_NOT_WAIT` + `EVENT` 查询，预算内等到就返回 `Some`。
    ///
    /// 真机上出现过「GPU 空闲后拷贝要 400+ ms 才完成、而后续帧只要 2 ms」的停顿；
    /// 阻塞等它会把解码任务卡满半秒，进而塞满控制器的解码队列并触发丢帧/关键帧风暴。
    /// 预算内等不到就返回 `Ok(None)`，让调用方**跳过这一帧**继续解下一帧。
    fn map_with_budget(
        &self,
        staging: &ID3D11Texture2D,
        budget_us: u64,
    ) -> Result<Option<D3D11_MAPPED_SUBRESOURCE>, WinVideoError> {
        let flag = u32::try_from(D3D11_MAP_FLAG_DO_NOT_WAIT.0).unwrap_or(0);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: `staging` 是 STAGING 纹理且带 CPU_ACCESS_READ；`flag` 保证不阻塞。
        match unsafe {
            self.device
                .context
                .Map(staging, 0, D3D11_MAP_READ, flag, Some(&raw mut mapped))
        } {
            Ok(()) => return Ok(Some(mapped)),
            Err(err) if err.code() == DXGI_ERROR_WAS_STILL_DRAWING => {}
            Err(err) => return Err(decode_err("ID3D11DeviceContext::Map", &err)),
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_micros(budget_us);
        let query = self.create_event_query()?;
        // SAFETY: `query` 是本设备的 EVENT 查询，插到命令流末尾再推给驱动。
        unsafe {
            self.device.context.End(&query);
            self.device.context.Flush();
        }
        loop {
            // SAFETY: 同上；最后一次尝试也用 `DO_NOT_WAIT`，绝不阻塞。
            match unsafe {
                self.device
                    .context
                    .Map(staging, 0, D3D11_MAP_READ, flag, Some(&raw mut mapped))
            } {
                Ok(()) => return Ok(Some(mapped)),
                Err(err) if err.code() == DXGI_ERROR_WAS_STILL_DRAWING => {}
                Err(err) => return Err(decode_err("ID3D11DeviceContext::Map", &err)),
            }
            if std::time::Instant::now() >= deadline {
                return Ok(None);
            }
            // 事件查询：`S_OK` = 之前的 GPU 工作已完成；未完成时短暂让出 CPU。
            // SAFETY: `query` 有效；`None` 表示不取数据，只问「是否完成」。
            let ready = unsafe {
                self.device.context.GetData(
                    &query,
                    None,
                    0,
                    u32::try_from(D3D11_ASYNC_GETDATA_DONOTFLUSH.0).unwrap_or(0),
                )
            };
            if ready.is_ok() {
                std::thread::yield_now();
            } else {
                // 轮询退避：等待本身是真实的 GPU 完成时间，这里只避免纯自旋烧掉
                // 一个核（每次 200 µs，不会延迟唤醒）。
                std::thread::sleep(std::time::Duration::from_micros(200));
            }
        }
    }

    /// 建一个 `D3D11_QUERY_EVENT` 查询（等待 GPU 完成的显式同步点）。
    fn create_event_query(&self) -> Result<ID3D11Query, WinVideoError> {
        let desc = D3D11_QUERY_DESC {
            Query: D3D11_QUERY_EVENT,
            MiscFlags: 0,
        };
        let mut query: Option<ID3D11Query> = None;
        // SAFETY: `desc` 指向本栈帧；`query` 是输出槽。
        unsafe { self.device.device.CreateQuery(&raw const desc, Some(&raw mut query)) }
            .map_err(|err| decode_err("ID3D11Device::CreateQuery(EVENT)", &err))?;
        query.ok_or_else(|| WinVideoError::Decode("CreateQuery(EVENT) 返回空查询".to_owned()))
    }

    /// 诊断行为：`DO_NOT_WAIT` 轮询，未就绪时用 `EVENT` 查询等 GPU 完成。
    ///
    /// 预期**总耗时与阻塞 `Map` 相同**（等的都是同一件 GPU 工作）；若明显不同，
    /// 说明时间花在 `Map` 的驱动实现而不是 GPU 上。
    fn map_poll_event(
        &self,
        staging: &ID3D11Texture2D,
    ) -> Result<Option<D3D11_MAPPED_SUBRESOURCE>, WinVideoError> {
        let query = self.create_event_query()?;
        // SAFETY: `query` 是本设备上的 EVENT 查询；`End` 把它插到当前命令流末尾。
        unsafe {
            self.device.context.End(&query);
            self.device.context.Flush();
        }

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        let flag = u32::try_from(D3D11_MAP_FLAG_DO_NOT_WAIT.0).unwrap_or(0);
        let mut spins = 0u32;
        loop {
            // SAFETY: `staging`/`mapped` 同上；`DO_NOT_WAIT` 保证不会阻塞。
            match unsafe {
                self.device
                    .context
                    .Map(staging, 0, D3D11_MAP_READ, flag, Some(&raw mut mapped))
            } {
                Ok(()) => return Ok(Some(mapped)),
                Err(err) if err.code() == DXGI_ERROR_WAS_STILL_DRAWING => {}
                Err(err) => return Err(decode_err("ID3D11DeviceContext::Map（DO_NOT_WAIT）", &err)),
            }
            // 事件查询：`S_OK` = 之前的 GPU 工作已完成。
            // SAFETY: `query` 有效；`None` 表示不取数据，只问「是否完成」。
            let ready = unsafe {
                self.device.context.GetData(
                    &query,
                    None,
                    0,
                    u32::try_from(D3D11_ASYNC_GETDATA_DONOTFLUSH.0).unwrap_or(0),
                )
            };
            if ready.is_ok() {
                // 事件已置位：再 Map 一次（仍用 DO_NOT_WAIT，驱动偶有滞后）。
                continue;
            }
            spins = spins.saturating_add(1);
            if spins < 200 {
                std::thread::yield_now();
            } else {
                // 轮询退避：等待本身是真实的 GPU 完成时间，这里只避免纯自旋烧掉
                // 一个核（每次 200 µs，不会延迟唤醒）。
                std::thread::sleep(std::time::Duration::from_micros(200));
            }
        }
    }

    /// 从 `IMFDXGIBuffer` 取出底层 `ID3D11Texture2D`。
    fn dxgi_texture(buffer: &IMFDXGIBuffer) -> Result<ID3D11Texture2D, WinVideoError> {
        let mut raw: *mut c_void = ptr::null_mut();
        // SAFETY: IID 与 `ID3D11Texture2D` 匹配；`raw` 指向本栈帧，成功后由
        // `Type::from_abi` 接管引用计数。
        unsafe { buffer.GetResource(&ID3D11Texture2D::IID, &raw mut raw) }
            .map_err(|err| decode_err("IMFDXGIBuffer::GetResource", &err))?;
        if raw.is_null() {
            return Err(WinVideoError::Decode(
                "IMFDXGIBuffer::GetResource 返回空纹理".to_owned(),
            ));
        }
        // SAFETY: 成功路径的 `raw` 非空且引用计数为 1。
        unsafe { windows::core::Type::from_abi(raw) }
            .map_err(|err| decode_err("IMFDXGIBuffer 资源接口转换", &err))
    }

    /// 取得（必要时创建）与编码尺寸匹配的 staging 纹理。
    fn ensure_staging(
        &mut self,
        width: u32,
        height: u32,
    ) -> Result<ID3D11Texture2D, WinVideoError> {
        if self.staging_size != Some((width, height)) || self.staging.is_none() {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: u32::try_from(D3D11_CPU_ACCESS_READ.0).unwrap_or_default(),
                MiscFlags: 0,
            };
            let mut texture: Option<ID3D11Texture2D> = None;
            // SAFETY: `desc` 指向本栈帧；`texture` 是输出槽，成功后由返回值接管。
            unsafe {
                self.device
                    .device
                    .CreateTexture2D(&raw const desc, None, Some(&raw mut texture))
            }
            .map_err(|err| decode_err("ID3D11Device::CreateTexture2D（staging）", &err))?;
            let texture = texture
                .ok_or_else(|| WinVideoError::Decode("创建 staging 纹理返回空对象".to_owned()))?;
            self.staging = Some(texture);
            self.staging_size = Some((width, height));
        }
        self.staging
            .clone()
            .ok_or_else(|| WinVideoError::Decode("staging 纹理缺失".to_owned()))
    }
}

/// `MFCreateDXGIDeviceManager` + `ResetDevice`（运行期符号，见 [`super::MfApi`]）。
fn create_device_manager(device: &D3d11) -> Result<IMFDXGIDeviceManager, WinVideoError> {
    let api = mf_api()?;
    let create = api.create_dxgi_device_manager.ok_or_else(|| {
        WinVideoError::Decode(
            "mfplat.dll 未导出 MFCreateDXGIDeviceManager（DXVA 不可用）".to_owned(),
        )
    })?;

    let mut reset_token = 0u32;
    let mut raw: *mut c_void = ptr::null_mut();
    // SAFETY: 两个输出指针都指向本栈帧；成功后返回引用计数为 1 的接口指针。
    let result = unsafe { create(&raw mut reset_token, &raw mut raw) };
    result
        .ok()
        .map_err(|err| decode_err("MFCreateDXGIDeviceManager", &err))?;
    if raw.is_null() {
        return Err(WinVideoError::Decode(
            "MFCreateDXGIDeviceManager 返回空管理器".to_owned(),
        ));
    }
    // SAFETY: `raw` 非空且引用计数为 1，所有权交给接口。
    let manager: IMFDXGIDeviceManager = unsafe { windows::core::Type::from_abi(raw) }
        .map_err(|err| decode_err("MFCreateDXGIDeviceManager（接口转换）", &err))?;
    // SAFETY: `manager` 与 `device.device` 都有效；reset token 与本管理器匹配。
    unsafe { manager.ResetDevice(&device.device, reset_token) }
        .map_err(|err| decode_err("IMFDXGIDeviceManager::ResetDevice", &err))?;
    Ok(manager)
}
