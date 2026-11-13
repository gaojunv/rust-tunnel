//! DXGI Desktop Duplication 采集后端。
//!
//! WGC 不可用时的回退路径。按 HMONITOR 找到它所属的 `IDXGIAdapter`/`IDXGIOutput`，
//! 在**同一块适配器**上建 D3D11 设备，再 `IDXGIOutput1::DuplicateOutput`。
//!
//! # 无新帧与错误恢复
//!
//! - `AcquireNextFrame` 超时（`DXGI_ERROR_WAIT_TIMEOUT`）不是致命错误：首帧最多
//!   等 [`FIRST_FRAME_TIMEOUT`]，之后每次最多等 [`NEXT_FRAME_POLL_TIMEOUT`]，超时
//!   就复用上一帧；只有在**从未采到过帧**时才报错。
//! - `DXGI_ERROR_ACCESS_LOST`（切换分辨率/HDR/安全桌面、显卡驱动重置等）会触发
//!   一次重建：重新枚举该 HMONITOR 的输出并 `DuplicateOutput`，然后重试一次；
//!   重试仍失败才把错误交给调用方。

use super::{
    find_output, BgraReader, CaptureBackend, CaptureSource, CapturedFrame, MonitorInfo,
    FIRST_FRAME_TIMEOUT, NEXT_FRAME_POLL_TIMEOUT,
};
use crate::d3d11;
use crate::WinVideoError;
use windows::core::Interface as _;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIOutput, IDXGIOutput1, IDXGIOutput5, IDXGIOutputDuplication, IDXGIResource,
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
};
use windows::Win32::Graphics::Gdi::HMONITOR;

// SAFETY: `HMONITOR` 是进程级显示器句柄（不指向线程局部数据），Win32 允许在任意
// 线程使用；其余字段（D3D11/DXGI COM 对象）本身都实现 `Send`。
unsafe impl Send for DxgiSource {}

/// DXGI Desktop Duplication 采集源。
struct DxgiSource {
    /// 设备建在显示器所属适配器上。
    device: ID3D11Device,
    /// 立即上下文。
    context: ID3D11DeviceContext,
    /// 当前复制会话。
    duplication: IDXGIOutputDuplication,
    /// 目标显示器，重建时用它重新匹配输出。
    hmonitor: HMONITOR,
    /// BGRA 读回器（staging 纹理缓存）。
    reader: BgraReader,
    /// 最近一次成功采集的帧。
    last: Option<CapturedFrame>,
}

/// `AcquireNextFrame` 的一次结果（把「超时/丢失」从错误路径里分出来）。
enum Acquired {
    /// 成功拿到（可能为空的）桌面资源。
    Resource(Option<IDXGIResource>),
    /// 超时内没有新帧。
    Timeout,
    /// 复制会话失效，需要重建。
    AccessLost,
}

impl CaptureSource for DxgiSource {
    fn backend(&self) -> CaptureBackend {
        CaptureBackend::Dxgi
    }

    fn capture(&mut self) -> Result<CapturedFrame, WinVideoError> {
        match self.acquire()? {
            Acquired::Resource(resource) => self.finish_capture(resource.as_ref()),
            Acquired::Timeout => self.reuse("超时内没有新帧"),
            Acquired::AccessLost => {
                tracing::warn!("DXGI 桌面复制会话失效（ACCESS_LOST），尝试重建");
                self.rebuild()?;
                match self.acquire()? {
                    Acquired::Resource(resource) => self.finish_capture(resource.as_ref()),
                    Acquired::Timeout => self.reuse("重建后超时内没有新帧"),
                    Acquired::AccessLost => self.reuse("重建后再次 ACCESS_LOST"),
                }
            }
        }
    }
}

impl DxgiSource {
    /// 等待并取下一帧。
    fn acquire(&mut self) -> Result<Acquired, WinVideoError> {
        let timeout = if self.last.is_some() {
            NEXT_FRAME_POLL_TIMEOUT
        } else {
            FIRST_FRAME_TIMEOUT
        };
        let timeout_ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        // SAFETY: `duplication` 有效；`info`/`resource` 都是栈上输出参数。
        match unsafe {
            self.duplication
                .AcquireNextFrame(timeout_ms, &raw mut info, &raw mut resource)
        } {
            Ok(()) => Ok(Acquired::Resource(resource)),
            Err(err) if err.code() == DXGI_ERROR_WAIT_TIMEOUT => Ok(Acquired::Timeout),
            Err(err) if err.code() == DXGI_ERROR_ACCESS_LOST => Ok(Acquired::AccessLost),
            Err(err) => Err(WinVideoError::Capture(format!(
                "AcquireNextFrame 失败: {err}"
            ))),
        }
    }

    /// 读取已获取的帧，然后无条件 `ReleaseFrame`。
    fn finish_capture(
        &mut self,
        resource: Option<&IDXGIResource>,
    ) -> Result<CapturedFrame, WinVideoError> {
        // 先读，再无条件 ReleaseFrame；读取失败也必须释放，否则下一次
        // AcquireNextFrame 会一直失败。故这里不能用 `?` 提前返回。
        //
        // `resource == None` 表示本次只有指针更新、没有新的桌面图像（或首帧尚未
        // 就绪）：不做读回，交给 `reuse` 决定复用上一帧还是报错。
        let read = match resource {
            Some(resource) => self.read_resource(resource).map(Some),
            None => Ok(None),
        };
        // SAFETY: `AcquireNextFrame` 成功后必须配对调用 `ReleaseFrame`（即使读取失败）。
        let released = unsafe { self.duplication.ReleaseFrame() };
        let frame = read?;
        if let Err(err) = released {
            return Err(WinVideoError::Capture(format!("ReleaseFrame 失败: {err}")));
        }
        match frame {
            Some(frame) => {
                self.last = Some(frame.clone());
                Ok(frame)
            }
            None => self.reuse("本次只有指针更新，没有新的桌面图像"),
        }
    }

    /// 把桌面资源读成紧凑 BGRA（不负责 `ReleaseFrame`）。
    fn read_resource(&mut self, resource: &IDXGIResource) -> Result<CapturedFrame, WinVideoError> {
        let texture: ID3D11Texture2D = resource
            .cast()
            .map_err(|err| WinVideoError::Capture(format!("桌面资源转 D3D11 纹理失败: {err}")))?;
        let (width, height, bgra) = self.reader.read(&self.device, &self.context, &texture)?;
        Ok(CapturedFrame {
            width,
            height,
            bgra,
            backend: CaptureBackend::Dxgi,
        })
    }

    /// 复用上一帧；从未采到过帧时把原因报给调用方。
    fn reuse(&self, reason: &str) -> Result<CapturedFrame, WinVideoError> {
        match &self.last {
            Some(frame) => Ok(frame.clone()),
            None => Err(WinVideoError::Capture(format!(
                "DXGI: {reason}，且还没有任何已采集帧"
            ))),
        }
    }

    /// 会话失效后重建 `DuplicateOutput`。
    fn rebuild(&mut self) -> Result<(), WinVideoError> {
        let matched = find_output(self.hmonitor)?;
        let dxgi_device: IDXGIDevice = self.device.cast().map_err(|err| {
            WinVideoError::Capture(format!("D3D11 设备转 IDXGIDevice 失败: {err}"))
        })?;
        self.duplication = create_duplication(&matched.output, &dxgi_device)
            .map_err(|err| WinVideoError::Capture(format!("重建失败: {err}")))?;
        Ok(())
    }
}

/// 创建桌面复制会话。
///
/// 优先 `IDXGIOutput5::DuplicateOutput1` 并显式请求 `B8G8R8A8_UNORM`：SDR 桌面以及
/// 愿意做格式转换的驱动会直接给出 8-bit BGRA。**注意**：本机（Windows 11 + HDR
/// 显示器）实测 `DuplicateOutput1` 仍然返回 `R16G16B16A16_FLOAT`，即 DWM 不会替
/// 调用方做 HDR→SDR 色调映射；这种桌面下 DXGI 后端会以「不支持的纹理格式」失败，
/// 由上层回退 GDI `BitBlt`（WGC 主路径不受影响，DWM 会为 WGC 输出 BGRA）。
/// 老系统/驱动没有 `IDXGIOutput5` 时退回 `DuplicateOutput`。
fn create_duplication(
    output: &IDXGIOutput,
    device: &IDXGIDevice,
) -> Result<IDXGIOutputDuplication, WinVideoError> {
    match output.cast::<IDXGIOutput5>() {
        Ok(output5) => {
            let formats = [DXGI_FORMAT_B8G8R8A8_UNORM];
            // SAFETY: `output5`/`device` 有效且同属一块适配器；flags 必须为 0。
            match unsafe { output5.DuplicateOutput1(device, 0, &formats) } {
                Ok(duplication) => {
                    tracing::debug!("DXGI 复制会话已按 B8G8R8A8_UNORM 建立");
                    return Ok(duplication);
                }
                Err(err) => {
                    tracing::debug!(error = %err, "DuplicateOutput1 失败，回退 DuplicateOutput");
                }
            }
        }
        Err(err) => {
            tracing::debug!(error = %err, "IDXGIOutput 不支持 IDXGIOutput5，回退 DuplicateOutput");
        }
    }
    let output1: IDXGIOutput1 = output.cast().map_err(|err| {
        WinVideoError::Capture(format!("IDXGIOutput 转 IDXGIOutput1 失败: {err}"))
    })?;
    // SAFETY: `output1`/`device` 有效且同属一块适配器。
    unsafe { output1.DuplicateOutput(device) }
        .map_err(|err| WinVideoError::Capture(format!("DuplicateOutput 失败: {err}")))
}

/// 打开指定显示器的 DXGI Desktop Duplication 采集源。
pub(super) fn open(monitor: &MonitorInfo) -> Result<Box<dyn CaptureSource>, WinVideoError> {
    let matched = find_output(monitor.hmonitor)?;
    let d3d = d3d11::create(Some(matched.adapter_index))?;
    let dxgi_device: IDXGIDevice = d3d
        .device
        .cast()
        .map_err(|err| WinVideoError::Capture(format!("D3D11 设备转 IDXGIDevice 失败: {err}")))?;
    let duplication = create_duplication(&matched.output, &dxgi_device)?;

    tracing::info!(
        monitor = %monitor.device_name,
        adapter = %d3d.adapter.name,
        "DXGI 桌面复制采集源已打开"
    );
    Ok(Box::new(DxgiSource {
        device: d3d.device,
        context: d3d.context,
        duplication,
        hmonitor: monitor.hmonitor,
        reader: BgraReader::default(),
        last: None,
    }))
}
