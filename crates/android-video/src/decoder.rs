//! [`VideoDecoderApi`] 适配层：把 MediaCodec 引擎（或非 Android 的桩）包装成解码器。
//!
//! 惰性生命周期：`configure(width, height)` 只记录尺寸（非 Android 直接报错），
//! 真正的解码器实例在**首次 `decode`** 时按尺寸创建——这样能把首帧里的 SPS/PPS
//! 填进 `csd-0`，也避免了「先 configure 后拿到参数集」的时序问题。

use crate::nal::Syntax;
use rust_tunnel_remote::codec::DecodeTimings;
use rust_tunnel_remote::screen::DecoderInfo;
#[cfg(not(target_os = "android"))]
use rust_tunnel_remote::RemoteError;
use rust_tunnel_remote::{
    CodecId, DecodedVideoFrame, RemoteResult, VideoDecoderApi, VideoDecoderFactory,
};

/// 引擎类型：Android 上是真实 MediaCodec 引擎，其余平台是返回错误的桩。
#[cfg(target_os = "android")]
type Engine = crate::ffi::MediaCodecEngine;

/// 非 Android 平台：configure/decode 一律报错（`codec_available` 也恒为 false，
/// 控制器不会声明该能力，正常流程根本不会走到这里）。
#[cfg(not(target_os = "android"))]
struct Engine {
    /// 保留语法字段以便未来按平台区分错误文案。
    _syntax: Syntax,
}

#[cfg(not(target_os = "android"))]
#[allow(
    clippy::unused_self,
    reason = "桩引擎的方法不访问 self，只为与 Android 真实引擎保持同一签名"
)]
impl Engine {
    /// 构造桩引擎。
    fn new(syntax: Syntax) -> Self {
        Self { _syntax: syntax }
    }

    /// 非 Android 平台无法配置 MediaCodec。
    fn configure(&mut self, _width: u32, _height: u32) -> RemoteResult<()> {
        Err(RemoteError::Encode(
            "MediaCodec 解码器仅在 Android 上可用".to_owned(),
        ))
    }

    /// 非 Android 平台无法解码。
    fn decode(&mut self, _data: &[u8]) -> RemoteResult<Option<DecodedVideoFrame>> {
        Err(RemoteError::Encode(
            "MediaCodec 解码器仅在 Android 上可用".to_owned(),
        ))
    }

    /// 桩引擎没有后端信息。
    fn info(&self) -> DecoderInfo {
        DecoderInfo::default()
    }

    /// 桩引擎没有耗时读数。
    fn timings(&self) -> DecodeTimings {
        DecodeTimings::default()
    }
}

/// MediaCodec 解码器工厂（H.264 / H.265 各一个实例）。
#[derive(Debug)]
pub(crate) struct MediaCodecDecoderFactory {
    /// 本工厂产出的编码语法。
    syntax: Syntax,
}

impl MediaCodecDecoderFactory {
    /// 构造工厂。
    pub(crate) fn new(syntax: Syntax) -> Self {
        Self { syntax }
    }
}

impl VideoDecoderFactory for MediaCodecDecoderFactory {
    fn codec(&self) -> CodecId {
        self.syntax.codec_id()
    }

    fn create(&self) -> RemoteResult<Box<dyn VideoDecoderApi>> {
        Ok(Box::new(MediaCodecAdapter::new(self.syntax)))
    }
}

/// 单个解码器实例（惰性：`configure` 前没有底层实例）。
struct MediaCodecAdapter {
    /// 已记录尺寸、可能已创建实例的引擎。
    engine: Engine,
}

impl MediaCodecAdapter {
    /// 构造适配器（尚未 `configure`）。
    fn new(syntax: Syntax) -> Self {
        Self {
            engine: Engine::new(syntax),
        }
    }
}

impl VideoDecoderApi for MediaCodecAdapter {
    fn configure(&mut self, width: u32, height: u32) -> RemoteResult<()> {
        self.engine.configure(width, height)
    }

    fn decode(&mut self, data: &[u8]) -> RemoteResult<Option<DecodedVideoFrame>> {
        self.engine.decode(data)
    }

    fn info(&self) -> DecoderInfo {
        self.engine.info()
    }

    fn last_decode_timings(&self) -> DecodeTimings {
        self.engine.timings()
    }
}
