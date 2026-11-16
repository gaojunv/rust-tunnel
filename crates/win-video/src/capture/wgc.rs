//! WGC（`Windows.Graphics.Capture`）采集后端。
//!
//! 用 `IGraphicsCaptureItemInterop::CreateForMonitor` 直接按显示器创建捕获项，
//! 不弹系统选择器。帧池用 `CreateFreeThreaded`：`capture()` 由调用方在任意线程
//! 同步轮询，不做事件回调。
//!
//! # 前置条件
//!
//! - 进程必须已初始化 WinRT（`RoInitialize(RO_INIT_MULTITHREADED)`）。本模块在
//!   `open` 时初始化；若当前线程已经是别的套间模式（`RPC_E_CHANGED_MODE`），
//!   只记日志、继续尝试——`CreateFreeThreaded` 不要求调用线程是 MTA。
//! - 显示器捕获在 Windows 10 1903+ 可用；`GraphicsCaptureSession::IsSupported()`
//!   为 false 时直接返回错误，让 [`super::open_best`] 回退 DXGI。
//!
//! # 排空到最新帧
//!
//! 帧池只有 2 张缓冲（见 `open` 里的 `CreateFreeThreaded(..., 2, ...)`）。合成器
//! 按显示器刷新率产帧，而采集按引擎节拍来取（20/30 fps），所以两拍之间帧池里往往
//! 已经排了不止一帧。`TryGetNextFrame` 返回的是**队首那张最旧的帧**——只取一张就
//! 走，等于把队列里更新的帧留到下一拍，每多排一帧就多一个合成器周期的**纯排队
//! 延迟**（60 Hz 下约 16.7 ms），对画质没有任何贡献。
//!
//! 因此每次采集调用都先把帧池**排空**：反复 `TryGetNextFrame`，只保留最新的一帧
//! 去 `Surface()`/`CopyResource`，前面更旧的帧立即 `Close` 归还缓冲（缓冲早归还，
//! 合成器才不用为等槽位而丢帧）。排空是**有界**的（最多 `MAX_DRAIN_FRAMES` 张，
//! 正常最多 2 张），循环里不睡眠、不延长首帧/新帧的等待上限，所以「没有新帧」
//! 的语义与轮询节奏完全不变。
//!
//! 同步轮询没有更省事的「取最新」原语：帧池按产帧顺序交付，**排到空为止**取到的
//! 那张就是最新的一帧，无需逐帧比较时间戳。

use super::{
    create_device_for, texture_dimensions, BgraReader, CaptureBackend, CaptureCounters,
    CaptureSource, CapturedFrame, CapturedTexture, ChangeReport, DirtyGrid, MonitorInfo,
    TexturePool, FIRST_FRAME_TIMEOUT, NEXT_FRAME_POLL_TIMEOUT, POLL_INTERVAL,
};
use crate::d3d11;
use crate::WinVideoError;
use std::sync::Arc;
use std::time::Instant;
use windows::core::Interface as _;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

/// 一次采集调用里最多从帧池排空的帧数。
///
/// 帧池只有 2 张缓冲（`CreateFreeThreaded(..., 2, ...)`），正常情况排 2 张就取空了。
/// 8 是留给异常状态（帧池内部补发、调用方长时间没采集）的余量，同时保证排空循环
/// **绝不会**在帧池持续产帧时变成不设上限的忙循环。
const MAX_DRAIN_FRAMES: usize = 8;

// 编译期断言：排空上限必须覆盖帧池的 2 张缓冲，否则一次「池被填满」的采集仍会
// 留下旧帧，排空就失去了意义。
const _: () = assert!(MAX_DRAIN_FRAMES >= 2);

/// WGC 采集源。
struct WgcSource {
    /// 捕获项；捕获会话/帧池内部也会持有引用，这里保留一份便于排障与生命周期
    /// 明确化。
    _item: GraphicsCaptureItem,
    /// 捕获会话（`StartCapture` 之后持续生效）。
    _session: GraphicsCaptureSession,
    /// 自由线程帧池。
    pool: Direct3D11CaptureFramePool,
    /// D3D11 设备（帧纹理由它创建，读回/GPU 拷贝也要用它）。
    d3d: Arc<d3d11::D3d11>,
    /// BGRA 读回器（staging 纹理缓存）。
    reader: BgraReader,
    /// 最近一次成功采集的帧，用于桌面静止时复用。
    last: Option<CapturedFrame>,
    /// 持久 BGRA 纹理池（GPU 路径）；尺寸/格式变化时重建。
    textures: Option<TexturePool>,
    /// 像素级变化检测（GPU 路径）；WGC 在静止桌面仍按合成器节奏出帧，只能靠它判断
    /// 「内容确实没变」。
    change: super::change::TextureChangeTracker,
    /// 最近一次 GPU 采集结果（无新帧时返回它的克隆）。
    last_texture: Option<CapturedTexture>,
    /// 「超时 / 无新帧 / 复用上一帧」的累计次数（[`CaptureCounters::no_new_frame`]）。
    no_new_frame: u64,
}

impl CaptureSource for WgcSource {
    fn backend(&self) -> CaptureBackend {
        CaptureBackend::Wgc
    }

    fn capture(&mut self) -> Result<CapturedFrame, WinVideoError> {
        let deadline = Instant::now() + self.capture_timeout();
        loop {
            // 排空到最新帧：帧池里排着的更旧帧已经过时，读回它们只会把
            // 端到端延迟拖后 1–2 个合成器周期（模块文档「排空到最新帧」）。
            if let Some(frame) = self.drain_latest_frame()? {
                let captured = self.read_frame(&frame)?;
                self.last = Some(captured.clone());
                return Ok(captured);
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        match &self.last {
            Some(frame) => {
                // 超时内没有新帧，复用上一帧：记一次「无新帧」（桌面静止的正常表现）。
                self.no_new_frame = self.no_new_frame.saturating_add(1);
                Ok(frame.clone())
            }
            None => Err(WinVideoError::Capture(format!(
                "WGC: {:?} 内没有收到首帧",
                self.capture_timeout()
            ))),
        }
    }

    fn capture_texture(&mut self) -> Result<CapturedTexture, WinVideoError> {
        self.capture_texture_with_dirty()
            .map(|(texture, _, _)| texture)
    }

    fn capture_texture_with_change(
        &mut self,
    ) -> Result<(CapturedTexture, ChangeReport), WinVideoError> {
        self.capture_texture_with_dirty()
            .map(|(texture, report, _)| (texture, report))
    }

    fn capture_texture_with_dirty(
        &mut self,
    ) -> Result<(CapturedTexture, ChangeReport, Option<DirtyGrid>), WinVideoError> {
        let timeout = self.capture_timeout();
        let deadline = Instant::now() + timeout;
        loop {
            // 与 CPU 路径同一策略：先排空，只拷最新一帧（更旧的帧已在
            // `drain_latest_frame` 里 Close 归还缓冲）。
            if let Some(frame) = self.drain_latest_frame()? {
                let result = self.copy_surface(&frame);
                // 最新帧处理完也要归还缓冲（失败只影响归还时机，不影响已拷贝数据）。
                close_frame(&frame);
                let captured = result?;
                // **像素级**变化检测：WGC 在基本静止的桌面上仍按合成器节奏产新帧
                // （实测 120 次采集 119 次有新帧），所以「有新帧」不等于「有变化」。
                // 同时拿到脏格点网格，供上层决定是否走补丁模式。
                let (report, grid) = self.change.changed_with_grid(&self.d3d, &captured);
                self.last_texture = Some(captured.clone());
                return Ok((captured, report, grid));
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        // 超时：直接复用上一张纹理，**不做 CopyResource**（画面没变，拷贝纯属浪费）。
        // 返回「无变化」让上层跳过编码/发送；旧语义（恒为「有变化」）由默认实现保留。
        match &self.last_texture {
            // 复用上一帧时**不能**给网格：它是上一拍的内容，补丁模式拿它去覆盖控制器
            // 画面会让两侧错位。返回 `None` 让上层走全帧。
            Some(texture) => {
                self.no_new_frame = self.no_new_frame.saturating_add(1);
                Ok((texture.clone(), ChangeReport::unchanged(), None))
            }
            None => Err(WinVideoError::Capture(format!(
                "WGC: {timeout:?} 内没有收到首帧（GPU 纹理）"
            ))),
        }
    }

    fn device(&self) -> Option<Arc<d3d11::D3d11>> {
        Some(self.d3d.clone())
    }

    fn stats(&self) -> CaptureCounters {
        CaptureCounters {
            no_new_frame: self.no_new_frame,
        }
    }
}

impl WgcSource {
    /// 首帧等 2 秒，之后每次最多等 10 毫秒（两种路径共用同一策略）。
    fn capture_timeout(&self) -> std::time::Duration {
        if self.last.is_some() || self.last_texture.is_some() {
            NEXT_FRAME_POLL_TIMEOUT
        } else {
            FIRST_FRAME_TIMEOUT
        }
    }

    /// 把帧池里**已就绪**的帧全部取走，只返回最新的一帧（没有就绪帧时返回 `None`）。
    ///
    /// 为什么要排空：`TryGetNextFrame` 交付的是队首（最旧）那张帧。合成器产帧快于
    /// 采集节拍时，池里常排着 1–2 张更新的帧，只取一张就处理等于主动多背 1–2 个
    /// 合成器周期的排队延迟（60 Hz 下 ~16.7 ms/帧），而这段延迟对画面质量毫无贡献。
    ///
    /// 排空过程中更旧的帧**立即 `Close`**：`Direct3D11CaptureFrame` 的 `Drop` 只释放
    /// COM 引用、不会调用 `IClosable::Close`，不显式关闭的话缓冲要等垃圾回收才归还，
    /// 帧池可用的槽位会被无谓占用。保留最新帧期间关闭前面的帧是安全的——每张帧对象
    /// 各自持有自己的缓冲。
    ///
    /// 循环有界（`MAX_DRAIN_FRAMES`），不睡眠、不推进 deadline；真正的 HRESULT 失败
    /// 向上报（并顺手关闭已收集的帧），与原先「第一次失败即报错」的语义一致。
    fn drain_latest_frame(&self) -> Result<Option<Direct3D11CaptureFrame>, WinVideoError> {
        drain_newest(
            || match self.pool.TryGetNextFrame() {
                Ok(frame) => Ok(Some(frame)),
                // 帧池没有新帧时 `from_abi` 把空指针折成 `Error::empty()`，
                // 其 HRESULT 为 0（成功码）。真正的 HRESULT 失败才向上报。
                Err(err) if err.code().is_ok() => Ok(None),
                Err(err) => Err(WinVideoError::Capture(format!(
                    "WGC TryGetNextFrame 失败: {err}"
                ))),
            },
            |frame| close_frame(&frame),
        )
    }

    /// 把帧表面拷贝到池纹理（GPU 到 GPU，不落 CPU）。
    fn copy_surface(
        &mut self,
        frame: &Direct3D11CaptureFrame,
    ) -> Result<CapturedTexture, WinVideoError> {
        let surface = frame
            .Surface()
            .map_err(|err| WinVideoError::Capture(format!("WGC Surface() 失败: {err}")))?;
        let access: IDirect3DDxgiInterfaceAccess = surface
            .cast()
            .map_err(|err| WinVideoError::Capture(format!("表面转 DXGI 接口失败: {err}")))?;
        // SAFETY: `access` 是帧表面的 DXGI 访问接口；请求的是 D3D11 纹理。
        let source: ID3D11Texture2D = unsafe { access.GetInterface() }
            .map_err(|err| WinVideoError::Capture(format!("帧表面不是 D3D11 纹理: {err}")))?;
        let ((width, height), format) = texture_dimensions(&source)?;

        if self
            .textures
            .as_ref()
            .is_none_or(|pool| !pool.matches(width, height, format))
        {
            self.textures = Some(TexturePool::new(&self.d3d.device, width, height, format)?);
        }
        let pool = self
            .textures
            .as_ref()
            .ok_or_else(|| WinVideoError::Config("WGC 纹理池缺失".to_owned()))?;
        let (lease, target) = pool.acquire(&self.d3d.device)?;

        // SAFETY: 两张纹理在同一设备上、尺寸与格式一致（池按源纹理描述创建）；
        // 拷贝在立即上下文上同步完成。
        unsafe { self.d3d.context.CopyResource(&target, &source) };

        Ok(CapturedTexture {
            device: self.d3d.clone(),
            texture: target,
            subresource: 0,
            width,
            height,
            lease,
        })
    }

    /// 把一帧 WGC 画面读成紧凑 BGRA，并关闭帧对象归还缓冲。
    fn read_frame(
        &mut self,
        frame: &Direct3D11CaptureFrame,
    ) -> Result<CapturedFrame, WinVideoError> {
        let result = self.read_surface(frame);
        close_frame(frame);
        result
    }

    fn read_surface(
        &mut self,
        frame: &Direct3D11CaptureFrame,
    ) -> Result<CapturedFrame, WinVideoError> {
        let surface = frame
            .Surface()
            .map_err(|err| WinVideoError::Capture(format!("WGC Surface() 失败: {err}")))?;
        let access: IDirect3DDxgiInterfaceAccess = surface
            .cast()
            .map_err(|err| WinVideoError::Capture(format!("表面转 DXGI 接口失败: {err}")))?;
        // SAFETY: `access` 是帧表面的 DXGI 访问接口；请求的是 D3D11 纹理。
        let texture: ID3D11Texture2D = unsafe { access.GetInterface() }
            .map_err(|err| WinVideoError::Capture(format!("帧表面不是 D3D11 纹理: {err}")))?;
        let (width, height, bgra) =
            self.reader
                .read(&self.d3d.device, &self.d3d.context, &texture)?;
        Ok(CapturedFrame {
            width,
            height,
            bgra,
            backend: CaptureBackend::Wgc,
        })
    }
}

/// 关闭 WGC 帧对象、把缓冲立即还给帧池。
///
/// 帧对象的 `Drop` 只释放 COM 引用，**不会**调用 `IClosable::Close`；不显式关闭的话
/// 缓冲要等引用计数归零后由池内部回收，期间新帧可能因没有空槽而被丢弃。失败只记
/// debug——它只影响归还时机，不影响任何已经拷贝出来的数据。
fn close_frame(frame: &Direct3D11CaptureFrame) {
    if let Err(err) = frame.Close() {
        tracing::debug!(error = %err, "WGC 帧对象 Close 失败");
    }
}

/// 「排空到最新」的通用循环：反复调用 `next` 直到取空或出错，只返回**最新**一项；
/// 被替换掉的旧项立即交给 `recycle`。
///
/// 抽成泛型是为了让四条不变量（保留最新、回收旧项、循环有界、出错时也回收）能被
/// 不依赖 Windows 的单测钉住——真实的 `Direct3D11CaptureFrame` 无法在单测里构造。
fn drain_newest<T, E>(
    mut next: impl FnMut() -> Result<Option<T>, E>,
    mut recycle: impl FnMut(T),
) -> Result<Option<T>, E> {
    let mut latest: Option<T> = None;
    for _ in 0..MAX_DRAIN_FRAMES {
        match next() {
            Ok(Some(item)) => {
                if let Some(previous) = latest.replace(item) {
                    recycle(previous);
                }
            }
            Ok(None) => break,
            Err(err) => {
                // 已经收集到的帧也要归还，否则帧池会把槽位算作仍被占用。
                if let Some(previous) = latest.take() {
                    recycle(previous);
                }
                return Err(err);
            }
        }
    }
    Ok(latest)
}

/// 打开指定显示器的 WGC 采集源。
pub(super) fn open(monitor: &MonitorInfo) -> Result<Box<dyn CaptureSource>, WinVideoError> {
    initialize_winrt()?;

    match GraphicsCaptureSession::IsSupported() {
        Ok(true) => {}
        Ok(false) => {
            return Err(WinVideoError::Capture(
                "GraphicsCaptureSession::IsSupported() 返回 false".to_owned(),
            ))
        }
        Err(err) => return Err(WinVideoError::Capture(format!("WGC 支持性查询失败: {err}"))),
    }

    let interop: IGraphicsCaptureItemInterop =
        windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>().map_err(
            |err| WinVideoError::Capture(format!("获取 IGraphicsCaptureItemInterop 失败: {err}")),
        )?;
    // SAFETY: `interop` 有效；`hmonitor` 来自 `EnumDisplayMonitors`，在显示器
    // 存活期间有效。
    let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(monitor.hmonitor) }
        .map_err(|err| WinVideoError::Capture(format!("CreateForMonitor 失败: {err}")))?;

    let d3d = Arc::new(create_device_for(monitor.hmonitor)?);
    let device = winrt_device(&d3d.device)?;

    let size = item
        .Size()
        .map_err(|err| WinVideoError::Capture(format!("GraphicsCaptureItem::Size 失败: {err}")))?;
    if size.Width <= 0 || size.Height <= 0 {
        return Err(WinVideoError::Capture(format!(
            "WGC 捕获项尺寸非法: {}x{}",
            size.Width, size.Height
        )));
    }

    // 2 张缓冲：采集端每拍都会排空到最新帧（见 `drain_latest_frame`），缓冲越少，
    // 池里能累积的排队帧就越少；再多给缓冲只是把延迟换成「不会丢帧」，不划算。
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        size,
    )
    .map_err(|err| WinVideoError::Capture(format!("CreateFreeThreaded 失败: {err}")))?;
    let session = pool
        .CreateCaptureSession(&item)
        .map_err(|err| WinVideoError::Capture(format!("CreateCaptureSession 失败: {err}")))?;

    // 显式关闭帧内光标：控制端窗口只显示本机系统光标，帧内再画一个远端光标就会
    // 出现两个指针（且远端光标还会因采集与网络延迟拖尾）。本模块没有光标形状
    // 通道，把光标留在帧内除了「可见」没有任何好处，故不捕获。
    // 该接口在老系统上可能不可用；失败时维持 WGC 默认的「不捕获」，与目标一致，
    // 因此只记 debug。
    if let Err(err) = session.SetIsCursorCaptureEnabled(false) {
        tracing::debug!(error = %err, "WGC 无法显式关闭光标捕获（维持默认不捕获）");
    }
    // 边框同属可选能力：老系统上接口不可用时失败不影响采集。
    if let Err(err) = session.SetIsBorderRequired(false) {
        tracing::debug!(error = %err, "WGC 无法禁用捕获边框（继续）");
    }
    session
        .StartCapture()
        .map_err(|err| WinVideoError::Capture(format!("StartCapture 失败: {err}")))?;

    tracing::info!(
        monitor = %monitor.device_name,
        width = size.Width,
        height = size.Height,
        "WGC 采集源已打开"
    );
    Ok(Box::new(WgcSource {
        _item: item,
        _session: session,
        pool,
        d3d,
        reader: BgraReader::default(),
        last: None,
        textures: None,
        change: super::change::TextureChangeTracker::new(),
        last_texture: None,
        no_new_frame: 0,
    }))
}

/// 初始化 WinRT。容忍「当前线程已处于其它套间模式」。
fn initialize_winrt() -> Result<(), WinVideoError> {
    // SAFETY: 无参数初始化调用；重复调用是标准 WinRT 用法。
    match unsafe { RoInitialize(RO_INIT_MULTITHREADED) } {
        Ok(()) => Ok(()),
        Err(err) if err.code() == RPC_E_CHANGED_MODE => {
            tracing::warn!(
                error = %err,
                "当前线程已处于非 MTA 套间，WGC 仍按自由线程帧池继续"
            );
            Ok(())
        }
        Err(err) => Err(WinVideoError::Capture(format!("RoInitialize 失败: {err}"))),
    }
}

/// 把 DXGI 设备包成 WinRT 的 `IDirect3DDevice`。
fn winrt_device(device: &ID3D11Device) -> Result<IDirect3DDevice, WinVideoError> {
    let dxgi_device: IDXGIDevice = device
        .cast()
        .map_err(|err| WinVideoError::Capture(format!("D3D11 设备转 IDXGIDevice 失败: {err}")))?;
    // SAFETY: `dxgi_device` 是本进程刚创建的有效设备。
    let inspectable =
        unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device) }.map_err(|err| {
            WinVideoError::Capture(format!("CreateDirect3D11DeviceFromDXGIDevice 失败: {err}"))
        })?;
    inspectable
        .cast()
        .map_err(|err| WinVideoError::Capture(format!("转 IDirect3DDevice 失败: {err}")))
}

#[cfg(test)]
mod tests {
    use super::{drain_newest, MAX_DRAIN_FRAMES};
    use std::collections::VecDeque;

    /// 池里有 3 张就绪帧：只保留最新那张，前两张按取出的顺序立即回收。
    #[test]
    fn drain_keeps_newest_and_recycles_older_items() {
        let mut ready = VecDeque::from([1_u32, 2, 3]);
        let mut recycled = Vec::new();
        let result = drain_newest(
            || Ok::<_, ()>(ready.pop_front()),
            |item| recycled.push(item),
        );
        assert!(result.is_ok(), "测试闭包不会返回错误");
        assert_eq!(result.ok().flatten(), Some(3));
        assert_eq!(recycled, vec![1, 2]);
    }

    /// 帧池一直有帧时也不能无限排空：循环必须停在 `MAX_DRAIN_FRAMES`。
    #[test]
    fn drain_is_bounded_when_frames_never_stop() {
        let mut produced = 0_usize;
        let mut recycled = Vec::new();
        let result = drain_newest(
            || {
                produced += 1;
                Ok::<_, ()>(Some(produced))
            },
            |item| recycled.push(item),
        );
        assert!(result.is_ok(), "测试闭包不会返回错误");
        assert_eq!(result.ok().flatten(), Some(MAX_DRAIN_FRAMES));
        assert_eq!(produced, MAX_DRAIN_FRAMES);
        assert_eq!(recycled.len(), MAX_DRAIN_FRAMES - 1);
    }

    /// 帧池取空时返回 `None`，且不回收任何东西。
    #[test]
    fn drain_returns_none_when_pool_is_empty() {
        let mut recycled = Vec::new();
        let result = drain_newest(|| Ok::<_, ()>(None::<u32>), |item| recycled.push(item));
        assert!(result.is_ok(), "测试闭包不会返回错误");
        assert!(result.ok().flatten().is_none());
        assert!(recycled.is_empty());
    }

    /// 真正出错时既要向上报错，也要把已经收集到的帧回收掉（否则缓冲永远不归还）。
    #[test]
    fn drain_error_recycles_collected_items_and_propagates() {
        let mut calls = 0_u32;
        let mut recycled = Vec::new();
        let result = drain_newest(
            || {
                calls += 1;
                match calls {
                    1 => Ok::<Option<u32>, &'static str>(Some(7)),
                    _ => Err("boom"),
                }
            },
            |item| recycled.push(item),
        );
        assert_eq!(result, Err("boom"));
        assert_eq!(recycled, vec![7]);
    }
}
