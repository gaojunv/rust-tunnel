//! VideoToolbox **硬件编码**（H.264 / H.265，4:2:0）。
//!
//! # 输入与输出
//!
//! 编码输入是 `kCVPixelFormatType_32BGRA` 的 `CVPixelBuffer`，逐行 `memcpy` 拷进去，
//! 由 VideoToolbox 内部转 4:2:0（**不要**自己先转 NV12——多一次 CPU 往返且丢掉
//! VT 的硬件色彩转换）。编码输出是 AVCC，本模块转成 Annex B 后返回；关键帧额外从
//! `CMVideoFormatDescription` 取参数集前置。
//!
//! # 会话属性（低延迟）
//!
//! - `RealTime = true`、`AllowFrameReordering = false`：不做 B 帧/重排序，编码结果
//!   **编码顺序 = 显示顺序**，这正是会话循环「按序号断层请求关键帧」的前提；
//! - `MaxKeyFrameInterval = 1_000_000`：不要让编码器周期性插 IDR（IDR 是带宽尖峰，
//!   关键帧由引擎显式请求）；
//! - `AverageBitRate = kbps * 1000`、`ExpectedFrameRate = fps`：QoS 闭环每秒在线改这两项，
//!   **只调 `VTSessionSetProperty`，绝不重建会话、绝不插 IDR**；
//! - `MaxAllowedFrameQP = max_qp`（默认 20，对齐 Windows 的 `MAX_QP`）：画质地板，
//!   只给码率时编码器会把预算摊到整幅画面、文字被平均掉；
//! - 颜色三件套按 BT.709 打在**像素缓冲属性**上（VT 从 buffer attachment 读颜色信息）。
//!
//! # 硬件
//!
//! 创建时传 `kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder = true`，
//! 但**不 Require**：Intel Mac 上 HEVC 可能没有硬编，Require 会让会话直接创建失败。
//! 实际是否硬件由 `kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder` 读出。
//!
//! # 线程
//!
//! 编码器**独占使用**（引擎移进 `spawn_blocking`）：本类型 `Send` 但非 `Sync`。

use crate::annexb;
use crate::codec::CodecKind;
use crate::color;
use crate::error::{MacVideoError, MacVideoResult};
use objc2_core_foundation::{
    kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFBoolean, CFDictionary,
    CFNumber, CFRetained, CFString, CFType,
};
use objc2_core_media::{kCMTimeInvalid, CMSampleBuffer, CMTime};
use objc2_core_video::{
    kCVImageBufferColorPrimariesKey, kCVImageBufferColorPrimaries_ITU_R_709_2,
    kCVImageBufferTransferFunctionKey, kCVImageBufferTransferFunction_ITU_R_709_2,
    kCVImageBufferYCbCrMatrixKey, kCVImageBufferYCbCrMatrix_ITU_R_709_2, kCVPixelBufferHeightKey,
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
    kCVPixelFormatType_32BGRA, CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddress,
    CVPixelBufferGetBytesPerRow, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress,
};
use objc2_video_toolbox::{
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ColorPrimaries, kVTCompressionPropertyKey_ExpectedFrameRate,
    kVTCompressionPropertyKey_MaxAllowedFrameQP, kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
    kVTCompressionPropertyKey_TransferFunction,
    kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder,
    kVTCompressionPropertyKey_YCbCrMatrix, kVTEncodeFrameOptionKey_ForceKeyFrame,
    kVTProfileLevel_H264_Main_AutoLevel, kVTProfileLevel_HEVC_Main_AutoLevel,
    kVTPropertyNotSupportedErr, kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
    VTCompressionSession, VTEncodeInfoFlags,
};
use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tracing::{debug, info};

/// 默认画质地板（QP 上限），对齐 Windows 侧的 `MAX_QP = 20`。
pub const DEFAULT_MAX_QP: u32 = 20;

/// 编码尺寸上限（与 remote 的 `MAX_FRAME_DIM` 一致，防止异常输入把 VT 喂崩）。
pub const MAX_FRAME_DIM: u32 = 16384;

/// 等待编码回调的上限。
///
/// 硬件编码一帧 1080p 通常 <10 ms，2 s 足以覆盖「首帧初始化」；超时视为编码器故障，
/// 调用方应丢弃实例后重建（`failed` 会拒绝后续调用，避免半死状态）。
pub const ENCODE_TIMEOUT: Duration = Duration::from_secs(2);

/// 编码器配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodeConfig {
    /// 编码标准。
    pub codec: CodecKind,
    /// 编码宽度（像素，偶数）。
    pub width: u32,
    /// 编码高度（像素，偶数）。
    pub height: u32,
    /// 目标码率（kbps）。
    pub bitrate_kbps: u32,
    /// 目标帧率。
    pub fps: u32,
    /// QP 上限（画质地板）。
    pub max_qp: u32,
}

impl EncodeConfig {
    /// 用默认 QP 上限（[`DEFAULT_MAX_QP`]）构造配置。
    #[must_use]
    pub const fn new(
        codec: CodecKind,
        width: u32,
        height: u32,
        bitrate_kbps: u32,
        fps: u32,
    ) -> Self {
        Self {
            codec,
            width,
            height,
            bitrate_kbps,
            fps,
            max_qp: DEFAULT_MAX_QP,
        }
    }

    /// 参数自检。
    ///
    /// # Errors
    /// 尺寸为零/超上限/非偶数、码率为零、帧率为零、QP 上限超出 `1..=51` 时返回
    /// [`MacVideoError::Config`]。
    pub fn validate(&self) -> MacVideoResult<()> {
        if self.width < 2
            || self.height < 2
            || self.width > MAX_FRAME_DIM
            || self.height > MAX_FRAME_DIM
        {
            return Err(MacVideoError::Config(format!(
                "编码尺寸非法：{}x{}（上限 {MAX_FRAME_DIM}）",
                self.width, self.height
            )));
        }
        if !self.width.is_multiple_of(2) || !self.height.is_multiple_of(2) {
            return Err(MacVideoError::Config(format!(
                "编码尺寸必须为偶数（4:2:0）：{}x{}",
                self.width, self.height
            )));
        }
        if self.bitrate_kbps == 0 {
            return Err(MacVideoError::Config("目标码率不能为 0".to_owned()));
        }
        if self.fps == 0 {
            return Err(MacVideoError::Config("帧率不能为 0".to_owned()));
        }
        if !(1..=51).contains(&self.max_qp) {
            return Err(MacVideoError::Config(format!(
                "QP 上限必须落在 1..=51，收到 {}",
                self.max_qp
            )));
        }
        Ok(())
    }
}

/// 编码器自报信息（供上层映射成 remote 的 `EncoderInfo`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderInfo {
    /// 后端名（恒为 `"VideoToolbox"`）。
    pub backend: String,
    /// 编码器/编码标准名（例如 `"VT H.264"`）。
    pub name: String,
    /// 是否硬件编码。
    pub hardware: bool,
    /// 编码宽度。
    pub width: u32,
    /// 编码高度。
    pub height: u32,
    /// 当前目标码率（kbps）。
    pub bitrate_kbps: u32,
    /// 当前目标帧率。
    pub fps: u32,
}

/// 一次编码的输出。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedPacket {
    /// Annex B 码流（关键帧已前置参数集）。
    pub data: Vec<u8>,
    /// 本帧是否关键帧（IDR / IRAP）。
    pub keyframe: bool,
    /// 编码宽度。
    pub width: u32,
    /// 编码高度。
    pub height: u32,
}

/// 回调线程产出的原始结果（参数集已前置）。
#[derive(Debug, Clone)]
struct EncodedOutput {
    /// Annex B 码流。
    data: Vec<u8>,
    /// 是否关键帧。
    keyframe: bool,
}

/// 编码回调与编码线程之间的交接槽。
#[derive(Debug)]
struct OutputSlot {
    /// 本次调用的结果；`None` 表示回调还没来。
    output: Option<MacVideoResult<EncodedOutput>>,
}

/// 回调共享状态（`refCon` 指向它；用 `Arc` 保证生命周期覆盖会话）。
struct SharedOutput {
    /// 编码标准（回调里解析 NAL 用）。
    codec: CodecKind,
    /// 交接槽。
    slot: Mutex<OutputSlot>,
    /// 条件变量（编码线程等待回调）。
    ready: Condvar,
}

/// VideoToolbox 硬件编码器。
pub struct VtEncoder {
    /// 压缩会话（`Drop` 里 `invalidate` 后再释放）。
    session: CFRetained<VTCompressionSession>,
    /// 当前生效配置。
    config: EncodeConfig,
    /// 是否硬件编码（创建后读一次）。
    hardware: bool,
    /// 回调共享状态。
    shared: Arc<SharedOutput>,
    /// 复用的输入像素缓冲（每次 `encode` 都覆盖；编码是同步的，VT 用完才返回）。
    input: Option<CFRetained<CVPixelBuffer>>,
    /// 单调递增的显示时间戳。
    frame_index: i64,
    /// 「强制关键帧」的帧属性字典（创建一次复用）。
    force_keyframe_properties: CFRetained<CFDictionary>,
    /// 编码器是否已进入不可恢复状态（超时/故障）：拒绝后续编码，强制上层重建。
    failed: bool,
}

// SAFETY: VideoToolbox 的会话/像素缓冲句柄不是 `Send`，但引擎把编码器**独占**移进
// `spawn_blocking`，任何时刻只有一个线程调用它；回调线程只访问 `Arc<SharedOutput>`
// （`Mutex` + `Condvar`，本身是 `Send + Sync`）。因此跨线程移动是安全的。
unsafe impl Send for VtEncoder {}

impl std::fmt::Debug for VtEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VtEncoder")
            .field("config", &self.config)
            .field("hardware", &self.hardware)
            .field("failed", &self.failed)
            .finish_non_exhaustive()
    }
}

impl Drop for VtEncoder {
    fn drop(&mut self) {
        // SAFETY: `session` 有效；invalidate 之后 VT 不再回调 `shared`，随后 `CFRetained`
        // 的析构会释放会话引用。`shared` 声明在前、`session` 声明在后，字段按声明顺序
        // 析构，因此 `Arc` 比会话活得更久。
        unsafe { self.session.invalidate() };
    }
}

impl VtEncoder {
    /// 创建并配置一个压缩会话。
    ///
    /// # Errors
    /// 配置非法、会话创建失败、必需属性设置失败时返回错误。
    pub fn new(config: EncodeConfig) -> MacVideoResult<Self> {
        config.validate()?;
        let shared = Arc::new(SharedOutput {
            codec: config.codec,
            slot: Mutex::new(OutputSlot { output: None }),
            ready: Condvar::new(),
        });
        let ref_con = Arc::as_ptr(&shared).cast_mut().cast::<c_void>();
        let specification = encoder_specification()?;
        let source_attributes = pixel_buffer_attributes(config.width, config.height)?;
        let (width, height) = (
            i32::try_from(config.width)
                .map_err(|_| MacVideoError::Config("编码宽度超出 i32".to_owned()))?,
            i32::try_from(config.height)
                .map_err(|_| MacVideoError::Config("编码高度超出 i32".to_owned()))?,
        );
        let mut session_ptr: *mut VTCompressionSession = ptr::null_mut();
        let status = unsafe {
            VTCompressionSession::create(
                None,
                width,
                height,
                config.codec.codec_type(),
                Some(&specification),
                Some(&source_attributes),
                None,
                Some(compression_output_callback),
                ref_con,
                NonNull::from(&mut session_ptr),
            )
        };
        if status != 0 {
            return Err(MacVideoError::os_status(
                "VTCompressionSessionCreate",
                status,
            ));
        }
        let session = unsafe {
            CFRetained::from_raw(NonNull::new(session_ptr).ok_or_else(|| {
                MacVideoError::Encode("VTCompressionSessionCreate 返回空会话".to_owned())
            })?)
        };
        apply_properties(&session, &config)?;
        let hardware = read_session_bool(&session, unsafe {
            kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder
        })
        .unwrap_or(false);
        let force_keyframe_properties = force_keyframe_dictionary()?;
        info!(
            codec = config.codec.label(),
            width = config.width,
            height = config.height,
            hardware,
            "VideoToolbox 编码会话已创建"
        );
        Ok(Self {
            session,
            config,
            hardware,
            shared,
            input: None,
            frame_index: 0,
            force_keyframe_properties,
            failed: false,
        })
    }

    /// 编码尺寸（宽, 高）。
    #[must_use]
    pub const fn dimensions(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    /// 当前目标码率（kbps）。
    #[must_use]
    pub const fn bitrate_kbps(&self) -> u32 {
        self.config.bitrate_kbps
    }

    /// 当前目标帧率。
    #[must_use]
    pub const fn fps(&self) -> u32 {
        self.config.fps
    }

    /// 是否硬件编码。
    #[must_use]
    pub const fn is_hardware(&self) -> bool {
        self.hardware
    }

    /// 编码标准。
    #[must_use]
    pub const fn codec(&self) -> CodecKind {
        self.config.codec
    }

    /// 编码器信息。
    #[must_use]
    pub fn info(&self) -> EncoderInfo {
        EncoderInfo {
            backend: "VideoToolbox".to_owned(),
            name: format!("VT {}", self.config.codec.label()),
            hardware: self.hardware,
            width: self.config.width,
            height: self.config.height,
            bitrate_kbps: self.config.bitrate_kbps,
            fps: self.config.fps,
        }
    }

    /// **在线**重配置目标码率（不重建会话、不插 IDR）。
    ///
    /// 编码器若明确不支持在线改码率（`kVTPropertyNotSupportedErr`），保留旧值并返回
    /// `Ok`——每秒一次的 QoS 闭环失败就重建编码器会强制每帧 IDR，代价远大于「码率不生效」。
    ///
    /// # Errors
    /// 除「属性不支持」外的 `VTSessionSetProperty` 失败返回错误（调用方应丢弃编码器）。
    pub fn set_bitrate(&mut self, kbps: u32) -> MacVideoResult<()> {
        if kbps == 0 {
            return Err(MacVideoError::Config("目标码率不能为 0".to_owned()));
        }
        let value = i64::from(kbps).saturating_mul(1000);
        let status = set_session_i64(
            &self.session,
            unsafe { kVTCompressionPropertyKey_AverageBitRate },
            value,
        );
        if status == kVTPropertyNotSupportedErr {
            debug!(kbps, "编码器不支持在线改码率，保留旧值");
            return Ok(());
        }
        if status != 0 {
            return Err(MacVideoError::os_status("设置 AverageBitRate", status));
        }
        self.config.bitrate_kbps = kbps;
        Ok(())
    }

    /// **在线**重配置目标帧率（不重建会话、不插 IDR）。
    ///
    /// 与 [`Self::set_bitrate`] 相同的容错口径：属性不支持时保留旧值。
    ///
    /// # Errors
    /// 帧率为零，或除「属性不支持」外的 `VTSessionSetProperty` 失败时返回错误。
    pub fn set_fps(&mut self, fps: u32) -> MacVideoResult<()> {
        if fps == 0 {
            return Err(MacVideoError::Config("帧率不能为 0".to_owned()));
        }
        let value =
            i32::try_from(fps).map_err(|_| MacVideoError::Config("帧率超出 i32".to_owned()))?;
        let status = set_session_i32(
            &self.session,
            unsafe { kVTCompressionPropertyKey_ExpectedFrameRate },
            value,
        );
        if status == kVTPropertyNotSupportedErr {
            debug!(fps, "编码器不支持在线改帧率，保留旧值");
            return Ok(());
        }
        if status != 0 {
            return Err(MacVideoError::os_status("设置 ExpectedFrameRate", status));
        }
        self.config.fps = fps;
        Ok(())
    }

    /// 编码一帧自顶向下 BGRA。
    ///
    /// 源尺寸与编码尺寸不同时先做盒式下采样（BGRA→BGRA），再逐行拷进
    /// `kCVPixelFormatType_32BGRA` 的 `CVPixelBuffer`。
    ///
    /// # Errors
    /// 输入长度与源尺寸不符、编码器已失效、VideoToolbox 返回错误或回调超时时返回错误。
    pub fn encode_bgra(
        &mut self,
        bgra: &[u8],
        src_width: u32,
        src_height: u32,
        force_keyframe: bool,
    ) -> MacVideoResult<EncodedPacket> {
        if self.failed {
            return Err(MacVideoError::Encode(
                "编码器已失效（上次编码超时/故障），需要重建".to_owned(),
            ));
        }
        let expected = usize::try_from(src_width)
            .ok()
            .and_then(|w| usize::try_from(src_height).ok().map(|h| (w, h)))
            .and_then(|(w, h)| w.checked_mul(h))
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| MacVideoError::Config("源帧尺寸溢出".to_owned()))?;
        if bgra.len() != expected {
            return Err(MacVideoError::Config(format!(
                "BGRA 长度 {} 与源尺寸 {src_width}x{src_height} 不符（应为 {expected}）",
                bgra.len()
            )));
        }
        let scaled;
        let source = if (src_width, src_height) == (self.config.width, self.config.height) {
            bgra
        } else {
            scaled = color::scale_bgra_box(
                bgra,
                src_width,
                src_height,
                self.config.width,
                self.config.height,
            )?;
            &scaled
        };
        let buffer = self.upload(source)?;
        // 提交前清空交接槽（上一帧已取走；这里防御性地再清一次）。
        {
            let mut slot = self
                .shared
                .slot
                .lock()
                .map_err(|_| MacVideoError::Encode("编码交接槽锁已中毒".to_owned()))?;
            slot.output = None;
        }
        let fps = i32::try_from(self.config.fps.max(1)).unwrap_or(1);
        let presentation_time = unsafe { CMTime::new(self.frame_index, fps) };
        let duration = unsafe { CMTime::new(1, fps) };
        self.frame_index = self.frame_index.saturating_add(1);
        let properties: Option<&CFDictionary> = if force_keyframe {
            Some(&self.force_keyframe_properties)
        } else {
            None
        };
        let mut info_flags = VTEncodeInfoFlags(0);
        let status = unsafe {
            self.session.encode_frame(
                &buffer,
                presentation_time,
                duration,
                properties,
                ptr::null_mut(),
                &raw mut info_flags,
            )
        };
        if status != 0 {
            self.failed = true;
            return Err(MacVideoError::os_status(
                "VTCompressionSessionEncodeFrame",
                status,
            ));
        }
        if info_flags.contains(VTEncodeInfoFlags::FrameDropped) {
            self.failed = true;
            return Err(MacVideoError::Encode(
                "VideoToolbox 丢弃了输入帧".to_owned(),
            ));
        }
        // 硬件编码器可能有一帧流水线延迟：强制把已提交的帧全部吐出来，回调才会到达。
        // `kCMTimeInvalid` = 完成所有挂起帧（不是「不完成」）。
        let complete = unsafe { self.session.complete_frames(kCMTimeInvalid) };
        if complete != 0 {
            debug!(status = complete, "VTCompressionSessionCompleteFrames 失败");
        }
        let output = self.wait_for_output()?;
        Ok(EncodedPacket {
            data: output.data,
            keyframe: output.keyframe,
            width: self.config.width,
            height: self.config.height,
        })
    }

    /// 把 BGRA 拷进一个 `32BGRA` 的 `CVPixelBuffer`（按需创建/复用）。
    fn upload(&mut self, bgra: &[u8]) -> MacVideoResult<CFRetained<CVPixelBuffer>> {
        let buffer = self.input_buffer()?;
        let lock_status =
            unsafe { CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags(0)) };
        if lock_status != 0 {
            return Err(MacVideoError::os_status(
                "CVPixelBufferLockBaseAddress",
                lock_status,
            ));
        }
        let row_bytes = usize::try_from(self.config.width)
            .ok()
            .and_then(|w| w.checked_mul(4))
            .ok_or_else(|| MacVideoError::Config("编码行宽溢出".to_owned()))?;
        let height = usize::try_from(self.config.height).unwrap_or(0);
        let base = CVPixelBufferGetBaseAddress(&buffer).cast::<u8>();
        let pitch = CVPixelBufferGetBytesPerRow(&buffer);
        if base.is_null() || pitch < row_bytes {
            unsafe { CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags(0)) };
            return Err(MacVideoError::Encode(format!(
                "像素缓冲不可写（base 空或 pitch {pitch} < 行宽 {row_bytes}）"
            )));
        }
        for row in 0..height {
            let src_start = row.saturating_mul(row_bytes);
            let dst_start = row.saturating_mul(pitch);
            let Some(src_row) = bgra.get(src_start..src_start.saturating_add(row_bytes)) else {
                unsafe { CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags(0)) };
                return Err(MacVideoError::Encode("BGRA 行越界".to_owned()));
            };
            // SAFETY: `base + dst_start` 在 pitch×height 的缓冲内（pitch ≥ row_bytes，
            // row < height）；源切片长度已按 width×height×4 校验。
            unsafe {
                ptr::copy_nonoverlapping(src_row.as_ptr(), base.add(dst_start), row_bytes);
            }
        }
        let unlock_status =
            unsafe { CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags(0)) };
        if unlock_status != 0 {
            return Err(MacVideoError::os_status(
                "CVPixelBufferUnlockBaseAddress",
                unlock_status,
            ));
        }
        Ok(buffer)
    }

    /// 取（或创建）与编码尺寸匹配的输入像素缓冲。
    fn input_buffer(&mut self) -> MacVideoResult<CFRetained<CVPixelBuffer>> {
        if let Some(buffer) = &self.input {
            return Ok(buffer.clone());
        }
        let attributes = pixel_buffer_attributes(self.config.width, self.config.height)?;
        let mut buffer_ptr: *mut CVPixelBuffer = ptr::null_mut();
        let status = unsafe {
            CVPixelBufferCreate(
                None,
                usize::try_from(self.config.width).unwrap_or(0),
                usize::try_from(self.config.height).unwrap_or(0),
                kCVPixelFormatType_32BGRA,
                Some(&attributes),
                NonNull::from(&mut buffer_ptr),
            )
        };
        if status != 0 {
            return Err(MacVideoError::os_status("CVPixelBufferCreate", status));
        }
        let buffer = unsafe {
            CFRetained::from_raw(NonNull::new(buffer_ptr).ok_or_else(|| {
                MacVideoError::Encode("CVPixelBufferCreate 返回空缓冲".to_owned())
            })?)
        };
        self.input = Some(buffer.clone());
        Ok(buffer)
    }

    /// 等待回调把结果写进交接槽（最多 [`ENCODE_TIMEOUT`]）。
    fn wait_for_output(&mut self) -> MacVideoResult<EncodedOutput> {
        let deadline = Instant::now() + ENCODE_TIMEOUT;
        let mut slot = self
            .shared
            .slot
            .lock()
            .map_err(|_| MacVideoError::Encode("编码交接槽锁已中毒".to_owned()))?;
        loop {
            if let Some(result) = slot.output.take() {
                return result;
            }
            let now = Instant::now();
            if now >= deadline {
                self.failed = true;
                return Err(MacVideoError::Encode(format!(
                    "VideoToolbox 编码超时（>{ENCODE_TIMEOUT:?}）"
                )));
            }
            let remaining = deadline.saturating_duration_since(now);
            let (guard, _) = self
                .shared
                .ready
                .wait_timeout(slot, remaining)
                .map_err(|_| MacVideoError::Encode("编码等待锁已中毒".to_owned()))?;
            slot = guard;
        }
    }
}

/// VideoToolbox 压缩输出回调。
///
/// # Safety
/// `ref_con` 必须是 [`VtEncoder::new`] 里传入的 `Arc<SharedOutput>` 裸指针，且
/// 会话在 `Arc` 释放前被 invalidate（由 [`Drop for VtEncoder`] 保证）。
unsafe extern "C-unwind" fn compression_output_callback(
    ref_con: *mut c_void,
    _source_frame_ref_con: *mut c_void,
    status: i32,
    _info_flags: VTEncodeInfoFlags,
    sample_buffer: *mut CMSampleBuffer,
) {
    if ref_con.is_null() {
        return;
    }
    // SAFETY: 由 `VtEncoder` 保证 ref_con 指向活着的 `Arc<SharedOutput>`。
    let shared = unsafe { &*ref_con.cast::<SharedOutput>() };
    let result = if status != 0 {
        Err(MacVideoError::os_status("VideoToolbox 编码回调", status))
    } else if sample_buffer.is_null() {
        Err(MacVideoError::Encode("编码回调没有输出样本".to_owned()))
    } else {
        // SAFETY: VT 在回调期间持有 sample_buffer；这里只借用，不跨回调保留。
        build_output(shared.codec, unsafe { &*sample_buffer })
    };
    if let Ok(mut slot) = shared.slot.lock() {
        slot.output = Some(result);
    }
    shared.ready.notify_all();
}

/// 把编码输出的 `CMSampleBuffer`（AVCC）转成 Annex B，关键帧前置参数集。
fn build_output(codec: CodecKind, sample: &CMSampleBuffer) -> MacVideoResult<EncodedOutput> {
    let data_buffer = unsafe { sample.data_buffer() }
        .ok_or_else(|| MacVideoError::Encode("输出样本没有 CMBlockBuffer".to_owned()))?;
    let length = unsafe { data_buffer.data_length() };
    if length == 0 {
        return Err(MacVideoError::Encode("输出样本为空".to_owned()));
    }
    let mut avcc = vec![0u8; length];
    let destination = NonNull::new(avcc.as_mut_ptr().cast::<c_void>())
        .ok_or_else(|| MacVideoError::Encode("输出缓冲为空指针".to_owned()))?;
    let status = unsafe { data_buffer.copy_data_bytes(0, length, destination) };
    if status != 0 {
        return Err(MacVideoError::os_status(
            "CMBlockBufferCopyDataBytes",
            status,
        ));
    }
    let mut data = annxb_convert(&avcc)?;
    let keyframe = annexb::has_keyframe(codec, &data);
    if keyframe {
        let mut sets = Vec::new();
        if let Some(description) = unsafe { sample.format_description() } {
            sets = parameter_sets_from_description(codec, &description);
        }
        // 兜底：编码器若把参数集内联在关键帧码流里（少见），从码流自身提取，
        // 否则控制器会因为「没有 SPS/PPS」建不出解码格式描述。
        if sets.is_empty() {
            sets = annexb::parameter_set_blob(codec, &data);
        }
        if !sets.is_empty() {
            data = annexb::prepend_parameter_sets(&sets, &data);
        }
    }
    Ok(EncodedOutput { data, keyframe })
}

/// AVCC → Annex B（薄包装，保持回调里的错误信息带上下文）。
fn annxb_convert(avcc: &[u8]) -> MacVideoResult<Vec<u8>> {
    annexb::avcc_to_annex_b(avcc).map_err(|err| MacVideoError::Encode(err.to_string()))
}

/// 从格式描述里提取参数集，拼成带 4 字节起始码的 Annex B 串。
fn parameter_sets_from_description(
    codec: CodecKind,
    description: &objc2_core_media::CMFormatDescription,
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut count = 0usize;
    // 先问一次总数（索引 0 只是为了拿 count_out）。
    let status = unsafe {
        parameter_set_at_index(
            codec,
            description,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            &raw mut count,
            ptr::null_mut(),
        )
    };
    if status != 0 || count == 0 {
        debug!(
            codec = codec.label(),
            status, "读取参数集总数失败，关键帧不带内联参数集"
        );
        return out;
    }
    for index in 0..count {
        let mut pointer: *const u8 = ptr::null();
        let mut size = 0usize;
        let status = unsafe {
            parameter_set_at_index(
                codec,
                description,
                index,
                &raw mut pointer,
                &raw mut size,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if status != 0 || pointer.is_null() || size == 0 {
            continue;
        }
        // SAFETY: 指针指向格式描述内部的参数集 NAL，长度由 VT 给出；只在本函数内拷贝。
        let nal = unsafe { std::slice::from_raw_parts(pointer, size) };
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
    }
    out
}

/// 按编码标准调用对应的 `CMVideoFormatDescriptionGet*ParameterSetAtIndex`。
///
/// # Safety
/// 输出指针要么为空、要么指向可写的 `usize` / 指针变量（与 CoreMedia 的约定一致）。
unsafe fn parameter_set_at_index(
    codec: CodecKind,
    description: &objc2_core_media::CMFormatDescription,
    index: usize,
    pointer_out: *mut *const u8,
    size_out: *mut usize,
    count_out: *mut usize,
    header_length_out: *mut std::ffi::c_int,
) -> i32 {
    if codec.is_hevc() {
        unsafe {
            objc2_core_media::CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                description,
                index,
                pointer_out,
                size_out,
                count_out,
                header_length_out,
            )
        }
    } else {
        unsafe {
            objc2_core_media::CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                description,
                index,
                pointer_out,
                size_out,
                count_out,
                header_length_out,
            )
        }
    }
}

/// 是否可用该编码标准创建编码会话（真实创建一次 64×64 会话）。
#[must_use]
pub fn probe(codec: CodecKind) -> bool {
    match VtEncoder::new(EncodeConfig::new(codec, 64, 64, 800, 30)) {
        Ok(_) => true,
        Err(err) => {
            info!(codec = codec.label(), error = %err, "VideoToolbox 编码器不可用");
            false
        }
    }
}

/// 组装编码器规格字典（启用硬件、不 Require）。
fn encoder_specification() -> MacVideoResult<CFRetained<CFDictionary>> {
    let enabled: &CFType = CFBoolean::new(true);
    cf_dictionary(&[(
        unsafe { kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder },
        enabled,
    )])
}

/// 组装像素缓冲属性（32BGRA + IOSurface + BT.709 颜色三件套）。
fn pixel_buffer_attributes(width: u32, height: u32) -> MacVideoResult<CFRetained<CFDictionary>> {
    let format = CFNumber::new_i32(
        i32::try_from(kCVPixelFormatType_32BGRA)
            .map_err(|_| MacVideoError::Config("像素格式码超出 i32".to_owned()))?,
    );
    let width_value = CFNumber::new_i32(
        i32::try_from(width).map_err(|_| MacVideoError::Config("宽度超出 i32".to_owned()))?,
    );
    let height_value = CFNumber::new_i32(
        i32::try_from(height).map_err(|_| MacVideoError::Config("高度超出 i32".to_owned()))?,
    );
    let surface_properties = cf_dictionary(&[])?;
    let format: &CFType = &format;
    let width_value: &CFType = &width_value;
    let height_value: &CFType = &height_value;
    let surface_properties: &CFType = &surface_properties;
    let primaries: &CFType = unsafe { kCVImageBufferColorPrimaries_ITU_R_709_2 };
    let transfer: &CFType = unsafe { kCVImageBufferTransferFunction_ITU_R_709_2 };
    let matrix: &CFType = unsafe { kCVImageBufferYCbCrMatrix_ITU_R_709_2 };
    cf_dictionary(&[
        (unsafe { kCVPixelBufferPixelFormatTypeKey }, format),
        (unsafe { kCVPixelBufferWidthKey }, width_value),
        (unsafe { kCVPixelBufferHeightKey }, height_value),
        (
            unsafe { kCVPixelBufferIOSurfacePropertiesKey },
            surface_properties,
        ),
        (unsafe { kCVImageBufferColorPrimariesKey }, primaries),
        (unsafe { kCVImageBufferTransferFunctionKey }, transfer),
        (unsafe { kCVImageBufferYCbCrMatrixKey }, matrix),
    ])
}

/// 「强制关键帧」的帧属性字典。
fn force_keyframe_dictionary() -> MacVideoResult<CFRetained<CFDictionary>> {
    let yes: &CFType = CFBoolean::new(true);
    cf_dictionary(&[(unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame }, yes)])
}

/// 设置会话的必需属性（失败即创建失败）。
fn apply_properties(session: &VTCompressionSession, config: &EncodeConfig) -> MacVideoResult<()> {
    let real_time = set_session_bool(session, unsafe { kVTCompressionPropertyKey_RealTime }, true);
    if real_time != 0 {
        return Err(MacVideoError::os_status("设置 RealTime", real_time));
    }
    let no_reorder = set_session_bool(
        session,
        unsafe { kVTCompressionPropertyKey_AllowFrameReordering },
        false,
    );
    if no_reorder != 0 {
        return Err(MacVideoError::os_status(
            "设置 AllowFrameReordering",
            no_reorder,
        ));
    }
    // 极大值 ≈ 永不自动插 IDR；关键帧由引擎显式请求。
    let interval = set_session_i32(
        session,
        unsafe { kVTCompressionPropertyKey_MaxKeyFrameInterval },
        1_000_000,
    );
    if interval != 0 {
        return Err(MacVideoError::os_status(
            "设置 MaxKeyFrameInterval",
            interval,
        ));
    }
    let bitrate = set_session_i64(
        session,
        unsafe { kVTCompressionPropertyKey_AverageBitRate },
        i64::from(config.bitrate_kbps).saturating_mul(1000),
    );
    if bitrate != 0 {
        return Err(MacVideoError::os_status("设置 AverageBitRate", bitrate));
    }
    let fps = set_session_i32(
        session,
        unsafe { kVTCompressionPropertyKey_ExpectedFrameRate },
        i32::try_from(config.fps.max(1)).unwrap_or(1),
    );
    if fps != 0 {
        return Err(MacVideoError::os_status("设置 ExpectedFrameRate", fps));
    }
    let max_qp = set_session_i32(
        session,
        unsafe { kVTCompressionPropertyKey_MaxAllowedFrameQP },
        i32::try_from(config.max_qp).unwrap_or(20),
    );
    if max_qp != 0 {
        // 不是所有编码器都支持 QP 上限；失败只意味着画质调优项缺失，不影响编码。
        debug!(status = max_qp, "编码器不支持 MaxAllowedFrameQP");
    }
    let profile = if config.codec.is_hevc() {
        unsafe { kVTProfileLevel_HEVC_Main_AutoLevel }
    } else {
        unsafe { kVTProfileLevel_H264_Main_AutoLevel }
    };
    let status = {
        let value: &CFType = profile;
        let session_type: &CFType = session;
        unsafe {
            objc2_video_toolbox::VTSessionSetProperty(
                session_type,
                kVTCompressionPropertyKey_ProfileLevel,
                Some(value),
            )
        }
    };
    if status != 0 {
        debug!(status, "编码器不接受 ProfileLevel，使用默认档位");
    }
    // 颜色三件套同时在**会话**上声明（像素缓冲属性只描述输入；有些编码器不会把输入
    // 附件透传到码流）。目标是 BT.709 limited，与控制端 wgpu 着色器、Windows NV12 一致。
    let session_type: &CFType = session;
    let primaries: &CFType = unsafe { kCVImageBufferColorPrimaries_ITU_R_709_2 };
    let transfer: &CFType = unsafe { kCVImageBufferTransferFunction_ITU_R_709_2 };
    let matrix: &CFType = unsafe { kCVImageBufferYCbCrMatrix_ITU_R_709_2 };
    for (key, value) in [
        (
            unsafe { kVTCompressionPropertyKey_ColorPrimaries },
            primaries,
        ),
        (
            unsafe { kVTCompressionPropertyKey_TransferFunction },
            transfer,
        ),
        (unsafe { kVTCompressionPropertyKey_YCbCrMatrix }, matrix),
    ] {
        // SAFETY: 会话有效、key/value 都是视频工具箱文档定义的静态常量。
        let code =
            unsafe { objc2_video_toolbox::VTSessionSetProperty(session_type, key, Some(value)) };
        if code != 0 {
            debug!(status = code, "编码器不接受颜色属性，沿用默认色彩标记");
        }
    }
    Ok(())
}

/// `VTSessionSetProperty`（布尔值）。
fn set_session_bool(session: &VTCompressionSession, key: &CFString, value: bool) -> i32 {
    let boolean = CFBoolean::new(value);
    let value: &CFType = boolean;
    let session: &CFType = session;
    unsafe { objc2_video_toolbox::VTSessionSetProperty(session, key, Some(value)) }
}

/// `VTSessionSetProperty`（i32 值）。
fn set_session_i32(session: &VTCompressionSession, key: &CFString, value: i32) -> i32 {
    let number = CFNumber::new_i32(value);
    let value: &CFType = &number;
    let session: &CFType = session;
    unsafe { objc2_video_toolbox::VTSessionSetProperty(session, key, Some(value)) }
}

/// `VTSessionSetProperty`（i64 值）。
fn set_session_i64(session: &VTCompressionSession, key: &CFString, value: i64) -> i32 {
    let number = CFNumber::new_i64(value);
    let value: &CFType = &number;
    let session: &CFType = session;
    unsafe { objc2_video_toolbox::VTSessionSetProperty(session, key, Some(value)) }
}

/// 读取会话的布尔属性（不支持/类型不符返回 `None`）。
pub(crate) fn read_session_bool(session: &CFType, key: &CFString) -> Option<bool> {
    let mut value: *mut c_void = ptr::null_mut();
    let status = unsafe {
        objc2_video_toolbox::VTSessionCopyProperty(
            session,
            key,
            None,
            ptr::from_mut(&mut value).cast::<c_void>(),
        )
    };
    if status != 0 || value.is_null() {
        return None;
    }
    // SAFETY: VTSessionCopyProperty 返回的是 +1 的 CFType 引用。
    let retained = unsafe { CFRetained::from_raw(NonNull::new(value.cast::<CFType>())?) };
    let boolean = retained.downcast::<CFBoolean>().ok()?;
    Some(boolean.as_bool())
}

/// 用 `CFType` 键值对建一个不可变字典（调用方保证键值数量一致）。
pub(crate) fn cf_dictionary(
    pairs: &[(&CFString, &CFType)],
) -> MacVideoResult<CFRetained<CFDictionary>> {
    let count = isize::try_from(pairs.len())
        .map_err(|_| MacVideoError::Config("字典条目过多".to_owned()))?;
    let mut keys: Vec<*const c_void> = pairs
        .iter()
        .map(|(key, _)| {
            let key: *const CFString = *key;
            key.cast::<c_void>()
        })
        .collect();
    let mut values: Vec<*const c_void> = pairs
        .iter()
        .map(|(_, value)| {
            let value: *const CFType = *value;
            value.cast::<c_void>()
        })
        .collect();
    let dictionary = unsafe {
        CFDictionary::new(
            None,
            keys.as_mut_ptr(),
            values.as_mut_ptr(),
            count,
            &raw const kCFTypeDictionaryKeyCallBacks,
            &raw const kCFTypeDictionaryValueCallBacks,
        )
    };
    dictionary.ok_or_else(|| MacVideoError::Encode("CFDictionaryCreate 失败".to_owned()))
}
