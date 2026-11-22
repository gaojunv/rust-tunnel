//! macOS 硬件编解码适配（VideoToolbox）。
//!
//! 与 Windows 的 `win_video` 同构：`rust-tunnel-mac-video` 只提供平台编解码器，本模块把它们
//! 包装成引擎的 [`VideoEncoderFactory`] / [`VideoDecoderFactory`]，由
//! [`super::hardware_factories_with_decoders`] 注入。
//!
//! # 与 Windows 的差异
//!
//! - 没有「采集设备厂商 → 后端选路」：macOS 只有一个 VideoToolbox，硬件/软件由系统选。
//! - 零拷贝载体是 `CVPixelBuffer` 而不是 D3D11 纹理：采集帧（ScreenCaptureKit）原样
//!   交给 VT；尺寸/格式不一致时由 `mac-video` 的 `VTPixelTransferSession` 在 GPU 上
//!   缩放并转 4:2:0（见 [`VtEncoder::encode_pixel_buffer`]）。
//! - 在线重配置同样走属性设置（不重建、不插 IDR），见 `mac-video` 的 `set_bitrate` /
//!   `set_fps`。

use std::sync::Arc;

use rust_tunnel_mac_video::{CodecKind, EncodeConfig, VtDecoder, VtEncoder};
use rust_tunnel_remote::codec::{
    VideoDecoderApi, VideoDecoderFactory, VideoEncoderApi, VideoEncoderFactory,
};
use rust_tunnel_remote::screen::{DecoderInfo, EncoderInfo, RawFrame};
use rust_tunnel_remote::{
    CodecId, DecodedNv12, DecodedVideoFrame, EncodedVideo, RemoteError, RemoteResult,
};
use tracing::info;

use super::mac_capture::MacGpuFrame;
use super::HardwareFactories;

/// 与 Windows 版 `MAX_QP` 对齐的画质地板（H.264 视觉近无损区间）。
const MAX_QP: u32 = 20;

/// 构造 macOS 的硬件编解码工厂。
///
/// `probe_decoders` 与 Windows 同义：无人值守实例永远不发起控制，可跳过解码器探测。
/// macOS 的探测是真实创建一次小尺寸会话，代价远低于 Windows 的 MFT，但仍尊重该开关。
#[must_use]
pub fn factories(probe_decoders: bool) -> HardwareFactories {
    // 现场排障开关：与 Windows 共用同一个环境变量，置任意值即完全回退 VP9。
    if std::env::var_os("RUST_TUNNEL_DISABLE_HWCODEC").is_some() {
        info!("RUST_TUNNEL_DISABLE_HWCODEC 已设置，跳过 VideoToolbox 探测");
        return HardwareFactories::default();
    }

    let mut out = HardwareFactories::default();

    if rust_tunnel_mac_video::encoder_available(CodecKind::H264) {
        info!("VideoToolbox H.264 硬件编码已注入");
        out.h264_encoder = Some(Arc::new(VtEncoderFactory::new(CodecKind::H264)));
    } else {
        info!("VideoToolbox H.264 编码不可用，回退 VP9");
    }

    if super::h265_disabled_by_env() {
        info!("RUST_TUNNEL_DISABLE_H265 已设置，不声明 H.265 编码能力");
    } else if rust_tunnel_mac_video::encoder_available(CodecKind::H265) {
        info!("VideoToolbox H.265 硬件编码已注入");
        out.h265_encoder = Some(Arc::new(VtEncoderFactory::new(CodecKind::H265)));
    } else {
        info!("VideoToolbox H.265 编码不可用，协商回退 H.264/VP9");
    }

    if probe_decoders {
        if rust_tunnel_mac_video::decoder_available(CodecKind::H264) {
            info!("VideoToolbox H.264 硬件解码已注入");
            out.h264_decoder = Some(Arc::new(VtDecoderFactory::new(CodecKind::H264)));
        }
        if !super::h265_disabled_by_env()
            && rust_tunnel_mac_video::decoder_available(CodecKind::H265)
        {
            info!("VideoToolbox H.265 硬件解码已注入");
            out.h265_decoder = Some(Arc::new(VtDecoderFactory::new(CodecKind::H265)));
        }
    } else {
        info!("跳过解码器探测（无人值守实例不发起控制）");
    }

    out
}

fn codec_id(codec: CodecKind) -> CodecId {
    match codec {
        CodecKind::H264 => CodecId::H264,
        CodecKind::H265 => CodecId::H265,
    }
}

fn codec_label(codec: CodecKind) -> &'static str {
    match codec {
        CodecKind::H264 => "H.264",
        CodecKind::H265 => "H.265",
    }
}

/// VideoToolbox 编码器工厂。
#[derive(Debug)]
struct VtEncoderFactory {
    codec: CodecKind,
}

impl VtEncoderFactory {
    fn new(codec: CodecKind) -> Self {
        Self { codec }
    }
}

impl VideoEncoderFactory for VtEncoderFactory {
    fn codec(&self) -> CodecId {
        codec_id(self.codec)
    }

    fn create(
        &self,
        width: u32,
        height: u32,
        target_bitrate_kbps: u32,
        max_fps: u8,
    ) -> RemoteResult<Box<dyn VideoEncoderApi>> {
        let config = EncodeConfig {
            codec: self.codec,
            width,
            height,
            bitrate_kbps: target_bitrate_kbps,
            fps: u32::from(max_fps),
            max_qp: MAX_QP,
        };
        let inner = VtEncoder::new(config).map_err(|err| {
            RemoteError::Encode(format!(
                "VideoToolbox {} 编码器创建失败: {err}",
                codec_label(self.codec)
            ))
        })?;
        Ok(Box::new(VtEncoderAdapter {
            inner,
            width,
            height,
            bitrate_kbps: target_bitrate_kbps,
            fps: u32::from(max_fps),
            max_in_flight: pipeline_depth(self.codec, width, height),
        }) as Box<dyn VideoEncoderApi>)
    }
}

/// [`VideoEncoderApi`] 适配器：引擎给 RGBA，VideoToolbox 吃 BGRA。
///
/// # 一帧深流水线
///
/// VideoToolbox 是流水线式的硬件编码器：单帧「提交 → 等完成」的延迟在 1080p 约 10–20 ms、
/// **4K 约 35 ms**，但真实吞吐明显更高（真机 4K 实测 ~40–48 fps）。逐帧同步等待把
/// **流水线延迟当成吞吐**——4K 原生分辨率下 35 ms/帧 + 采集 4–5 ms 直接掉到 ~25 fps，
/// 这正是「Windows→macOS 原生 4K 卡、ToDesk/UU 不卡」的直接原因。
///
/// 这里保持少量帧在途（[`pipeline_depth`]：默认 1，4K H.264 为 2）：提交本帧后取回
/// **最老的**一帧码流。码流顺序不变，参考链完整；代价是每帧多一份排队延迟。两个关键细节：
///
/// - **IDR 走精确同步路径**：先把在途 P 帧泵出来丢掉（IDR 不引用它们，且这些帧从未
///   交给上层，控制端看不到序号断层），再提交本帧并等它自己的输出。这样
///   `force_keyframe` 的语义与统计（`last_keyframe_us`、补丁模式基准帧
///   `base_frame_sent`）仍然精确；
/// - **首个 P 帧返回空包**（引擎明确支持 `EncodedVideo::is_empty()`）：起流水线需要先
///   提交本帧、下一拍才能取回；空包不经过线上，只是让引擎保持节拍。之后每拍提交新帧、
///   取回上一帧。
struct VtEncoderAdapter {
    inner: VtEncoder,
    width: u32,
    height: u32,
    bitrate_kbps: u32,
    fps: u32,
    /// 流水线深度（在途帧上限）。按编码标准与分辨率选，见 [`pipeline_depth`]。
    max_in_flight: usize,
}

/// 1080p 的像素数（流水线深度选择的分界）。
const FULLHD_PIXELS: u64 = 1920 * 1080;

/// 一帧深流水线的**在途帧上限**。
///
/// 真机（M4）实测的 4K 吞吐：H.265 在深度 1 就有 ~47 fps（提交不阻塞），一帧足够；
/// H.264 的 4K 硬件编码更慢（深度 1 ~28–33 fps、深度 2 ~33 fps、深度 3 ~39 fps），
/// 30 fps 目标下深度 1 会卡在临界点，深度 2 让每次提交都有队列空位。超过 1080p 才加
/// 深度，避免给普通会话白加一帧延迟。
#[must_use]
fn pipeline_depth(codec: CodecKind, width: u32, height: u32) -> usize {
    let pixels = u64::from(width) * u64::from(height);
    if codec == CodecKind::H264 && pixels > FULLHD_PIXELS {
        2
    } else {
        1
    }
}

impl VtEncoderAdapter {
    /// 同步编码一帧并取回它**自己的**输出（空包不进线上）。
    ///
    /// 只给 IDR 与（未走流式路径的）`encode` 用；调用前必须保证没有在途帧
    /// （`encode_frame` 的 IDR 分支会先 `discard_pending`）。
    fn encode_sync(
        &mut self,
        frame: &RawFrame,
        force_keyframe: bool,
    ) -> RemoteResult<rust_tunnel_mac_video::EncodedPacket> {
        if let Some(gpu) = frame.gpu.as_deref() {
            let gpu_frame = gpu.as_any().downcast_ref::<MacGpuFrame>().ok_or_else(|| {
                RemoteError::Encode("VideoToolbox 收到无法识别的 GPU 帧".to_owned())
            })?;
            return self
                .inner
                .encode_pixel_buffer(&gpu_frame.buffer, force_keyframe)
                .map_err(|err| {
                    RemoteError::Encode(format!("VideoToolbox 像素缓冲编码失败: {err}"))
                });
        }
        let bgra = rgba_to_bgra(&frame.rgba);
        self.inner
            .encode_bgra(&bgra, frame.width, frame.height, force_keyframe)
            .map_err(|err| encode_err(&err))
    }

    /// 异步提交一帧（不等输出）。
    fn submit_async(&mut self, frame: &RawFrame, force_keyframe: bool) -> RemoteResult<()> {
        if let Some(gpu) = frame.gpu.as_deref() {
            let gpu_frame = gpu.as_any().downcast_ref::<MacGpuFrame>().ok_or_else(|| {
                RemoteError::Encode("VideoToolbox 收到无法识别的 GPU 帧".to_owned())
            })?;
            return self
                .inner
                .submit_pixel_buffer(&gpu_frame.buffer, force_keyframe)
                .map_err(|err| encode_err(&err));
        }
        let bgra = rgba_to_bgra(&frame.rgba);
        self.inner
            .submit_bgra(&bgra, frame.width, frame.height, force_keyframe)
            .map_err(|err| encode_err(&err))
    }
}

/// GPU 帧的形状校验：载体类型、物理尺寸必须有效。
///
/// 认不出来的 GPU 帧说明上游接错了模块，直接报错——绝不静默编码空帧（那会给控制端
/// 送去一帧黑屏）。
fn validate_frame(frame: &RawFrame) -> RemoteResult<()> {
    let Some(gpu) = frame.gpu.as_deref() else {
        return Ok(());
    };
    let gpu_frame = gpu
        .as_any()
        .downcast_ref::<MacGpuFrame>()
        .ok_or_else(|| RemoteError::Encode("VideoToolbox 收到无法识别的 GPU 帧".to_owned()))?;
    if gpu_frame.source_width == 0 || gpu_frame.source_height == 0 {
        return Err(RemoteError::Encode(format!(
            "GPU 帧物理尺寸非法：{}x{}",
            gpu_frame.source_width, gpu_frame.source_height
        )));
    }
    Ok(())
}

/// 引擎的 CPU 帧是 RGBA；VideoToolbox 的 `CVPixelBuffer` 用 BGRA。
///
/// 缩放交给 `mac-video`（编码尺寸与源尺寸不等时由它做盒式下采样），与 Windows 的
/// `scale_rgba_box` 语义一致。
fn rgba_to_bgra(rgba: &[u8]) -> Vec<u8> {
    let mut bgra = rgba.to_vec();
    for pixel in bgra.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    bgra
}

fn encode_err(err: &rust_tunnel_mac_video::MacVideoError) -> RemoteError {
    RemoteError::Encode(format!("VideoToolbox 编码失败: {err}"))
}

impl VideoEncoderApi for VtEncoderAdapter {
    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn bitrate_kbps(&self) -> u32 {
        self.bitrate_kbps
    }

    fn fps(&self) -> u32 {
        self.fps
    }

    fn set_bitrate(&mut self, kbps: u32) -> RemoteResult<()> {
        self.inner
            .set_bitrate(kbps)
            .map_err(|err| RemoteError::Encode(format!("VideoToolbox 在线改码率失败: {err}")))?;
        self.bitrate_kbps = kbps;
        Ok(())
    }

    fn set_fps(&mut self, fps: u8) -> RemoteResult<()> {
        self.inner
            .set_fps(u32::from(fps))
            .map_err(|err| RemoteError::Encode(format!("VideoToolbox 在线改帧率失败: {err}")))?;
        self.fps = u32::from(fps);
        Ok(())
    }

    fn encode(
        &mut self,
        rgba: &[u8],
        src_width: u32,
        src_height: u32,
        force_keyframe: bool,
    ) -> RemoteResult<EncodedVideo> {
        // 同步入口：先泵干在途帧，保证 `encode_bgra` 等到的是本帧的输出。
        self.inner.discard_pending().map_err(|err| encode_err(&err))?;
        let expected = (src_width as usize)
            .checked_mul(src_height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| RemoteError::Encode("帧尺寸溢出".to_owned()))?;
        if rgba.len() != expected {
            return Err(RemoteError::Encode(format!(
                "RGBA 长度 {} 与源尺寸 {src_width}x{src_height} 不符（应为 {expected}）",
                rgba.len()
            )));
        }

        // 引擎的 CPU 帧是 RGBA；VideoToolbox 的 `CVPixelBuffer` 用 BGRA。
        // 缩放交给 `mac-video`（编码尺寸与源尺寸不等时由它做盒式下采样），
        // 与 Windows 的 `scale_rgba_box` 语义一致。
        let bgra = rgba_to_bgra(rgba);

        let packet = self
            .inner
            .encode_bgra(&bgra, src_width, src_height, force_keyframe)
            .map_err(|err| encode_err(&err))?;
        Ok(EncodedVideo {
            data: packet.data,
            keyframe: packet.keyframe,
        })
    }

    /// 编码一帧平台画面：GPU 帧（`CVPixelBuffer`）直通 VideoToolbox，CPU 帧上传后编码。
    ///
    /// 见类型文档的「一帧深流水线」：P 帧提交本帧、取回上一帧；IDR 走精确同步路径。
    /// GPU 帧的载体**必须**是 [`MacGpuFrame`]；认不出来说明上游接错了模块，直接报错
    /// ——绝不静默编码空帧（那会给控制端送去一帧黑屏）。
    fn encode_frame(
        &mut self,
        frame: &RawFrame,
        force_keyframe: bool,
    ) -> RemoteResult<EncodedVideo> {
        validate_frame(frame)?;
        if force_keyframe {
            // 精确路径：丢掉在途 P 帧（IDR 不需要它们，且它们从未交给上层），再同步
            // 编码本帧并取回**本帧**的输出。补丁模式基准帧/关键帧恢复都依赖这一点。
            self.inner.discard_pending().map_err(|err| encode_err(&err))?;
            let packet = self.encode_sync(frame, true)?;
            return Ok(EncodedVideo {
                data: packet.data,
                keyframe: packet.keyframe,
            });
        }
        // P 帧：在途帧数不足 `max_in_flight` 时只提交、返回空包（引擎明确支持）；达到
        // 深度后每拍提交新帧、取回最老的一帧，保持固定深度的流水线。
        let prime = self.inner.pending_frames() < self.max_in_flight;
        self.submit_async(frame, false)?;
        if prime {
            return Ok(EncodedVideo {
                data: Vec::new(),
                keyframe: false,
            });
        }
        let packet = self.inner.next_packet().map_err(|err| encode_err(&err))?;
        Ok(EncodedVideo {
            data: packet.data,
            keyframe: packet.keyframe,
        })
    }

    fn info(&self) -> EncoderInfo {
        let info = self.inner.info();
        EncoderInfo {
            // 把硬件/软件如实放进后端名：控制端角标一眼能看出是否真走了硬编。
            backend: if info.hardware {
                "VideoToolbox (HW)"
            } else {
                "VideoToolbox (SW)"
            }
            .to_owned(),
            name: info.name,
            adapter: String::new(),
            // VideoToolbox 直接接受 `CVPixelBuffer`（`encode_pixel_buffer`），采集侧
            // 传进来的 `MacGpuFrame` 正是 SCK 的 GPU 缓冲；这里与 NVENC 同为能力位，
            // 表示「编码器能吃 GPU 纹理」，不是「本帧一定来自 GPU」。
            gpu_input: true,
        }
    }
}

/// VideoToolbox 解码器工厂（控制器侧）。
#[derive(Debug)]
struct VtDecoderFactory {
    codec: CodecKind,
}

impl VtDecoderFactory {
    fn new(codec: CodecKind) -> Self {
        Self { codec }
    }
}

impl VideoDecoderFactory for VtDecoderFactory {
    fn codec(&self) -> CodecId {
        codec_id(self.codec)
    }

    fn create(&self) -> RemoteResult<Box<dyn VideoDecoderApi>> {
        Ok(Box::new(VtDecoderAdapter {
            codec: self.codec,
            inner: None,
            configured: None,
        }) as Box<dyn VideoDecoderApi>)
    }
}

/// [`VideoDecoderApi`] 适配器：VideoToolbox 输出紧包 NV12，直接交给 wgpu 上屏。
struct VtDecoderAdapter {
    codec: CodecKind,
    inner: Option<VtDecoder>,
    configured: Option<(u32, u32)>,
}

impl VideoDecoderApi for VtDecoderAdapter {
    fn configure(&mut self, width: u32, height: u32) -> RemoteResult<()> {
        if self.inner.is_some() && self.configured == Some((width, height)) {
            return Ok(());
        }
        // 解码器会话是懒建的（`configure` 里才拿到尺寸），构造本身不会失败。
        let mut decoder = VtDecoder::with_codec(Some(self.codec));
        decoder.configure(width, height).map_err(|err| {
            RemoteError::Encode(format!("VideoToolbox 解码器配置尺寸失败: {err}"))
        })?;
        self.inner = Some(decoder);
        self.configured = Some((width, height));
        Ok(())
    }

    fn decode(&mut self, data: &[u8]) -> RemoteResult<Option<DecodedVideoFrame>> {
        let decoder = self
            .inner
            .as_mut()
            .ok_or_else(|| RemoteError::Encode("VideoToolbox 解码器尚未 configure".to_owned()))?;
        let frame = decoder
            .decode(data)
            .map_err(|err| RemoteError::Encode(format!("VideoToolbox 解码失败: {err}")))?;
        Ok(frame.map(|frame| DecodedVideoFrame {
            width: frame.width,
            height: frame.height,
            rgba: Vec::new(),
            nv12: Some(DecodedNv12 {
                y: frame.y,
                uv: frame.uv,
            }),
        }))
    }

    fn info(&self) -> DecoderInfo {
        let Some(decoder) = self.inner.as_ref() else {
            return DecoderInfo::default();
        };
        let info = decoder.info();
        DecoderInfo {
            backend: "VideoToolbox".to_owned(),
            name: info.name,
            adapter: String::new(),
            hardware: info.hardware,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// 320×180 的测试帧：黑底 + 逐帧移动的白色方块（保证编码器有变化可编）。
    fn frame(index: u8) -> RawFrame {
        let (width, height) = (320u32, 180u32);
        let mut rgba = vec![0u8; (width * height * 4) as usize];
        for pixel in rgba.as_chunks_mut::<4>().0 {
            pixel[3] = 255;
        }
        let left = u32::from(index) * 8 % (width - 16);
        for y in 70..110 {
            for x in left..left + 16 {
                let offset = ((y * width + x) * 4) as usize;
                rgba[offset..offset + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
        RawFrame {
            display: 1,
            width,
            height,
            rgba,
            gpu: None,
            changed: true,
            changed_permille: 500,
        }
    }

    /// 4K H.264 的深度 2：前两帧填满在途，第三帧起开始取回；IDR 仍当拍精确产出。
    #[test]
    fn deep_pipeline_primes_two_frames_and_idr_is_exact() {
        let (width, height) = (2560u32, 1440u32);
        let factory = VtEncoderFactory::new(CodecKind::H264);
        let mut encoder = factory.create(width, height, 8000, 30).unwrap();
        let frame = |index: u8| {
            let mut frame = frame(index);
            frame.width = width;
            frame.height = height;
            frame.rgba = vec![128u8; (width * height * 4) as usize];
            for pixel in frame.rgba.as_chunks_mut::<4>().0 {
                pixel[3] = 255;
            }
            frame
        };

        assert!(encoder.encode_frame(&frame(0), true).unwrap().keyframe);
        assert!(encoder.encode_frame(&frame(1), false).unwrap().is_empty());
        assert!(encoder.encode_frame(&frame(2), false).unwrap().is_empty());
        let packet = encoder.encode_frame(&frame(3), false).unwrap();
        assert!(!packet.is_empty() && !packet.keyframe);

        let idr = encoder.encode_frame(&frame(4), true).unwrap();
        assert!(idr.keyframe && !idr.is_empty());
    }

    /// 流水线：IDR 当拍精确产出；起流水时返回空包；之后每拍取回上一帧。
    #[test]
    fn pipeline_depth_is_two_only_for_large_h264() {
        // 1080p 及以下：一帧足够（1080p H.264 实测 ~188 fps）。
        assert_eq!(pipeline_depth(CodecKind::H264, 1920, 1080), 1);
        // 4K H.264：深度 1 只有 ~28–33 fps，深度 2 稳定过 30。
        assert_eq!(pipeline_depth(CodecKind::H264, 3840, 2160), 2);
        assert_eq!(pipeline_depth(CodecKind::H264, 2560, 1440), 2);
        // H.265 4K 深度 1 已有 ~47 fps，不需要多付延迟。
        assert_eq!(pipeline_depth(CodecKind::H265, 3840, 2160), 1);
    }

    #[test]
    fn pipeline_keeps_order_and_keyframes_exact() {
        let factory = VtEncoderFactory::new(CodecKind::H264);
        let mut encoder = factory.create(320, 180, 2000, 30).unwrap();

        let first = encoder.encode_frame(&frame(0), true).unwrap();
        assert!(first.keyframe, "首帧必须是 IDR");
        assert!(!first.is_empty());

        // 起流水线：本拍只提交，返回空包。
        let priming = encoder.encode_frame(&frame(1), false).unwrap();
        assert!(priming.is_empty(), "首个 P 帧只提交，不应产出码流");

        // 之后每拍取回上一帧：非空、非关键帧（顺序由输出队列 FIFO 保证，这里只验证
        // 「流水线持续出包」且不会把 IDR 当成 P 帧混进来）。
        for index in 2..10u8 {
            let packet = encoder.encode_frame(&frame(index), false).unwrap();
            assert!(!packet.is_empty(), "第 {index} 帧应取回上一帧");
            assert!(!packet.keyframe);
        }

        // 在途帧存在时请求 IDR：必须**当拍**返回关键帧（不等下一帧）。
        let idr = encoder.encode_frame(&frame(10), true).unwrap();
        assert!(idr.keyframe && !idr.is_empty(), "IDR 必须精确同步产出");

        // IDR 之后重新起流水：首个 P 帧再次是空包。
        assert!(encoder.encode_frame(&frame(11), false).unwrap().is_empty());
    }
}
