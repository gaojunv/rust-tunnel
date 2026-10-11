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
//!   控制器 → 控制器直连该地址。跨 NAT 的 `PunchHoleResponse.socket_addr` 是**目标端信令
//!   TCP 的源地址**（临时端口，目标端并未在那里监听），因此阶段 1 不据此打洞，只回退中继。
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
use crate::transport::{BoxDuplex, EstablishRequest, Role, TransportEstablisher};
use async_trait::async_trait;
use bytes::BytesMut;
use rust_tunnel_common::protocol::RemoteParams;
use rust_tunnel_rustdesk::addr;
use rust_tunnel_rustdesk::frame::{frame, FrameCodec};
use rust_tunnel_rustdesk::proto::rendezvous_message::Union;
use rust_tunnel_rustdesk::proto::{
    FetchLocalAddr, LocalAddr, ProstMessage, PunchHoleRequest, PunchHoleResponse, RegisterPeer, RegisterPk,
    RendezvousMessage, RequestRelay,
};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
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
    /// 服务端经 UDP 推送的消息队列。
    pushes: Mutex<mpsc::UnboundedReceiver<RendezvousMessage>>,
    /// 直连接入的流队列。
    direct_rx: Mutex<mpsc::UnboundedReceiver<TcpStream>>,
    /// 关闭信号。
    cancel: CancellationToken,
    /// 后台登记任务。
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

        let udp = UdpSocket::bind("0.0.0.0:0")
            .await
            .map_err(|err| RemoteError::Signaling(format!("绑定 UDP 失败: {err}")))?;
        let listener = TcpListener::bind("0.0.0.0:0")
            .await
            .map_err(|err| RemoteError::Signaling(format!("绑定直连监听失败: {err}")))?;
        let listen_port = listener
            .local_addr()
            .map_err(|err| RemoteError::Signaling(format!("取监听地址失败: {err}")))?
            .port();
        let local_ip = probe_local_ip(hbbs).await;
        let direct_addr = SocketAddr::new(local_ip, listen_port);

        let (push_tx, push_rx) = mpsc::unbounded_channel();
        let (direct_tx, direct_rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();

        let registration = tokio::spawn(registration_task(
            udp,
            cfg.clone(),
            hbbs,
            direct_addr,
            push_tx,
            cancel.clone(),
        ));
        let acceptor = tokio::spawn(accept_task(listener, direct_tx, cancel.clone()));

        Ok(std::sync::Arc::new(Self {
            cfg,
            hbbs,
            relay_addr,
            direct_addr,
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

    /// 关闭后台任务。
    pub fn shutdown(&self) {
        self.cancel.cancel();
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

    /// 尝试同内网直连；不可行时返回 `None`。
    async fn try_direct(&self, peer_rd_id: &str) -> RemoteResult<Option<TcpStream>> {
        let mut stream = TcpStream::connect(self.hbbs).await?;
        let msg = RendezvousMessage::with(Union::PunchHoleRequest(PunchHoleRequest {
            id: peer_rd_id.to_owned(),
            licence_key: self.cfg.licence_key.clone(),
            version: CLIENT_VERSION.to_owned(),
            ..Default::default()
        }));
        stream.write_all(&encode_message(&msg)?).await?;
        stream.flush().await?;

        let response = match timeout(RELAY_WAIT, read_message(&mut stream)).await {
            Ok(Ok(msg)) => msg,
            Ok(Err(err)) => {
                debug!(error = %err, "读打洞应答失败");
                return Ok(None);
            }
            Err(_) => {
                debug!("等待打洞应答超时");
                return Ok(None);
            }
        };
        let Some(Union::PunchHoleResponse(PunchHoleResponse { socket_addr, .. })) = response.union else {
            return Ok(None);
        };
        let Some(target) = addr::decode(&socket_addr) else {
            return Ok(None);
        };
        // 跨 NAT 时该地址是目标端信令连接源地址（未监听），连接会失败 → 回退中继。
        if let Ok(Ok(stream)) = timeout(DIRECT_CONNECT_TIMEOUT, TcpStream::connect(target)).await {
            let _ = stream.set_nodelay(true);
            Ok(Some(stream))
        } else {
            debug!(%target, "直连失败，回退中继");
            Ok(None)
        }
    }
}

#[async_trait]
impl TransportEstablisher for RustDeskTransport {
    async fn establish(&self, request: EstablishRequest, role: Role) -> RemoteResult<BoxDuplex> {
        match role {
            Role::Controller => {
                if request.prefer_direct && !self.cfg.always_use_relay {
                    if let Some(mut stream) = self.try_direct(&request.peer_rd_id).await? {
                        // 直连路径没有中继帧可流水线，直接补写 Hello。
                        if !request.early.is_empty() {
                            stream.write_all(&request.early).await?;
                            stream.flush().await?;
                        }
                        return Ok(Box::new(stream));
                    }
                }
                self.notify_relay(&request.peer_rd_id, &request.session_id).await?;
                let stream = self.connect_relay(&request.session_id, &request.early).await?;
                Ok(Box::new(stream))
            }
            Role::Controlled => {
                // 竞争直连接入与中继推送：谁先到用谁。
                tokio::select! {
                    direct = self.next_direct() => {
                        let stream = direct?;
                        let _ = stream.set_nodelay(true);
                        Ok(Box::new(stream))
                    }
                    pushed = self.wait_relay_push(&request.session_id, RELAY_WAIT) => {
                        pushed?;
                        let stream = self.connect_relay(&request.session_id, &[]).await?;
                        Ok(Box::new(stream))
                    }
                }
            }
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

/// 后台登记任务：UDP 心跳 + 处理服务端推送。
async fn registration_task(
    udp: UdpSocket,
    cfg: SignalingConfig,
    hbbs: SocketAddr,
    direct_addr: SocketAddr,
    pushes: mpsc::UnboundedSender<RendezvousMessage>,
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
                let Ok((len, _)) = received else { continue };
                let Some(payload) = buf.get(..len) else { continue };
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
                    _ => {}
                }
            }
        }
    }
    debug!("会合登记任务结束");
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn encode_message_produces_framed_protobuf() {
        let msg = RendezvousMessage::with(Union::RegisterPeer(RegisterPeer {
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
}
