//! macOS 远程控制硬件数据面（VideoToolbox）。
//!
//! # 与 Windows `win-video` 的对应关系
//!
//! | Windows | 本 crate |
//! |---|---|
//! | NVENC / MFT H.264·H.265 编码 | VideoToolbox `VTCompressionSession` |
//! | MSH264DecoderMFT / MSH265DecoderMFT | `VTDecompressionSession` |
//! | WGC/DXGI BGRA 帧 | 由 `rust-tunnel-mac-capture`（ScreenCaptureKit）提供 |
//! | VideoProcessor 纹理缩放（GPU 零拷贝） | `pixel_transfer`（`VTPixelTransferSession`） |
//! | `pixels.rs` / `change.rs` / 脏矩形 | [`color`] / [`change`] |
//! | Annex B NAL 解析（`mft_encoder/bitstream.rs`） | [`annexb`] |
//!
//! # 编译面
//!
//! 纯逻辑模块（[`annexb`] / [`color`] / [`change`]）**在所有平台编译**，因此可以在
//! Windows 开发机上直接 `cargo test -p rust-tunnel-mac-video --lib`；Apple API 只在
//! `target_os = "macos"` 下编译，不会把 objc2 依赖带进 Windows 构建。
//!
//! # 线程约定
//!
//! 编码器/解码器**独占使用**：引擎把编码器移进 `spawn_blocking`，解码器由一个任务持有。
//! 因此类型只要求 `Send`，不要求 `Sync`（与 `remote::codec` 的约定一致）。

pub mod annexb;
pub mod change;
pub mod codec;
pub mod color;
pub mod error;

#[cfg(target_os = "macos")]
pub mod pixel_transfer;
#[cfg(target_os = "macos")]
pub mod vt_decoder;
#[cfg(target_os = "macos")]
pub mod vt_encoder;

pub use codec::CodecKind;
pub use error::{MacVideoError, MacVideoResult};

#[cfg(target_os = "macos")]
pub use pixel_transfer::PixelTransfer;
#[cfg(target_os = "macos")]
pub use vt_decoder::{DecodedNv12Frame, VtDecoder};
#[cfg(target_os = "macos")]
pub use vt_encoder::{EncodeConfig, EncodedPacket, VtEncoder};

/// 本机是否有该编码标准的**硬件编码器**（macOS 上真实创建一次探测；其它平台恒 `false`）。
#[must_use]
pub fn encoder_available(codec: CodecKind) -> bool {
    #[cfg(target_os = "macos")]
    {
        vt_encoder::probe(codec)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = codec;
        false
    }
}

/// 本机是否有该编码标准的**硬件解码器**（macOS 上真实创建一次探测；其它平台恒 `false`）。
#[must_use]
pub fn decoder_available(codec: CodecKind) -> bool {
    #[cfg(target_os = "macos")]
    {
        vt_decoder::probe(codec)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = codec;
        false
    }
}
