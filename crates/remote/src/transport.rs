//! 数据面传输抽象：把「建立一条双向字节流」与「会话跑在这条流上」解耦。
//!
//! 建立策略是**三条路径**（[`crate::signaling::RustDeskTransport`] 实现）：
//! **跨 NAT UDP 打洞**（优先，`Punch`）→ **同内网 TCP 直连**（`Direct`）→ **hbbr 中继兜底**
//! （`Relay`，所有 NAT 场景可用）。测试用 `tokio::io::duplex` 注入内存替身。
//!
//! # 建立契约（务必遵守）
//!
//! - 中继路径：TCP 连 hbbr 后**恰好发一帧** `RequestRelay`，之后立即裸字节；**没有 ack**，
//!   不要等待任何应答。
//! - 控制器的 `Hello` 帧（[`crate::session::ControllerHandshake::hello_frame`]）由建立方
//!   在 `RequestRelay` **之后、同一次 write** 里一并写出（流水线，省一个 RTT）；被控端
//!   `early` 为空。因此会话循环**不再**发送 `Hello`，只等 `HelloAck`。**非中继路径**（直连/
//!   打洞）没有中继帧可流水线，建立方要在拿到流后把 `early` 补写进去再返回。
//! - 握手必须容忍「刚连上就收到对端 `Hello`」（后到者场景），不得假设严格的先发后收顺序。
//! - 建立方**必须如实报告实际走的路径**（[`EstablishedStream::path`]）：直连/打洞与中继的延迟
//!   差一个数量级，UI 需要据实展示；「回退中继」尤其不能谎报成直连，`Punch` 也**绝不能**是
//!   「打洞失败后凑出来的」——**谎报路径等于骗用户**。

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
/// 三条路径的往返延迟通常相差一个数量级（中继 > 打洞 > 同内网直连），UI 要据实展示；是否走了
/// 打洞/直连也是用户判断「为什么慢」的关键线索，因此建立方必须如实回报，**不得**把回退中继
/// 谎报成打洞或直连。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportPath {
    /// 同内网 TCP 直连：hbbs 判定双方同内网后回目标端的内网监听地址，控制器 `TcpStream::connect`。
    Direct,
    /// hbbr 中继：跨 NAT 或打洞/直连不可用时的兜底（所有 NAT 场景可用）。
    Relay,
    /// 跨 NAT UDP 打洞成功：经 hbbs 交换双方公网 UDP 映射后对打，走 [`crate::reliable`] 可靠层。
    ///
    /// **必须**只在打洞握手（`SYN`/`SYN|ACK` 双向可达）真正成功后回报；打洞失败回退中继时
    /// 必须报 [`TransportPath::Relay`]。
    Punch,
}

/// 建立完成的一条数据面流：裸字节流 + **实际**走的路径。
pub struct EstablishedStream {
    /// 双向字节流，交给会话循环跑 [`crate::session`] 协议。
    pub stream: BoxDuplex,
    /// 该流实际走的路径（供 UI 展示「打洞 / 直连 / 中继」）。
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
    /// 是否优先尝试打洞/直连（失败回退中继）。
    pub prefer_direct: bool,
    /// 中继路径下与 `RequestRelay` 帧写在同一次 write 的前置字节（控制器为预编码 `Hello` 帧）。
    pub early: Vec<u8>,
    /// 打洞会话标识（= [`crate::punch::conn_id_from_token`]`(token).to_be_bytes()`）。
    ///
    /// # 为什么必须有
    ///
    /// RustDesk 的 `PunchHole` 协议里**没有任何会话标识**——hbbs 只按「请求方地址 ↔ 目标端地址」
    /// 牵线，不携带会话 id。多标签页 / 并发会话下，两端若各自凭地址猜会话就会**串线**。因此打洞
    /// 数据面必须自带一个双方各自算出、且第三方推不出的会话标识：它由服务端经**加密控制通道**
    /// 下发的一次性 `token` 派生（见 [`crate::punch`]），hbbs 的**明文**会合流量里看不到它。
    ///
    /// # 为什么是 `[u8; 8]` 而不是 `String`
    ///
    /// 传的是**派生结果**而不是 `token` 本身：`token` 是数据面 KDF 与握手 HMAC 的秘密，不该为了
    /// 一个会话标识而扩散到更多结构里。8 字节正是可靠层段头里 `conn_id`（`u64`）的宽度。
    ///
    /// 这是 **Rust 内部类型、不上线**（`PunchHole` 报文字段一个都不动）。
    pub punch_cookie: [u8; 8],
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
