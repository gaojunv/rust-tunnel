//! VideoToolbox **硬件解码**（H.264 / H.265 → 紧包 NV12，BT.709 limited）。
//!
//! # 工作方式
//!
//! 输入是 Annex B 码流。解码器维护最近一次见到的 SPS/PPS(/VPS)：
//!
//! 1. 见到（新的）参数集 → 用 `CMVideoFormatDescriptionCreateFromH264ParameterSets`
//!    （或 `...HEVCParameterSets`）重建格式描述，并**丢弃旧解码会话**（参数集变了，
//!    旧会话继续用会解出错帧）；
//! 2. 把非参数集 NAL 转成 **AVCC**（4 字节长度前缀），组 `CMBlockBuffer` +
//!    `CMSampleBuffer`，交给 `VTDecompressionSession::decode_frame`；
//! 3. 回调拿到 `CVPixelBuffer`，按 `bytesPerRow` 拷成紧包 NV12。
//!
//! 输出格式通过 `kCVPixelBufferPixelFormatTypeKey = 420v` 指定；VT 有时会给 `420f`，
//! 两者都接受（都是 BiPlanar NV12，只是范围标签不同；本 crate 按 BT.709 limited 解释，
//! 与编码侧及控制器着色器一致）。
//!
//! # 线程
//!
//! 解码器由**单个任务独占持有**：本类型 `Send` 但非 `Sync`。

use crate::annexb;
use crate::codec::CodecKind;
use crate::error::{MacVideoError, MacVideoResult};
use crate::vt_encoder::{cf_dictionary, read_session_bool, EncodeConfig, VtEncoder};
use objc2_core_foundation::{
    kCFAllocatorDefault, CFBoolean, CFDictionary, CFNumber, CFRetained, CFType,
};
use objc2_core_media::{
    kCMBlockBufferAssureMemoryNowFlag, kCMTimeInvalid, CMBlockBuffer, CMFormatDescription,
    CMSampleBuffer, CMSampleTimingInfo, CMTime,
    CMVideoFormatDescriptionCreateFromH264ParameterSets,
    CMVideoFormatDescriptionCreateFromHEVCParameterSets,
};
use objc2_core_video::{
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferPixelFormatTypeKey,
    kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, CVImageBuffer, CVPixelBuffer,
    CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use objc2_video_toolbox::{
    kVTDecompressionPropertyKey_RealTime,
    kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder,
    kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder, VTDecodeFrameFlags,
    VTDecodeInfoFlags, VTDecompressionOutputCallbackRecord, VTDecompressionSession,
    VTIsHardwareDecodeSupported,
};
use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tracing::{debug, info};

/// 等待解码回调的上限。与编码同口径：单帧解码远超不过 2 s。
pub const DECODE_TIMEOUT: Duration = Duration::from_secs(2);

/// 解码出的一帧紧包 NV12（BT.709 limited）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedNv12Frame {
    /// 宽（像素）。
    pub width: u32,
    /// 高（像素）。
    pub height: u32,
    /// Y 平面（长度 = `width * height`）。
    pub y: Vec<u8>,
    /// 交织 UV 平面（长度 = `width * height / 2`）。
    pub uv: Vec<u8>,
}

/// 解码器自报信息（供上层映射成 remote 的 `DecoderInfo`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecoderInfo {
    /// 后端名（恒为 `"VideoToolbox"`）。
    pub backend: String,
    /// 解码器/编码标准名（例如 `"VT H.264"`）。
    pub name: String,
    /// 是否硬件解码。
    pub hardware: bool,
}

/// 回调与解码线程之间的交接槽。
#[derive(Debug)]
struct FrameSlot {
    /// 本次调用的结果；`None` 表示回调还没来。
    frame: Option<MacVideoResult<DecodedNv12Frame>>,
}

/// 解码回调共享状态。
struct SharedDecode {
    /// 交接槽。
    slot: Mutex<FrameSlot>,
    /// 条件变量。
    ready: Condvar,
}

/// VideoToolbox 硬件解码器。
pub struct VtDecoder {
    /// 解码会话（参数集/尺寸变化时重建）。
    session: Option<CFRetained<VTDecompressionSession>>,
    /// 当前格式描述。
    format: Option<CFRetained<CMFormatDescription>>,
    /// 当前格式描述对应的参数集载荷（用于判断「参数集是否变了」）。
    stored_sets: Vec<Vec<u8>>,
    /// 编码标准；`None` 表示尚未从码流识别出来。
    codec: Option<CodecKind>,
    /// 上次 `configure` 的尺寸。
    configured: Option<(u32, u32)>,
    /// 回调共享状态。
    shared: Arc<SharedDecode>,
    /// 是否硬件解码（建会话后读一次）。
    hardware: bool,
}

// SAFETY: 与编码器同理：解码会话句柄不是 `Send`，但引擎把它交给单个任务独占持有；
// 回调线程只访问 `Arc<SharedDecode>`（`Mutex` + `Condvar`）。
unsafe impl Send for VtDecoder {}

impl std::fmt::Debug for VtDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VtDecoder")
            .field("codec", &self.codec)
            .field("configured", &self.configured)
            .field("hardware", &self.hardware)
            .finish_non_exhaustive()
    }
}

impl Default for VtDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for VtDecoder {
    fn drop(&mut self) {
        if let Some(session) = &self.session {
            // SAFETY: 会话有效；invalidate 后 VT 不再回调 `shared`。字段按声明顺序析构，
            // `session` 声明在 `shared` 之前，因此 `Arc` 比会话活得久。
            unsafe { session.invalidate() };
        }
    }
}

impl VtDecoder {
    /// 创建一个编码标准待识别的解码器（首个参数集到来时自动判定 H.264 / H.265）。
    #[must_use]
    pub fn new() -> Self {
        Self::with_codec(None)
    }

    /// 创建一个已知编码标准的解码器（跳过自动识别）。
    #[must_use]
    pub fn with_codec(codec: Option<CodecKind>) -> Self {
        Self {
            session: None,
            format: None,
            stored_sets: Vec::new(),
            codec,
            configured: None,
            shared: Arc::new(SharedDecode {
                slot: Mutex::new(FrameSlot { frame: None }),
                ready: Condvar::new(),
            }),
            hardware: false,
        }
    }

    /// 告知即将解码的编码尺寸。
    ///
    /// VideoToolbox 从参数集自身就能得到尺寸；这个钩子只用于「尺寸变化 → 丢弃旧会话」
    /// 的显式失效。同一尺寸重复调用是空操作。
    ///
    /// # Errors
    /// 尺寸为零时返回 [`MacVideoError::Config`]。
    pub fn configure(&mut self, width: u32, height: u32) -> MacVideoResult<()> {
        if width == 0 || height == 0 {
            return Err(MacVideoError::Config(format!(
                "解码尺寸不能为 0（{width}x{height}）"
            )));
        }
        if self.configured == Some((width, height)) {
            return Ok(());
        }
        self.configured = Some((width, height));
        // 尺寸变了：旧会话的参考帧与格式描述都不再适用，等下一组参数集重建。
        self.invalidate_session();
        self.format = None;
        self.stored_sets.clear();
        Ok(())
    }

    /// 当前编码标准（尚未识别时为 `None`）。
    #[must_use]
    pub const fn codec(&self) -> Option<CodecKind> {
        self.codec
    }

    /// 是否硬件解码。
    #[must_use]
    pub const fn is_hardware(&self) -> bool {
        self.hardware
    }

    /// 解码器信息。
    #[must_use]
    pub fn info(&self) -> DecoderInfo {
        DecoderInfo {
            backend: "VideoToolbox".to_owned(),
            name: match self.codec {
                Some(codec) => format!("VT {}", codec.label()),
                None => "VT（编码标准未识别）".to_owned(),
            },
            hardware: self.hardware,
        }
    }

    /// 解码一批 Annex B 码流，返回**最后**一帧；无输出时为 `None`。
    ///
    /// # Errors
    /// 码流非法、参数集缺失（还没见过 SPS/PPS(/VPS)）、解码会话创建失败、解码回调
    /// 报错或超时时返回错误。
    pub fn decode(&mut self, data: &[u8]) -> MacVideoResult<Option<DecodedNv12Frame>> {
        if data.is_empty() {
            return Ok(None);
        }
        let units = annexb::nal_units(data);
        if units.is_empty() {
            return Err(MacVideoError::Decode(
                "码流里没有任何 Annex B NAL".to_owned(),
            ));
        }
        if self.codec.is_none() {
            self.codec = detect_codec(&units);
        }
        let codec = self.codec.ok_or_else(|| {
            MacVideoError::Decode("无法识别编码标准（还没见到参数集）".to_owned())
        })?;
        // 参数集变化 → 重建格式描述与会话。
        let incoming: Vec<&[u8]> = units
            .iter()
            .copied()
            .filter(|unit| {
                annexb::nal_type(codec, unit).is_some_and(|nal| codec.is_parameter_set(nal))
            })
            .collect();
        if !incoming.is_empty() && !sets_match(&self.stored_sets, &incoming) {
            let format = make_format_description(codec, &incoming)?;
            self.invalidate_session();
            self.format = Some(format);
            self.stored_sets = incoming.iter().map(|unit| unit.to_vec()).collect();
        }
        // 只把切片 NAL 写成 AVCC（参数集已在格式描述里）。
        let mut avcc = Vec::with_capacity(data.len());
        for unit in &units {
            if annexb::nal_type(codec, unit).is_some_and(|nal| codec.is_parameter_set(nal)) {
                continue;
            }
            let length = u32::try_from(unit.len())
                .map_err(|_| MacVideoError::Decode("NAL 超过 4 GiB".to_owned()))?;
            avcc.extend_from_slice(&length.to_be_bytes());
            avcc.extend_from_slice(unit);
        }
        if avcc.is_empty() {
            return Ok(None);
        }
        let format = self
            .format
            .as_ref()
            .ok_or_else(|| MacVideoError::Decode("尚未收到参数集，无法创建解码会话".to_owned()))?
            .clone();
        self.ensure_session(&format)?;
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| MacVideoError::Decode("解码会话缺失".to_owned()))?;
        let sample = make_sample_buffer(&format, &avcc)?;
        {
            let mut slot = self
                .shared
                .slot
                .lock()
                .map_err(|_| MacVideoError::Decode("解码交接槽锁已中毒".to_owned()))?;
            slot.frame = None;
        }
        let mut info_flags = VTDecodeInfoFlags(0);
        let status = unsafe {
            session.decode_frame(
                &sample,
                VTDecodeFrameFlags(0),
                ptr::null_mut(),
                &raw mut info_flags,
            )
        };
        if status != 0 {
            self.invalidate_session();
            return Err(MacVideoError::os_status(
                "VTDecompressionSessionDecodeFrame",
                status,
            ));
        }
        self.wait_for_frame()
    }

    /// 丢弃当前解码会话（参数集/尺寸变化或解码失败时调用）。
    fn invalidate_session(&mut self) {
        if let Some(session) = self.session.take() {
            // SAFETY: 会话有效；invalidate 之后不再回调。
            unsafe { session.invalidate() };
        }
        self.hardware = false;
    }

    /// 按需创建解码会话。
    fn ensure_session(&mut self, format: &CFRetained<CMFormatDescription>) -> MacVideoResult<()> {
        if self.session.is_some() {
            return Ok(());
        }
        let specification = decoder_specification()?;
        let attributes = decoder_output_attributes()?;
        let record = VTDecompressionOutputCallbackRecord {
            decompressionOutputCallback: Some(decompression_output_callback),
            decompressionOutputRefCon: Arc::as_ptr(&self.shared).cast_mut().cast::<c_void>(),
        };
        let format_ref: &CMFormatDescription = format;
        let mut session_ptr: *mut VTDecompressionSession = ptr::null_mut();
        let status = unsafe {
            VTDecompressionSession::create(
                None,
                format_ref,
                Some(&specification),
                Some(&attributes),
                &raw const record,
                NonNull::from(&mut session_ptr),
            )
        };
        if status != 0 {
            return Err(MacVideoError::os_status(
                "VTDecompressionSessionCreate",
                status,
            ));
        }
        let session = unsafe {
            CFRetained::from_raw(NonNull::new(session_ptr).ok_or_else(|| {
                MacVideoError::Decode("VTDecompressionSessionCreate 返回空会话".to_owned())
            })?)
        };
        // 低延迟：不要为省电而延迟输出。
        let real_time = {
            let session_type: &CFType = &session;
            let value: &CFType = CFBoolean::new(true);
            unsafe {
                objc2_video_toolbox::VTSessionSetProperty(
                    session_type,
                    kVTDecompressionPropertyKey_RealTime,
                    Some(value),
                )
            }
        };
        if real_time != 0 {
            debug!(status = real_time, "解码器不接受 RealTime 属性");
        }
        self.hardware = read_session_bool(&session, unsafe {
            kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder
        })
        .unwrap_or(false);
        info!(
            codec = self.codec.map_or("未知", CodecKind::label),
            hardware = self.hardware,
            "VideoToolbox 解码会话已创建"
        );
        self.session = Some(session);
        Ok(())
    }

    /// 等待回调把帧写进交接槽（最多 [`DECODE_TIMEOUT`]）。
    fn wait_for_frame(&mut self) -> MacVideoResult<Option<DecodedNv12Frame>> {
        let outcome = self.wait_for_frame_shared();
        if outcome.is_err() {
            self.invalidate_session();
        }
        outcome
    }

    /// [`Self::wait_for_frame`] 的只读半段：只碰 `self.shared`，便于在错误后由调用方
    /// 安全地失效会话（避免 `MutexGuard` 存活期间可变借用 `self`）。
    fn wait_for_frame_shared(&self) -> MacVideoResult<Option<DecodedNv12Frame>> {
        let deadline = Instant::now() + DECODE_TIMEOUT;
        let mut slot = self
            .shared
            .slot
            .lock()
            .map_err(|_| MacVideoError::Decode("解码交接槽锁已中毒".to_owned()))?;
        loop {
            if let Some(result) = slot.frame.take() {
                return result.map(Some);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(MacVideoError::Decode(format!(
                    "VideoToolbox 解码超时（>{DECODE_TIMEOUT:?}）"
                )));
            }
            let remaining = deadline.saturating_duration_since(now);
            let (guard, _) = self
                .shared
                .ready
                .wait_timeout(slot, remaining)
                .map_err(|_| MacVideoError::Decode("解码等待锁已中毒".to_owned()))?;
            slot = guard;
        }
    }
}

/// 从首个码流里的参数集识别编码标准。
fn detect_codec(units: &[&[u8]]) -> Option<CodecKind> {
    let mut h264 = false;
    let mut hevc = false;
    for unit in units {
        let Some(&first) = unit.first() else {
            continue;
        };
        let h264_type = first & 0x1F;
        let hevc_type = (first >> 1) & 0x3F;
        if h264_type == 7 || h264_type == 8 {
            h264 = true;
        }
        if (32..=34).contains(&hevc_type) {
            hevc = true;
        }
    }
    match (h264, hevc) {
        (true, false) => Some(CodecKind::H264),
        // 同时命中时优先 HEVC：HEVC 的 VPS/SPS/PPS（0x40/0x42/0x44）是独有的，
        // 而 H.264 的 SPS 首字节 0x67/0x68 按 HEVC 位布局不是参数集。
        (_, true) => Some(CodecKind::H265),
        (false, false) => None,
    }
}

/// 参数集载荷是否与已存的一致。
fn sets_match(stored: &[Vec<u8>], incoming: &[&[u8]]) -> bool {
    stored.len() == incoming.len()
        && stored
            .iter()
            .zip(incoming.iter())
            .all(|(before, now)| before.as_slice() == *now)
}

/// 用参数集建格式描述。
fn make_format_description(
    codec: CodecKind,
    sets: &[&[u8]],
) -> MacVideoResult<CFRetained<CMFormatDescription>> {
    if sets.len() < codec.parameter_set_count() {
        return Err(MacVideoError::Decode(format!(
            "{} 参数集不完整：需要 {} 段，收到 {} 段",
            codec.label(),
            codec.parameter_set_count(),
            sets.len()
        )));
    }
    let mut owned: Vec<Vec<u8>> = sets.iter().map(|set| set.to_vec()).collect();
    let mut pointers: Vec<NonNull<u8>> = owned
        .iter_mut()
        .map(|set| NonNull::new(set.as_mut_ptr()))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| MacVideoError::Decode("参数集指针为空".to_owned()))?;
    let mut sizes: Vec<usize> = owned.iter().map(Vec::len).collect();
    let count = pointers.len();
    let pointer_array = NonNull::new(pointers.as_mut_ptr())
        .ok_or_else(|| MacVideoError::Decode("参数集指针数组为空".to_owned()))?;
    let size_array = NonNull::new(sizes.as_mut_ptr())
        .ok_or_else(|| MacVideoError::Decode("参数集长度数组为空".to_owned()))?;
    let mut format_ptr: *const CMFormatDescription = ptr::null();
    let out = NonNull::from(&mut format_ptr);
    let status = if codec.is_hevc() {
        unsafe {
            CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                None,
                count,
                pointer_array,
                size_array,
                4,
                None,
                out,
            )
        }
    } else {
        unsafe {
            CMVideoFormatDescriptionCreateFromH264ParameterSets(
                None,
                count,
                pointer_array,
                size_array,
                4,
                out,
            )
        }
    };
    if status != 0 {
        return Err(MacVideoError::os_status(
            "CMVideoFormatDescriptionCreateFromParameterSets",
            status,
        ));
    }
    let format = unsafe {
        CFRetained::from_raw(
            NonNull::new(format_ptr.cast_mut())
                .ok_or_else(|| MacVideoError::Decode("格式描述创建返回空指针".to_owned()))?,
        )
    };
    Ok(format)
}

/// 把 AVCC 数据包成 `CMBlockBuffer`。
fn make_block_buffer(data: &[u8]) -> MacVideoResult<CFRetained<CMBlockBuffer>> {
    if data.is_empty() {
        return Err(MacVideoError::Decode("空 AVCC 样本".to_owned()));
    }
    let mut block_ptr: *mut CMBlockBuffer = ptr::null_mut();
    let status = unsafe {
        CMBlockBuffer::create_with_memory_block(
            None,
            ptr::null_mut(),
            data.len(),
            kCFAllocatorDefault,
            ptr::null(),
            0,
            data.len(),
            kCMBlockBufferAssureMemoryNowFlag,
            NonNull::from(&mut block_ptr),
        )
    };
    if status != 0 {
        return Err(MacVideoError::os_status(
            "CMBlockBufferCreateWithMemoryBlock",
            status,
        ));
    }
    let block = unsafe {
        CFRetained::from_raw(NonNull::new(block_ptr).ok_or_else(|| {
            MacVideoError::Decode("CMBlockBufferCreateWithMemoryBlock 返回空缓冲".to_owned())
        })?)
    };
    let source = NonNull::new(data.as_ptr().cast_mut().cast::<c_void>())
        .ok_or_else(|| MacVideoError::Decode("AVCC 源指针为空".to_owned()))?;
    let status = unsafe { CMBlockBuffer::replace_data_bytes(source, &block, 0, data.len()) };
    if status != 0 {
        return Err(MacVideoError::os_status(
            "CMBlockBufferReplaceDataBytes",
            status,
        ));
    }
    Ok(block)
}

/// 把 AVCC 数据包成 `CMSampleBuffer`（一个样本 = 一帧）。
fn make_sample_buffer(
    format: &CMFormatDescription,
    avcc: &[u8],
) -> MacVideoResult<CFRetained<CMSampleBuffer>> {
    let block = make_block_buffer(avcc)?;
    let timing = CMSampleTimingInfo {
        duration: unsafe { CMTime::new(1, 30) },
        presentationTimeStamp: unsafe { CMTime::new(0, 30) },
        decodeTimeStamp: unsafe { kCMTimeInvalid },
    };
    let size: usize = avcc.len();
    let mut sample_ptr: *mut CMSampleBuffer = ptr::null_mut();
    let status = unsafe {
        CMSampleBuffer::create_ready(
            None,
            Some(&block),
            Some(format),
            1,
            1,
            &raw const timing,
            1,
            &raw const size,
            NonNull::from(&mut sample_ptr),
        )
    };
    if status != 0 {
        return Err(MacVideoError::os_status(
            "CMSampleBufferCreateReady",
            status,
        ));
    }
    unsafe {
        Ok::<_, MacVideoError>(CFRetained::from_raw(NonNull::new(sample_ptr).ok_or_else(
            || MacVideoError::Decode("CMSampleBufferCreateReady 返回空样本".to_owned()),
        )?))
    }
}

/// VideoToolbox 解码输出回调。
///
/// # Safety
/// `ref_con` 必须是 [`VtDecoder`] 创建会话时传入的 `Arc<SharedDecode>` 裸指针，且会话
/// 在 `Arc` 释放前被 invalidate（由 [`Drop for VtDecoder`] 与
/// [`VtDecoder::invalidate_session`] 保证）。
unsafe extern "C-unwind" fn decompression_output_callback(
    ref_con: *mut c_void,
    _source_frame_ref_con: *mut c_void,
    status: i32,
    info_flags: VTDecodeInfoFlags,
    image_buffer: *mut CVImageBuffer,
    _presentation_time: CMTime,
    _duration: CMTime,
) {
    if ref_con.is_null() {
        return;
    }
    // SAFETY: 由 `VtDecoder` 保证 ref_con 指向活着的 `Arc<SharedDecode>`。
    let shared = unsafe { &*ref_con.cast::<SharedDecode>() };
    let result = if status != 0 {
        Err(MacVideoError::os_status("VideoToolbox 解码回调", status))
    } else if info_flags.contains(VTDecodeInfoFlags::FrameDropped) {
        Err(MacVideoError::Decode("解码器丢弃了该帧".to_owned()))
    } else if image_buffer.is_null() {
        Err(MacVideoError::Decode("解码回调没有输出图像".to_owned()))
    } else {
        // SAFETY: VT 在回调期间持有图像缓冲；这里只借用并立即拷贝。
        copy_nv12(unsafe { &*image_buffer })
    };
    if let Ok(mut slot) = shared.slot.lock() {
        slot.frame = Some(result);
    }
    shared.ready.notify_all();
}

/// 把 `CVPixelBuffer` 拷成紧包 NV12。
fn copy_nv12(pixel: &CVPixelBuffer) -> MacVideoResult<DecodedNv12Frame> {
    let format = CVPixelBufferGetPixelFormatType(pixel);
    if format != kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
        && format != kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
    {
        return Err(MacVideoError::Decode(format!(
            "解码输出不是 BiPlanar NV12（格式 0x{format:08X}）"
        )));
    }
    let lock_status = unsafe { CVPixelBufferLockBaseAddress(pixel, CVPixelBufferLockFlags(0)) };
    if lock_status != 0 {
        return Err(MacVideoError::os_status(
            "CVPixelBufferLockBaseAddress",
            lock_status,
        ));
    }
    let result = copy_nv12_locked(pixel);
    let unlock_status = unsafe { CVPixelBufferUnlockBaseAddress(pixel, CVPixelBufferLockFlags(0)) };
    if unlock_status != 0 {
        debug!(
            status = unlock_status,
            "CVPixelBufferUnlockBaseAddress 失败"
        );
    }
    result
}

/// 在已加锁的像素缓冲上拷出 Y/UV 平面。
fn copy_nv12_locked(pixel: &CVPixelBuffer) -> MacVideoResult<DecodedNv12Frame> {
    let width = CVPixelBufferGetWidth(pixel);
    let height = CVPixelBufferGetHeight(pixel);
    if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(MacVideoError::Decode(format!(
            "解码输出尺寸不是偶数：{width}x{height}"
        )));
    }
    let y_base = CVPixelBufferGetBaseAddressOfPlane(pixel, 0).cast::<u8>();
    let uv_base = CVPixelBufferGetBaseAddressOfPlane(pixel, 1).cast::<u8>();
    let y_pitch = CVPixelBufferGetBytesPerRowOfPlane(pixel, 0);
    let uv_pitch = CVPixelBufferGetBytesPerRowOfPlane(pixel, 1);
    if y_base.is_null() || uv_base.is_null() || y_pitch < width || uv_pitch < width {
        return Err(MacVideoError::Decode("解码输出平面不可读".to_owned()));
    }
    let mut y = vec![0u8; width * height];
    for row in 0..height {
        let source = unsafe { y_base.add(row * y_pitch) };
        let destination = &mut y[row * width..(row + 1) * width];
        unsafe { ptr::copy_nonoverlapping(source, destination.as_mut_ptr(), width) };
    }
    let uv_rows = height / 2;
    let mut uv = vec![0u8; width * uv_rows];
    for row in 0..uv_rows {
        let source = unsafe { uv_base.add(row * uv_pitch) };
        let destination = &mut uv[row * width..(row + 1) * width];
        unsafe { ptr::copy_nonoverlapping(source, destination.as_mut_ptr(), width) };
    }
    Ok(DecodedNv12Frame {
        width: u32::try_from(width).unwrap_or(0),
        height: u32::try_from(height).unwrap_or(0),
        y,
        uv,
    })
}

/// 解码器规格字典（启用硬件解码，不 Require）。
fn decoder_specification() -> MacVideoResult<CFRetained<CFDictionary>> {
    let enabled: &CFType = CFBoolean::new(true);
    cf_dictionary(&[(
        unsafe { kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder },
        enabled,
    )])
}

/// 解码输出属性（要求 BiPlanar NV12，并显式启用 IOSurface 以走硬件解码）。
fn decoder_output_attributes() -> MacVideoResult<CFRetained<CFDictionary>> {
    let format = CFNumber::new_i32(
        i32::try_from(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange)
            .map_err(|_| MacVideoError::Config("像素格式码超出 i32".to_owned()))?,
    );
    let surface = cf_dictionary(&[])?;
    let format: &CFType = &format;
    let surface: &CFType = &surface;
    cf_dictionary(&[
        (unsafe { kCVPixelBufferPixelFormatTypeKey }, format),
        (unsafe { kCVPixelBufferIOSurfacePropertiesKey }, surface),
    ])
}

/// 本机是否有该编码标准的解码能力（真实往返一次；失败时回退到硬件能力查询）。
///
/// **创建式探测**：只有真正编码一帧、建解码会话并解出画面才算可用。编码器不可用
/// （例如老 Intel Mac 没有 HEVC 硬编，且系统也没给软编）时退回
/// [`VTIsHardwareDecodeSupported`]——它只回答硬件解码，因此这一路是保守的。
#[must_use]
pub fn probe(codec: CodecKind) -> bool {
    match probe_roundtrip(codec) {
        Ok(()) => true,
        Err(err) => {
            let hardware = unsafe { VTIsHardwareDecodeSupported(codec.codec_type()) };
            info!(
                codec = codec.label(),
                error = %err,
                hardware,
                "VideoToolbox 解码往返探测失败，回退到硬件解码能力查询"
            );
            hardware
        }
    }
}

/// 编一帧再解一帧的往返探测。
///
/// # Errors
/// 编码器创建/编码失败、解码器创建失败、解不出画面时返回错误。
fn probe_roundtrip(codec: CodecKind) -> MacVideoResult<()> {
    let (width, height) = (1280u32, 720u32);
    let mut encoder = VtEncoder::new(EncodeConfig::new(codec, width, height, 4000, 30))?;
    let mut frame = vec![0u8; (width * height) as usize * 4];
    for y in 0..height {
        for x in 0..width {
            let offset = ((y * width + x) as usize) * 4;
            frame[offset] = u8::try_from(x % 256).unwrap_or(0);
            frame[offset + 1] = u8::try_from(y % 256).unwrap_or(0);
            frame[offset + 2] = u8::try_from((x / 4) % 256).unwrap_or(0);
            frame[offset + 3] = 255;
        }
    }
    let packet = encoder.encode_bgra(&frame, width, height, true)?;
    let mut decoder = VtDecoder::with_codec(Some(codec));
    decoder.configure(width, height)?;
    let frame_out = decoder
        .decode(&packet.data)?
        .ok_or_else(|| MacVideoError::Decode("解码探针没有输出帧".to_owned()))?;
    if frame_out.width != width || frame_out.height != height {
        return Err(MacVideoError::Decode(format!(
            "解码探针尺寸不符：{}x{}（期望 {width}x{height}）",
            frame_out.width, frame_out.height
        )));
    }
    Ok(())
}
