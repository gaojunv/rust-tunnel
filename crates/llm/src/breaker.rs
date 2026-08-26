//! 按模型粒度的内存熔断器。
//!
//! 状态机：Closed（正常计数）→ 连续失败达 `FAILURE_THRESHOLD` 转 Open（拒绝请求）。
//! Open 冷却期满后，下一个 `allow` 调用通过单飞标志抢到半开试探权：
//! 试探成功 → Closed 复位；试探失败 → 冷却翻倍（上限 `MAX_COOLDOWN_SECS`）重新 Open。
//! 熔断状态纯内存，不落库；进程重启即清零（符合"重启恢复"语义）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 陈旧试探回收宽限：冷却期满后再过 5 分钟（上游读超时）视为陈旧，可重新夺取试探权。
const STALE_PROBE_GRACE: std::time::Duration = std::time::Duration::from_mins(5);

/// 连续失败多少次打开熔断。
pub const FAILURE_THRESHOLD: u32 = 5;
/// 首次冷却秒数。
pub const BASE_COOLDOWN_SECS: u64 = 30;
/// 冷却上限秒数（半开失败指数退避到此为止）。
pub const MAX_COOLDOWN_SECS: u64 = 600;

/// 对外暴露的熔断状态视图。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerStateView {
    /// 正常。
    Closed,
    /// 熔断中（冷却期内）。
    Open,
    /// 半开试探进行中（已有请求抢到试探权）。
    HalfOpenProbe,
}

/// 单个模型的熔断快照（管理 API 用）。
#[derive(Debug, Clone)]
pub struct BreakerSnapshot {
    /// 当前状态。
    pub state: BreakerStateView,
    /// 连续失败计数（Open 时保持打开时的值）。
    pub consecutive_failures: u32,
    /// Open 状态的剩余冷却秒数；其他状态为 0。
    pub cooldown_remaining_secs: u64,
}

/// 内部状态。
#[derive(Debug, Default)]
struct BreakerEntry {
    consecutive_failures: u32,
    /// None = Closed；Some = Open（含打开时间与当前冷却时长）。
    open: Option<OpenState>,
    /// 半开试探单飞标志：true 表示已有试探在飞。
    probe_in_flight: bool,
}

#[derive(Debug, Clone, Copy)]
struct OpenState {
    opened_at: Instant,
    cooldown: Duration,
}

/// 按 model_id 统计的熔断器集合。`Clone` 共享内部状态（Arc）。
#[derive(Debug, Clone, Default)]
pub struct ModelBreakers {
    inner: Arc<Mutex<HashMap<String, BreakerEntry>>>,
}

impl ModelBreakers {
    /// 新建空熔断器集合。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 是否放行该模型的请求。
    ///
    /// - Closed：放行。
    /// - Open 冷却未满：拒绝。
    /// - Open 冷却已满：第一个到达的请求抢到半开试探权（放行），
    ///   试探期间其余请求拒绝（单飞）。
    #[must_use]
    pub fn allow(&self, model_id: &str) -> bool {
        let mut map = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = map.entry(model_id.to_string()).or_default();
        match entry.open {
            None => true,
            Some(open) => {
                let elapsed = open.opened_at.elapsed();
                // 冷却未满 → 拒绝
                if elapsed < open.cooldown {
                    false
                } else if entry.probe_in_flight {
                    // 陈旧试探回收：试探请求被客户端断开时 record_* 不会执行，
                    // probe_in_flight 可能永真（单模型场景永久 503）。
                    // 超过冷却 + 上游读超时（300s）视为陈旧，允许重新夺取。
                    if elapsed > open.cooldown + STALE_PROBE_GRACE {
                        // 重新夺取：把 opened_at 重置到冷却刚满的锚点，开启新的试探窗口。
                        // 否则陈旧窗口（opened_at 仍在过去）对后续请求恒成立，单飞失效，
                        // 会同时放行多个并发试探冲击上游。
                        if let Some(o) = entry.open.as_mut() {
                            o.opened_at = std::time::Instant::now()
                                .checked_sub(o.cooldown)
                                .unwrap_or_else(std::time::Instant::now);
                        }
                        entry.probe_in_flight = true;
                        true
                    } else {
                        false
                    }
                } else {
                    // 冷却已满且无试探 → 抢下试探权放行
                    entry.probe_in_flight = true;
                    true
                }
            }
        }
    }

    /// 记录成功（含上游返回 4xx——说明上游可达）：复位为 Closed。
    pub fn record_success(&self, model_id: &str) {
        let mut map = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = map.entry(model_id.to_string()).or_default();
        let was_probe = entry.probe_in_flight;
        entry.consecutive_failures = 0;
        entry.open = None;
        entry.probe_in_flight = false;
        if was_probe {
            tracing::info!(model_id = %model_id, "LLM breaker half-open probe succeeded, closed");
        }
    }

    /// 记录失败（连接错误 / 超时 / 5xx / 429）。
    ///
    /// Closed 下累加，达 `FAILURE_THRESHOLD` 转 Open（冷却 `BASE_COOLDOWN_SECS`）。
    /// 半开试探失败：冷却翻倍（上限 `MAX_COOLDOWN_SECS`）重新 Open。
    pub fn record_failure(&self, model_id: &str) {
        let mut map = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = map.entry(model_id.to_string()).or_default();

        if entry.probe_in_flight {
            // 半开试探失败：冷却翻倍重新打开
            let prev = entry
                .open
                .map_or(Duration::from_secs(BASE_COOLDOWN_SECS), |o| o.cooldown);
            let next = (prev * 2).min(Duration::from_secs(MAX_COOLDOWN_SECS));
            entry.open = Some(OpenState {
                opened_at: Instant::now(),
                cooldown: next,
            });
            entry.probe_in_flight = false;
            tracing::warn!(
                model_id = %model_id,
                cooldown_secs = next.as_secs(),
                "LLM breaker half-open probe failed, reopened with doubled cooldown"
            );
            return;
        }

        entry.consecutive_failures += 1;
        if entry.consecutive_failures >= FAILURE_THRESHOLD && entry.open.is_none() {
            let cooldown = Duration::from_secs(BASE_COOLDOWN_SECS);
            entry.open = Some(OpenState {
                opened_at: Instant::now(),
                cooldown,
            });
            tracing::warn!(
                model_id = %model_id,
                consecutive_failures = entry.consecutive_failures,
                cooldown_secs = BASE_COOLDOWN_SECS,
                "LLM breaker opened"
            );
        }
    }

    /// 手动重置单个模型。
    pub fn reset(&self, model_id: &str) {
        let mut map = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = map.get_mut(model_id) {
            entry.consecutive_failures = 0;
            entry.open = None;
            entry.probe_in_flight = false;
        }
    }

    /// 批量重置（组级 reset-breaker API 用）。
    pub fn reset_many(&self, model_ids: &[String]) {
        for id in model_ids {
            self.reset(id);
        }
    }

    /// 快照（组详情 API）。
    #[must_use]
    pub fn snapshot(&self, model_id: &str) -> BreakerSnapshot {
        let map = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(entry) = map.get(model_id) else {
            return BreakerSnapshot {
                state: BreakerStateView::Closed,
                consecutive_failures: 0,
                cooldown_remaining_secs: 0,
            };
        };
        match entry.open {
            None => BreakerSnapshot {
                state: BreakerStateView::Closed,
                consecutive_failures: entry.consecutive_failures,
                cooldown_remaining_secs: 0,
            },
            Some(open) => {
                let remaining = open
                    .cooldown
                    .checked_sub(open.opened_at.elapsed())
                    .unwrap_or(Duration::ZERO);
                let state = if entry.probe_in_flight {
                    BreakerStateView::HalfOpenProbe
                } else {
                    BreakerStateView::Open
                };
                BreakerSnapshot {
                    state,
                    consecutive_failures: entry.consecutive_failures,
                    cooldown_remaining_secs: remaining.as_secs(),
                }
            }
        }
    }

    /// 测试辅助：强制冷却期满（把 opened_at 拨回到过去）。
    #[cfg(test)]
    fn force_cooldown_elapsed_for_test(&self, model_id: &str) {
        let mut map = self.inner.lock().expect("breaker mutex poisoned");
        if let Some(entry) = map.get_mut(model_id) {
            if let Some(open) = entry.open.as_mut() {
                open.opened_at = Instant::now()
                    .checked_sub(open.cooldown)
                    .unwrap()
                    .checked_sub(Duration::from_secs(1))
                    .unwrap();
            }
            entry.probe_in_flight = false;
        }
    }

    /// 测试辅助：把 opened_at 拨到冷却 + 上游读超时（300s）之后，模拟"试探请求被
    /// 客户端断开、record_* 未执行"导致的陈旧试探（probe_in_flight 保持原值）。
    #[cfg(test)]
    fn force_probe_stale_for_test(&self, model_id: &str) {
        let mut map = self.inner.lock().expect("breaker mutex poisoned");
        if let Some(entry) = map.get_mut(model_id) {
            if let Some(open) = entry.open.as_mut() {
                open.opened_at = Instant::now()
                    .checked_sub(open.cooldown)
                    .unwrap()
                    .checked_sub(Duration::from_secs(301))
                    .unwrap();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_allows_and_counts_failures() {
        let b = ModelBreakers::new();
        assert!(b.allow("m1"));
        for _ in 0..4 {
            b.record_failure("m1");
            assert!(b.allow("m1"), "未达阈值应放行");
        }
        b.record_failure("m1"); // 第 5 次
        assert!(!b.allow("m1"), "达阈值应熔断");
        let snap = b.snapshot("m1");
        assert_eq!(snap.state, BreakerStateView::Open);
        assert_eq!(snap.consecutive_failures, 5);
        assert!(snap.cooldown_remaining_secs > 0);
    }

    #[test]
    fn success_resets() {
        let b = ModelBreakers::new();
        for _ in 0..3 {
            b.record_failure("m1");
        }
        b.record_success("m1");
        assert_eq!(b.snapshot("m1").consecutive_failures, 0);
        // 再失败 5 次才熔断（验证已复位）
        for _ in 0..4 {
            b.record_failure("m1");
        }
        assert!(b.allow("m1"));
    }

    #[test]
    fn half_open_probe_single_flight() {
        let b = ModelBreakers::new();
        for _ in 0..FAILURE_THRESHOLD {
            b.record_failure("m1");
        }
        assert!(!b.allow("m1"));
        // 手动把冷却期缩短到 0 来模拟冷却期满——通过 reset + 重新打开并等候不可行，
        // 改用测试专用构造：直接操纵 entry（提供 #[cfg(test)] 辅助函数 force_cooldown_elapsed）。
        b.force_cooldown_elapsed_for_test("m1");
        // 第一个请求抢到试探权
        assert!(b.allow("m1"));
        // 并发第二个请求抢不到
        assert!(!b.allow("m1"));
        // 试探成功 → 完全恢复
        b.record_success("m1");
        assert!(b.allow("m1"));
        assert_eq!(b.snapshot("m1").state, BreakerStateView::Closed);
    }

    #[test]
    fn half_open_failure_doubles_cooldown() {
        let b = ModelBreakers::new();
        for _ in 0..FAILURE_THRESHOLD {
            b.record_failure("m1");
        }
        b.force_cooldown_elapsed_for_test("m1");
        assert!(b.allow("m1")); // 抢到试探
        b.record_failure("m1"); // 试探失败
        let snap = b.snapshot("m1");
        assert_eq!(snap.state, BreakerStateView::Open);
        // 冷却翻倍（30→60），此时剩余应在 (30, 60] 之间
        assert!(snap.cooldown_remaining_secs > BASE_COOLDOWN_SECS);
        assert!(snap.cooldown_remaining_secs <= BASE_COOLDOWN_SECS * 2);
    }

    #[test]
    fn stale_probe_is_reclaimed_after_timeout() {
        let b = ModelBreakers::new();
        for _ in 0..FAILURE_THRESHOLD {
            b.record_failure("m1");
        }
        b.force_cooldown_elapsed_for_test("m1");
        assert!(b.allow("m1")); // 抢到试探权，probe_in_flight=true
        assert!(!b.allow("m1")); // 单飞：并发请求被拒
                                 // 不 record_* —— 模拟试探请求被客户端断开（record_success/failure 均不执行）
                                 // 把 opened_at 拨到冷却 + 301s 之后（超过冷却 + 上游读超时 300s）→ 陈旧
        b.force_probe_stale_for_test("m1");
        assert!(b.allow("m1"), "陈旧试探应被回收，允许重新夺取");
        // 回收后仍保持单飞（重新夺取的试探在飞）
        assert!(!b.allow("m1"));
        // 试探成功 → 正常恢复
        b.record_success("m1");
        assert_eq!(b.snapshot("m1").state, BreakerStateView::Closed);
        assert!(b.allow("m1"));
    }

    #[test]
    fn fresh_probe_not_reclaimed_during_window() {
        // 试探在飞但未超过"冷却 + 300s"窗口：不允许重新夺取
        let b = ModelBreakers::new();
        for _ in 0..FAILURE_THRESHOLD {
            b.record_failure("m1");
        }
        b.force_cooldown_elapsed_for_test("m1");
        assert!(b.allow("m1")); // 抢到试探
        assert!(!b.allow("m1"));
        // 只拨过冷却期一小段（未到 +300s 陈旧窗口）
        {
            let mut map = b.inner.lock().unwrap();
            let entry = map.get_mut("m1").unwrap();
            let open = entry.open.as_mut().unwrap();
            open.opened_at = std::time::Instant::now()
                .checked_sub(open.cooldown)
                .unwrap()
                .checked_sub(std::time::Duration::from_secs(1))
                .unwrap();
        }
        assert!(!b.allow("m1"), "窗口内的在飞试探不应被回收");
    }

    #[test]
    fn cooldown_caps_at_max() {
        let b = ModelBreakers::new();
        for _ in 0..FAILURE_THRESHOLD {
            b.record_failure("m1");
        }
        // 连续多次半开失败，冷却指数增长但不超过 MAX
        for _ in 0..10 {
            b.force_cooldown_elapsed_for_test("m1");
            assert!(b.allow("m1"));
            b.record_failure("m1");
        }
        let snap = b.snapshot("m1");
        assert!(snap.cooldown_remaining_secs <= MAX_COOLDOWN_SECS);
    }

    #[test]
    fn reset_clears_state() {
        let b = ModelBreakers::new();
        for _ in 0..FAILURE_THRESHOLD {
            b.record_failure("m1");
        }
        assert!(!b.allow("m1"));
        b.reset("m1");
        assert!(b.allow("m1"));
        assert_eq!(b.snapshot("m1").state, BreakerStateView::Closed);
        assert_eq!(b.snapshot("m1").consecutive_failures, 0);
    }

    #[test]
    fn unknown_model_snapshot_is_closed() {
        let b = ModelBreakers::new();
        let snap = b.snapshot("never-seen");
        assert_eq!(snap.state, BreakerStateView::Closed);
        assert_eq!(snap.consecutive_failures, 0);
    }
}
