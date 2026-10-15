//! 客户端侧的 RustDesk 会合/中继编排（数据面建立）。
//!
//! 复用 `rust-tunnel-rustdesk` 的 `frame`/`proto`/`addr`，只实现**客户端**行为：
//!
//! # 服务端语义（照抄 `crates/rustdesk`，不要凭记忆改）
//!
//! - 心跳 `RegisterPeer` 与 `RegisterPk` **只走 UDP** 到 hbbs 主端口（`rendezvous_port`）。
//! - 打洞/中继信令走主端口 **TCP**；`OnlineRequest` 只在主端口 **-1** 受理。
//! - hbbr：两端各自 TCP 连中继端口，各发**恰好一帧** `RequestRelay{id, uuid, licence_key}`，
//!   之后立即裸字节；**没有任何 ack**；按 **uuid 相同**配对；`pair_window`/`idle_timeout`
//!   都是 30s。第一到者最多在等待表里停 30s，期间其「帧后残留字节」会被缓存并在配对时
//!   补给对端——因此把 `RequestRelay` 与会话 `Hello` 写在**同一次 write** 可以省一个 RTT。
//! - 同内网直连：控制器发 `PunchHoleRequest` → 服务端推 `FetchLocalAddr` 给被控端 →
//!   被控端回 `LocalAddr`（带自己的内网地址）→ 服务端回 `PunchHoleResponse{is_local}` 给
//!   控制器 → 控制器直连该地址。
//! - 跨 NAT 打洞：控制器发 `PunchHoleRequest`（`udp_port` 填**公网观测端口**，见下）→ 服务端把
//!   请求方地址推给被控端（`PunchHole`，含请求方 `socket_addr` 与 `udp_port`）→ 被控端回
//!   `PunchHoleSent`（`socket_addr` **原样回显**请求方地址）**并反向打洞** → 服务端把目标端
//!   **已登记的 UDP 映射**转成 `PunchHoleResponse` 回给控制器那条信令连接 → 两端对打 UDP。
//!   路径由 `PunchHoleResponse.socket_addr` 是「内网 TCP 地址」还是「UDP 映射」经 oneof
//!   (`IsLocal(true)`) 区分。
//!
//! # 为什么 UDP 打洞**不需要**端口复用
//!
//! TCP 同时打开才要求两端在同一端口上收发（故需 SO_REUSEADDR/端口复用）；UDP 不需要——
//! 从**已注册的那个 UDP 套接字**向对端发一个数据报，就为「本端 → 对端」建立了一条 NAT 映射，
//! 对端从它自己的套接字发过来的包便能穿过本端 NAT（cone 下）。这正是 UDP 打洞比 TCP 同时打开
//! 简单的地方，也是本实现让 NAT 探测、demux、可靠层发送**共用同一个 UDP 套接字**的原因
//! （换了套接字，映射就对不上）。
//!
//! # `licence_key` 与 `IdPk` 验签
//!
//! `licence_key` 取自控制通道下发的 [`RemoteParams::public_key`]（服务端已归一化为公钥
//! 字符串或共享口令），填进 `PunchHoleRequest` / `RequestRelay` 供 hbbs/hbbr 明文比对。
//!
//! 服务端还会用私钥对 `ID ‖ 公钥`（`IdPk`）签名下发，供客户端在安全模式下验签以防
//! 会合服务器替换对端公钥。**阶段 1 不校验该签名**：本实现的数据面身份由服务端签发的一次性
//! `token` 的 HMAC 证明（见 [`crate::auth`]），对端公钥不参与任何信任决策，因此验签不是
//! 必需的。接入非本项目客户端或引入基于公钥的信任时需要补上（`rustdesk::key::ServerKey::verify_id_pk`）。

use crate::error::{RemoteError, RemoteResult};
use crate::nat::{classify, NatCache, NatClass, NAT_PROBE_TIMEOUT};
use crate::punch::{self, PunchRegistry, PUNCH_BUDGET};
use crate::reliable::{self, DatagramRx};
use crate::transport::{EstablishedStream, EstablishRequest, Role, TransportEstablisher, TransportPath};
use async_trait::async_trait;
use bytes::BytesMut;
use rust_tunnel_common::protocol::RemoteParams;
use rust_tunnel_rustdesk::addr;
use rust_tunnel_rustdesk::frame::{frame, FrameCodec};
use rust_tunnel_rustdesk::proto::punch_hole_response;
use rust_tunnel_rustdesk::proto::rendezvous_message::Union;
use rust_tunnel_rustdesk::proto::{
    FetchLocalAddr, LocalAddr, ProstMessage, PunchHole, PunchHoleRequest, PunchHoleSent, RegisterPeer, RegisterPk,
    RendezvousMessage, RequestRelay, TestNatRequest,
};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio::time::{interval, timeout, Instant, MissedTickBehavior};
use tokio_util::codec::Decoder as _;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// 客户端版本串（非空即可；服务端据此决定是否对 `ID ‖ 公钥` 签名）。
const CLIENT_VERSION: &str = "1.2.2";

/// 心跳间隔；必须小于 hbbs 的 `REG_TIMEOUT`（30s）。
const REGISTER_INTERVAL: Duration = Duration::from_secs(15);

/// 等待中继信令推送的最长时间（略小于中继的 `pair_window`）。
const RELAY_WAIT: Duration = Duration::from_secs(25);

/// 直连尝试的连接超时。
const DIRECT_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// 等待 `PunchHoleResponse` 的超时（**不是** [`RELAY_WAIT`]，见下）。
///
/// # 它覆盖的是什么
///
/// 无论同内网还是跨 NAT，hbbs 都会回一条 `PunchHoleResponse`——只是路径不同：
///
/// - 同内网：`PunchHoleRequest` → 服务端推 `FetchLocalAddr` → 目标端回 `LocalAddr` → 服务端把
///   内网地址转成 `PunchHoleResponse{IsLocal(true)}` 回给请求方；
/// - 跨 NAT：`PunchHoleRequest` → 服务端推 `PunchHole` 给目标端 → 目标端回 `PunchHoleSent`（并
///   反向打洞）→ 服务端把它转成 `PunchHoleResponse` 回给请求方那条连接。
///
/// 所以这个超时覆盖的是「约 3 个 RTT」，不再是阶段 1 那句「跨 NAT 永远等不到应答」。
///
/// # 为什么必须远小于 `RELAY_WAIT`
///
/// 若这里沿用 `RELAY_WAIT`(25s)，而 hbbs 因网络故障/对端未登记而**真的**不回（例如
/// `PunchHoleRequest` 落到一个静默的服务端），控制器要空等 25s 才去发中继信令，而被控端的
/// `wait_relay_push(RELAY_WAIT)` 几乎同时超时 → **会话直接失败**，用户看到的是「卡在连接中」
/// 25 秒。故给它一个短窗口：拿不到就立刻回退中继。
///
/// # 预算不等式（打洞全链路）
///
/// 控制器最坏耗时 = `DIRECT_SIGNAL_TIMEOUT`(3s) + [`PUNCH_BUDGET`](3s) = **6s**，必须 **≪**
/// 被控端 `RELAY_WAIT`(25s)，否则会出现「控制器还在打洞、被控端已放弃转中继」的错位。
/// 配 `try_direct_gives_up_quickly_when_hbbs_never_answers` 锁定。
const DIRECT_SIGNAL_TIMEOUT: Duration = Duration::from_secs(3);

/// 会合/中继客户端配置。
#[derive(Debug, Clone)]
pub struct SignalingConfig {
    /// 本端 RustDesk ID。
    pub rd_id: String,
    /// 服务端密钥（`licence_key` 明文比对；空串表示服务端不校验）。
    pub licence_key: String,
    /// 会合服务器主机（由控制通道下发的 `RemoteParams.rendezvous_host` 补齐）。
    pub host: String,
    /// 会合主端口。
    pub rendezvous_port: u16,
    /// 中继端口。
    pub relay_port: u16,
    /// 中继地址覆盖（空串 = 用 `host`）。
    pub relay_server: String,
    /// 是否强制走中继（跳过直连尝试）。
    pub always_use_relay: bool,
}

impl SignalingConfig {
    /// 从控制通道下发的 [`RemoteParams`] 与本端信息构造。
    #[must_use]
    pub fn from_params(params: &RemoteParams, rd_id: String, local_host: &str) -> Self {
        let host = if params.rendezvous_host.is_empty() {
            local_host.to_owned()
        } else {
            params.rendezvous_host.clone()
        };
        Self {
            rd_id,
            licence_key: params.public_key.clone(),
            host,
            rendezvous_port: params.rendezvous_port,
            relay_port: params.relay_port,
            relay_server: params.relay_server.clone(),
            always_use_relay: params.always_use_relay,
        }
    }
}

/// 把一条会合消息编码成一帧线上字节。
fn encode_message(msg: &RendezvousMessage) -> RemoteResult<BytesMut> {
    frame(&msg.encode_to_vec()).map_err(|err| RemoteError::Signaling(format!("会合帧编码失败: {err}")))
}

/// 剥掉 `host` 里可能带的端口，只留主机部分。
///
/// GUI 传进来的 `server_host` 就是 `config.server`，即 `host:port`；而这里的端口由
/// [`RemoteParams`] 单独给出。若不剥离，`getaddrinfo` 会把 `"hbbs.example:8080"`
/// 整个当成主机名而解析失败，会话永远建不起来。
///
/// 同时处理 IPv6 字面量：`[::1]:8080` → `::1`；裸 `::1` 有多个冒号、右侧不是端口，
/// 原样返回。
fn strip_port(host: &str) -> &str {
    if let Some(rest) = host.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    match host.rsplit_once(':') {
        Some((head, tail)) if !head.contains(':') && tail.parse::<u16>().is_ok() => head,
        _ => host,
    }
}

/// 解析主机 + 端口为套接字地址。
///
/// `host` 允许带端口（见 [`strip_port`]），带的端口会被忽略，以 `port` 为准。
async fn resolve(host: &str, port: u16) -> RemoteResult<SocketAddr> {
    let host = strip_port(host);
    tokio::net::lookup_host((host, port))
        .await
        .map_err(|err| RemoteError::Signaling(format!("无法解析 {host}:{port}: {err}")))?
        .next()
        .ok_or_else(|| RemoteError::Signaling(format!("{host}:{port} 无可用地址")))
}

/// 读一条带长度前缀帧的会合消息（复用 `rustdesk::frame::FrameCodec` 的可变长头部解析）。
async fn read_message<R: AsyncRead + Unpin>(reader: &mut R) -> RemoteResult<RendezvousMessage> {
    let mut codec = FrameCodec::new();
    let mut buf = BytesMut::with_capacity(1024);
    loop {
        if let Some(payload) = codec
            .decode(&mut buf)
            .map_err(|err| RemoteError::Signaling(format!("会合帧解码失败: {err}")))?
        {
            return RendezvousMessage::decode(&payload[..])
                .map_err(|err| RemoteError::Signaling(format!("会合消息解码失败: {err}")));
        }
        let n = reader.read_buf(&mut buf).await?;
        if n == 0 {
            return Err(RemoteError::Signaling("信令连接提前关闭".to_owned()));
        }
    }
}

/// 打洞信令的两种结果（`PunchHoleResponse.socket_addr` **一个字段两种含义**，只能靠 oneof 判别）。
enum PunchSignal {
    /// 同内网：该地址是目标端的**内网 TCP 监听地址** → 走 TCP 直连。
    Local(SocketAddr),
    /// 跨 NAT：该地址是目标端**已登记的 UDP 映射** → 走 UDP 打洞。**绝不要**对它
    /// `TcpStream::connect`（会白等满 [`DIRECT_CONNECT_TIMEOUT`] 才去尝试打洞）。
    Punch(SocketAddr),
}

/// 被控端三个建立候选的胜出者。
enum Candidate {
    /// UDP 打洞成功（已建立可靠层字节流）。
    Punch(DuplexStream),
    /// TCP 直连接入成功。
    Direct(TcpStream),
    /// 中继推送到达（后续再连 hbbr）。
    Relay,
}

/// RustDesk 客户端传输建立器。
pub struct RustDeskTransport {
    /// 配置。
    cfg: SignalingConfig,
    /// hbbs 主端口地址（TCP 信令 + UDP 心跳）。
    hbbs: SocketAddr,
    /// hbbr 中继地址。
    relay_addr: SocketAddr,
    /// 本端直连监听地址（同内网优化用）。
    direct_addr: SocketAddr,
    /// **共享**的 UDP 套接字：NAT 探测、demux、可靠层发送三方共用同一个（否则 NAT 映射对不上）。
    udp: Arc<UdpSocket>,
    /// 本端 UDP 套接字的**本地**端口（NAT 探测失败时兜底填进 `PunchHoleRequest.udp_port`）。
    local_udp_port: u16,
    /// 服务端观测到的**公网** UDP 源端口（仅主端口探测成功时有值）。见 [`Self::udp_port_for_peer`]。
    public_udp_port: Option<u16>,
    /// 本端 NAT 类型（`new()` 里预判一次）。
    nat_class: NatClass,
    /// 打洞失败缓存（按对端 `rd_id` 索引；仅控制器侧使用，见 `establish`）。
    nat_cache: NatCache,
    /// 打洞会话的 `conn_id` → 数据报投递端（demux 据此路由）。
    punch_registry: Arc<PunchRegistry>,
    /// 服务端经 UDP 推送的消息队列。
    pushes: Mutex<mpsc::UnboundedReceiver<RendezvousMessage>>,
    /// 直连接入的流队列。
    direct_rx: Mutex<mpsc::UnboundedReceiver<TcpStream>>,
    /// 关闭信号。
    cancel: CancellationToken,
    /// 后台 demux 任务（UDP 心跳 + 推送处理 + 打洞数据报分发）。
    _registration: JoinHandle<()>,
    /// 后台直连接受任务。
    _acceptor: JoinHandle<()>,
}

impl std::fmt::Debug for RustDeskTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RustDeskTransport")
            .field("rd_id", &self.cfg.rd_id)
            .field("hbbs", &self.hbbs)
            .field("relay", &self.relay_addr)
            .field("direct", &self.direct_addr)
            .finish_non_exhaustive()
    }
}

impl Drop for RustDeskTransport {
    fn drop(&mut self) {
        // 析构即取消后台登记任务与直连接受任务。
        //
        // 为什么必须有：`RustDeskTransport::new` 会 spawn 一个登记任务（UDP 心跳 + 推送处理）和
        // 一个直连 accept 任务（监听 0.0.0.0:0）。若无人取消它们，`LazyTransport::transport_for`
        // 在 `rd_id` 变化时重建、或建立阶段被取消而丢弃传输时，旧任务与那个 `TcpListener` 就会
        // **永久泄漏**——绑定的端口不释放、心跳也一直往外发。
        //
        // 这里的 `JoinHandle` 不 join（join 会要求 `Drop` 里阻塞等待，语义也过度），随结构体一起
        // detach 即可：`cancel` 一 fire，两个任务都在各自的 `select!` 里看到 `cancelled()` 后退出。
        self.cancel.cancel();
    }
}

impl RustDeskTransport {
    /// 构造并启动后台登记/直连监听任务。
    ///
    /// # Errors
    /// 主机解析、套接字绑定失败时返回 [`RemoteError::Signaling`] / [`RemoteError::Io`]。
    pub async fn new(cfg: SignalingConfig) -> RemoteResult<std::sync::Arc<Self>> {
        let hbbs = resolve(&cfg.host, cfg.rendezvous_port).await?;
        let relay_host = if cfg.relay_server.is_empty() {
            cfg.host.clone()
        } else {
            cfg.relay_server.clone()
        };
        let relay_addr = resolve(&relay_host, cfg.relay_port).await?;

        let udp = Arc::new(
            UdpSocket::bind("0.0.0.0:0")
                .await
                .map_err(|err| RemoteError::Signaling(format!("绑定 UDP 失败: {err}")))?,
        );
        let local_udp_port = udp
            .local_addr()
            .map_err(|err| RemoteError::Signaling(format!("取 UDP 地址失败: {err}")))?
            .port();
        let listener = TcpListener::bind("0.0.0.0:0")
            .await
            .map_err(|err| RemoteError::Signaling(format!("绑定直连监听失败: {err}")))?;
        let listen_port = listener
            .local_addr()
            .map_err(|err| RemoteError::Signaling(format!("取监听地址失败: {err}")))?
            .port();
        let local_ip = probe_local_ip(hbbs).await;
        let direct_addr = SocketAddr::new(local_ip, listen_port);

        // NAT 预判**必须在 spawn demux 之前**做完：demux 是 UDP 套接字唯一的 `recv_from` 读者，
        // 两条读路径会互相偷走数据报（这里偷走心跳应答、那里偷走探测应答）。见 `probe_nat`。
        let (nat_class, public_udp_port) = probe_nat(&udp, hbbs, cfg.rendezvous_port).await;

        let (push_tx, push_rx) = mpsc::unbounded_channel();
        let (direct_tx, direct_rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let punch_registry = Arc::new(PunchRegistry::new());

        let registration = tokio::spawn(udp_demux_task(
            Arc::clone(&udp),
            cfg.clone(),
            hbbs,
            direct_addr,
            push_tx,
            Arc::clone(&punch_registry),
            cancel.clone(),
        ));
        let acceptor = tokio::spawn(accept_task(listener, direct_tx, cancel.clone()));

        Ok(std::sync::Arc::new(Self {
            cfg,
            hbbs,
            relay_addr,
            direct_addr,
            udp,
            local_udp_port,
            public_udp_port,
            nat_class,
            nat_cache: NatCache::new(),
            punch_registry,
            pushes: Mutex::new(push_rx),
            direct_rx: Mutex::new(direct_rx),
            cancel,
            _registration: registration,
            _acceptor: acceptor,
        }))
    }

    /// 本端直连监听地址。
    #[must_use]
    pub fn direct_addr(&self) -> SocketAddr {
        self.direct_addr
    }

    /// 填进 `PunchHoleRequest.udp_port` 的端口。
    ///
    /// **必须是公网观测端口，不是本地端口**：对端拿 `(socket_addr 的 IP, udp_port)` 组地址反向
    /// 打洞，若填本地端口而 NAT 不做端口保持（很常见），那个端口在本端 NAT 上**没有映射**，包会
    /// 被直接丢掉。探测失败/老服务端（无观测值）时兜底填**本地端口**——退化成上游行为，不会更差。
    fn udp_port_for_peer(&self) -> i32 {
        i32::from(self.public_udp_port.unwrap_or(self.local_udp_port))
    }

    /// 本端 NAT 分类（单测断言用）。
    #[cfg(test)]
    fn nat_class_for_test(&self) -> NatClass {
        self.nat_class
    }

    /// 公网观测端口（单测断言用）。
    #[cfg(test)]
    fn public_udp_port_for_test(&self) -> Option<u16> {
        self.public_udp_port
    }

    /// 登记一个打洞会话，返回按 `conn_id` 路由进来的数据报接收端。
    ///
    /// 队列容量 256：demux 用 `try_send`，满了就丢这一条——UDP 语义本就允许丢包，可靠层会按 RTO
    /// 重传；**绝不能阻塞 demux**（会拖垮所有会话）。
    ///
    /// # 注销绑在会话生命周期上
    ///
    /// 可靠层建立后 `rx` 被移进协议任务，**后续所有数据报仍须经 [`PunchRegistry`] 路由进来**——
    /// 所以**不能**在 `connect`/`accept` 返回时立刻注销（那会让本端从那一刻起收不到对端任何
    /// 数据报，会话半死）。而 `conn_id` 由一次性 token 派生、永不复用，一直留在表里则是**每会话
    /// 一条**的泄漏。故派生一个清理任务：`rx` 被丢弃（会话结束）时 `Sender::closed()` 就绪，
    /// 随即注销。既保证会话期间路由不断，又不留悬挂表项。
    async fn register_punch_session(&self, conn_id: u64) -> DatagramRx {
        let (tx, rx) = mpsc::channel(256);
        self.punch_registry.register(conn_id, tx.clone()).await;
        let registry = Arc::clone(&self.punch_registry);
        tokio::spawn(async move {
            // 通道关闭 = 会话结束（可靠层任务退出、`rx` 被丢弃）。
            tx.closed().await;
            registry.unregister(conn_id).await;
        });
        rx
    }

    /// 向 hbbs 发一帧 `RequestRelay`（`id` = 目标端），让服务端把请求推给目标端。
    async fn notify_relay(&self, peer_rd_id: &str, uuid: &str) -> RemoteResult<()> {
        let mut stream = TcpStream::connect(self.hbbs).await?;
        let msg = RendezvousMessage::with(Union::RequestRelay(RequestRelay {
            id: peer_rd_id.to_owned(),
            uuid: uuid.to_owned(),
            licence_key: self.cfg.licence_key.clone(),
            ..Default::default()
        }));
        stream.write_all(&encode_message(&msg)?).await?;
        stream.flush().await?;
        // 无需应答；服务端已把它转发给目标端（UDP）。连接可直接关闭。
        Ok(())
    }

    /// 连接 hbbr 并写出 `RequestRelay` + 前置字节，返回裸字节流。
    async fn connect_relay(&self, uuid: &str, early: &[u8]) -> RemoteResult<TcpStream> {
        let mut stream = TcpStream::connect(self.relay_addr).await?;
        let _ = stream.set_nodelay(true);
        let msg = RendezvousMessage::with(Union::RequestRelay(RequestRelay {
            id: self.cfg.rd_id.clone(),
            uuid: uuid.to_owned(),
            licence_key: self.cfg.licence_key.clone(),
            ..Default::default()
        }));
        let mut wire = encode_message(&msg)?;
        // 关键：RequestRelay 帧与首批应用字节（Hello）同一次 write，配对后对端立刻拿到。
        wire.extend_from_slice(early);
        stream.write_all(&wire).await?;
        stream.flush().await?;
        Ok(stream)
    }

    /// 等待一条匹配 `uuid` 的 `RequestRelay` 推送。
    async fn wait_relay_push(&self, uuid: &str, wait: Duration) -> RemoteResult<()> {
        let deadline = Instant::now() + wait;
        let mut guard = self.pushes.lock().await;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(RemoteError::Signaling("等待中继信令超时".to_owned()));
            }
            let msg = timeout(remaining, guard.recv())
                .await
                .map_err(|_| RemoteError::Signaling("等待中继信令超时".to_owned()))?
                .ok_or_else(|| RemoteError::Signaling("信令通道已关闭".to_owned()))?;
            if let Some(Union::RequestRelay(rf)) = msg.union {
                if rf.uuid == uuid {
                    return Ok(());
                }
            }
        }
    }

    /// 等待一条直连接入。
    async fn next_direct(&self) -> RemoteResult<TcpStream> {
        let mut guard = self.direct_rx.lock().await;
        guard
            .recv()
            .await
            .ok_or_else(|| RemoteError::Signaling("直连监听已关闭".to_owned()))
    }

    /// 打洞信令：向 hbbs 发 `PunchHoleRequest`，等 `PunchHoleResponse` 并按 oneof 分流。
    ///
    /// # 为什么这条 TCP 连接必须保持打开直到读到应答
    ///
    /// hbbs 靠这条连接的**源地址**在 `sinks` 表里找回路由（`push_to_connection`）：目标端的
    /// `PunchHoleSent` 经 hbbs 转成 `PunchHoleResponse` 后，正是推回**发起请求的那条连接**。若
    /// 提前关闭，应答就找不到路由（静默丢弃），控制器只会看到超时。读完再关。
    ///
    /// # 关键分流
    ///
    /// 改动 (b) 之后 `PunchHoleResponse.socket_addr` **一个字段两种含义**，只能靠 oneof 判别：
    ///
    /// - `IsLocal(true)` → 目标端**内网 TCP 监听地址** → [`PunchSignal::Local`]（TCP 直连）；
    /// - 其余（`None` / `NatType(_)`）→ 目标端**已登记的 UDP 映射** → [`PunchSignal::Punch`]
    ///   （UDP 打洞）。**绝不要**对它做 `TcpStream::connect`：那会白等满
    ///   [`DIRECT_CONNECT_TIMEOUT`] 才去尝试打洞。
    ///
    /// `failure != 0`、地址解不出、超时、连接失败一律返回 `None`（调用方回退中继）。
    async fn signal_punch(&self, peer_rd_id: &str) -> Option<PunchSignal> {
        let mut stream = match TcpStream::connect(self.hbbs).await {
            Ok(s) => s,
            Err(err) => {
                debug!(error = %err, "连接会合服务器失败");
                return None;
            }
        };
        let msg = RendezvousMessage::with(Union::PunchHoleRequest(PunchHoleRequest {
            id: peer_rd_id.to_owned(),
            licence_key: self.cfg.licence_key.clone(),
            version: CLIENT_VERSION.to_owned(),
            // 公网观测端口（见 `udp_port_for_peer`），不是本地端口。
            udp_port: self.udp_port_for_peer(),
            ..Default::default()
        }));
        let Ok(wire) = encode_message(&msg) else {
            return None;
        };
        if let Err(err) = stream.write_all(&wire).await {
            debug!(error = %err, "发送打洞请求失败");
            return None;
        }
        if let Err(err) = stream.flush().await {
            debug!(error = %err, "刷新打洞请求失败");
            return None;
        }

        // 短窗口等待：同内网/跨 NAT 都会回（见常量文档），网络故障则放弃、回退中继。
        let response = match timeout(DIRECT_SIGNAL_TIMEOUT, read_message(&mut stream)).await {
            Ok(Ok(msg)) => msg,
            Ok(Err(err)) => {
                debug!(error = %err, "读打洞应答失败");
                return None;
            }
            Err(_) => {
                debug!("等待打洞应答超时，回退中继");
                return None;
            }
        };
        let Some(Union::PunchHoleResponse(resp)) = response.union else {
            return None;
        };
        if resp.failure != 0 {
            debug!(failure = resp.failure, "打洞请求被服务端拒绝");
            return None;
        }
        let Some(target) = addr::decode(&resp.socket_addr) else {
            debug!("打洞应答里的地址无法解出");
            return None;
        };
        match resp.union {
            Some(punch_hole_response::Union::IsLocal(true)) => Some(PunchSignal::Local(target)),
            _ => Some(PunchSignal::Punch(target)),
        }
    }

    /// TCP 直连一个已知目标（同内网路径）。失败返回 `None`。
    async fn try_direct_tcp(&self, target: SocketAddr) -> Option<TcpStream> {
        match timeout(DIRECT_CONNECT_TIMEOUT, TcpStream::connect(target)).await {
            Ok(Ok(stream)) => {
                let _ = stream.set_nodelay(true);
                Some(stream)
            }
            Ok(Err(err)) => {
                debug!(error = %err, %target, "直连失败，回退中继");
                None
            }
            Err(_) => {
                debug!(%target, "直连超时，回退中继");
                None
            }
        }
    }

    /// UDP 打洞（跨 NAT 路径）。失败返回 `None`。
    ///
    /// 打洞前先过 [`NatCache::should_punch`]：对称 NAT 直接回 `None`（**零额外等待**直接回中继），
    /// 该对端在 TTL 内刚失败过也回 `None`（省下一次白付的 [`PUNCH_BUDGET`]）。
    async fn try_punch(&self, cookie: [u8; 8], peer_rd_id: &str, target: SocketAddr) -> Option<DuplexStream> {
        if !self.nat_cache.should_punch(peer_rd_id, self.nat_class) {
            debug!(%peer_rd_id, "NAT 预判/失败缓存判定不打洞，直接回退中继");
            return None;
        }
        let conn_id = u64::from_be_bytes(cookie);
        let rx = self.register_punch_session(conn_id).await;
        match reliable::connect(self.udp.clone(), conn_id, rx, target, PUNCH_BUDGET).await {
            Ok(stream) => {
                self.nat_cache.record_success(peer_rd_id);
                Some(stream)
            }
            Err(err) => {
                self.nat_cache.record_failure(peer_rd_id);
                debug!(error = %err, %target, "UDP 打洞失败，回退中继");
                None
            }
        }
    }

    /// 被控端的建立：`always_use_relay` 时直接走中继；否则三个候选**并发竞争**。
    ///
    /// `always_use_relay` 是**两侧**的 kill switch。补上被控端这一侧（旧实现忽略它，导致规则
    /// 不对称：控制器强制中继、被控端仍去打洞/直连）。
    async fn establish_controlled(&self, request: &EstablishRequest) -> RemoteResult<EstablishedStream> {
        if self.cfg.always_use_relay {
            self.wait_relay_push(&request.session_id, RELAY_WAIT).await?;
            let stream = self.connect_relay(&request.session_id, &[]).await?;
            return Ok(EstablishedStream {
                stream: Box::new(stream),
                path: TransportPath::Relay,
            });
        }

        // 三个候选：UDP 打洞 accept、TCP 直连接入、中继推送。谁先到用谁。
        //
        // 打洞只在 `prefer_direct` 且本端**不是**对称 NAT 时启用：本端对称 NAT 下打洞无望，
        // 省下 3 秒预算与无用的反向探测。
        //
        // 被控端**不用** `nat_cache` 的失败缓存：`EstablishRequest.peer_rd_id` 在被控端是空串
        // （`RemoteSessionIncoming` 不带 `from_rd_id`），拿空串当 key 会把「所有对端」一起抑制
        // 10 分钟。
        let punch_enabled = request.prefer_direct && self.nat_class != NatClass::Symmetric;
        let conn_id = u64::from_be_bytes(request.punch_cookie);
        let punch_fut = async {
            if punch_enabled {
                let rx = self.register_punch_session(conn_id).await;
                reliable::accept(self.udp.clone(), conn_id, rx, PUNCH_BUDGET).await
            } else {
                // 由 `if punch_enabled` 守卫，本分支永不被轮询；仅用于统一类型。
                std::future::pending().await
            }
        };
        let direct_fut = self.next_direct();
        let relay_fut = self.wait_relay_push(&request.session_id, RELAY_WAIT);
        // **必须先建好再 pin**：`wait_relay_push` 内部自己算 `Instant::now() + wait` 的 deadline。
        // 若每轮 `select!` 里重新调用它，超时会一轮一轮往后推（总等待变成 25s + 25s …）。pin 之后
        // 用 `&mut` 跨轮次复用同一个 future。
        tokio::pin!(punch_fut, direct_fut, relay_fut);

        let mut punch_active = punch_enabled;
        let mut direct_active = true;
        let candidate = loop {
            tokio::select! {
                res = &mut punch_fut, if punch_active => match res {
                    Ok(stream) => break Candidate::Punch(stream),
                    Err(err) => {
                        // 打洞失败**必须继续等** direct/relay，不能直接返回错误。
                        debug!(error = %err, "UDP 打洞接受失败，继续等直连/中继");
                        punch_active = false;
                    }
                },
                res = &mut direct_fut, if direct_active => match res {
                    Ok(stream) => break Candidate::Direct(stream),
                    Err(err) => {
                        debug!(error = %err, "直连接入通道已关闭");
                        direct_active = false;
                    }
                },
                res = &mut relay_fut => {
                    res?;
                    break Candidate::Relay;
                }
            }
        };

        Ok(match candidate {
            Candidate::Punch(stream) => EstablishedStream {
                stream: Box::new(stream),
                path: TransportPath::Punch,
            },
            Candidate::Direct(stream) => {
                let _ = stream.set_nodelay(true);
                EstablishedStream {
                    stream: Box::new(stream),
                    path: TransportPath::Direct,
                }
            }
            Candidate::Relay => {
                let stream = self.connect_relay(&request.session_id, &[]).await?;
                EstablishedStream {
                    stream: Box::new(stream),
                    path: TransportPath::Relay,
                }
            }
        })
    }
}

#[async_trait]
impl TransportEstablisher for RustDeskTransport {
    async fn establish(&self, request: EstablishRequest, role: Role) -> RemoteResult<EstablishedStream> {
        match role {
            Role::Controller => {
                // 三条路径按优先级尝试：UDP 打洞 → 同内网 TCP 直连 → hbbr 中继兜底。
                if request.prefer_direct && !self.cfg.always_use_relay {
                    match self.signal_punch(&request.peer_rd_id).await {
                        Some(PunchSignal::Local(addr)) => {
                            if let Some(mut stream) = self.try_direct_tcp(addr).await {
                                // 直连路径没有中继帧可流水线，补写 Hello。
                                if !request.early.is_empty() {
                                    stream.write_all(&request.early).await?;
                                    stream.flush().await?;
                                }
                                return Ok(EstablishedStream {
                                    stream: Box::new(stream),
                                    path: TransportPath::Direct,
                                });
                            }
                        }
                        Some(PunchSignal::Punch(addr)) => {
                            if let Some(mut stream) = self.try_punch(request.punch_cookie, &request.peer_rd_id, addr).await
                            {
                                // 打洞路径同样没有中继帧可流水线，补写 Hello。
                                if !request.early.is_empty() {
                                    stream.write_all(&request.early).await?;
                                    stream.flush().await?;
                                }
                                return Ok(EstablishedStream {
                                    stream: Box::new(stream),
                                    path: TransportPath::Punch,
                                });
                            }
                        }
                        // 无应答/被拒/直连失败/打洞失败 → 直接回退中继（不额外等待）。
                        None => {}
                    }
                }
                self.notify_relay(&request.peer_rd_id, &request.session_id).await?;
                let stream = self.connect_relay(&request.session_id, &request.early).await?;
                Ok(EstablishedStream {
                    stream: Box::new(stream),
                    path: TransportPath::Relay,
                })
            }
            Role::Controlled => self.establish_controlled(&request).await,
        }
    }
}

/// 探测到 `remote` 的可达本机 IP（用于直连监听地址的对外呈现）。
async fn probe_local_ip(remote: SocketAddr) -> std::net::IpAddr {
    match TcpStream::connect(remote).await {
        Ok(stream) => stream.local_addr().map_or_else(|_| std::net::IpAddr::from([0, 0, 0, 0]), |a| a.ip()),
        Err(_) => std::net::IpAddr::from([0, 0, 0, 0]),
    }
}

/// 后台 demux 任务：**唯一**的 UDP `recv_from` 读者——UDP 心跳、服务端推送、打洞数据报全在这里分派。
///
/// # 每条数据报的分派顺序（**不可乱**）
///
/// 1. **可靠层数据报**：`peek_conn_id` 命中（前 4 字节 == `RTP1` 且长度 ≥ 27）→ 按 `conn_id` 路由。
///    命中即 `continue`。
/// 2. **未命中则继续往下走 protobuf 路径**（**不要直接丢**）。理由：`peek_conn_id` 只做廉价判定，
///    理论上存在首 4 字节恰为 `RTP1` 的合法会合消息；落回 protobuf 解析的代价是零（真打洞数据报
///    会解析失败被丢），却能保证**心跳路径逐字节不变**。
/// 3. **protobuf 路径**：与接线前**逐字节一致**，排最后。
///
/// # 安全不变量（配机械锁）
///
/// 未命中登记表的 `conn_id` **永远不被投递给任何会话、且永远不产生任何回复**（反反射）。解析失败
/// / 未知变体一律静默丢弃。
async fn udp_demux_task(
    udp: Arc<UdpSocket>,
    cfg: SignalingConfig,
    hbbs: SocketAddr,
    direct_addr: SocketAddr,
    pushes: mpsc::UnboundedSender<RendezvousMessage>,
    punch_registry: Arc<PunchRegistry>,
    cancel: CancellationToken,
) {
    let device_uuid = uuid::Uuid::new_v4().into_bytes().to_vec();
    let mut pk = vec![0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut pk);

    let mut ticker = interval(REGISTER_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut serial: i32 = 0;
    let mut buf = vec![0u8; 64 * 1024];

    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            _ = ticker.tick() => {
                serial = serial.wrapping_add(1);
                let msg = RendezvousMessage::with(Union::RegisterPeer(RegisterPeer {
                    id: cfg.rd_id.clone(),
                    serial,
                }));
                if udp.send_to(&msg.encode_to_vec(), hbbs).await.is_err() {
                    debug!("心跳发送失败");
                }
            }
            received = udp.recv_from(&mut buf) => {
                let Ok((len, from)) = received else { continue };
                let Some(payload) = buf.get(..len) else { continue };

                // 1) 可靠层快速路径：magic 命中 → 按 conn_id 路由（未命中则落回 protobuf）。
                if let Some(conn_id) = punch::peek_conn_id(payload) {
                    if punch_registry.route(conn_id, from, bytes::Bytes::copy_from_slice(payload)).await {
                        continue;
                    }
                }

                // 2/3) protobuf 路径（与接线前逐字节一致）。
                let Ok(msg) = RendezvousMessage::decode(payload) else { continue };
                match msg.union.clone() {
                    Some(Union::RegisterPeerResponse(resp)) if resp.request_pk => {
                        let rk = RendezvousMessage::with(Union::RegisterPk(RegisterPk {
                            id: cfg.rd_id.clone(),
                            uuid: device_uuid.clone(),
                            pk: pk.clone(),
                            ..Default::default()
                        }));
                        if udp.send_to(&rk.encode_to_vec(), hbbs).await.is_err() {
                            debug!("公钥登记发送失败");
                        }
                    }
                    Some(Union::FetchLocalAddr(fla)) => {
                        // 同内网直连：回报本端监听地址。
                        tokio::spawn(reply_local_addr(hbbs, cfg.rd_id.clone(), direct_addr, fla));
                    }
                    Some(Union::RequestRelay(_)) => {
                        // 交给等待中的 establish；无人接收说明会话已结束。
                        let dropped = pushes.send(msg).is_err();
                        if dropped {
                            debug!("中继推送无人接收，丢弃");
                        }
                    }
                    Some(Union::PunchHole(ph)) => {
                        // 1) 回 PunchHoleSent：让 hbbs 把「我们已登记的 UDP 映射」回给请求方。
                        tokio::spawn(reply_punch_hole_sent(hbbs, cfg.rd_id.clone(), ph.clone()));
                        // 2) 反向打洞：打开本端 NAT 的过滤窗口（见 `punch::punch_back`）。
                        //    `udp_port` 必须是**合法非零**端口才反向打：0 是老请求方没报端口，
                        //    负数/超界是恶意或损坏的 `PunchHole`——直接打向端口 0 只会白跑满预算。
                        let port = u16::try_from(ph.udp_port).ok().filter(|p| *p != 0);
                        if let (Some(port), Some(ip)) = (port, addr::decode(&ph.socket_addr).map(|a| a.ip())) {
                            tokio::spawn(punch::punch_back(udp.clone(), SocketAddr::new(ip, port), PUNCH_BUDGET));
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    debug!("UDP demux 任务结束");
}

/// 回应 `PunchHole`：把本端 ID 与**原样回显的请求方地址**经 TCP 回报给 hbbs。
///
/// **`socket_addr` 必须原样回显 `ph.socket_addr`**：hbbs 的 `forward_punch_result` 靠解码它去
/// `sinks` 表（按信令 TCP 源地址索引）里找请求方那条连接。填错/填空就找不到路由，请求方永远
/// 收不到 `PunchHoleResponse`，于是白等满 [`DIRECT_SIGNAL_TIMEOUT`] 才回退中继。
///
/// 形状照抄既有 [`reply_local_addr`]：新建一条 TCP 连 hbbs、发一帧、不管应答。
async fn reply_punch_hole_sent(hbbs: SocketAddr, rd_id: String, ph: PunchHole) {
    let Ok(mut stream) = TcpStream::connect(hbbs).await else {
        debug!("回报打洞结果时连接失败");
        return;
    };
    let msg = RendezvousMessage::with(Union::PunchHoleSent(PunchHoleSent {
        socket_addr: ph.socket_addr,
        id: rd_id,
        version: CLIENT_VERSION.to_owned(),
        ..Default::default()
    }));
    if let Ok(wire) = encode_message(&msg) {
        if stream.write_all(&wire).await.is_err() {
            warn!("回报打洞结果失败");
        }
    }
}

/// NAT 预判：**并发**向 hbbs 主端口与 NAT 探测端口（主端口 − 1）各发一条 `TestNatRequest`，
/// 再比对服务端观测到的源端口。
///
/// 返回 `(分类, 公网观测端口)`；观测端口取**主端口那次**应答（cone 下两次相同；对称 NAT 本来就
/// 不打洞，这个值不必可信）。
///
/// # 为什么并发而不是串行
///
/// `new()` 在会话关键路径上（`LazyTransport` 拿到 `RemoteCapabilities` 即构造）。串行最坏要
/// 2 × [`NAT_PROBE_TIMEOUT`]，白白拖慢每一次建立。
///
/// # 为什么必须在 spawn demux 之前做完
///
/// `udp_demux_task` 是 UDP 套接字**唯一**的 `recv_from` 读者（否则两条读路径会互相偷走数据报）。
/// 本函数自己 `recv_from`，故必须在 demux 起之前完成。
///
/// # 老服务端 / 探测端口被墙
///
/// 超时 → [`NatClass::Unknown`] → **fail-open**，照打一次洞（见 [`crate::nat`]）。
async fn probe_nat(udp: &UdpSocket, hbbs: SocketAddr, rendezvous_port: u16) -> (NatClass, Option<u16>) {
    // `rendezvous_port - 1` 下溢保护：端口 0/1 时无法探测。
    if rendezvous_port < 2 {
        return (NatClass::Unknown, None);
    }
    let nat_port = rendezvous_port - 1;
    let nat_addr = SocketAddr::new(hbbs.ip(), nat_port);

    // 两条探测**并发**发出（同一套接字、两个不同目的端口——这是分类成立的前提，见 `nat`）。
    let req_main = RendezvousMessage::with(Union::TestNatRequest(TestNatRequest { serial: 1 })).encode_to_vec();
    let req_nat = RendezvousMessage::with(Union::TestNatRequest(TestNatRequest { serial: 2 })).encode_to_vec();
    if udp.send_to(&req_main, hbbs).await.is_err() || udp.send_to(&req_nat, nat_addr).await.is_err() {
        return (NatClass::Unknown, None);
    }

    let deadline = Instant::now() + NAT_PROBE_TIMEOUT;
    let mut port_main = None;
    let mut port_nat = None;
    let mut buf = vec![0u8; 2048];
    while port_main.is_none() || port_nat.is_none() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let Ok(Ok((len, from))) = timeout(remaining, udp.recv_from(&mut buf)).await else {
            break;
        };
        // 只接受来自 hbbs 主机的应答（挡掉无关流量）。
        if from.ip() != hbbs.ip() {
            continue;
        }
        let Ok(msg) = RendezvousMessage::decode(&buf[..len]) else {
            continue;
        };
        let Some(Union::TestNatResponse(resp)) = msg.union else {
            continue;
        };
        // 应答的**源端口**等于我们探测的目的端口，据此把两条应答对上两个目的地。
        let observed = u16::try_from(resp.port).ok();
        if from.port() == hbbs.port() {
            port_main = observed;
        } else if from.port() == nat_port {
            port_nat = observed;
        }
    }
    (classify(port_main, port_nat), port_main)
}

/// 回应 `FetchLocalAddr`：把本端内网监听地址经 TCP 回报给 hbbs。
async fn reply_local_addr(hbbs: SocketAddr, rd_id: String, direct_addr: SocketAddr, fla: FetchLocalAddr) {
    let Ok(mut stream) = TcpStream::connect(hbbs).await else {
        debug!("回报内网地址时连接失败");
        return;
    };
    let msg = RendezvousMessage::with(Union::LocalAddr(LocalAddr {
        socket_addr: fla.socket_addr,
        local_addr: addr::encode(direct_addr),
        id: rd_id,
        version: CLIENT_VERSION.to_owned(),
        ..Default::default()
    }));
    if let Ok(wire) = encode_message(&msg) {
        if stream.write_all(&wire).await.is_err() {
            warn!("回报内网地址失败");
        }
    }
}

/// 后台直连接受任务。
async fn accept_task(listener: TcpListener, tx: mpsc::UnboundedSender<TcpStream>, cancel: CancellationToken) {
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        if tx.send(stream).is_err() {
                            break;
                        }
                    }
                    Err(err) => debug!(error = %err, "直连 accept 失败"),
                }
            }
        }
    }
    debug!("直连接受任务结束");
}

/// 延迟构造的 [`RustDeskTransport`]。
///
/// [`RemoteManager`](crate::RemoteManager) 构造时就需要 `Arc<dyn TransportEstablisher>`，而
/// [`RustDeskTransport::new`] 依赖 [`RemoteParams`]（会合/中继地址、`licence_key`）——它要等
/// 控制通道下发 `RemoteCapability` 才有。本类型把构造推迟到参数到手，并在拿到参数时**立即**
/// 构造（见 [`TransportEstablisher::prepare`] 的文档），让本端提前登记到会合服务器。
///
/// 按本端 `rd_id` 缓存：`rd_id` 变化（重新注册、换服务端）时重建。
pub struct LazyTransport {
    /// 本机服务端 `host:port`（`params.rendezvous_host` 留空时补齐）。
    server_host: String,
    /// 已构造的传输及其对应的本端 `rd_id`。
    inner: Mutex<Option<(String, std::sync::Arc<RustDeskTransport>)>>,
}

impl LazyTransport {
    /// 以本机服务端地址构造。
    #[must_use]
    pub fn new(server_host: String) -> Self {
        Self {
            server_host,
            inner: Mutex::new(None),
        }
    }

    /// 取（必要时构造并缓存）`local_rd_id` 对应的传输。
    ///
    /// [`RustDeskTransport::new`] 构造时即向会合服务器登记，因此「构造」就等于「上线」。
    ///
    /// # Errors
    /// 参数无法解析出地址、或套接字绑定失败时返回 [`RemoteError`]。
    async fn transport_for(
        &self,
        params: &RemoteParams,
        local_rd_id: &str,
    ) -> RemoteResult<std::sync::Arc<RustDeskTransport>> {
        let mut guard = self.inner.lock().await;
        if let Some((cached_rd_id, transport)) = guard.as_ref() {
            if cached_rd_id == local_rd_id {
                return Ok(std::sync::Arc::clone(transport));
            }
        }
        let config = SignalingConfig::from_params(params, local_rd_id.to_owned(), &self.server_host);
        let transport = RustDeskTransport::new(config).await?;
        *guard = Some((local_rd_id.to_owned(), std::sync::Arc::clone(&transport)));
        Ok(transport)
    }
}

#[async_trait]
impl TransportEstablisher for LazyTransport {
    /// 提前构造传输 = 提前向会合服务器登记。
    ///
    /// **必须做**：hbbs 对未登记的对端会静默丢弃中继请求，而控制器的中继请求往往在目标登记
    /// 之前就发出（详见 [`TransportEstablisher::prepare`]）。管理器在收到 `RemoteCapability`
    /// 与打开「允许被远程控制」开关时都会调用本方法。
    async fn prepare(&self, params: &RemoteParams, local_rd_id: &str) -> RemoteResult<()> {
        let transport = self.transport_for(params, local_rd_id).await?;
        debug!(
            rd_id = %local_rd_id,
            direct = %transport.direct_addr(),
            "远程数据面已预登记到会合服务器"
        );
        Ok(())
    }

    async fn establish(&self, request: EstablishRequest, role: Role) -> RemoteResult<EstablishedStream> {
        let transport = self.transport_for(&request.params, &request.local_rd_id).await?;
        transport.establish(request, role).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::punch::{conn_id_from_token, peek_conn_id};
    use crate::reliable::{self, DatagramSender, SEGMENT_HEADER_LEN};
    use bytes::Bytes;
    use rust_tunnel_rustdesk::proto::{PunchHoleResponse, TestNatResponse};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex as StdMutex;

    #[test]
    fn signaling_config_fills_host_from_local_when_params_empty() {
        let params = RemoteParams {
            rendezvous_host: String::new(),
            rendezvous_port: 21116,
            relay_port: 21117,
            relay_server: String::new(),
            public_key: "key".into(),
            always_use_relay: false,
        };
        let cfg = SignalingConfig::from_params(&params, "123456789".into(), "tunnel.example:8080");
        assert_eq!(cfg.host, "tunnel.example:8080");
        assert_eq!(cfg.licence_key, "key");
        assert_eq!(cfg.rendezvous_port, 21116);
    }

    #[test]
    fn signaling_config_prefers_params_host() {
        let params = RemoteParams {
            rendezvous_host: "hbbs.example".into(),
            rendezvous_port: 21116,
            relay_port: 21117,
            relay_server: "relay.example".into(),
            public_key: String::new(),
            always_use_relay: true,
        };
        let cfg = SignalingConfig::from_params(&params, "x".into(), "ignored");
        assert_eq!(cfg.host, "hbbs.example");
        assert_eq!(cfg.relay_server, "relay.example");
        assert!(cfg.always_use_relay);
    }

    #[tokio::test]
    async fn resolve_accepts_host_with_port() {
        // GUI 传进来的 `server_host` 是 `config.server`，即 `host:port`。若 `resolve`
        // 把整串当主机名交给 getaddrinfo，会解析失败，导致会话永远建不起来。
        let addr = resolve("127.0.0.1:8080", 21116).await;
        assert_eq!(
            addr.ok(),
            Some("127.0.0.1:21116".parse().unwrap()),
            "带端口的 host 应被剥掉端口后解析"
        );
    }

    #[test]
    fn strip_port_handles_hosts_and_ipv6() {
        assert_eq!(strip_port("hbbs.example:8080"), "hbbs.example");
        assert_eq!(strip_port("127.0.0.1:3000"), "127.0.0.1");
        assert_eq!(strip_port("hbbs.example"), "hbbs.example");
        assert_eq!(strip_port("127.0.0.1"), "127.0.0.1");
        // 裸 IPv6 的冒号不是端口分隔符，不能误剥。
        assert_eq!(strip_port("::1"), "::1");
        assert_eq!(strip_port("2001:db8::1"), "2001:db8::1");
        // 带方括号的 IPv6 字面量。
        assert_eq!(strip_port("[::1]:8080"), "::1");
        assert_eq!(strip_port("[::1]"), "::1");
    }

    #[test]
    fn encode_message_produces_framed_protobuf() {        let msg = RendezvousMessage::with(Union::RegisterPeer(RegisterPeer {
            id: "123456789".into(),
            serial: 1,
        }));
        let wire = encode_message(&msg).unwrap();
        // 首字节低 2 位是头宽标记；载荷应能被解回同一消息。
        let mut buf = BytesMut::from(&wire[..]);
        let mut codec = rust_tunnel_rustdesk::frame::FrameCodec::new();
        let payload = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(
            RendezvousMessage::decode(&payload[..]).unwrap(),
            msg,
            "帧应能解回原消息"
        );
    }

    /// hbbs **永不回** `PunchHoleResponse`（网络故障 / 静默服务端）：`signal_punch` 必须在**短窗口**
    /// 内放弃并回退中继。
    ///
    /// 回归用例：若沿用 `RELAY_WAIT`(25s)，控制器要空等 25s 才去发中继信令，而被控端的
    /// `wait_relay_push(RELAY_WAIT)` 几乎同时超时——跨 NAT（最常见的部署形态）下会话**必现失败**，
    /// 用户看到的是「卡在连接中」25 秒。
    #[tokio::test]
    async fn try_direct_gives_up_quickly_when_hbbs_never_answers() {
        // 假 hbbs：接受 TCP 连接后保持沉默。
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hbbs_port = listener.local_addr().unwrap().port();
        let held = tokio::spawn(async move {
            let mut keep = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                // 拿着不放、也不回任何字节。
                keep.push(stream);
            }
        });

        let transport = RustDeskTransport::new(SignalingConfig {
            rd_id: "111111111".into(),
            licence_key: String::new(),
            host: "127.0.0.1".into(),
            rendezvous_port: hbbs_port,
            relay_port: hbbs_port,
            relay_server: String::new(),
            always_use_relay: false,
        })
        .await
        .unwrap();

        let started = Instant::now();
        let result = transport.signal_punch("222222222").await;
        let elapsed = started.elapsed();
        held.abort();

        assert!(result.is_none(), "无应答时应放弃打洞/直连、回退中继");
        assert!(
            elapsed >= DIRECT_SIGNAL_TIMEOUT,
            "不该在应答窗口结束前就放弃（实测 {elapsed:?}）"
        );
        assert!(
            elapsed < Duration::from_secs(6),
            "必须在远小于 RELAY_WAIT({RELAY_WAIT:?}) 的时间内放弃，实测 {elapsed:?}"
        );
    }

    // ─────────────── 假 hbbs / 假 hbbr / 假对端（真实 UDP+TCP，本地回环） ───────────────

    /// 假 hbbs 的共享状态。
    #[derive(Default)]
    struct HbbsState {
        /// 收到的全部 TCP 会合消息（断言用）。
        received: StdMutex<Vec<RendezvousMessage>>,
        /// 最近一次 NAT 探测的观测源（= 客户端 UDP 套接字地址）。
        client_udp: StdMutex<Option<SocketAddr>>,
        /// 收到 `PunchHoleRequest` 时回给请求方的应答（`None` = 保持沉默）。
        punch_response: StdMutex<Option<PunchHoleResponse>>,
        /// 两个 NAT 探测端口是否回**不同**的观测端口（模拟对称 NAT）。
        symmetric_nat: AtomicBool,
        /// 是否完全不回 NAT 探测应答（模拟老服务端 / 端口被墙）。
        silent_nat: AtomicBool,
    }

    /// 假 hbbs（UDP 主端口 + UDP NAT 端口 + TCP 信令端口）+ 假 hbbr（TCP 中继端口）。
    struct FakeHbbs {
        /// 会合主端口（= UDP 主端口 = TCP 信令端口）。
        main_port: u16,
        /// 假 hbbr 中继端口。
        relay_port: u16,
        /// 共享状态。
        state: Arc<HbbsState>,
        /// 后台任务（`Drop` 时全部 abort）。
        tasks: Vec<JoinHandle<()>>,
    }

    impl Drop for FakeHbbs {
        fn drop(&mut self) {
            for task in &self.tasks {
                task.abort();
            }
        }
    }

    impl FakeHbbs {
        async fn start() -> Self {
            // NAT 探测端口必须 = 主端口 − 1（`probe_nat` 的硬约束），故先绑主端口再绑 main−1。
            let (udp_main, udp_nat, main_port) = bind_udp_pair().await;
            let tcp = TcpListener::bind(("127.0.0.1", main_port)).await.unwrap();
            let hbbr = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let relay_port = hbbr.local_addr().unwrap().port();
            let state = Arc::new(HbbsState::default());

            let mut tasks = Vec::new();
            tasks.push(tokio::spawn(nat_responder(udp_main, Arc::clone(&state), 0)));
            // NAT 端口用 delta=1：`symmetric_nat` 打开时两个端口观测不同 → 对称 NAT。
            tasks.push(tokio::spawn(nat_responder(udp_nat, Arc::clone(&state), 1)));
            tasks.push(tokio::spawn(tcp_handler(tcp, Arc::clone(&state))));
            tasks.push(tokio::spawn(async move {
                // 假 hbbr：接受连接后拿着不放（中继路径只需 TCP 连上）。
                let mut keep = Vec::new();
                while let Ok((stream, _)) = hbbr.accept().await {
                    keep.push(stream);
                }
            }));
            Self {
                main_port,
                relay_port,
                state,
                tasks,
            }
        }

        fn make_config(&self, always_use_relay: bool) -> SignalingConfig {
            SignalingConfig {
                rd_id: "111111111".into(),
                licence_key: String::new(),
                host: "127.0.0.1".into(),
                rendezvous_port: self.main_port,
                relay_port: self.relay_port,
                relay_server: "127.0.0.1".into(),
                always_use_relay,
            }
        }

        async fn make_transport(&self) -> Arc<RustDeskTransport> {
            self.make_transport_with(false).await
        }

        async fn make_transport_with(&self, always_use_relay: bool) -> Arc<RustDeskTransport> {
            RustDeskTransport::new(self.make_config(always_use_relay))
                .await
                .unwrap()
        }

        /// 配置 `PunchHoleRequest` 的应答（`socket_addr` 会被编码）。
        fn set_punch_response(&self, socket_addr: SocketAddr, union: Option<punch_hole_response::Union>) {
            let resp = PunchHoleResponse {
                socket_addr: addr::encode(socket_addr),
                union,
                ..Default::default()
            };
            *self.state.punch_response.lock().unwrap() = Some(resp);
        }

        fn received(&self) -> Vec<RendezvousMessage> {
            self.state.received.lock().unwrap().clone()
        }

        fn client_udp(&self) -> Option<SocketAddr> {
            *self.state.client_udp.lock().unwrap()
        }
    }

    /// 绑一对相邻的 UDP 端口（`main` 与 `main − 1`）。
    async fn bind_udp_pair() -> (UdpSocket, UdpSocket, u16) {
        for _ in 0..100 {
            let main = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let port = main.local_addr().unwrap().port();
            if port >= 2 {
                if let Ok(nat) = UdpSocket::bind(("127.0.0.1", port - 1)).await {
                    return (main, nat, port);
                }
            }
        }
        panic!("无法绑到相邻的 UDP 端口对");
    }

    /// NAT 探测应答器：回 `from.port() + bump`（`symmetric_nat` 下两个端口 bump 不同）。
    async fn nat_responder(udp: UdpSocket, state: Arc<HbbsState>, delta: i32) {
        let mut buf = vec![0u8; 2048];
        loop {
            let Ok((len, from)) = udp.recv_from(&mut buf).await else {
                continue;
            };
            let Ok(msg) = RendezvousMessage::decode(&buf[..len]) else {
                continue;
            };
            if !matches!(msg.union, Some(Union::TestNatRequest(_))) {
                continue;
            }
            // 即便保持沉默也要记下观测源（未知分类的用例要用它当「本地端口」）。
            if let Ok(mut guard) = state.client_udp.lock() {
                *guard = Some(from);
            }
            if state.silent_nat.load(Ordering::SeqCst) {
                continue;
            }
            let bump = if state.symmetric_nat.load(Ordering::SeqCst) {
                delta
            } else {
                0
            };
            let port = i32::from(from.port()).saturating_add(bump);
            let resp = RendezvousMessage::with(Union::TestNatResponse(TestNatResponse {
                port,
                ..Default::default()
            }));
            let _ = udp.send_to(&resp.encode_to_vec(), from).await;
        }
    }

    /// 假 hbbs 的 TCP 信令处理：记录消息；`PunchHoleRequest` 时按配置回一条应答。
    async fn tcp_handler(listener: TcpListener, state: Arc<HbbsState>) {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                continue;
            };
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                loop {
                    let Ok(msg) = read_message(&mut stream).await else {
                        break;
                    };
                    let is_punch = matches!(msg.union, Some(Union::PunchHoleRequest(_)));
                    if let Ok(mut guard) = state.received.lock() {
                        guard.push(msg);
                    }
                    if is_punch {
                        let resp = state.punch_response.lock().ok().and_then(|guard| guard.clone());
                        if let Some(resp) = resp {
                            let wire = RendezvousMessage::with(Union::PunchHoleResponse(resp)).encode_to_vec();
                            if let Ok(framed) = frame(&wire) {
                                let _ = stream.write_all(&framed).await;
                                let _ = stream.flush().await;
                            }
                        }
                    }
                }
            });
        }
    }

    /// 轮询等待条件成立（有界）。
    async fn wait_for<T>(mut cond: impl FnMut() -> Option<T>) -> Option<T> {
        for _ in 0..200 {
            if let Some(v) = cond() {
                return Some(v);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        cond()
    }

    /// 造一个 UDP 套接字 + 自建 demux（`peek_conn_id` → `try_send`），返回套接字、地址、接收端。
    async fn spawn_udp_endpoint(conn_id: u64) -> (Arc<UdpSocket>, SocketAddr, DatagramRx) {
        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr = sock.local_addr().unwrap();
        let (tx, rx) = mpsc::channel(256);
        let demux_sock = Arc::clone(&sock);
        tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            loop {
                let Ok((len, from)) = demux_sock.recv_from(&mut buf).await else {
                    continue;
                };
                if peek_conn_id(&buf[..len]) == Some(conn_id) {
                    let _ = tx.try_send((from, Bytes::copy_from_slice(&buf[..len])));
                }
            }
        });
        (sock, addr, rx)
    }

    /// 假对端（被控端角色）：跑 `reliable::accept`。
    async fn spawn_fake_acceptor(conn_id: u64) -> (SocketAddr, JoinHandle<RemoteResult<DuplexStream>>) {
        let (sock, addr, rx) = spawn_udp_endpoint(conn_id).await;
        let link: Arc<dyn DatagramSender> = sock;
        let handle = tokio::spawn(async move { reliable::accept(link, conn_id, rx, PUNCH_BUDGET).await });
        (addr, handle)
    }

    /// 假对端（控制器角色）：跑 `reliable::connect`，向 `peer` 发 SYN。
    async fn spawn_fake_connector(
        conn_id: u64,
        peer: SocketAddr,
    ) -> (SocketAddr, JoinHandle<RemoteResult<DuplexStream>>) {
        let (sock, addr, rx) = spawn_udp_endpoint(conn_id).await;
        let link: Arc<dyn DatagramSender> = sock;
        let handle = tokio::spawn(async move { reliable::connect(link, conn_id, rx, peer, PUNCH_BUDGET).await });
        (addr, handle)
    }

    fn dummy_request(session_id: &str, peer_rd_id: &str, cookie: [u8; 8], early: Vec<u8>) -> EstablishRequest {
        EstablishRequest {
            session_id: session_id.to_owned(),
            local_rd_id: "111111111".to_owned(),
            peer_rd_id: peer_rd_id.to_owned(),
            params: RemoteParams {
                rendezvous_host: "127.0.0.1".into(),
                rendezvous_port: 21116,
                relay_port: 21117,
                relay_server: String::new(),
                public_key: String::new(),
                always_use_relay: false,
            },
            prefer_direct: true,
            early,
            punch_cookie: cookie,
        }
    }

    // ───────────────────────────── 打洞端到端 ─────────────────────────────

    /// **最重要的一条**：控制器打洞成功 → `TransportPath::Punch`，且流上双向字节真的通。
    #[tokio::test]
    async fn punch_succeeds_end_to_end_on_controller_side() {
        let hbbs = FakeHbbs::start().await;
        let conn_id = conn_id_from_token("session-token-abc");
        let (peer_addr, peer_handle) = spawn_fake_acceptor(conn_id).await;
        // union: None → socket_addr 是目标端已登记的 UDP 映射。
        hbbs.set_punch_response(peer_addr, None);
        let transport = hbbs.make_transport().await;

        let established = transport
            .establish(
                dummy_request("sess-1", "222222222", conn_id.to_be_bytes(), b"HELLO".to_vec()),
                Role::Controller,
            )
            .await
            .unwrap();
        assert_eq!(established.path, TransportPath::Punch, "应走 UDP 打洞路径");
        let mut stream = established.stream;

        // 对端应收到建立阶段补写的 early（打洞路径没有中继帧可流水线）。
        let mut peer = peer_handle.await.unwrap().unwrap();
        let mut hello = [0u8; 5];
        peer.read_exact(&mut hello).await.unwrap();
        assert_eq!(&hello, b"HELLO", "打洞路径也要补写 early（Hello）");

        // 双向字节真的通。
        peer.write_all(b"pong").await.unwrap();
        let mut back = [0u8; 4];
        stream.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"pong", "对端 → 控制器方向应通");
        stream.write_all(b"ping").await.unwrap();
        let mut fwd = [0u8; 4];
        peer.read_exact(&mut fwd).await.unwrap();
        assert_eq!(&fwd, b"ping", "控制器 → 对端方向应通");
    }

    /// 被控端角色：假对端跑 `connect`，被控端 `establish` 走打洞 `accept`。
    #[tokio::test]
    async fn punch_succeeds_end_to_end_on_controlled_side() {
        let hbbs = FakeHbbs::start().await;
        let conn_id = conn_id_from_token("token-controlled");
        let transport = hbbs.make_transport().await;
        let udp_addr = hbbs.client_udp().expect("NAT 探测应已记录客户端 UDP 地址");
        let (_peer_addr, peer_handle) = spawn_fake_connector(conn_id, udp_addr).await;

        let established = transport
            .establish(
                dummy_request("sess-2", "", conn_id.to_be_bytes(), Vec::new()),
                Role::Controlled,
            )
            .await
            .unwrap();
        assert_eq!(established.path, TransportPath::Punch, "被控端应走 UDP 打洞");
        let mut stream = established.stream;
        let mut peer = peer_handle.await.unwrap().unwrap();

        stream.write_all(b"from-controlled").await.unwrap();
        let mut buf = [0u8; 15];
        peer.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"from-controlled");
        peer.write_all(b"hi").await.unwrap();
        let mut reply = [0u8; 2];
        stream.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"hi");
    }

    /// `IsLocal(true)` → 该地址是内网 TCP 监听地址 → 走 TCP 直连，**不是**打洞。
    #[tokio::test]
    async fn is_local_response_takes_tcp_direct_path() {
        let hbbs = FakeHbbs::start().await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let direct_addr = listener.local_addr().unwrap();
        let accepted = tokio::spawn(async move { listener.accept().await.map(|(s, _)| s) });
        hbbs.set_punch_response(direct_addr, Some(punch_hole_response::Union::IsLocal(true)));
        let transport = hbbs.make_transport().await;

        let established = transport
            .establish(
                dummy_request("s", "222222222", conn_id_from_token("t").to_be_bytes(), Vec::new()),
                Role::Controller,
            )
            .await
            .unwrap();
        assert_eq!(
            established.path,
            TransportPath::Direct,
            "IsLocal(true) 应走 TCP 直连而非打洞"
        );
        assert!(accepted.await.unwrap().is_ok(), "控制器应真的连上目标监听");
    }

    /// 打洞失败必须在预算内**如实**回退中继（不谎报 `Punch`，也不拖到更久/报错）。
    #[tokio::test]
    async fn punch_failure_falls_back_to_relay_within_budget() {
        let hbbs = FakeHbbs::start().await;
        // 一个没人回应的 UDP 地址（绑住但不读不回，避免 ICMP 干扰）。
        let dead = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let dead_addr = dead.local_addr().unwrap();
        hbbs.set_punch_response(dead_addr, None);
        let transport = hbbs.make_transport().await;

        let started = Instant::now();
        let established = transport
            .establish(
                dummy_request("s", "222222222", conn_id_from_token("t").to_be_bytes(), Vec::new()),
                Role::Controller,
            )
            .await
            .unwrap();
        let elapsed = started.elapsed();
        assert_eq!(established.path, TransportPath::Relay, "打洞失败必须如实回退中继");
        assert!(
            elapsed < PUNCH_BUDGET + Duration::from_secs(2),
            "应在打洞预算 + 余量内回退，实测 {elapsed:?}"
        );
        drop(dead);
    }

    // ───────────────────────────── 反向打洞 ─────────────────────────────

    /// 收到 `PunchHole` 必须：① 回 `PunchHoleSent`（地址原样回显、id = 本端 rd_id）；
    /// ② 向请求方地址反向发裸探测段（27 字节、magic `RTP1`、`conn_id == 0`）。
    #[tokio::test]
    async fn punch_hole_triggers_reverse_punch_and_punch_hole_sent() {
        let hbbs = FakeHbbs::start().await;
        let _transport = hbbs.make_transport().await;
        let client_udp = hbbs.client_udp().expect("应记录客户端 UDP 地址");

        // 假请求方：一个 UDP 套接字（收反向探测）。
        let requester = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let requester_addr = requester.local_addr().unwrap();
        let ph_socket_addr = addr::encode(requester_addr);

        // 往 transport 的 UDP 套接字推一条 PunchHole。
        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let ph = RendezvousMessage::with(Union::PunchHole(PunchHole {
            socket_addr: ph_socket_addr.clone(),
            udp_port: i32::from(requester_addr.port()),
            ..Default::default()
        }))
        .encode_to_vec();
        sender.send_to(&ph, client_udp).await.unwrap();

        // (a) 假 hbbs 应收到 PunchHoleSent。
        let sent = wait_for(&|| {
            hbbs.received().iter().find_map(|m| match &m.union {
                Some(Union::PunchHoleSent(s)) => Some(s.clone()),
                _ => None,
            })
        })
        .await
        .expect("收到 PunchHole 后应回 PunchHoleSent");
        assert_eq!(sent.socket_addr, ph_socket_addr, "socket_addr 必须原样回显请求方地址");
        assert_eq!(sent.id, "111111111", "id 必须是本端 rd_id");

        // (b) 假请求方应收到反向探测。
        let mut buf = vec![0u8; 64];
        let (len, _from) = tokio::time::timeout(Duration::from_secs(2), requester.recv_from(&mut buf))
            .await
            .expect("假请求方应在预算内收到反向探测")
            .unwrap();
        assert_eq!(len, SEGMENT_HEADER_LEN, "反向探测应是 27 字节裸段");
        assert_eq!(&buf[..4], b"RTP1", "magic 必须正确");
        assert_eq!(peek_conn_id(&buf[..len]), Some(0), "反向探测 conn_id 必须是 0 哨兵");
    }

    /// `udp_port == 0`（老请求方没报端口）时**不**反向打洞（仍回 `PunchHoleSent`）。
    #[tokio::test]
    async fn punch_hole_with_zero_udp_port_does_not_reverse_punch() {
        let hbbs = FakeHbbs::start().await;
        let _transport = hbbs.make_transport().await;
        let client_udp = hbbs.client_udp().unwrap();
        let requester = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let requester_addr = requester.local_addr().unwrap();

        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let ph = RendezvousMessage::with(Union::PunchHole(PunchHole {
            socket_addr: addr::encode(requester_addr),
            udp_port: 0,
            ..Default::default()
        }))
        .encode_to_vec();
        sender.send_to(&ph, client_udp).await.unwrap();

        assert!(
            wait_for(&|| {
                hbbs.received()
                    .iter()
                    .any(|m| matches!(m.union, Some(Union::PunchHoleSent(_))))
                    .then_some(())
            })
            .await
            .is_some(),
            "仍应回 PunchHoleSent"
        );

        let mut buf = vec![0u8; 64];
        assert!(
            tokio::time::timeout(Duration::from_millis(600), requester.recv_from(&mut buf))
                .await
                .is_err(),
            "udp_port == 0 时不应反向打洞"
        );
    }

    /// **反反射**：合法格式但 `conn_id` 未登记的段，必须静默丢弃、绝不回复。
    #[tokio::test]
    async fn unregistered_conn_id_never_gets_a_reply() {
        let hbbs = FakeHbbs::start().await;
        let _transport = hbbs.make_transport().await;
        let client_udp = hbbs.client_udp().unwrap();

        let attacker = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let seg = reliable::SegmentHeader {
            conn_id: 0xDEAD_BEEF_DEAD_BEEF,
            seq: 0,
            ack: 0,
            ack_bits: 0,
            flags: reliable::SegmentHeader::SYN,
            window: 64,
            len: 0,
        }
        .encode();
        attacker.send_to(&seg, client_udp).await.unwrap();

        let mut buf = vec![0u8; 64];
        assert!(
            tokio::time::timeout(Duration::from_millis(600), attacker.recv_from(&mut buf))
                .await
                .is_err(),
            "未命中登记表的数据报必须静默丢弃、绝不回复"
        );
    }

    // ───────────────────────────── NAT 预判 ─────────────────────────────

    /// 两个目的端口观测到**相同**源端口 → `Cone`，且记下公网观测端口并填进 `PunchHoleRequest`。
    #[tokio::test]
    async fn nat_probe_classifies_cone_and_fills_observed_port() {
        let hbbs = FakeHbbs::start().await;
        let transport = hbbs.make_transport().await;
        assert_eq!(
            transport.nat_class_for_test(),
            NatClass::Cone,
            "两个目的端口观测一致应判为 cone"
        );
        let observed = hbbs.client_udp().unwrap().port();
        assert_eq!(
            transport.public_udp_port_for_test(),
            Some(observed),
            "应记下服务端观测到的源端口"
        );

        hbbs.set_punch_response("127.0.0.1:1".parse().unwrap(), Some(punch_hole_response::Union::IsLocal(true)));
        let _ = transport.signal_punch("222222222").await;
        let req = wait_for(&|| {
            hbbs.received().iter().find_map(|m| match &m.union {
                Some(Union::PunchHoleRequest(r)) => Some(r.clone()),
                _ => None,
            })
        })
        .await
        .expect("应收到 PunchHoleRequest");
        assert_eq!(req.udp_port, i32::from(observed), "udp_port 必须填公网观测端口");
    }

    /// 两个目的端口观测到**不同**源端口 → `Symmetric`（打洞无望）。
    #[tokio::test]
    async fn nat_probe_detects_symmetric_nat() {
        let hbbs = FakeHbbs::start().await;
        hbbs.state.symmetric_nat.store(true, Ordering::SeqCst);
        let transport = hbbs.make_transport().await;
        assert_eq!(
            transport.nat_class_for_test(),
            NatClass::Symmetric,
            "两个目的端口观测不同应判为对称 NAT"
        );

        hbbs.set_punch_response("127.0.0.1:1".parse().unwrap(), Some(punch_hole_response::Union::IsLocal(true)));
        let _ = transport.signal_punch("222222222").await;
        let req = wait_for(&|| {
            hbbs.received().iter().find_map(|m| match &m.union {
                Some(Union::PunchHoleRequest(r)) => Some(r.clone()),
                _ => None,
            })
        })
        .await
        .expect("应收到 PunchHoleRequest");
        // 本机无 NAT：观测端口 == 本地端口，故此处等价于「兜底本地端口」。
        let local = hbbs.client_udp().unwrap().port();
        assert_eq!(req.udp_port, i32::from(local));
    }

    /// 探测**无应答** → `Unknown`（fail-open），`udp_port` 兜底本地端口。
    #[tokio::test]
    async fn nat_probe_is_unknown_without_response() {
        let hbbs = FakeHbbs::start().await;
        hbbs.state.silent_nat.store(true, Ordering::SeqCst);
        let transport = hbbs.make_transport().await;
        assert_eq!(
            transport.nat_class_for_test(),
            NatClass::Unknown,
            "无应答应判为未知（fail-open）"
        );
        assert_eq!(transport.public_udp_port_for_test(), None, "无观测值");

        hbbs.set_punch_response("127.0.0.1:1".parse().unwrap(), Some(punch_hole_response::Union::IsLocal(true)));
        let _ = transport.signal_punch("222222222").await;
        let req = wait_for(&|| {
            hbbs.received().iter().find_map(|m| match &m.union {
                Some(Union::PunchHoleRequest(r)) => Some(r.clone()),
                _ => None,
            })
        })
        .await
        .expect("应收到 PunchHoleRequest");
        let local = hbbs.client_udp().unwrap().port();
        assert_eq!(req.udp_port, i32::from(local), "无观测值时 udp_port 兜底本地端口");
    }

    // ───────────────────────────── always_use_relay（两侧） ─────────────────────────────

    /// `always_use_relay` 控制器侧：直接走中继，**不**发 `PunchHoleRequest`。
    #[tokio::test]
    async fn always_use_relay_skips_punch_on_controller_side() {
        let hbbs = FakeHbbs::start().await;
        hbbs.set_punch_response("127.0.0.1:1".parse().unwrap(), None);
        let transport = hbbs.make_transport_with(true).await;

        let established = transport
            .establish(
                dummy_request("s", "222222222", conn_id_from_token("t").to_be_bytes(), Vec::new()),
                Role::Controller,
            )
            .await
            .unwrap();
        assert_eq!(established.path, TransportPath::Relay, "always_use_relay 应直接走中继");
        assert!(
            !hbbs
                .received()
                .iter()
                .any(|m| matches!(m.union, Some(Union::PunchHoleRequest(_)))),
            "强制中继时不应发出 PunchHoleRequest"
        );
    }

    /// `always_use_relay` 被控端侧：也直接走中继（旧实现忽略它，规则不对称）。
    #[tokio::test]
    async fn always_use_relay_skips_punch_on_controlled_side() {
        let hbbs = FakeHbbs::start().await;
        let transport = hbbs.make_transport_with(true).await;
        let client_udp = hbbs.client_udp().unwrap();
        let session_id = "sess-relay";

        // 并发：establish 等中继推送；从旁发一条 UDP RequestRelay 把它唤醒。
        let est = transport.establish(
            dummy_request(session_id, "", [0u8; 8], Vec::new()),
            Role::Controlled,
        );
        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let push = RendezvousMessage::with(Union::RequestRelay(RequestRelay {
            id: "111111111".into(),
            uuid: session_id.into(),
            ..Default::default()
        }))
        .encode_to_vec();
        tokio::time::sleep(Duration::from_millis(100)).await;
        sender.send_to(&push, client_udp).await.unwrap();

        let established = tokio::time::timeout(Duration::from_secs(5), est)
            .await
            .expect("应在预算内完成")
            .unwrap();
        assert_eq!(established.path, TransportPath::Relay);
    }
}
