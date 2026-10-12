// 测试代码豁免 panic 风险 lint（生产代码仍告警）。
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! rust-tunnel 远程控制引擎：**纯协议 + 引擎，不含任何平台/ UI 依赖**。
//!
//! # 定位
//!
//! 两个 rust-tunnel GUI 客户端（`crates/client-gui`）之间一键互控的端侧引擎。本 crate
//! 只负责「协议 + 编排」：把控制通道下发的远程消息翻译成数据面会话，把屏幕/输入这类
//! 平台相关动作抽象成 trait 交给上层实现。截屏（`xcap`）、输入注入（`enigo`）、egui
//! 渲染都在 `crates/client-gui`，本 crate 一行平台代码都没有。
//!
//! # 分层
//!
//! - 控制面：服务端经加密控制通道下发/上报 [`rust_tunnel_common::protocol::ControlMessage`]
//!   的 9 个远程变体。本 crate 的 [`manager::RemoteManager`] 实现
//!   [`rust_tunnel_common::remote_bridge::RemoteBridge`]，由 `crates/client` 的读循环驱动。
//! - 数据面：会话帧协议跑在一条**双向字节流**上，由 [`transport::TransportEstablisher`]
//!   建立。以 **hbbr 中继为主路径**，同内网直连为优化；跨 NAT 打洞不做。
//! - 接入确认：被控端默认 [`approval::ApprovalPolicy::Confirm`]——收到 `RemoteSessionIncoming`
//!   后先弹窗等本机用户拍板，**批准了才登记 token 并回 `accepted:true`**（未批准前不登记、
//!   不回包、不派生采集任务）。因此控制器把数据面建立**推迟到看见 `accepted:true` 之后**，
//!   这是「连接中」会停留若干秒的原因。时限见 [`approval`]。
//! - 平台抽象：[`screen::ScreenSource`] / [`screen::InputSink`] / [`screen::FrameSink`] /
//!   [`screen::Clipboard`] 四个 trait 定义在 [`screen`]，实现在上层。
//! - 会话内功能：瓦片差分（[`tiles`]）、文件传输（[`file`] 沙箱 + [`transfer`] 状态机）、
//!   多显示器切换、画质档位、剪贴板，全部由 [`controller`]/[`controlled`] 的会话循环编排。
//!
//! # 数据面协议（自研，跑在中继/直连建立的裸字节流上）
//!
//! - 帧：4 字节大端长度前缀 + 1 字节 `lane` + bincode 载荷，上限
//!   [`session::MAX_SESSION_FRAME`]（16 MiB）。**不复用控制通道的 1 MiB 上限**。
//! - 握手鉴权：服务端经控制通道下发一次性 `token`，控制器发
//!   `HMAC-SHA256(token, session_id ‖ controller_rd_id ‖ nonce)`，被控端用挂起 pending
//!   会话的 token 常数时间比对（`hmac::verify_slice`），TTL 60s、一次性。
//! - 保活：[`session::HEARTBEAT_INTERVAL`]（10s，硬性小于 hbbr 的 30s `idle_timeout`）。
//! - 加密：握手交换 X25519 临时公钥，ChaCha20-Poly1305 加密后续帧（见 [`crypto`]）。
//!
//! # 与 `crates/rustdesk` 的关系
//!
//! 帧编解码、protobuf 定义、地址编码、会合/中继的**服务端语义**全部复用
//! `rust-tunnel-rustdesk`（[`transport`] 与 [`signaling`] 直接调用其 `frame`/`proto`/`addr`）。
//! 本 crate 只实现**客户端侧**的会合与中继编排。

pub mod approval;
pub mod auth;
pub mod controller;
pub mod controlled;
pub mod crypto;
pub mod error;
pub mod file;
pub mod manager;
pub mod screen;
pub mod session;
pub mod signaling;
pub mod stats;
pub mod tiles;
pub mod transfer;
pub mod transport;

#[cfg(test)]
mod integration_tests;

pub use approval::{ApprovalPolicy, PendingApproval, APPROVAL_TIMEOUT, APPROVAL_WAIT_TIMEOUT};
pub use error::{RemoteError, RemoteResult};
pub use file::{FileService, TransferMeta};
pub use controller::ControllerCommand;
pub use manager::{
    ActiveSessionInfo, RemoteManager, RemoteManagerConfig, RemoteUiState, SessionRole, SessionState,
};
pub use screen::{
    Clipboard, DecodedFrame, DecodedTile, DisplayInfo, EncodeOptions, EncodedFrame, FileEntry, FrameSink, InputEvent,
    InputSink, KeyEvent, MouseButton, QualityPreset, RawFrame, ScreenSource, TILE_SIZE, TilePatch, TileUpdate,
};
pub use session::{
    ControllerHandshake, HEARTBEAT_INTERVAL, Lane, MAX_SESSION_FRAME, SESSION_PROTO_VERSION, SessionEnd, SessionFrame,
    SessionMessage, plan_controller_handshake,
};
pub use stats::{SessionStats, SessionStatsReporter};
pub use tiles::{TileDiffer, TileEncodeResult};
pub use transfer::{
    TransferCommand, TransferDirection, TransferEvent, TransferHub, TransferProgress, CHUNK_PAUSE, CHUNK_SIZE,
};
pub use transport::{BoxDuplex, EstablishedStream, EstablishRequest, Role, TransportEstablisher, TransportPath};
