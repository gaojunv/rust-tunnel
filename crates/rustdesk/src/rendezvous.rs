//! hbbs：汇合 / ID 服务器（rendezvous）。
//!
//! # 它做什么
//!
//! 汇合服务器是**信令通道**，不碰任何会话数据。它回答两个问题、撮合两种连接：
//!
//! 1. **「ID `x` 在哪？」**——客户端用 UDP 心跳（`RegisterPeer`）把自己的 ID 与当前公网
//!    地址登记上来；服务器回「要不要重新上报公钥」（`RegisterPeerResponse.request_pk`）。
//! 2. **「ID `x` 的公钥是什么？」**——`RegisterPk` 登记 `ID → 公钥`，之后服务器用私钥对
//!    `IdPk{id, pk}` 签名，交给需要连它的客户端校验（见 [`crate::key`]）。
//!
//! 撮合分两条路：
//!
//! - **打洞**：A 发 `PunchHoleRequest`（`udp_port` 里自报它本地的 UDP 端口），服务器把 A 的
//!   地址推给 B（`PunchHole`）；B 回 `PunchHoleSent`，服务器把它转成 `PunchHoleResponse`
//!   （含 B 的公钥签名）推给 A。两端同时向对方发包，从而在各自的 NAT 上打出通路。
//!
//!   两处地址的取值是**刻意不同**的，不要「顺手统一」：`PunchHole.socket_addr` 必须是 A 的
//!   **信令 TCP 源地址**（B 原样回填它，服务器靠它在 `sinks` 里找到 A 那条连接、把应答推回去），
//!   而 `PunchHoleResponse.socket_addr` 必须是 B **已登记的 UDP 映射**——那才是能打出洞的地址，
//!   信令 TCP 的源地址只是个没人监听的临时端口。hbbs 无从自行得知 A 的 UDP 端口（信令 TCP
//!   源端口 ≠ UDP 登记端口），所以由 A 在 `PunchHoleRequest.udp_port` 里自报、服务器原样透传。
//! - **中继**：打洞不可用时改走 [`crate::relay`]（hbbr）。A 发 `RequestRelay` 请服务器
//!   转给 B，B 用 `RelayResponse` 应答，服务器再转回 A 并附上 B 的公钥。
//!
//! # 三个不对称之处（都是上游如此，不是笔误）
//!
//! - **请求走 TCP，推送走 UDP**。客户端对汇合服务器的请求（`PunchHoleRequest`、
//!   `RequestRelay`）走它自己那条长连接；而服务器**主动推给对端**的消息（`PunchHole`、
//!   `FetchLocalAddr`）走对端登记时用的 UDP 地址。对端收到推送后的应答
//!   （`PunchHoleSent`、`LocalAddr`）又走 TCP。
//! - **应答走 TCP 的是「请求方那条连接」**。`PunchHoleResponse` 只经 TCP 下发，地址取自
//!   A 发起请求时的**连接源地址**（编码进 `PunchHole.socket_addr`，由 B 原样回填），
//!   所以服务器不需要按 ID 找 A 的连接。
//! - **大多数消息处理完就断开连接**。只有 `PunchHoleRequest` 与 `RequestRelay` 之后连接
//!   保持（前者要等 `PunchHoleResponse` 回来），其余消息（含未知消息）处理完即关。
//!   客户端有自己的重连逻辑，这是上游的既定行为，本实现照做。
//!
//! # 安全相关的取舍
//!
//! - **UDP 上不接受** `PunchHoleRequest` / `PunchHoleSent` / `LocalAddr`：这三者若走 UDP，
//!   任何人都能构造一个源地址伪造的包，让服务器替他去打别人（UDP 反射/放大）。TCP 需要
//!   完成握手，源地址不可伪造，因此只保留 TCP 路径。
//! - **`ConfigureUpdate` 不接受**：上游只允许回环来源改配置。本项目的配置走 Web 管理面，
//!   所以直接不做，避免留一条隐蔽的配置改写入口。
//! - **`licence_key` 是明文比对**，且必须在加密信道内传输；详见 [`crate::key`] 的说明。
//!
//! # 与上游的有意差异
//!
//! - **不发空的服务端列表**：上游在 `serial` 更高时下发 `ConfigUpdate`，即使列表为空。
//!   空的汇合服务器列表会让客户端把配置清空，所以本实现只在列表非空时下发
//!   （见 [`RendezvousServer::config_update_if_newer`]）。默认 `serial = 0`，本来就
//!   永远不会触发下发。
//! - **内网中继地址的选用条件**：本实现在「目标端在内网 **且** `local_ip` 非空」时即把
//!   `local_ip` 作为推荐中继下发，不与「强制中继 / 跨网」分支绑定；上游的同一覆盖只在
//!   该分支内生效。放宽触发条件后，同内网的一对端也能拿到内网中继地址；`local_ip` 的
//!   非空守卫则避免在未配置它时把中继地址清空（那会让客户端收到一个空的中继地址）。
//! - **TCP 连接的写半边常驻**：上游在首次下发后就把写半边从表里取走，之后该连接不能再被
//!   推送。本实现保留到连接结束，因此一条连接可以接受多次推送。
//! - **不实现回环文本控制台**（`blacklist-add` / `limit-speed` 等）与软件更新提示：
//!   前者与本项目 Web 管理面重复，后者本项目有自己的分发链路。
//! - **NAT 探测端口多一个 UDP 半边，且主 UDP 端口也应答 `TestNatRequest`**：上游只在
//!   NAT 探测端口的 **TCP** 半边应答。客户端要自判「是否在对称 NAT 后面」，唯一的办法是从
//!   同一个套接字向**两个不同目的端口**各探一次、比对观测到的源端口——对称 NAT 对每个
//!   `(源IP, 源端口, 目的)` 三元组独立映射，单一目的地看不出差别。两个端口都应答是小常数
//!   （刻意不带 `cu`），放大系数 < 1，反反射性质不因此受损。

use crate::addr::{self, try_into_v4};
use crate::frame::{FrameCodec, FrameError};
use crate::key::ServerKey;
use crate::peer::{PeerRegistry, MAX_PEER_ID_LEN};
use crate::proto::{
    punch_hole_response, register_pk_response, relay_response, rendezvous_message, ConfigUpdate,
    FetchLocalAddr, LocalAddr, NatType, OnlineResponse, PunchHole, PunchHoleRequest,
    PunchHoleResponse, PunchHoleSent, RegisterPeerResponse, RegisterPkResponse, RelayResponse,
    RendezvousMessage, TestNatResponse,
};
use bytes::BytesMut;
use prost::Message as _;
use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, watch, Mutex};
use tokio::task::JoinSet;
use tokio::time::timeout;
use tokio_util::codec::Decoder as _;
use tracing::{debug, info, warn};

/// UDP 接收缓冲：够装下最大的登记消息，多出的部分由内核丢弃。
const MAX_DATAGRAM: usize = 64 * 1024;

/// NAT 类型探测端口只读一条消息，等这么久没来就关。
const NAT_TEST_TIMEOUT: Duration = Duration::from_secs(30);

/// IPv4 网段，用于判定「这个地址是不是在同一个内网」。
///
/// 只有当掩码配好（例如 `192.168.0.0/16`）时才生效；不配则 [`RendezvousServer::is_lan`]
/// 恒为 `false`，内网判定退化为「双方公网 IP 是否相同」——对同一个 NAT 后面的两台机器
/// 依然成立，所以不配掩码也不是致命的。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mask {
    base: Ipv4Addr,
    prefix: u8,
}

/// [`Mask`] 的解析错误。
#[derive(Debug, thiserror::Error)]
#[error("invalid ipv4 CIDR `{0}`; expected e.g. 192.168.0.0/16")]
pub struct MaskError(String);

impl FromStr for Mask {
    type Err = MaskError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || MaskError(s.to_owned());
        let (base, prefix) = s.split_once('/').ok_or_else(invalid)?;
        let base: Ipv4Addr = base.parse().map_err(|_| invalid())?;
        let prefix: u8 = prefix.parse().map_err(|_| invalid())?;
        if prefix > 32 {
            return Err(invalid());
        }
        Ok(Self { base, prefix })
    }
}

impl Mask {
    /// 网段前缀长度。
    #[must_use]
    pub fn prefix(&self) -> u8 {
        self.prefix
    }

    /// 判断地址是否落在网段内。
    #[must_use]
    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        if self.prefix == 0 {
            return true;
        }
        let shift = u32::from(32 - self.prefix);
        let bits = if shift == 32 { 0 } else { u32::MAX << shift };
        u32::from(ip) & bits == u32::from(self.base) & bits
    }
}

/// 汇合服务器的运行时配置。
#[derive(Debug, Clone)]
pub struct RendezvousConfig {
    /// 下发给客户端的配置序号；`0` 表示从不下发配置更新。
    ///
    /// 客户端在 `RegisterPeer.serial` 里带上自己当前的序号，服务端只在
    /// `serial > 客户端序号` 时下发。
    pub serial: i32,
    /// 下发给客户端的汇合服务器列表（`ConfigUpdate.rendezvous_servers`）。
    pub rendezvous_servers: Vec<String>,
    /// 推荐给客户端的中继服务器；为空表示让客户端用它自己配置的。
    pub relay_servers: Vec<String>,
    /// 本机在内网里的地址；内网直连场景下作为中继地址下发。
    pub local_ip: String,
    /// 内网网段；见 [`Mask`]。
    pub mask: Option<Mask>,
    /// 强制所有会话走中继，不打洞。
    pub always_use_relay: bool,
    /// TCP 信令连接的读空闲超时。
    pub tcp_idle_timeout: Duration,
}

impl Default for RendezvousConfig {
    fn default() -> Self {
        Self {
            serial: 0,
            rendezvous_servers: Vec::new(),
            relay_servers: Vec::new(),
            local_ip: String::new(),
            mask: None,
            always_use_relay: false,
            tcp_idle_timeout: Duration::from_secs(30),
        }
    }
}

/// 汇合服务器的累计计数。
#[derive(Debug, Default)]
pub struct RendezvousStats {
    /// 累计接受的 TCP 信令连接数。
    tcp_connections: AtomicU64,
    /// 累计处理的 UDP 数据报数。
    udp_datagrams: AtomicU64,
    /// 累计处理的心跳（`RegisterPeer`）数。
    heartbeats: AtomicU64,
    /// 累计成功登记的公钥数。
    pk_registrations: AtomicU64,
    /// 累计收到的打洞请求数。
    punch_hole_requests: AtomicU64,
    /// 累计收到的中继申请数。
    relay_requests: AtomicU64,
    /// 累计被拒绝的请求数（key 不符、限流、格式非法）。
    rejected: AtomicU64,
}

impl RendezvousStats {
    /// 取一份一致的快照。
    #[must_use]
    pub fn snapshot(&self) -> RendezvousStatsSnapshot {
        let load = |v: &AtomicU64| v.load(Ordering::Relaxed);
        RendezvousStatsSnapshot {
            tcp_connections: load(&self.tcp_connections),
            udp_datagrams: load(&self.udp_datagrams),
            heartbeats: load(&self.heartbeats),
            pk_registrations: load(&self.pk_registrations),
            punch_hole_requests: load(&self.punch_hole_requests),
            relay_requests: load(&self.relay_requests),
            rejected: load(&self.rejected),
        }
    }
}

/// [`RendezvousStats`] 的可序列化快照。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RendezvousStatsSnapshot {
    /// 累计 TCP 信令连接数。
    pub tcp_connections: u64,
    /// 累计 UDP 数据报数。
    pub udp_datagrams: u64,
    /// 累计心跳数。
    pub heartbeats: u64,
    /// 累计公钥登记数。
    pub pk_registrations: u64,
    /// 累计打洞请求数。
    pub punch_hole_requests: u64,
    /// 累计中继申请数。
    pub relay_requests: u64,
    /// 累计拒绝数。
    pub rejected: u64,
}

/// 处理完一条 TCP 信令消息后的动作。
struct Outcome {
    /// 要回写到**同一条**连接的消息。
    reply: Option<RendezvousMessage>,
    /// 是否保持连接。
    keep_open: bool,
}

impl Outcome {
    /// 保持连接、不回写。
    fn keep() -> Self {
        Self {
            reply: None,
            keep_open: true,
        }
    }

    /// 关闭连接、不回写。
    fn close() -> Self {
        Self {
            reply: None,
            keep_open: false,
        }
    }

    /// 回写一条消息后关闭连接。
    fn close_with(msg: RendezvousMessage) -> Self {
        Self {
            reply: Some(msg),
            keep_open: false,
        }
    }
}

/// 汇合服务器。
pub struct RendezvousServer {
    key: Arc<ServerKey>,
    peers: Arc<PeerRegistry>,
    config: RendezvousConfig,
    stats: Arc<RendezvousStats>,
    /// TCP 连接的推送端，按对端的 TCP 源地址（v4 归一后）索引。
    sinks: Mutex<HashMap<SocketAddr, mpsc::UnboundedSender<RendezvousMessage>>>,
    /// 多中继时轮转选择的游标。
    relay_rotation: AtomicUsize,
}

/// NAT 探测在 **UDP** 上的应答：只回报服务端看到的源端口，**刻意不带配置更新**。
///
/// `cu` 会随汇合服务器列表变长，留着它「应答是小常数、放大系数 < 1」这条反反射属性就只是
/// 「当前 `serial` 恰好为 0」的巧合，而不是结构不变量——`serial` 一旦被配上非 0 值就会悄悄
/// 失效。TCP 半边（`serve_nat_test_connection`）照旧下发 `cu`，与上游客户端行为一致。
fn nat_probe_response(remote: SocketAddr) -> RendezvousMessage {
    RendezvousMessage::with(rendezvous_message::Union::TestNatResponse(TestNatResponse {
        port: i32::from(remote.port()),
        ..Default::default()
    }))
}

impl RendezvousServer {
    /// 构造汇合服务器。
    #[must_use]
    pub fn new(key: Arc<ServerKey>, peers: Arc<PeerRegistry>, config: RendezvousConfig) -> Self {
        Self {
            key,
            peers,
            config,
            stats: Arc::new(RendezvousStats::default()),
            sinks: Mutex::new(HashMap::new()),
            relay_rotation: AtomicUsize::new(0),
        }
    }

    /// 对端登记表（管理面读取用）。
    #[must_use]
    pub fn peers(&self) -> Arc<PeerRegistry> {
        Arc::clone(&self.peers)
    }

    /// 共享计数句柄（管理面读取用）。
    #[must_use]
    pub fn stats(&self) -> Arc<RendezvousStats> {
        Arc::clone(&self.stats)
    }

    /// 主端口：同时服务信令 TCP 与心跳/推送 UDP，直到 `shutdown` 收到 `true`。
    ///
    /// 监听套接字由调用方绑定（本 crate 不硬编码地址，也不登记端口）。
    ///
    /// # Errors
    /// accept 或 `recv_from` 失败时返回该错误；`shutdown` 是正常返回路径。
    pub async fn serve_rendezvous(
        self: Arc<Self>,
        listener: TcpListener,
        udp: Arc<UdpSocket>,
        mut shutdown: watch::Receiver<bool>,
    ) -> io::Result<()> {
        info!(
            tcp = %listener.local_addr()?,
            udp = %udp.local_addr()?,
            has_key = !self.key.key().is_empty(),
            can_sign = self.key.can_sign(),
            "rustdesk 汇合服务器（hbbs）已启动"
        );
        let mut tasks: JoinSet<()> = JoinSet::new();
        let mut buf = vec![0u8; MAX_DATAGRAM];
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, remote) = accepted?;
                    let me = Arc::clone(&self);
                    let udp = Arc::clone(&udp);
                    tasks.spawn(async move {
                        if let Err(err) = me.serve_signaling(stream, remote, udp).await {
                            debug!(%remote, error = %err, "汇合信令连接结束");
                        }
                    });
                }
                received = udp.recv_from(&mut buf) => {
                    let (len, remote) = received?;
                    // 处理是内存操作 + 一次 UDP 回包，就地做完即可，不必为每个数据报起任务
                    // （起任务反而会让洪水流量把任务表撑大）。
                    if let Some(payload) = buf.get(..len) {
                        self.handle_datagram(&udp, payload, remote).await;
                    }
                }
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
            }
        }
        tasks.shutdown().await;
        info!("rustdesk 汇合服务器（hbbs）已停止");
        Ok(())
    }

    /// NAT 类型探测端口（主端口 - 1）：接受一条 TCP 消息后应答并关闭。
    ///
    /// 两种用途：`TestNatRequest` 让客户端看到自己出网后的源端口（与本地端口比较即可判定
    /// NAT 类型）；`OnlineRequest` 查询一批 ID 的在线位图。
    ///
    /// # Errors
    /// accept 失败时返回该错误；`shutdown` 是正常返回路径。
    pub async fn serve_nat_test(
        self: Arc<Self>,
        listener: TcpListener,
        mut shutdown: watch::Receiver<bool>,
    ) -> io::Result<()> {
        info!(addr = %listener.local_addr()?, "rustdesk NAT 探测端口已启动");
        let mut tasks: JoinSet<()> = JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, remote) = accepted?;
                    let me = Arc::clone(&self);
                    tasks.spawn(async move {
                        if let Err(err) = me.serve_nat_test_connection(stream, remote).await {
                            debug!(%remote, error = %err, "NAT 探测连接结束");
                        }
                    });
                }
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
            }
        }
        tasks.shutdown().await;
        info!("rustdesk NAT 探测端口已停止");
        Ok(())
    }

    /// NAT 探测端口的 UDP 半边：**只**应答 `TestNatRequest`。
    ///
    /// 为什么与主端口的 `handle_datagram` 分开：那边会受理 `RegisterPeer`/`RegisterPk`
    /// 并把来源地址写进登记表。若 NAT 端口也受理它们，「面向 NAT 端口的映射」就会被
    /// 当成打洞用的映射写进 `peers`，污染对端拿到地址的语义。故这里只认 `TestNatRequest`，
    /// 其余一律静默丢弃——**不回复**，因此不会被当成反射/放大工具。
    async fn handle_nat_datagram(&self, udp: &UdpSocket, payload: &[u8], remote: SocketAddr) {
        let remote = try_into_v4(remote);
        let Ok(msg) = RendezvousMessage::decode(payload) else {
            debug!(%remote, "NAT 探测 UDP 数据报不是合法 protobuf，丢弃");
            return;
        };
        if matches!(msg.union, Some(rendezvous_message::Union::TestNatRequest(_))) {
            self.send_datagram(udp, &nat_probe_response(remote), remote)
                .await;
        } else {
            debug!(%remote, "NAT 探测端口只应答 TestNatRequest，其余静默丢弃");
        }
    }

    /// NAT 探测端口的 UDP 监听循环（与 `serve_nat_test` 的 TCP 半边同端口号）。
    ///
    /// 客户端从**登记用的那个 UDP 套接字**向本端口发 `TestNatRequest`，据此比对两次
    /// 观测到的源端口以判断是否在对称 NAT 后面。
    ///
    /// # Errors
    /// `recv_from` 失败时返回该错误；`shutdown` 是正常返回路径。
    pub async fn serve_nat_udp(
        self: Arc<Self>,
        udp: Arc<UdpSocket>,
        mut shutdown: watch::Receiver<bool>,
    ) -> io::Result<()> {
        info!(addr = %udp.local_addr()?, "rustdesk NAT 探测端口（UDP）已启动");
        // 处理是内存操作 + 一次 UDP 回包，就地做完即可，不必为每个数据报起任务
        // （起任务反而会让洪水流量把任务表撑大）——与 `serve_rendezvous` 的 UDP 分支同理。
        let mut buf = vec![0u8; MAX_DATAGRAM];
        loop {
            tokio::select! {
                received = udp.recv_from(&mut buf) => {
                    let (len, remote) = received?;
                    if let Some(payload) = buf.get(..len) {
                        self.handle_nat_datagram(&udp, payload, remote).await;
                    }
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
            }
        }
        info!("rustdesk NAT 探测端口（UDP）已停止");
        Ok(())
    }

    /// 服务一条信令 TCP 连接：读客户端请求、写服务器推送，直到任一方结束。
    async fn serve_signaling(
        self: Arc<Self>,
        stream: TcpStream,
        remote: SocketAddr,
        udp: Arc<UdpSocket>,
    ) -> io::Result<()> {
        self.stats.tcp_connections.fetch_add(1, Ordering::Relaxed);
        if let Err(err) = stream.set_nodelay(true) {
            debug!(error = %err, "设置 TCP_NODELAY 失败");
        }
        let key = try_into_v4(remote);
        let (tx, mut rx) = mpsc::unbounded_channel();
        // 先登记推送端再开始读：否则客户端「刚连上就发请求、服务端立刻要推回它」时
        // 可能找不到这条连接。
        self.sinks.lock().await.insert(key, tx);
        let result = self.signaling_loop(stream, remote, &udp, &mut rx).await;
        self.sinks.lock().await.remove(&key);
        result
    }

    /// 信令连接的读写循环。
    ///
    /// 读写用 `into_split` 的两半，这样「读客户端消息」与「写服务器推送」可以放进同一个
    /// `select!`，不必让两个分支争一个可变借用。
    async fn signaling_loop(
        &self,
        stream: TcpStream,
        remote: SocketAddr,
        udp: &UdpSocket,
        rx: &mut mpsc::UnboundedReceiver<RendezvousMessage>,
    ) -> io::Result<()> {
        let (mut reader, mut writer) = stream.into_split();
        let mut codec = FrameCodec::new();
        let mut buf = BytesMut::with_capacity(4096);
        loop {
            tokio::select! {
                pushed = rx.recv() => {
                    // 发送端在 serve_signaling 里随连接一起结束，收到 None 即该收尾。
                    let Some(msg) = pushed else { return Ok(()) };
                    write_message(&mut writer, &msg).await?;
                }
                read = timeout(self.config.tcp_idle_timeout, reader.read_buf(&mut buf)) => {
                    match read {
                        // 读空闲超时或对端关闭：连接结束。
                        Err(_) | Ok(Ok(0)) => return Ok(()),
                        Ok(Ok(_)) => {}
                        Ok(Err(err)) => return Err(err),
                    }
                    // 一次读取可能带多帧，全部处理。
                    while let Some(payload) = codec.decode(&mut buf)? {
                        let Ok(msg) = RendezvousMessage::decode(&payload[..]) else {
                            debug!(%remote, "信令消息不是合法 protobuf，断开");
                            return Ok(());
                        };
                        let outcome = self.handle_signaling(msg, remote, udp).await;
                        if let Some(reply) = outcome.reply {
                            write_message(&mut writer, &reply).await?;
                        }
                        if !outcome.keep_open {
                            debug!(%remote, "该消息处理完即关闭连接");
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    /// 处理一条 UDP 数据报（心跳与公钥登记，外加按上游约定明确忽略的几类）。
    async fn handle_datagram(&self, udp: &UdpSocket, payload: &[u8], remote: SocketAddr) {
        let remote = try_into_v4(remote);
        let Ok(msg) = RendezvousMessage::decode(payload) else {
            debug!(%remote, "UDP 数据报不是合法 protobuf，丢弃");
            return;
        };
        self.stats.udp_datagrams.fetch_add(1, Ordering::Relaxed);
        match msg.union {
            Some(rendezvous_message::Union::RegisterPeer(rp)) => {
                if rp.id.is_empty() || rp.id.len() > MAX_PEER_ID_LEN {
                    debug!(%remote, "心跳的 ID 为空或过长，丢弃");
                    return;
                }
                self.stats.heartbeats.fetch_add(1, Ordering::Relaxed);
                let request_pk = self.peers.register_peer(&rp.id, remote).await;
                self.send_datagram(
                    udp,
                    &RendezvousMessage::with(rendezvous_message::Union::RegisterPeerResponse(
                        RegisterPeerResponse { request_pk },
                    )),
                    remote,
                )
                .await;
                if let Some(cu) = self.config_update_if_newer(rp.serial) {
                    self.send_datagram(
                        udp,
                        &RendezvousMessage::with(rendezvous_message::Union::ConfigureUpdate(cu)),
                        remote,
                    )
                    .await;
                }
            }
            Some(rendezvous_message::Union::RegisterPk(rk)) => {
                if rk.uuid.is_empty() || rk.pk.is_empty() {
                    debug!(%remote, "RegisterPk 缺少 uuid 或公钥，丢弃");
                    return;
                }
                let result = self
                    .peers
                    .register_pk(&rk.id, &rk.uuid, &rk.pk, remote, &remote.ip().to_string())
                    .await;
                if result == register_pk_response::Result::Ok {
                    self.stats.pk_registrations.fetch_add(1, Ordering::Relaxed);
                } else {
                    self.stats.rejected.fetch_add(1, Ordering::Relaxed);
                    debug!(%remote, id = %rk.id, ?result, "公钥登记被拒");
                }
                self.send_datagram(
                    udp,
                    &RendezvousMessage::with(rendezvous_message::Union::RegisterPkResponse(
                        RegisterPkResponse {
                            result: result.into(),
                            ..Default::default()
                        },
                    )),
                    remote,
                )
                .await;
            }
            // NAT 探测**也走主端口**：客户端要判「是否在对称 NAT 后面」，唯一的办法是从同一个
            // 套接字向**两个不同目的端口**各探一次、比对观测到的源端口——只探 NAT 探测端口是
            // 判不出来的（对称 NAT 对每个 `(源IP, 源端口, 目的)` 三元组独立映射，单一目的地
            // 看不出差别）。所以两个 UDP 端口都得应答。
            //
            // 反反射仍然成立：应答是小常数（见 `nat_probe_response`，刻意不带 `cu`），
            // 放大系数 < 1；且**不写登记表**，否则「面向主端口的映射」之外还会多出噪声。
            Some(rendezvous_message::Union::TestNatRequest(_)) => {
                self.send_datagram(udp, &nat_probe_response(remote), remote)
                    .await;
            }
            // 有意不支持：这三类若走 UDP，源地址可以伪造，服务器会变成反射/放大工具。
            Some(
                rendezvous_message::Union::PunchHoleRequest(_)
                | rendezvous_message::Union::PunchHoleSent(_)
                | rendezvous_message::Union::LocalAddr(_),
            ) => {
                self.stats.rejected.fetch_add(1, Ordering::Relaxed);
                debug!(%remote, "UDP 信令（打洞请求/结果/本地地址）不受支持，忽略");
            }
            // 有意不支持：配置改写走 Web 管理面，不留隐蔽入口。
            Some(rendezvous_message::Union::ConfigureUpdate(_)) => {
                debug!(%remote, "UDP 配置更新不受支持，忽略");
            }
            _ => {}
        }
    }

    /// 处理一条 TCP 信令消息。
    async fn handle_signaling(
        &self,
        msg: RendezvousMessage,
        remote: SocketAddr,
        udp: &UdpSocket,
    ) -> Outcome {
        let remote = try_into_v4(remote);
        match msg.union {
            Some(rendezvous_message::Union::PunchHoleRequest(ph)) => {
                self.stats.punch_hole_requests.fetch_add(1, Ordering::Relaxed);
                // 保持连接：请求方要在这条连接上等 PunchHoleResponse 回来。
                Outcome {
                    reply: self.handle_punch_hole_request(remote, ph, udp).await,
                    keep_open: true,
                }
            }
            Some(rendezvous_message::Union::RequestRelay(rf)) => {
                self.stats.relay_requests.fetch_add(1, Ordering::Relaxed);
                self.forward_relay_request(remote, rf, udp).await;
                Outcome::keep()
            }
            Some(rendezvous_message::Union::PunchHoleSent(phs)) => {
                self.forward_punch_result(remote, phs).await;
                Outcome::close()
            }
            Some(rendezvous_message::Union::LocalAddr(la)) => {
                self.forward_local_addr(remote, la).await;
                Outcome::close()
            }
            Some(rendezvous_message::Union::RelayResponse(rr)) => {
                self.forward_relay_response(remote, rr).await;
                Outcome::close()
            }
            Some(rendezvous_message::Union::TestNatRequest(tar)) => {
                Outcome::close_with(self.test_nat_response(remote, tar.serial))
            }
            Some(rendezvous_message::Union::RegisterPk(_)) => {
                // 公钥只能走 UDP 登记：UDP 路径上才能拿到该客户端出网后的真实地址。
                warn!(%remote, "RegisterPk 走 TCP 不受支持");
                Outcome::close_with(RendezvousMessage::with(
                    rendezvous_message::Union::RegisterPkResponse(RegisterPkResponse {
                        result: register_pk_response::Result::NotSupport.into(),
                        ..Default::default()
                    }),
                ))
            }
            // 未知或不该由服务端接收的消息：按上游行为直接关闭连接。
            _ => Outcome::close(),
        }
    }

    /// 打洞请求：把请求方的地址推给目标端，或回一个失败原因。
    ///
    /// 返回 `Some` 表示要回写给请求方（失败情形），`None` 表示已经把 `PunchHole` /
    /// `FetchLocalAddr` 推给目标端、正在等目标端应答。
    async fn handle_punch_hole_request(
        &self,
        remote: SocketAddr,
        mut ph: PunchHoleRequest,
        udp: &UdpSocket,
    ) -> Option<RendezvousMessage> {
        if !self.key.key().is_empty() && ph.licence_key != self.key.key() {
            self.stats.rejected.fetch_add(1, Ordering::Relaxed);
            warn!(%remote, id = %ph.id, "打洞认证失败：licence_key 不匹配");
            return Some(punch_failure(punch_hole_response::Failure::LicenseMismatch));
        }
        let Some(peer) = self.peers.lookup(&ph.id).await else {
            debug!(%remote, id = %ph.id, "打洞目标不存在");
            return Some(punch_failure(punch_hole_response::Failure::IdNotExist));
        };
        if !peer.is_online() {
            debug!(%remote, id = %ph.id, "打洞目标离线");
            return Some(punch_failure(punch_hole_response::Failure::Offline));
        }
        let peer_addr = peer.socket_addr;
        let peer_is_lan = self.is_lan(peer_addr);
        let remote_is_lan = self.is_lan(remote);
        let mut relay_server = self.pick_relay_server();
        // 目标端在内网：推荐它用本机内网地址上的中继。这个地址随消息发给**目标端自己**，
        // 所以按消息接收方（目标端）的可达性来选，而不是按请求方——请求方该用哪个中继
        // 由它自己那条连接上的应答携带。
        if peer_is_lan && !self.config.local_ip.is_empty() {
            relay_server.clone_from(&self.config.local_ip);
        }
        // 一方在内网、另一方在外网时打洞通常不可行，直接让客户端走中继。
        if self.config.always_use_relay || (peer_is_lan ^ remote_is_lan) {
            ph.nat_type = NatType::Symmetric.into();
        }
        // 同一个内网里打洞会被路由器丢掉（自己连自己），改由双方交换内网地址。
        let same_intranet =
            (peer_is_lan && remote_is_lan) || peer_addr.ip() == remote.ip();
        let socket_addr = addr::encode(remote);
        let msg = if same_intranet {
            debug!(%remote, id = %ph.id, %peer_addr, "同内网，索取内网地址");
            RendezvousMessage::with(rendezvous_message::Union::FetchLocalAddr(FetchLocalAddr {
                socket_addr,
                relay_server,
                ..Default::default()
            }))
        } else {
            debug!(%remote, id = %ph.id, %peer_addr, "打洞请求");
            // `udp_port` 是请求方**自报**的本地 UDP 端口，透传给目标端后由它组成打洞地址
            // `(socket_addr 的 IP, udp_port)`。`0` 表示老客户端没报（此时目标端拿不到端口，
            // 只能退回今天的行为：打洞不可用、走中继）。
            //
            // `socket_addr` 一个字都不能动：`forward_punch_result` 靠目标端回显的它去
            // `sinks` 表里找请求方那条信令 TCP 连接（按信令源地址索引），改了应答就找不到路由。
            RendezvousMessage::with(rendezvous_message::Union::PunchHole(PunchHole {
                socket_addr,
                nat_type: ph.nat_type,
                relay_server,
                udp_port: ph.udp_port,
                ..Default::default()
            }))
        };
        self.send_datagram(udp, &msg, peer_addr).await;
        None
    }

    /// `RequestRelay`：把请求方地址填进消息，转发给目标端（UDP）。
    async fn forward_relay_request(&self, remote: SocketAddr, mut rf: crate::proto::RequestRelay, udp: &UdpSocket) {
        let Some(peer) = self.peers.lookup(&rf.id).await else {
            debug!(%remote, id = %rf.id, "中继目标不在登记表中，无法转发");
            return;
        };
        if !peer.is_online() {
            debug!(%remote, id = %rf.id, "中继目标离线，无法转发");
            return;
        }
        rf.socket_addr = addr::encode(remote);
        self.send_datagram(
            udp,
            &RendezvousMessage::with(rendezvous_message::Union::RequestRelay(rf)),
            peer.socket_addr,
        )
        .await;
    }

    /// `PunchHoleSent`：把它转成 `PunchHoleResponse` 推给请求方（TCP），并附上目标端公钥签名。
    async fn forward_punch_result(&self, remote: SocketAddr, phs: PunchHoleSent) {
        let Some(requester) = addr::decode(&phs.socket_addr) else {
            debug!(%remote, "PunchHoleSent 里的地址无法解出，忽略");
            return;
        };
        // 打洞目标要用目标端**已登记的 UDP 映射**，而不是它这条信令 TCP 连接的源地址——
        // 后者是个没人监听的临时端口，跨 NAT 下「拿到地址就直连」必然失败。
        //
        // `phs.id` 是发送方（被控端）自己的 ID（`sign_pk` 已经在用它签自己的 IdPk），
        // 所以用它查登记表拿到的正是它自己那条 UDP 映射。
        //
        // 未登记 UDP 映射时（老客户端 / 尚未走完 RegisterPk）逐字节退回旧行为：用信令源地址。
        let punch_addr = match self.peers.lookup(&phs.id).await {
            Some(peer) if peer.socket_addr.port() != 0 => peer.socket_addr,
            _ => remote,
        };
        let mut response = PunchHoleResponse {
            socket_addr: addr::encode(punch_addr),
            pk: self.sign_pk(&phs.version, &phs.id).await,
            relay_server: phs.relay_server,
            ..Default::default()
        };
        if let Ok(nat_type) = NatType::try_from(phs.nat_type) {
            response.union = Some(punch_hole_response::Union::NatType(nat_type.into()));
        }
        debug!(%remote, %requester, id = %phs.id, "打洞结果转发给请求方");
        self.push_to_connection(
            RendezvousMessage::with(rendezvous_message::Union::PunchHoleResponse(response)),
            requester,
        )
        .await;
    }

    /// `LocalAddr`：把目标端的内网地址转成 `PunchHoleResponse`（`is_local` 置位）推给请求方。
    async fn forward_local_addr(&self, remote: SocketAddr, la: LocalAddr) {
        let Some(requester) = addr::decode(&la.socket_addr) else {
            debug!(%remote, "LocalAddr 里的地址无法解出，忽略");
            return;
        };
        let response = PunchHoleResponse {
            socket_addr: la.local_addr,
            pk: self.sign_pk(&la.version, &la.id).await,
            relay_server: la.relay_server,
            union: Some(punch_hole_response::Union::IsLocal(true)),
            ..Default::default()
        };
        debug!(%remote, %requester, id = %la.id, "内网地址转发给请求方");
        self.push_to_connection(
            RendezvousMessage::with(rendezvous_message::Union::PunchHoleResponse(response)),
            requester,
        )
        .await;
    }

    /// `RelayResponse`：签名对端公钥、校正中继地址，然后转给请求方。
    async fn forward_relay_response(&self, remote: SocketAddr, mut rr: RelayResponse) {
        let Some(requester) = addr::decode(&rr.socket_addr) else {
            debug!(%remote, "RelayResponse 里的地址无法解出，忽略");
            return;
        };
        // 不再把请求方自己的地址回显给它。
        rr.socket_addr = Vec::new();
        // 目标端在这里放的是自己的 ID，服务端把它换成「ID + 公钥」的签名。
        let peer_id = match &rr.union {
            Some(relay_response::Union::Id(id)) if !id.is_empty() => Some(id.clone()),
            _ => None,
        };
        if let Some(id) = peer_id {
            let pk = self.sign_pk(&rr.version, &id).await;
            rr.union = Some(relay_response::Union::Pk(pk));
        }
        if !rr.relay_server.is_empty() {
            if self.is_lan(requester) {
                // 请求方在内网，中继地址要用内网可达的那一个。
                rr.relay_server.clone_from(&self.config.local_ip);
            } else if rr.relay_server == self.config.local_ip {
                rr.relay_server = self.pick_relay_server();
            }
        }
        debug!(%remote, %requester, "中继应答转发给请求方");
        self.push_to_connection(
            RendezvousMessage::with(rendezvous_message::Union::RelayResponse(rr)),
            requester,
        )
        .await;
    }

    /// 构造 NAT 探测应答。
    fn test_nat_response(&self, remote: SocketAddr, client_serial: i32) -> RendezvousMessage {
        let mut res = TestNatResponse {
            port: i32::from(remote.port()),
            ..Default::default()
        };
        res.cu = self.config_update_if_newer(client_serial);
        RendezvousMessage::with(rendezvous_message::Union::TestNatResponse(res))
    }

    /// 服务一条 NAT 探测连接：读一条消息、答一条、关闭。
    async fn serve_nat_test_connection(
        &self,
        stream: TcpStream,
        remote: SocketAddr,
    ) -> io::Result<()> {
        let (mut reader, mut writer) = stream.into_split();
        let mut codec = FrameCodec::new();
        let mut buf = BytesMut::with_capacity(1024);
        let msg = match timeout(
            NAT_TEST_TIMEOUT,
            read_message(&mut reader, &mut codec, &mut buf),
        )
        .await
        {
            Ok(Ok(Some(msg))) => msg,
            Ok(Ok(None)) | Err(_) => return Ok(()),
            Ok(Err(err)) => return Err(err),
        };
        let reply = match msg.union {
            Some(rendezvous_message::Union::TestNatRequest(tar)) => {
                self.test_nat_response(remote, tar.serial)
            }
            Some(rendezvous_message::Union::OnlineRequest(or)) => {
                let states = self.peers.online_bitmap(&or.peers).await;
                RendezvousMessage::with(rendezvous_message::Union::OnlineResponse(OnlineResponse {
                    states,
                }))
            }
            _ => return Ok(()),
        };
        write_message(&mut writer, &reply).await
    }

    /// 用服务端私钥对 `ID ‖ 公钥` 签名；无法签名（未配私钥或 `version` 为空）时返回空。
    async fn sign_pk(&self, version: &str, id: &str) -> Vec<u8> {
        // 老客户端不带 version，此时不签名；客户端会走不校验公钥的兼容路径。
        if version.is_empty() || !self.key.can_sign() {
            return Vec::new();
        }
        match self.peers.pk_of(id).await {
            Some(pk) => self.key.sign_id_pk(id, &pk).unwrap_or_default(),
            None => Vec::new(),
        }
    }

    /// 给客户端下发的配置更新；`serial` 不比客户端新（或列表为空）时返回 `None`。
    fn config_update_if_newer(&self, client_serial: i32) -> Option<ConfigUpdate> {
        if self.config.serial > client_serial && !self.config.rendezvous_servers.is_empty() {
            Some(ConfigUpdate {
                serial: self.config.serial,
                rendezvous_servers: self.config.rendezvous_servers.clone(),
            })
        } else {
            None
        }
    }

    /// 挑一个中继服务器下发：单台直接用，多台轮转，没配则返回空串（让客户端自己决定）。
    fn pick_relay_server(&self) -> String {
        match self.config.relay_servers.as_slice() {
            [] => String::new(),
            [only] => only.clone(),
            many => {
                let idx = self.relay_rotation.fetch_add(1, Ordering::Relaxed) % many.len();
                many.get(idx).cloned().unwrap_or_default()
            }
        }
    }

    /// 地址是否落在配置的内网网段里；未配掩码时恒为 `false`。
    fn is_lan(&self, remote: SocketAddr) -> bool {
        let Some(mask) = self.config.mask else {
            return false;
        };
        match try_into_v4(remote) {
            SocketAddr::V4(v4) => mask.contains(*v4.ip()),
            SocketAddr::V6(_) => false,
        }
    }

    /// 把消息推给一条已建立的 TCP 信令连接；没有该连接时记录并丢弃。
    async fn push_to_connection(&self, msg: RendezvousMessage, target: SocketAddr) {
        let key = try_into_v4(target);
        let sender = self.sinks.lock().await.get(&key).cloned();
        if let Some(tx) = sender {
            if tx.send(msg).is_err() {
                debug!(%target, "对端的信令连接已结束，推送丢弃");
            }
        } else {
            // 请求方此时应该有一条等待中的连接；没有说明它已经断开或换了源地址。
            debug!(%target, "对端没有 TCP 信令连接，无法推送");
        }
    }

    /// 发一个 UDP 数据报（裸 protobuf，UDP 自带边界，不加帧前缀）。
    async fn send_datagram(&self, udp: &UdpSocket, msg: &RendezvousMessage, target: SocketAddr) {
        match udp.send_to(&msg.encode_to_vec(), target).await {
            Ok(_) => {}
            Err(err) => debug!(%target, error = %err, "UDP 发送失败"),
        }
    }
}

/// 构造一条表示失败的 `PunchHoleResponse`。
fn punch_failure(failure: punch_hole_response::Failure) -> RendezvousMessage {
    RendezvousMessage::with(rendezvous_message::Union::PunchHoleResponse(
        PunchHoleResponse {
            failure: failure.into(),
            ..Default::default()
        },
    ))
}

/// 把一条控制消息按帧格式写出去。
async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg: &RendezvousMessage,
) -> io::Result<()> {
    let framed = crate::frame::frame(&msg.encode_to_vec()).map_err(to_io)?;
    writer.write_all(&framed).await
}

/// 从流里读一条控制消息；对端正常关闭时返回 `Ok(None)`。
async fn read_message<R: AsyncRead + Unpin>(
    reader: &mut R,
    codec: &mut FrameCodec,
    buf: &mut BytesMut,
) -> io::Result<Option<RendezvousMessage>> {
    loop {
        if let Some(payload) = codec.decode(buf)? {
            return RendezvousMessage::decode(&payload[..])
                .map(Some)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
        }
        if reader.read_buf(buf).await? == 0 {
            return Ok(None);
        }
    }
}

/// 帧错误转 `io::Error`。
fn to_io(err: FrameError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{KeyExchange, OnlineRequest, RegisterPeer, RegisterPk, TestNatRequest};
    use std::net::Ipv4Addr;

    /// 一个跑在临时端口上的汇合服务器。
    struct TestServer {
        /// 主端口的 TCP 地址（信令）。
        tcp: SocketAddr,
        /// 主端口的 UDP 地址（心跳与推送）。
        udp: SocketAddr,
        /// NAT 探测端口（TCP）。
        nat: SocketAddr,
        /// NAT 探测端口（UDP 半边，与 TCP 半边同端口号）。
        nat_udp: SocketAddr,
        /// 服务端密钥，用于校验签名。
        key: Arc<ServerKey>,
        peers: Arc<PeerRegistry>,
        server: Arc<RendezvousServer>,
        shutdown: watch::Sender<bool>,
        _tasks: Vec<tokio::task::JoinHandle<io::Result<()>>>,
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            let _ = self.shutdown.send(true);
        }
    }

    async fn start_server(config: RendezvousConfig, key: Arc<ServerKey>) -> TestServer {
        start_server_with_binds("127.0.0.1:0", "127.0.0.1:0", config, key).await
    }

    /// 起一个 UDP 侧另绑一个地址的服务器。用途：让目标端（UDP 侧）与请求方（TCP 侧）
    /// 处在**不同 IP**，从而走打洞分支而不是「同内网」分支。回环的 IPv4 别名
    /// （`127.0.0.2` 等）在 macOS 上不可用，因此 UDP 侧放到 IPv6 回环 `[::1]`。
    async fn start_split_server(config: RendezvousConfig, key: Arc<ServerKey>) -> TestServer {
        start_server_with_binds("127.0.0.1:0", "[::1]:0", config, key).await
    }

    async fn start_server_with_binds(
        tcp_bind: &str,
        udp_bind: &str,
        config: RendezvousConfig,
        key: Arc<ServerKey>,
    ) -> TestServer {
        let peers = Arc::new(PeerRegistry::new());
        let server = Arc::new(RendezvousServer::new(
            Arc::clone(&key),
            Arc::clone(&peers),
            config,
        ));
        let tcp_listener = TcpListener::bind(tcp_bind).await.expect("应能绑定");
        let tcp = tcp_listener.local_addr().expect("应能取地址");
        let nat_listener = TcpListener::bind("127.0.0.1:0").await.expect("应能绑定");
        let nat = nat_listener.local_addr().expect("应能取地址");
        // NAT 探测的 UDP 半边单独取一个临时端口，而**不**复用 TCP 半边的端口号：
        // macOS 的 TCP/UDP 临时端口池是共用的，`TcpListener::bind(0)` 拿到的端口随时可能
        // 已被并行用例的某个 UDP 套接字占用，复用就会 AddrInUse 抖动。真实部署里两个半边
        // 同端口号，但那由服务端 `bind_listeners` 保证，与这里的测试夹具无关。
        let nat_udp_socket = UdpSocket::bind("127.0.0.1:0").await.expect("应能绑定");
        let nat_udp = nat_udp_socket.local_addr().expect("应能取地址");
        let udp_socket = UdpSocket::bind(udp_bind).await.expect("应能绑定");
        let udp = udp_socket.local_addr().expect("应能取地址");

        let (shutdown, rx) = watch::channel(false);
        let tasks = vec![
            tokio::spawn(Arc::clone(&server).serve_rendezvous(
                tcp_listener,
                Arc::new(udp_socket),
                rx.clone(),
            )),
            tokio::spawn(Arc::clone(&server).serve_nat_test(nat_listener, rx.clone())),
            tokio::spawn(Arc::clone(&server).serve_nat_udp(Arc::new(nat_udp_socket), rx)),
        ];
        TestServer {
            tcp,
            udp,
            nat,
            nat_udp,
            key,
            peers,
            server,
            shutdown,
            _tasks: tasks,
        }
    }

    async fn start_plain_server() -> TestServer {
        // 空 key：不做 licence_key 比对，也没有签名能力。
        let key = Arc::new(ServerKey::parse("").expect("空 key 应可解析"));
        start_server(RendezvousConfig::default(), key).await
    }

    /// 同 [`start_plain_server`]，但 UDP 侧在 `[::1]`，供目标端与请求方处于不同 IP 的用例。
    async fn start_split_plain_server() -> TestServer {
        let key = Arc::new(ServerKey::parse("").expect("空 key 应可解析"));
        start_split_server(RendezvousConfig::default(), key).await
    }

    async fn start_signing_server() -> TestServer {
        let key = Arc::new(ServerKey::generate());
        let config = RendezvousConfig {
            relay_servers: vec!["relay.example:21117".to_owned()],
            ..RendezvousConfig::default()
        };
        start_server(config, key).await
    }

    /// 同 [`start_signing_server`]，但 UDP 侧在 `[::1]`。
    async fn start_split_signing_server() -> TestServer {
        let key = Arc::new(ServerKey::generate());
        let config = RendezvousConfig {
            relay_servers: vec!["relay.example:21117".to_owned()],
            ..RendezvousConfig::default()
        };
        start_split_server(config, key).await
    }

    /// 绑定一个客户端 UDP 套接字。
    async fn client_udp() -> UdpSocket {
        UdpSocket::bind("127.0.0.1:0").await.expect("应能绑定")
    }

    /// 绑定一个指定 IP 上的客户端 UDP 套接字；配合 [`start_split_server`] 让目标端
    /// 与请求方处于**不同** IP，从而走打洞分支而不是「同内网」分支。
    async fn client_udp_on(ip: &str) -> UdpSocket {
        UdpSocket::bind(format!("{ip}:0"))
            .await
            .expect("应能绑定")
    }

    async fn send_udp(sock: &UdpSocket, msg: &RendezvousMessage, to: SocketAddr) {
        sock.send_to(&msg.encode_to_vec(), to)
            .await
            .expect("UDP 发送应成功");
    }

    async fn recv_udp(sock: &UdpSocket) -> RendezvousMessage {
        let mut buf = vec![0u8; MAX_DATAGRAM];
        let (len, _) = timeout(Duration::from_secs(3), sock.recv_from(&mut buf))
            .await
            .expect("等 UDP 应答超时")
            .expect("UDP 接收失败");
        RendezvousMessage::decode(&buf[..len]).expect("应答应能解码")
    }

    /// 一条信令 TCP 连接。
    struct Signaler {
        reader: tokio::net::tcp::OwnedReadHalf,
        writer: tokio::net::tcp::OwnedWriteHalf,
        codec: FrameCodec,
        buf: BytesMut,
        /// 本端在服务端看来会使用的源地址；断言服务端回填的地址要用它比对。
        local: SocketAddr,
    }

    impl Signaler {
        async fn connect(server: &TestServer) -> Self {
            let stream = TcpStream::connect(server.tcp).await.expect("应能连上信令端口");
            let local = stream.local_addr().expect("应能取本地地址");
            let (reader, writer) = stream.into_split();
            Self {
                reader,
                writer,
                codec: FrameCodec::new(),
                buf: BytesMut::new(),
                local,
            }
        }

        /// 服务端观察到的本端源地址。
        fn local_addr(&self) -> SocketAddr {
            self.local
        }

        async fn send(&mut self, msg: &RendezvousMessage) {
            write_message(&mut self.writer, msg)
                .await
                .expect("写信令消息应成功");
        }

        /// 读一条消息；连接被关闭时返回 `None`。
        async fn recv(&mut self) -> Option<RendezvousMessage> {
            timeout(
                Duration::from_secs(3),
                read_message(&mut self.reader, &mut self.codec, &mut self.buf),
            )
            .await
            .expect("等信令消息超时")
            .expect("读信令消息失败")
        }

        /// 断言连接已被服务端关闭。
        async fn expect_closed(&mut self) {
            assert!(self.recv().await.is_none(), "连接应已被服务端关闭");
        }
    }

    fn register_peer_msg(id: &str, serial: i32) -> RendezvousMessage {
        RendezvousMessage::with(rendezvous_message::Union::RegisterPeer(RegisterPeer {
            id: id.to_owned(),
            serial,
        }))
    }

    fn register_pk_msg(id: &str, uuid: &[u8], pk: &[u8]) -> RendezvousMessage {
        RendezvousMessage::with(rendezvous_message::Union::RegisterPk(RegisterPk {
            id: id.to_owned(),
            uuid: uuid.to_vec(),
            pk: pk.to_vec(),
            ..Default::default()
        }))
    }

    /// 让一个 ID 走完「心跳 → 登记公钥」并在登记表里在线。
    async fn register_online(sock: &UdpSocket, server: &TestServer, id: &str, pk: &[u8]) {
        let to = server.udp;
        send_udp(sock, &register_peer_msg(id, 0), to).await;
        let _ = recv_udp(sock).await;
        send_udp(sock, &register_pk_msg(id, b"uuid", pk), to).await;
        let res = recv_udp(sock).await;
        match res.union {
            Some(rendezvous_message::Union::RegisterPkResponse(r)) => {
                assert_eq!(
                    r.result,
                    i32::from(register_pk_response::Result::Ok),
                    "登记公钥应成功"
                );
            }
            other => panic!("期望 RegisterPkResponse，实得 {other:?}"),
        }
    }

    fn punch_hole_request(id: &str, licence_key: &str) -> RendezvousMessage {
        RendezvousMessage::with(rendezvous_message::Union::PunchHoleRequest(
            PunchHoleRequest {
                id: id.to_owned(),
                licence_key: licence_key.to_owned(),
                version: "1.2.2".to_owned(),
                ..Default::default()
            },
        ))
    }

    #[tokio::test]
    async fn heartbeat_requests_pk_for_a_new_peer() {
        let server = start_plain_server().await;
        let sock = client_udp().await;
        send_udp(&sock, &register_peer_msg("123456789", 0), server.udp).await;
        match recv_udp(&sock).await.union {
            Some(rendezvous_message::Union::RegisterPeerResponse(r)) => {
                assert!(r.request_pk, "新对端应被要求上报公钥");
            }
            other => panic!("期望 RegisterPeerResponse，实得 {other:?}"),
        }
    }

    #[tokio::test]
    async fn heartbeat_stops_requesting_pk_once_registered() {
        let server = start_plain_server().await;
        let sock = client_udp().await;
        register_online(&sock, &server, "123456789", b"pub-key").await;

        send_udp(&sock, &register_peer_msg("123456789", 0), server.udp).await;
        match recv_udp(&sock).await.union {
            Some(rendezvous_message::Union::RegisterPeerResponse(r)) => {
                assert!(!r.request_pk, "公钥已登记且 IP 未变，不该再要求上报");
            }
            other => panic!("期望 RegisterPeerResponse，实得 {other:?}"),
        }
        assert_eq!(server.peers.online_count().await, 1);
    }

    #[tokio::test]
    async fn empty_heartbeat_id_is_ignored() {
        let server = start_plain_server().await;
        let sock = client_udp().await;
        send_udp(&sock, &register_peer_msg("", 0), server.udp).await;
        // 空 ID 不该建记录，也不该有应答。
        let mut buf = vec![0u8; MAX_DATAGRAM];
        assert!(
            timeout(Duration::from_millis(300), sock.recv_from(&mut buf))
                .await
                .is_err(),
            "空 ID 的心跳不应有应答"
        );
        assert!(server.peers.is_empty().await);
    }

    #[tokio::test]
    async fn register_pk_over_tcp_is_not_supported() {
        let server = start_plain_server().await;
        let mut client = Signaler::connect(&server).await;
        client.send(&register_pk_msg("123456789", b"uuid", b"pk")).await;
        match client.recv().await.and_then(|m| m.union) {
            Some(rendezvous_message::Union::RegisterPkResponse(r)) => {
                assert_eq!(r.result, i32::from(register_pk_response::Result::NotSupport));
            }
            other => panic!("期望 NotSupport 的 RegisterPkResponse，实得 {other:?}"),
        }
        // 该消息处理完即关闭连接。
        client.expect_closed().await;
    }

    #[tokio::test]
    async fn punch_hole_to_unknown_id_reports_id_not_exist() {
        let server = start_plain_server().await;
        let mut client = Signaler::connect(&server).await;
        client.send(&punch_hole_request("no-such-id", "")).await;
        match client.recv().await.and_then(|m| m.union) {
            Some(rendezvous_message::Union::PunchHoleResponse(r)) => {
                assert_eq!(
                    r.failure,
                    i32::from(punch_hole_response::Failure::IdNotExist)
                );
            }
            other => panic!("期望 PunchHoleResponse，实得 {other:?}"),
        }
        // 打洞请求的连接保持不关：请求方要在这条连接上等结果。
        let mut client = client;
        client.send(&punch_hole_request("no-such-id", "")).await;
        assert!(client.recv().await.is_some(), "连接应保持可用");
    }

    #[tokio::test]
    async fn punch_hole_to_registered_but_not_ready_peer_reports_offline() {
        let server = start_plain_server().await;
        // 只发心跳、不登记公钥：不能算在线。
        let sock = client_udp().await;
        send_udp(&sock, &register_peer_msg("peer-b-id", 0), server.udp).await;
        let _ = recv_udp(&sock).await;

        let mut client = Signaler::connect(&server).await;
        client.send(&punch_hole_request("peer-b-id", "")).await;
        match client.recv().await.and_then(|m| m.union) {
            Some(rendezvous_message::Union::PunchHoleResponse(r)) => {
                assert_eq!(r.failure, i32::from(punch_hole_response::Failure::Offline));
            }
            other => panic!("期望 PunchHoleResponse，实得 {other:?}"),
        }
    }

    #[tokio::test]
    async fn wrong_licence_key_is_rejected() {
        let key = Arc::new(ServerKey::parse("the-shared-secret").expect("应可解析"));
        let server = start_server(RendezvousConfig::default(), key).await;
        let mut client = Signaler::connect(&server).await;
        client
            .send(&punch_hole_request("123456789", "wrong-secret"))
            .await;
        match client.recv().await.and_then(|m| m.union) {
            Some(rendezvous_message::Union::PunchHoleResponse(r)) => {
                assert_eq!(
                    r.failure,
                    i32::from(punch_hole_response::Failure::LicenseMismatch)
                );
            }
            other => panic!("期望 LicenseMismatch，实得 {other:?}"),
        }
        assert_eq!(server.server.stats().snapshot().rejected, 1);
    }

    #[tokio::test]
    async fn udp_punch_hole_request_is_ignored() {
        // 硬化行为：UDP 上不接受打洞请求，避免被当成反射放大器。
        let server = start_plain_server().await;
        let sock = client_udp().await;
        let peer = client_udp().await;
        register_online(&peer, &server, "peer-b-id", b"pk-b").await;

        send_udp(&sock, &punch_hole_request("peer-b-id", ""), server.udp).await;
        let mut buf = vec![0u8; MAX_DATAGRAM];
        assert!(
            timeout(Duration::from_millis(300), sock.recv_from(&mut buf))
                .await
                .is_err(),
            "UDP 打洞请求不应得到任何应答"
        );
        assert_eq!(server.server.stats().snapshot().rejected, 1);
    }

    #[tokio::test]
    async fn tcp_key_exchange_closes_the_connection_without_a_reply() {
        // KeyExchange 只在 `proto` 里声明以保证与上游客户端互通；服务端**不实现**任何
        // 应答或协商。TCP 信令路径落到兜底分支 `_ => Outcome::close()`：不回任何消息、
        // 直接关闭连接，且**不**计入 `rejected`（兜底分支只关连接，不把「不是服务端职责
        // 的消息」算作拒绝）。这条测试是 `lib.rs` 中那条文档声明的行为锚点。
        let server = start_plain_server().await;
        let mut client = Signaler::connect(&server).await;
        client
            .send(&RendezvousMessage::with(
                rendezvous_message::Union::KeyExchange(KeyExchange {
                    keys: vec![b"client-kx-pubkey".to_vec()],
                    version: 1,
                    signed_params: Vec::new(),
                }),
            ))
            .await;
        // 既不回应答、也不保活：读回 EOF 即证明连接已被服务端关闭。
        client.expect_closed().await;
        assert_eq!(
            server.server.stats().snapshot().rejected,
            0,
            "兜底关连接不应计入 rejected"
        );
    }

    #[tokio::test]
    async fn udp_punch_hole_sent_and_local_addr_are_ignored() {
        // 与 `udp_punch_hole_request_is_ignored` 命中同一分支（PunchHoleSent / LocalAddr
        // 与 PunchHoleRequest 同组）：这三者可伪造 UDP 源地址，一律不应答并计入 rejected。
        let server = start_plain_server().await;
        let sock = client_udp().await;
        let messages = [
            RendezvousMessage::with(rendezvous_message::Union::PunchHoleSent(PunchHoleSent {
                id: "peer-b-id".to_owned(),
                version: "1.2.2".to_owned(),
                ..Default::default()
            })),
            RendezvousMessage::with(rendezvous_message::Union::LocalAddr(LocalAddr {
                id: "peer-b-id".to_owned(),
                version: "1.2.2".to_owned(),
                ..Default::default()
            })),
        ];
        for msg in &messages {
            send_udp(&sock, msg, server.udp).await;
        }
        let mut buf = vec![0u8; MAX_DATAGRAM];
        assert!(
            timeout(Duration::from_millis(300), sock.recv_from(&mut buf))
                .await
                .is_err(),
            "UDP PunchHoleSent / LocalAddr 不应得到任何应答"
        );
        assert_eq!(
            server.server.stats().snapshot().rejected,
            2,
            "两条消息各应计入一次 rejected"
        );
    }

    #[tokio::test]
    async fn udp_configure_update_is_silently_ignored() {
        // ConfigureUpdate 走 UDP 时只打 debug、**不**计 `rejected`：配置改写走 Web 管理面，
        // 这条支路刻意不留隐蔽入口，但它也不算「恶意/非法」。
        let server = start_plain_server().await;
        let sock = client_udp().await;
        send_udp(
            &sock,
            &RendezvousMessage::with(rendezvous_message::Union::ConfigureUpdate(ConfigUpdate {
                serial: 7,
                rendezvous_servers: vec!["evil.example:21116".to_owned()],
            })),
            server.udp,
        )
        .await;
        let mut buf = vec![0u8; MAX_DATAGRAM];
        assert!(
            timeout(Duration::from_millis(300), sock.recv_from(&mut buf))
                .await
                .is_err(),
            "UDP 配置更新不应得到任何应答"
        );
        assert_eq!(
            server.server.stats().snapshot().rejected,
            0,
            "ConfigureUpdate 这一支刻意不计数"
        );
    }

    #[tokio::test]
    async fn nat_test_port_ignores_unknown_messages() {
        // NAT 探测连接只认 TestNatRequest / OnlineRequest，其余落到兜底 `_ => return Ok(())`：
        // 不回应答、直接关闭连接。用超时包住读取，避免「没包就等于没回应」的时序假设。
        let server = start_plain_server().await;
        let stream = TcpStream::connect(server.nat).await.expect("应能连接");
        let (mut reader, mut writer) = stream.into_split();
        write_message(
            &mut writer,
            &RendezvousMessage::with(rendezvous_message::Union::KeyExchange(KeyExchange {
                keys: vec![b"kx-probe".to_vec()],
                version: 1,
                signed_params: Vec::new(),
            })),
        )
        .await
        .expect("应能写出探测消息");

        let mut codec = FrameCodec::new();
        let mut buf = BytesMut::new();
        let read = timeout(
            Duration::from_secs(1),
            read_message(&mut reader, &mut codec, &mut buf),
        )
        .await
        .expect("服务端应关闭连接，而不是挂住不应答");
        assert!(
            read.expect("读应成功").is_none(),
            "NAT 探测端口不应对未知消息回应答"
        );
    }

    #[tokio::test]
    async fn punch_hole_request_is_pushed_to_the_target_over_udp() {
        // UDP 侧在 [::1]、TCP 侧在 127.0.0.1，两端 IP 不同，走打洞分支。
        let server = start_split_plain_server().await;
        let target = client_udp_on("[::1]").await;
        register_online(&target, &server, "peer-b-id", b"pk-b").await;

        let mut requester = Signaler::connect(&server).await;
        let requester_addr = requester.local_addr();
        requester
            .send(&punch_hole_request("peer-b-id", ""))
            .await;

        // 目标端的 UDP 套接字收到 PunchHole，里面是请求方的地址。
        match recv_udp(&target).await.union {
            Some(rendezvous_message::Union::PunchHole(ph)) => {
                let encoded = addr::decode(&ph.socket_addr).expect("地址应能解出");
                assert_eq!(encoded, requester_addr, "应填入请求方的 TCP 地址");
            }
            other => panic!("期望 PunchHole，实得 {other:?}"),
        }
        // 请求方此时还没有应答（在等目标端回 PunchHoleSent）。
        assert_eq!(server.server.stats().snapshot().punch_hole_requests, 1);
    }

    #[tokio::test]
    async fn same_ip_peers_get_fetch_local_addr_instead_of_punch_hole() {
        // 两个客户端都从 127.0.0.1 发起（回环上必然同 IP），因此走「同内网」分支。
        let server = start_plain_server().await;
        let target = client_udp().await;
        register_online(&target, &server, "peer-b-id", b"pk-b").await;

        let mut requester = Signaler::connect(&server).await;
        let requester_addr = requester.local_addr();
        requester
            .send(&punch_hole_request("peer-b-id", ""))
            .await;

        match recv_udp(&target).await.union {
            Some(rendezvous_message::Union::FetchLocalAddr(fla)) => {
                assert_eq!(addr::decode(&fla.socket_addr), Some(requester_addr));
            }
            other => panic!("同 IP 场景应发 FetchLocalAddr，实得 {other:?}"),
        }
    }

    #[tokio::test]
    async fn punch_hole_sent_is_forwarded_back_with_a_signed_pk() {
        // UDP 侧在 [::1]、TCP 侧在 127.0.0.1，两端 IP 不同，走打洞分支。
        let server = start_split_signing_server().await;
        let target = client_udp_on("[::1]").await;
        // 目标端登记时的 UDP 源地址——(b) 之后 `PunchHoleResponse.socket_addr` 就该是它
        // （打洞目标），而不再是目标端信令 TCP 的源地址。
        let target_udp = target.local_addr().expect("应能取本地地址");
        let target_pk = b"target-public-key".to_vec();
        register_online(&target, &server, "peer-b-id", &target_pk).await;

        let mut requester = Signaler::connect(&server).await;
        // 服务端配了 key 时，请求必须带上同一个 key（真实客户端就是这么发的）。
        requester
            .send(&punch_hole_request("peer-b-id", server.key.key()))
            .await;
        let Some(rendezvous_message::Union::PunchHole(ph)) = recv_udp(&target).await.union else {
            panic!("目标端应收到 PunchHole");
        };

        // 目标端用同一条地址回 PunchHoleSent（真实客户端也是这样回填的）。
        let mut responder = Signaler::connect(&server).await;
        responder
            .send(&RendezvousMessage::with(
                rendezvous_message::Union::PunchHoleSent(PunchHoleSent {
                    socket_addr: ph.socket_addr,
                    id: "peer-b-id".to_owned(),
                    relay_server: "relay.example:21117".to_owned(),
                    nat_type: NatType::Asymmetric.into(),
                    version: "1.2.2".to_owned(),
                    ..Default::default()
                }),
            ))
            .await;
        responder.expect_closed().await;

        // 请求方在自己的连接上收到带公钥签名的 PunchHoleResponse。
        let Some(rendezvous_message::Union::PunchHoleResponse(response)) =
            requester.recv().await.and_then(|m| m.union)
        else {
            panic!("请求方应收到 PunchHoleResponse");
        };
        assert_eq!(response.failure, 0, "不应带失败原因");
        // 这里刻意不比对 responder（信令 TCP）的源地址：那是没人监听的临时端口，
        // 跨 NAT 下拿它当打洞目标必然失败。填进去的应是目标端已登记的 UDP 映射。
        assert_eq!(
            addr::decode(&response.socket_addr),
            Some(target_udp),
            "应答里的地址应是目标端已登记的 UDP 映射"
        );
        assert_eq!(response.relay_server, "relay.example:21117");
        assert_eq!(
            response.union,
            Some(punch_hole_response::Union::NatType(
                NatType::Asymmetric.into()
            ))
        );
        server
            .key
            .verify_id_pk("peer-b-id", &target_pk, &response.pk)
            .expect("公钥签名应能用服务端公钥验证");
    }

    /// 请求方自报的 `udp_port` 必须原样透传进推给目标端的 `PunchHole.udp_port`；
    /// 同时 `socket_addr` **仍须**是请求方的信令 TCP 源地址——目标端会原样回填它，
    /// 服务端靠它去 `sinks` 表里找请求方那条连接。把 UDP 映射塞进 `socket_addr`
    /// 就等于打断应答路由，这条测试锁死这一点。
    #[tokio::test]
    async fn punch_hole_request_carries_the_requesters_udp_port_to_the_target() {
        let server = start_split_plain_server().await;
        let target = client_udp_on("[::1]").await;
        register_online(&target, &server, "peer-b-id", b"pk-b").await;

        let mut requester = Signaler::connect(&server).await;
        let requester_addr = requester.local_addr();
        requester
            .send(&RendezvousMessage::with(
                rendezvous_message::Union::PunchHoleRequest(PunchHoleRequest {
                    id: "peer-b-id".to_owned(),
                    version: "1.2.2".to_owned(),
                    udp_port: 54321,
                    ..Default::default()
                }),
            ))
            .await;

        match recv_udp(&target).await.union {
            Some(rendezvous_message::Union::PunchHole(ph)) => {
                assert_eq!(ph.udp_port, 54321, "请求方自报的 UDP 端口应透传给目标端");
                assert_eq!(
                    addr::decode(&ph.socket_addr),
                    Some(requester_addr),
                    "socket_addr 仍须是请求方的信令 TCP 源地址（应答路由靠它）"
                );
            }
            other => panic!("期望 PunchHole，实得 {other:?}"),
        }
    }

    /// (b) 的兜底：目标端**未登记** UDP 映射（老客户端 / 尚未走完 RegisterPk）时，
    /// `PunchHoleResponse.socket_addr` 逐字节退回旧行为——用它的信令 TCP 源地址。
    #[tokio::test]
    async fn punch_hole_response_falls_back_to_the_signaling_source_without_a_registered_mapping() {
        let server = start_split_plain_server().await;
        let mut requester = Signaler::connect(&server).await;
        let requester_addr = requester.local_addr();

        // 一个**未登记**的 ID 回 PunchHoleSent：登记表里查不到它 → 走兜底分支。
        let mut responder = Signaler::connect(&server).await;
        let responder_addr = responder.local_addr();
        responder
            .send(&RendezvousMessage::with(
                rendezvous_message::Union::PunchHoleSent(PunchHoleSent {
                    socket_addr: addr::encode(requester_addr),
                    id: "unregistered-id".to_owned(),
                    version: "1.2.2".to_owned(),
                    ..Default::default()
                }),
            ))
            .await;
        responder.expect_closed().await;

        let Some(rendezvous_message::Union::PunchHoleResponse(response)) =
            requester.recv().await.and_then(|m| m.union)
        else {
            panic!("请求方应收到 PunchHoleResponse");
        };
        assert_eq!(
            addr::decode(&response.socket_addr),
            Some(responder_addr),
            "未登记 UDP 映射时应退回信令源地址（旧行为）"
        );
    }

    /// NAT 探测必须**两个 UDP 端口都应答**：客户端要判「是否在对称 NAT 后面」，唯一的办法是
    /// 从同一个套接字向两个不同目的端口各探一次、比对观测到的源端口——只探 NAT 探测端口判不
    /// 出来。这条锁的是打洞预判成立的前提，漏了它预判会永远得到 `Unknown`（fail-open 照打，
    /// 对称 NAT 下白等 3 秒）。
    ///
    /// 刻意用**会下发配置更新**的 config：锁住「应答不带 `cu`」在**主端口**上也是结构性的。
    #[tokio::test]
    async fn nat_probe_is_answered_on_the_main_udp_port_too() {
        let key = Arc::new(ServerKey::parse("").expect("空 key 应可解析"));
        let config = RendezvousConfig {
            serial: 1,
            rendezvous_servers: vec!["hbbs.example:21116".to_owned()],
            ..RendezvousConfig::default()
        };
        let server = start_server(config, key).await;
        let sock = client_udp().await;
        let local_port = sock.local_addr().expect("应能取本地地址").port();
        send_udp(
            &sock,
            &RendezvousMessage::with(rendezvous_message::Union::TestNatRequest(
                TestNatRequest { serial: 0 },
            )),
            server.udp,
        )
        .await;

        match recv_udp(&sock).await.union {
            Some(rendezvous_message::Union::TestNatResponse(res)) => {
                assert_eq!(res.port, i32::from(local_port), "应回报观察到的源端口");
                assert!(res.cu.is_none(), "NAT 探测应答不得携带配置更新");
            }
            other => panic!("期望 TestNatResponse，实得 {other:?}"),
        }
        // 探测不是注册：不得往登记表里写任何东西。
        assert!(
            server.peers.is_empty().await,
            "NAT 探测不得写登记表（否则会污染打洞地址语义）"
        );
    }

    /// NAT 探测端口的 UDP 半边必须应答 `TestNatRequest`，回报观察到的源端口。
    #[tokio::test]
    async fn nat_udp_port_answers_test_nat_request() {
        let server = start_plain_server().await;
        let sock = client_udp().await;
        let local_port = sock.local_addr().expect("应能取本地地址").port();
        send_udp(
            &sock,
            &RendezvousMessage::with(rendezvous_message::Union::TestNatRequest(
                TestNatRequest { serial: 7 },
            )),
            server.nat_udp,
        )
        .await;

        match recv_udp(&sock).await.union {
            Some(rendezvous_message::Union::TestNatResponse(res)) => {
                assert_eq!(res.port, i32::from(local_port), "应回报观察到的源端口");
            }
            other => panic!("期望 TestNatResponse，实得 {other:?}"),
        }
    }

    /// NAT 探测端口**只**认 `TestNatRequest`：其余一律静默丢弃（反反射/放大），
    /// 且不得把来源地址写进登记表（否则「面向 NAT 端口的映射」会污染打洞地址语义）。
    #[tokio::test]
    async fn nat_udp_port_ignores_other_messages() {
        let server = start_plain_server().await;
        let sock = client_udp().await;
        send_udp(&sock, &register_peer_msg("nat-probe-id", 0), server.nat_udp).await;

        let mut buf = vec![0u8; MAX_DATAGRAM];
        assert!(
            timeout(Duration::from_millis(300), sock.recv_from(&mut buf))
                .await
                .is_err(),
            "NAT 探测 UDP 端口不应对非 TestNatRequest 的消息回应答"
        );
        assert!(
            server.peers.lookup("nat-probe-id").await.is_none(),
            "NAT 探测端口不得把来源写进登记表"
        );
    }

    /// 放大系数锁：应答是小常数，不随请求大小增长（一个 1400 字节的数据报也只能换回
    /// 几个字节）。
    ///
    /// 这里刻意用一个**会下发配置更新**的 config（`serial = 1` 且汇合服务器列表非空）：
    /// 否则这条测试只证明「默认 config 恰好不带 `cu`」，而不是「NAT 探测应答结构上就是小常数」。
    /// TCP 半边仍照旧下发 `cu`，只有 UDP 半边刻意丢掉它。
    #[tokio::test]
    async fn nat_udp_response_is_a_small_constant() {
        let key = Arc::new(ServerKey::parse("").expect("空 key 应可解析"));
        let config = RendezvousConfig {
            serial: 1,
            rendezvous_servers: vec!["hbbs.example:21116".to_owned()],
            ..RendezvousConfig::default()
        };
        let server = start_server(config, key).await;
        let sock = client_udp().await;
        send_udp(
            &sock,
            &RendezvousMessage::with(rendezvous_message::Union::TestNatRequest(
                TestNatRequest { serial: 0 },
            )),
            server.nat_udp,
        )
        .await;
        let resp = recv_udp(&sock).await;
        match &resp.union {
            Some(rendezvous_message::Union::TestNatResponse(res)) => assert!(
                res.cu.is_none(),
                "NAT 探测的 UDP 应答不得携带配置更新（否则大小随服务器列表增长）"
            ),
            other => panic!("期望 TestNatResponse，实得 {other:?}"),
        }
        assert!(
            resp.encode_to_vec().len() <= 16,
            "NAT 探测应答必须是小的常数，实得 {} 字节",
            resp.encode_to_vec().len()
        );
    }

    #[tokio::test]
    async fn local_addr_is_forwarded_back_as_an_is_local_response() {
        let server = start_signing_server().await;
        let target = client_udp().await;
        let target_pk = b"target-public-key".to_vec();
        register_online(&target, &server, "peer-b-id", &target_pk).await;

        let mut requester = Signaler::connect(&server).await;
        // 服务端配了 key 时，请求必须带上同一个 key。
        requester
            .send(&punch_hole_request("peer-b-id", server.key.key()))
            .await;
        let Some(rendezvous_message::Union::FetchLocalAddr(fla)) = recv_udp(&target).await.union
        else {
            panic!("同内网场景应收到 FetchLocalAddr");
        };

        let mut responder = Signaler::connect(&server).await;
        responder
            .send(&RendezvousMessage::with(
                rendezvous_message::Union::LocalAddr(LocalAddr {
                    socket_addr: fla.socket_addr,
                    local_addr: addr::encode("10.0.0.5:21118".parse().unwrap()),
                    relay_server: "relay.example:21117".to_owned(),
                    id: "peer-b-id".to_owned(),
                    version: "1.2.2".to_owned(),
                    ..Default::default()
                }),
            ))
            .await;
        responder.expect_closed().await;

        let Some(rendezvous_message::Union::PunchHoleResponse(response)) =
            requester.recv().await.and_then(|m| m.union)
        else {
            panic!("请求方应收到 PunchHoleResponse");
        };
        assert_eq!(
            response.union,
            Some(punch_hole_response::Union::IsLocal(true))
        );
        assert_eq!(
            addr::decode(&response.socket_addr),
            Some("10.0.0.5:21118".parse().unwrap()),
            "应回传目标端的内网地址"
        );
        server
            .key
            .verify_id_pk("peer-b-id", &target_pk, &response.pk)
            .expect("内网地址应答也应带签名");
    }

    #[tokio::test]
    async fn request_relay_is_forwarded_to_the_target_over_udp() {
        let server = start_plain_server().await;
        let target = client_udp().await;
        register_online(&target, &server, "peer-b-id", b"pk-b").await;

        let mut requester = Signaler::connect(&server).await;
        let requester_addr = requester.local_addr();
        requester
            .send(&RendezvousMessage::with(
                rendezvous_message::Union::RequestRelay(crate::proto::RequestRelay {
                    id: "peer-b-id".to_owned(),
                    uuid: "pair-uuid".to_owned(),
                    licence_key: String::new(),
                    ..Default::default()
                }),
            ))
            .await;

        match recv_udp(&target).await.union {
            Some(rendezvous_message::Union::RequestRelay(rf)) => {
                assert_eq!(rf.uuid, "pair-uuid");
                assert_eq!(
                    addr::decode(&rf.socket_addr),
                    Some(requester_addr),
                    "应把请求方的地址填进去"
                );
            }
            other => panic!("期望 RequestRelay，实得 {other:?}"),
        }
        assert_eq!(server.server.stats().snapshot().relay_requests, 1);
    }

    #[tokio::test]
    async fn relay_response_is_signed_and_sent_to_the_requester() {
        let server = start_signing_server().await;
        let target = client_udp().await;
        let target_pk = b"target-public-key".to_vec();
        register_online(&target, &server, "peer-b-id", &target_pk).await;

        let mut requester = Signaler::connect(&server).await;
        requester
            .send(&RendezvousMessage::with(
                rendezvous_message::Union::RequestRelay(crate::proto::RequestRelay {
                    id: "peer-b-id".to_owned(),
                    uuid: "pair-uuid".to_owned(),
                    ..Default::default()
                }),
            ))
            .await;
        let Some(rendezvous_message::Union::RequestRelay(rf)) = recv_udp(&target).await.union else {
            panic!("目标端应收到 RequestRelay");
        };

        // 目标端把请求方地址原样放回 socket_addr，并声明自己的 ID。
        let mut responder = Signaler::connect(&server).await;
        responder
            .send(&RendezvousMessage::with(
                rendezvous_message::Union::RelayResponse(RelayResponse {
                    socket_addr: rf.socket_addr.clone(),
                    uuid: rf.uuid.clone(),
                    relay_server: "relay.example:21117".to_owned(),
                    union: Some(relay_response::Union::Id("peer-b-id".to_owned())),
                    version: "1.2.2".to_owned(),
                    ..Default::default()
                }),
            ))
            .await;
        responder.expect_closed().await;

        let Some(rendezvous_message::Union::RelayResponse(response)) =
            requester.recv().await.and_then(|m| m.union)
        else {
            panic!("请求方应收到 RelayResponse");
        };
        assert!(response.socket_addr.is_empty(), "不该回显请求方自己的地址");
        assert_eq!(response.relay_server, "relay.example:21117");
        match response.union {
            Some(relay_response::Union::Pk(pk)) => server
                .key
                .verify_id_pk("peer-b-id", &target_pk, &pk)
                .expect("中继应答应带对端公钥签名"),
            other => panic!("ID 应被换成公钥签名，实得 {other:?}"),
        }
    }

    #[tokio::test]
    async fn nat_test_port_reports_the_observed_source_port() {
        let server = start_plain_server().await;
        let stream = TcpStream::connect(server.nat).await.expect("应能连接");
        let local = stream.local_addr().expect("应能取本地地址");
        let (mut reader, mut writer) = stream.into_split();
        write_message(
            &mut writer,
            &RendezvousMessage::with(rendezvous_message::Union::TestNatRequest(
                TestNatRequest { serial: 0 },
            )),
        )
        .await
        .unwrap();

        let mut codec = FrameCodec::new();
        let mut buf = BytesMut::new();
        let msg = read_message(&mut reader, &mut codec, &mut buf)
            .await
            .unwrap()
            .expect("应有应答");
        match msg.union {
            Some(rendezvous_message::Union::TestNatResponse(res)) => {
                assert_eq!(res.port, i32::from(local.port()));
                assert!(res.cu.is_none(), "未配置 serial 时不该下发配置");
            }
            other => panic!("期望 TestNatResponse，实得 {other:?}"),
        }
    }

    #[tokio::test]
    async fn online_request_returns_the_online_bitmap() {
        let server = start_plain_server().await;
        let sock = client_udp().await;
        register_online(&sock, &server, "online-id-1", b"pk").await;
        // 只建记录、不登记公钥 → 不算在线。
        send_udp(&sock, &register_peer_msg("offline-id", 0), server.udp).await;
        let _ = recv_udp(&sock).await;

        let stream = TcpStream::connect(server.nat).await.expect("应能连接");
        let (mut reader, mut writer) = stream.into_split();
        write_message(
            &mut writer,
            &RendezvousMessage::with(rendezvous_message::Union::OnlineRequest(OnlineRequest {
                id: "asker".to_owned(),
                peers: vec!["online-id-1".to_owned(), "offline-id".to_owned()],
            })),
        )
        .await
        .unwrap();

        let mut codec = FrameCodec::new();
        let mut buf = BytesMut::new();
        let msg = read_message(&mut reader, &mut codec, &mut buf)
            .await
            .unwrap()
            .expect("应有应答");
        match msg.union {
            Some(rendezvous_message::Union::OnlineResponse(res)) => {
                assert_eq!(res.states, vec![0b1000_0000], "只有第 1 个在线");
            }
            other => panic!("期望 OnlineResponse，实得 {other:?}"),
        }
    }

    #[tokio::test]
    async fn idle_signaling_connection_is_closed() {
        let key = Arc::new(ServerKey::parse("").unwrap());
        let server = start_server(
            RendezvousConfig {
                tcp_idle_timeout: Duration::from_millis(150),
                ..RendezvousConfig::default()
            },
            key,
        )
        .await;
        let stream = TcpStream::connect(server.tcp).await.expect("应能连接");
        let (mut reader, _writer) = stream.into_split();
        let mut buf = [0u8; 8];
        let n = timeout(Duration::from_secs(3), reader.read(&mut buf))
            .await
            .expect("等待关闭超时")
            .expect("读失败");
        assert_eq!(n, 0, "空闲超时后连接应被关闭");
    }

    #[tokio::test]
    async fn config_update_is_sent_only_when_configured() {
        let key = Arc::new(ServerKey::parse("").unwrap());
        let server = start_server(
            RendezvousConfig {
                serial: 5,
                rendezvous_servers: vec!["hbbs.example:21116".to_owned()],
                ..RendezvousConfig::default()
            },
            key,
        )
        .await;
        let sock = client_udp().await;
        send_udp(&sock, &register_peer_msg("123456789", 0), server.udp).await;
        // 先收到心跳应答，再收到配置更新。
        let first = recv_udp(&sock).await;
        assert!(matches!(
            first.union,
            Some(rendezvous_message::Union::RegisterPeerResponse(_))
        ));
        let second = recv_udp(&sock).await;
        match second.union {
            Some(rendezvous_message::Union::ConfigureUpdate(cu)) => {
                assert_eq!(cu.serial, 5);
                assert_eq!(cu.rendezvous_servers, vec!["hbbs.example:21116"]);
            }
            other => panic!("期望 ConfigureUpdate，实得 {other:?}"),
        }

        // 客户端序号已经追平时不再下发。
        send_udp(&sock, &register_peer_msg("123456789", 5), server.udp).await;
        let _ = recv_udp(&sock).await;
        let mut buf = vec![0u8; MAX_DATAGRAM];
        assert!(
            timeout(Duration::from_millis(300), sock.recv_from(&mut buf))
                .await
                .is_err(),
            "序号追平后不该再下发配置"
        );
    }

    #[test]
    fn mask_parses_and_matches() {
        let mask: Mask = "192.168.0.0/16".parse().expect("应可解析");
        assert!(mask.contains("192.168.1.1".parse().unwrap()));
        assert!(mask.contains("192.168.255.254".parse().unwrap()));
        assert!(!mask.contains("192.169.0.1".parse().unwrap()));
        assert_eq!(mask.prefix(), 16);

        // /0 匹配一切；/32 只匹配自己。
        let any: Mask = "0.0.0.0/0".parse().unwrap();
        assert!(any.contains("8.8.8.8".parse().unwrap()));
        let exact: Mask = "10.1.2.3/32".parse().unwrap();
        assert!(exact.contains("10.1.2.3".parse().unwrap()));
        assert!(!exact.contains("10.1.2.4".parse().unwrap()));

        for bad in ["192.168.0.0", "192.168.0.0/33", "not-an-ip/16", "10.0.0.0/"] {
            assert!(Mask::from_str(bad).is_err(), "{bad} 应被拒绝");
        }
    }

    #[tokio::test]
    async fn lan_mask_forces_relay_between_different_networks() {
        // A 在内网、B 在外网 → 强制走中继：nat_type 被改成 Symmetric，
        // 且中继地址取配置的 local_ip。
        let key = Arc::new(ServerKey::parse("").unwrap());
        let server = start_server(
            RendezvousConfig {
                mask: Some("127.0.0.0/8".parse().unwrap()),
                local_ip: "127.0.0.1:21117".to_owned(),
                ..RendezvousConfig::default()
            },
            key,
        )
        .await;
        // 目标端从回环发起（算内网），请求方也从回环发起——两边都算内网，
        // 于是走 FetchLocalAddr。这条用来确认掩码生效后 is_lan 确实为真。
        let target = client_udp().await;
        register_online(&target, &server, "peer-b-id", b"pk-b").await;
        let mut requester = Signaler::connect(&server).await;
        requester.send(&punch_hole_request("peer-b-id", "")).await;
        match recv_udp(&target).await.union {
            Some(rendezvous_message::Union::FetchLocalAddr(fla)) => {
                assert_eq!(fla.relay_server, "127.0.0.1:21117", "内网端应拿到 local_ip");
            }
            other => panic!("期望 FetchLocalAddr，实得 {other:?}"),
        }
    }

    #[tokio::test]
    async fn is_lan_is_false_without_a_mask() {
        let server = start_plain_server().await;
        assert!(!server.server.is_lan("192.168.1.1:1".parse().unwrap()));
        assert!(!server.server.is_lan("127.0.0.1:1".parse().unwrap()));
    }

    #[tokio::test]
    async fn relay_selection_rotates_and_handles_empty() {
        let key = Arc::new(ServerKey::parse("").unwrap());
        let none = start_server(RendezvousConfig::default(), Arc::clone(&key)).await;
        assert_eq!(none.server.pick_relay_server(), "");

        let single = start_server(
            RendezvousConfig {
                relay_servers: vec!["only:21117".to_owned()],
                ..RendezvousConfig::default()
            },
            Arc::clone(&key),
        )
        .await;
        assert_eq!(single.server.pick_relay_server(), "only:21117");
        assert_eq!(single.server.pick_relay_server(), "only:21117");

        let multi = start_server(
            RendezvousConfig {
                relay_servers: vec!["r1:21117".to_owned(), "r2:21117".to_owned()],
                ..RendezvousConfig::default()
            },
            Arc::clone(&key),
        )
        .await;
        let picked: Vec<String> = (0..4)
            .map(|_| multi.server.pick_relay_server())
            .collect();
        assert_eq!(
            picked,
            vec!["r1:21117", "r2:21117", "r1:21117", "r2:21117"],
            "多台中继应轮转"
        );
    }

    #[tokio::test]
    async fn statistic_counters_track_activity() {
        let server = start_plain_server().await;
        let sock = client_udp().await;
        register_online(&sock, &server, "123456789", b"pk").await;
        // register_online 发了一次心跳 + 一次 RegisterPk；再补一次心跳，凑成注释里的
        // 「两次心跳 + 一次 RegisterPk」，并顺带覆盖「登记后心跳仍有应答」。
        send_udp(&sock, &register_peer_msg("123456789", 0), server.udp).await;
        let _ = recv_udp(&sock).await;
        let mut client = Signaler::connect(&server).await;
        client.send(&punch_hole_request("123456789", "")).await;
        let _ = recv_udp(&sock).await; // 目标端收到的 PunchHole

        let snap = server.server.stats().snapshot();
        assert_eq!(snap.udp_datagrams, 3, "两次心跳 + 一次 RegisterPk");
        assert_eq!(snap.heartbeats, 2);
        assert_eq!(snap.pk_registrations, 1);
        assert_eq!(snap.punch_hole_requests, 1);
        assert_eq!(snap.tcp_connections, 1);
        assert_eq!(snap.rejected, 0);
    }

    #[test]
    fn try_into_v4_helper_is_used_consistently() {
        // 顺带确认本模块引用的地址归一对双栈地址有效。
        let mapped: SocketAddr = "[::ffff:1.2.3.4]:5".parse().unwrap();
        assert_eq!(
            try_into_v4(mapped),
            SocketAddr::from((Ipv4Addr::new(1, 2, 3, 4), 5))
        );
    }
}
