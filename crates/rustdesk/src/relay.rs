//! hbbr：中继服务器（relay）。
//!
//! # 协议概览
//!
//! 中继的控制面只有**一条消息**，比汇合服务器简单得多：
//!
//! 1. 客户端 TCP 连上中继端口，发一帧 [`RendezvousMessage`]，内含
//!    [`RequestRelay`]，其中 `licence_key` 用于认证、`uuid` 是配对令牌。
//! 2. 服务端按 `uuid` 在「等待表」里查：
//!    - **没有** → 把这连接放进等待表，最多等 [`RelayConfig::pair_window`]。**不回复
//!      任何消息**（线上确实没有 ack，配对成功的确认由之后的裸字节通道里跑
//!      P2P 握手来体现）。
//!    - **已有** → 取出等待者，两条连接**同时切到裸字节模式**，之后就是无脑双向
//!      转发，直到任一端关闭或空闲超时。
//!
//! 也就是说中继**不理解**它转发的内容——RustDesk 的会话语义全在两端之间，
//! 中继只按 `uuid` 把两条 TCP 管子对接起来。密钥唯一的用处是掐掉未授权连接。
//!
//! # 两个容易写错的地方
//!
//! - **握手帧之后的残留字节必须转发**。客户端完全可能把 `RequestRelay` 帧和紧随
//!   其后的 P2P 握手字节放在**同一次 write** 里发出。帧解码会留下这些残留字节，
//!   若丢掉它们，被控端会一直等一个永远不来的握手。见
//!   [`read_handshake`] 的返回值与 [`RelayServer::pair`] 的转发逻辑。
//! - **等待者的所有权**。等待者被放进 map，由 map 持有；到点后 `remove` 拿到
//!   `Some` 才丢弃。若已被认领则 `remove` 返回 `None`，天然不会与配对路径争抢。
//!
//! # 与上游的有意差异
//!
//! - **不实现控制台子协议**：上游对来自 loopback 的连接会解析一套文本管理命令
//!   （`blacklist-add` / `limit-speed` …）。那是一块独立的、面向运维的交互面，
//!   与本项目的 Web 管理面重复，因此不做。
//! - **不实现 WebSocket 端口**（主端口 + 2）：仅 Web 客户端需要，原生桌面客户端
//!   走裸 TCP 不受影响。
//! - **不限速/不降级**：上游按 IP 做限速与「降级」判定。本实现只做流量计数，
//!   限速交给上游的边缘组件或后续迭代。

use crate::frame::FrameCodec;
use crate::key::ServerKey;
use crate::proto::{rendezvous_message, RendezvousMessage};
use bytes::BytesMut;
use prost::Message as _;
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Mutex};
use tokio::task::JoinSet;
use tokio::time::{interval, sleep, timeout, Instant, MissedTickBehavior};
use tokio_util::codec::Decoder as _;
use tracing::{debug, info, warn};

/// 裸字节转发的单次读取缓冲区大小。
const RELAY_BUF_SIZE: usize = 64 * 1024;

/// 中继运行时参数。
#[derive(Debug, Clone, Copy)]
pub struct RelayConfig {
    /// 先到的一方在等待表里的最长停留时间。
    ///
    /// 上游用 30 秒。这段时间里连接是「占着但不转发」的，因此也是慢速资源占用的
    /// 主要来源。
    pub pair_window: Duration,
    /// 一条已配对的连接多久没有任何字节流动就判定为死链并关闭。
    pub idle_timeout: Duration,
    /// 空闲检查的轮询间隔；必须显著小于 `idle_timeout`。
    ///
    /// 单独暴露是为了让测试能把超时压到毫秒级，而不必真的等 30 秒。
    pub idle_check_interval: Duration,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            pair_window: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(30),
            idle_check_interval: Duration::from_secs(3),
        }
    }
}

/// 中继的累计计数。
///
/// 全部用原子量，读侧（管理面轮询）不必加锁；`bytes_from_*` 按**数据来源方**计数，
/// 因此入站/出站是对两条腿分别统计的，不是同一份流量的两次计数。
#[derive(Debug, Default)]
pub struct RelayStats {
    /// 当前已配对、正在转发的连接对数。
    active_pairs: AtomicUsize,
    /// 累计成功配对次数。
    total_pairs: AtomicU64,
    /// 从「先到者」转发到「后到者」的字节数。
    bytes_from_first: AtomicU64,
    /// 从「后到者」转发到「先到者」的字节数。
    bytes_from_second: AtomicU64,
    /// 累计被拒绝的连接数（key 不符、无 uuid、控制帧非法）。
    rejected: AtomicU64,
}

/// [`RelayStats`] 的可序列化快照。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RelayStatsSnapshot {
    /// 当前活跃配对数。
    pub active_pairs: usize,
    /// 累计配对数。
    pub total_pairs: u64,
    /// 先到者 → 后到者的字节数。
    pub bytes_from_first: u64,
    /// 后到者 → 先到者的字节数。
    pub bytes_from_second: u64,
    /// 累计拒绝数。
    pub rejected: u64,
}

impl RelayStats {
    /// 取一份一致的快照。
    #[must_use]
    pub fn snapshot(&self) -> RelayStatsSnapshot {
        RelayStatsSnapshot {
            active_pairs: self.active_pairs.load(Ordering::Relaxed),
            total_pairs: self.total_pairs.load(Ordering::Relaxed),
            bytes_from_first: self.bytes_from_first.load(Ordering::Relaxed),
            bytes_from_second: self.bytes_from_second.load(Ordering::Relaxed),
            rejected: self.rejected.load(Ordering::Relaxed),
        }
    }
}

/// 减一守卫：无论转发循环因何退出，活跃配对数都会回退。
struct ActivePairGuard<'a>(&'a AtomicUsize);

impl Drop for ActivePairGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// 等待配对的一方：连接本身 + 握手帧之后已到的残留字节。
struct WaitingPeer {
    stream: TcpStream,
    leftover: BytesMut,
    addr: SocketAddr,
}

/// 中继服务器。
pub struct RelayServer {
    key: Arc<ServerKey>,
    config: RelayConfig,
    /// 按 `uuid` 索引的等待表。
    pending: Mutex<HashMap<String, WaitingPeer>>,
    stats: Arc<RelayStats>,
}

impl RelayServer {
    /// 构造中继服务器。
    #[must_use]
    pub fn new(key: Arc<ServerKey>, config: RelayConfig) -> Self {
        Self {
            key,
            config,
            pending: Mutex::new(HashMap::new()),
            stats: Arc::new(RelayStats::default()),
        }
    }

    /// 共享计数句柄（管理面读取用）。
    #[must_use]
    pub fn stats(&self) -> Arc<RelayStats> {
        Arc::clone(&self.stats)
    }

    /// 当前等待配对的数量。
    pub async fn pending_count(&self) -> usize {
        self.pending.lock().await.len()
    }

    /// 在已绑定的 `listener` 上服务，直到 `shutdown` 收到 `true` 或发送端被丢弃。
    ///
    /// 监听地址由调用方决定（本 crate 不做 `0.0.0.0` 硬编码，也不登记端口——
    /// 端口登记属于服务端的 `ProxyPortsState` 职责）。关闭时所有在途连接（包括
    /// 尚在等待配对的）都会被 abort。
    ///
    /// # Errors
    /// accept 失败（如 fd 耗尽）时返回该错误；`shutdown` 是正常返回路径。
    pub async fn serve(
        self: Arc<Self>,
        listener: TcpListener,
        mut shutdown: watch::Receiver<bool>,
    ) -> io::Result<()> {
        info!(
            addr = %listener.local_addr()?,
            has_key = !self.key.key().is_empty(),
            "rustdesk 中继（hbbr）已启动"
        );
        let mut tasks: JoinSet<()> = JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, addr) = accepted?;
                    let me = Arc::clone(&self);
                    tasks.spawn(async move {
                        if let Err(err) = me.handle_connection(stream, addr).await {
                            debug!(%addr, error = %err, "中继连接结束");
                        }
                    });
                }
                // 回收已结束的任务，避免 JoinSet 无界增长。
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
                changed = shutdown.changed() => {
                    // 发送端被丢弃也视为关闭信号。
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
            }
        }
        // abort 掉所有在途连接，保证端口与 fd 尽快释放（配置热重载走 stop-then-start）。
        tasks.shutdown().await;
        info!("rustdesk 中继（hbbr）已停止");
        Ok(())
    }

    /// 处理一条入站连接：读控制帧 → 认证 → 配对或等待。
    async fn handle_connection(
        self: Arc<Self>,
        mut stream: TcpStream,
        addr: SocketAddr,
    ) -> io::Result<()> {
        // 中继承载的是实时桌面流，Nagle 会明显增加小包延迟。
        if let Err(err) = stream.set_nodelay(true) {
            debug!(%addr, error = %err, "设置 TCP_NODELAY 失败");
        }
        let deadline = Instant::now() + self.config.pair_window;
        let Some((msg, leftover)) = read_handshake(&mut stream, deadline).await? else {
            debug!(%addr, "未在等待窗口内收到合法控制帧，关闭");
            return Ok(());
        };
        let Some(rendezvous_message::Union::RequestRelay(req)) = msg.union else {
            self.stats.rejected.fetch_add(1, Ordering::Relaxed);
            debug!(%addr, "中继端口只接受 RequestRelay，忽略其它消息");
            return Ok(());
        };
        if !self.key.key().is_empty() && req.licence_key != self.key.key() {
            self.stats.rejected.fetch_add(1, Ordering::Relaxed);
            warn!(%addr, id = %req.id, "中继认证失败：licence_key 不匹配");
            return Ok(());
        }
        if req.uuid.is_empty() {
            self.stats.rejected.fetch_add(1, Ordering::Relaxed);
            debug!(%addr, "RequestRelay 缺少 uuid，忽略");
            return Ok(());
        }

        let waiting = self.pending.lock().await.remove(&req.uuid);
        if let Some(first) = waiting {
            // 我是后到者：由本任务接管两条连接。
            debug!(%addr, uuid = %req.uuid, "中继配对成功");
            self.pair(first, stream, leftover, addr, &req.uuid).await
        } else {
            debug!(%addr, uuid = %req.uuid, "中继等待对端配对");
            self.pending
                .lock()
                .await
                .insert(req.uuid.clone(), WaitingPeer { stream, leftover, addr });
            sleep(self.config.pair_window).await;
            // 到点仍未被认领就丢弃；若已被认领则 remove 为 None，不干扰配对路径。
            if let Some(expired) = self.pending.lock().await.remove(&req.uuid) {
                debug!(addr = %expired.addr, uuid = %req.uuid, "等待配对超时，丢弃连接");
            }
            Ok(())
        }
    }

    /// 把两条连接对接成裸字节双向通道。
    async fn pair(
        &self,
        first: WaitingPeer,
        second_stream: TcpStream,
        second_leftover: BytesMut,
        second_addr: SocketAddr,
        uuid: &str,
    ) -> io::Result<()> {
        let mut first_stream = first.stream;
        let mut second_stream = second_stream;
        let starting = self
            .stats
            .active_pairs
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        self.stats.total_pairs.fetch_add(1, Ordering::Relaxed);
        let _guard = ActivePairGuard(&self.stats.active_pairs);

        // 关键：握手帧之后的残留字节必须补给对端，否则两端会在 P2P 握手上互等。
        if !first.leftover.is_empty() {
            second_stream.write_all(&first.leftover).await?;
        }
        if !second_leftover.is_empty() {
            first_stream.write_all(&second_leftover).await?;
        }

        if let Err(err) = second_stream.set_nodelay(true) {
            debug!(error = %err, "设置 TCP_NODELAY 失败");
        }
        info!(
            uuid = %uuid,
            first = %first.addr,
            second = %second_addr,
            active = starting,
            "中继开始转发"
        );
        let result = pump(
            &mut first_stream,
            &mut second_stream,
            &self.config,
            &self.stats,
        )
        .await;
        debug!(uuid = %uuid, ?result, "中继转发结束");
        result
    }
}

/// 读第一条控制帧，并把**帧之后剩余的字节**一并返回。
///
/// 返回值第二项是残留字节：客户端可能把控制帧与紧随其后的应用数据放在同一次
/// write 里，这些字节必须原样转交给对端（见模块级说明）。
async fn read_handshake(
    stream: &mut TcpStream,
    deadline: Instant,
) -> io::Result<Option<(RendezvousMessage, BytesMut)>> {
    let mut codec = FrameCodec::new();
    let mut buf = BytesMut::with_capacity(1024);
    loop {
        if let Some(payload) = codec.decode(&mut buf)? {
            let msg = RendezvousMessage::decode(&payload[..])
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            return Ok(Some((msg, buf)));
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        // read_buf 是取消安全的：超时被丢弃时不会吞掉半帧数据（本函数此后即退出，
        // 所以即便不是也无所谓，但保持这个性质便于日后复用）。
        match timeout(deadline - now, stream.read_buf(&mut buf)).await {
            // 超时（没等到控制帧）与对端关闭一样，都当作「没有握手」。
            Err(_) | Ok(Ok(0)) => return Ok(None),
            Ok(Ok(_)) => {}
            Ok(Err(err)) => return Err(err),
        }
    }
}

/// 裸字节双向转发，按 [`RelayConfig::idle_timeout`] 关闭死链。
async fn pump(
    first: &mut TcpStream,
    second: &mut TcpStream,
    config: &RelayConfig,
    stats: &RelayStats,
) -> io::Result<()> {
    let mut first_buf = vec![0u8; RELAY_BUF_SIZE];
    let mut second_buf = vec![0u8; RELAY_BUF_SIZE];
    let mut last_activity = Instant::now();
    let mut ticker = interval(config.idle_check_interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            read = first.read(&mut first_buf) => {
                let n = read?;
                if n == 0 {
                    return Ok(()); // 先到者关闭
                }
                second.write_all(&first_buf[..n]).await?;
                stats.bytes_from_first.fetch_add(n as u64, Ordering::Relaxed);
                last_activity = Instant::now();
            }
            read = second.read(&mut second_buf) => {
                let n = read?;
                if n == 0 {
                    return Ok(()); // 后到者关闭
                }
                first.write_all(&second_buf[..n]).await?;
                stats.bytes_from_second.fetch_add(n as u64, Ordering::Relaxed);
                last_activity = Instant::now();
            }
            _ = ticker.tick() => {
                if last_activity.elapsed() >= config.idle_timeout {
                    return Ok(()); // 空闲超时
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{ProstMessage, RequestRelay};
    use tokio::net::TcpStream;
    use tokio::task::JoinHandle;

    /// 一个跑在临时端口上的中继实例。
    struct TestRelay {
        addr: SocketAddr,
        server: Arc<RelayServer>,
        shutdown: watch::Sender<bool>,
        _task: JoinHandle<io::Result<()>>,
    }

    async fn start_relay(key: &str, config: RelayConfig) -> TestRelay {
        let key = Arc::new(ServerKey::parse(key).expect("测试 key 应可解析"));
        let server = Arc::new(RelayServer::new(key, config));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("应能绑定");
        let addr = listener.local_addr().expect("应能取本地地址");
        let (shutdown, rx) = watch::channel(false);
        let task = tokio::spawn(Arc::clone(&server).serve(listener, rx));
        TestRelay {
            addr,
            server,
            shutdown,
            _task: task,
        }
    }

    impl Drop for TestRelay {
        fn drop(&mut self) {
            // 幂等：接收端已退出时忽略发送失败。
            let _ = self.shutdown.send(true);
        }
    }

    /// 构造一帧 `RequestRelay` 控制消息（含帧前缀，即真实线上字节）。
    fn request_relay_bytes(uuid: &str, key: &str) -> Vec<u8> {
        let msg = RendezvousMessage::with(rendezvous_message::Union::RequestRelay(RequestRelay {
            id: "test-peer".to_owned(),
            uuid: uuid.to_owned(),
            licence_key: key.to_owned(),
            ..Default::default()
        }));
        crate::frame::frame(&msg.encode_to_vec())
            .expect("应能编码")
            .to_vec()
    }

    /// 轮询等待「等待表里有 n 个」，避免用固定 sleep 猜时序。
    async fn wait_for_pending(server: &RelayServer, n: usize) {
        for _ in 0..200 {
            if server.pending_count().await == n {
                return;
            }
            sleep(Duration::from_millis(5)).await;
        }
        panic!("等待 pending_count == {n} 超时");
    }

    /// 轮询等待统计量满足条件，返回满足时的快照。
    ///
    /// 转发侧的 `fetch_add` 发生在 `write_all` **之后**，所以客户端读到自己要的
    /// 字节时计数未必已更新；直接断言会有偶发失败。
    async fn wait_for_stats(
        server: &RelayServer,
        what: &str,
        mut ok: impl FnMut(RelayStatsSnapshot) -> bool,
    ) -> RelayStatsSnapshot {
        for _ in 0..200 {
            let snap = server.stats().snapshot();
            if ok(snap) {
                return snap;
            }
            sleep(Duration::from_millis(5)).await;
        }
        panic!("等待统计量（{what}）超时：{:?}", server.stats().snapshot());
    }

    async fn connect(addr: SocketAddr) -> TcpStream {
        TcpStream::connect(addr).await.expect("应能连接中继")
    }

    /// 读一次并返回字节数；0 表示对端已关闭。
    async fn read_once(stream: &mut TcpStream) -> usize {
        let mut buf = [0u8; 64];
        timeout(Duration::from_secs(2), stream.read(&mut buf))
            .await
            .expect("读超时")
            .expect("读失败")
    }

    #[tokio::test]
    async fn pairs_two_clients_and_relays_bytes_both_ways() {
        let relay = start_relay("s3cret", RelayConfig::default()).await;

        let mut first = connect(relay.addr).await;
        first
            .write_all(&request_relay_bytes("pair-1", "s3cret"))
            .await
            .unwrap();
        wait_for_pending(&relay.server, 1).await;

        let mut second = connect(relay.addr).await;
        second
            .write_all(&request_relay_bytes("pair-1", "s3cret"))
            .await
            .unwrap();

        // 配对后两端都是裸字节通道：双向都要通。
        first.write_all(b"from-first").await.unwrap();
        let mut buf = [0u8; 10];
        second.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"from-first");

        second.write_all(b"from-second").await.unwrap();
        let mut buf = [0u8; 11];
        first.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"from-second");

        // 计数应反映两侧各自的流入量。
        let snap = wait_for_stats(&relay.server, "双向字节计数", |s| {
            s.bytes_from_first == 10 && s.bytes_from_second == 11
        })
        .await;
        assert_eq!(snap.total_pairs, 1);
        assert_eq!(snap.active_pairs, 1);
    }

    #[tokio::test]
    async fn rejects_wrong_licence_key() {
        let relay = start_relay("s3cret", RelayConfig::default()).await;
        let mut bad = connect(relay.addr).await;
        bad.write_all(&request_relay_bytes("pair-2", "WRONG"))
            .await
            .unwrap();

        // 认证失败应立即关闭连接，且不进入等待表。
        assert_eq!(read_once(&mut bad).await, 0, "错误 key 的连接应被关闭");
        assert_eq!(relay.server.pending_count().await, 0);
        assert_eq!(relay.server.stats().snapshot().rejected, 1);
    }

    #[tokio::test]
    async fn empty_key_disables_authentication() {
        // key 为空时不比对 licence_key，任意 key 都应能配对。
        let relay = start_relay("", RelayConfig::default()).await;
        let mut first = connect(relay.addr).await;
        first
            .write_all(&request_relay_bytes("pair-3", "whatever"))
            .await
            .unwrap();
        wait_for_pending(&relay.server, 1).await;

        let mut second = connect(relay.addr).await;
        second
            .write_all(&request_relay_bytes("pair-3", "another"))
            .await
            .unwrap();

        first.write_all(b"ok").await.unwrap();
        let mut buf = [0u8; 2];
        second.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ok");
    }

    #[tokio::test]
    async fn forwards_bytes_sent_in_the_same_write_as_the_handshake() {
        // 客户端把控制帧与首批应用数据放同一次 write。这些「帧后残留字节」
        // 若不转发，两端会在 P2P 握手上互等——这是最容易写错的点。
        let relay = start_relay("k", RelayConfig::default()).await;

        let mut first = connect(relay.addr).await;
        let mut payload = request_relay_bytes("pair-4", "k");
        payload.extend_from_slice(b"EARLY");
        first.write_all(&payload).await.unwrap();
        wait_for_pending(&relay.server, 1).await;

        let mut second = connect(relay.addr).await;
        let mut payload = request_relay_bytes("pair-4", "k");
        payload.extend_from_slice(b"LATE");
        second.write_all(&payload).await.unwrap();

        // 先到者的残留字节到了后到者手里，反之亦然。
        let mut buf = [0u8; 5];
        second.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"EARLY");
        let mut buf = [0u8; 4];
        first.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"LATE");
    }

    #[tokio::test]
    async fn distinguishes_different_uuids() {
        // 不同 uuid 不应被配到一起：先到者仍在等待。
        let relay = start_relay("k", RelayConfig::default()).await;
        let mut a = connect(relay.addr).await;
        a.write_all(&request_relay_bytes("uuid-a", "k")).await.unwrap();
        wait_for_pending(&relay.server, 1).await;

        let mut b = connect(relay.addr).await;
        b.write_all(&request_relay_bytes("uuid-b", "k")).await.unwrap();
        wait_for_pending(&relay.server, 2).await;

        // 两个都在等，谁都没配上。
        assert_eq!(relay.server.stats().snapshot().total_pairs, 0);
    }

    #[tokio::test]
    async fn idle_timeout_closes_established_pair() {
        let relay = start_relay(
            "k",
            RelayConfig {
                pair_window: Duration::from_secs(5),
                idle_timeout: Duration::from_millis(150),
                idle_check_interval: Duration::from_millis(30),
            },
        )
        .await;

        let mut first = connect(relay.addr).await;
        first
            .write_all(&request_relay_bytes("idle-1", "k"))
            .await
            .unwrap();
        wait_for_pending(&relay.server, 1).await;
        let mut second = connect(relay.addr).await;
        second
            .write_all(&request_relay_bytes("idle-1", "k"))
            .await
            .unwrap();

        // 先确认通道是通的，再保持静默。
        first.write_all(b"x").await.unwrap();
        let mut buf = [0u8; 1];
        second.read_exact(&mut buf).await.unwrap();

        // 静默超过 idle_timeout 后两端都应被关闭（读到 EOF）。
        assert_eq!(read_once(&mut first).await, 0, "空闲超时后应先到者关闭");
    }

    #[tokio::test]
    async fn waiting_peer_is_dropped_after_pair_window() {
        let relay = start_relay(
            "k",
            RelayConfig {
                pair_window: Duration::from_millis(120),
                ..RelayConfig::default()
            },
        )
        .await;

        let mut lonely = connect(relay.addr).await;
        lonely
            .write_all(&request_relay_bytes("alone", "k"))
            .await
            .unwrap();
        wait_for_pending(&relay.server, 1).await;

        // 窗口过后等待者被丢弃：连接关闭、等待表清空。
        assert_eq!(read_once(&mut lonely).await, 0, "等待窗口过后应关闭连接");
        wait_for_pending(&relay.server, 0).await;
    }

    #[tokio::test]
    async fn non_relay_message_is_rejected() {
        // 中继端口只认 RequestRelay；别的消息应被拒绝而不是被当成等待者。
        let relay = start_relay("k", RelayConfig::default()).await;
        let msg = RendezvousMessage::with(rendezvous_message::Union::RegisterPeer(
            crate::proto::RegisterPeer {
                id: "x".to_owned(),
                serial: 0,
            },
        ));
        let mut stream = connect(relay.addr).await;
        stream
            .write_all(&crate::frame::frame(&msg.encode_to_vec()).unwrap())
            .await
            .unwrap();

        assert_eq!(read_once(&mut stream).await, 0);
        assert_eq!(relay.server.pending_count().await, 0);
        assert_eq!(relay.server.stats().snapshot().rejected, 1);
    }

    #[tokio::test]
    async fn garbage_handshake_is_dropped_without_hanging() {
        // 非 protobuf 的垃圾字节：read_buf 先拿到的是不完整帧，最终应超时关闭，
        // 而不是让连接永远挂着。
        let relay = start_relay(
            "k",
            RelayConfig {
                pair_window: Duration::from_millis(100),
                ..RelayConfig::default()
            },
        )
        .await;
        let mut stream = connect(relay.addr).await;
        stream.write_all(b"\xff\xff\xff\xff not a frame").await.unwrap();
        assert_eq!(read_once(&mut stream).await, 0);
        assert_eq!(relay.server.pending_count().await, 0);
    }
}

