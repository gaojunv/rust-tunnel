//! 最小 TCP 式 ARQ over datagrams：把无序/丢包/重复的 UDP 数据报变成一条可靠字节流。
//!
//! 跨 NAT 直连走 UDP 打洞，而 UDP 无序、可能丢包、可能重复。本模块把它收敛成一条
//! `AsyncRead + AsyncWrite` 的字节流，直接塞进 [`crate::transport::BoxDuplex`]。
//!
//! # 为什么可靠层**不需要**保留 write 边界
//!
//! 会话帧自带 `u32` 长度前缀，且 AEAD 是**逐帧**的（不是流式）——帧边界由长度前缀恢复
//! （见 [`crate::session`]）。因此可靠层只要提供「**纯字节流**」，写边界如何切分对上层完全
//! 透明。这是整个方案能收敛的前提，也意味着本模块**不做任何消息定界**。
//!
//! # 背压是自动的
//!
//! 本模块返回 [`tokio::io::DuplexStream`]：协议任务在内部 spawn，应用侧拿到另一半。应用不读
//! → 协议任务写不动 → 停止确认 → 对端接收窗口填满 → 对端停发。**内存上界因此与对端行为
//! 无关**（乱序缓冲硬上界 [`MAX_RECV_WINDOW`] 段、发送缓冲硬上界 [`SEND_BUFFER_LIMIT`] 字节）。
//!
//! # 不与 `qos.rs` 抢方向盘（**重要**）
//!
//! 应用层码率闭环（[`crate::qos`]）已经是这条数据面的**主拥塞控制**。本模块的 `cwnd` / RTO
//! 只做**保守地板**：默认编码宽度 ≤ 1920、CBR 码率由 QoS 调，传输层只需保证「丢包时退让、
//! 不把链路打死」。因此这里的 `cwnd` 初值小、RTO 下限低，绝不试图去「探测带宽」——那是 QoS
//! 的活。传输层只负责**可靠性**（不丢、不乱、不重）。
//!
//! # 段序号
//!
//! `SYN` 占 seq 0，**数据从 seq 1 开始**（两个方向各自独立）。ISN 恒为 0——`conn_id`
//! （[`crate::punch::conn_id_from_token`]，由一次性 token 派生）已经保证旧报文不会串进新会话，
//! 不需要随机 ISN。

use crate::error::{RemoteError, RemoteResult};
use async_trait::async_trait;
use bytes::{Buf, Bytes, BytesMut};
use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::mpsc;
use tokio::time::Instant;

/// 段头 magic（`b"RTP1"`）。
pub(crate) const MAGIC: [u8; 4] = *b"RTP1";

/// 段头长度（**字节布局冻结**，见 [`SegmentHeader`]）。
pub(crate) const SEGMENT_HEADER_LEN: usize = 27;

/// 载荷上限。
///
/// `27 + 1200 = 1227 ≤ 1232`（IPv6 最小 MTU 1500 − 40 IPv6 头 − 8 UDP 头 − 余量），
/// 即**任何情况下都不触发 IP 分片**。这个数是反推出来的，不是随手取的。
pub(crate) const MAX_PAYLOAD: usize = 1200;

/// SYN 重发间隔。
pub(crate) const PROBE_INTERVAL: Duration = Duration::from_millis(250);

/// 接收窗口上限（段）：乱序缓冲 + 待交付缓冲的硬上界。
const MAX_RECV_WINDOW: usize = 64;

/// 拥塞窗口初值（段）。
const INITIAL_CWND_SEGMENTS: usize = 10;

/// 拥塞窗口下限（字节）——RTO 时退回到此值。
const MIN_CWND_BYTES: usize = 2 * MAX_PAYLOAD;

/// RTO 下限。
const RTO_MIN: Duration = Duration::from_millis(200);

/// RTO 上限。
const RTO_MAX: Duration = Duration::from_secs(2);

/// 延迟确认时长。
const DELAYED_ACK: Duration = Duration::from_millis(50);

/// 触发快速重传的重复 ACK 数。
const DUP_ACK_THRESHOLD: u32 = 3;

/// 连续 RTO 超时上限：超过即判链路死亡、有界退出（不无限重传）。
const MAX_RETRANSMITS: u32 = 8;

/// 应用侧待分段缓冲上限（字节）：超过就停止从应用读，把背压传回去。
const SEND_BUFFER_LIMIT: usize = MAX_PAYLOAD * 16;

/// 返回给应用的 `DuplexStream` 缓冲区大小。
const DUPLEX_CAPACITY: usize = 64 * 1024;

/// 零窗口 persist 探测间隔。
const PERSIST_INTERVAL: Duration = RTO_MIN;

/// 序列号比较：`a` 是否在 `b` 之后（回绕安全，与 TCP 一致）。
///
/// 用 `diff != 0 && diff < 2^31` 而非 `as i32` 强转，避免 `cast_possible_wrap`。
fn seq_after(a: u32, b: u32) -> bool {
    let diff = a.wrapping_sub(b);
    diff != 0 && diff < (1 << 31)
}

/// 接收窗口上限的 `u16` 表示（构造段头时用）。
fn recv_window_max() -> u16 {
    u16::try_from(MAX_RECV_WINDOW).unwrap_or(u16::MAX)
}

/// 段头。
///
/// # 字节布局（**冻结，见 `segment_header_layout_is_frozen`**）
///
/// ```text
/// 偏移  宽度  字段
/// 0..4   4    magic = b"RTP1"
/// 4..12  8    conn_id（u64 大端）
/// 12..16 4    seq（u32 大端，段序号，不是字节序号）
/// 16..20 4    ack（u32 大端，累计确认 = 下一个期望的 seq）
/// 20..22 2    ack_bits（u16 大端，bit i 置位 = ack + i + 1 已收到）
/// 22     1    flags（SYN/ACK/FIN/RST）
/// 23..25 2    window（u16 大端，接收方空闲窗口，单位 = 段）
/// 25..27 2    len（u16 大端，本段载荷字节数）
/// ```
///
/// **为什么 `conn_id` 固定在偏移 4、总长 27**：分发任务（demux）要能只读前 12 个字节就把
/// 数据报路由到会话，不做完整解析、不分配（见 [`crate::punch::peek_conn_id`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SegmentHeader {
    /// 会话标识。
    pub conn_id: u64,
    /// 段序号。
    pub seq: u32,
    /// 累计确认（下一个期望的 seq）。
    pub ack: u32,
    /// 确认位图。
    pub ack_bits: u16,
    /// 标志位。
    pub flags: u8,
    /// 接收方空闲窗口（段）。
    pub window: u16,
    /// 载荷字节数。
    pub len: u16,
}

impl SegmentHeader {
    /// SYN：发起/确认握手。
    pub(crate) const SYN: u8 = 0x01;
    /// ACK：携带确认。
    pub(crate) const ACK: u8 = 0x02;
    /// FIN：本方向结束。
    pub(crate) const FIN: u8 = 0x04;
    /// RST：异常终止。
    pub(crate) const RST: u8 = 0x08;

    /// 编码成 27 字节。
    #[must_use]
    pub(crate) fn encode(&self) -> [u8; SEGMENT_HEADER_LEN] {
        let mut buf = [0u8; SEGMENT_HEADER_LEN];
        buf[0..4].copy_from_slice(&MAGIC);
        buf[4..12].copy_from_slice(&self.conn_id.to_be_bytes());
        buf[12..16].copy_from_slice(&self.seq.to_be_bytes());
        buf[16..20].copy_from_slice(&self.ack.to_be_bytes());
        buf[20..22].copy_from_slice(&self.ack_bits.to_be_bytes());
        buf[22] = self.flags;
        buf[23..25].copy_from_slice(&self.window.to_be_bytes());
        buf[25..27].copy_from_slice(&self.len.to_be_bytes());
        buf
    }

    /// 解析；长度不足 27、magic 不符、或 `len > MAX_PAYLOAD` 时返回 `None`。
    #[must_use]
    pub(crate) fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < SEGMENT_HEADER_LEN {
            return None;
        }
        if buf[..4] != MAGIC {
            return None;
        }
        let conn_id = u64::from_be_bytes(buf[4..12].try_into().ok()?);
        let seq = u32::from_be_bytes(buf[12..16].try_into().ok()?);
        let ack = u32::from_be_bytes(buf[16..20].try_into().ok()?);
        let ack_bits = u16::from_be_bytes(buf[20..22].try_into().ok()?);
        let flags = buf[22];
        let window = u16::from_be_bytes(buf[23..25].try_into().ok()?);
        let len = u16::from_be_bytes(buf[25..27].try_into().ok()?);
        if usize::from(len) > MAX_PAYLOAD {
            return None;
        }
        Some(Self {
            conn_id,
            seq,
            ack,
            ack_bits,
            flags,
            window,
            len,
        })
    }

    /// 是否置位某个标志。
    #[must_use]
    pub(crate) fn has(self, flag: u8) -> bool {
        self.flags & flag != 0
    }
}

/// 拼一个完整段（27 字节头 + 载荷）。
fn encode_segment(
    conn_id: u64,
    seq: u32,
    ack: u32,
    ack_bits: u16,
    flags: u8,
    window: u16,
    payload: &[u8],
) -> Vec<u8> {
    let hdr = SegmentHeader {
        conn_id,
        seq,
        ack,
        ack_bits,
        flags,
        window,
        len: u16::try_from(payload.len()).unwrap_or(0),
    };
    let mut out = Vec::with_capacity(SEGMENT_HEADER_LEN + payload.len());
    out.extend_from_slice(&hdr.encode());
    out.extend_from_slice(payload);
    out
}

/// 数据报发送端。生产实现是共享的 `UdpSocket`；测试注入内存链路。
#[async_trait]
pub(crate) trait DatagramSender: Send + Sync {
    /// 发送一个数据报到 `target`。
    ///
    /// # Errors
    /// 底层套接字错误时返回 `io::Error`。
    async fn send_datagram(&self, buf: &[u8], target: SocketAddr) -> std::io::Result<usize>;
}

#[async_trait]
impl DatagramSender for tokio::net::UdpSocket {
    async fn send_datagram(&self, buf: &[u8], target: SocketAddr) -> std::io::Result<usize> {
        self.send_to(buf, target).await
    }
}

/// 由 demux 任务按 `conn_id` 路由过来的本会话数据报。
pub(crate) type DatagramRx = mpsc::Receiver<(SocketAddr, Bytes)>;

/// 未确认的已发段（含 FIN 这类零载荷段）。
struct Unacked {
    /// 段序号。
    seq: u32,
    /// 载荷（FIN 为空）。
    data: Bytes,
    /// 最近一次发送时刻（RTO 基准）。
    last_sent: Instant,
    /// 是否被重传过（Karn：重传过的段不取 RTT 样本）。
    retransmitted: bool,
    /// 本段标志位（重传时原样重发，FIN 段才能带 FIN）。
    flags: u8,
}

/// 连接统计（单测断言用）。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ConnStats {
    /// 已发出的数据段数。
    sent_segments: u64,
    /// 已确认的数据段数。
    acked_segments: u64,
    /// RTO 超时次数。
    rto_timeouts: u64,
    /// 快速重传次数。
    fast_retransmits: u64,
    /// 收到的乱序段数。
    out_of_order_segments: u64,
    /// 收到的重复段数。
    duplicate_segments: u64,
    /// 因超出接收窗口而丢弃的段数。
    out_of_window_drops: u64,
    /// 收到的按序段数。
    in_order_segments: u64,
}

/// 一条可靠连接的协议状态机（同步，便于单测用虚拟时钟直接驱动）。
#[expect(
    clippy::struct_excessive_bools,
    reason = "连接状态天然由若干独立布尔（fin_sent/peer_fin/app_eof/fatal…）表达，合并成枚举反而更难读"
)]
#[expect(
    clippy::struct_field_names,
    reason = "`conn_id` 是跨模块冻结的字段语义（段头/分发都叫它 conn_id），改名反而与协议脱节"
)]
struct Conn {
    /// 数据报发送端。
    link: Arc<dyn DatagramSender>,
    /// 会话标识。
    conn_id: u64,
    /// 已锁存的对端源地址。
    peer: SocketAddr,
    // ── 发送侧 ──
    /// 下一个要分配的段序号。
    send_next_seq: u32,
    /// 最老的未确认段序号。
    send_base: u32,
    /// 未确认段队列（按 seq 升序）。
    unacked: VecDeque<Unacked>,
    /// 应用侧待分段字节。
    send_buf: BytesMut,
    /// 拥塞窗口（字节）。
    cwnd: usize,
    /// 慢启动阈值（字节）。
    ssthresh: usize,
    /// 连续重复 ACK 计数。
    dup_acks: u32,
    // ── RTT/RTO ──
    /// 平滑 RTT（无样本时为 `None`）。
    srtt: Option<Duration>,
    /// RTT 偏差。
    rttvar: Duration,
    /// 当前 RTO。
    rto: Duration,
    // ── 接收侧 ──
    /// 下一个期望的段序号。
    recv_next: u32,
    /// 乱序缓冲。
    recv_ooo: BTreeMap<u32, Bytes>,
    /// 按序待交付给应用的段。
    recv_deliver: VecDeque<Bytes>,
    /// 距上次立即确认以来的按序段数（延迟确认用）。
    in_order_since_ack: u32,
    /// 是否有待发送的确认。
    ack_pending: bool,
    // ── 对端 ──
    /// 对端最近通告的空闲窗口（段）。
    peer_window: u16,
    // ── 收尾 ──
    /// 本端已发 FIN。
    fin_sent: bool,
    /// 本端 FIN 的段序号。
    fin_seq: Option<u32>,
    /// 对端已发 FIN。
    peer_fin: bool,
    /// 对端 FIN 的段序号。
    peer_fin_seq: Option<u32>,
    /// 应用侧已 EOF（不再有新数据）。
    app_eof: bool,
    // ── 计时 ──
    /// RTO 到期时刻。
    rto_deadline: Option<Instant>,
    /// 延迟确认到期时刻。
    ack_deadline: Option<Instant>,
    /// 零窗口 persist 探测到期时刻。
    persist_deadline: Option<Instant>,
    /// 连续 RTO 超时计数。
    consecutive_timeouts: u32,
    // ── 出站 & 状态 ──
    /// 待发出的原始数据报。
    outgoing: Vec<Vec<u8>>,
    /// 统计。
    stats: ConnStats,
    /// 致命错误（RST 或重传耗尽）：置位后任务退出、应用侧流关闭。
    fatal: bool,
}

impl Conn {
    /// 建一条新连接（握手的 SYN 已由 [`connect`]/[`accept`] 交换完毕）。
    fn new(link: Arc<dyn DatagramSender>, conn_id: u64, peer: SocketAddr) -> Self {
        Self {
            link,
            conn_id,
            peer,
            send_next_seq: 1,
            send_base: 1,
            unacked: VecDeque::new(),
            send_buf: BytesMut::new(),
            cwnd: INITIAL_CWND_SEGMENTS * MAX_PAYLOAD,
            ssthresh: usize::MAX / 2,
            dup_acks: 0,
            srtt: None,
            rttvar: RTO_MIN / 2,
            rto: RTO_MIN,
            recv_next: 1,
            recv_ooo: BTreeMap::new(),
            recv_deliver: VecDeque::new(),
            in_order_since_ack: 0,
            ack_pending: false,
            peer_window: recv_window_max(),
            fin_sent: false,
            fin_seq: None,
            peer_fin: false,
            peer_fin_seq: None,
            app_eof: false,
            rto_deadline: None,
            ack_deadline: None,
            persist_deadline: None,
            consecutive_timeouts: 0,
            outgoing: Vec::new(),
            stats: ConnStats::default(),
            fatal: false,
        }
    }

    /// 当前接收窗口（段）。
    fn window(&self) -> u16 {
        let buffered = self.recv_deliver.len() + self.recv_ooo.len();
        u16::try_from(MAX_RECV_WINDOW.saturating_sub(buffered)).unwrap_or(u16::MAX)
    }

    /// 当前确认位图：bit i 置位 = `recv_next + i + 1` 已在乱序缓冲里。
    fn ack_bits(&self) -> u16 {
        let mut bits = 0u16;
        for i in 0..16u32 {
            let seq = self.recv_next.wrapping_add(i + 1);
            if self.recv_ooo.contains_key(&seq) {
                bits |= 1 << i;
            }
        }
        bits
    }

    /// 在途未确认字节数。
    fn inflight_bytes(&self) -> usize {
        self.unacked.iter().map(|u| u.data.len()).sum()
    }

    /// 对端窗口折算成字节。
    fn peer_window_bytes(&self) -> usize {
        usize::from(self.peer_window) * MAX_PAYLOAD
    }

    /// 是否还能从应用读入更多字节（背压闸门）。
    ///
    /// **必须含 `!app_eof`**：应用半关闭后 `read` 会**立刻**返回 `Ok(0)`，若继续把它挂在
    /// `select!` 里，那个分支每轮都就绪 → 忙循环 → 虚拟时钟永不推进（连定时器都饿死）。
    fn wants_app_read(&self) -> bool {
        !self.app_eof && !self.fin_sent && self.send_buf.len() < SEND_BUFFER_LIMIT
    }

    /// 收下应用写来的字节。
    fn push_app_data(&mut self, data: &[u8]) {
        self.send_buf.extend_from_slice(data);
    }

    /// 应用侧 EOF：记下来，待所有数据发完且被确认后发 FIN。
    fn on_app_eof(&mut self) {
        self.app_eof = true;
    }

    /// 待交付给应用的下一段（无则空切片）。
    fn deliver_slice(&self) -> &[u8] {
        match self.recv_deliver.front() {
            Some(b) => b,
            None => &[],
        }
    }

    /// 应用已消费 `n` 字节：窗口若由 0 变正，立即通告（零窗口恢复）。
    fn consume_delivered(&mut self, n: usize) {
        let was_zero = self.window() == 0;
        if let Some(front) = self.recv_deliver.front_mut() {
            if n >= front.len() {
                self.recv_deliver.pop_front();
            } else {
                front.advance(n);
            }
        }
        if was_zero && self.window() > 0 {
            self.ack_pending = true;
            self.ack_deadline = None;
        }
    }

    /// 最近的一个计时器到期时刻。
    fn next_deadline(&self) -> Option<Instant> {
        [self.rto_deadline, self.ack_deadline, self.persist_deadline]
            .into_iter()
            .flatten()
            .min()
    }

    /// 收到一个数据报（已由 demux 按 `conn_id` 路由过来）。
    fn on_datagram(&mut self, from: SocketAddr, data: &[u8]) {
        // 只接受已锁存的对端源地址（`conn_id` 才是安全边界，这里是廉价加固）。
        if from != self.peer {
            return;
        }
        let Some(hdr) = SegmentHeader::parse(data) else {
            return;
        };
        let payload_end = SEGMENT_HEADER_LEN + usize::from(hdr.len);
        if data.len() < payload_end {
            return;
        }
        let payload = data[SEGMENT_HEADER_LEN..payload_end].to_vec();
        self.on_segment(hdr, &payload, Instant::now());
    }

    /// 处理一个段。
    fn on_segment(&mut self, hdr: SegmentHeader, payload: &[u8], now: Instant) {
        if hdr.has(SegmentHeader::RST) {
            self.fatal = true;
            return;
        }
        // 裸 `SYN`（无 `ACK`）= 对端在重发它的握手请求，说明它**没收到我们的 `SYN|ACK`**。
        //
        // 为什么必须补发：被动方那条 `SYN|ACK` 占 seq 0 但**不在 `unacked` 里**，`arm_rto`
        // 对空队列返回 `None`，所以它**没有任何重传路径**——丢一次就再没有第二次机会，对端会
        // 一直重发 `SYN` 直到预算耗尽，打洞直接失败（而这正是可靠层存在的理由）。
        //
        // 为什么条件是「裸 `SYN`」而不是「`SYN|ACK`」：ISN 恒为 0 ⇒ 两端的 `SYN|ACK` 形状
        // **完全对称**、无法区分谁是谁的。若两者都触发补发，两端会互相触发，形成每 RTT 一包的
        // **无限 `SYN|ACK` 回声**。主动方重发的是裸 `SYN`（见 `connect`），所以这个条件足够。
        if hdr.has(SegmentHeader::SYN) && !hdr.has(SegmentHeader::ACK) {
            let conn_id = self.conn_id;
            let window = self.window();
            self.outgoing.push(encode_segment(
                conn_id,
                0,
                1,
                0,
                SegmentHeader::SYN | SegmentHeader::ACK,
                window,
                &[],
            ));
            return;
        }
        if hdr.has(SegmentHeader::ACK) {
            self.process_ack(hdr.ack, hdr.window, now);
        } else {
            self.peer_window = hdr.window;
        }
        if !payload.is_empty() {
            self.process_data(hdr.seq, payload, now);
        } else if hdr.flags == 0 {
            // 裸 persist 探测段：立即回报当前窗口（对端可能正卡在零窗口上）。
            self.ack_pending = true;
            self.ack_deadline = None;
        }
        if hdr.has(SegmentHeader::FIN) {
            self.peer_fin_seq = Some(hdr.seq);
            self.try_consume_fin();
        }
    }

    /// 若对端 FIN 的段序号已被覆盖（数据已按序收齐），则消费它。
    fn try_consume_fin(&mut self) {
        let Some(fs) = self.peer_fin_seq else {
            return;
        };
        if seq_after(fs, self.recv_next) {
            // 还差数据：等补齐。
            return;
        }
        if self.recv_next == fs {
            self.recv_next = self.recv_next.wrapping_add(1);
        }
        self.peer_fin = true;
        self.ack_pending = true;
        self.ack_deadline = None;
    }

    /// 处理对端确认。
    fn process_ack(&mut self, ack: u32, window: u16, now: Instant) {
        self.peer_window = window;
        if seq_after(ack, self.send_base) {
            // 新确认：弹出所有 seq < ack 的未确认段。
            let mut sample: Option<Duration> = None;
            while let Some(front) = self.unacked.front() {
                if !seq_after(ack, front.seq) {
                    break;
                }
                let Some(seg) = self.unacked.pop_front() else {
                    break;
                };
                self.stats.acked_segments += 1;
                if sample.is_none() && !seg.retransmitted {
                    sample = Some(now.saturating_duration_since(seg.last_sent));
                }
                self.grow_cwnd(seg.data.len());
            }
            self.send_base = ack;
            self.dup_acks = 0;
            self.consecutive_timeouts = 0;
            if let Some(s) = sample {
                self.update_rtt(s);
            }
            self.arm_rto();
        } else if ack == self.send_base && !self.unacked.is_empty() {
            // 重复 ACK：对端收到了 send_base 之后的段（空洞探测）。
            self.dup_acks += 1;
            if self.dup_acks == DUP_ACK_THRESHOLD {
                self.fast_retransmit(now);
            }
        }
    }

    /// 处理对端数据段。
    fn process_data(&mut self, seq: u32, payload: &[u8], now: Instant) {
        // 旧段（已交付过）：立即重发 ACK。
        if seq_after(self.recv_next, seq) {
            self.stats.duplicate_segments += 1;
            self.ack_pending = true;
            self.ack_deadline = None;
            return;
        }
        if seq == self.recv_next {
            // 缓冲上界：超出窗口就丢弃（接收方不读时窗口收缩到 0，发送方据实停发）。
            if self.recv_deliver.len() + self.recv_ooo.len() >= MAX_RECV_WINDOW {
                self.stats.out_of_window_drops += 1;
                return;
            }
            self.recv_deliver.push_back(Bytes::copy_from_slice(payload));
            self.recv_next = self.recv_next.wrapping_add(1);
            self.stats.in_order_segments += 1;
            // 从乱序缓冲里连续取出。
            let mut drained_ooo = false;
            while let Some(data) = self.recv_ooo.remove(&self.recv_next) {
                self.recv_deliver.push_back(data);
                self.recv_next = self.recv_next.wrapping_add(1);
                drained_ooo = true;
            }
            if drained_ooo {
                self.try_consume_fin();
            }
            self.in_order_since_ack += 1;
            // 立即确认：乱序刚被补齐、或每第 2 段；否则延迟确认。
            if drained_ooo || self.in_order_since_ack >= 2 {
                self.in_order_since_ack = 0;
                self.ack_pending = true;
                self.ack_deadline = None;
            } else {
                self.ack_pending = true;
                if self.ack_deadline.is_none() {
                    self.ack_deadline = Some(now + DELAYED_ACK);
                }
            }
            return;
        }
        // 乱序：seq > recv_next。
        let window_end = self
            .recv_next
            .wrapping_add(u32::try_from(MAX_RECV_WINDOW).unwrap_or(u32::MAX));
        if !seq_after(window_end, seq) {
            // seq >= window_end：超出接收窗口 → 丢弃并计数。
            self.stats.out_of_window_drops += 1;
            return;
        }
        if self.recv_deliver.len() + self.recv_ooo.len() >= MAX_RECV_WINDOW {
            self.stats.out_of_window_drops += 1;
            return;
        }
        if self.recv_ooo.contains_key(&seq) {
            self.stats.duplicate_segments += 1;
            self.ack_pending = true;
            self.ack_deadline = None;
            return;
        }
        self.recv_ooo.insert(seq, Bytes::copy_from_slice(payload));
        self.stats.out_of_order_segments += 1;
        // 乱序段立即确认（让对端尽早看到空洞、触发快速重传）。
        self.ack_pending = true;
        self.ack_deadline = None;
    }

    /// 拥塞窗口增长（慢启动翻倍 / 拥塞避免每 RTT +1 段）。
    fn grow_cwnd(&mut self, acked_bytes: usize) {
        if self.cwnd < self.ssthresh {
            self.cwnd += acked_bytes.max(1);
        } else {
            self.cwnd += (MAX_PAYLOAD * MAX_PAYLOAD) / self.cwnd.max(1);
        }
        // 上限：略高于对端窗口即可，避免无谓膨胀。
        let cap = self.peer_window_bytes().max(MIN_CWND_BYTES) + MIN_CWND_BYTES;
        if self.cwnd > cap {
            self.cwnd = cap;
        }
    }

    /// 用一次 RTT 样本更新 SRTT/RTTVAR/RTO（Jacobson/Karels）。
    fn update_rtt(&mut self, sample: Duration) {
        match self.srtt {
            None => {
                self.srtt = Some(sample);
                self.rttvar = sample / 2;
            }
            Some(srtt) => {
                let diff = sample.abs_diff(srtt);
                self.rttvar = self.rttvar.mul_f32(0.75) + diff.mul_f32(0.25);
                self.srtt = Some(srtt.mul_f32(0.875) + sample.mul_f32(0.125));
            }
        }
        let srtt = self.srtt.unwrap_or(RTO_MIN);
        self.rto = (srtt + self.rttvar.mul_f32(4.0)).clamp(RTO_MIN, RTO_MAX);
    }

    /// 按最老的未确认段重设 RTO 定时器。
    fn arm_rto(&mut self) {
        let deadline = self.unacked.front().map(|u| u.last_sent + self.rto);
        self.rto_deadline = deadline;
    }

    /// 3 个重复 ACK 触发快速重传（必须早于 RTO）。
    fn fast_retransmit(&mut self, now: Instant) {
        self.ssthresh = (self.cwnd / 2).max(MIN_CWND_BYTES);
        self.cwnd = self.ssthresh + usize::try_from(DUP_ACK_THRESHOLD).unwrap_or(3) * MAX_PAYLOAD;
        let ack_bits = self.ack_bits();
        let window = self.window();
        let send_base = self.send_base;
        let conn_id = self.conn_id;
        if let Some(front) = self.unacked.front_mut() {
            front.retransmitted = true;
            front.last_sent = now;
            let seq = front.seq;
            let data = front.data.clone();
            let seg = encode_segment(conn_id, seq, send_base, ack_bits, SegmentHeader::ACK, window, &data);
            self.outgoing.push(seg);
            self.stats.fast_retransmits += 1;
        }
        self.arm_rto();
    }

    /// RTO 超时：重传最老未确认段、退避 RTO、退回拥塞窗口。
    fn retransmit_oldest(&mut self, now: Instant) {
        let ack_bits = self.ack_bits();
        let window = self.window();
        let send_base = self.send_base;
        let conn_id = self.conn_id;
        if let Some(front) = self.unacked.front_mut() {
            front.retransmitted = true;
            front.last_sent = now;
            let seq = front.seq;
            let data = front.data.clone();
            let flags = front.flags;
            let seg = encode_segment(conn_id, seq, send_base, ack_bits, flags, window, &data);
            self.outgoing.push(seg);
            self.stats.rto_timeouts += 1;
        }
        // 拥塞：RTO 把窗口退回 2 段（保守地板，不与应用层 QoS 抢方向盘）。
        self.ssthresh = (self.cwnd / 2).max(MIN_CWND_BYTES);
        self.cwnd = MIN_CWND_BYTES;
        // 指数退避。
        self.rto = (self.rto * 2).min(RTO_MAX);
        self.consecutive_timeouts += 1;
        if self.consecutive_timeouts > MAX_RETRANSMITS {
            self.fatal = true;
        }
        self.arm_rto();
    }

    /// 按拥塞窗口/对端窗口把待发字节切成段发出去。
    fn pump(&mut self, now: Instant) {
        if self.fatal || self.fin_sent {
            return;
        }
        let ack_bits = self.ack_bits();
        let window = self.window();
        let conn_id = self.conn_id;
        let mut sent_any = false;
        loop {
            if self.send_buf.is_empty() {
                break;
            }
            let limit = self.cwnd.min(self.peer_window_bytes());
            let avail = limit.saturating_sub(self.inflight_bytes());
            if avail == 0 {
                break;
            }
            let n = self.send_buf.len().min(MAX_PAYLOAD).min(avail);
            if n == 0 {
                break;
            }
            let data = self.send_buf.split_to(n).freeze();
            let seq = self.send_next_seq;
            self.send_next_seq = self.send_next_seq.wrapping_add(1);
            let send_base = self.send_base;
            let seg = encode_segment(conn_id, seq, send_base, ack_bits, SegmentHeader::ACK, window, &data);
            self.outgoing.push(seg);
            self.unacked.push_back(Unacked {
                seq,
                data,
                last_sent: now,
                retransmitted: false,
                flags: SegmentHeader::ACK,
            });
            self.stats.sent_segments += 1;
            sent_any = true;
        }
        if sent_any {
            // 数据段自带 ack 字段，等于顺带把确认发出去了。
            self.ack_pending = false;
            self.ack_deadline = None;
        }
        self.arm_rto();
        self.arm_persist(now);
    }

    /// 有数据待发却发不出去（窗口/拥塞窗口为 0）时，起 persist 定时器。
    fn arm_persist(&mut self, now: Instant) {
        if !self.send_buf.is_empty() && !self.fin_sent && !self.fatal {
            if self.persist_deadline.is_none() {
                self.persist_deadline = Some(now + PERSIST_INTERVAL);
            }
        } else {
            self.persist_deadline = None;
        }
    }

    /// 数据都发完且被确认后，发 FIN（**必须等 unacked 空**，否则 FIN 会赶在数据前到达）。
    fn maybe_send_fin(&mut self, now: Instant) {
        if self.fin_sent || !self.app_eof || !self.send_buf.is_empty() || !self.unacked.is_empty() {
            return;
        }
        self.fin_sent = true;
        let seq = self.send_next_seq;
        self.send_next_seq = self.send_next_seq.wrapping_add(1);
        self.fin_seq = Some(seq);
        let ack_bits = self.ack_bits();
        let window = self.window();
        let send_base = self.send_base;
        let conn_id = self.conn_id;
        let seg = encode_segment(
            conn_id,
            seq,
            send_base,
            ack_bits,
            SegmentHeader::FIN | SegmentHeader::ACK,
            window,
            &[],
        );
        self.outgoing.push(seg);
        self.unacked.push_back(Unacked {
            seq,
            data: Bytes::new(),
            last_sent: now,
            retransmitted: false,
            flags: SegmentHeader::FIN | SegmentHeader::ACK,
        });
        self.arm_rto();
    }

    /// 发一条纯 ACK（把当前 `ack`/`ack_bits`/`window` 通告出去）。
    fn flush_ack(&mut self) {
        if !self.ack_pending {
            return;
        }
        let ack_bits = self.ack_bits();
        let window = self.window();
        let seq = self.send_next_seq;
        let recv_next = self.recv_next;
        let conn_id = self.conn_id;
        let seg = encode_segment(conn_id, seq, recv_next, ack_bits, SegmentHeader::ACK, window, &[]);
        self.outgoing.push(seg);
        self.ack_pending = false;
        self.ack_deadline = None;
    }

    /// 零窗口 persist 探测：发一条 `flags == 0` 的裸段，请对端回报当前窗口。
    ///
    /// 用 `flags == 0`（不带 ACK）是关键：对端据「收到 flags==0 段」回一条纯 ACK，而收到
    /// 纯 ACK 的一端**不回复**——因此不会形成 ACK 风暴。
    fn send_persist_probe(&mut self, now: Instant) {
        let ack_bits = self.ack_bits();
        let window = self.window();
        let seq = self.send_next_seq;
        let send_base = self.send_base;
        let conn_id = self.conn_id;
        let seg = encode_segment(conn_id, seq, send_base, ack_bits, 0, window, &[]);
        self.outgoing.push(seg);
        self.persist_deadline = Some(now + PERSIST_INTERVAL);
    }

    /// 计时器到期处理。
    fn on_timeout(&mut self, now: Instant) {
        if let Some(deadline) = self.ack_deadline {
            if now >= deadline {
                self.flush_ack();
            }
        }
        if let Some(deadline) = self.rto_deadline {
            if now >= deadline && !self.unacked.is_empty() {
                self.retransmit_oldest(now);
            }
        }
        if let Some(deadline) = self.persist_deadline {
            if now >= deadline {
                self.send_persist_probe(now);
            }
        }
    }

    /// 一次事件后推进状态机（发数据 → 发 FIN → 发立即确认）。
    fn step(&mut self, now: Instant) {
        if self.fatal {
            return;
        }
        self.pump(now);
        self.maybe_send_fin(now);
        if self.ack_pending && self.ack_deadline.is_none() {
            self.flush_ack();
        }
    }

    /// 本端 FIN 是否已被确认。
    fn fin_acked(&self) -> bool {
        self.fin_sent && self.unacked.is_empty()
    }

    /// 把待发数据报全部写出。
    async fn send_out(&mut self) {
        for seg in std::mem::take(&mut self.outgoing) {
            let _ = self.link.send_datagram(&seg, self.peer).await;
        }
    }
}

/// 起协议任务、返回应用侧那一半流。
fn spawn_conn(conn: Conn, rx: DatagramRx) -> DuplexStream {
    let (app_side, task_side) = tokio::io::duplex(DUPLEX_CAPACITY);
    tokio::spawn(run_conn(conn, task_side, rx));
    app_side
}

/// 协议任务：在应用侧流与数据报之间搬运字节。
async fn run_conn(mut conn: Conn, task_side: DuplexStream, mut rx: DatagramRx) {
    let (mut rd, mut wr) = tokio::io::split(task_side);
    let mut app_buf = vec![0u8; MAX_PAYLOAD];
    let mut write_shutdown = false;

    loop {
        if conn.fatal || (conn.app_eof && conn.fin_acked()) {
            break;
        }

        // 推进状态机并冲刷出站数据报。
        conn.step(Instant::now());
        conn.send_out().await;

        // 对端 FIN：数据交付完后半关闭应用侧写端（应用读到 EOF，但仍可继续写）。
        if conn.peer_fin && !write_shutdown && conn.recv_deliver.is_empty() {
            let _ = wr.shutdown().await;
            write_shutdown = true;
        }

        let deadline = conn.next_deadline();
        let sleep = async {
            match deadline {
                Some(d) => tokio::time::sleep_until(d).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::pin!(sleep);

        tokio::select! {
            maybe = rx.recv() => match maybe {
                Some((from, data)) => conn.on_datagram(from, &data),
                None => break, // demux 通道消失：会话结束。
            },
            res = rd.read(&mut app_buf), if conn.wants_app_read() => match res {
                Ok(0) => conn.on_app_eof(),
                Ok(n) => conn.push_app_data(&app_buf[..n]),
                Err(_) => break,
            },
            res = wr.write(conn.deliver_slice()), if !conn.recv_deliver.is_empty() => match res {
                Ok(0) => {}
                Ok(n) => conn.consume_delivered(n),
                Err(_) => break,
            },
            () = &mut sleep => conn.on_timeout(Instant::now()),
        }
    }
    // 退出即丢弃 task_side：应用侧读端 EOF、写端 BrokenPipe。
}

/// 主动方：周期发 `SYN` 直到收到匹配的 `SYN|ACK`，**只有收到它才进入 Established**
/// （证明双向可达；单向打洞不算成功）。锁存观测到的对端源地址。
///
/// # Errors
/// 握手预算 `budget` 内未收到匹配的 `SYN|ACK`，或 demux 通道关闭时返回 [`RemoteError::Signaling`]。
pub(crate) async fn connect(
    link: Arc<dyn DatagramSender>,
    conn_id: u64,
    mut rx: DatagramRx,
    peer: SocketAddr,
    budget: Duration,
) -> RemoteResult<DuplexStream> {
    let deadline = Instant::now() + budget;
    loop {
        let sent_at = Instant::now();
        let syn = encode_segment(conn_id, 0, 0, 0, SegmentHeader::SYN, recv_window_max(), &[]);
        let _ = link.send_datagram(&syn, peer).await;

        let wait = async {
            while let Some((from, data)) = rx.recv().await {
                if let Some(hdr) = SegmentHeader::parse(&data) {
                    if hdr.conn_id == conn_id
                        && hdr.has(SegmentHeader::SYN)
                        && hdr.has(SegmentHeader::ACK)
                        && hdr.ack == 1
                    {
                        return Some(from);
                    }
                }
            }
            None
        };
        tokio::select! {
            got = wait => match got {
                Some(from) => {
                    let mut conn = Conn::new(link, conn_id, from);
                    conn.update_rtt(Instant::now().saturating_duration_since(sent_at));
                    return Ok(spawn_conn(conn, rx));
                }
                None => return Err(RemoteError::Signaling("打洞 demux 通道已关闭".into())),
            },
            () = tokio::time::sleep_until(sent_at + PROBE_INTERVAL) => {}
        }
        if Instant::now() >= deadline {
            return Err(RemoteError::Signaling(
                "打洞握手超时：未收到 SYN|ACK（单向可达不算成功）".into(),
            ));
        }
    }
}

/// 被动方：等 `SYN`（**接受任何源地址**），向**该数据报的观测源**回 `SYN|ACK`，锁存该地址。
///
/// # Errors
/// 握手预算 `budget` 内未收到 `SYN`，或 demux 通道关闭时返回 [`RemoteError::Signaling`]。
pub(crate) async fn accept(
    link: Arc<dyn DatagramSender>,
    conn_id: u64,
    mut rx: DatagramRx,
    budget: Duration,
) -> RemoteResult<DuplexStream> {
    let deadline = Instant::now() + budget;
    let wait = async {
        while let Some((from, data)) = rx.recv().await {
            if let Some(hdr) = SegmentHeader::parse(&data) {
                // 只认匹配的 SYN；conn_id 不符或非 SYN 一律静默丢弃，**绝不回复**。
                if hdr.conn_id == conn_id && hdr.has(SegmentHeader::SYN) {
                    return Some(from);
                }
            }
        }
        None
    };
    tokio::select! {
        got = wait => match got {
            Some(from) => {
                let syn_ack = encode_segment(
                    conn_id,
                    0,
                    1,
                    0,
                    SegmentHeader::SYN | SegmentHeader::ACK,
                    recv_window_max(),
                    &[],
                );
                let _ = link.send_datagram(&syn_ack, from).await;
                // 被动方没有握手 RTT 样本（不回等 ACK），RTO 用下限起步。
                let conn = Conn::new(link, conn_id, from);
                Ok(spawn_conn(conn, rx))
            }
            None => Err(RemoteError::Signaling("打洞 demux 通道已关闭".into())),
        },
        () = tokio::time::sleep_until(deadline) => {
            Err(RemoteError::Signaling("打洞等待 SYN 超时".into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::punch::PUNCH_BUDGET;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex as StdMutex;

    /// 确定性 xorshift64（与 `tests/rustdesk_server.rs` 的 `next_rand` 同款，保持一致）。
    fn next_rand(state: &mut u64) -> u64 {
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        x
    }

    /// 链路损伤参数（全部按百分比、确定性随机）。
    #[derive(Clone, Copy, Default)]
    struct Impairment {
        /// 丢包率（%）。
        loss_pct: u64,
        /// 乱序率（%，与下一条数据报互换）。
        reorder_pct: u64,
        /// 重复率（%）。
        dup_pct: u64,
        /// 每条数据报的延迟。
        delay: Option<Duration>,
    }

    /// 内存链路共享状态：把发送端的 `send_datagram` 接到接收端的 `rx` 上，并施加损伤。
    struct Net {
        endpoints: StdMutex<HashMap<SocketAddr, mpsc::Sender<(SocketAddr, Bytes)>>>,
        log: StdMutex<Vec<(SocketAddr, SocketAddr, Vec<u8>)>>,
        cfg: StdMutex<Impairment>,
        held: StdMutex<Option<(SocketAddr, SocketAddr, Vec<u8>)>>,
        rng: StdMutex<u64>,
        blackhole: AtomicBool,
        blocked_target: StdMutex<Option<SocketAddr>>,
    }

    impl Net {
        fn new(seed: u64) -> Arc<Self> {
            Arc::new(Self {
                endpoints: StdMutex::new(HashMap::new()),
                log: StdMutex::new(Vec::new()),
                cfg: StdMutex::new(Impairment::default()),
                held: StdMutex::new(None),
                rng: StdMutex::new(seed | 1),
                blackhole: AtomicBool::new(false),
                blocked_target: StdMutex::new(None),
            })
        }

        fn register(&self, addr: SocketAddr) -> DatagramRx {
            let (tx, rx) = mpsc::channel(8192);
            self.endpoints.lock().unwrap().insert(addr, tx);
            rx
        }

        fn set_cfg(&self, cfg: Impairment) {
            *self.cfg.lock().unwrap() = cfg;
        }

        fn set_blackhole(&self, on: bool) {
            self.blackhole.store(on, Ordering::SeqCst);
        }

        fn block_target(&self, addr: SocketAddr) {
            *self.blocked_target.lock().unwrap() = Some(addr);
        }

        fn clear_log(&self) {
            self.log.lock().unwrap().clear();
        }

        /// 某个地址发出的全部数据报字节。
        fn sent_by(&self, from: SocketAddr) -> Vec<Vec<u8>> {
            self.log
                .lock()
                .unwrap()
                .iter()
                .filter(|(f, _, _)| *f == from)
                .map(|(_, _, b)| b.clone())
                .collect()
        }

        /// 全部数据报字节（MTU 断言用）。
        fn all_datagrams(&self) -> Vec<Vec<u8>> {
            self.log.lock().unwrap().iter().map(|(_, _, b)| b.clone()).collect()
        }

        async fn forward(&self, from: SocketAddr, target: SocketAddr, buf: &[u8]) {
            self.log.lock().unwrap().push((from, target, buf.to_vec()));
            if self.blackhole.load(Ordering::SeqCst) {
                return;
            }
            if let Some(blocked) = *self.blocked_target.lock().unwrap() {
                if blocked == target {
                    return;
                }
            }
            let cfg = *self.cfg.lock().unwrap();
            let r = {
                let mut g = self.rng.lock().unwrap();
                next_rand(&mut g)
            };
            if cfg.loss_pct > 0 && (r % 100) < cfg.loss_pct {
                return;
            }
            let dup = cfg.dup_pct > 0 && (r % 100) < cfg.dup_pct;
            let reorder = cfg.reorder_pct > 0 && (r % 100) < cfg.reorder_pct;

            // 相邻互换：先把上一条被压后的包投出去，再把当前这条压后。
            let held = self.held.lock().unwrap().take();
            if let Some((f, t, b)) = held {
                self.deliver(f, t, &b, cfg.delay).await;
            }
            if reorder {
                *self.held.lock().unwrap() = Some((from, target, buf.to_vec()));
                return;
            }
            self.deliver(from, target, buf, cfg.delay).await;
            if dup {
                self.deliver(from, target, buf, cfg.delay).await;
            }
        }

        async fn deliver(&self, from: SocketAddr, target: SocketAddr, buf: &[u8], delay: Option<Duration>) {
            if let Some(d) = delay {
                tokio::time::sleep(d).await;
            }
            let tx = self.endpoints.lock().unwrap().get(&target).cloned();
            if let Some(tx) = tx {
                // `try_send`：链路绝不阻塞发送方；容量足够时不会丢。
                let _ = tx.try_send((from, Bytes::copy_from_slice(buf)));
            }
        }

        /// 直接注入一个数据报（绕过损伤，模拟第三方/伪造报文）。
        fn inject(&self, from: SocketAddr, target: SocketAddr, bytes: Vec<u8>) {
            let tx = self.endpoints.lock().unwrap().get(&target).cloned();
            if let Some(tx) = tx {
                let _ = tx.try_send((from, Bytes::from(bytes)));
            }
        }
    }

    /// 一个端点：实现 [`DatagramSender`]，把自己的地址作为源。
    struct Endpoint {
        net: Arc<Net>,
        local: SocketAddr,
    }

    #[async_trait]
    impl DatagramSender for Endpoint {
        async fn send_datagram(&self, buf: &[u8], target: SocketAddr) -> std::io::Result<usize> {
            self.net.forward(self.local, target, buf).await;
            Ok(buf.len())
        }
    }

    /// 造一个端点（发送端 + 接收端 + 本地址）。
    fn endpoint(net: &Arc<Net>, addr: &str) -> (Arc<dyn DatagramSender>, DatagramRx, SocketAddr) {
        let local: SocketAddr = addr.parse().unwrap();
        let rx = net.register(local);
        let sender: Arc<dyn DatagramSender> = Arc::new(Endpoint {
            net: Arc::clone(net),
            local,
        });
        (sender, rx, local)
    }

    /// 起一对已连通的端点（好链路），返回 (主动方应用侧, 被动方应用侧)。
    async fn connected_pair(net: &Arc<Net>) -> (DuplexStream, DuplexStream) {
        let (link_a, rx_a, _addr_a) = endpoint(net, "10.0.0.1:1111");
        let (link_b, rx_b, addr_b) = endpoint(net, "10.0.0.2:2222");
        let conn_id = 0xABCD_1234_5678_9ABC;
        let a = connect(link_a, conn_id, rx_a, addr_b, PUNCH_BUDGET);
        let b = accept(link_b, conn_id, rx_b, PUNCH_BUDGET);
        let (ra, rb) = tokio::join!(a, b);
        (ra.unwrap(), rb.unwrap())
    }

    /// 造一段确定性图案。
    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| u8::try_from(i % 251).unwrap()).collect()
    }

    /// 断言链路里没有超过 MTU 预算的数据报。
    fn assert_within_mtu(net: &Arc<Net>) {
        for d in net.all_datagrams() {
            assert!(
                d.len() <= SEGMENT_HEADER_LEN + MAX_PAYLOAD,
                "数据报 {} 字节超过 MTU 预算 {}",
                d.len(),
                SEGMENT_HEADER_LEN + MAX_PAYLOAD
            );
        }
    }

    // ───────────────────────── 段头与布局 ─────────────────────────

    #[test]
    fn segment_header_layout_is_frozen() {
        // 锁的不变量：段头**恰好 27 字节**，`conn_id` 在偏移 4、magic 在偏移 0，
        // 所有多字节字段**大端序**。demux 的快速路径只读前 12 字节就要路由，
        // 任何挪动字段的改动都会悄悄破坏它，故在此逐字节钉死。
        let hdr = SegmentHeader {
            conn_id: 0x0102_0304_0506_0708,
            seq: 0x1112_1314,
            ack: 0x2122_2324,
            ack_bits: 0x3132,
            flags: SegmentHeader::SYN | SegmentHeader::ACK,
            window: 0x4142,
            len: 0x0400,
        };
        let bytes = hdr.encode();
        assert_eq!(bytes.len(), SEGMENT_HEADER_LEN, "段头必须恰好 27 字节");
        assert_eq!(&bytes[0..4], b"RTP1", "magic 必须在偏移 0");
        assert_eq!(
            &bytes[4..12],
            &[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08],
            "conn_id 必须在偏移 4、大端"
        );
        assert_eq!(&bytes[12..16], &[0x11, 0x12, 0x13, 0x14], "seq 大端");
        assert_eq!(&bytes[16..20], &[0x21, 0x22, 0x23, 0x24], "ack 大端");
        assert_eq!(&bytes[20..22], &[0x31, 0x32], "ack_bits 大端");
        assert_eq!(bytes[22], 0x03, "flags 在偏移 22");
        assert_eq!(&bytes[23..25], &[0x41, 0x42], "window 大端");
        assert_eq!(&bytes[25..27], &[0x04, 0x00], "len 大端");

        // 往返一致。
        assert_eq!(SegmentHeader::parse(&bytes), Some(hdr));
        // 常量自洽：MAGIC 就是 b"RTP1"，偏移 4 起 8 字节是 conn_id。
        assert_eq!(MAGIC, *b"RTP1");
    }

    #[test]
    fn segment_header_parse_rejects_bad_input() {
        let good = SegmentHeader {
            conn_id: 1,
            seq: 1,
            ack: 0,
            ack_bits: 0,
            flags: SegmentHeader::ACK,
            window: 8,
            len: 4,
        }
        .encode();

        // 长度不足 27。
        for n in 0..SEGMENT_HEADER_LEN {
            assert_eq!(SegmentHeader::parse(&good[..n]), None, "长度 {n} 应被拒绝");
        }
        // magic 不符。
        let mut bad_magic = good;
        bad_magic[3] = b'X';
        assert_eq!(SegmentHeader::parse(&bad_magic), None);
        // len > MAX_PAYLOAD。
        let mut bad_len = good;
        bad_len[25..27].copy_from_slice(&(u16::try_from(MAX_PAYLOAD + 1).unwrap()).to_be_bytes());
        assert_eq!(SegmentHeader::parse(&bad_len), None);
        // 边界：len == MAX_PAYLOAD 合法。
        let mut max_len = good;
        max_len[25..27].copy_from_slice(&(u16::try_from(MAX_PAYLOAD).unwrap()).to_be_bytes());
        assert!(SegmentHeader::parse(&max_len).is_some());
    }

    #[test]
    fn mtu_budget_is_respected() {
        // 反推：段头 + 满载荷必须塞进「不触发 IP 分片」的保守预算。
        // IPv6 最小 MTU 1500 − 40（IPv6 头）− 8（UDP 头）= 1452，再留余量。
        let ipv6_min_mtu = 1500usize;
        let ipv6_header = 40usize;
        let udp_header = 8usize;
        let budget = ipv6_min_mtu - ipv6_header - udp_header - 225; // 1452 - 225 = 1227
        assert!(budget == 1227, "预算常量自洽（27 + 1200）");

        // 满载荷段的真实长度必须 ≤ 预算。
        let full = encode_segment(0, 1, 0, 0, SegmentHeader::ACK, 64, &vec![0u8; MAX_PAYLOAD]);
        assert!(
            full.len() <= budget,
            "满载荷段 {} 字节超过 MTU 预算 {budget}，会触发 IP 分片",
            full.len()
        );
    }

    #[test]
    fn seq_after_wraps_correctly() {
        assert!(seq_after(2, 1));
        assert!(!seq_after(1, 2));
        assert!(!seq_after(5, 5));
        // 回绕：0 在 u32::MAX 之后。
        assert!(seq_after(0, u32::MAX));
        assert!(!seq_after(u32::MAX, 0));
    }

    // ───────────────────────── 可靠层（Conn 单元） ─────────────────────────

    fn bare_conn(net: &Arc<Net>, conn_id: u64) -> Conn {
        let (link, _rx, _addr) = endpoint(net, "10.0.0.1:1000");
        Conn::new(link, conn_id, "10.0.0.2:2000".parse().unwrap())
    }

    /// 造一个对端段（默认窗口 64、ACK 标志）。
    fn peer_seg(conn_id: u64, seq: u32, ack: u32, len: u16) -> SegmentHeader {
        SegmentHeader {
            conn_id,
            seq,
            ack,
            ack_bits: 0,
            flags: SegmentHeader::ACK,
            window: 64,
            len,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn rto_backs_off_exponentially() {
        let net = Net::new(1);
        let mut conn = bare_conn(&net, 42);
        let base = Instant::now();
        conn.push_app_data(&[7u8; 100]);
        conn.pump(base);
        assert_eq!(conn.unacked.len(), 1, "应发出一段");

        let mut times = Vec::new();
        for _ in 0..5 {
            let d = conn.rto_deadline.expect("应有 RTO 定时器");
            let before = conn.outgoing.len();
            conn.on_timeout(d);
            if conn.outgoing.len() > before {
                times.push(u64::try_from(d.saturating_duration_since(base).as_millis()).unwrap());
            }
        }
        // 间隔：200、400、800、1600、2000（上限 2s）→ 累计 200、600、1400、3000、5000。
        assert_eq!(times, vec![200, 600, 1400, 3000, 5000], "RTO 应指数退避并封顶 2s");
    }

    #[tokio::test(start_paused = true)]
    async fn fast_retransmit_precedes_rto() {
        let net = Net::new(2);
        let mut conn = bare_conn(&net, 7);
        let base = Instant::now();
        conn.push_app_data(&[1u8; 100]);
        conn.pump(base);
        assert_eq!(conn.unacked.len(), 1);
        let before = conn.outgoing.len();

        // 三个重复 ACK（ack == send_base），都发生在 RTO 到期之前。
        for _ in 0..3 {
            conn.on_segment(peer_seg(7, 0, 1, 0), &[], base + Duration::from_millis(10));
        }
        assert_eq!(conn.stats.fast_retransmits, 1, "3 个重复 ACK 应触发快速重传");
        assert_eq!(conn.stats.rto_timeouts, 0, "快速重传必须早于 RTO");
        assert!(conn.outgoing.len() > before, "应重发了一段");
    }

    #[tokio::test(start_paused = true)]
    async fn rto_retransmit_reaches_peer_and_clears() {
        // 一段数据被丢一次：重传后应被确认、unacked 清空、且不再超时。
        let net = Net::new(3);
        let mut conn = bare_conn(&net, 11);
        let base = Instant::now();
        conn.push_app_data(&[5u8; 50]);
        conn.pump(base);
        let d = conn.rto_deadline.unwrap();
        conn.on_timeout(d);
        assert_eq!(conn.stats.rto_timeouts, 1);
        // 对端确认 seq 2（即 ack 了 seq 1）。
        conn.on_segment(peer_seg(11, 0, 2, 0), &[], d + Duration::from_millis(5));
        assert!(conn.unacked.is_empty(), "确认后应清空未确认队列");
        assert_eq!(conn.rto_deadline, None, "无未确认段则无 RTO 定时器");
    }

    #[tokio::test(start_paused = true)]
    async fn blackhole_after_retransmit_limit_becomes_fatal() {
        let net = Net::new(4);
        let mut conn = bare_conn(&net, 13);
        let base = Instant::now();
        conn.push_app_data(&[9u8; 30]);
        conn.pump(base);
        // 一直超时直到致命（有界，不无限重传）。
        let mut guard = 0;
        while !conn.fatal && guard < 100 {
            let d = conn.rto_deadline.expect("应有 RTO");
            conn.on_timeout(d);
            guard += 1;
        }
        assert!(conn.fatal, "超过重传上限应判链路死亡");
        assert!(guard <= MAX_RETRANSMITS + 2, "应在有界次数内退出，实为 {guard}");
    }

    #[tokio::test(start_paused = true)]
    async fn out_of_window_segments_are_dropped_and_counted() {
        let net = Net::new(5);
        let mut conn = bare_conn(&net, 9);
        let now = Instant::now();

        // 远超窗口的段：丢弃并计数。
        let far = 1 + u32::try_from(MAX_RECV_WINDOW).unwrap() + 5;
        conn.on_segment(peer_seg(9, far, 0, 4), &[1, 2, 3, 4], now);
        assert_eq!(conn.stats.out_of_window_drops, 1);
        assert!(conn.recv_ooo.is_empty());

        // 灌入大量窗口内乱序段：乱序缓冲必须有硬上界。
        for i in 0..400u32 {
            let seq = 2 + i;
            conn.on_segment(peer_seg(9, seq, 0, 1), &[9], now);
        }
        assert!(
            conn.recv_ooo.len() <= MAX_RECV_WINDOW,
            "乱序缓冲硬上界应为 {MAX_RECV_WINDOW} 段，实为 {}",
            conn.recv_ooo.len()
        );
        assert!(conn.stats.out_of_window_drops > 1, "超出窗口的段应被计数丢弃");
    }

    #[tokio::test(start_paused = true)]
    async fn receive_window_shrinks_to_zero_when_app_stalls() {
        // 应用不消费时，接收窗口必须收缩到 0（发送方据此停发）——这是内存上界的来源。
        let net = Net::new(6);
        let mut conn = bare_conn(&net, 21);
        let now = Instant::now();
        for i in 1..=u32::try_from(MAX_RECV_WINDOW).unwrap() {
            conn.on_segment(peer_seg(21, i, 0, 1), &[1], now);
        }
        assert_eq!(conn.window(), 0, "缓冲满后窗口应为 0");
        // 应用读走一段 → 窗口重新打开。
        conn.consume_delivered(1);
        assert!(conn.window() > 0, "应用消费后窗口应恢复");
    }

    // ───────────────────────── 端到端（内存链路） ─────────────────────────

    #[tokio::test(start_paused = true)]
    async fn end_to_end_punch_connects_both_sides_and_moves_data() {
        let net = Net::new(10);
        let (mut a, mut b) = connected_pair(&net).await;
        // 双向数据。
        a.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        b.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");
        b.write_all(b"pong").await.unwrap();
        a.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"pong");
    }

    #[tokio::test(start_paused = true)]
    async fn connect_times_out_when_syn_ack_never_returns() {
        let net = Net::new(11);
        let (link_a, rx_a, addr_a) = endpoint(&net, "10.0.0.1:1111");
        let (link_b, rx_b, addr_b) = endpoint(&net, "10.0.0.2:2222");
        // 阻断 B→A：被动方能收到 SYN，但回不去（单向打洞）。
        net.block_target(addr_a);
        let conn_id = 1;
        let a = connect(link_a, conn_id, rx_a, addr_b, PUNCH_BUDGET);
        let b = accept(link_b, conn_id, rx_b, PUNCH_BUDGET);
        let (ra, rb) = tokio::join!(a, b);
        assert!(ra.is_err(), "只在收到匹配的 SYN|ACK 时才提交；单向可达必须超时");
        assert!(rb.is_ok(), "被动方收到 SYN 即成功（它只等 SYN，不回等 ACK）");
    }

    /// 被动方那条 `SYN|ACK` 丢一次**不能**让握手失败：它不在 `unacked` 里、`arm_rto` 对空队列
    /// 返回 `None`，所以**没有任何定时重传**——唯一的补救是「对端重发裸 `SYN` 时补发一次」。
    ///
    /// 丢包链路上单丢一条 `SYN|ACK` 的概率并不低，而它一旦成为单点故障就等于按丢包率线性拉低
    /// 打洞成功率（打洞失败会回落中继，功能不受损，但打洞的收益全被吃掉）。
    #[tokio::test(start_paused = true)]
    async fn duplicate_syn_recovers_a_dropped_syn_ack() {
        let net = Net::new(19);
        let (link_a, rx_a, addr_a) = endpoint(&net, "10.0.0.1:1111");
        let (link_b, rx_b, addr_b) = endpoint(&net, "10.0.0.2:2222");
        // 挡住 B→A 的一切：被动方的 `SYN|ACK` 发得出去但到不了（等价于链路丢它）。
        net.block_target(addr_a);
        let conn_id = 5;

        let a = connect(link_a, conn_id, rx_a, addr_b, PUNCH_BUDGET);
        let b = accept(link_b, conn_id, rx_b, PUNCH_BUDGET);
        // 两个 `PROBE_INTERVAL` 之后放行：此时被动方已发过 `SYN|ACK`（被丢），主动方也已重发过
        // 至少一次裸 `SYN`，因此**补救只能来自「收到裸 SYN 时补发」**这条路。
        let unblock = async {
            tokio::time::sleep(PROBE_INTERVAL * 2).await;
            *net.blocked_target.lock().unwrap() = None;
        };
        let (ra, rb, ()) = tokio::join!(a, b, unblock);
        assert!(rb.is_ok(), "被动方收到 SYN 即成功");
        assert!(
            ra.is_ok(),
            "丢掉一条 SYN|ACK 后仍应在预算内建立（靠对端重发的裸 SYN 触发补发），实得 {ra:?}"
        );
    }

    /// 补发只由**裸 `SYN`** 触发；收到 `SYN|ACK` 不触发。
    ///
    /// 这条锁的是**回环**：ISN 恒为 0 ⇒ 两端的 `SYN|ACK` 形状完全对称、无法区分谁是谁的。若
    /// `SYN|ACK` 也触发补发，两端会互相触发，形成每 RTT 一包的无限 `SYN|ACK` 回声。
    #[tokio::test(start_paused = true)]
    async fn syn_ack_does_not_trigger_a_resend() {
        let net = Net::new(20);
        let mut conn = bare_conn(&net, 0x00AB_CDEF);
        let now = Instant::now();

        // 裸 SYN（主动方重发：`connect` 里 `encode_segment(..., SYN, ...)` 只带 SYN 标志）。
        let mut bare_syn = peer_seg(0x00AB_CDEF, 0, 0, 0);
        bare_syn.flags = SegmentHeader::SYN;
        let before = conn.outgoing.len();
        conn.on_segment(bare_syn, &[], now);
        assert_eq!(conn.outgoing.len(), before + 1, "裸 SYN 应触发一次 SYN|ACK 补发");
        let resend = SegmentHeader::parse(&conn.outgoing[before]).expect("补发的必须是合法段");
        assert_eq!(resend.conn_id, 0x00AB_CDEF, "补发的 conn_id 必须一致");
        assert!(resend.has(SegmentHeader::SYN) && resend.has(SegmentHeader::ACK));
        assert_eq!((resend.seq, resend.ack), (0, 1), "补发的仍是握手段（seq 0 / ack 1）");

        // 对称的 `SYN|ACK`（对端也在握手）**不得**触发补发——否则两端无限回声。
        let before = conn.outgoing.len();
        let mut syn_ack = peer_seg(0x00AB_CDEF, 0, 1, 0);
        syn_ack.flags = SegmentHeader::SYN | SegmentHeader::ACK;
        conn.on_segment(syn_ack, &[], now);
        assert_eq!(conn.outgoing.len(), before, "收到 SYN|ACK 不得补发（否则会与对端互相触发）");
    }

    #[tokio::test(start_paused = true)]
    async fn connect_times_out_in_blackhole() {
        let net = Net::new(12);
        net.set_blackhole(true);
        let (link_a, rx_a, _) = endpoint(&net, "10.0.0.1:1111");
        let addr_b: SocketAddr = "10.0.0.2:2222".parse().unwrap();
        let r = connect(link_a, 2, rx_a, addr_b, PUNCH_BUDGET).await;
        assert!(r.is_err(), "黑洞下 connect 必须在预算内报错");
    }

    #[tokio::test(start_paused = true)]
    async fn accept_times_out_without_syn() {
        let net = Net::new(13);
        let (link_b, rx_b, _) = endpoint(&net, "10.0.0.2:2222");
        let r = accept(link_b, 3, rx_b, PUNCH_BUDGET).await;
        assert!(r.is_err(), "无 SYN 时 accept 必须在预算内报错");
    }

    #[tokio::test(start_paused = true)]
    async fn accept_ignores_unmatched_conn_id_and_never_replies() {
        let net = Net::new(14);
        let (link_b, rx_b, addr_b) = endpoint(&net, "10.0.0.2:2222");
        net.clear_log();
        // 注入一个 conn_id 不符的 SYN（来自某个第三方地址）。
        let intruder: SocketAddr = "203.0.113.5:3333".parse().unwrap();
        let bad = encode_segment(0xDEAD_BEEF, 0, 0, 0, SegmentHeader::SYN, 64, &[]);
        net.inject(intruder, addr_b, bad);

        // 在短窗口内跑 accept：既没有匹配的 SYN，也**不得**回复任何数据报。
        let r = tokio::time::timeout(Duration::from_millis(500), accept(link_b, 5, rx_b, PUNCH_BUDGET)).await;
        assert!(r.is_err(), "无匹配 SYN 应超时");
        assert!(net.sent_by(addr_b).is_empty(), "未匹配的数据报绝不回复");
    }

    #[tokio::test(start_paused = true)]
    async fn two_mib_survives_five_percent_loss() {
        let net = Net::new(0x5EED_0001);
        net.set_cfg(Impairment {
            loss_pct: 5,
            ..Impairment::default()
        });
        let (a, b) = connected_pair(&net).await;
        let payload = pattern(2 * 1024 * 1024);
        let expect = payload.clone();
        let writer = tokio::spawn(async move {
            let mut a = a;
            a.write_all(&payload).await.unwrap();
            a.shutdown().await.unwrap();
        });
        let mut got = Vec::new();
        let mut b = b;
        tokio::time::timeout(Duration::from_secs(600), b.read_to_end(&mut got))
            .await
            .expect("2 MiB 应在预算内传完")
            .unwrap();
        writer.await.unwrap();
        assert_eq!(got, expect, "5% 丢包下必须逐字节相同");
        assert_within_mtu(&net);
    }

    #[tokio::test(start_paused = true)]
    async fn data_survives_reorder_and_duplication() {
        let net = Net::new(0x5EED_0002);
        net.set_cfg(Impairment {
            reorder_pct: 20,
            dup_pct: 15,
            ..Impairment::default()
        });
        let (a, b) = connected_pair(&net).await;
        let payload = pattern(512 * 1024);
        let expect = payload.clone();
        let writer = tokio::spawn(async move {
            let mut a = a;
            a.write_all(&payload).await.unwrap();
            a.shutdown().await.unwrap();
        });
        let mut got = Vec::new();
        let mut b = b;
        tokio::time::timeout(Duration::from_secs(600), b.read_to_end(&mut got))
            .await
            .expect("乱序+重复下应在预算内传完")
            .unwrap();
        writer.await.unwrap();
        assert_eq!(got, expect, "乱序+重复注入下必须逐字节相同（去重正确）");
    }

    #[tokio::test(start_paused = true)]
    async fn fin_yields_eof_on_read() {
        let net = Net::new(15);
        let (mut a, mut b) = connected_pair(&net).await;
        a.write_all(b"hello-fin").await.unwrap();
        a.shutdown().await.unwrap();
        let mut buf = Vec::new();
        tokio::time::timeout(Duration::from_secs(30), b.read_to_end(&mut buf))
            .await
            .expect("FIN 后读端应到达 EOF")
            .unwrap();
        assert_eq!(&buf, b"hello-fin");
    }

    #[tokio::test(start_paused = true)]
    async fn rst_closes_stream() {
        let net = Net::new(16);
        let (mut a, mut b) = connected_pair(&net).await;
        let a_addr: SocketAddr = "10.0.0.1:1111".parse().unwrap();
        let b_addr: SocketAddr = "10.0.0.2:2222".parse().unwrap();
        let conn_id = 0xABCD_1234_5678_9ABC;
        let rst = encode_segment(conn_id, 1, 0, 0, SegmentHeader::RST, 0, &[]);
        net.inject(a_addr, b_addr, rst);

        // 先读：RST 处理后读端 EOF（`DuplexStream` 的对端被丢弃）。
        let mut buf = [0u8; 8];
        let n = tokio::time::timeout(Duration::from_secs(30), b.read(&mut buf))
            .await
            .expect("RST 后读端应结束")
            .unwrap();
        assert_eq!(n, 0, "RST 后读端应为 EOF");
        // 再写：写端报错。
        let werr = tokio::time::timeout(Duration::from_secs(30), b.write_all(b"x"))
            .await
            .expect("写不应挂起");
        assert!(werr.is_err(), "RST 后写端应报错");
        let _ = &mut a;
    }

    #[tokio::test(start_paused = true)]
    async fn blackhole_gives_up_within_budget() {
        let net = Net::new(17);
        let (mut a, _b) = connected_pair(&net).await;
        net.set_blackhole(true);
        // 写点数据后读：链路死亡应**有界**结束（EOF），不无限重传。
        a.write_all(b"stuck").await.unwrap();
        let mut buf = [0u8; 8];
        let n = tokio::time::timeout(Duration::from_secs(120), a.read(&mut buf))
            .await
            .expect("黑洞下应有界退出，而非无限重传")
            .unwrap();
        assert_eq!(n, 0, "链路死亡后读端应 EOF");
    }

    #[tokio::test(start_paused = true)]
    async fn backpressure_bounds_sender_writes() {
        let net = Net::new(18);
        // `_b` 从不读：接收窗口收缩到 0，发送方必须有界阻塞。
        let (mut a, _b) = connected_pair(&net).await;
        let chunk = vec![0xAAu8; MAX_PAYLOAD];
        let mut written = 0usize;
        for _ in 0..4096 {
            match tokio::time::timeout(Duration::from_millis(200), a.write_all(&chunk)).await {
                Ok(Ok(())) => written += chunk.len(),
                _ => break,
            }
        }
        assert!(written > 0, "应先能写进去一些");
        assert!(
            written < 1024 * 1024,
            "接收方不读时发送方必须有界阻塞（内存与对端行为无关），实写 {written} 字节"
        );
    }
}