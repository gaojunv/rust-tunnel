//! Unified statistics collector for all connection types.
//!
//! Tracks bytes in/out, rate (sliding window), RTT/loss, and
//! active connection counts per entity. Flushes 1-minute snapshots to the
//! database and broadcasts them via SSE every minute.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Instant;

use chrono::{DateTime, Timelike, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::server::db::Database;

// ── Entity type ──────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityType {
    Client,
    Proxy,
    Shadowsocks,
    Trojan,
}

impl EntityType {
    pub fn as_str(&self) -> &'static str {
        match self {
            EntityType::Client => "client",
            EntityType::Proxy => "proxy",
            EntityType::Shadowsocks => "shadowsocks",
            EntityType::Trojan => "trojan",
        }
    }
}

impl std::fmt::Display for EntityType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

// ── Snapshot (memory / DB / API wire format) ─────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatsSnapshot {
    pub entity_type: String,
    pub entity_id: String,
    pub timestamp: DateTime<Utc>,
    pub bytes_in: i64,
    pub bytes_out: i64,
    pub bytes_in_rate: f64,
    pub bytes_out_rate: f64,
    pub rtt_ms: Option<f64>,
    pub loss_pct: Option<f64>,
    pub active_conns: i32,
}

// ── In-memory accumulator per entity ─────────────────────────────

struct EntityStats {
    bytes_in: u64,
    bytes_out: u64,
    /// Sliding window for rate calculation: (instant, cumulative bytes_in, cumulative bytes_out)
    rate_window: VecDeque<(Instant, u64, u64)>,
    bytes_in_rate: f64,
    bytes_out_rate: f64,
    /// Recent RTT samples in ms
    rtt_samples: VecDeque<f64>,
    active_conns: u64,
}

impl EntityStats {
    fn new() -> Self {
        Self {
            bytes_in: 0,
            bytes_out: 0,
            rate_window: VecDeque::with_capacity(32),
            bytes_in_rate: 0.0,
            bytes_out_rate: 0.0,
            rtt_samples: VecDeque::with_capacity(60),
            active_conns: 0,
        }
    }

    fn record_bytes(&mut self, bytes_in: u64, bytes_out: u64) {
        self.bytes_in += bytes_in;
        self.bytes_out += bytes_out;
        self.rate_window
            .push_back((Instant::now(), self.bytes_in, self.bytes_out));
        let cutoff = Instant::now() - std::time::Duration::from_secs(60);
        while self
            .rate_window
            .front()
            .is_some_and(|(t, _, _)| *t < cutoff)
        {
            self.rate_window.pop_front();
        }
    }

    fn recalc_rate(&mut self) {
        if let (Some(first), Some(last)) = (self.rate_window.front(), self.rate_window.back()) {
            let dt = (last.0 - first.0).as_secs_f64();
            if dt > 0.0 {
                self.bytes_in_rate = (last.1.saturating_sub(first.1)) as f64 / dt;
                self.bytes_out_rate = (last.2.saturating_sub(first.2)) as f64 / dt;
            }
        }
    }

    fn push_rtt(&mut self, rtt_ms: f64) {
        self.rtt_samples.push_back(rtt_ms);
        if self.rtt_samples.len() > 60 {
            self.rtt_samples.pop_front();
        }
    }

    fn median_rtt(&self) -> Option<f64> {
        if self.rtt_samples.is_empty() {
            return None;
        }
        let mut sorted: Vec<f64> = self.rtt_samples.iter().copied().collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        Some(sorted[sorted.len() / 2])
    }

    fn snapshot(
        &self,
        entity_type: EntityType,
        entity_id: &str,
        ts: DateTime<Utc>,
    ) -> StatsSnapshot {
        StatsSnapshot {
            entity_type: entity_type.as_str().to_string(),
            entity_id: entity_id.to_string(),
            timestamp: ts,
            bytes_in: self.bytes_in as i64,
            bytes_out: self.bytes_out as i64,
            bytes_in_rate: self.bytes_in_rate,
            bytes_out_rate: self.bytes_out_rate,
            rtt_ms: self.median_rtt(),
            loss_pct: None,
            active_conns: self.active_conns as i32,
        }
    }
}

// ── StatsCollector ────────────────────────────────────────────────

/// Thread-safe, cloneable handle to the unified stats collector.
#[derive(Clone)]
pub struct StatsCollector {
    inner: Arc<StdMutex<HashMap<(EntityType, String), EntityStats>>>,
    db: Option<Database>,
    tx: broadcast::Sender<StatsSnapshot>,
}

impl StatsCollector {
    /// Create a new collector. `db` may be None for in-memory-only mode.
    pub fn new(db: Option<Database>) -> Self {
        let (tx, _) = broadcast::channel(256);
        Self {
            inner: Arc::new(StdMutex::new(HashMap::new())),
            db,
            tx,
        }
    }

    /// Get a receiver for SSE streaming.
    pub fn subscribe(&self) -> broadcast::Receiver<StatsSnapshot> {
        self.tx.subscribe()
    }

    // ── Recording API (called from connection handlers) ───────────

    /// Record traffic bytes for an entity.
    pub fn record_bytes(
        &self,
        entity_type: EntityType,
        entity_id: &str,
        bytes_in: u64,
        bytes_out: u64,
    ) {
        if bytes_in == 0 && bytes_out == 0 {
            return;
        }
        let key = (entity_type, entity_id.to_string());
        let mut map = self.inner.lock().unwrap();
        let entry = map.entry(key).or_insert_with(EntityStats::new);
        entry.record_bytes(bytes_in, bytes_out);
    }

    /// Record an RTT sample (from client heartbeat or TCP_INFO).
    pub fn record_rtt(&self, entity_type: EntityType, entity_id: &str, rtt_ms: f64) {
        let key = (entity_type, entity_id.to_string());
        let mut map = self.inner.lock().unwrap();
        map.entry(key)
            .or_insert_with(EntityStats::new)
            .push_rtt(rtt_ms);
    }

    /// Increment active connection count.
    pub fn incr_conns(&self, entity_type: EntityType, entity_id: &str) {
        let key = (entity_type, entity_id.to_string());
        let mut map = self.inner.lock().unwrap();
        map.entry(key).or_insert_with(EntityStats::new).active_conns += 1;
    }

    /// Decrement active connection count.
    pub fn decr_conns(&self, entity_type: EntityType, entity_id: &str) {
        let key = (entity_type, entity_id.to_string());
        let mut map = self.inner.lock().unwrap();
        if let Some(e) = map.get_mut(&key) {
            if e.active_conns > 0 {
                e.active_conns -= 1;
            }
        }
    }

    // ── Tick: recalc rates (every 5 seconds) ──────────────────────

    pub fn tick_rates(&self) {
        let mut map = self.inner.lock().unwrap();
        for stats in map.values_mut() {
            stats.recalc_rate();
        }
    }

    // ── Flush: write to DB + broadcast (every 60 seconds) ─────────

    pub async fn flush(&self) {
        let now = Utc::now();
        let snapshot_time = now
            .with_second(0)
            .and_then(|dt| dt.with_nanosecond(0))
            .unwrap_or(now);

        let snapshots: Vec<StatsSnapshot> = {
            let map = self.inner.lock().unwrap();
            map.iter()
                .map(|((et, eid), stats)| stats.snapshot(*et, eid, snapshot_time))
                .collect()
        };

        if let Some(ref db) = self.db {
            for snap in &snapshots {
                if let Err(e) = sqlx::query(
                    r#"INSERT OR REPLACE INTO stats_snapshots
                       (entity_type, entity_id, timestamp, bytes_in, bytes_out,
                        bytes_in_rate, bytes_out_rate, rtt_ms, loss_pct, active_conns)
                       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
                )
                .bind(&snap.entity_type)
                .bind(&snap.entity_id)
                .bind(snap.timestamp)
                .bind(snap.bytes_in)
                .bind(snap.bytes_out)
                .bind(snap.bytes_in_rate)
                .bind(snap.bytes_out_rate)
                .bind(snap.rtt_ms)
                .bind(snap.loss_pct)
                .bind(snap.active_conns)
                .execute(&db.pool)
                .await
                {
                    tracing::warn!(
                        "Failed to flush stats snapshot for {}/{}: {}",
                        snap.entity_type,
                        snap.entity_id,
                        e
                    );
                }
            }
        }

        for snap in snapshots {
            let _ = self.tx.send(snap);
        }
    }

    /// Build a summary of current stats (in-memory only, no DB query).
    pub fn get_summary(&self) -> StatsSummary {
        let map = self.inner.lock().unwrap();
        let mut summary = StatsSummary::default();

        for ((entity_type, _entity_id), stats) in map.iter() {
            let bucket = match entity_type {
                EntityType::Client => &mut summary.clients,
                EntityType::Proxy => &mut summary.proxy,
                EntityType::Shadowsocks => &mut summary.shadowsocks,
                EntityType::Trojan => &mut summary.trojan,
            };
            bucket.total_bytes_in += stats.bytes_in;
            bucket.total_bytes_out += stats.bytes_out;
            bucket.total_conns += stats.active_conns;
            bucket.entity_count += 1;
        }

        summary
    }
}

// ── Summary response ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StatsSummary {
    pub clients: EntitySummary,
    pub proxy: EntitySummary,
    pub shadowsocks: EntitySummary,
    pub trojan: EntitySummary,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EntitySummary {
    pub total_bytes_in: u64,
    pub total_bytes_out: u64,
    pub total_conns: u64,
    pub entity_count: u64,
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_bytes_accumulates() {
        let c = StatsCollector::new(None);
        c.record_bytes(EntityType::Proxy, "r1", 100, 200);
        c.record_bytes(EntityType::Proxy, "r1", 50, 0);
        let summary = c.get_summary();
        assert_eq!(summary.proxy.total_bytes_in, 150);
        assert_eq!(summary.proxy.total_bytes_out, 200);
        assert_eq!(summary.proxy.entity_count, 1);
    }

    #[test]
    fn incr_decr_conns() {
        let c = StatsCollector::new(None);
        c.incr_conns(EntityType::Client, "home-nas");
        c.incr_conns(EntityType::Client, "home-nas");
        c.decr_conns(EntityType::Client, "home-nas");
        let summary = c.get_summary();
        assert_eq!(summary.clients.total_conns, 1);
    }

    #[test]
    fn rate_calculation() {
        let c = StatsCollector::new(None);
        {
            let mut map = c.inner.lock().unwrap();
            let key = (EntityType::Proxy, "r1".to_string());
            let entry = map.entry(key).or_insert_with(EntityStats::new);
            let past = Instant::now() - std::time::Duration::from_secs(10);
            entry.rate_window.clear();
            entry.bytes_out = 10000;
            entry.rate_window.push_back((past, 0, 0));
            entry.rate_window.push_back((Instant::now(), 0, 10000));
        }
        c.tick_rates();
        let map = c.inner.lock().unwrap();
        let key = (EntityType::Proxy, "r1".to_string());
        let stats = map.get(&key).unwrap();
        assert!(stats.bytes_out_rate > 0.0);
    }

    #[test]
    fn summary_per_type_separate() {
        let c = StatsCollector::new(None);
        c.record_bytes(EntityType::Client, "c1", 10, 20);
        c.record_bytes(EntityType::Proxy, "r1", 30, 40);
        c.incr_conns(EntityType::Shadowsocks, "ss:8388");

        let summary = c.get_summary();
        assert_eq!(summary.clients.entity_count, 1);
        assert_eq!(summary.proxy.entity_count, 1);
        assert_eq!(summary.shadowsocks.entity_count, 1);
        assert_eq!(summary.trojan.entity_count, 0);
        assert_eq!(summary.clients.total_bytes_in, 10);
        assert_eq!(summary.proxy.total_bytes_in, 30);
        assert_eq!(summary.shadowsocks.total_conns, 1);
    }

    #[test]
    fn median_rtt_correct() {
        let c = StatsCollector::new(None);
        c.record_rtt(EntityType::Client, "c1", 10.0);
        c.record_rtt(EntityType::Client, "c1", 20.0);
        c.record_rtt(EntityType::Client, "c1", 30.0);
        let map = c.inner.lock().unwrap();
        let key = (EntityType::Client, "c1".to_string());
        let stats = map.get(&key).unwrap();
        assert_eq!(stats.median_rtt(), Some(20.0));
    }

    #[test]
    fn snapshot_has_correct_entity_type() {
        let c = StatsCollector::new(None);
        c.record_bytes(EntityType::Proxy, "r1", 100, 200);
        let map = c.inner.lock().unwrap();
        let key = (EntityType::Proxy, "r1".to_string());
        let stats = map.get(&key).unwrap();
        let snap = stats.snapshot(EntityType::Proxy, "r1", Utc::now());
        assert_eq!(snap.entity_type, "proxy");
        assert_eq!(snap.entity_id, "r1");
        assert_eq!(snap.bytes_in, 100);
        assert_eq!(snap.bytes_out, 200);
    }

    #[test]
    fn decr_conns_does_not_underflow() {
        let c = StatsCollector::new(None);
        c.decr_conns(EntityType::Client, "ghost");
        let summary = c.get_summary();
        assert_eq!(summary.clients.total_conns, 0);
    }

    #[test]
    fn record_zero_bytes_ignored() {
        let c = StatsCollector::new(None);
        c.record_bytes(EntityType::Proxy, "r1", 0, 0);
        let summary = c.get_summary();
        assert_eq!(summary.proxy.entity_count, 0);
    }

    /// 同一 entity 同一分钟内 flush 两次：INSERT OR REPLACE 应替换旧行而不是
    /// 报主键冲突（服务重启后启动 flush 与重启前同一分钟的行撞主键的场景）。
    #[tokio::test]
    async fn flush_twice_same_minute_replaces_row() {
        let db = crate::server::db::Database::new(":memory:")
            .await
            .expect("in-memory db");
        let c = StatsCollector::new(Some(db.clone()));
        c.record_bytes(EntityType::Proxy, "r1", 100, 200);

        c.flush().await;
        c.record_bytes(EntityType::Proxy, "r1", 50, 50);
        c.flush().await;

        let (count, bytes_in): (i64, i64) = sqlx::query_as(
            "SELECT COUNT(*), COALESCE(SUM(bytes_in), 0) FROM stats_snapshots
             WHERE entity_type = 'proxy' AND entity_id = 'r1'",
        )
        .fetch_one(&db.pool)
        .await
        .expect("query count");
        assert_eq!(count, 1, "second flush must REPLACE, not conflict");
        assert_eq!(bytes_in, 150, "row should hold the latest flush values");
    }
}
