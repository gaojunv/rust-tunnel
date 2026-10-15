//! NAT 类型分类与打洞失败缓存。
//!
//! 跨 NAT 直连只有在**端点无关映射**（cone）下才可行：对称 NAT 对每个
//! `(源IP, 源端口, 目的)` 三元组独立分配映射，从对端看过去本端每次的源端口都不同，打洞
//! 几乎必然失败。因此发起打洞前先分类一次，对称 NAT 直接走中继、**零额外等待**。
//!
//! # 分类方法（以及它为什么必须是「两个目的端口」）
//!
//! 用**同一个**套接字向**两个不同的目的端口**各发一条探测，再比对对端观测到的源端口：
//! 相等 → cone，不等 → 对称。
//!
//! 常见误判点：有人用**两个本地套接字**各发一条探测再比对源端口——这**恒不相等**（两个本地
//! 套接字本来就必然得到两个不同映射），因此无法区分「对称」与「端点无关」。判定必须落在
//! **同一套接字、不同目的**这一维上。
//!
//! # 诚实的局限
//!
//! 两个目的端口落在**同一个 IP**（NAT 探测服务通常只暴露一台）。因此本方法检测不到
//! 「地址无关但**端口依赖**」的映射（现实中罕见）——这类会被误判成 cone，白打一次洞，
//! 由 [`NatCache`] 的失败缓存兜住（下次直接走中继）。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tokio::time::Instant;

/// 一次 NAT 探测的等待上限（两个目的端口**并发**各发一次，故最坏只等一个超时）。
///
/// 接线方（[`crate::signaling`] 的 `probe_nat`）**并发**向两个目的端口发出探测，再在
/// 本超时内最多收两条应答——而不是串行各等一次（串行最坏 2 × 800ms，而它在会话关键路径上）。
pub(crate) const NAT_PROBE_TIMEOUT: Duration = Duration::from_millis(800);

/// 打洞失败缓存的 TTL。
///
/// 10 分钟：短于一次典型的网络切换（换 Wi-Fi、切热点）间隔，避免「换网络后还记着旧失败」；
/// 又长于一次会话重连的间隔，避免同一对端反复白付 3s 打洞预算。
pub(crate) const NAT_FAILURE_TTL: Duration = Duration::from_secs(600);

/// NAT 映射类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NatClass {
    /// 端点无关映射（cone）：打洞可行。
    Cone,
    /// 对称 NAT：打洞通常失败，直接走中继、**零额外等待**。
    Symmetric,
    /// 判不出来（老服务端没有 UDP NAT 端口、探测超时）：**fail-open，照打一次**。
    Unknown,
}

/// **纯函数**：同一套接字向**两个不同目的端口**各发一条探测后观测到的源端口。
///
/// 相等 → [`NatClass::Cone`]，不等 → [`NatClass::Symmetric`]，任一为 `None`（探测超时、
/// 老服务端无 UDP NAT 端口）→ [`NatClass::Unknown`]。
///
/// 抽成纯函数以便表驱动单测：分类的全部逻辑都在这里，套接字与探测流程在调用方。
#[must_use]
pub(crate) fn classify(port_a: Option<u16>, port_b: Option<u16>) -> NatClass {
    match (port_a, port_b) {
        (Some(a), Some(b)) if a == b => NatClass::Cone,
        (Some(_), Some(_)) => NatClass::Symmetric,
        _ => NatClass::Unknown,
    }
}

/// 进程内的打洞失败缓存（按对端 `rd_id` 索引）。
///
/// # 为什么刻意不放 client-gui 的偏好文件
///
/// 它是**传输层状态**：随传输重建自然失效。偏好文件需要 schema + 持久化 + 失效策略，且跨
/// 网络切换会变陈旧。代价只是「App 重启后首次连某对端多付一次 [`crate::punch::PUNCH_BUDGET`]」。
///
/// 时钟用 [`tokio::time::Instant`]（而非 `std`）以便单测用虚拟时钟确定性地跨过 TTL。
#[derive(Debug, Default)]
pub(crate) struct NatCache {
    /// 对端 `rd_id` → 最近一次失败时刻。
    failures: Mutex<HashMap<String, Instant>>,
}

impl NatCache {
    /// 建一张空缓存。
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 是否值得尝试打洞。
    ///
    /// `Symmetric` → 恒 `false`（打洞无望，省下 [`crate::punch::PUNCH_BUDGET`]）。
    /// `Cone` / `Unknown` → 除非该对端在 [`NAT_FAILURE_TTL`] 内刚失败过。
    #[must_use]
    pub(crate) fn should_punch(&self, peer_rd_id: &str, class: NatClass) -> bool {
        if class == NatClass::Symmetric {
            return false;
        }
        let Ok(guard) = self.failures.lock() else {
            // 锁中毒：按「没失败过」处理（fail-open，照打一次）。
            return true;
        };
        match guard.get(peer_rd_id) {
            Some(at) => at.elapsed() >= NAT_FAILURE_TTL,
            None => true,
        }
    }

    /// 记录一次打洞失败（进入 TTL 冷却）。
    pub(crate) fn record_failure(&self, peer_rd_id: &str) {
        if let Ok(mut guard) = self.failures.lock() {
            guard.insert(peer_rd_id.to_owned(), Instant::now());
        }
    }

    /// 记录一次打洞成功（清掉失败记录，下次照打）。
    pub(crate) fn record_success(&self, peer_rd_id: &str) {
        if let Ok(mut guard) = self.failures.lock() {
            guard.remove(peer_rd_id);
        }
    }

    /// 当前缓存条目数（单测用）。
    #[cfg(test)]
    fn len(&self) -> usize {
        self.failures.lock().map_or(0, |guard| guard.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_is_table_driven() {
        // 同一套接字、两个不同目的端口观测到同一源端口 → cone。
        assert_eq!(classify(Some(40000), Some(40000)), NatClass::Cone);
        // 观测到不同源端口 → 对称 NAT。
        assert_eq!(classify(Some(40000), Some(41234)), NatClass::Symmetric);
        // 任一探测超时/缺失 → 判不出来，fail-open。
        assert_eq!(classify(None, Some(40000)), NatClass::Unknown);
        assert_eq!(classify(Some(40000), None), NatClass::Unknown);
        assert_eq!(classify(None, None), NatClass::Unknown);
    }

    #[test]
    fn symmetric_never_punches_even_after_success() {
        let cache = NatCache::new();
        // 即便刚成功过，对称 NAT 也不打（分类是硬否决，不查缓存）。
        cache.record_success("peer-1");
        assert!(!cache.should_punch("peer-1", NatClass::Symmetric));
        cache.record_failure("peer-1");
        assert!(!cache.should_punch("peer-1", NatClass::Symmetric));
    }

    #[test]
    fn cone_and_unknown_default_to_punching() {
        let cache = NatCache::new();
        assert!(cache.should_punch("peer-1", NatClass::Cone), "没失败过就该打");
        assert!(cache.should_punch("peer-1", NatClass::Unknown), "未知也 fail-open 照打");
    }

    #[tokio::test(start_paused = true)]
    async fn failure_suppresses_within_ttl_and_expires_after() {
        let cache = NatCache::new();
        assert!(cache.should_punch("peer-1", NatClass::Cone));
        cache.record_failure("peer-1");
        assert_eq!(cache.len(), 1);

        // TTL 内：不打。
        assert!(!cache.should_punch("peer-1", NatClass::Cone), "TTL 内应抑制");
        assert!(!cache.should_punch("peer-1", NatClass::Unknown), "Unknown 也受抑制");
        // 另一个对端不受影响（按 rd_id 索引）。
        assert!(cache.should_punch("peer-2", NatClass::Cone));

        // 逼近 TTL 边界：仍不打。
        tokio::time::advance(NAT_FAILURE_TTL.saturating_sub(Duration::from_secs(1))).await;
        assert!(!cache.should_punch("peer-1", NatClass::Cone));

        // 越过 TTL：恢复。
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(cache.should_punch("peer-1", NatClass::Cone), "TTL 过后应恢复");
    }

    #[tokio::test(start_paused = true)]
    async fn record_success_clears_failure() {
        let cache = NatCache::new();
        cache.record_failure("peer-1");
        assert!(!cache.should_punch("peer-1", NatClass::Cone));
        cache.record_success("peer-1");
        assert_eq!(cache.len(), 0, "成功应清掉失败记录");
        assert!(cache.should_punch("peer-1", NatClass::Cone));
    }
}
