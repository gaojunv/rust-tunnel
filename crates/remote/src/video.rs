//! VP9 4:4:4 编解码封装 + RGBA↔YUV444 转换（远程控制数据面）。
//!
//! # 为什么是 VP9 profile 1
//!
//! 现状 JPEG 本来就是 4:4:4（`image` 0.25 的 `JpegEncoder` 不做色度下采样），所以退回 4:2:0
//! 是相对现状的**退化**。8-bit 4:4:4 在 VP9 里是 profile 1——上游 `shiguredo_libvpx` 表达不了
//! 它，故本仓库 vendor 了该 crate 并打了补丁（见 `vendor/shiguredo_libvpx/src/lib.rs` 的
//! `PATCH:` 注释与 `crates/remote/tests/vendor_patches.rs` 的机械锁）。
//!
//! # 色彩：BT.709 + full range，两端共用同一份常数
//!
//! 编码器显式设 `VP9E_SET_COLOR_SPACE = BT.709` / `VP9E_SET_COLOR_RANGE = Full`（补丁 5），
//! 本模块的转换矩阵与之**严格一致**。这是「画面发灰 / 颜色偏掉」的经典事故点：不设时码流恒为
//! `CS_UNKNOWN(0)` + limited range，第三方解码器只能靠猜。改动其中任何一侧都必须同时改另一侧。
//!
//! # 关键参数（实测依据见计划「阶段 0 实测结论」）
//!
//! - `lag_in_frames = Some(1)`：**必设**。`None` 只是「不覆盖」，libvpx 默认 `g_lag_in_frames
//!   = 25` 会把前 25 帧全缓冲住、一帧都不吐。
//! - `cpu_used = 8`：4K 下唯一能落进 40 ms 预算的设置；本模块默认编码宽度 ≤
//!   [`MAX_ENCODE_WIDTH`]（1080p 级），8 仍有充分余量。
//! - `rc_buffer_size_ms ≈ 300`：默认 6000 ms 下 CBR 在秒级尺度几乎不控码率（补丁 4）。
//! - `keyframe_interval` 只作**兜底**：实测 libvpx 在 `VPX_KF_AUTO` + realtime + CBR 下
//!   **不会**按场景切换自动插 IDR（250 帧逐帧整屏反色只在 `kf_max_dist` 的整数倍处出关键帧），
//!   故无需 `kf_min_dist` 补丁。IDR 完全由上层显式驱动（首帧 / 配置变更 / `RequestKeyframe`）。
//! - **静止画面完全不喂编码器**：上层用 memcmp 与上一帧比对，相同则连转换都不做（带宽趋零的
//!   唯一来源）。

use crate::error::{RemoteError, RemoteResult};
use crate::screen::scaled_dimension;
use shiguredo_libvpx::{
    CodecConfig, ColorRange, ColorSpace, Decoder, DecoderCodec, DecoderConfig,
    EncodeOptions as VpxEncodeOptions, Encoder, EncoderConfig, EncodingDeadline, ImageData, ImageFormat,
    RateControlMode, Vp9Config, Vp9Profile,
};
use std::num::NonZeroUsize;

/// 默认编码宽度上限（像素）。
///
/// 4K 解码单线程实测 37–58 ms，与现状 JPEG 的 53 ms 同档，收益不在单帧开销；故**默认编码宽度
/// ≤ 1920**，4K 只能作为显式选项。1080p 编码在 `cpu_used = 8` 下远快于 40 ms 预算。
pub const MAX_ENCODE_WIDTH: u32 = 1920;

/// 单帧声明/解码尺寸的合理性上限（像素，对宽、高各自生效）。
///
/// 这不是「能显示多大」的限制，而是给**对端单方声明**的尺寸兜底：真实单台显示器最大为 8K
/// （7680x4320），16384（≈2 倍 8K 宽、≈4 倍 8K 高）已远超任何真实画面，同时把 `width*height*4`
/// 的最坏情形压在 1 GiB 量级。数据面已由一次性 token + AEAD 认证（见 [`crate::crypto`]），
/// 本层是**纵深防御**：令牌泄漏、被控端被攻陷或实现跑飞时也不该让一条 ≤16 MiB 的消息把控制器
/// 打爆。
pub const MAX_FRAME_DIM: u32 = 16384;

/// 关键帧兜底间隔（帧数）。约合 12 fps 下 25 秒；正常路径由显式 `RequestKeyframe` 驱动。
const KEYFRAME_INTERVAL: usize = 300;

/// CBR 的码率控制缓冲（毫秒）。交互级——见模块文档。
const RC_BUFFER_MS: usize = 300;

/// libvpx 编码速度档（VP9 0–9，越大越快）。4K 达标的唯一设置。
const CPU_USED: usize = 8;

/// 每像素字节数（RGBA）。
const BYTES_PER_PIXEL: usize = 4;

// ── BT.709 full-range 系数（与 `color_space = Bt709` / `color_range = Full` 严格一致）──
/// 红分量权重（BT.709）。
const KR: f32 = 0.2126;
/// 蓝分量权重（BT.709）。
const KB: f32 = 0.0722;
/// 绿分量权重（由 `1 - KR - KB` 推出，避免手写常量与二者不一致）。
const KG: f32 = 1.0 - KR - KB;
/// `Cb = (B - Y) * CB_SCALE + 128`（BT.709）。
const CB_SCALE: f32 = 1.0 / (2.0 * (1.0 - KB));
/// `Cr = (R - Y) * CR_SCALE + 128`（BT.709）。
const CR_SCALE: f32 = 1.0 / (2.0 * (1.0 - KR));

/// 编译期断言：`Encoder` / `Decoder` 是 `Send` 但**不是** `Sync`（阶段 0 实测结论）。
///
/// 上层把编码器/解码器**独占**移进 `spawn_blocking`（采集与编码是阻塞调用），因此只需要 `Send`。
/// 这条断言把这个前提钉在编译期：上游哪天把它变成 `!Send`，这里立刻编译失败，而不是等到
/// `spawn_blocking` 处才发现。
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<Encoder>();
    assert_send::<Decoder>();
};

/// 一个非零 `usize`（0 回退到 1，仅用于编译期常量，不会真的走到回退）。
fn nonzero(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).unwrap_or(NonZeroUsize::MIN)
}

/// 编码线程数 = 逻辑核数（`available_parallelism` 取不到时回退 1）。
fn encode_threads() -> NonZeroUsize {
    std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN)
}

/// 按缩放比例与宽度上限计算**编码尺寸**。
///
/// 先按 `scale` 缩放，再按 `max_width` 封顶（保持宽高比），最后对齐到偶数、下限 2 像素。
/// 偶数对齐让 stride 与尺寸推导更简单，也避开部分平台对奇数尺寸的边缘处理差异。
///
/// 边界（由 `encode_dimensions_*` 单测锁定）：正好 4K → 封顶到 1920；超限屏（5120 宽）→ 1920；
/// `scale = 0.5` → 先减半再看是否封顶；奇数尺寸 → 向下对齐到偶数。
#[must_use]
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "尺寸远小于 u32 上限，缩放比例为有界正浮点，结果为非负整数像素"
)]
pub fn encode_dimensions(width: u32, height: u32, scale: f32, max_width: u32) -> (u32, u32) {
    let mut w = scaled_dimension(width, scale).max(2);
    let mut h = scaled_dimension(height, scale).max(2);
    if w > max_width && max_width >= 2 {
        let ratio = f64::from(max_width) / f64::from(w);
        h = ((f64::from(h) * ratio).round() as u32).max(2);
        w = max_width;
    }
    (w & !1, h & !1)
}

/// 把 `0.0..=255.0` 的浮点钳到 `u8`。
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "已钳到 0..=255，取整后落在 u8 内"
)]
fn clamp_u8(value: f32) -> u8 {
    value.clamp(0.0, 255.0).round() as u8
}

/// RGBA 单像素 → BT.709 full-range 的 `(Y, Cb, Cr)`。
fn rgb_to_yuv(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    let (rf, gf, bf) = (f32::from(r), f32::from(g), f32::from(b));
    let y = KR.mul_add(rf, KG.mul_add(gf, KB * bf));
    let cb = 128.0 + (bf - y) * CB_SCALE;
    let cr = 128.0 + (rf - y) * CR_SCALE;
    (clamp_u8(y), clamp_u8(cb), clamp_u8(cr))
}

/// BT.709 full-range 的 `(Y, Cb, Cr)` → RGBA 单像素（alpha 恒 255）。
fn yuv_to_rgb(y: u8, cb: u8, cr: u8) -> (u8, u8, u8) {
    let yf = f32::from(y);
    let cbf = f32::from(cb) - 128.0;
    let crf = f32::from(cr) - 128.0;
    let r = yf + crf / CR_SCALE;
    let b = yf + cbf / CB_SCALE;
    let g = (yf - KR * r - KB * b) / KG;
    (clamp_u8(r), clamp_u8(g), clamp_u8(b))
}

/// 把一帧 RGBA 转成 I444 三平面（必要时同时降采样）。
///
/// **缩放与转换合并成一步**：先按比例缩到中间 RGBA 再转换会多一次整帧拷贝与一趟遍历，而
/// 本函数在降采样时按目标像素对源像素做盒式平均（只降不升，故盒式平均即正确的面积平均），
/// 一趟遍历完成。`src == dst` 时退化为逐像素直转。
#[expect(
    clippy::many_single_char_names,
    clippy::too_many_arguments,
    reason = "Y/U/V 与 R/G/B 是色彩领域的通用单字母记法，改名反而难读；参数成组传递是转换函数的固有形状"
)]
fn rgba_to_i444(
    rgba: &[u8],
    src_width: usize,
    src_height: usize,
    dst_width: usize,
    dst_height: usize,
    y: &mut Vec<u8>,
    u: &mut Vec<u8>,
    v: &mut Vec<u8>,
) -> RemoteResult<()> {
    let expected = src_width
        .checked_mul(src_height)
        .and_then(|n| n.checked_mul(BYTES_PER_PIXEL))
        .ok_or_else(|| RemoteError::Encode("帧尺寸溢出".to_owned()))?;
    if rgba.len() != expected {
        return Err(RemoteError::Encode(format!(
            "RGBA 长度 {} 与 {}x{} 不符（应为 {expected}）",
            rgba.len(),
            src_width,
            src_height
        )));
    }
    let pixels = dst_width
        .checked_mul(dst_height)
        .ok_or_else(|| RemoteError::Encode("编码尺寸溢出".to_owned()))?;
    y.clear();
    u.clear();
    v.clear();
    y.reserve(pixels);
    u.reserve(pixels);
    v.reserve(pixels);

    if src_width == dst_width && src_height == dst_height {
        for px in rgba.as_chunks::<BYTES_PER_PIXEL>().0 {
            let (yy, cb, cr) = rgb_to_yuv(px[0], px[1], px[2]);
            y.push(yy);
            u.push(cb);
            v.push(cr);
        }
        return Ok(());
    }

    for dy in 0..dst_height {
        // 目标行 `dy` 覆盖的源行区间 `[sy0, sy1)`；`sy1 > sy0` 保证非空。
        let sy0 = dy * src_height / dst_height;
        let sy1 = (((dy + 1) * src_height) / dst_height).max(sy0 + 1);
        for dx in 0..dst_width {
            let sx0 = dx * src_width / dst_width;
            let sx1 = (((dx + 1) * src_width) / dst_width).max(sx0 + 1);
            let (mut r, mut g, mut b) = (0u64, 0u64, 0u64);
            let mut n = 0u64;
            for sy in sy0..sy1 {
                let row = sy * src_width;
                for sx in sx0..sx1 {
                    let off = (row + sx) * BYTES_PER_PIXEL;
                    r += u64::from(rgba[off]);
                    g += u64::from(rgba[off + 1]);
                    b += u64::from(rgba[off + 2]);
                    n += 1;
                }
            }
            #[expect(
                clippy::cast_possible_truncation,
                reason = "均值落在 0..=255，取整后落在 u8 内"
            )]
            let (avg_r, avg_g, avg_b) = ((r / n) as u8, (g / n) as u8, (b / n) as u8);
            let (yy, cb, cr) = rgb_to_yuv(avg_r, avg_g, avg_b);
            y.push(yy);
            u.push(cb);
            v.push(cr);
        }
    }
    Ok(())
}

/// 把解码出的 I444 三平面转回 RGBA。
///
/// **必须按 stride 逐行取**：libvpx 的 stride 会 64 字节对齐（例如宽 320 时 stride 是 384），
/// 绝不能假设 `stride == 宽`。
#[expect(
    clippy::many_single_char_names,
    clippy::too_many_arguments,
    reason = "Y/U/V 是色彩领域的通用单字母记法；三平面各带 stride 是 libvpx 接口的固有形状"
)]
fn i444_to_rgba(
    y: &[u8],
    y_stride: usize,
    u: &[u8],
    u_stride: usize,
    v: &[u8],
    v_stride: usize,
    width: usize,
    height: usize,
) -> RemoteResult<Vec<u8>> {
    // 防御：stride 小于宽时下面的切片会越界 panic；非法布局一律拒绝。
    if y_stride < width || u_stride < width || v_stride < width {
        return Err(RemoteError::Encode(format!(
            "色度/亮度 stride 小于宽度（y={y_stride}, u={u_stride}, v={v_stride}, w={width}）"
        )));
    }
    let mut rgba = vec![0u8; width * height * BYTES_PER_PIXEL];
    for row in 0..height {
        let y_row = &y[row * y_stride..row * y_stride + width];
        let u_row = &u[row * u_stride..row * u_stride + width];
        let v_row = &v[row * v_stride..row * v_stride + width];
        for col in 0..width {
            let (r, g, b) = yuv_to_rgb(y_row[col], u_row[col], v_row[col]);
            let off = (row * width + col) * BYTES_PER_PIXEL;
            rgba[off] = r;
            rgba[off + 1] = g;
            rgba[off + 2] = b;
            rgba[off + 3] = 255;
        }
    }
    Ok(rgba)
}

/// 一次编码的产出：长度前缀拼接的一个或多个 VP9 帧。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedVideo {
    /// `u32 大端长度 + 帧字节` 重复拼接。`lag_in_frames = 1` 下通常恰含一帧。
    pub data: Vec<u8>,
    /// 本批是否含关键帧（IDR）。
    pub keyframe: bool,
}

impl EncodedVideo {
    /// 本批是否为空（编码器本轮无输出）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

/// VP9 4:4:4 编码器。
///
/// **`Send` 但非 `Sync`**：上层必须独占持有（移进 `spawn_blocking` 或由单个任务持有），
/// 不能共享引用。
pub struct VideoEncoder {
    inner: Encoder,
    /// 编码尺寸的宽。
    width: u32,
    /// 编码尺寸的高。
    height: u32,
    /// Y 平面暂存（复用，避免每帧分配）。
    y: Vec<u8>,
    /// U 平面暂存。
    u: Vec<u8>,
    /// V 平面暂存。
    v: Vec<u8>,
}

impl std::fmt::Debug for VideoEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoEncoder")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

impl VideoEncoder {
    /// 构造一个编码 `width × height` 的 VP9 profile 1 编码器。
    ///
    /// # Errors
    /// 尺寸为零或超出 [`MAX_FRAME_DIM`]、libvpx 初始化失败时返回 [`RemoteError::Encode`]。
    pub fn new(width: u32, height: u32, target_bitrate_kbps: u32, max_fps: u8) -> RemoteResult<Self> {
        if width == 0 || height == 0 || width > MAX_FRAME_DIM || height > MAX_FRAME_DIM {
            return Err(RemoteError::Encode(format!(
                "编码尺寸非法：{width}x{height}（上限 {MAX_FRAME_DIM}）"
            )));
        }
        let vp9 = Vp9Config {
            // 补丁 1：上游没有 Profile1，用 Profile0/2 + I444 会让 `encode()` 失败。
            profile: Vp9Profile::Profile1,
            // 补丁 5：显式声明 BT.709 + full range，与 `rgb_to_yuv` 的常数严格一致。
            color_space: Some(ColorSpace::Bt709),
            color_range: Some(ColorRange::Full),
            ..Default::default()
        };
        let mut config = EncoderConfig::new(
            usize::try_from(width).unwrap_or(usize::MAX),
            usize::try_from(height).unwrap_or(usize::MAX),
            ImageFormat::I444,
            CodecConfig::Vp9(vp9),
        );
        config.deadline = EncodingDeadline::Realtime;
        config.rate_control = RateControlMode::Cbr;
        config.target_bitrate = usize::try_from(target_bitrate_kbps)
            .unwrap_or(usize::MAX)
            .saturating_mul(1000);
        config.fps_numerator = usize::from(max_fps.max(1));
        config.fps_denominator = 1;
        config.cpu_used = Some(CPU_USED);
        config.threads = Some(encode_threads());
        // 必设：`None` 只是「不覆盖」，libvpx 默认会把前 25 帧全缓冲住。
        config.lag_in_frames = Some(NonZeroUsize::MIN);
        config.error_resilient = true;
        config.keyframe_interval = Some(nonzero(KEYFRAME_INTERVAL));
        config.rc_buffer_size_ms = Some(nonzero(RC_BUFFER_MS));

        let inner = Encoder::new(config)
            .map_err(|err| RemoteError::Encode(format!("VP9 编码器创建失败: {err}")))?;
        Ok(Self {
            inner,
            width,
            height,
            y: Vec::new(),
            u: Vec::new(),
            v: Vec::new(),
        })
    }

    /// 编码尺寸。
    #[must_use]
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// 当前生效的目标码率（kbps）。
    #[must_use]
    pub fn bitrate_kbps(&self) -> u32 {
        self.inner.bitrate_kbps()
    }

    /// 当前生效的帧率。
    #[must_use]
    pub fn fps(&self) -> u32 {
        self.inner.fps()
    }

    /// **在线**重配置目标码率（不重建编码器、不插 IDR）。
    ///
    /// QoS 闭环（AIMD）每秒动一次码率，走的就是这条路（vendor 补丁 6 → `vpx_codec_enc_config_set`）。
    /// **绝不能改成重建编码器**：重建会强制一个 IDR，闭环每秒一个关键帧，正好把阶段 2 消掉的
    /// 带宽尖峰引回来。
    ///
    /// # Errors
    /// libvpx 拒绝该配置时返回 [`RemoteError::Encode`]（调用方应丢弃编码器、下一帧重建）。
    pub fn set_bitrate(&mut self, kbps: u32) -> RemoteResult<()> {
        self.inner
            .set_bitrate(kbps)
            .map_err(|err| RemoteError::Encode(format!("在线改码率失败: {err}")))
    }

    /// **在线**重配置帧率（不重建编码器、不插 IDR）。同 [`VideoEncoder::set_bitrate`]。
    ///
    /// # Errors
    /// libvpx 拒绝该配置时返回 [`RemoteError::Encode`]。
    pub fn set_fps(&mut self, fps: u8) -> RemoteResult<()> {
        self.inner
            .set_fps(u32::from(fps))
            .map_err(|err| RemoteError::Encode(format!("在线改帧率失败: {err}")))
    }

    /// 把一帧 `src_width × src_height` 的 RGBA 编码成 VP9。
    ///
    /// `force_keyframe` 为真时本帧必为 IDR（会话首帧、配置变更、收到 `RequestKeyframe`）。
    ///
    /// # Errors
    /// 输入长度与源尺寸不符、libvpx 编码失败时返回 [`RemoteError::Encode`]。
    pub fn encode(
        &mut self,
        rgba: &[u8],
        src_width: u32,
        src_height: u32,
        force_keyframe: bool,
    ) -> RemoteResult<EncodedVideo> {
        rgba_to_i444(
            rgba,
            usize::try_from(src_width).unwrap_or(usize::MAX),
            usize::try_from(src_height).unwrap_or(usize::MAX),
            usize::try_from(self.width).unwrap_or(usize::MAX),
            usize::try_from(self.height).unwrap_or(usize::MAX),
            &mut self.y,
            &mut self.u,
            &mut self.v,
        )?;
        self.inner
            .encode(
                &ImageData::I444 {
                    y: &self.y,
                    u: &self.u,
                    v: &self.v,
                },
                &VpxEncodeOptions { force_keyframe },
            )
            .map_err(|err| RemoteError::Encode(format!("VP9 编码失败: {err}")))?;

        let mut data = Vec::new();
        let mut keyframe = false;
        while let Some(packet) = self.inner.next_frame() {
            let bytes = packet.data();
            let len = u32::try_from(bytes.len())
                .map_err(|_| RemoteError::Encode("单帧超过 u32 长度上限".to_owned()))?;
            data.extend_from_slice(&len.to_be_bytes());
            data.extend_from_slice(bytes);
            keyframe |= packet.is_keyframe();
        }
        Ok(EncodedVideo { data, keyframe })
    }
}

/// 一帧解码出的 RGBA 画面。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedVideoFrame {
    /// 宽（像素）。
    pub width: u32,
    /// 高（像素）。
    pub height: u32,
    /// RGBA8 像素数据，长度 = `width * height * 4`。
    pub rgba: Vec<u8>,
}

/// VP9 解码器。
///
/// **`Send` 但非 `Sync`**：由控制器侧的解码任务独占持有。
pub struct VideoDecoder {
    inner: Decoder,
}

impl std::fmt::Debug for VideoDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoDecoder").finish_non_exhaustive()
    }
}

impl VideoDecoder {
    /// 构造一个 VP9 解码器。
    ///
    /// # Errors
    /// libvpx 初始化失败时返回 [`RemoteError::Encode`]。
    pub fn new() -> RemoteResult<Self> {
        let inner = Decoder::new(DecoderConfig {
            codec: DecoderCodec::Vp9,
        })
        .map_err(|err| RemoteError::Encode(format!("VP9 解码器创建失败: {err}")))?;
        Ok(Self { inner })
    }

    /// 解码一批长度前缀拼接的 VP9 帧，返回**最后**一帧（无输出时为 `None`）。
    ///
    /// # Errors
    /// 长度前缀不完整、帧数据非法、libvpx 解码失败、解码尺寸非法时返回 [`RemoteError::Encode`]。
    pub fn decode(&mut self, data: &[u8]) -> RemoteResult<Option<DecodedVideoFrame>> {
        let mut last = None;
        let mut rest = data;
        while !rest.is_empty() {
            let Some((len_bytes, tail)) = rest.split_at_checked(4) else {
                return Err(RemoteError::Encode("VP9 帧长度前缀不完整".to_owned()));
            };
            let len = u32::from_be_bytes(
                len_bytes
                    .try_into()
                    .map_err(|_| RemoteError::Encode("VP9 帧长度前缀读取失败".to_owned()))?,
            );
            let len = usize::try_from(len)
                .map_err(|_| RemoteError::Encode("VP9 帧长度溢出".to_owned()))?;
            let Some((packet, next)) = tail.split_at_checked(len) else {
                return Err(RemoteError::Encode(format!(
                    "VP9 帧长度 {len} 超过剩余字节 {}",
                    tail.len()
                )));
            };
            rest = next;
            self.inner
                .decode(packet)
                .map_err(|err| RemoteError::Encode(format!("VP9 解码失败: {err}")))?;
            loop {
                let frame = self
                    .inner
                    .next_frame()
                    .map_err(|err| RemoteError::Encode(format!("VP9 取帧失败: {err}")))?;
                let Some(frame) = frame else { break };
                let width = frame.width();
                let height = frame.height();
                if width == 0
                    || height == 0
                    || width > MAX_FRAME_DIM as usize
                    || height > MAX_FRAME_DIM as usize
                {
                    return Err(RemoteError::Encode(format!(
                        "解码尺寸非法：{width}x{height}（上限 {MAX_FRAME_DIM}）"
                    )));
                }
                let rgba = i444_to_rgba(
                    frame.y_plane(),
                    frame.y_stride(),
                    frame.u_plane(),
                    frame.u_stride(),
                    frame.v_plane(),
                    frame.v_stride(),
                    width,
                    height,
                )?;
                last = Some(DecodedVideoFrame {
                    width: u32::try_from(width).unwrap_or(u32::MAX),
                    height: u32::try_from(height).unwrap_or(u32::MAX),
                    rgba,
                });
            }
        }
        Ok(last)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::QualityPreset;

    /// 造一帧带纹理的 RGBA（纯色会让编码器退化，颜色往返也测不出东西）。
    fn textured(width: u32, height: u32) -> Vec<u8> {
        let mut rgba = Vec::with_capacity((width * height) as usize * BYTES_PER_PIXEL);
        for y in 0..height {
            for x in 0..width {
                rgba.push(u8::try_from((x * 3) % 256).unwrap_or(0));
                rgba.push(u8::try_from((y * 5) % 256).unwrap_or(0));
                rgba.push(u8::try_from((x + y) % 256).unwrap_or(0));
                rgba.push(255);
            }
        }
        rgba
    }

    fn encode_frame(enc: &mut VideoEncoder, rgba: &[u8], w: u32, h: u32, force: bool) -> EncodedVideo {
        enc.encode(rgba, w, h, force).expect("编码应成功")
    }

    #[test]
    fn color_conversion_roundtrips() {
        // 逐个采样 RGB 立方体，往返误差应远小于有损编码误差。
        for &(r, g, b) in &[
            (0u8, 0u8, 0u8),
            (255, 255, 255),
            (255, 0, 0),
            (0, 255, 0),
            (0, 0, 255),
            (128, 64, 200),
            (10, 200, 30),
            (200, 30, 10),
        ] {
            let (y, cb, cr) = rgb_to_yuv(r, g, b);
            let (r2, g2, b2) = yuv_to_rgb(y, cb, cr);
            assert!(
                (i32::from(r) - i32::from(r2)).abs() <= 2
                    && (i32::from(g) - i32::from(g2)).abs() <= 2
                    && (i32::from(b) - i32::from(b2)).abs() <= 2,
                "({r},{g},{b}) → ({r2},{g2},{b2}) 往返误差过大"
            );
        }
    }

    #[test]
    fn gray_has_neutral_chroma() {
        // 灰色（R=G=B）的 Cb/Cr 应恰为 128——BT.709 full range 的判据。
        let (y, cb, cr) = rgb_to_yuv(128, 128, 128);
        assert_eq!((cb, cr), (128, 128), "灰色色度必须中性");
        assert_eq!(y, 128, "灰色亮度应等于原值（full range）");
    }

    #[test]
    fn encode_dimensions_boundaries() {
        // 正好 4K → 封顶到 1920x1080。
        assert_eq!(encode_dimensions(3840, 2160, 1.0, MAX_ENCODE_WIDTH), (1920, 1080));
        // 超限屏（5120x2880）→ 同样封顶，保持宽高比。
        assert_eq!(encode_dimensions(5120, 2880, 1.0, MAX_ENCODE_WIDTH), (1920, 1080));
        // 1080p 原尺寸 → 不动。
        assert_eq!(encode_dimensions(1920, 1080, 1.0, MAX_ENCODE_WIDTH), (1920, 1080));
        // 0.5 缩放 → 减半。
        assert_eq!(encode_dimensions(3840, 2160, 0.5, MAX_ENCODE_WIDTH), (1920, 1080));
        assert_eq!(encode_dimensions(1280, 800, 0.5, MAX_ENCODE_WIDTH), (640, 400));
        // 奇数尺寸 → 向下对齐到偶数。
        assert_eq!(encode_dimensions(101, 77, 1.0, MAX_ENCODE_WIDTH), (100, 76));
        // 极小尺寸 → 下限 2 像素（偶数）。
        assert_eq!(encode_dimensions(1, 1, 1.0, MAX_ENCODE_WIDTH), (2, 2));
    }

    #[test]
    fn encode_decode_roundtrip_dimensions_and_chroma() {
        // 小尺寸（远小于 256x192），保证秒级完成。
        let (w, h) = (64u32, 48u32);
        let mut enc = VideoEncoder::new(w, h, 800, 12).expect("编码器");
        assert_eq!(enc.dimensions(), (w, h));
        let rgba = textured(w, h);
        let out = encode_frame(&mut enc, &rgba, w, h, true);
        assert!(out.keyframe, "首帧应为关键帧");
        assert!(!out.is_empty());

        let mut dec = VideoDecoder::new().expect("解码器");
        let frame = dec.decode(&out.data).expect("解码应成功").expect("应有输出帧");
        assert_eq!((frame.width, frame.height), (w, h), "解码尺寸应与编码尺寸一致");
        assert_eq!(frame.rgba.len(), (w * h) as usize * BYTES_PER_PIXEL);
        // 4:4:4：颜色应大体还原（有损但无色度下采样带来的整体偏色）。
        let mid = ((h / 2) * w + (w / 2)) as usize * BYTES_PER_PIXEL;
        let src = ((h / 2) * w + (w / 2)) as usize * BYTES_PER_PIXEL;
        for c in 0..3 {
            assert!(
                (i32::from(frame.rgba[mid + c]) - i32::from(rgba[src + c])).abs() <= 24,
                "通道 {c} 还原偏差过大"
            );
        }
    }

    #[test]
    fn forced_keyframe_produces_idr() {
        let (w, h) = (64u32, 48u32);
        let mut enc = VideoEncoder::new(w, h, 800, 12).expect("编码器");
        let rgba = textured(w, h);
        let first = encode_frame(&mut enc, &rgba, w, h, true);
        assert!(first.keyframe, "首帧关键帧");
        // 同一内容再编一次且不强制 → 应为 P 帧（非 IDR）。
        let second = encode_frame(&mut enc, &rgba, w, h, false);
        assert!(!second.keyframe, "非强制帧不应是 IDR");
        // 强制 → 必为 IDR。
        let forced = encode_frame(&mut enc, &rgba, w, h, true);
        assert!(forced.keyframe, "force_keyframe 应产出 IDR");
    }

    #[test]
    fn keyframes_only_at_the_configured_interval() {
        // 机械锁：libvpx 在 realtime + CBR 下**不**按场景切换自动插 IDR。
        //
        // 桌面上的「场景切换」是家常便饭（滚一下窗口就是），自动插帧会把带宽尖峰引回来。
        // 实测（阶段 2）：250 帧逐帧整屏反色只在 `kf_max_dist` 的整数倍处出关键帧。这条用例
        // 把该前提钉住——若上游/libvpx 行为变化导致自动插帧，这里立刻红。
        let (w, h) = (64u32, 48u32);
        let mut enc = VideoEncoder::new(w, h, 800, 12).expect("编码器");
        let mut keyframe_at = Vec::new();
        for i in 0..40u32 {
            // 逐帧整屏反色：最大可能的「场景切换」。
            let mut rgba = textured(w, h);
            if i % 2 == 1 {
                for px in rgba.as_chunks_mut::<BYTES_PER_PIXEL>().0 {
                    px[0] = 255 - px[0];
                    px[1] = 255 - px[1];
                    px[2] = 255 - px[2];
                }
            }
            let out = encode_frame(&mut enc, &rgba, w, h, i == 0);
            if out.keyframe {
                keyframe_at.push(i);
            }
        }
        assert_eq!(
            keyframe_at,
            vec![0],
            "关键帧只应出现在显式强制的那一帧；自动插帧会让带宽尖峰回归"
        );
    }

    #[test]
    fn scaled_encode_downscales_and_decodes_at_encoded_size() {
        let (sw, sh) = (128u32, 96u32);
        let (dw, dh) = encode_dimensions(sw, sh, 0.5, MAX_ENCODE_WIDTH);
        assert_eq!((dw, dh), (64, 48));
        let mut enc = VideoEncoder::new(dw, dh, 800, 12).expect("编码器");
        let rgba = textured(sw, sh);
        let out = encode_frame(&mut enc, &rgba, sw, sh, true);
        let mut dec = VideoDecoder::new().expect("解码器");
        let frame = dec.decode(&out.data).expect("解码").expect("帧");
        assert_eq!((frame.width, frame.height), (dw, dh), "解码尺寸应为编码尺寸而非源尺寸");
    }

    #[test]
    fn decoder_handles_resolution_change_on_keyframe() {
        // 控制器侧切显示器/切档位时编码器会以新尺寸重建并立刻发关键帧。解码器能否在同一个
        // 实例上跟随分辨率变化，决定了是否需要在 `ScreenConfig` 变化时重建解码器。
        let mut dec = VideoDecoder::new().expect("解码器");
        let mut enc_a = VideoEncoder::new(64, 48, 800, 12).expect("编码器 A");
        let a = encode_frame(&mut enc_a, &textured(64, 48), 64, 48, true);
        let fa = dec.decode(&a.data).expect("解码 A").expect("帧 A");
        assert_eq!((fa.width, fa.height), (64, 48));

        let mut enc_b = VideoEncoder::new(96, 64, 800, 12).expect("编码器 B");
        let b = encode_frame(&mut enc_b, &textured(96, 64), 96, 64, true);
        let fb = dec.decode(&b.data).expect("解码 B").expect("帧 B");
        assert_eq!((fb.width, fb.height), (96, 64), "同一解码器应跟随分辨率变化");
    }

    #[test]
    fn garbage_packet_is_rejected() {
        let mut dec = VideoDecoder::new().expect("解码器");
        // 长度前缀声明 4 字节，但内容是垃圾。
        let mut data = Vec::new();
        data.extend_from_slice(&4u32.to_be_bytes());
        data.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
        assert!(matches!(dec.decode(&data), Err(RemoteError::Encode(_))));
    }

    #[test]
    fn truncated_length_prefix_is_rejected() {
        let mut dec = VideoDecoder::new().expect("解码器");
        assert!(matches!(dec.decode(&[0x00, 0x01]), Err(RemoteError::Encode(_))));
    }

    #[test]
    fn presets_map_to_encodable_options() {
        // 三档都应能构造出合法编码器（尺寸取小值）。
        for preset in QualityPreset::all() {
            let opts = preset.options();
            let (w, h) = encode_dimensions(320, 240, opts.scale, MAX_ENCODE_WIDTH);
            let enc = VideoEncoder::new(w, h, opts.target_bitrate_kbps, opts.max_fps);
            assert!(enc.is_ok(), "{preset:?} 应能构造编码器：{enc:?}");
        }
    }
}
