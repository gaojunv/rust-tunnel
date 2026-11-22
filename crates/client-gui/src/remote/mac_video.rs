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
        }) as Box<dyn VideoEncoderApi>)
    }
}

/// [`VideoEncoderApi`] 适配器：引擎给 RGBA，VideoToolbox 吃 BGRA。
struct VtEncoderAdapter {
    inner: VtEncoder,
    width: u32,
    height: u32,
    bitrate_kbps: u32,
    fps: u32,
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
        let mut bgra = rgba.to_vec();
        for pixel in bgra.as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
        }

        let packet = self
            .inner
            .encode_bgra(&bgra, src_width, src_height, force_keyframe)
            .map_err(|err| RemoteError::Encode(format!("VideoToolbox 编码失败: {err}")))?;
        Ok(EncodedVideo {
            data: packet.data,
            keyframe: packet.keyframe,
        })
    }

    /// 编码一帧平台画面：GPU 帧（`CVPixelBuffer`）直通 VideoToolbox，CPU 帧走原路径。
    ///
    /// GPU 帧的载体**必须**是 [`MacGpuFrame`]；认不出来说明上游接错了模块，直接报错
    /// ——绝不静默编码空帧（那会给控制端送去一帧黑屏）。
    fn encode_frame(
        &mut self,
        frame: &RawFrame,
        force_keyframe: bool,
    ) -> RemoteResult<EncodedVideo> {
        let Some(gpu) = frame.gpu.as_deref() else {
            return self.encode(&frame.rgba, frame.width, frame.height, force_keyframe);
        };
        let gpu_frame = gpu
            .as_any()
            .downcast_ref::<MacGpuFrame>()
            .ok_or_else(|| RemoteError::Encode("VideoToolbox 收到无法识别的 GPU 帧".to_owned()))?;
        // 物理尺寸为零说明采集侧给了坏帧：直接报错，别把空缓冲交给 VT。
        if gpu_frame.source_width == 0 || gpu_frame.source_height == 0 {
            return Err(RemoteError::Encode(format!(
                "GPU 帧物理尺寸非法：{}x{}",
                gpu_frame.source_width, gpu_frame.source_height
            )));
        }
        let packet = self
            .inner
            .encode_pixel_buffer(&gpu_frame.buffer, force_keyframe)
            .map_err(|err| RemoteError::Encode(format!("VideoToolbox 像素缓冲编码失败: {err}")))?;
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
