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
//! - 建立方**必须如实报告实际走的路径**（[`EstablishedStream::path`]）：直连与中继的延迟
//!   差一个数量级，UI 需要据实展示；「回退中继」尤其不能谎报成直连。

use crate::error::RemoteResult;
use async_trait::async_trait;
use rust_tunnel_common::protocol::RemoteParams;
use tokio::io::{AsyncRead, AsyncWrite};

/// 一条双向字节流（中继连接 / 直连连接 / 内存替身）。
pub trait Duplex: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Duplex for T {}

/// 装箱后的双向字节流。
pub type BoxDuplex = Box<dyn Duplex>;

/// 数据面实际走的路径。
///
/// 直连（同内网 P2P）与中继（hbbr 转发）的往返延迟通常差一个数量级，UI 要据实展示；是否走了
/// 直连也是用户判断「为什么慢」的关键线索，因此建立方必须如实回报，**不得**把回退中继谎报成直连。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportPath {
    /// 同内网 P2P 直连（[`crate::signaling::RustDeskTransport`] 的 `try_direct` 成功）。
    Direct,
    /// hbbr 中继（跨 NAT 或直连不可用时的主路径）。
    Relay,
}

/// 建立完成的一条数据面流：裸字节流 + **实际**走的路径。
pub struct EstablishedStream {
    /// 双向字节流，交给会话循环跑 [`crate::session`] 协议。
    pub stream: BoxDuplex,
    /// 该流实际走的路径（供 UI 展示「直连 / 中继」）。
    pub path: TransportPath,
}

impl std::fmt::Debug for EstablishedStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `BoxDuplex` 不是 `Debug`，只打印路径即可（流本身没有可读信息）。
        f.debug_struct("EstablishedStream")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

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
    /// 预登记（可选钩子）：拿到服务端下发的 [`RemoteParams`] 后、**首个会话之前**调用。
    ///
    /// # 为什么需要这个钩子（别省掉）
    ///
    /// 会合服务器 hbbs 对**未登记**的对端是「静默丢弃」的：
    /// - `RequestRelay` 在目标不在登记表里时只打一条 `debug!` 就丢掉——**不回复、不排队、
    ///   不重试**（`crates/rustdesk/src/rendezvous.rs` 的 `forward_relay_request`）；
    /// - `PunchHoleRequest` 对不存在的 ID 立刻回 `IdNotExist`，于是控制器的
    ///   [`crate::signaling::RustDeskTransport`] 会在**一个 RTT 内**就判定「无直连」并转去
    ///   `notify_relay`。
    ///
    /// 而被控端若等到接受 `RemoteSessionIncoming` 之后才构造传输（要解析 DNS、绑套接字、
    /// 探测本机 IP，还要先等用户/桥接点「接受」），它登记进 hbbs 时控制器的中继请求早已发完
    /// 并被丢弃——被控端随后白等一整个等待窗口才超时。**这不是理论上的竞争，是必现的**：
    /// 控制器只需一个 RTT，被控端要等一次人工/桥接确认。
    ///
    /// 因此实现应当在**本端已具备被控能力时提前完成登记**，让两端在会话发起前就都在线。
    /// 这正是真实 RustDesk 客户端「常驻登记」的做法。
    ///
    /// 默认实现为空操作：测试替身、以及不需要预登记的实现无需关心。
    ///
    /// # Errors
    /// 解析、绑定、登记失败时返回 [`crate::error::RemoteError`]。调用方只记日志、不中断流程
    /// （真正的会话发起会再走一次 [`Self::establish`]）。
    async fn prepare(&self, params: &RemoteParams, local_rd_id: &str) -> RemoteResult<()> {
        let _ = (params, local_rd_id);
        Ok(())
    }

    /// 建立一条双向字节流。
    ///
    /// 实现必须遵守模块级「建立契约」：中继路径只发一帧 `RequestRelay` 后转裸字节、不等应答，
    /// 并把 `req.early` 与之合并写出；返回的 [`EstablishedStream::path`] 必须如实反映实际路径。
    ///
    /// # Errors
    /// 信令失败、连接超时、参数缺失时返回 [`crate::error::RemoteError`]。
    async fn establish(&self, request: EstablishRequest, role: Role) -> RemoteResult<EstablishedStream>;
}
