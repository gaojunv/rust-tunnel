//! Windows 远程控制的**音频数据面**：WASAPI loopback 采集 / 播放 + Opus。
//!
//! 与 `rust-tunnel-win-video` 同构：本 crate 只做平台强绑定的采集/回放与编解码，
//! **不依赖 `rust-tunnel-remote`**、不定义会话协议。引擎侧只认 `AudioSource` /
//! `AudioSink` 两个平台 trait（在 `rust-tunnel-remote` 里定义），由 `client-gui`
//! 把这里的能力适配过去。
//!
//! # 固定口径
//!
//! - 采样率 **48 kHz**、**立体声**、`f32` 交错；采集/播放都用 WASAPI 的
//!   `AUTOCONVERTPCM | SRC_DEFAULT_QUALITY` 让音频引擎完成设备混音格式的转换
//!   （44.1 kHz、5.1、24-bit 设备都由此归一，避免自己写重采样器）。
//! - 编码 **Opus**，20 ms 一帧、96 kbps CBR；一帧就是一条会话消息。
//! - 采集源是**默认回放设备**的 loopback（被控端系统声音），不是麦克风。
//!
//! # 编译面
//!
//! 非 Windows 只编译空壳；Windows 的 `x86_64-pc-windows-gnu`（出厂 GUI）链接
//! `vendor/libopus` 的预编译静态库，其它 Windows target（含 MSVC）退化为运行期
//! 返回 [`AudioError::Unsupported`] 的空壳——见 `build.rs`。

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        clippy::float_cmp
    )
)]
#![cfg_attr(
    not(windows),
    allow(
        unused_crate_dependencies,
        reason = "Windows-only crate：非 Windows 平台没有依赖使用点"
    )
)]

#[cfg(windows)]
pub mod clock;
#[cfg(windows)]
pub mod error;
#[cfg(windows)]
pub mod format;
#[cfg(windows)]
pub mod jitter;
#[cfg(windows)]
pub mod metrics;
#[cfg(windows)]
pub mod opus;
#[cfg(windows)]
pub mod playback;
#[cfg(windows)]
pub mod source;
#[cfg(windows)]
pub mod wasapi;
#[cfg(windows)]
pub mod wav;

#[cfg(windows)]
pub use error::{AudioError, AudioResult};
#[cfg(windows)]
pub use wasapi::{Capture, Render};
