//! Android NDK **MediaCodec** 裸 FFI 绑定与解码引擎（仅 `target_os = "android"` 编译）。
//!
//! 本模块手写 `extern "C"` 绑定（不引入 bindgen），直接链接系统库 `libmediandk.so`。
//! 只用 **API 21** 起就存在的符号，确保 `--platform 21` 的最低设备也能跑。
//!
//! # 输出通路：字节缓冲（ByteBuffer），不是 Surface
//!
//! 引擎把解码结果读回 CPU 的**紧包 NV12**（[`rust_tunnel_remote::screen::DecodedNv12`]），
//! 因此 `configure` 时 **surface 传 null**（字节缓冲输出模式）。
//!
//! # 关于 `AMediaCodec_getOutputImage`
//!
//! NDK 的 `NdkMediaCodec.h` **没有**声明 `AMediaCodec_getOutputImage`（本机 NDK r29
//! 的 `libmediandk.so` 桩里也没有该符号），所以无法链接该函数。输出一律走
//! `AMediaCodec_getOutputBuffer` + 输出格式里的 `color-format` 解释平面布局：
//! `21`（NV12）/ `0x7F420888`（Flexible，按 NV12 处理）/ `19`（I420，交织成 NV12）。
//!
//! # 线程
//!
//! `AMediaCodec` 句柄不是 `Send`；本类型用 [`unsafe impl Send`] 包装，约定由控制器的
//! **单个** `spawn_blocking` 解码任务独占持有（`VideoDecoderApi` 只要求 `Send`，不要求
//! `Sync`），与 `win-video` 的 MFT 解码器同一套约定。

use crate::nal::{self, Syntax};
use crate::nv12::{self, ColorFormat};
use rust_tunnel_remote::codec::DecodeTimings;
use rust_tunnel_remote::screen::{DecodedNv12, DecoderInfo};
use rust_tunnel_remote::{DecodedVideoFrame, RemoteError, RemoteResult};
use std::ffi::{c_char, c_int, c_void};
use std::ptr;
use std::time::Instant;

/// `AMEDIA_OK`。
const AMEDIA_OK: c_int = 0;
/// `AMEDIACODEC_INFO_OUTPUT_BUFFERS_CHANGED`。
const INFO_OUTPUT_BUFFERS_CHANGED: isize = -3;
/// `AMEDIACODEC_INFO_OUTPUT_FORMAT_CHANGED`。
const INFO_OUTPUT_FORMAT_CHANGED: isize = -2;
/// `AMEDIACODEC_INFO_TRY_AGAIN_LATER`。
const INFO_TRY_AGAIN_LATER: isize = -1;
/// `AMEDIACODEC_BUFFER_FLAG_CODEC_CONFIG`。
const BUFFER_FLAG_CODEC_CONFIG: u32 = 2;
/// `AMEDIACODEC_BUFFER_FLAG_END_OF_STREAM`。
const BUFFER_FLAG_END_OF_STREAM: u32 = 4;

/// 单次 `dequeue*` 的等待上限（微秒）。
const DEQUEUE_TIMEOUT_US: i64 = 10_000;
/// 输入缓冲不可用时最多重试的次数（每次等待 [`DEQUEUE_TIMEOUT_US`]）。
const MAX_INPUT_ATTEMPTS: u32 = 10;
/// 名义帧间隔（微秒，60 fps）；只用于 `pts` 单调递增，不影响解码正确性。
const FRAME_INTERVAL_US: u64 = 16_667;
/// `max-input-size`：4 MiB，覆盖 4K IDR（单帧预算约 2 MiB）。
const MAX_INPUT_SIZE: i32 = 4 * 1024 * 1024;

/// `AMediaFormat` 键名（NUL 结尾，直接作为 `*const c_char`）。
const KEY_MIME: &[u8] = b"mime\0";
/// `width` 键。
const KEY_WIDTH: &[u8] = b"width\0";
/// `height` 键。
const KEY_HEIGHT: &[u8] = b"height\0";
/// `color-format` 键。
const KEY_COLOR_FORMAT: &[u8] = b"color-format\0";
/// `max-input-size` 键。
const KEY_MAX_INPUT_SIZE: &[u8] = b"max-input-size\0";
/// `low-latency` 键（尽力设置，未必被支持）。
const KEY_LOW_LATENCY: &[u8] = b"low-latency\0";
/// `csd-0` 键（参数集）。
const KEY_CSD0: &[u8] = b"csd-0\0";
/// `stride` 键（输出格式里的行步长）。
const KEY_STRIDE: &[u8] = b"stride\0";
/// `slice-height` 键（输出格式里的切片高度）。
const KEY_SLICE_HEIGHT: &[u8] = b"slice-height\0";

/// 不透明句柄：`AMediaCodec`。
#[repr(C)]
struct AMediaCodec {
    _private: [u8; 0],
}

/// 不透明句柄：`AMediaFormat`。
#[repr(C)]
struct AMediaFormat {
    _private: [u8; 0],
}

/// 不透明句柄：`ANativeWindow`（本层恒传 null）。
#[repr(C)]
struct ANativeWindow {
    _private: [u8; 0],
}

/// 不透明句柄：`AMediaCrypto`（本层恒传 null）。
#[repr(C)]
struct AMediaCrypto {
    _private: [u8; 0],
}

/// `AMediaCodecBufferInfo` 的 C 布局。
#[repr(C)]
#[derive(Default)]
struct AMediaCodecBufferInfo {
    /// 有效数据在输出缓冲里的字节偏移（非负）。
    offset: i32,
    /// 有效数据字节数。
    size: i32,
    /// 展示时间戳（微秒）。
    #[allow(
        dead_code,
        reason = "FFI 布局字段：MediaCodec 写入它，本层只按顺序读帧、不依赖时间戳"
    )]
    presentation_time_us: i64,
    /// `AMEDIACODEC_BUFFER_FLAG_*` 位标志。
    flags: u32,
}

#[link(name = "mediandk")]
extern "C" {
    fn AMediaCodec_createDecoderByType(mime: *const c_char) -> *mut AMediaCodec;
    fn AMediaCodec_delete(codec: *mut AMediaCodec) -> c_int;
    fn AMediaCodec_configure(
        codec: *mut AMediaCodec,
        format: *const AMediaFormat,
        surface: *mut ANativeWindow,
        crypto: *mut AMediaCrypto,
        flags: u32,
    ) -> c_int;
    fn AMediaCodec_start(codec: *mut AMediaCodec) -> c_int;
    fn AMediaCodec_stop(codec: *mut AMediaCodec) -> c_int;
    fn AMediaCodec_getInputBuffer(
        codec: *mut AMediaCodec,
        index: usize,
        out_size: *mut usize,
    ) -> *mut u8;
    fn AMediaCodec_getOutputBuffer(
        codec: *mut AMediaCodec,
        index: usize,
        out_size: *mut usize,
    ) -> *mut u8;
    fn AMediaCodec_dequeueInputBuffer(codec: *mut AMediaCodec, timeout_us: i64) -> isize;
    fn AMediaCodec_queueInputBuffer(
        codec: *mut AMediaCodec,
        index: usize,
        offset: isize,
        size: usize,
        time: u64,
        flags: u32,
    ) -> c_int;
    fn AMediaCodec_dequeueOutputBuffer(
        codec: *mut AMediaCodec,
        info: *mut AMediaCodecBufferInfo,
        timeout_us: i64,
    ) -> isize;
    fn AMediaCodec_getOutputFormat(codec: *mut AMediaCodec) -> *mut AMediaFormat;
    fn AMediaCodec_releaseOutputBuffer(
        codec: *mut AMediaCodec,
        index: usize,
        render: bool,
    ) -> c_int;
    fn AMediaFormat_new() -> *mut AMediaFormat;
    fn AMediaFormat_delete(format: *mut AMediaFormat) -> c_int;
    fn AMediaFormat_setInt32(format: *mut AMediaFormat, name: *const c_char, value: i32);
    fn AMediaFormat_setString(format: *mut AMediaFormat, name: *const c_char, value: *const c_char);
    fn AMediaFormat_setBuffer(
        format: *mut AMediaFormat,
        name: *const c_char,
        data: *const c_void,
        size: usize,
    );
    fn AMediaFormat_getInt32(format: *mut AMediaFormat, name: *const c_char, out: *mut i32)
        -> bool;
}

/// 探测某语法是否有可用的**硬件**解码器（真的能建出解码器才算）。
#[must_use]
pub(crate) fn probe_available(syntax: Syntax) -> bool {
    let mime = syntax.mime();
    // SAFETY: mime 是 NUL 结尾的静态字节串，满足 createDecoderByType 的 C 字符串契约。
    let codec = unsafe { AMediaCodec_createDecoderByType(mime.as_ptr().cast()) };
    if codec.is_null() {
        return false;
    }
    // SAFETY: codec 由 createDecoderByType 成功返回且未 start，可直接 delete。
    unsafe { AMediaCodec_delete(codec) };
    true
}

/// 微秒级耗时（饱和到 `u64::MAX`）。
fn elapsed_us(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
}

/// 在输入缓冲里取一个可用索引；持续不可用时返回错误。
fn dequeue_input(codec: *mut AMediaCodec) -> RemoteResult<usize> {
    for _ in 0..MAX_INPUT_ATTEMPTS {
        // SAFETY: codec 有效且已 start。
        let index = unsafe { AMediaCodec_dequeueInputBuffer(codec, DEQUEUE_TIMEOUT_US) };
        if index >= 0 {
            return usize::try_from(index)
                .map_err(|_| RemoteError::Encode("MediaCodec 输入缓冲索引非法".to_owned()));
        }
    }
    Err(RemoteError::Encode(
        "MediaCodec 输入缓冲持续不可用".to_owned(),
    ))
}

/// MediaCodec 解码引擎（惰性创建：`configure` 记尺寸，首次 `decode` 才建解码器）。
pub(crate) struct MediaCodecEngine {
    /// 编码语法。
    syntax: Syntax,
    /// 解码器句柄；null 表示尚未创建或已释放。
    codec: *mut AMediaCodec,
    /// 解码器是否已 `start`（`Drop`/`teardown` 据此决定是否 `stop`）。
    started: bool,
    /// `configure` 记录的编码尺寸。
    size: Option<(u32, u32)>,
    /// 当前输出帧宽度（**逻辑可见像素**，来自 `configure`，补丁/坐标以它为准）。
    fmt_width: usize,
    /// 当前输出帧高度（**逻辑可见像素**，来自 `configure`）。
    fmt_height: usize,
    /// 输出格式上报的缓冲宽度（宏块对齐后可能 >= 逻辑宽度；读平面时用）。
    buf_width: usize,
    /// 输出格式上报的缓冲高度（H.264 常见 1080→1088；读平面/UV 基址时用）。
    buf_height: usize,
    /// 当前输出色度布局。
    color: ColorFormat,
    /// 输出行步长（缺省回退到紧凑宽度）。
    row_stride: Option<usize>,
    /// 输出切片高度（缺省回退到紧凑高度）。
    slice_height: Option<usize>,
    /// 下一个输入缓冲的展示时间戳（单调递增）。
    next_pts_us: u64,
    /// 上次 `decode` 的子阶段耗时。
    timings: DecodeTimings,
}

// SAFETY: MediaCodec 句柄不是线程安全的。本类型由控制器单个解码任务独占持有
// （`VideoDecoderApi: Send`，不要求 `Sync`），构造后不会跨线程共享，符合该约定。
unsafe impl Send for MediaCodecEngine {}

impl MediaCodecEngine {
    /// 构造一个尚未创建解码器实例的引擎。
    pub(crate) fn new(syntax: Syntax) -> Self {
        Self {
            syntax,
            codec: ptr::null_mut(),
            started: false,
            size: None,
            fmt_width: 0,
            fmt_height: 0,
            buf_width: 0,
            buf_height: 0,
            color: ColorFormat::Flexible,
            row_stride: None,
            slice_height: None,
            next_pts_us: 0,
            timings: DecodeTimings::default(),
        }
    }

    /// 记录即将解码的编码尺寸；尺寸变化会释放旧实例（下次 `decode` 重建）。
    ///
    /// # Errors
    /// 尺寸为 0 或为奇数（4:2:0 要求偶数）时返回错误。
    pub(crate) fn configure(&mut self, width: u32, height: u32) -> RemoteResult<()> {
        if width == 0 || height == 0 {
            return Err(RemoteError::Encode(format!(
                "{} 解码尺寸非法: {width}x{height}",
                self.syntax.label()
            )));
        }
        if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(RemoteError::Encode(format!(
                "{} 解码尺寸必须为偶数: {width}x{height}",
                self.syntax.label()
            )));
        }
        if self.size == Some((width, height)) && !self.codec.is_null() {
            return Ok(());
        }
        self.teardown();
        self.size = Some((width, height));
        self.fmt_width = usize::try_from(width).unwrap_or(0);
        self.fmt_height = usize::try_from(height).unwrap_or(0);
        // 输出格式尚未上报时先按紧凑尺寸处理；`refresh_format` 会覆盖。
        self.buf_width = self.fmt_width;
        self.buf_height = self.fmt_height;
        Ok(())
    }

    /// 解码一批长度前缀拼接的码流，返回**最后**一帧；无输出时 `Ok(None)`。
    ///
    /// # Errors
    /// 尚未 `configure`、码流非法、或 MediaCodec 处理失败时返回错误。
    pub(crate) fn decode(&mut self, data: &[u8]) -> RemoteResult<Option<DecodedVideoFrame>> {
        self.timings = DecodeTimings::default();
        if data.is_empty() {
            return Ok(None);
        }
        let (width, height) = self.size.ok_or_else(|| {
            RemoteError::Encode(format!("{} 解码器尚未 configure", self.syntax.label()))
        })?;
        let units = nal::access_units(data).map_err(|err| {
            RemoteError::Encode(format!("{} 码流解析失败: {err}", self.syntax.label()))
        })?;
        let mut last = None;
        for unit in &units {
            if self.codec.is_null() {
                // 只有首帧确实带参数集时才填 `csd-0`；否则交给 IDR 内联参数集。
                let csd = if nal::has_parameter_sets(unit, self.syntax) {
                    nal::parameter_sets(unit, self.syntax)
                } else {
                    Vec::new()
                };
                self.create_codec(width, height, &csd)?;
            }
            self.submit(unit)?;
            if let Some(frame) = self.drain()? {
                last = Some(frame);
            }
        }
        Ok(last)
    }

    /// 本解码器的遥测信息。
    #[must_use]
    pub(crate) fn info(&self) -> DecoderInfo {
        DecoderInfo {
            backend: "MediaCodec".to_owned(),
            name: format!("{} (MediaCodec)", self.syntax.label()),
            adapter: String::new(),
            hardware: true,
        }
    }

    /// 上次 `decode` 的子阶段耗时。
    #[must_use]
    pub(crate) fn timings(&self) -> DecodeTimings {
        self.timings
    }

    /// 创建并 configure+start 一个解码器；首帧的参数集可选填进 `csd-0`。
    fn create_codec(&mut self, width: u32, height: u32, csd: &[u8]) -> RemoteResult<()> {
        let mime = self.syntax.mime();
        // SAFETY: mime 是 NUL 结尾的静态字节串。
        let codec = unsafe { AMediaCodec_createDecoderByType(mime.as_ptr().cast()) };
        if codec.is_null() {
            return Err(RemoteError::Encode(format!(
                "无法创建 MediaCodec {} 解码器",
                self.syntax.label()
            )));
        }
        match configure_and_start(codec, self.syntax, width, height, csd) {
            Ok(()) => {
                self.codec = codec;
                self.started = true;
                self.fmt_width = usize::try_from(width).unwrap_or(0);
                self.fmt_height = usize::try_from(height).unwrap_or(0);
                self.color = ColorFormat::Flexible;
                self.row_stride = None;
                self.slice_height = None;
                self.next_pts_us = 0;
                Ok(())
            }
            Err(err) => {
                // SAFETY: codec 由上面的 createDecoderByType 成功返回，未 start。
                unsafe { AMediaCodec_delete(codec) };
                Err(err)
            }
        }
    }

    /// 把一个 Annex B 访问单元送进解码器。
    fn submit(&mut self, unit: &[u8]) -> RemoteResult<()> {
        let codec = self.codec;
        let index = dequeue_input(codec)?;
        let mut capacity = 0usize;
        // SAFETY: codec 有效且已 start；index 来自 dequeueInputBuffer。
        let buffer = unsafe { AMediaCodec_getInputBuffer(codec, index, &mut capacity) };
        if buffer.is_null() {
            return Err(RemoteError::Encode("MediaCodec 返回空输入缓冲".to_owned()));
        }
        if unit.len() > capacity {
            return Err(RemoteError::Encode(format!(
                "访问单元 {} 字节超过输入缓冲容量 {capacity} 字节",
                unit.len()
            )));
        }
        let started = Instant::now();
        // SAFETY: buffer 至少可写 capacity 字节；unit 与之不重叠且 unit.len() <= capacity。
        unsafe { ptr::copy_nonoverlapping(unit.as_ptr(), buffer, unit.len()) };
        self.timings.submit_us = self.timings.submit_us.saturating_add(elapsed_us(started));
        let pts = self.next_pts_us;
        self.next_pts_us = self.next_pts_us.saturating_add(FRAME_INTERVAL_US);
        // SAFETY: index 来自 dequeueInputBuffer；offset 0、size 已校验。
        let status = unsafe { AMediaCodec_queueInputBuffer(codec, index, 0, unit.len(), pts, 0) };
        if status != AMEDIA_OK {
            return Err(RemoteError::Encode(format!(
                "MediaCodec queueInputBuffer 失败：status={status}"
            )));
        }
        Ok(())
    }

    /// 排空当前可用的输出帧，返回最后一张。
    fn drain(&mut self) -> RemoteResult<Option<DecodedVideoFrame>> {
        let codec = self.codec;
        let output_started = Instant::now();
        let mut last = None;
        loop {
            let mut info = AMediaCodecBufferInfo::default();
            // SAFETY: codec 有效；info 指向本栈帧。
            let index =
                unsafe { AMediaCodec_dequeueOutputBuffer(codec, &mut info, DEQUEUE_TIMEOUT_US) };
            if index == INFO_TRY_AGAIN_LATER {
                break;
            }
            if index == INFO_OUTPUT_FORMAT_CHANGED {
                self.refresh_format();
                continue;
            }
            if index == INFO_OUTPUT_BUFFERS_CHANGED {
                continue;
            }
            if index < 0 {
                self.timings.output_us = self
                    .timings
                    .output_us
                    .saturating_add(elapsed_us(output_started));
                return Err(RemoteError::Encode(format!(
                    "MediaCodec dequeueOutputBuffer 返回异常状态 {index}"
                )));
            }
            let frame_index = usize::try_from(index)
                .map_err(|_| RemoteError::Encode("MediaCodec 输出缓冲索引非法".to_owned()))?;
            let frame = self.take_output(frame_index, &info);
            // SAFETY: frame_index 来自 dequeueOutputBuffer；render=false（不上屏）。
            unsafe { AMediaCodec_releaseOutputBuffer(codec, frame_index, false) };
            match frame {
                Ok(Some(frame)) => last = Some(frame),
                Ok(None) => {}
                Err(err) => {
                    self.timings.output_us = self
                        .timings
                        .output_us
                        .saturating_add(elapsed_us(output_started));
                    return Err(err);
                }
            }
            if info.flags & BUFFER_FLAG_END_OF_STREAM != 0 {
                break;
            }
        }
        self.timings.output_us = self
            .timings
            .output_us
            .saturating_add(elapsed_us(output_started));
        Ok(last)
    }

    /// 把一张输出缓冲读成紧包 NV12（在 `releaseOutputBuffer` 之前调用）。
    fn take_output(
        &mut self,
        index: usize,
        info: &AMediaCodecBufferInfo,
    ) -> RemoteResult<Option<DecodedVideoFrame>> {
        if info.flags & BUFFER_FLAG_CODEC_CONFIG != 0 || info.size <= 0 {
            return Ok(None);
        }
        let offset = usize::try_from(info.offset)
            .map_err(|_| RemoteError::Encode("MediaCodec 输出缓冲偏移为负".to_owned()))?;
        let size = usize::try_from(info.size)
            .map_err(|_| RemoteError::Encode("MediaCodec 输出缓冲长度非法".to_owned()))?;
        let mut capacity = 0usize;
        // SAFETY: codec 有效；index 来自 dequeueOutputBuffer。
        let buffer = unsafe { AMediaCodec_getOutputBuffer(self.codec, index, &mut capacity) };
        if buffer.is_null() {
            return Err(RemoteError::Encode("MediaCodec 返回空输出缓冲".to_owned()));
        }
        let end = offset
            .checked_add(size)
            .ok_or_else(|| RemoteError::Encode("MediaCodec 输出缓冲偏移溢出".to_owned()))?;
        if end > capacity {
            return Err(RemoteError::Encode(format!(
                "MediaCodec 输出缓冲越界：offset={offset} size={size} capacity={capacity}"
            )));
        }
        // SAFETY: 已确认 [offset, end) 落在 capacity 内，缓冲在 release 前保持有效。
        let data = unsafe { std::slice::from_raw_parts(buffer.add(offset).cast_const(), size) };
        // 逻辑尺寸（configure）决定输出帧与补丁坐标；缓冲尺寸（输出格式）只用于
        // 读平面。H.264 的 1080 常被解码成 1088（宏块对齐），若把 1088 当成输出高度，
        // 补丁会因「尺寸不符」被整批丢弃、画面永久缺块。
        let geometry = nv12::plane_geometry(
            self.fmt_width,
            self.fmt_height,
            self.buf_width,
            self.buf_height,
            self.row_stride,
            self.slice_height,
        )
        .map_err(|reason| {
            RemoteError::Encode(format!(
                "MediaCodec {} 几何非法（可见 {}x{}，缓冲 {}x{}）: {reason}",
                self.syntax.label(),
                self.fmt_width,
                self.fmt_height,
                self.buf_width,
                self.buf_height
            ))
        })?;
        let color_started = Instant::now();
        let packed = match self.color {
            ColorFormat::Flexible | ColorFormat::SemiPlanar => nv12::pack_nv12(data, geometry),
            ColorFormat::Planar => nv12::pack_i420(data, geometry),
        }
        .map_err(|reason| {
            RemoteError::Encode(format!(
                "MediaCodec {} {} 打包失败: {reason}",
                self.syntax.label(),
                self.color.label()
            ))
        })?;
        self.timings.color_us = self
            .timings
            .color_us
            .saturating_add(elapsed_us(color_started));
        let width = u32::try_from(self.fmt_width)
            .map_err(|_| RemoteError::Encode("解码帧宽超过 u32 上限".to_owned()))?;
        let height = u32::try_from(self.fmt_height)
            .map_err(|_| RemoteError::Encode("解码帧高超过 u32 上限".to_owned()))?;
        Ok(Some(DecodedVideoFrame {
            width,
            height,
            rgba: Vec::new(),
            nv12: Some(DecodedNv12 {
                y: packed.y,
                uv: packed.uv,
            }),
        }))
    }

    /// 读取 `INFO_OUTPUT_FORMAT_CHANGED` 后的实际宽高/色度格式/步长。
    ///
    /// 读不到就沿用旧值（不致命）。
    fn refresh_format(&mut self) {
        // SAFETY: codec 有效。
        let fmt = unsafe { AMediaCodec_getOutputFormat(self.codec) };
        if fmt.is_null() {
            return;
        }
        let mut width = 0i32;
        let mut height = 0i32;
        let mut color = 0i32;
        let mut stride = 0i32;
        let mut slice = 0i32;
        // SAFETY: fmt 有效；各 out 指针指向本栈帧的 i32。
        let has_width =
            unsafe { AMediaFormat_getInt32(fmt, KEY_WIDTH.as_ptr().cast(), &mut width) };
        let has_height =
            unsafe { AMediaFormat_getInt32(fmt, KEY_HEIGHT.as_ptr().cast(), &mut height) };
        let has_color =
            unsafe { AMediaFormat_getInt32(fmt, KEY_COLOR_FORMAT.as_ptr().cast(), &mut color) };
        let has_stride =
            unsafe { AMediaFormat_getInt32(fmt, KEY_STRIDE.as_ptr().cast(), &mut stride) };
        let has_slice =
            unsafe { AMediaFormat_getInt32(fmt, KEY_SLICE_HEIGHT.as_ptr().cast(), &mut slice) };
        // SAFETY: fmt 由 getOutputFormat 返回，必须释放。
        unsafe { AMediaFormat_delete(fmt) };
        // 只更新**缓冲**尺寸，逻辑尺寸保持 `configure` 的值：解码器上报的
        // width/height 可能是宏块对齐后的（1080→1088），不能让它污染输出帧与补丁坐标。
        if has_width && width > 0 {
            if let Ok(value) = usize::try_from(width) {
                self.buf_width = value;
            }
        }
        if has_height && height > 0 {
            if let Ok(value) = usize::try_from(height) {
                self.buf_height = value;
            }
        }
        if has_color {
            if let Some(format) = ColorFormat::from_i32(color) {
                self.color = format;
            }
        }
        if has_stride && stride > 0 {
            self.row_stride = usize::try_from(stride).ok();
        }
        if has_slice && slice > 0 {
            self.slice_height = usize::try_from(slice).ok();
        }
    }

    /// 释放解码器实例（幂等）。
    fn teardown(&mut self) {
        if !self.codec.is_null() {
            // SAFETY: codec 由本结构独占；started 反映是否已 start。
            unsafe {
                if self.started {
                    AMediaCodec_stop(self.codec);
                }
                AMediaCodec_delete(self.codec);
            }
            self.codec = ptr::null_mut();
        }
        self.started = false;
    }
}

impl Drop for MediaCodecEngine {
    fn drop(&mut self) {
        self.teardown();
    }
}

/// configure + start 一个新建的解码器（失败时由调用方 delete）。
fn configure_and_start(
    codec: *mut AMediaCodec,
    syntax: Syntax,
    width: u32,
    height: u32,
    csd: &[u8],
) -> RemoteResult<()> {
    let width_i = i32::try_from(width)
        .map_err(|_| RemoteError::Encode("解码宽度超过 i32 上限".to_owned()))?;
    let height_i = i32::try_from(height)
        .map_err(|_| RemoteError::Encode("解码高度超过 i32 上限".to_owned()))?;
    // SAFETY: AMediaFormat_new 返回新句柄或 null。
    let fmt = unsafe { AMediaFormat_new() };
    if fmt.is_null() {
        return Err(RemoteError::Encode(
            "AMediaFormat_new 返回空句柄".to_owned(),
        ));
    }
    let mime = syntax.mime();
    // SAFETY: fmt 有效；所有键名都是 NUL 结尾的静态字节串；csd 在调用期间有效。
    unsafe {
        AMediaFormat_setString(fmt, KEY_MIME.as_ptr().cast(), mime.as_ptr().cast());
        AMediaFormat_setInt32(fmt, KEY_WIDTH.as_ptr().cast(), width_i);
        AMediaFormat_setInt32(fmt, KEY_HEIGHT.as_ptr().cast(), height_i);
        AMediaFormat_setInt32(
            fmt,
            KEY_COLOR_FORMAT.as_ptr().cast(),
            nv12::COLOR_FORMAT_FLEXIBLE,
        );
        AMediaFormat_setInt32(fmt, KEY_MAX_INPUT_SIZE.as_ptr().cast(), MAX_INPUT_SIZE);
        AMediaFormat_setInt32(fmt, KEY_LOW_LATENCY.as_ptr().cast(), 1);
        if !csd.is_empty() {
            AMediaFormat_setBuffer(
                fmt,
                KEY_CSD0.as_ptr().cast(),
                csd.as_ptr().cast(),
                csd.len(),
            );
        }
    }
    // SAFETY: codec 有效；surface/crypto 传 null 表示字节缓冲输出、不解密。
    let status = unsafe { AMediaCodec_configure(codec, fmt, ptr::null_mut(), ptr::null_mut(), 0) };
    // SAFETY: fmt 不再使用，必须释放。
    unsafe { AMediaFormat_delete(fmt) };
    if status != AMEDIA_OK {
        return Err(RemoteError::Encode(format!(
            "MediaCodec {} configure 失败：status={status}",
            syntax.label()
        )));
    }
    // SAFETY: codec 已成功 configure。
    let started = unsafe { AMediaCodec_start(codec) };
    if started != AMEDIA_OK {
        return Err(RemoteError::Encode(format!(
            "MediaCodec {} start 失败：status={started}",
            syntax.label()
        )));
    }
    Ok(())
}
