//! 编解码器抽象与工厂（协议层与具体实现解耦）。
//!
//! # 为什么需要这层
//!
//! 阶段 5 起数据面不再只有 VP9：Windows 被控端可以注入一个 H.264（4:2:0）硬件编码器，
//! 由 [`crate::controlled`] 在协商后选用。会话循环只依赖下面这些 trait，不依赖 libvpx 或
//! 任何平台编码器类型；具体实现（默认 VP9、注入的 H.264）由工厂提供。
//!
//! **默认实现必须仍是 VP9**：未注入任何工厂时行为与阶段 3/4 完全一致。
//!
//! # 谁实现什么
//!
//! - [`VideoEncoder`] / [`VideoDecoder`]（[`crate::video`]）实现 [`VideoEncoderApi`] /
//!   [`VideoDecoderApi`]，对应 VP9 profile 1 4:4:4 软件编解码。
//! - [`Vp9EncoderFactory`] / [`Vp9DecoderFactory`] 是它们的工厂。
//! - 硬件实现（`crates/win-video`）只需实现这两个 API trait 并各自提供一个工厂，由上层经
//!   `RemoteManagerConfig` / `ControlledConfig` / `ControllerConfig` 注入。
//!
//! # 线程约定
//!
//! 编码器/解码器**独占使用**：编码任务把它移进 `spawn_blocking`，解码任务由一个任务持有。
//! 因此 trait 只要求 `Send`（不要求 `Sync`）；工厂本身是共享配置，要求 `Send + Sync`。
//!
//! # 色度格式
//!
//! 本层**不协商色度格式**：VP9 固定 profile 1（4:4:4），H.264 固定 4:2:0。需要 4:4:4 的
//! 调用方应继续选 VP9。

use crate::error::RemoteResult;
use crate::session::CodecId;
use crate::video::{DecodedVideoFrame, EncodedVideo, VideoDecoder, VideoEncoder};
use std::sync::Arc;

/// 编码器实例接口。
///
/// 每个实例绑定一个固定的**编码尺寸**（[`VideoEncoderApi::dimensions`]）；几何变化由上层
/// 丢弃旧实例、用工厂新建一个实现（与阶段 3 的编码器重建规则一致）。
pub trait VideoEncoderApi: Send {
    /// 编码尺寸（宽, 高）。
    #[must_use]
    fn dimensions(&self) -> (u32, u32);

    /// 当前生效的目标码率（kbps）。
    #[must_use]
    fn bitrate_kbps(&self) -> u32;

    /// 当前生效的帧率。
    #[must_use]
    fn fps(&self) -> u32;

    /// **在线**重配置目标码率（不重建编码器、不插 IDR）。
    ///
    /// # Errors
    /// 实现拒绝该配置时返回错误；调用方应丢弃编码器、下一帧重建。
    fn set_bitrate(&mut self, kbps: u32) -> RemoteResult<()>;

    /// **在线**重配置帧率（不重建编码器、不插 IDR）。
    ///
    /// # Errors
    /// 实现拒绝该配置时返回错误。
    fn set_fps(&mut self, fps: u8) -> RemoteResult<()>;

    /// 编码一帧 `src_width × src_height` 的 RGBA。
    ///
    /// 实现负责缩放到 [`VideoEncoderApi::dimensions`]。`force_keyframe` 为真时本帧必须是
    /// 可独立解码的关键帧（会话首帧、配置变更、`RequestKeyframe`）。
    ///
    /// # Errors
    /// 输入长度与源尺寸不符、编码器内部失败时返回错误。
    fn encode(
        &mut self,
        rgba: &[u8],
        src_width: u32,
        src_height: u32,
        force_keyframe: bool,
    ) -> RemoteResult<EncodedVideo>;

    /// 编码一帧 [`RawFrame`](crate::screen::RawFrame)。
    ///
    /// 默认实现把 CPU 帧的 `rgba` 交给 [`VideoEncoderApi::encode`]；GPU 帧由平台
    /// 实现覆盖（例如 Windows 的硬件编码器直接吃 D3D11 纹理）。VP9 永远走默认实现
    /// ——引擎只在 `RawFrame::gpu` 为 `Some` 时才会出现 GPU 帧，而那只会在 H.264
    /// 硬件路径上发生。
    ///
    /// # Errors
    /// 与 [`VideoEncoderApi::encode`] 相同；GPU 实现还会在纹理无法使用时返回错误
    /// （调用方据此重建编码器并回退 CPU 采集）。
    fn encode_frame(
        &mut self,
        frame: &crate::screen::RawFrame,
        force_keyframe: bool,
    ) -> RemoteResult<EncodedVideo> {
        self.encode(&frame.rgba, frame.width, frame.height, force_keyframe)
    }
}

/// 解码器实例接口。
pub trait VideoDecoderApi: Send {
    /// 告知解码器即将解码的**编码尺寸**（宽, 高）。
    ///
    /// VP9 解码器从码流自身就能得到尺寸，故默认实现是空操作。硬件解码器可能需要在创建
    /// MFT/解码会话时就知道 `width×height`（Windows Media Foundation 的 `MF_MT_FRAME_SIZE`），
    /// 而尺寸要到 `ScreenConfig` / 首个 `VideoPacket` 才拿得到，所以把它做成**创建后的独立
    /// 生命周期钩子**，而不是塞进 [`VideoDecoderFactory::create`] 的参数里。
    ///
    /// 调用方在**每个解码器实例**上、每次编码尺寸变化时调用；同一尺寸不重复调用。
    ///
    /// # Errors
    /// 实现无法配置该尺寸时返回错误；调用方应丢弃解码器、等下一个关键帧重建。
    fn configure(&mut self, width: u32, height: u32) -> RemoteResult<()> {
        let _ = (width, height);
        Ok(())
    }

    /// 解码一批（长度前缀拼接的）码流，返回**最后**一帧；无输出时为 `None`。
    ///
    /// # Errors
    /// 长度前缀不完整、码流非法或解码器失败时返回错误。
    fn decode(&mut self, data: &[u8]) -> RemoteResult<Option<DecodedVideoFrame>>;
}

/// 编码器工厂：被控端按协商结果创建编码器实例。
///
/// `Debug` 是超集要求：配置结构体（[`crate::controlled::ControlledConfig`]）派生了 `Debug`。
pub trait VideoEncoderFactory: Send + Sync + std::fmt::Debug {
    /// 本工厂产出的 codec 标识。
    #[must_use]
    fn codec(&self) -> CodecId;

    /// 创建一个编码 `width × height` 的编码器实例。
    ///
    /// # Errors
    /// 尺寸非法或底层编码器初始化失败时返回错误。
    fn create(
        &self,
        width: u32,
        height: u32,
        target_bitrate_kbps: u32,
        max_fps: u8,
    ) -> RemoteResult<Box<dyn VideoEncoderApi>>;
}

/// 解码器工厂：控制器按协商结果创建解码器实例。
pub trait VideoDecoderFactory: Send + Sync + std::fmt::Debug {
    /// 本工厂产出的 codec 标识。
    #[must_use]
    fn codec(&self) -> CodecId;

    /// 创建一个解码器实例。
    ///
    /// # Errors
    /// 底层解码器初始化失败时返回错误。
    fn create(&self) -> RemoteResult<Box<dyn VideoDecoderApi>>;
}

impl VideoEncoderApi for VideoEncoder {
    fn dimensions(&self) -> (u32, u32) {
        VideoEncoder::dimensions(self)
    }

    fn bitrate_kbps(&self) -> u32 {
        VideoEncoder::bitrate_kbps(self)
    }

    fn fps(&self) -> u32 {
        VideoEncoder::fps(self)
    }

    fn set_bitrate(&mut self, kbps: u32) -> RemoteResult<()> {
        VideoEncoder::set_bitrate(self, kbps)
    }

    fn set_fps(&mut self, fps: u8) -> RemoteResult<()> {
        VideoEncoder::set_fps(self, fps)
    }

    fn encode(
        &mut self,
        rgba: &[u8],
        src_width: u32,
        src_height: u32,
        force_keyframe: bool,
    ) -> RemoteResult<EncodedVideo> {
        VideoEncoder::encode(self, rgba, src_width, src_height, force_keyframe)
    }
}

impl VideoDecoderApi for VideoDecoder {
    fn decode(&mut self, data: &[u8]) -> RemoteResult<Option<DecodedVideoFrame>> {
        VideoDecoder::decode(self, data)
    }
}

/// 默认（VP9 profile 1 4:4:4）编码器工厂。
#[derive(Debug, Default, Clone, Copy)]
pub struct Vp9EncoderFactory;

impl VideoEncoderFactory for Vp9EncoderFactory {
    fn codec(&self) -> CodecId {
        CodecId::Vp9
    }

    fn create(
        &self,
        width: u32,
        height: u32,
        target_bitrate_kbps: u32,
        max_fps: u8,
    ) -> RemoteResult<Box<dyn VideoEncoderApi>> {
        Ok(Box::new(VideoEncoder::new(
            width,
            height,
            target_bitrate_kbps,
            max_fps,
        )?))
    }
}

/// 默认（VP9 profile 1 4:4:4）解码器工厂。
#[derive(Debug, Default, Clone, Copy)]
pub struct Vp9DecoderFactory;

impl VideoDecoderFactory for Vp9DecoderFactory {
    fn codec(&self) -> CodecId {
        CodecId::Vp9
    }

    fn create(&self) -> RemoteResult<Box<dyn VideoDecoderApi>> {
        Ok(Box::new(VideoDecoder::new()?))
    }
}

/// 取默认 VP9 编码器工厂（未注入硬件实现时使用）。
#[must_use]
pub fn vp9_encoder_factory() -> Arc<dyn VideoEncoderFactory> {
    Arc::new(Vp9EncoderFactory)
}

/// 取默认 VP9 解码器工厂（未注入硬件实现时使用）。
#[must_use]
pub fn vp9_decoder_factory() -> Arc<dyn VideoDecoderFactory> {
    Arc::new(Vp9DecoderFactory)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::MAX_FRAME_DIM;

    /// 造一帧带纹理的 RGBA（纯色会让编码器退化）。
    fn textured(width: u32, height: u32) -> Vec<u8> {
        let mut rgba = Vec::with_capacity((width * height) as usize * 4);
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

    #[test]
    fn vp9_factories_report_their_codec() {
        assert_eq!(Vp9EncoderFactory.codec(), CodecId::Vp9);
        assert_eq!(Vp9DecoderFactory.codec(), CodecId::Vp9);
        assert_eq!(vp9_encoder_factory().codec(), CodecId::Vp9);
        assert_eq!(vp9_decoder_factory().codec(), CodecId::Vp9);
    }

    #[test]
    fn trait_objects_roundtrip_a_frame() {
        let (w, h) = (64u32, 48u32);
        let mut enc = Vp9EncoderFactory.create(w, h, 800, 12).expect("创建编码器");
        assert_eq!(enc.dimensions(), (w, h));
        assert_eq!(enc.bitrate_kbps(), 800);
        assert_eq!(enc.fps(), 12);
        let out = enc.encode(&textured(w, h), w, h, true).expect("编码应成功");
        assert!(out.keyframe, "首帧应为关键帧");
        assert!(!out.is_empty());

        let mut dec = Vp9DecoderFactory.create().expect("创建解码器");
        let frame = dec
            .decode(&out.data)
            .expect("解码应成功")
            .expect("应有输出帧");
        assert_eq!((frame.width, frame.height), (w, h));
        assert_eq!(frame.rgba.len(), (w * h) as usize * 4);
    }

    #[test]
    fn trait_object_supports_online_reconfiguration() {
        let mut enc = Vp9EncoderFactory
            .create(64, 48, 800, 12)
            .expect("创建编码器");
        enc.set_bitrate(400).expect("在线改码率");
        enc.set_fps(8).expect("在线改帧率");
        assert_eq!(enc.bitrate_kbps(), 400);
        assert_eq!(enc.fps(), 8);
    }

    #[test]
    fn factory_rejects_invalid_dimensions() {
        assert!(Vp9EncoderFactory.create(0, 48, 800, 12).is_err());
        assert!(Vp9EncoderFactory
            .create(MAX_FRAME_DIM + 1, 48, 800, 12)
            .is_err());
    }
}
