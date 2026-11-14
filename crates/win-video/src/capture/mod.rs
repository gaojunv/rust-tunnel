//! Windows 屏幕采集：WGC 优先，DXGI Desktop Duplication 回退。
//!
//! 对外契约（供 `client-gui` 适配）：
//!
//! - [`enumerate_monitors`]：枚举显示器，返回稳定序号、尺寸、主屏标志与 HMONITOR。
//! - [`CaptureSource`]：一次打开、反复采帧；[`CaptureSource::capture`] 返回
//!   **自顶向下 BGRA8**（WGC/DXGI 的原生格式，避免每帧一次通道交换）。
//! - [`CaptureSource::capture_texture`]：同一画面直接落在采集源的**持久 GPU
//!   纹理**上（不落 CPU），供 D3D11 VideoProcessor + 硬件编码器直通；
//!   [`CaptureSource::device`] 暴露采集设备，编码器必须与它共用同一设备。
//! - 采集帧**不含鼠标光标**：WGC 的光标捕获已显式关闭，DXGI Desktop Duplication
//!   与 GDI `BitBlt` 本来就不画光标。控制端窗口用本机系统光标作为唯一指针，
//!   避免帧内远端光标与本机光标并存造成「双光标」。
//! - [`open_best`]：按 WGC → DXGI 的顺序尝试，返回实际后端；两个后端都失败时
//!   错误信息里同时带上两侧的原因。
//!
//! 安全桌面（Winlogon）不属于本模块：那条路径只能用 GDI `BitBlt`，由
//! `client-gui`/`win-platform` 的实例模式处理。
//!
//! # 无新帧策略
//!
//! DXGI 的 `AcquireNextFrame` 在桌面静止时返回 `DXGI_ERROR_WAIT_TIMEOUT`；WGC 则**按
//! 合成器节奏持续交付帧**——实测基本静止的桌面在 30 fps 轮询下 120 次里 119 次都能在
//! 0.2 ms 内拿到新帧，所以「有新帧」**不等于**「内容有变化」。两者都**不把「没有新帧」
//! 当错误**：
//!
//! - 首帧最多等 [`FIRST_FRAME_TIMEOUT`]（2 秒），拿不到才报错；
//! - 之后每次最多等 [`NEXT_FRAME_POLL_TIMEOUT`]（10 毫秒），超时就返回**上一帧**
//!   （桌面静止时上一帧就是当前画面），因此 `capture()` 在成功打开后始终能拿到
//!   与当前屏幕一致的 BGRA；
//! - 代价是静止画面会被上层重复送进编码器，是否跳过重复帧由调用方按自身节奏
//!   决定。返回的帧不携带序号，调用方如需去重可自行比较（或改用带事件回调的
//!   采集循环，本同步契约刻意不做）。
//! - GPU 路径的 `bool` 是**两层**判断：先看「本次是否真的拿到了新帧」（能过滤
//!   WGC/DXGI 的超时拍），拿到新帧后再过一遍 [`change`] 的**像素级变化检测**
//!   （VideoProcessor 缩到 128×72 + staging 回读比较，4K 实测 ~0.7–1.0 ms/帧）。
//!   这是 GPU 路径「静止桌面不编码」的来源：只靠合成器信号做不到，因为 WGC 在
//!   静止桌面上仍持续产帧。**不要**用 CPU 整帧读回比较代替它——那会把零拷贝的
//!   收益全部吃掉。
//!
//! `capture()` 是同步阻塞调用（内部用短睡眠轮询），不要直接放在 Tokio 运行时的
//! 核心线程上；`client-gui` 的应用层采集循环应把它放进 `spawn_blocking`。

mod change;
mod dxgi;
mod wgc;

use crate::d3d11;
use crate::WinVideoError;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use windows::core::BOOL;
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
    DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW,
};

/// 首帧等待上限。WGC 的 frame pool 与 DXGI 的 duplication 都需要一点时间才
/// 产出第一帧；超过这个时间仍没有帧说明采集链路实际不可用。
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(2);

/// 已成功采到帧之后，每次 `capture()` 为等待新帧额外阻塞的上限。桌面静止时
/// 这段等待会空转到超时，然后复用上一帧。
///
/// **不能取得太大**：引擎按目标帧率调用 `capture()`（20 fps = 50 ms 一拍），
/// 若这里也等 50 ms，静止桌面会把每一拍都占满、动态内容的首帧延迟也被放大。
/// 10 ms 足够覆盖 WGC 的事件投递抖动；静止画面本就会被上层的 memcmp 去重。
const NEXT_FRAME_POLL_TIMEOUT: Duration = Duration::from_millis(10);

/// 轮询间隔。WGC 没有同步等待原语（只有事件回调），所以用短睡眠轮询。
const POLL_INTERVAL: Duration = Duration::from_millis(2);

/// 一块显示器的静态信息。
///
/// 序号由 [`enumerate_monitors`] 的枚举顺序决定，同一次进程生命周期内稳定，
/// 但**不保证**跨重启或插拔显示器后不变；需要持久化时应改用 [`MonitorInfo::device_name`]
/// 或左上角坐标。
///
/// # 关于 DPI
///
/// 坐标与宽高来自 `GetMonitorInfoW`，和所有 Win32 窗口 API 一样处在**调用进程的
/// DPI 感知上下文**里：进程未声明 DPI 感知时，缩放 150% 的 3840×2160 显示器会
/// 报成 2560×1440（本机探针实测）。[`CapturedFrame`] 的宽高则始终是**物理像素**，
/// 因此二者可能不相等——需要像素级坐标时应以帧尺寸为准，并在进程入口按需调用
/// `SetProcessDpiAwarenessContext`。
#[derive(Debug, Clone)]
pub struct MonitorInfo {
    /// 稳定序号（从 0 开始，按 [`enumerate_monitors`] 的枚举顺序）。
    pub index: usize,
    /// Win32 显示器句柄（`HMONITOR`），只在当前会话内有效。
    pub hmonitor: HMONITOR,
    /// 虚拟桌面坐标系中的左边界（像素，可为负）。
    pub left: i32,
    /// 虚拟桌面坐标系中的上边界（像素，可为负）。
    pub top: i32,
    /// 宽度（像素）。
    pub width: u32,
    /// 高度（像素）。
    pub height: u32,
    /// 是否主显示器。
    pub is_primary: bool,
    /// 设备名，例如 `\\.\DISPLAY1`。
    pub device_name: String,
}

/// 实际使用的采集后端。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureBackend {
    /// `Windows.Graphics.Capture`。
    Wgc,
    /// DXGI Desktop Duplication。
    Dxgi,
}

impl fmt::Display for CaptureBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Wgc => f.write_str("WGC"),
            Self::Dxgi => f.write_str("DXGI"),
        }
    }
}

/// 一帧采集结果。
///
/// `bgra` 是**自顶向下**的 BGRA8（每像素 4 字节，行间无填充），长度恒为
/// `width * height * 4`。
#[derive(Debug, Clone)]
pub struct CapturedFrame {
    /// 帧宽（像素）。
    pub width: u32,
    /// 帧高（像素）。
    pub height: u32,
    /// 自顶向下 BGRA8，长度 = `width * height * 4`。
    pub bgra: Vec<u8>,
    /// 产出这一帧的后端。
    pub backend: CaptureBackend,
}

/// 池里同时存在的 GPU 纹理数量。
///
/// 引擎侧最多同时持有 2 帧在途（编码队列 1 + 当前帧 1），留 4 张足够轮转；
/// 池耗尽时 [`TexturePool::acquire`] 会临时新建一张（只记 debug，不失败）。
const TEXTURE_POOL_SIZE: usize = 4;

/// 一帧 **GPU 纹理**形式的采集结果（不落 CPU）。
///
/// 纹理是采集源内部纹理池里的持久纹理：内容在 `Drop` 之前有效，之后槽位才会
/// 被下一次采集复用。克隆共享同一份槽位租约（最后一个克隆销毁时才归还），
/// 因此「静止画面复用上一帧」不会让同一槽位被重复归还。
///
/// 纹理格式只保证是 BGRA8（`B8G8R8A8_UNORM` / `_SRGB`），尺寸由 `width`/`height`
/// 给出。后端不支持的格式（例如 HDR 的 `R16G16B16A16_FLOAT`）在
/// [`CaptureSource::capture_texture`] 里直接报错，由调用方回退 CPU/GDI。
pub struct CapturedTexture {
    /// 纹理所在的 D3D11 设备（与采集源是同一个 `Arc`）。
    pub device: Arc<d3d11::D3d11>,
    /// 持久 BGRA 纹理。
    pub texture: ID3D11Texture2D,
    /// 纹理子资源序号（2D 纹理恒为 0）。
    pub subresource: u32,
    /// 帧宽（像素）。
    pub width: u32,
    /// 帧高（像素）。
    pub height: u32,
    /// 池槽位租约；外部构造（探针）的纹理没有租约。
    lease: Option<SlotLease>,
}

impl CapturedTexture {
    /// 用外部纹理构造一个不参与采集源纹理池的 [`CapturedTexture`]。
    ///
    /// 探针/测试用它把自建纹理喂给编码器；纹理必须与调用方后续使用的编码器
    /// 处在**同一个** [`d3d11::D3d11`] 设备上。
    #[must_use]
    pub fn from_texture(
        device: Arc<d3d11::D3d11>,
        texture: ID3D11Texture2D,
        subresource: u32,
        width: u32,
        height: u32,
    ) -> Self {
        Self {
            device,
            texture,
            subresource,
            width,
            height,
            lease: None,
        }
    }

    /// 把这张 GPU 纹理读回成自顶向下、紧凑排列的 BGRA8（同步阻塞）。
    ///
    /// 这是一次「GPU → CPU」拷贝，只给**不能直接吃纹理**的编码器用（例如 NVENC
    /// 适配器）；QSV/VideoProcessor 的零拷贝路径不应调用它。每次调用会新建一张
    /// staging 纹理——需要逐帧回读的热路径请改用 [`BgraReadback`] 复用 staging。
    ///
    /// # Errors
    /// 纹理格式不是 BGRA8、`CopyResource`/`Map` 失败时返回 [`WinVideoError::Capture`]。
    pub fn read_bgra(&self) -> Result<(u32, u32, Vec<u8>), WinVideoError> {
        BgraReader::default().read(&self.device.device, &self.device.context, &self.texture)
    }
}

/// 可复用的 GPU→CPU BGRA 回读器：staging 纹理按 `(宽, 高, 格式)` 缓存。
///
/// 用于「逐帧都要回读」的降级路径（NVENC 暂时不能直接吃采集纹理）。QSV 的
/// VideoProcessor 零拷贝路径不使用它。
#[derive(Default)]
pub struct BgraReadback {
    inner: BgraReader,
    /// staging 纹理所属的设备；设备变化时整只回读器重置。
    device: Option<Arc<d3d11::D3d11>>,
}

impl BgraReadback {
    /// 构造一个尚未分配 staging 纹理的回读器。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 读取 `captured` 的全部像素（自顶向下、紧凑 BGRA8）。
    ///
    /// # Errors
    /// 纹理格式不是 BGRA8、`CopyResource`/`Map` 失败时返回 [`WinVideoError::Capture`]。
    pub fn read(
        &mut self,
        captured: &CapturedTexture,
    ) -> Result<(u32, u32, Vec<u8>), WinVideoError> {
        // staging 纹理不能跨设备使用（`CopyResource` 要求同设备）：采集源重建换了
        // 设备时把旧的丢掉。尺寸/格式变化由 `BgraReader` 自己处理。
        let same_device = self
            .device
            .as_ref()
            .is_some_and(|device| Arc::ptr_eq(device, &captured.device));
        if !same_device {
            self.inner = BgraReader::default();
            self.device = Some(Arc::clone(&captured.device));
        }
        self.inner
            .read(&captured.device.device, &captured.device.context, &captured.texture)
    }
}

// 编译期断言：回读器要能随编码器适配器移进 `spawn_blocking`。
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<BgraReadback>();
};

impl Clone for CapturedTexture {
    /// 克隆共享池租约（引用计数归零才归还槽位）。
    fn clone(&self) -> Self {
        Self {
            device: self.device.clone(),
            texture: self.texture.clone(),
            subresource: self.subresource,
            width: self.width,
            height: self.height,
            lease: self.lease.clone(),
        }
    }
}

impl fmt::Debug for CapturedTexture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CapturedTexture")
            .field("adapter", &self.device.adapter.name)
            .field("size", &(self.width, self.height))
            .field("subresource", &self.subresource)
            .finish_non_exhaustive()
    }
}

/// 池槽位的租约；克隆共享同一租约，最后一个销毁时把序号还给空闲表。
#[derive(Clone)]
struct SlotLease(
    #[expect(
        dead_code,
        reason = "字段只用于持有租约；归还槽位发生在 Arc 内的 SlotLeaseInner::drop"
    )]
    Arc<SlotLeaseInner>,
);

struct SlotLeaseInner {
    free: Arc<Mutex<Vec<usize>>>,
    index: usize,
}

impl Drop for SlotLeaseInner {
    fn drop(&mut self) {
        lock_free(&self.free).push(self.index);
    }
}

/// 取空闲表锁；互斥锁中毒时恢复内部数据（池只是序号表，没有一致性风险）。
fn lock_free(free: &Mutex<Vec<usize>>) -> MutexGuard<'_, Vec<usize>> {
    free.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// 采集源内部的持久 BGRA 纹理池。
///
/// 尺寸或格式变化时整池重建（旧租约仍持有旧池的 `Arc`，归还到旧表里，不会污染
/// 新池）。**不每帧新建纹理**——`CopyResource` 落到已有槽位。
struct TexturePool {
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    textures: Vec<ID3D11Texture2D>,
    free: Arc<Mutex<Vec<usize>>>,
}

impl TexturePool {
    fn new(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
    ) -> Result<Self, WinVideoError> {
        let mut textures = Vec::with_capacity(TEXTURE_POOL_SIZE);
        for _ in 0..TEXTURE_POOL_SIZE {
            textures.push(create_pool_texture(device, width, height, format)?);
        }
        // 从尾部弹出，首个被取到的槽位是 0（与插入顺序一致，便于真机排障）。
        let free = Arc::new(Mutex::new((0..TEXTURE_POOL_SIZE).rev().collect()));
        Ok(Self {
            width,
            height,
            format,
            textures,
            free,
        })
    }

    fn matches(&self, width: u32, height: u32, format: DXGI_FORMAT) -> bool {
        self.width == width && self.height == height && self.format == format
    }

    /// 借一个槽位；池耗尽时临时新建一张（不归还，随 `CapturedTexture` 释放）。
    fn acquire(
        &self,
        device: &ID3D11Device,
    ) -> Result<(Option<SlotLease>, ID3D11Texture2D), WinVideoError> {
        let index = lock_free(&self.free).pop();
        let Some(index) = index else {
            tracing::debug!("GPU 纹理池已耗尽，临时新建一张纹理");
            let texture = create_pool_texture(device, self.width, self.height, self.format)?;
            return Ok((None, texture));
        };
        let texture = self
            .textures
            .get(index)
            .cloned()
            .ok_or_else(|| WinVideoError::Config(format!("GPU 纹理池槽位 {index} 不存在")))?;
        let lease = SlotLease(Arc::new(SlotLeaseInner {
            free: self.free.clone(),
            index,
        }));
        Ok((Some(lease), texture))
    }
}

/// 创建一张池纹理：默认用途、可作渲染目标与着色器资源，供 VideoProcessor 输入。
fn create_pool_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
) -> Result<ID3D11Texture2D, WinVideoError> {
    let bind = D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0;
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: u32::try_from(bind).unwrap_or(0),
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture: Option<ID3D11Texture2D> = None;
    // SAFETY: `desc` 合法；输出指针指向栈上 Option。
    unsafe { device.CreateTexture2D(&raw const desc, None, Some(&raw mut texture)) }
        .map_err(|err| WinVideoError::Capture(format!("创建纹理池纹理失败: {err}")))?;
    texture.ok_or_else(|| WinVideoError::Capture("CreateTexture2D 返回空纹理".to_owned()))
}

/// 校验采集纹理格式并读出尺寸。
fn texture_dimensions(
    texture: &ID3D11Texture2D,
) -> Result<((u32, u32), DXGI_FORMAT), WinVideoError> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: `texture` 是有效纹理；`desc` 指向本栈帧。
    unsafe { texture.GetDesc(&raw mut desc) };
    if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM && desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM_SRGB {
        let hint = if desc.Format == DXGI_FORMAT_R16G16B16A16_FLOAT {
            "；这是 HDR 桌面的 DXGI 输出，需要 HDR→SDR 色调映射"
        } else {
            ""
        };
        return Err(WinVideoError::Capture(format!(
            "GPU 采集纹理格式不受支持 {:?}（只接受 B8G8R8A8_UNORM / SRGB）{hint}",
            desc.Format
        )));
    }
    if desc.Width == 0 || desc.Height == 0 {
        return Err(WinVideoError::Capture(format!(
            "GPU 采集纹理尺寸非法：{}x{}",
            desc.Width, desc.Height
        )));
    }
    Ok(((desc.Width, desc.Height), desc.Format))
}

/// 一次打开、反复采帧的采集源。
///
/// 实现必须是 `Send`：采集通常被放进 `spawn_blocking`（`capture()` 会阻塞）。
pub trait CaptureSource: Send {
    /// 实际使用的后端。
    fn backend(&self) -> CaptureBackend;

    /// 采集当前屏幕内容。
    ///
    /// 返回的是**当前画面**：桌面静止、后端没有新帧时复用上一次成功采集的帧，
    /// 而不是报错（详见模块文档的「无新帧策略」）。
    ///
    /// # Errors
    /// 首帧超时、设备/会话丢失且重建失败、或纹理格式不受支持时返回
    /// [`WinVideoError::Capture`] / [`WinVideoError::Windows`]。
    fn capture(&mut self) -> Result<CapturedFrame, WinVideoError>;

    /// 采集当前屏幕内容到 **GPU 纹理**（不落 CPU）。
    ///
    /// 返回的纹理由采集源内部池持有，克隆/释放语义见 [`CapturedTexture`]。
    /// 无新帧时复用上一次的纹理（同一画面对应的槽位租约被共享）。
    ///
    /// 默认实现返回「该后端不支持」；WGC / DXGI 两个后端都实现了它。
    ///
    /// # Errors
    /// 首帧超时、设备/会话丢失且重建失败、或纹理格式不是 BGRA8 时返回
    /// [`WinVideoError::Capture`]。
    fn capture_texture(&mut self) -> Result<CapturedTexture, WinVideoError> {
        Err(WinVideoError::Capture(
            "该采集后端不支持 GPU 纹理".to_owned(),
        ))
    }

    /// 采集一帧 GPU 纹理，并报告**画面内容自上次采集以来是否有可见变化**。
    ///
    /// 返回 `Ok((texture, false))` 表示这一拍可以跳过编码：要么超时内没有新帧
    /// （`texture` 是上一次的画面），要么拿到了新帧但**像素完全一致**。`true` 表示
    /// 内容确实变了。
    ///
    /// WGC / DXGI 的实现是**两层判断**：① 本次是否真的拿到了新帧（过滤
    /// `TryGetNextFrame` / `AcquireNextFrame` 超时）；② 拿到后过一遍 [`change`] 的
    /// 像素级检测。第二层不可少——实测 WGC 在基本静止的桌面上仍按合成器节奏持续
    /// 产帧（120 次采集里 119 次在 0.2 ms 内拿到新帧），只看「有新帧」根本省不下
    /// 带宽。检测器构造失败或内部出错时保守返回 `true`（宁可多编码一帧）。
    ///
    /// 默认实现保留旧语义（永远认为有变化），因此只实现 [`Self::capture_texture`]
    /// 的后端行为不变。**不要**用 CPU 整帧读回比较代替它——把纹理拷回内存既慢又
    /// 会吃掉零拷贝的收益。
    ///
    /// 默认实现保留旧语义（永远认为有更新），因此只实现 [`Self::capture_texture`]
    /// 的后端行为不变。
    ///
    /// # Errors
    /// 与 [`Self::capture_texture`] 相同。
    fn capture_texture_with_change(
        &mut self,
    ) -> Result<(CapturedTexture, bool), WinVideoError> {
        self.capture_texture().map(|texture| (texture, true))
    }

    /// 采集源使用的 D3D11 设备（编解码需要与采集共用同一设备）。
    ///
    /// 默认返回 `None`（后端不支持 GPU 纹理）。
    fn device(&self) -> Option<Arc<d3d11::D3d11>> {
        None
    }
}

// 编译期断言：`CapturedTexture` 必须能随采集源移进 `spawn_blocking`。
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<CapturedTexture>();
};

/// 枚举所有显示器。
///
/// 顺序即 `EnumDisplayMonitors` 的枚举顺序，`index` 与返回向量下标一致。
///
/// # Errors
/// `EnumDisplayMonitors` 失败或一个显示器都枚举不到时返回 [`WinVideoError`]。
pub fn enumerate_monitors() -> Result<Vec<MonitorInfo>, WinVideoError> {
    let mut ctx = EnumContext {
        monitors: Vec::new(),
    };
    // SAFETY: `ctx` 在调用期间存在于栈上；回调只通过该指针写入，`EnumDisplayMonitors`
    // 返回后不再保留指针。
    let ok = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(enum_monitor_proc),
            LPARAM(std::ptr::from_mut(&mut ctx) as isize),
        )
    };
    if !ok.as_bool() {
        return Err(WinVideoError::Windows(windows::core::Error::from_thread()));
    }
    if ctx.monitors.is_empty() {
        return Err(WinVideoError::Config(
            "EnumDisplayMonitors 没有枚举到任何显示器".to_owned(),
        ));
    }
    Ok(ctx.monitors)
}

struct EnumContext {
    monitors: Vec<MonitorInfo>,
}

/// `EnumDisplayMonitors` 回调。返回 `TRUE` 继续枚举。
///
/// # Safety
/// `lparam` 必须是 [`enumerate_monitors`] 传入的、指向 [`EnumContext`] 的有效指针，
/// 且回调执行期间该指针保持有效。
unsafe extern "system" fn enum_monitor_proc(
    hmonitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    // SAFETY: 见函数文档；`lparam` 由 `enumerate_monitors` 用 `&mut EnumContext` 构造。
    let ctx = unsafe { &mut *(lparam.0 as *mut EnumContext) };
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = u32::try_from(std::mem::size_of::<MONITORINFOEXW>()).unwrap_or(0);
    // SAFETY: `info` 是合法的 MONITORINFOEXW：`cbSize` 已填，函数按 EX 结构写
    // 基础 MONITORINFO + szDevice。
    let ok = unsafe { GetMonitorInfoW(hmonitor, &raw mut info.monitorInfo) };
    if ok.as_bool() {
        let rc = info.monitorInfo.rcMonitor;
        let width = u32::try_from(rc.right - rc.left).unwrap_or(0);
        let height = u32::try_from(rc.bottom - rc.top).unwrap_or(0);
        if width > 0 && height > 0 {
            ctx.monitors.push(MonitorInfo {
                index: ctx.monitors.len(),
                hmonitor,
                left: rc.left,
                top: rc.top,
                width,
                height,
                // `MONITORINFOF_PRIMARY` 的值是 1；不为此引一个仅含常量的 features。
                is_primary: info.monitorInfo.dwFlags & 1 != 0,
                device_name: wide_to_string(&info.szDevice),
            });
        }
    }
    BOOL(1)
}

/// 解码 UTF-16 设备名（去掉结尾的 NUL）。
fn wide_to_string(wide: &[u16]) -> String {
    let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    String::from_utf16_lossy(&wide[..end])
}

/// 按「WGC → DXGI」的顺序打开最佳采集源。
///
/// 两个后端都失败时返回的 [`WinVideoError::Capture`] 会同时带上两侧的具体原因，
/// 便于真机排障。
///
/// # Errors
/// 两个后端都打不开时返回 [`WinVideoError::Capture`]。
pub fn open_best(monitor: &MonitorInfo) -> Result<Box<dyn CaptureSource>, WinVideoError> {
    let mut reasons = Vec::with_capacity(2);
    match wgc::open(monitor) {
        Ok(source) => return Ok(source),
        Err(err) => {
            tracing::debug!(error = %err, "WGC 采集不可用，回退 DXGI");
            reasons.push(format!("WGC: {err}"));
        }
    }
    match dxgi::open(monitor) {
        Ok(source) => Ok(source),
        Err(err) => {
            reasons.push(format!("DXGI: {err}"));
            Err(WinVideoError::Capture(format!(
                "没有可用的采集后端（{}）",
                reasons.join("；")
            )))
        }
    }
}

/// 强制打开 WGC 采集源（探针/排障用）。
///
/// # Errors
/// WGC 不可用或初始化失败时返回 [`WinVideoError`]。
pub fn open_wgc(monitor: &MonitorInfo) -> Result<Box<dyn CaptureSource>, WinVideoError> {
    wgc::open(monitor)
}

/// 强制打开 DXGI Desktop Duplication 采集源（探针/排障用）。
///
/// # Errors
/// 显示器没有对应的 DXGI 输出、设备创建失败或 `DuplicateOutput` 失败时返回
/// [`WinVideoError`]。
pub fn open_dxgi(monitor: &MonitorInfo) -> Result<Box<dyn CaptureSource>, WinVideoError> {
    dxgi::open(monitor)
}

/// 与指定 HMONITOR 对应的 DXGI 输出。
struct OutputMatch {
    /// 该输出所属适配器在 `IDXGIFactory1::EnumAdapters1` 中的序号。
    adapter_index: u32,
    /// 输出本身。
    output: IDXGIOutput,
}

/// 在 DXGI 适配器/输出里找到 HMONITOR 所属的那一个。
fn find_output(hmonitor: HMONITOR) -> Result<OutputMatch, WinVideoError> {
    // SAFETY: 无参数，返回新建的 COM 对象。
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }?;
    for index in 0.. {
        // SAFETY: 枚举到末尾时返回 `DXGI_ERROR_NOT_FOUND`，是正常终止条件。
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(index) }) else {
            break;
        };
        for out_index in 0.. {
            // SAFETY: 同上，`EnumOutputs` 到末尾返回 `DXGI_ERROR_NOT_FOUND`。
            let Ok(output) = (unsafe { adapter.EnumOutputs(out_index) }) else {
                break;
            };
            // SAFETY: `output` 是刚枚举出的有效 COM 对象。
            let desc = unsafe { output.GetDesc() }?;
            if desc.Monitor == hmonitor {
                return Ok(OutputMatch {
                    adapter_index: index,
                    output,
                });
            }
        }
    }
    Err(WinVideoError::Config(format!(
        "HMONITOR {hmonitor:?} 没有匹配到任何 DXGI 输出"
    )))
}

/// 在对应适配器上建 D3D11 设备（WGC 与 DXGI 共用）。
fn create_device_for(hmonitor: HMONITOR) -> Result<d3d11::D3d11, WinVideoError> {
    // WGC 不依赖 DXGI 输出也能工作（跨适配器复制由 DWM 完成），所以匹配不到
    // 输出时退回「第一块非软件适配器」而不是直接失败。
    let adapter_index = find_output(hmonitor).ok().map(|m| m.adapter_index);
    d3d11::create(adapter_index)
}

/// 把 GPU 纹理读回成自顶向下、紧凑排列的 BGRA8。
///
/// staging 纹理按 `(宽, 高, 格式)` 缓存，尺寸/格式不变时不重建。
#[derive(Default)]
struct BgraReader {
    staging: Option<ID3D11Texture2D>,
    key: Option<(u32, u32, DXGI_FORMAT)>,
}

impl BgraReader {
    /// 读取 `source` 的全部像素。
    fn read(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        source: &ID3D11Texture2D,
    ) -> Result<(u32, u32, Vec<u8>), WinVideoError> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `source` 由调用方保证是有效纹理；`desc` 是栈上可写结构。
        unsafe { source.GetDesc(&raw mut desc) };
        if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM
            && desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM_SRGB
        {
            let hint = if desc.Format == DXGI_FORMAT_R16G16B16A16_FLOAT {
                "；这是 HDR 桌面的 DXGI 输出，需要 HDR→SDR 色调映射（WGC 主路径不受影响）"
            } else {
                ""
            };
            return Err(WinVideoError::Capture(format!(
                "不支持的采集纹理格式 {desc:?}（只接受 B8G8R8A8_UNORM / SRGB）{hint}"
            )));
        }

        let key = (desc.Width, desc.Height, desc.Format);
        if self.key != Some(key) || self.staging.is_none() {
            self.staging = Some(create_staging(device, &desc)?);
            self.key = Some(key);
        }
        let staging = self
            .staging
            .as_ref()
            .ok_or_else(|| WinVideoError::Config("staging 纹理缺失".to_owned()))?;

        // SAFETY: 两张纹理格式/尺寸一致（staging 由 `desc` 构造），`staging` 是
        // STAGING 用途、`source` 是本设备上的资源；拷贝在立即上下文上同步完成。
        unsafe { context.CopyResource(staging, source) };
        read_mapped(context, staging, desc.Width, desc.Height)
    }
}

/// 按源纹理描述创建 CPU 可读的 staging 纹理。
fn create_staging(
    device: &ID3D11Device,
    source: &D3D11_TEXTURE2D_DESC,
) -> Result<ID3D11Texture2D, WinVideoError> {
    let cpu_access = u32::try_from(D3D11_CPU_ACCESS_READ.0)
        .map_err(|_| WinVideoError::Config("D3D11_CPU_ACCESS_READ 不是非负值".to_owned()))?;
    let desc = D3D11_TEXTURE2D_DESC {
        Width: source.Width,
        Height: source.Height,
        MipLevels: 1,
        ArraySize: 1,
        Format: source.Format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: cpu_access,
        MiscFlags: 0,
    };
    let mut texture: Option<ID3D11Texture2D> = None;
    // SAFETY: `desc` 合法；输出指针指向栈上 Option。
    unsafe { device.CreateTexture2D(&raw const desc, None, Some(&raw mut texture))? };
    texture.ok_or_else(|| WinVideoError::Config("CreateTexture2D 返回空纹理".to_owned()))
}

/// Map staging 纹理并按行拷成紧凑 BGRA。
fn read_mapped(
    context: &ID3D11DeviceContext,
    staging: &ID3D11Texture2D,
    width: u32,
    height: u32,
) -> Result<(u32, u32, Vec<u8>), WinVideoError> {
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    // SAFETY: `staging` 是 STAGING 纹理；`mapped` 是栈上输出结构。
    unsafe { context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&raw mut mapped))? };
    let result = copy_rows(&mapped, width, height).map(|bgra| (width, height, bgra));
    // SAFETY: 与上面的 `Map` 严格配对。
    unsafe { context.Unmap(staging, 0) };
    result
}

/// 把 Map 出来的、带行填充的图像拷成自顶向下紧凑 BGRA。
fn copy_rows(
    mapped: &D3D11_MAPPED_SUBRESOURCE,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, WinVideoError> {
    if mapped.pData.is_null() {
        return Err(WinVideoError::Capture("Map 返回空指针".to_owned()));
    }
    let row_bytes = usize::try_from(width)
        .ok()
        .and_then(|w| w.checked_mul(4))
        .ok_or_else(|| WinVideoError::Config(format!("帧宽 {width} 溢出")))?;
    let rows = usize::try_from(height)
        .map_err(|_| WinVideoError::Config(format!("帧高 {height} 溢出")))?;
    let pitch = usize::try_from(mapped.RowPitch)
        .map_err(|_| WinVideoError::Config("RowPitch 溢出".to_owned()))?;
    if pitch < row_bytes {
        return Err(WinVideoError::Capture(format!(
            "RowPitch {pitch} 小于一行像素字节数 {row_bytes}"
        )));
    }
    let source_len = pitch
        .checked_mul(rows)
        .ok_or_else(|| WinVideoError::Config("Map 缓冲区长度溢出".to_owned()))?;
    let out_len = row_bytes
        .checked_mul(rows)
        .ok_or_else(|| WinVideoError::Config("输出图像长度溢出".to_owned()))?;
    // SAFETY: Map 保证 `pData` 在 Unmap 前至少可读 `RowPitch * Height` 字节。
    let source = unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>(), source_len) };
    let mut out = vec![0_u8; out_len];
    for y in 0..rows {
        let src_start = y * pitch;
        let dst_start = y * row_bytes;
        out[dst_start..dst_start + row_bytes]
            .copy_from_slice(&source[src_start..src_start + row_bytes]);
    }
    Ok(out)
}
