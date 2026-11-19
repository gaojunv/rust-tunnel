//! WASAPI：默认回放设备的 loopback 采集 + 默认输出设备的播放。
//!
//! # 线程亲和
//!
//! WASAPI 对象是 COM 公寓绑定的：[`Capture`] / [`Render`] 必须在**创建它们的
//! 同一个线程**上使用与销毁。本模块用 [`ComGuard`] 在该线程上完成
//! `CoInitializeEx(MTA)`，并用 `PhantomData<*const ()>` 让它们 `!Send`——误移到
//! 其它线程会在编译期报错，而不是运行期出现难查的 `RPC_E_WRONG_THREAD`。
//!
//! # 为什么用轮询而不是事件
//!
//! loopback 采集的 event-driven 模式在 Win10 之前不可用，且事件句柄要额外一套
//! 生命周期；本项目按 20 ms 一帧消费，5 ms 轮询的调度开销可以忽略，换来的是在
//! GUI 被控端与 SYSTEM 无头实例上一致的行为。

use crate::error::{AudioError, AudioResult};
use crate::format::{
    wave_format, CHANNELS, FRAME_FLOATS, FRAME_FRAMES, FRAME_MS, SAMPLE_BYTES,
};
use std::marker::PhantomData;
use std::time::{Duration, Instant};
use windows::core::IUnknown;
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDevice,
    IMMDeviceCollection, IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, DEVICE_STATE_ACTIVE, WAVEFORMATEX,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::Sleep;

/// 采集流标志：loopback + 由音频引擎转换到我们的固定格式。
const LOOPBACK_FLAGS: u32 =
    AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;

/// 播放流标志：同样让音频引擎转换到 48 kHz / 立体声 / f32。
const RENDER_FLAGS: u32 =
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;

/// 流缓冲时长（100 ns 单位）：200 ms。20 ms 的消费/生产节拍留出 10 拍余量。
const BUFFER_HNS: i64 = 2_000_000;

/// `AUDCLNT_BUFFERFLAGS_SILENT`：缓冲区内容是未定义的，调用方必须按静音处理。
const BUFFERFLAGS_SILENT: u32 = 0x2;

/// 枚举出来的回放设备。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// 端点 ID（`IMMDevice::GetId`）。
    pub id: String,
    /// 是否为当前默认回放设备。
    pub is_default: bool,
}

/// 本线程的 COM 初始化守卫。
struct ComGuard {
    /// `CoInitializeEx` 返回成功（含 `S_FALSE`）时才由我们配对 `CoUninitialize`。
    uninit: bool,
}

impl ComGuard {
    /// 以 MTA 初始化当前线程。
    ///
    /// `RPC_E_CHANGED_MODE`（线程已被别人初始化成 STA）按成功处理：WASAPI 在两种
    /// 公寓里都能用，且此时**不该**由我们 `CoUninitialize`。
    fn init_mta() -> Self {
        // SAFETY: 第一个参数为空指针是 `CoInitializeEx` 允许的。
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        Self { uninit: hr.is_ok() }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.uninit {
            // SAFETY: 与同一线程上的 `CoInitializeEx` 成对。
            unsafe { CoUninitialize() };
        }
    }
}

/// 把 `PWSTR` 复制成 `String` 并释放 COM 分配的内存。
fn take_pwstr(ptr: windows::core::PWSTR) -> String {
    // SAFETY: `GetId` 返回以 NUL 结尾的 UTF-16 字符串，指针在释放前有效。
    let text = unsafe { ptr.to_string() }.unwrap_or_default();
    // SAFETY: `GetId` 返回的字符串由 `CoTaskMemAlloc` 分配，所有权转移给调用方。
    unsafe { CoTaskMemFree(Some(ptr.0.cast())) };
    text
}

fn default_render_device_id(enumerator: &IMMDeviceEnumerator) -> String {
    // SAFETY: 所有调用都在本函数内、同一线程，对象生命周期由局部变量持有。
    unsafe {
        let Ok(device) = enumerator.GetDefaultAudioEndpoint(eRender, eConsole) else {
            return String::new();
        };
        match device.GetId() {
            Ok(id) => take_pwstr(id),
            Err(_) => String::new(),
        }
    }
}

/// 列出当前所有活动的回放设备（探针/诊断用）。
///
/// # Errors
/// COM 或 WASAPI 枚举失败时返回 [`AudioError::Windows`]。
pub fn list_render_devices() -> AudioResult<Vec<DeviceInfo>> {
    let _com = ComGuard::init_mta();
    // SAFETY: COM 已在本线程初始化，局部变量持有接口直到函数结束。
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None::<&IUnknown>, CLSCTX_ALL)? };
    let default_id = default_render_device_id(&enumerator);
    let collection: IMMDeviceCollection =
        unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)? };
    let count = unsafe { collection.GetCount()? };
    let mut devices = Vec::with_capacity(count as usize);
    for index in 0..count {
        let device = unsafe { collection.Item(index)? };
        let id = take_pwstr(unsafe { device.GetId()? });
        devices.push(DeviceInfo {
            is_default: id == default_id,
            id,
        });
    }
    Ok(devices)
}

/// 默认回放设备的 loopback 采集源（被控端系统声音）。
pub struct Capture {
    client: IAudioClient,
    source: IAudioCaptureClient,
    _device: IMMDevice,
    /// 已从 WASAPI 读出、但还不够一个 20 ms 帧的采样点。
    pending: Vec<f32>,
    /// 流缓冲的帧数（诊断用）。
    buffer_frames: u32,
    /// 必须最后声明：先释放 COM 接口，再 `CoUninitialize`。
    _com: ComGuard,
    /// COM 对象是公寓绑定的，禁止跨线程移动。
    _not_send: PhantomData<*const ()>,
}

// SAFETY: WASAPI 的 MTA 对象是自由线程的；`_not_send` 只是防止误用的类型标记。
// 引擎保证 `start`/`read`/`stop` 都在同一个 `spawn_blocking` 线程上调用，且访问全部经
// `&mut self` 串行化。把已创建的 `Capture` 移到同一时刻只有一个使用者线程是安全的。
unsafe impl Send for Capture {}

impl Capture {
    /// 打开默认回放设备的 loopback 采集并启动。
    ///
    /// # Errors
    /// 没有默认回放设备、设备不支持 loopback、Initialize/Start 失败时返回
    /// [`AudioError::Windows`]。
    pub fn open_default_loopback() -> AudioResult<Self> {
        let com = ComGuard::init_mta();
        // SAFETY: COM 已初始化；接口与设备在结构体中保有所有权。
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None::<&IUnknown>, CLSCTX_ALL)? };
        let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole)? };
        let client: IAudioClient =
            unsafe { device.Activate(CLSCTX_ALL, None::<*const PROPVARIANT>)? };
        let format = wave_format();
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                LOOPBACK_FLAGS,
                BUFFER_HNS,
                0,
                std::ptr::addr_of!(format).cast::<WAVEFORMATEX>(),
                None,
            )?;
        }
        let buffer_frames = unsafe { client.GetBufferSize()? };
        let capture: IAudioCaptureClient = unsafe { client.GetService()? };
        unsafe { client.Start()? };
        Ok(Self {
            client,
            source: capture,
            _device: device,
            pending: Vec::with_capacity(FRAME_FLOATS * 2),
            buffer_frames,
            _com: com,
            _not_send: PhantomData,
        })
    }

    /// 流缓冲的帧数（诊断用）。
    #[must_use]
    pub fn buffer_frames(&self) -> u32 {
        self.buffer_frames
    }

    /// 阻塞读取一个完整的 20 ms 帧（`FRAME_FLOATS` 个交错 `f32`）。
    ///
    /// 返回 `Ok(false)` 表示在 `timeout` 内没有攒够一帧（调用方应检查停止标志）。
    /// 设备失效（拔耳机/切默认设备）会返回 `Err`，调用方应重建采集源。
    ///
    /// # Errors
    /// WASAPI 调用失败时返回 [`AudioError::Windows`]。
    #[expect(
        clippy::cast_ptr_alignment,
        reason = "WASAPI 的 f32 采样缓冲按 4 字节对齐；GetBuffer 保证按当前格式对齐"
    )]
    pub fn read_block(&mut self, out: &mut Vec<f32>, timeout: Duration) -> AudioResult<bool> {
        let deadline = Instant::now() + timeout;
        out.clear();
        while self.pending.len() < FRAME_FLOATS {
            let mut available = unsafe { self.source.GetNextPacketSize()? };
            if available == 0 {
                if Instant::now() >= deadline {
                    return Ok(false);
                }
                // 5 ms 轮询：20 ms 一帧的消费节奏下，这个粒度足够且不占 CPU。
                unsafe { Sleep(5) };
                continue;
            }
            while available != 0 {
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0_u32;
                let mut flags = 0_u32;
                unsafe {
                    self.source
                        .GetBuffer(
                            std::ptr::addr_of_mut!(data),
                            std::ptr::addr_of_mut!(frames),
                            std::ptr::addr_of_mut!(flags),
                            None,
                            None,
                        )?;
                }
                let samples = frames as usize * usize::from(CHANNELS);
                let start = self.pending.len();
                self.pending.resize(start + samples, 0.0);
                if flags & BUFFERFLAGS_SILENT == 0 && !data.is_null() && samples > 0 {
                    // SAFETY: GetBuffer 保证 `data` 指向 `frames * channels` 个 f32；
                    // 长度已按同一口径计算。
                    let src = unsafe { std::slice::from_raw_parts(data.cast::<f32>(), samples) };
                    self.pending[start..].copy_from_slice(src);
                }
                unsafe { self.source.ReleaseBuffer(frames)? };
                available = unsafe { self.source.GetNextPacketSize()? };
            }
        }
        out.extend_from_slice(&self.pending[..FRAME_FLOATS]);
        self.pending.drain(..FRAME_FLOATS);
        Ok(true)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // SAFETY: 与 Start 成对；销毁路径上忽略错误（没有可补救的动作）。
        let _ = unsafe { self.client.Stop() };
    }
}

/// 默认输出设备的播放端（控制端）。
pub struct Render {
    client: IAudioClient,
    sink: IAudioRenderClient,
    _device: IMMDevice,
    buffer_frames: u32,
    _com: ComGuard,
    _not_send: PhantomData<*const ()>,
}

impl Render {
    /// 打开默认输出设备并启动播放流。
    ///
    /// # Errors
    /// 没有默认输出设备或 Initialize/Start 失败时返回 [`AudioError::Windows`]。
    pub fn open_default_output() -> AudioResult<Self> {
        let com = ComGuard::init_mta();
        // SAFETY: COM 已初始化；接口与设备在结构体中保有所有权。
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None::<&IUnknown>, CLSCTX_ALL)? };
        let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole)? };
        let client: IAudioClient =
            unsafe { device.Activate(CLSCTX_ALL, None::<*const PROPVARIANT>)? };
        let format = wave_format();
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                RENDER_FLAGS,
                BUFFER_HNS,
                0,
                std::ptr::addr_of!(format).cast::<WAVEFORMATEX>(),
                None,
            )?;
        }
        let buffer_frames = unsafe { client.GetBufferSize()? };
        let render: IAudioRenderClient = unsafe { client.GetService()? };
        unsafe { client.Start()? };
        Ok(Self {
            client,
            sink: render,
            _device: device,
            buffer_frames,
            _com: com,
            _not_send: PhantomData,
        })
    }

    /// 流缓冲的帧数。
    #[must_use]
    pub fn buffer_frames(&self) -> u32 {
        self.buffer_frames
    }

    /// 当前缓冲里待播放的帧数（抖动缓冲的自适应读数）。
    ///
    /// # Errors
    /// WASAPI 调用失败时返回 [`AudioError::Windows`]。
    pub fn padding_frames(&self) -> AudioResult<u32> {
        Ok(unsafe { self.client.GetCurrentPadding()? })
    }

    /// 写入一个 20 ms 帧；缓冲满时阻塞等待。
    ///
    /// `pcm` 至少要有 `FRAME_FLOATS` 个采样点，多余的忽略。
    ///
    /// # Errors
    /// PCM 长度不足或 WASAPI 调用失败时返回错误。
    pub fn write_block(&mut self, pcm: &[f32]) -> AudioResult<()> {
        if pcm.len() < FRAME_FLOATS {
            return Err(AudioError::Message(format!(
                "PCM 不足一帧：{} < {FRAME_FLOATS}",
                pcm.len()
            )));
        }
        let frames = u32::try_from(FRAME_FRAMES)
            .map_err(|_| AudioError::Message("帧长溢出".to_owned()))?;
        loop {
            let padding = unsafe { self.client.GetCurrentPadding()? };
            if padding.saturating_add(frames) <= self.buffer_frames {
                break;
            }
            // 2 ms 一探：播放端由缓冲水位自然限速，不需要单独的定时线程。
            unsafe { Sleep(2) };
        }
        let data = unsafe { self.sink.GetBuffer(frames)? };
        let bytes = FRAME_FLOATS * SAMPLE_BYTES;
        // SAFETY: GetBuffer 保证至少有 `frames` 帧可写；`bytes` 正好是这些帧的字节数。
        unsafe {
            std::ptr::copy_nonoverlapping(pcm.as_ptr().cast::<u8>(), data, bytes);
        }
        unsafe { self.sink.ReleaseBuffer(frames, 0)? };
        Ok(())
    }
}

impl Drop for Render {
    fn drop(&mut self) {
        // SAFETY: 与 Start 成对；销毁路径上忽略错误。
        let _ = unsafe { self.client.Stop() };
    }
}

/// 一帧的毫秒数（供调用方编排采集/播放节拍）。
#[must_use]
pub const fn frame_ms() -> u32 {
    FRAME_MS
}
