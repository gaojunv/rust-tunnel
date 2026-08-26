//! 确定性失败缓存：候选因 401/403/404 等“必然失败”被临时标记，TTL 内跳过网络调用。
//!
//! 与 [`crate::breaker::ModelBreakers`] 的分工：
//! - 熔断器处理「服务端/网络」级别的故障（5xx/超时/连接失败），按失败次数门控；
//! - 本模块处理「配置/请求确定性错误」——401/403 说明该 provider 的 key 失效或
//!   无权限，404 说明模型不存在，这些错误换多少次重试都一样，反复命中只会白烧
//!   网络往返（并让"失效 key 的首选候选"每次都阻塞整条候选链）。
//!
//! 在 TTL 内跳过已确认死掉的候选：单模型链直接秒回缓存错误，模型组则跳过死候选
//! 继续尝试健康备选。TTL 到期自动进入探测（下次请求重新发起真实调用，恢复则清除，
//! 仍失败则重新记录）。配置变更（换 key / 改模型）由管理面调用 [`crate::down::KnownFailures::clear_all`]
//! 立即解除标记，不等 TTL。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 确定性失败条目的存活时间。到期后自动重新探测。
pub const KNOWN_FAILURE_TTL: Duration = Duration::from_mins(5);

/// 失败类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// 认证/权限失败（provider 级：key 失效/被禁，该 provider 全部模型同因失败）。
    ProviderAuth,
    /// 模型级失败（如上游 404 model not found）。
    Model,
}

/// 一次查询可见的失败信息（TTL 有效期内）。
#[derive(Debug, Clone)]
pub struct KnownFailureInfo {
    /// 失败类别。
    pub kind: FailureKind,
    /// HTTP 状态码（如 401/403/404）。
    pub status: u16,
    /// 上游错误消息（已脱敏截断）。
    pub message: String,
}

#[derive(Debug, Clone)]
struct Entry {
    info: KnownFailureInfo,
    recorded_at: Instant,
}

/// 已知失败集合。`Clone` 共享内部状态。
#[derive(Debug, Clone, Default)]
pub struct KnownFailures {
    inner: Arc<Mutex<HashMap<String, Entry>>>,
}

impl KnownFailures {
    /// 新建空集合。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录一次确定性失败。`key` 由调用方约定：
    /// - provider 级（认证失败）：`p:<provider_id>`
    /// - 模型级（404 等）：`m:<model_id>`
    pub fn record(&self, key: &str, kind: FailureKind, status: u16, message: &str) {
        let mut map = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        map.insert(
            key.to_string(),
            Entry {
                info: KnownFailureInfo {
                    kind,
                    status,
                    message: message.to_string(),
                },
                recorded_at: Instant::now(),
            },
        );
    }

    /// 查询该键对应的失败信息；超过 TTL 视为已恢复并清除（惰性过期）。
    #[must_use]
    pub fn lookup(&self, key: &str) -> Option<KnownFailureInfo> {
        let mut map = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = map.get(key)?;
        if entry.recorded_at.elapsed() >= KNOWN_FAILURE_TTL {
            map.remove(key);
            return None;
        }
        Some(entry.info.clone())
    }

    /// 清除单条（如某模型被编辑后立即恢复探测）。
    pub fn remove(&self, key: &str) {
        let mut map = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        map.remove(key);
    }

    /// 全量清除：配置变更（provider/model/group CRUD、手动重置）后调用，
    /// 让新配置立即可见、无需等 TTL。
    pub fn clear_all(&self) {
        let mut map = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        map.clear();
    }
}

impl Default for KnownFailureInfo {
    fn default() -> Self {
        Self {
            kind: FailureKind::Model,
            status: 0,
            message: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_lookup_roundtrip() {
        let kf = KnownFailures::new();
        assert!(kf.lookup("p:p1").is_none());

        kf.record("p:p1", FailureKind::ProviderAuth, 401, "invalid key");
        let info = kf.lookup("p:p1").expect("recorded failure visible");
        assert_eq!(info.kind, FailureKind::ProviderAuth);
        assert_eq!(info.status, 401);
        assert_eq!(info.message, "invalid key");

        // 不同键互不影响
        assert!(kf.lookup("m:m1").is_none());
    }

    #[test]
    fn clear_and_remove() {
        let kf = KnownFailures::new();
        kf.record("p:p1", FailureKind::ProviderAuth, 401, "x");
        kf.record("m:m1", FailureKind::Model, 404, "y");

        kf.remove("m:m1");
        assert!(kf.lookup("m:m1").is_none());
        assert!(kf.lookup("p:p1").is_some());

        kf.clear_all();
        assert!(kf.lookup("p:p1").is_none());
    }

    #[test]
    fn expiry_returns_none_and_cleans_up() {
        let kf = KnownFailures::new();
        kf.record("p:p1", FailureKind::ProviderAuth, 401, "x");
        // 拨快时钟：直接操纵内部 entry 模拟 TTL 到期
        {
            let mut map = kf.inner.lock().unwrap();
            let entry = map.get_mut("p:p1").unwrap();
            entry.recorded_at =
                Instant::now().checked_sub(KNOWN_FAILURE_TTL).unwrap().checked_sub(Duration::from_secs(1)).unwrap();
        }
        assert!(kf.lookup("p:p1").is_none());
        // 惰性过期已清理
        assert!(kf.inner.lock().unwrap().is_empty());
    }
}
