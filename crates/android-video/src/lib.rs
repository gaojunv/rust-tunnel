//! Android **MediaCodec** H.264 / H.265 硬件解码（控制器侧）。
//!
//! 本 crate 把 Android NDK 的 `AMediaCodec` 包装成 `rust-tunnel-remote` 的
//! [`VideoDecoderApi`](rust_tunnel_remote::VideoDecoderApi) 实现，供 Android 控制器
//! （`crates/controller-core` / `crates/android-jni`）在协商到 H.264 / H.265 时注入。
//!
//! # 提供什么
//!
//! - [`h264_decoder_factory`] / [`h265_decoder_factory`]：两个解码器工厂，装配进
//!   `RemoteManagerConfig::{h264_decoder_factory, h265_decoder_factory}`。
//! - [`codec_available`]：创建式能力探测（真的能建出对应 codec 的解码器才返回 `true`）。
//!
//! # 数据流
//!
//! ```text
//! VideoPacket.data（4 字节大端长度前缀 + 负载 的拼接）
//!   └─ nal::access_units        → 若干 Annex B 访问单元（补/规整 00 00 00 01 起始码）
//!        └─ AMediaCodec（字节缓冲输出，surface = null）
//!             └─ nv12::pack_*   → 紧包 NV12（去 stride/slice 填充，I420 则交织 U/V）
//!                  └─ DecodedVideoFrame { nv12: Some(..), rgba: [] }
//! ```
//!
//! # 平台
//!
//! FFI 与真实引擎仅在 `target_os = "android"` 编译；其它平台只编译纯逻辑
//! （`nal` / `nv12`）以便在宿主上单测，[`codec_available`] 恒为 `false`。
//!
//! # 编码器尺寸
//!
//! 线上尺寸由对端协商（H.264/H.265 固定 4:2:0、BT.709 limited）。`configure(width,
//! height)` 告诉解码器编码尺寸；MediaCodec 的可见尺寸可能与编码尺寸略有差异（宏块/
//! CTU 对齐），本层按输出格式上报的宽高裁剪，仍需真机确认具体设备是否已应用裁剪。

// 非 Android 平台只编译纯逻辑用于宿主单测；FFI 使用点在 `cfg(target_os = "android")`
// 之外不可见，故整体放行 dead_code（真正使用的代码在 Android 上全部有调用点）。
#![cfg_attr(
    not(target_os = "android"),
    allow(
        dead_code,
        reason = "非 Android 平台只编译纯逻辑，FFI 使用点在 cfg 外不可见"
    )
)]

mod decoder;
mod nal;
mod nv12;

#[cfg(target_os = "android")]
mod ffi;

use rust_tunnel_remote::{CodecId, VideoDecoderFactory};
use std::sync::Arc;

use crate::decoder::MediaCodecDecoderFactory;
use crate::nal::Syntax;

/// H.264 / AVC 硬件解码器工厂（`AMediaCodec` 的 `video/avc`）。
///
/// 装配进 `RemoteManagerConfig::h264_decoder_factory`；`codec()` 返回
/// [`CodecId::H264`]，`create()` 返回惰性解码器。
#[must_use]
pub fn h264_decoder_factory() -> Arc<dyn VideoDecoderFactory> {
    Arc::new(MediaCodecDecoderFactory::new(Syntax::H264))
}

/// H.265 / HEVC 硬件解码器工厂（`AMediaCodec` 的 `video/hevc`）。
///
/// 装配进 `RemoteManagerConfig::h265_decoder_factory`；`codec()` 返回
/// [`CodecId::H265`]。设备不具备 HEVC 解码器时，[`codec_available`] 返回 `false`，
/// 控制器据此不声明 H.265、协商回退 H.264。
#[must_use]
pub fn h265_decoder_factory() -> Arc<dyn VideoDecoderFactory> {
    Arc::new(MediaCodecDecoderFactory::new(Syntax::H265))
}

/// 创建式能力探测：**真的能建出**对应 codec 的解码器才返回 `true`。
///
/// - `CodecId::H264` / `CodecId::H265`：在 Android 上调用
///   `AMediaCodec_createDecoderByType`，非空即视为可用（随即释放）。
/// - `CodecId::Vp9` 或非 Android 平台：恒为 `false`（本 crate 只做 H.264/H.265）。
///
/// 控制器用它决定 `CodecOffer` 里是否声明对应硬件 codec，避免「协商到某 codec 却在
/// 解码时才失败」。
#[must_use]
pub fn codec_available(codec: CodecId) -> bool {
    #[cfg(target_os = "android")]
    {
        match Syntax::from_codec_id(codec) {
            Some(syntax) => crate::ffi::probe_available(syntax),
            None => false,
        }
    }
    #[cfg(not(target_os = "android"))]
    {
        // 非 Android 平台没有 MediaCodec：无论哪个 codec 都不可用。
        let _ = codec;
        false
    }
}
