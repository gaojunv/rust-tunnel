//! 汇合服务器（hbbs）的对端登记表。
//!
//! # 登记表里有什么
//!
//! 每个 ID 一条记录，保存它在**控制通道上的可达地址**（`RegisterPeer` 的来源地址）、
//! 公钥、`uuid` 与最近一次心跳时间。汇合服务器本身不转发任何会话数据，它的全部价值
//! 就是回答两类问题：
//!
//! - 「ID `x` 现在在线吗？它的公网地址是什么？」——打洞要用（[`PeerRegistry::lookup`]）。
//! - 「ID `x` 的公钥是什么？请用你的私钥签个名。」——`ID ↔ 公钥` 绑定要用
//!   （[`PeerRegistry::pk_of`]，签名在 [`crate::key::ServerKey::sign_id_pk`]）。
//!
//! # 在线的定义
//!
//! 「在线」= 最近 [`REG_TIMEOUT`] 内收到过 `RegisterPeer` 心跳。客户端每几秒发一次，
//! 所以这个判定不需要服务端主动探测。
//!
//! 与上游的一处有意差异：上游把「从未注册」表示成一个**很早的时间戳**
//! （`Instant::now() - 一年`），在单调时钟从开机起算的平台上这会下溢。这里改用
//! `Option<Instant>`，`None` 即「从未心跳」，语义更直白也没有下溢风险。
//!
//! # 限流与封禁
//!
//! `RegisterPk` 会写公钥，是唯一能让服务端状态被别人改写的入口，因此上游对它做了两层
//! 限制，本模块同样实现：**单 ID 频率窗口**（[`REG_PK_WINDOW`] 内最多 3 次）与
//! **单 IP 请求窗口**（[`IP_BLOCK_WINDOW`] 内超过 [`IP_BLOCK_MAX`] 次即拒，另按
//! [`IP_IDS_WINDOW`] 限制同一 IP 能出现的不同 ID 数）。两者都以「拒绝」而非「封禁」
//! 的形式生效——被限流的请求得到 [`PkResult::TooFrequent`]，连接照常关闭。

use crate::proto::register_pk_response::Result as PkResult;
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// 超过这个时长没有心跳就视为离线。
pub const REG_TIMEOUT: Duration = Duration::from_secs(30);

/// ID 长度上限（与上游一致；也是常见主机名长度上限）。
pub const MAX_PEER_ID_LEN: usize = 253;

/// ID 长度下限。
pub const MIN_PEER_ID_LEN: usize = 6;

/// `RegisterPk` 的频率窗口。
const REG_PK_WINDOW: Duration = Duration::from_secs(6);

/// 频率窗口内允许的 `RegisterPk` 次数（超过即 [`PkResult::TooFrequent`]）。
const REG_PK_BURST: u32 = 2;

/// 单 IP 请求计数的窗口。
const IP_BLOCK_WINDOW: Duration = Duration::from_secs(60);

/// 窗口内单 IP 允许的最大请求数。
const IP_BLOCK_MAX: u32 = 30;

/// 单 IP 见过的 ID 集合的统计窗口。
const IP_IDS_WINDOW: Duration = Duration::from_hours(24);

/// 窗口内单 IP 允许出现的不同 ID 数。
const IP_IDS_MAX: usize = 300;

/// 对端在登记表里的状态。
#[derive(Debug)]
struct Peer {
    /// 最近一次心跳/注册的来源地址；`port == 0` 表示只知道过 IP、还没有可用地址。
    socket_addr: SocketAddr,
    /// 最近一次心跳时间；`None` 表示从未心跳过。
    last_reg_time: Option<Instant>,
    /// 客户端自报的随机标识，用于识别「同一 ID 换了台机器」。
    uuid: Vec<u8>,
    /// 客户端公钥。
    pk: Vec<u8>,
    /// 最近一次登记时记录的来源 IP（文本形式，供「IP 变了没有」的比较）。
    ip: String,
    /// (`RegisterPk` 频率窗口内的计数, 窗口起点)。
    reg_pk: (u32, Instant),
}

impl Default for Peer {
    fn default() -> Self {
        Self {
            socket_addr: SocketAddr::from(([0, 0, 0, 0], 0)),
            last_reg_time: None,
            uuid: Vec::new(),
            pk: Vec::new(),
            ip: String::new(),
            reg_pk: (0, Instant::now()),
        }
    }
}

impl Peer {
    /// 距最近一次心跳的时长；从未心跳过则返回 [`REG_TIMEOUT`]。
    fn elapsed(&self) -> Duration {
        self.last_reg_time.map_or(REG_TIMEOUT, |t| t.elapsed())
    }

    fn is_online(&self) -> bool {
        self.elapsed() < REG_TIMEOUT
    }
}

/// 对端的对外只读视图。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerView {
    /// 最近一次心跳/注册的来源地址。
    pub socket_addr: SocketAddr,
    /// 距最近一次心跳的时长。
    pub elapsed: Duration,
}

impl PeerView {
    /// 是否仍在 [`REG_TIMEOUT`] 内。
    #[must_use]
    pub fn is_online(&self) -> bool {
        self.elapsed < REG_TIMEOUT
    }
}

/// `RegisterPk` 的结果：既是对客户端的应答，也是登记表是否被改动的信号。
///
/// 只有 [`PkResult::Ok`] 表示公钥已登记并可被签名。
pub type RegisterPkOutcome = PkResult;

/// 对端登记表。
///
/// 全部状态在内存里：进程重启后需要客户端重新注册。这是有意的——`RegisterPk` 在每次
/// 客户端启动时都会发，重启丢失的只是「离线对端的公钥」，而离线对端本来也连不上。
/// 需要跨重启保留时由上层（服务端 crate）在启动时预置与写穿。
#[derive(Debug, Default)]
pub struct PeerRegistry {
    peers: RwLock<HashMap<String, Peer>>,
    blocker: RwLock<IpBlocker>,
}

impl PeerRegistry {
    /// 建一张空登记表。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 处理 `RegisterPeer`（心跳）。
    ///
    /// 返回**是否需要客户端补发 `RegisterPk`**。三种情况下需要：
    /// - 这个 ID 从未出现过；
    /// - 它的公钥还没登记过；
    /// - 来源 IP 变了（换了网络，旧的公钥绑定可能已被顶替）。
    ///
    /// 需要补发时**不更新地址与心跳时间**——此时对端还算不上「在线可用」，因为别人拿
    /// 不到它的公钥。这是上游的行为，也确实是更安全的顺序：先登记公钥，再接受打洞。
    ///
    /// 回环地址的 IP 变化被忽略（本地调试时地址天然会「变」）。
    pub async fn register_peer(&self, id: &str, addr: SocketAddr) -> bool {
        let mut peers = self.peers.write().await;
        let Some(peer) = peers.get_mut(id) else {
            peers.insert(id.to_owned(), Peer::default());
            return true;
        };
        let ip = addr.ip();
        let ip_changed = if peer.socket_addr.port() != 0 {
            ip != peer.socket_addr.ip()
        } else {
            ip.to_string() != peer.ip
        } && !ip.is_loopback();
        let request_pk = peer.pk.is_empty() || ip_changed;
        if !request_pk {
            peer.socket_addr = addr;
            peer.last_reg_time = Some(Instant::now());
        }
        request_pk
    }

    /// 处理 `RegisterPk`：登记或更新 `uuid` 与公钥。
    ///
    /// 返回值直接作为 `RegisterPkResponse.result` 回给客户端。
    ///
    /// # 与 `uuid` 有关的拒绝
    ///
    /// `uuid` 是客户端生成的随机值，服务端用它识别「同一个 ID 换了台机器」。若已有记录
    /// 的 `uuid` 与本次不同，一律 [`PkResult::UuidMismatch`]；若 `uuid` 相同但**同时**
    /// 换了 IP 又换了公钥，同样拒绝（可能是被冒用）。这两个判断共同保证：一个 ID 的公钥
    /// 不会被后来者悄悄顶替，客户端之间基于公钥的互信才成立。
    pub async fn register_pk(
        &self,
        id: &str,
        uuid: &[u8],
        pk: &[u8],
        addr: SocketAddr,
        ip: &str,
    ) -> RegisterPkOutcome {
        if !(MIN_PEER_ID_LEN..=MAX_PEER_ID_LEN).contains(&id.len()) {
            return PkResult::UuidMismatch;
        }
        if !self.check_ip_blocker(addr.ip(), id).await {
            return PkResult::TooFrequent;
        }

        let mut peers = self.peers.write().await;
        let peer = peers.entry(id.to_owned()).or_default();

        let changed = if peer.uuid.is_empty() {
            true
        } else {
            if peer.uuid == uuid {
                if peer.ip != ip && peer.pk != pk {
                    return PkResult::UuidMismatch;
                }
            } else {
                return PkResult::UuidMismatch;
            }
            peer.pk != pk || peer.ip != ip
        };

        // 频率窗口：窗口内已发满就拒。计数在窗口过期或成功受理时才推进。
        let now = Instant::now();
        if peer.reg_pk.1.elapsed() > REG_PK_WINDOW {
            peer.reg_pk.0 = 0;
        } else if peer.reg_pk.0 > REG_PK_BURST {
            return PkResult::TooFrequent;
        }
        peer.reg_pk.0 += 1;
        peer.reg_pk.1 = now;

        if changed {
            peer.socket_addr = addr;
            peer.last_reg_time = Some(now);
            peer.uuid = uuid.to_vec();
            peer.pk = pk.to_vec();
            ip.clone_into(&mut peer.ip);
        }
        PkResult::Ok
    }

    /// 查一个 ID 的在线情况；未知 ID 返回 `None`。
    pub async fn lookup(&self, id: &str) -> Option<PeerView> {
        self.peers.read().await.get(id).map(|p| PeerView {
            socket_addr: p.socket_addr,
            elapsed: p.elapsed(),
        })
    }

    /// 取一个 ID 已登记的公钥；未知或未登记公钥返回 `None`。
    pub async fn pk_of(&self, id: &str) -> Option<Vec<u8>> {
        self.peers
            .read()
            .await
            .get(id)
            .filter(|p| !p.pk.is_empty())
            .map(|p| p.pk.clone())
    }

    /// 把一个 ID 的在线状态写成 `OnlineResponse.states` 需要的位图。
    ///
    /// 位序与上游一致：**从左到右**，第 `i` 个 ID 对应第 `i / 8` 字节的第 `7 - i % 8`
    /// 位（即每字节内高位在前）。位图长度按 ID 数量向上取整。
    pub async fn online_bitmap(&self, ids: &[String]) -> Vec<u8> {
        let peers = self.peers.read().await;
        let mut states = vec![0u8; ids.len().div_ceil(8)];
        for (i, id) in ids.iter().enumerate() {
            let online = peers.get(id).is_some_and(Peer::is_online);
            if online {
                // `i / 8` 与 `i % 8` 一定落在位图范围内，不会越界。
                if let Some(byte) = states.get_mut(i / 8) {
                    *byte |= 1 << (7 - i % 8);
                }
            }
        }
        states
    }

    /// 登记表里的 ID 总数。
    pub async fn len(&self) -> usize {
        self.peers.read().await.len()
    }

    /// 登记表是否为空。
    pub async fn is_empty(&self) -> bool {
        self.peers.read().await.is_empty()
    }

    /// 当前在线（[`REG_TIMEOUT`] 内有心跳）的 ID 数。
    pub async fn online_count(&self) -> usize {
        self.peers.read().await.values().filter(|p| p.is_online()).count()
    }

    /// 按 IP 限流。返回 `false` 表示这次请求应当被拒。
    async fn check_ip_blocker(&self, ip: IpAddr, id: &str) -> bool {
        let mut blocker = self.blocker.write().await;
        blocker.allow(ip, id)
    }
}

/// 单 IP 的请求限流状态。
#[derive(Debug, Default)]
struct IpBlocker {
    entries: HashMap<IpAddr, IpEntry>,
}

#[derive(Debug)]
struct IpEntry {
    /// 请求计数与其窗口起点。
    requests: (u32, Instant),
    /// 见过的 ID 集合与其窗口起点。
    ids: (HashSet<String>, Instant),
}

impl IpBlocker {
    /// 记录一次请求并判断是否放行。
    fn allow(&mut self, ip: IpAddr, id: &str) -> bool {
        let now = Instant::now();
        let entry = self.entries.entry(ip).or_insert_with(|| IpEntry {
            requests: (0, now),
            ids: (HashSet::new(), now),
        });

        // 请求频率：窗口内超过上限就拒。
        if entry.requests.1.elapsed() > IP_BLOCK_WINDOW {
            entry.requests.0 = 0;
        } else if entry.requests.0 > IP_BLOCK_MAX {
            return false;
        }
        entry.requests.0 += 1;
        entry.requests.1 = now;

        // 不同 ID 数：窗口内见过太多 ID 的 IP 只允许继续用已见过的那些，
        // 否则一台机器就能批量抢占 ID。
        let is_new = !entry.ids.0.contains(id);
        if entry.ids.1.elapsed() > IP_IDS_WINDOW {
            entry.ids.0.clear();
        } else if entry.ids.0.len() > IP_IDS_MAX {
            return !is_new;
        }
        if is_new {
            entry.ids.0.insert(id.to_owned());
        }
        entry.ids.1 = now;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().expect("测试地址应可解析")
    }

    /// 走完「心跳 → 登记公钥」的正常流程。
    async fn register(registry: &PeerRegistry, id: &str, uuid: &[u8], pk: &[u8], a: &str) {
        registry.register_peer(id, addr(a)).await;
        let a = addr(a);
        let outcome = registry
            .register_pk(id, uuid, pk, a, &a.ip().to_string())
            .await;
        assert_eq!(outcome, PkResult::Ok, "id={id} 登记公钥应成功");
    }

    #[tokio::test]
    async fn first_heartbeat_requests_pk() {
        let registry = PeerRegistry::new();
        assert!(registry.register_peer("123456", addr("1.2.3.4:5000")).await);
        // 还没登记公钥，因此不算在线。
        let view = registry.lookup("123456").await.expect("应已建记录");
        assert!(!view.is_online());
    }

    #[tokio::test]
    async fn heartbeat_after_pk_registration_marks_online() {
        let registry = PeerRegistry::new();
        register(&registry, "123456", b"uuid", b"pk", "1.2.3.4:5000").await;
        assert!(
            !registry.register_peer("123456", addr("1.2.3.4:5000")).await,
            "公钥已登记且 IP 未变，不该再要求补发"
        );
        let view = registry.lookup("123456").await.expect("应已建记录");
        assert!(view.is_online());
        assert_eq!(view.socket_addr, addr("1.2.3.4:5000"));
    }

    #[tokio::test]
    async fn ip_change_requests_pk_again() {
        let registry = PeerRegistry::new();
        register(&registry, "123456", b"uuid", b"pk", "1.2.3.4:5000").await;
        assert!(
            registry.register_peer("123456", addr("9.9.9.9:6000")).await,
            "换了 IP 应要求重新登记公钥"
        );
    }

    #[tokio::test]
    async fn loopback_ip_change_is_ignored() {
        // 本地调试时端口/地址天然会变，不该每次都被要求补发公钥。
        let registry = PeerRegistry::new();
        register(&registry, "123456", b"uuid", b"pk", "127.0.0.1:5000").await;
        assert!(
            !registry.register_peer("123456", addr("127.0.0.1:5001")).await,
            "回环地址的变化不应触发重新登记"
        );
    }

    #[tokio::test]
    async fn different_uuid_is_rejected() {
        let registry = PeerRegistry::new();
        register(&registry, "123456", b"uuid-a", b"pk", "1.2.3.4:5000").await;
        let outcome = registry
            .register_pk("123456", b"uuid-b", b"pk", addr("1.2.3.4:5001"), "1.2.3.4")
            .await;
        assert_eq!(outcome, PkResult::UuidMismatch);
        // 原记录没被改动。
        assert_eq!(registry.pk_of("123456").await.as_deref(), Some(&b"pk"[..]));
    }

    #[tokio::test]
    async fn simultaneous_ip_and_pk_change_is_rejected() {
        let registry = PeerRegistry::new();
        register(&registry, "123456", b"uuid", b"pk-old", "1.2.3.4:5000").await;
        let outcome = registry
            .register_pk("123456", b"uuid", b"pk-new", addr("9.9.9.9:5000"), "9.9.9.9")
            .await;
        assert_eq!(outcome, PkResult::UuidMismatch, "同时换 IP 与公钥应被拒");
    }

    #[tokio::test]
    async fn pk_change_without_ip_change_is_allowed() {
        // 客户端重装/换密钥但网络不变，是合法场景。
        let registry = PeerRegistry::new();
        register(&registry, "123456", b"uuid", b"pk-old", "1.2.3.4:5000").await;
        let outcome = registry
            .register_pk("123456", b"uuid", b"pk-new", addr("1.2.3.4:5000"), "1.2.3.4")
            .await;
        assert_eq!(outcome, PkResult::Ok);
        assert_eq!(registry.pk_of("123456").await.as_deref(), Some(&b"pk-new"[..]));
    }

    #[tokio::test]
    async fn short_and_long_ids_are_rejected() {
        let registry = PeerRegistry::new();
        for id in ["abc", "12345"] {
            let outcome = registry
                .register_pk(id, b"uuid", b"pk", addr("1.2.3.4:5000"), "1.2.3.4")
                .await;
            assert_eq!(outcome, PkResult::UuidMismatch, "id={id} 过短应被拒");
        }
        let long = "a".repeat(MAX_PEER_ID_LEN + 1);
        let outcome = registry
            .register_pk(&long, b"uuid", b"pk", addr("1.2.3.4:5000"), "1.2.3.4")
            .await;
        assert_eq!(outcome, PkResult::UuidMismatch, "超长 id 应被拒");

        // 恰好处于下限与上限的 ID 应被接受。
        for id in ["123456".to_owned(), "b".repeat(MAX_PEER_ID_LEN)] {
            let outcome = registry
                .register_pk(&id, b"uuid", b"pk", addr("1.2.3.4:5000"), "1.2.3.4")
                .await;
            assert_eq!(outcome, PkResult::Ok, "id 长度 {} 应被接受", id.len());
        }
    }

    #[tokio::test]
    async fn repeated_register_pk_is_rate_limited() {
        let registry = PeerRegistry::new();
        // 同一 ID 在频率窗口内允许 3 次，第 4 次被拒。
        let outcomes: Vec<_> = futures_util::future::join_all((0..4).map(|_| {
            registry.register_pk("123456", b"uuid", b"pk", addr("1.2.3.4:5000"), "1.2.3.4")
        }))
        .await;
        assert_eq!(outcomes[0], PkResult::Ok);
        assert_eq!(outcomes[1], PkResult::Ok);
        assert_eq!(outcomes[2], PkResult::Ok);
        assert_eq!(outcomes[3], PkResult::TooFrequent, "第 4 次应被限流");
    }

    #[tokio::test]
    async fn one_ip_registering_many_ids_is_capped() {
        // 直接验证限流表本身：同一 IP 前 IP_IDS_MAX 个新 ID 放行，之后再来的新 ID 被拒，
        // 但已见过的 ID 仍放行。
        let mut blocker = IpBlocker::default();
        let ip = addr("1.2.3.4:1").ip();
        // 先把请求频率窗口撑满是不行的（会先被频率限制拦下），所以分成多个窗口来验证：
        // 这里直接构造一个「已见过很多 ID」的条目。
        blocker.entries.insert(
            ip,
            IpEntry {
                requests: (0, Instant::now()),
                ids: (
                    (0..=IP_IDS_MAX).map(|i| format!("id{i:06}")).collect(),
                    Instant::now(),
                ),
            },
        );
        assert!(blocker.allow(ip, "id000000"), "已见过的 ID 应继续放行");
        assert!(!blocker.allow(ip, "brand-new-id"), "新 ID 应被拒");
    }

    #[tokio::test]
    async fn ip_request_flood_is_blocked() {
        let mut blocker = IpBlocker::default();
        let ip = addr("1.2.3.4:1").ip();
        for i in 0..=IP_BLOCK_MAX {
            assert!(blocker.allow(ip, &format!("id{i:06}")), "第 {i} 次应放行");
        }
        assert!(!blocker.allow(ip, "id999999"), "超频后应被拒");
    }

    #[tokio::test]
    async fn online_bitmap_matches_upstream_bit_order() {
        let registry = PeerRegistry::new();
        // 让 id-2 与 id-9 在线（下标 1 与 8）。
        for id in ["id-000000", "id-000001", "id-000002"] {
            registry.register_peer(id, addr("1.2.3.4:5000")).await;
            registry
                .register_pk(id, b"uuid", b"pk", addr("1.2.3.4:5000"), "1.2.3.4")
                .await;
        }
        let ids: Vec<String> = ["id-000000", "id-000001", "id-000002", "missing-id"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let bitmap = registry.online_bitmap(&ids).await;
        assert_eq!(bitmap.len(), 1, "4 个 ID 只需 1 字节");
        // 下标 0、1、2 在线，位序从高位开始：0b1110_0000。
        assert_eq!(bitmap[0], 0b1110_0000);

        // 跨字节：9 个 ID 时下标 8 落在第 2 个字节的最高位。
        let many: Vec<String> = (0..9).map(|i| format!("id-{i:06}")).collect();
        registry.register_peer(&many[8], addr("5.6.7.8:1")).await;
        registry
            .register_pk(&many[8], b"uuid", b"pk", addr("5.6.7.8:1"), "5.6.7.8")
            .await;
        let bitmap = registry.online_bitmap(&many).await;
        assert_eq!(bitmap.len(), 2);
        assert_eq!(bitmap[1], 0b1000_0000, "第 9 个 ID 在第 2 字节最高位");
    }

    #[tokio::test]
    async fn offline_peer_is_not_in_bitmap() {
        let registry = PeerRegistry::new();
        // 只有心跳、没有登记公钥 → 不算在线。
        registry.register_peer("id-000000", addr("1.2.3.4:5000")).await;
        let bitmap = registry.online_bitmap(&["id-000000".to_owned()]).await;
        assert_eq!(bitmap, vec![0]);
    }

    #[tokio::test]
    async fn counts_track_registered_and_online_peers() {
        let registry = PeerRegistry::new();
        assert!(registry.is_empty().await);
        register(&registry, "id-000001", b"u1", b"p1", "1.2.3.4:1").await;
        register(&registry, "id-000002", b"u2", b"p2", "1.2.3.5:2").await;
        // 只建了记录、没登记公钥。
        registry.register_peer("id-000003", addr("1.2.3.6:3")).await;
        assert_eq!(registry.len().await, 3);
        assert_eq!(registry.online_count().await, 2);
        assert_eq!(registry.pk_of("id-000003").await, None);
        assert_eq!(registry.lookup("unknown-id").await, None);
    }
}
