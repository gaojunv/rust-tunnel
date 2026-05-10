use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::Mutex;
use crate::server::db::Database;

/// Connection quality alert thresholds
#[derive(Debug, Clone)]
pub struct QualityThresholds {
    pub warning_rtt_ms: f32,
    pub critical_rtt_ms: f32,
    pub warning_loss_rate: f32,
    pub critical_loss_rate: f32,
}

impl Default for QualityThresholds {
    fn default() -> Self {
        Self {
            warning_rtt_ms: 200.0,
            critical_rtt_ms: 500.0,
            warning_loss_rate: 0.05,
            critical_loss_rate: 0.15,
        }
    }
}

/// Real-time connection quality data for a client
#[derive(Debug, Clone, Serialize)]
pub struct ConnectionQuality {
    // RTT data (milliseconds)
    pub last_rtt_ms: f32,
    pub avg_rtt_ms: f32,
    pub min_rtt_ms: f32,
    pub max_rtt_ms: f32,

    // Packet loss data
    pub loss_rate: f32,          // 0.0 - 1.0
    pub consecutive_losses: u32, // Consecutive lost count

    // Throughput (bytes per second)
    pub bytes_in_per_sec: f64,
    pub bytes_out_per_sec: f64,

    // Quality score (0-100)
    pub quality_score: u8,

    // Status
    pub last_update: DateTime<Utc>,
    pub is_warning: bool,
    pub is_critical: bool,
}

impl Default for ConnectionQuality {
    fn default() -> Self {
        Self {
            last_rtt_ms: 0.0,
            avg_rtt_ms: 0.0,
            min_rtt_ms: f32::MAX,
            max_rtt_ms: 0.0,
            loss_rate: 0.0,
            consecutive_losses: 0,
            bytes_in_per_sec: 0.0,
            bytes_out_per_sec: 0.0,
            quality_score: 100,
            last_update: Utc::now(),
            is_warning: false,
            is_critical: false,
        }
    }
}

/// Historical quality sample (one per minute)
#[derive(Debug, Clone, Serialize)]
pub struct QualitySample {
    pub timestamp: DateTime<Utc>,
    pub avg_rtt_ms: f32,
    pub loss_rate: f32,
    pub bytes_in_per_sec: f64,
    pub bytes_out_per_sec: f64,
    pub quality_score: u8,
}

/// Quality tracker for RTT and loss calculation (per connection)
#[derive(Debug, Clone)]
pub struct QualityTracker {
    expected_seq: u32,
    rtt_samples: VecDeque<f32>,
    total_pings: u64,
    lost_pings: u64,
    recent_losses: VecDeque<bool>,
    pub last_sample_minute: u32,
}

impl Default for QualityTracker {
    fn default() -> Self {
        Self {
            expected_seq: 1,
            rtt_samples: VecDeque::with_capacity(20),
            total_pings: 0,
            lost_pings: 0,
            recent_losses: VecDeque::with_capacity(60),
            last_sample_minute: u32::MAX,
        }
    }
}

impl QualityTracker {
    /// Record a received Ping, calculate packet loss
    pub fn record_ping(&mut self, seq: u32) -> (u32, f32) {
        self.total_pings += 1;


        // Handle sequence number reset (client likely restarted)
        // If seq is significantly behind expected, reset the tracker to avoid
        // incorrectly calculating packet loss
        if seq < self.expected_seq && (seq <= 2 || self.expected_seq - seq > 5) {
            self.expected_seq = seq;
            self.recent_losses.clear();
        }

        // Calculate lost packets
        let lost = if seq > self.expected_seq {
            seq - self.expected_seq
        } else {
            0
        };

        if lost > 0 {
            self.lost_pings += lost as u64;
            for _ in 0..lost {
                self.recent_losses.push_back(true);
            }
        }

        // Record current packet as received
        self.recent_losses.push_back(false);

        // Keep only last 60 samples
        while self.recent_losses.len() > 60 {
            self.recent_losses.pop_front();
        }

        // Update next expected sequence number
        self.expected_seq = seq + 1;

        // Calculate recent loss rate
        let recent_loss_count = self.recent_losses.iter().filter(|&&l| l).count();
        let loss_rate = if self.recent_losses.is_empty() {
            0.0
        } else {
            recent_loss_count as f32 / self.recent_losses.len() as f32
        };

        (lost, loss_rate)
    }

    /// Record an RTT sample
    pub fn record_rtt(&mut self, rtt_ms: f32) {
        self.rtt_samples.push_back(rtt_ms);
        while self.rtt_samples.len() > 20 {
            self.rtt_samples.pop_front();
        }
    }

    /// Get average RTT
    pub fn get_avg_rtt(&self) -> f32 {
        if self.rtt_samples.is_empty() {
            return 0.0;
        }
        self.rtt_samples.iter().sum::<f32>() / self.rtt_samples.len() as f32
    }

    /// Get minimum RTT
    pub fn get_min_rtt(&self) -> f32 {
        self.rtt_samples.iter().cloned().fold(f32::MAX, f32::min)
    }

    /// Get maximum RTT
    pub fn get_max_rtt(&self) -> f32 {
        self.rtt_samples.iter().cloned().fold(0.0, f32::max)
    }
}

/// Calculate quality score (0-100)
pub fn calculate_quality_score(avg_rtt_ms: f32, loss_rate: f32) -> u8 {
    let latency_penalty = (avg_rtt_ms / 500.0 * 30.0).min(30.0);
    let loss_penalty = (loss_rate * 70.0).min(70.0);
    let score = 100.0 - latency_penalty - loss_penalty;
    score.max(0.0).min(100.0).round() as u8
}

/// Check if quality thresholds
pub fn check_warnings(
    avg_rtt_ms: f32,
    loss_rate: f32,
    thresholds: &QualityThresholds,
) -> (bool, bool) {
    let is_critical = avg_rtt_ms >= thresholds.critical_rtt_ms || loss_rate >= thresholds.critical_loss_rate;
    let is_warning = !is_critical && (avg_rtt_ms >= thresholds.warning_rtt_ms || loss_rate >= thresholds.warning_loss_rate);
    (is_warning, is_critical)
}

/// In-memory quality store with database persistence
#[derive(Clone)]
pub struct QualityStore {
    current: Arc<Mutex<HashMap<u16, ConnectionQuality>>>,
    samples: Arc<Mutex<HashMap<u16, VecDeque<QualitySample>>>>,
    db: Option<Database>,
}

impl Default for QualityStore {
    fn default() -> Self {
        Self::new()
    }
}

impl QualityStore {
    /// Create a new quality store without database (for backwards compatibility)
    pub fn new() -> Self {
        Self {
            current: Arc::new(Mutex::new(HashMap::new())),
            samples: Arc::new(Mutex::new(HashMap::new())),
            db: None,
        }
    }

    /// Create a new quality store with database persistence
    pub fn with_db(db: Database) -> Self {
        Self {
            current: Arc::new(Mutex::new(HashMap::new())),
            samples: Arc::new(Mutex::new(HashMap::new())),
            db: Some(db),
        }
    }

    /// Load quality history data from database (last 24 hours)
    pub async fn load_from_db(&self) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(db) = &self.db {
            let mut samples = self.samples.lock().await;

            // Get all distinct ports from quality history (last 24 hours)
            let ports = db.get_quality_ports(24).await?;

            // For each port, load last 60 minutes of history
            for port in ports {
                let history = db.get_quality_history(
                    port,
                    Utc::now() - Duration::hours(24),
                    Utc::now(),
                ).await?;

                let mut port_samples = VecDeque::with_capacity(60);
                for sample in history {
                    port_samples.push_back(sample);
                    if port_samples.len() >= 60 {
                        break;
                    }
                }

                samples.insert(port, port_samples);
            }
        }
        Ok(())
    }

    pub async fn update_quality(&self, port: u16, quality: ConnectionQuality) {
        let mut current = self.current.lock().await;
        current.insert(port, quality);
    }

    pub async fn add_sample(&self, port: u16, sample: QualitySample) {
        // Add to in-memory store
        let mut samples = self.samples.lock().await;
        let port_samples = samples.entry(port).or_insert_with(|| VecDeque::with_capacity(60));
        port_samples.push_back(sample.clone());

        while port_samples.len() > 60 {
            port_samples.pop_front();
        }
        drop(samples);

        // Persist to database if available
        if let Some(db) = &self.db {
            let _ = db.insert_quality_history(
                port,
                sample.timestamp,
                sample.avg_rtt_ms,
                sample.avg_rtt_ms, // min_rtt_ms (use same as avg for history)
                sample.avg_rtt_ms, // max_rtt_ms (use same as avg for history)
                sample.loss_rate,
                sample.bytes_in_per_sec,
                sample.bytes_out_per_sec,
                sample.quality_score,
                false, // is_warning
                false, // is_critical
            ).await;
        }
    }

    pub async fn get_quality(&self, port: u16) -> Option<ConnectionQuality> {
        let current = self.current.lock().await;
        current.get(&port).cloned()
    }

    pub async fn get_all_quality(&self) -> Vec<(u16, ConnectionQuality)> {
        let current = self.current.lock().await;
        current.iter().map(|(k, v)| (*k, v.clone())).collect()
    }

    pub async fn get_samples(&self, port: u16) -> Vec<QualitySample> {
        let samples = self.samples.lock().await;
        samples.get(&port).map(|s| s.iter().cloned().collect()).unwrap_or_default()
    }

    pub async fn remove_port(&self, port: u16) {
        let mut current = self.current.lock().await;
        let mut samples = self.samples.lock().await;
        current.remove(&port);
        samples.remove(&port);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_quality_score_calculation() {
        assert_eq!(calculate_quality_score(0.0, 0.0), 100);
        assert_eq!(calculate_quality_score(250.0, 0.0), 85);
        assert_eq!(calculate_quality_score(500.0, 0.0), 70);
        assert_eq!(calculate_quality_score(1000.0, 0.0), 70);
        assert_eq!(calculate_quality_score(0.0, 0.1), 93);
        assert_eq!(calculate_quality_score(0.0, 0.5), 65);
        assert_eq!(calculate_quality_score(0.0, 1.0), 30);
        assert_eq!(calculate_quality_score(500.0, 1.0), 0);
    }

    #[test]
    fn test_warning_checks() {
        let thresholds = QualityThresholds::default();
        assert_eq!(check_warnings(100.0, 0.01, &thresholds), (false, false));
        assert_eq!(check_warnings(250.0, 0.01, &thresholds), (true, false));
        assert_eq!(check_warnings(600.0, 0.01, &thresholds), (false, true));
        assert_eq!(check_warnings(100.0, 0.1, &thresholds), (true, false));
        assert_eq!(check_warnings(100.0, 0.2, &thresholds), (false, true));
        assert_eq!(check_warnings(600.0, 0.2, &thresholds), (false, true));
    }

    #[test]
    fn test_quality_tracker() {
        let mut tracker = QualityTracker::default();
        tracker.record_ping(1);
        tracker.record_ping(2);
        tracker.record_ping(3);
        let (lost, _) = tracker.record_ping(6);
        assert_eq!(lost, 2);
        tracker.record_rtt(50.0);
        tracker.record_rtt(60.0);
        tracker.record_rtt(70.0);
        assert_eq!(tracker.get_avg_rtt(), 60.0);
    }

    #[test]
    fn test_quality_tracker_seq_reset() {
        let mut tracker = QualityTracker::default();
        // Normal sequence
        tracker.record_ping(1);
        tracker.record_ping(2);
        tracker.record_ping(3);

        // Client restarted, sequence jumps back to 1
        let (lost, loss_rate) = tracker.record_ping(1);
        assert_eq!(lost, 0); // Should not count as loss when restart detected
        assert_eq!(loss_rate, 0.0); // Should be 0% after reset (only 1 packet received)

        // Continue with normal sequence after reset
        let (lost, loss_rate) = tracker.record_ping(2);
        assert_eq!(lost, 0);
        assert_eq!(loss_rate, 0.0); // Still 0% with 2 packets
    }

    #[tokio::test]
    async fn test_quality_store() {
        let store = QualityStore::new();
        let quality = ConnectionQuality::default();
        store.update_quality(8080, quality).await;
        let result = store.get_quality(8080).await;
        assert!(result.is_some());
        let all = store.get_all_quality().await;
        assert_eq!(all.len(), 1);
        store.remove_port(8080).await;
        let result = store.get_quality(8080).await;
        assert!(result.is_none());
    }
}
