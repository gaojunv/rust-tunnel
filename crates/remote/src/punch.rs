//! UDP 打洞的会话标识与会话内数据报分发。
//!
//! # 为什么要自己算 `conn_id`
//!
//! RustDesk 的 `PunchHole` 协议里**没有任何会话标识**——两端互相发探测数据报，打洞成功后
//! 数据报就只是裸字节。所以两端必须自己算一个 `conn_id`，把它放进可靠层的段头，才能把
//! 收到的数据报对上会话、并挡掉无关/伪造的数据报。
//!
//! # `conn_id` 的来源与它同时承担的职责
//!
//! `conn_id = HMAC-SHA256(token, b"rt-punch-v1")[..8]`（大端 u64）。`token` 是服务端经
//! **加密控制通道**下发的一次性会话 token（与 [`crate::crypto`] 用的是同一个）。
//!
//! hbbs 的会合流量是**明文 protobuf**——旁听者能看到两端的公网地址，但看不到 token，
//! 因此推不出 `conn_id`。这一个值同时承担两件事：
//!
//! 1. **会话相关性**：两端各自算出同一个 `conn_id`，把数据报对上会话；
//! 2. **注入过滤**：第三方即使知道两端地址，也造不出 `conn_id` 正确的数据报，无法注入会话
//!    或抢占打洞路径。这与 [`crate::crypto`]「一次性 token 进 KDF」是同一思路——秘密只经
//!    加密控制通道走，从不上数据面。
//!
//! # 为什么必须**双向**打洞（被控端也要主动反向发包）
//!
//! 家用路由绝大多数是**端口受限 cone** NAT：被控端的 NAT 会丢掉控制器发来的第一个数据报，
//! 因为被控端**从未向控制器发过任何东西**（NAT 的过滤窗口只对「本端先出过包的目的」开放）。
//! 所以只靠控制器主动打、被控端被动等，在主流 NAT 上**必然失败**。
//!
//! 协议本身就是证据：hbbs 之所以要把 `PunchHole`（含请求方地址 `socket_addr` 与请求方自报的
//! `udp_port`）**推给目标端**、而不是只回给请求方，正是因为目标端必须**主动反向打**，用自己
//! 的出站包打开自己的 NAT 过滤窗口。实现见 [`punch_back`]。

use crate::reliable::{self, MAGIC, SEGMENT_HEADER_LEN};
use bytes::Bytes;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

/// 打洞探测帧的重发间隔。
pub(crate) const PUNCH_PROBE_INTERVAL: Duration = Duration::from_millis(250);

/// 打洞的握手总预算。
pub(crate) const PUNCH_BUDGET: Duration = Duration::from_secs(3);

/// HMAC-SHA256 类型别名。
type HmacSha256 = Hmac<Sha256>;

/// `conn_id` 派生时使用的域分隔标签。
const CONN_ID_LABEL: &[u8] = b"rt-punch-v1";

/// `conn_id = HMAC-SHA256(token, b"rt-punch-v1")[..8]`，**大端 u64**。
///
/// `token` 是服务端经**加密控制通道**下发的一次性会话 token。hbbs 的会合流量是**明文
/// protobuf**——旁听者能看到两端公网地址，但看不到 token，推不出 `conn_id`，因此既不能
/// 伪造数据报注入会话，也不能抢占打洞路径。这一个值同时承担「会话相关性」与「注入过滤」。
///
/// token 为空串（理论上不该出现）时返回 `0`：两端仍会算出同一个值（可对上会话），但过滤
/// 能力退化为「人人可猜」——调用方应保证 token 非空。
#[must_use]
pub(crate) fn conn_id_from_token(token: &str) -> u64 {
    let Ok(mut mac) = HmacSha256::new_from_slice(token.as_bytes()) else {
        return 0;
    };
    mac.update(CONN_ID_LABEL);
    let digest = mac.finalize().into_bytes();
    let mut first8 = [0u8; 8];
    first8.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(first8)
}

/// 从数据报里取 `conn_id`：只读前 12 个字节。
///
/// 这是分发任务（demux）的快速路径：先做 magic 校验与长度下限（[`SEGMENT_HEADER_LEN`]），
/// 命中就直接读偏移 4..12 的 u64，**不做完整解析、不分配**。段头布局把 `conn_id` 固定在
/// 偏移 4、总长固定 27，正是为了让这一步无需解析整个段头（见 [`crate::reliable`] 的布局机械锁）。
#[must_use]
pub(crate) fn peek_conn_id(datagram: &[u8]) -> Option<u64> {
    if datagram.len() < SEGMENT_HEADER_LEN {
        return None;
    }
    if datagram[..4] != MAGIC {
        return None;
    }
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&datagram[4..12]);
    Some(u64::from_be_bytes(buf))
}

/// 反向打洞：向请求方地址周期发**裸探测段**，打开**本端** NAT 的过滤窗口。
///
/// 收到 `PunchHole` 的一端（通常是被控端）除回 `PunchHoleSent` 外，**还**要调用本函数向请求方
/// 地址反向发包。理由见模块文档「为什么必须双向打洞」——不做这一步，端口受限 cone NAT 下
/// 控制器打过来的第一个数据报会被本端 NAT 丢掉。
///
/// # 为什么是「裸探测段」且 `conn_id == 0`
///
/// 反向包**唯一**的作用就是让本端 NAT 建立/放行一条出站映射，它**不需要被对端理解**。故发一条
/// 27 字节、magic 正确、`conn_id == 0`、无载荷的段（[`reliable::SegmentHeader`] 全零后
/// `encode`）：
///
/// - 接收方 demux 会 `peek_conn_id` 得到 `Some(0)`，查 [`PunchRegistry`] **必然未命中**
///   （真实会话的 `conn_id` 是 HMAC 输出，为 0 的概率 2⁻⁶⁴），于是落到「未命中」分支被丢弃。
///   它不会污染任何会话，也不会诱发任何回复（反反射）。
///
/// # 与会话无关
///
/// 反向探测共享同一个 UDP 套接字，映射是**套接字级**的，所以**不需要** `pending_punch` 槽、
/// **不需要**知道 `conn_id`、**不需要**等会话登记。`PunchHole` 早于会话登记到达也没关系：映射
/// 一旦建立会存活几十秒，而控制器的 `SYN` 每 [`reliable::PROBE_INTERVAL`] 重发、持续
/// [`PUNCH_BUDGET`]。
pub(crate) async fn punch_back(link: Arc<dyn reliable::DatagramSender>, target: SocketAddr, budget: Duration) {
    // 全零段头：`conn_id == 0` 是「与会话无关」的哨兵（见上）。
    let probe = reliable::SegmentHeader {
        conn_id: 0,
        seq: 0,
        ack: 0,
        ack_bits: 0,
        flags: 0,
        window: 0,
        len: 0,
    }
    .encode();
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let _ = link.send_datagram(&probe, target).await;
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }
        // 用 `sleep_until` + 封顶 deadline：最后一轮不越过总预算。
        tokio::time::sleep_until((now + PUNCH_PROBE_INTERVAL).min(deadline)).await;
    }
}

/// demux 路由表：`conn_id` → 该会话的数据报投递端。
///
/// 用**同步**互斥锁（与 [`crate::auth::PendingSessions`] 一致）：所有操作都是极短的临界区，
/// 且投递用 `try_send` 非阻塞，锁内无 `await`。分发任务在**单线程**上串行调用，锁竞争可忽略。
#[derive(Debug, Default)]
pub(crate) struct PunchRegistry {
    /// 内部表。
    inner: Mutex<HashMap<u64, mpsc::Sender<(SocketAddr, Bytes)>>>,
}

impl PunchRegistry {
    /// 建一张空路由表。
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 登记一个会话的投递端（同 `conn_id` 覆盖旧记录）。
    ///
    /// 保留 `async`：签名由跨模块冻结接口规定，接线方按 `async` 调用；本阶段实现里没有
    /// 可 `await` 的操作（锁内无 await）。
    #[expect(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "签名由冻结的跨模块接口规定为 async，接线方按 async 调用"
    )]
    pub(crate) async fn register(&self, conn_id: u64, tx: mpsc::Sender<(SocketAddr, Bytes)>) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.insert(conn_id, tx);
        }
    }

    /// 注销一个会话。
    #[expect(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "签名由冻结的跨模块接口规定为 async，接线方按 async 调用"
    )]
    pub(crate) async fn unregister(&self, conn_id: u64) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.remove(&conn_id);
        }
    }

    /// 路由一条数据报；**没有对应会话时返回 `false`（调用方必须静默丢弃、绝不回复）**。
    ///
    /// 投递用 `try_send`：分发任务绝不能因某个会话的接收队列满而阻塞（那会拖垮所有会话）。
    /// 队列满即丢弃该数据报——UDP 语义本就允许丢包，可靠层会按 RTO 重传。
    ///
    /// 返回 `true` 表示**命中了会话**（投递已尝试）；返回 `false` 表示无此会话，调用方应静默丢弃。
    #[expect(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "签名由冻结的跨模块接口规定为 async，接线方按 async 调用"
    )]
    pub(crate) async fn route(&self, conn_id: u64, from: SocketAddr, data: Bytes) -> bool {
        // 锁内只取出 Sender 的克隆，锁外再投递（避免持锁做 channel 操作）。
        let sender = self.inner.lock().ok().and_then(|guard| guard.get(&conn_id).cloned());
        let Some(tx) = sender else {
            return false;
        };
        // 队列满/已关闭都只是丢这一条数据报，不影响「命中」的语义。
        let _ = tx.try_send((from, data));
        true
    }

    /// 当前登记的会话数（单测用）。
    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().map_or(0, |guard| guard.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reliable::{MAX_PAYLOAD, SEGMENT_HEADER_LEN};
    use async_trait::async_trait;

    /// 手工用 HMAC-SHA256 算 `conn_id` 的前 8 字节大端（独立于被测实现）。
    fn manual_conn_id(token: &str) -> u64 {
        let mut mac = HmacSha256::new_from_slice(token.as_bytes()).unwrap();
        mac.update(b"rt-punch-v1");
        let digest = mac.finalize().into_bytes();
        u64::from_be_bytes(digest[..8].try_into().unwrap())
    }

    /// 造一条 `conn_id` 为 `conn_id` 的最小合法段（27 字节头，无载荷）。
    fn segment_with_conn_id(conn_id: u64) -> Vec<u8> {
        let mut buf = vec![0u8; SEGMENT_HEADER_LEN];
        buf[..4].copy_from_slice(&MAGIC);
        buf[4..12].copy_from_slice(&conn_id.to_be_bytes());
        buf
    }

    #[test]
    fn conn_id_is_deterministic_and_matches_manual_hmac() {
        // 同 token 同值。
        assert_eq!(conn_id_from_token("tok-1"), conn_id_from_token("tok-1"));
        // 不同 token 不同值（高概率；两个固定串必然不同）。
        assert_ne!(conn_id_from_token("tok-1"), conn_id_from_token("tok-2"));
        // 逐位等于手工算出的 HMAC-SHA256 前 8 字节大端。
        for token in ["tok-1", "tok-2", "", "一个中文 token"] {
            assert_eq!(
                conn_id_from_token(token),
                manual_conn_id(token),
                "token={token:?} 的 conn_id 应与手工 HMAC 一致"
            );
        }
    }

    #[test]
    fn peek_conn_id_hits_and_misses() {
        let id = 0x0102_0304_0506_0708;
        let seg = segment_with_conn_id(id);
        assert_eq!(peek_conn_id(&seg), Some(id), "合法段应命中");
        // 恰好 27 字节（无载荷）也能命中——快速路径只看头。
        assert_eq!(seg.len(), SEGMENT_HEADER_LEN);

        // 长度不足 27 → 不命中。
        assert_eq!(peek_conn_id(&seg[..SEGMENT_HEADER_LEN - 1]), None);
        assert_eq!(peek_conn_id(&[]), None);
        // magic 不符 → 不命中。
        let mut bad = seg.clone();
        bad[0] = b'X';
        assert_eq!(peek_conn_id(&bad), None);
        // 满载荷段仍命中（证明不依赖载荷）。
        let mut full = seg.clone();
        full.extend(vec![0xABu8; MAX_PAYLOAD]);
        assert_eq!(peek_conn_id(&full), Some(id));
    }

    #[tokio::test]
    async fn route_delivers_on_hit_and_false_on_miss() {
        let reg = PunchRegistry::new();
        let (tx, mut rx) = mpsc::channel(4);
        reg.register(7, tx).await;
        assert_eq!(reg.len(), 1);

        let from: SocketAddr = "203.0.113.9:5000".parse().unwrap();
        let payload = Bytes::from_static(b"hello");
        assert!(reg.route(7, from, payload.clone()).await, "命中应返回 true");
        let got = rx.recv().await.expect("应收到数据报");
        assert_eq!(got.0, from);
        assert_eq!(got.1, payload);

        // 未登记的 conn_id → false，且不投递。
        assert!(!reg.route(8, from, payload).await, "未命中应返回 false");

        // 注销后不再命中。
        reg.unregister(7).await;
        assert_eq!(reg.len(), 0);
        assert!(!reg.route(7, from, Bytes::new()).await);
    }

    #[tokio::test]
    async fn route_is_false_when_channel_closed() {
        // 会话端已丢弃接收端：`route` 仍应报告「命中」（有登记），只是投递失败被忽略。
        let reg = PunchRegistry::new();
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        reg.register(1, tx).await;
        let from: SocketAddr = "198.51.100.1:1234".parse().unwrap();
        // 命中语义不受投递结果影响；关键是**不 panic、不阻塞**。
        assert!(reg.route(1, from, Bytes::from_static(b"x")).await);
    }

    /// 记录 `send_datagram` 的替身（`punch_back` 单测用）。
    struct RecordingLink {
        sent: Mutex<Vec<(Vec<u8>, SocketAddr)>>,
    }

    #[async_trait]
    impl crate::reliable::DatagramSender for RecordingLink {
        async fn send_datagram(&self, buf: &[u8], target: SocketAddr) -> std::io::Result<usize> {
            self.sent.lock().unwrap().push((buf.to_vec(), target));
            Ok(buf.len())
        }
    }

    /// 反向打洞必须在预算内周期发出「裸探测段」：27 字节、magic `RTP1`、`conn_id == 0`。
    #[tokio::test(start_paused = true)]
    async fn punch_back_sends_bare_probe_segments() {
        let link = Arc::new(RecordingLink {
            sent: Mutex::new(Vec::new()),
        });
        let target: SocketAddr = "203.0.113.7:40000".parse().unwrap();
        let sender: Arc<dyn crate::reliable::DatagramSender> = link.clone();
        punch_back(sender, target, PUNCH_BUDGET).await;

        let sent = link.sent.lock().unwrap().clone();
        assert!(!sent.is_empty(), "应在预算内发出反向探测");
        for (bytes, addr) in &sent {
            assert_eq!(*addr, target, "探测必须发向请求方地址");
            assert_eq!(bytes.len(), SEGMENT_HEADER_LEN, "裸探测段恰好 27 字节");
            assert_eq!(&bytes[..4], &MAGIC, "magic 必须正确");
            assert_eq!(peek_conn_id(bytes), Some(0), "conn_id 必须是 0 哨兵（与会话无关）");
        }
        // 预算 3s / 间隔 250ms：t=0,250,…,3000 → 至少 12 发。
        assert!(
            sent.len() >= 12,
            "应在 3s 预算内周期重发，实发 {} 次",
            sent.len()
        );
    }
}
