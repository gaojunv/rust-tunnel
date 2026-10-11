//! 数据面传输抽象：把「建立一条双向字节流」与「会话跑在这条流上」解耦。
//!
//! 阶段 1 的建立策略是 **hbbr 中继为主路径**（[`crate::signaling::RustDeskTransport`]），
//! 同内网直连为优化。测试用 `tokio::io::duplex` 注入内存替身。
//!
//! # 建立契约（务必遵守）
//!
//! - 中继路径：TCP 连 hbbr 后**恰好发一帧** `RequestRelay`，之后立即裸字节；**没有 ack**，
//!   不要等待任何应答。
//! - 控制器的 `Hello` 帧（[`crate::session::ControllerHandshake::hello_frame`]）由建立方
//!   在 `RequestRelay` **之后、同一次 write** 里一并写出（流水线，省一个 RTT）；被控端
//!   `early` 为空。因此会话循环**不再**发送 `Hello`，只等 `HelloAck`。
//! - 握手必须容忍「刚连上就收到对端 `Hello`」（后到者场景），不得假设严格的先发后收顺序。

use crate::error::RemoteResult;
use async_trait::async_trait;
use rust_tunnel_common::protocol::RemoteParams;
use tokio::io::{AsyncRead, AsyncWrite};

/// 一条双向字节流（中继连接 / 直连连接 / 内存替身）。
pub trait Duplex: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Duplex for T {}

/// 装箱后的双向字节流。
pub type BoxDuplex = Box<dyn Duplex>;

/// 本端在会话中的角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// 控制器（发起方）：发 `Hello`、收画面、回传输入。
    Controller,
    /// 被控端（响应方）：收 `Hello`、采集画面、注入输入。
    Controlled,
}

/// 建立数据面所需的上下文。
#[derive(Debug, Clone)]
pub struct EstablishRequest {
    /// 会话 ID（服务端签发，同时作为中继配对的 `uuid`）。
    pub session_id: String,
    /// 本端 RustDesk ID。
    pub local_rd_id: String,
    /// 对端 RustDesk ID（控制器用于向会合服务器发起中继/打洞信令；被控端可为空）。
    pub peer_rd_id: String,
    /// 会合/中继参数。
    pub params: RemoteParams,
    /// 是否优先尝试同内网直连（失败回退中继）。
    pub prefer_direct: bool,
    /// 中继路径下与 `RequestRelay` 帧写在同一次 write 的前置字节（控制器为预编码 `Hello` 帧）。
    pub early: Vec<u8>,
}

/// 数据面建立器。
#[async_trait]
pub trait TransportEstablisher: Send + Sync + 'static {
    /// 建立一条双向字节流。
    ///
    /// 实现必须遵守模块级「建立契约」：中继路径只发一帧 `RequestRelay` 后转裸字节、不等应答，
    /// 并把 `req.early` 与之合并写出。
    ///
    /// # Errors
    /// 信令失败、连接超时、参数缺失时返回 [`crate::error::RemoteError`]。
    async fn establish(&self, request: EstablishRequest, role: Role) -> RemoteResult<BoxDuplex>;
}
