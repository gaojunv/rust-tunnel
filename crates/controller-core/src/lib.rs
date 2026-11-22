// 测试代码豁免 panic 风险 lint（生产代码仍告警）。
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::pedantic
    )
)]

//! 平台无关的**无头控制器宿主**：把 `crates/client-gui` 里 bin 内部的编排抽成库，
//! 供 Android（JNI）等没有 egui/eframe 的宿主调用。
//!
//! # 这个 crate 做什么
//!
//! 只负责「控制端」这一半：建立加密控制通道、装配 [`RemoteManager`]（控制器视角、
//! 不承担被控）、按退避循环重连、把 UI 命令翻译成引擎调用、把引擎状态聚合成一份
//! 可 `watch` 的轻量快照。画面解码后的**渲染/触控/音频播放**由宿主实现
//! （Android 层实现 [`FrameSink`]，本 crate 一行平台代码都没有）。
//!
//! # 不依赖什么
//!
//! 不依赖 egui / eframe / winit / xcap / enigo / arboard / tray-icon / win-*。
//! `rust-tunnel-client` 以 `default-features = false` 引入（不带 tree-sitter /
//! portable-pty），`rust-tunnel-common` 也关掉了 SQLite 与 reqwest（见其 feature）。
//!
//! # 典型用法
//!
//! ```no_run
//! use rust_tunnel_common::RemoteMode;
//! use rust_tunnel_controller_core::{ControllerConfig, ControllerHost};
//!
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! let host = ControllerHost::start(
//!     ControllerConfig::new("example.com:3000", "client-token")
//!         .with_client_name("android-tablet"),
//! )?;
//! // 宿主自己的 FrameSink 实现（Android 里是 Surface/MediaCodec 那一层）。
//! // host.set_frame_sink(Arc::new(my_sink));
//! let mut snapshots = host.snapshots();
//! let commands = host.commands();
//!
//! let _ = commands.connect("home-pc", RemoteMode::Control);
//! while snapshots.changed().await.is_ok() {
//!     let snap = snapshots.borrow().clone();
//!     let _ = snap.event();
//! }
//! host.stop().await;
//! # Ok(())
//! # }
//! ```
//!
//! [`RemoteManager`]: rust_tunnel_remote::RemoteManager
//! [`FrameSink`]: rust_tunnel_remote::FrameSink

pub mod backoff;
pub mod commands;
pub mod config;
pub mod error;
pub mod host;
pub mod state;

pub use backoff::Backoff;
pub use commands::ControllerCommands;
pub use config::{ControllerConfig, DEFAULT_CLIENT_NAME, DEFAULT_LOG_LEVEL};
pub use error::{ControllerError, ControllerResult};
pub use host::{ControllerHost, STOP_TIMEOUT};
pub use state::{ConnectionState, ControllerEvent, ControllerSnapshot, LoopPhase, SessionPhase};

// 宿主实现 `FrameSink` / 构造命令时需要的引擎类型，集中再导出一份，JNI 层不必
// 同时记住 `rust-tunnel-remote` 与 `rust-tunnel-common` 的模块路径。
pub use rust_tunnel_common::RemoteMode;
pub use rust_tunnel_remote::screen::DecodedPatch;
pub use rust_tunnel_remote::{
    CursorShape, DecodedFrame, DisplayInfo, FrameSink, InputEvent, KeyEvent, MouseButton,
    QualityPreset,
};
