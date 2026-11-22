//! macOS 远程控制音频层（ScreenCaptureKit 系统声音 + AudioQueue 播放 + Opus）。
//!
//! 与 `rust-tunnel-win-audio` 对应：本 crate 不依赖 `rust-tunnel-remote`，由
//! `client-gui/src/remote/mac_audio.rs` 适配成 `AudioSource` / `AudioSink`。
//!
//! # 固定口径
//!
//! - 采样率 **48 kHz**、**立体声**、`f32` 交错；采集侧用 ScreenCaptureKit 的
//!   `SCStreamConfiguration.sampleRate/channelCount` 让系统把设备混音格式归一，
//!   播放侧用 AudioQueue 的 linear PCM 格式做同样的事。
//! - 编码 **Opus**，20 ms 一帧、96 kbps CBR；一帧就是一条会话消息。
//! - 采集源是**系统声音**（`SCStreamConfiguration.capturesAudio`），不是麦克风。
//!
//! # 编译面
//!
//! 平台强绑定的部分（[`capture`] / [`playback`]）只在 `cfg(target_os = "macos")`
//! 下编译；纯逻辑（[`pcm`] / [`metrics`] / [`wav`] / [`format`]，以及内部的抖动
//! 缓冲）在所有平台编译，因此 Windows 开发机上可以直接
//! `cargo test -p rust-tunnel-mac-audio`。`opus` feature 默认开启；关掉后 [`opus`]
//! 退化为返回 [`MacAudioError::Unsupported`] 的同签名实现，便于在非 macOS 主机上
//! 做 `--target aarch64-apple-darwin` 类型检查（`audiopus_sys` 的 CMake 构建无法
//! 从 Windows 交叉到 macOS）。
//!
//! # 与引擎 trait 的对应
//!
//! - [`capture::LoopbackEncoder`] → `rust_tunnel_remote::AudioSource`
//!   （`start` / `read_packet(&AtomicBool)` / `stop` 全部用 `&self` + 内部互斥，
//!   适配器可以直接把它放进去而不用 `Mutex` 包一层）。
//! - [`playback::Playback`] → `rust_tunnel_remote::AudioSink`
//!   （`on_packet` 只做 `try_send`，绝不阻塞引擎运行时线程）。

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
    not(target_os = "macos"),
    allow(
        unused_crate_dependencies,
        reason = "macOS-only crate：非 macOS 平台没有 objc2 依赖的使用点"
    )
)]

#[cfg_attr(
    not(target_os = "macos"),
    allow(dead_code, reason = "只有 macOS 的采集链路用到进程内单调时钟")
)]
mod clock;
pub mod error;
pub mod format;
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "只有 macOS 的播放线程使用抖动缓冲；其它平台只跑它的单测"
    )
)]
mod jitter;
pub mod metrics;
pub mod opus;
pub mod pcm;
pub mod wav;

#[cfg(target_os = "macos")]
pub mod capture;
#[cfg(target_os = "macos")]
pub mod playback;

pub use error::{MacAudioError, MacAudioResult};

#[cfg(target_os = "macos")]
pub use capture::{CaptureStats, EncodedPacket, LoopbackEncoder};
#[cfg(target_os = "macos")]
pub use playback::{Playback, PlaybackSnapshot, ReceivedPacket};
