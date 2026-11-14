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

use super::{
    create_device_for, texture_dimensions, BgraReader, CaptureBackend, CaptureSource,
    CapturedFrame, CapturedTexture, MonitorInfo, TexturePool, FIRST_FRAME_TIMEOUT,
    NEXT_FRAME_POLL_TIMEOUT, POLL_INTERVAL,
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
    /// 最近一次 GPU 采集结果（无新帧时返回它的克隆）。
    last_texture: Option<CapturedTexture>,
}

impl CaptureSource for WgcSource {
    fn backend(&self) -> CaptureBackend {
        CaptureBackend::Wgc
    }

    fn capture(&mut self) -> Result<CapturedFrame, WinVideoError> {
        let deadline = Instant::now() + self.capture_timeout();
        loop {
            match self.pool.TryGetNextFrame() {
                Ok(frame) => {
                    let captured = self.read_frame(&frame)?;
                    self.last = Some(captured.clone());
                    return Ok(captured);
                }
                // 帧池没有新帧时 `from_abi` 把空指针折成 `Error::empty()`，
                // 其 HRESULT 为 0（成功码）。真正的 HRESULT 失败才向上报。
                Err(err) if err.code().is_ok() => {}
                Err(err) => {
                    return Err(WinVideoError::Capture(format!(
                        "WGC TryGetNextFrame 失败: {err}"
                    )))
                }
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        match &self.last {
            Some(frame) => Ok(frame.clone()),
            None => Err(WinVideoError::Capture(format!(
                "WGC: {:?} 内没有收到首帧",
                self.capture_timeout()
            ))),
        }
    }

    fn capture_texture(&mut self) -> Result<CapturedTexture, WinVideoError> {
        self.capture_texture_with_change().map(|(texture, _)| texture)
    }

    fn capture_texture_with_change(
        &mut self,
    ) -> Result<(CapturedTexture, bool), WinVideoError> {
        let timeout = self.capture_timeout();
        let deadline = Instant::now() + timeout;
        loop {
            match self.pool.TryGetNextFrame() {
                Ok(frame) => {
                    let result = self.copy_surface(&frame);
                    // SAFETY 说明：Close 失败只影响缓冲归还时机，不影响已拷贝的数据。
                    if let Err(err) = frame.Close() {
                        tracing::debug!(error = %err, "WGC 帧对象 Close 失败");
                    }
                    let captured = result?;
                    self.last_texture = Some(captured.clone());
                    // 真的拿到了新帧：只有这一条路径需要 `CopyResource`。
                    return Ok((captured, true));
                }
                Err(err) if err.code().is_ok() => {}
                Err(err) => {
                    return Err(WinVideoError::Capture(format!(
                        "WGC TryGetNextFrame 失败: {err}"
                    )))
                }
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        // 超时：直接复用上一张纹理，**不做 CopyResource**（画面没变，拷贝纯属浪费）。
        // 返回 `false` 让上层跳过编码/发送；旧语义（恒为 `true`）由默认实现保留。
        match &self.last_texture {
            Some(texture) => Ok((texture.clone(), false)),
            None => Err(WinVideoError::Capture(format!(
                "WGC: {timeout:?} 内没有收到首帧（GPU 纹理）"
            ))),
        }
    }

    fn device(&self) -> Option<Arc<d3d11::D3d11>> {
        Some(self.d3d.clone())
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
        // SAFETY 说明：`Close` 是安全方法；失败只影响缓冲归还时机，不影响已读数据。
        if let Err(err) = frame.Close() {
            tracing::debug!(error = %err, "WGC 帧对象 Close 失败");
        }
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
        last_texture: None,
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
