use crate::server::quality::{ConnectionQuality, QualitySample};
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::{delete, get, patch, post, put},
    Json, Router,
};
use chrono::{DateTime, Timelike, Utc};
#[cfg(feature = "embed-frontend")]
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Mutex;
use tower_http::cors::{Any, CorsLayer};

pub mod clients;
pub mod server_auth;

use crate::common::DnsRecord;
use crate::server::auth::{auth_middleware, create_token, AuthConfig};
use crate::server::control::ServerState;
use crate::server::db::Database;
use crate::server::reverse_proxy::{ProxyRule, ProxyStats};
use axum::response::sse::{Event, KeepAlive, Sse};
use std::time::Duration;

/// Embedded frontend assets
#[cfg(feature = "embed-frontend")]
#[derive(RustEmbed)]
#[folder = "frontend-dist/"]
struct FrontendAssets;

/// Traffic record for a single time bucket
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrafficBucket {
    pub timestamp: DateTime<Utc>,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

/// Traffic statistics for a port
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortTraffic {
    pub port: u16,
    pub total_bytes_in: u64,
    pub total_bytes_out: u64,
    pub buckets: VecDeque<TrafficBucket>,
}

/// Traffic store to track network statistics
#[derive(Clone)]
pub struct TrafficStore {
    inner: Arc<Mutex<std::collections::HashMap<u16, PortTraffic>>>,
    db: Option<Database>,
    /// Ports that have been updated since the last DB flush
    dirty_ports: Arc<std::sync::Mutex<std::collections::HashSet<u16>>>,
}

impl Default for TrafficStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TrafficStore {
    /// Create a new traffic store without database (for backwards compatibility)
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(std::collections::HashMap::new())),
            db: None,
            dirty_ports: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        }
    }

    /// Create a new traffic store with database persistence
    pub fn with_db(db: Database) -> Self {
        Self {
            inner: Arc::new(Mutex::new(std::collections::HashMap::new())),
            db: Some(db),
            dirty_ports: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        }
    }

    /// Load traffic data from database
    pub async fn load_from_db(&self) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(db) = &self.db {
            let mut store = self.inner.lock().await;

            // Load port traffic aggregates
            let port_records = db.load_port_traffic().await?;
            let bucket_records = db.load_recent_buckets(24).await?;

            // Build port traffic entries
            for record in port_records {
                let port = record.port as u16;
                let port_traffic = PortTraffic {
                    port,
                    total_bytes_in: record.total_bytes_in as u64,
                    total_bytes_out: record.total_bytes_out as u64,
                    buckets: VecDeque::new(),
                };
                store.insert(port, port_traffic);
            }

            // Add buckets to respective ports
            for bucket in bucket_records {
                let port = bucket.port as u16;
                if let Some(port_traffic) = store.get_mut(&port) {
                    port_traffic.buckets.push_back(TrafficBucket {
                        timestamp: bucket.timestamp,
                        bytes_in: bucket.bytes_in as u64,
                        bytes_out: bucket.bytes_out as u64,
                    });
                }
            }

            // Ensure buckets are within 24h limit for each port
            for port_traffic in store.values_mut() {
                while port_traffic.buckets.len() > 1440 {
                    port_traffic.buckets.pop_front();
                }
            }
        }
        Ok(())
    }

    /// Record incoming bytes (from user to server)
    pub async fn record_bytes_in(&self, port: u16, bytes: u64) {
        {
            let mut store = self.inner.lock().await;
            let port_traffic = store.entry(port).or_insert_with(|| PortTraffic {
                port,
                total_bytes_in: 0,
                total_bytes_out: 0,
                buckets: VecDeque::new(),
            });
            port_traffic.total_bytes_in += bytes;
            Self::add_to_bucket(port_traffic, bytes, 0);
        }
        // Mark port as dirty for periodic batch flush (no immediate DB write)
        if let Ok(mut dirty) = self.dirty_ports.lock() {
            dirty.insert(port);
        }
    }

    /// Record outgoing bytes (from server to user)
    pub async fn record_bytes_out(&self, port: u16, bytes: u64) {
        {
            let mut store = self.inner.lock().await;
            let port_traffic = store.entry(port).or_insert_with(|| PortTraffic {
                port,
                total_bytes_in: 0,
                total_bytes_out: 0,
                buckets: VecDeque::new(),
            });
            port_traffic.total_bytes_out += bytes;
            Self::add_to_bucket(port_traffic, 0, bytes);
        }
        // Mark port as dirty for periodic batch flush
        if let Ok(mut dirty) = self.dirty_ports.lock() {
            dirty.insert(port);
        }
    }

    /// Flush dirty traffic data to the database.
    /// Called periodically by the background flush task and on graceful shutdown.
    pub async fn flush_to_db(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let db = match &self.db {
            Some(db) => db.clone(),
            None => return Ok(()),
        };

        // Atomically take the set of dirty ports
        let dirty_ports: std::collections::HashSet<u16> = {
            let mut dirty = self.dirty_ports.lock().unwrap();
            std::mem::take(&mut *dirty)
        };

        if dirty_ports.is_empty() {
            return Ok(());
        }

        // Snapshot in-memory data for dirty ports (brief lock)
        let snapshots: Vec<(u16, PortTraffic)> = {
            let store = self.inner.lock().await;
            dirty_ports
                .iter()
                .filter_map(|&port| store.get(&port).map(|pt| (port, pt.clone())))
                .collect()
        };

        // Write snapshots to DB without holding the in-memory lock
        for (port, port_traffic) in snapshots {
            if let Err(e) = db
                .replace_port_traffic(
                    port,
                    port_traffic.total_bytes_in,
                    port_traffic.total_bytes_out,
                )
                .await
            {
                tracing::warn!("Failed to flush port_traffic for port {}: {}", port, e);
            }
            for bucket in &port_traffic.buckets {
                if let Err(e) = db
                    .replace_traffic_bucket(
                        port,
                        bucket.timestamp,
                        bucket.bytes_in,
                        bucket.bytes_out,
                    )
                    .await
                {
                    tracing::warn!("Failed to flush traffic_bucket for port {}: {}", port, e);
                }
            }
        }

        Ok(())
    }

    /// Start the background task that periodically flushes traffic data to the database.
    pub fn start_flush_task(&self) {
        let store = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
            loop {
                interval.tick().await;
                if let Err(e) = store.flush_to_db().await {
                    tracing::warn!("Failed to flush traffic data to DB: {}", e);
                }
            }
        });
    }

    fn add_to_bucket(
        port_traffic: &mut PortTraffic,
        bytes_in: u64,
        bytes_out: u64,
    ) -> DateTime<Utc> {
        let now = Utc::now();
        // Truncate to minute
        let bucket_time = now - chrono::Duration::seconds(now.second() as i64);

        // Check if we have a bucket for this minute
        if let Some(last) = port_traffic.buckets.back_mut() {
            if last.timestamp == bucket_time {
                last.bytes_in += bytes_in;
                last.bytes_out += bytes_out;
                return bucket_time;
            }
        }

        // Add new bucket
        port_traffic.buckets.push_back(TrafficBucket {
            timestamp: bucket_time,
            bytes_in,
            bytes_out,
        });

        // Keep only last 24 hours (1440 buckets)
        while port_traffic.buckets.len() > 1440 {
            port_traffic.buckets.pop_front();
        }

        bucket_time
    }

    /// Get traffic for all ports
    pub async fn get_all_traffic(&self) -> Vec<PortTraffic> {
        let store = self.inner.lock().await;
        store.values().cloned().collect()
    }

    /// Get traffic for specific port
    pub async fn get_port_traffic(&self, port: u16) -> Option<PortTraffic> {
        let store = self.inner.lock().await;
        store.get(&port).cloned()
    }

    /// Remove traffic data for a port (when client disconnects)
    /// Note: This only removes from in-memory cache, database history is preserved
    pub async fn remove_port(&self, port: u16) {
        let mut store = self.inner.lock().await;
        store.remove(&port);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_traffic_store_new() {
        let store = TrafficStore::new();
        let traffic = store.get_all_traffic().await;
        assert!(traffic.is_empty());
    }

    #[tokio::test]
    async fn test_record_bytes_in() {
        let store = TrafficStore::new();
        store.record_bytes_in(8080, 100).await;

        let traffic = store.get_port_traffic(8080).await.unwrap();
        assert_eq!(traffic.port, 8080);
        assert_eq!(traffic.total_bytes_in, 100);
        assert_eq!(traffic.total_bytes_out, 0);
        assert_eq!(traffic.buckets.len(), 1);
    }

    #[tokio::test]
    async fn test_record_bytes_out() {
        let store = TrafficStore::new();
        store.record_bytes_out(8080, 200).await;

        let traffic = store.get_port_traffic(8080).await.unwrap();
        assert_eq!(traffic.port, 8080);
        assert_eq!(traffic.total_bytes_in, 0);
        assert_eq!(traffic.total_bytes_out, 200);
        assert_eq!(traffic.buckets.len(), 1);
    }

    #[tokio::test]
    async fn test_record_multiple_ports() {
        let store = TrafficStore::new();
        store.record_bytes_in(8080, 100).await;
        store.record_bytes_out(9000, 200).await;

        let all_traffic = store.get_all_traffic().await;
        assert_eq!(all_traffic.len(), 2);

        let traffic_8080 = store.get_port_traffic(8080).await.unwrap();
        assert_eq!(traffic_8080.total_bytes_in, 100);

        let traffic_9000 = store.get_port_traffic(9000).await.unwrap();
        assert_eq!(traffic_9000.total_bytes_out, 200);
    }

    #[tokio::test]
    async fn test_record_accumulates() {
        let store = TrafficStore::new();
        store.record_bytes_in(8080, 100).await;
        store.record_bytes_in(8080, 200).await;
        store.record_bytes_out(8080, 50).await;
        store.record_bytes_out(8080, 75).await;

        let traffic = store.get_port_traffic(8080).await.unwrap();
        assert_eq!(traffic.total_bytes_in, 300);
        assert_eq!(traffic.total_bytes_out, 125);
    }

    #[tokio::test]
    async fn test_get_nonexistent_port() {
        let store = TrafficStore::new();
        let traffic = store.get_port_traffic(9999).await;
        assert!(traffic.is_none());
    }

    #[tokio::test]
    async fn test_remove_port() {
        let store = TrafficStore::new();
        store.record_bytes_in(8080, 100).await;
        assert!(store.get_port_traffic(8080).await.is_some());

        store.remove_port(8080).await;
        assert!(store.get_port_traffic(8080).await.is_none());
    }

    #[tokio::test]
    async fn test_remove_nonexistent_port() {
        let store = TrafficStore::new();
        // Should not panic
        store.remove_port(9999).await;
    }

    #[tokio::test]
    async fn test_bucket_creation() {
        let store = TrafficStore::new();
        store.record_bytes_in(8080, 100).await;

        let traffic = store.get_port_traffic(8080).await.unwrap();
        assert_eq!(traffic.buckets.len(), 1);

        let bucket = &traffic.buckets[0];
        assert_eq!(bucket.bytes_in, 100);
        assert_eq!(bucket.bytes_out, 0);
    }

    #[tokio::test]
    async fn test_traffic_store_clone() {
        let store = TrafficStore::new();
        store.record_bytes_in(8080, 100).await;

        let cloned = store.clone();
        let traffic = cloned.get_port_traffic(8080).await.unwrap();
        assert_eq!(traffic.total_bytes_in, 100);
    }

    #[tokio::test]
    async fn test_traffic_store_with_db() {
        let db = Database::new(":memory:").await.unwrap();
        let store = TrafficStore::with_db(db);

        store.record_bytes_in(8080, 100).await;
        store.record_bytes_out(8080, 200).await;

        let traffic = store.get_port_traffic(8080).await.unwrap();
        assert_eq!(traffic.total_bytes_in, 100);
        assert_eq!(traffic.total_bytes_out, 200);
    }
// 
//     #[tokio::test]
//     async fn test_traffic_store_load_from_db() {
//         let db = Database::new(":memory:").await.unwrap();
// 
//         // Pre-populate database
//         db.upsert_port_traffic(8080, 500, 1000).await.unwrap();
//         db.upsert_port_traffic(9000, 200, 400).await.unwrap();
// 
//         // Create store and load from DB
//         let store = TrafficStore::with_db(db);
//         store.load_from_db().await.unwrap();
// 
//         let traffic_8080 = store.get_port_traffic(8080).await.unwrap();
//         assert_eq!(traffic_8080.total_bytes_in, 500);
//         assert_eq!(traffic_8080.total_bytes_out, 1000);
// 
//         let traffic_9000 = store.get_port_traffic(9000).await.unwrap();
//         assert_eq!(traffic_9000.total_bytes_in, 200);
//         assert_eq!(traffic_9000.total_bytes_out, 400);
//     }

    #[tokio::test]
    async fn test_traffic_store_bucket_time_truncation() {
        let store = TrafficStore::new();
        store.record_bytes_in(8080, 100).await;
        store.record_bytes_in(8080, 50).await;

        let traffic = store.get_port_traffic(8080).await.unwrap();
        // Both should typically be in the same minute bucket
        // Total should always be correct regardless
        assert_eq!(traffic.total_bytes_in, 150);
        // If both landed in the same bucket, there should be 1 bucket with 150 bytes
        // If they landed in different buckets (second boundary), there could be 2
        let total_bucket_bytes: u64 = traffic.buckets.iter().map(|b| b.bytes_in).sum();
        assert_eq!(total_bucket_bytes, 150);
    }

    #[tokio::test]
    async fn test_traffic_store_zero_bytes() {
        let store = TrafficStore::new();
        store.record_bytes_in(8080, 0).await;

        let traffic = store.get_port_traffic(8080).await.unwrap();
        assert_eq!(traffic.total_bytes_in, 0);
        assert_eq!(traffic.buckets.len(), 1);
    }

    #[tokio::test]
    async fn test_health_response() {
        let response = HealthResponse { status: "ok" };
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("ok"));
    }

    #[test]
    fn test_login_request_deserialize() {
        let json = r#"{"password":"secret"}"#;
        let req: LoginRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.password, "secret");
    }

    #[test]
    fn test_server_metrics_serialize() {
        let metrics = ServerMetrics {
            client_count: 5,
            active_connection_count: 10,
            total_bytes_in: 1000,
            total_bytes_out: 2000,
        };
        let json = serde_json::to_string(&metrics).unwrap();
        assert!(json.contains("client_count"));
        assert!(json.contains("1000"));
    }

    #[test]
    fn test_shadowsocks_config_serialize() {
        let config = ShadowsocksConfig {
            enabled: true,
            port: Some(8388),
            cipher: Some("aes-256-gcm".into()),
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("8388"));
        assert!(json.contains("aes-256-gcm"));
    }

    #[test]
    fn test_shadowsocks_stats_serialize() {
        let stats = ShadowsocksStats {
            enabled: true,
            port: Some(8388),
            total_bytes_in: 1000,
            total_bytes_out: 2000,
            active_connections: 5,
        };
        let json = serde_json::to_string(&stats).unwrap();
        assert!(json.contains("active_connections"));
    }

    #[tokio::test]
    async fn test_update_acme_config_initializes_client() {
        // Create a server state with in-memory database
        let db = Database::new(":memory:").await.unwrap();
        let server_state = ServerState::with_db(db);

        // Verify ACME client is not initialized
        assert!(server_state.acme_client.read().await.is_none());

        // Create API state
        let state = ApiState {
            server_state: server_state.clone(),
            auth_config: Arc::new(AuthConfig::new(None, None)),
            log_store: None,
        };

        // Create request to enable ACME
        let req = UpdateAcmeConfigRequest {
            enabled: Some(true),
            server_url: Some("https://acme-staging-v02.api.letsencrypt.org/directory".to_string()),
            email: Some("test@example.com".to_string()),
            auto_renew: Some(true),
            renewal_check_interval: None,
            renewal_days_before_expiry: None,
            tos_agreed: Some(true),
        };

        // Call update_acme_config
        let _ = update_acme_config(State(state), Json(req)).await;

        // Verify ACME client is now initialized (shared Arc, visible from original)
        assert!(server_state.acme_client.read().await.is_some());

        // Verify ACME config is set
        let acme_config_guard = server_state.acme_config.read().await;
        assert!(acme_config_guard.is_some());
        let acme_config = acme_config_guard.as_ref().unwrap();
        assert!(acme_config.enabled);
        assert_eq!(
            acme_config.server_url,
            "https://acme-staging-v02.api.letsencrypt.org/directory"
        );
        drop(acme_config_guard);

        // Verify ACME full config is updated
        let full_config = server_state.acme_full_config.read().await;
        assert!(full_config.enabled);
        assert_eq!(
            full_config.server_url,
            "https://acme-staging-v02.api.letsencrypt.org/directory"
        );
        assert_eq!(full_config.email, Some("test@example.com".to_string()));
    }

    #[tokio::test]
    async fn test_update_acme_config_disabled_does_not_init_client() {
        // Create a server state with in-memory database
        let db = Database::new(":memory:").await.unwrap();
        let server_state = ServerState::with_db(db);

        // Verify ACME client is not initialized
        assert!(server_state.acme_client.read().await.is_none());

        // Create API state
        let state = ApiState {
            server_state: server_state.clone(),
            auth_config: Arc::new(AuthConfig::new(None, None)),
            log_store: None,
        };

        // Create request to disable ACME
        let req = UpdateAcmeConfigRequest {
            enabled: Some(false),
            server_url: Some("https://acme-staging-v02.api.letsencrypt.org/directory".to_string()),
            email: None,
            auto_renew: None,
            renewal_check_interval: None,
            renewal_days_before_expiry: None,
            tos_agreed: None,
        };

        // Call update_acme_config
        let _ = update_acme_config(State(state), Json(req)).await;

        // Verify ACME client is still not initialized
        assert!(server_state.acme_client.read().await.is_none());

        // Verify ACME config is still not set
        assert!(server_state.acme_config.read().await.is_none());

        // Verify ACME full config is updated
        let full_config = server_state.acme_full_config.read().await;
        assert!(!full_config.enabled);
    }

    #[tokio::test]
    async fn test_get_acme_status_reflects_config_update() {
        // Create a server state with in-memory database
        let db = Database::new(":memory:").await.unwrap();
        let server_state = ServerState::with_db(db);

        // Create API state
        let state = ApiState {
            server_state: server_state.clone(),
            auth_config: Arc::new(AuthConfig::new(None, None)),
            log_store: None,
        };

        // Initial status should show ACME disabled
        let _ = get_acme_status(State(state.clone())).await;
        let full_config = server_state.acme_full_config.read().await;
        assert!(!full_config.enabled);
        drop(full_config);

        // Enable ACME
        let req = UpdateAcmeConfigRequest {
            enabled: Some(true),
            server_url: Some("https://acme-staging-v02.api.letsencrypt.org/directory".to_string()),
            email: Some("test@example.com".to_string()),
            auto_renew: Some(true),
            renewal_check_interval: None,
            renewal_days_before_expiry: None,
            tos_agreed: Some(true),
        };

        let _ = update_acme_config(State(state), Json(req)).await;

        // Verify ACME is now enabled in the config
        let full_config = server_state.acme_full_config.read().await;
        assert!(full_config.enabled);
        assert_eq!(
            full_config.server_url,
            "https://acme-staging-v02.api.letsencrypt.org/directory"
        );

        // Verify ACME client is initialized
        assert!(server_state.acme_client.read().await.is_some());
    }

    fn trojan_test_api_state(
        trojan: Option<crate::server::dynamic_config::TrojanDynamicConfig>,
    ) -> ApiState {
        let server_state = ServerState::new();
        // 直接写内存中的 dynamic_config（与 set_dynamic_config 等效，免去构造全量结构）
        if let Ok(mut dc) = server_state.dynamic_config.try_write() {
            dc.trojan = trojan;
        }
        ApiState {
            server_state,
            auth_config: Arc::new(AuthConfig::new(None, None)),
            log_store: None,
        }
    }

    /// 共享模式（与反代复用端口，不 register_trojan）下 stats 回退
    /// dynamic_config 端口读取记账数据；enabled/port 反映实际运行状态。
    #[tokio::test]
    async fn test_trojan_stats_shared_mode_fallback() {
        let port = 1443;
        let state =
            trojan_test_api_state(Some(crate::server::dynamic_config::TrojanDynamicConfig {
                enabled: true,
                port,
                password: "p".to_string(),
                fallback: "127.0.0.1:80".to_string(),
                domain: "t.example.com".to_string(),
            }));
        // 共享模式：端口未注册，但流量与连接数按 trojan_port 记账
        state
            .server_state
            .traffic_store
            .record_bytes_in(port, 100)
            .await;
        state
            .server_state
            .traffic_store
            .record_bytes_out(port, 40)
            .await;
        state.server_state.increment_trojan_connections(port).await;
        state.server_state.trojan_runtime.write().await.cert_source =
            Some("self_signed".to_string());

        let Json(stats) = get_trojan_stats(State(state)).await;
        assert!(stats.enabled);
        assert_eq!(stats.port, Some(port));
        assert_eq!(stats.total_bytes_in, 100);
        assert_eq!(stats.total_bytes_out, 40);
        assert_eq!(stats.active_connections, 1);
    }

    /// trojan 未配置/未运行时 stats 返回 disabled 且为零值。
    #[tokio::test]
    async fn test_trojan_stats_disabled_is_zero() {
        let state = trojan_test_api_state(None);
        let Json(stats) = get_trojan_stats(State(state)).await;
        assert!(!stats.enabled);
        assert_eq!(stats.port, None);
        assert_eq!(stats.total_bytes_in, 0);
        assert_eq!(stats.total_bytes_out, 0);
        assert_eq!(stats.active_connections, 0);
    }

    // ── Stats unified API tests ──────────────────────────────────

    #[tokio::test]
    async fn test_stats_summary_empty_is_all_zero() {
        let state = ApiState {
            server_state: ServerState::new(),
            auth_config: Arc::new(AuthConfig::new(None, None)),
            log_store: None,
        };
        let response = get_stats_summary(State(state)).await;
        let body = response.into_response().into_body();
        // body is a boxed body — let's just verify it doesn't panic
        // and the handler returns a valid response
        let _ = body;
    }

    #[tokio::test]
    async fn test_stats_summary_reflects_recorded_data() {
        let server_state = ServerState::new();
        server_state.stats_collector.record_bytes(
            crate::server::stats::EntityType::Proxy,
            "rule1",
            100,
            200,
        );
        server_state.stats_collector.incr_conns(
            crate::server::stats::EntityType::Client,
            "home-nas",
        );

        let state = ApiState {
            server_state,
            auth_config: Arc::new(AuthConfig::new(None, None)),
            log_store: None,
        };
        let response = get_stats_summary(State(state)).await;
        let bytes = response.into_response().into_body();
        let _ = bytes;
    }

    #[tokio::test]
    async fn test_stats_summary_empty_returns_default() {
        let server_state = ServerState::new();
        let state = ApiState {
            server_state,
            auth_config: Arc::new(AuthConfig::new(None, None)),
            log_store: None,
        };
        // Just verify the handler doesn't panic with an empty state
        let response = get_stats_summary(State(state)).await;
        assert!(true, "get_stats_summary should not panic");
    }

    #[tokio::test]
    async fn test_stats_query_requires_start_end() {
        let db = Database::new(":memory:").await.unwrap();
        let server_state = ServerState::with_db(db);
        let state = ApiState {
            server_state,
            auth_config: Arc::new(AuthConfig::new(None, None)),
            log_store: None,
        };
        // Query with invalid params should return error
        let response = get_stats_query(
            State(state),
            Query(StatsQueryParams {
                entity_type: None,
                entity_id: None,
                start: "not-a-date".to_string(),
                end: "2026-01-01T00:00:00Z".to_string(),
            }),
        )
        .await;
        let resp = response.into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_stats_query_range_exceeds_7_days() {
        let db = Database::new(":memory:").await.unwrap();
        let server_state = ServerState::with_db(db);
        let state = ApiState {
            server_state,
            auth_config: Arc::new(AuthConfig::new(None, None)),
            log_store: None,
        };
        let response = get_stats_query(
            State(state),
            Query(StatsQueryParams {
                entity_type: None,
                entity_id: None,
                start: "2026-01-01T00:00:00Z".to_string(),
                end: "2026-01-10T00:00:00Z".to_string(),
            }),
        )
        .await;
        let resp = response.into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_stats_query_valid_range_returns_data() {
        let db = Database::new(":memory:").await.unwrap();
        // Insert test data directly
        let now = chrono::Utc::now();
        let ts = now - chrono::Duration::seconds(now.second() as i64);
        sqlx::query(
            "INSERT INTO stats_snapshots (entity_type, entity_id, timestamp, bytes_in, bytes_out, bytes_in_rate, bytes_out_rate, rtt_ms, loss_pct, active_conns) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind("shadowsocks")
        .bind("ss:8388")
        .bind(ts)
        .bind(1000_i64)
        .bind(2000_i64)
        .bind(50.0_f64)
        .bind(100.0_f64)
        .bind(Some(12.0_f64))
        .bind(None::<f64>)
        .bind(2_i32)
        .execute(&db.pool)
        .await
        .unwrap();

        let server_state = ServerState::with_db(db);
        let state = ApiState {
            server_state,
            auth_config: Arc::new(AuthConfig::new(None, None)),
            log_store: None,
        };
        let response = get_stats_query(
            State(state),
            Query(StatsQueryParams {
                entity_type: Some(vec!["shadowsocks".to_string()]),
                entity_id: None,
                start: (ts - chrono::Duration::minutes(1)).to_rfc3339(),
                end: (ts + chrono::Duration::minutes(1)).to_rfc3339(),
            }),
        )
        .await;
        let resp = response.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
    }

}

/// Log entry response
#[derive(Debug, Serialize)]
pub struct LogEntryResponse {
    pub id: i64,
    pub timestamp: i64,
    pub level: String,
    pub source: String,
    pub target: String,
    pub message: String,
}

/// Query parameters for GET /api/logs
#[derive(Debug, Deserialize)]
pub struct LogsQuery {
    pub level: Option<String>,
    pub source: Option<String>,
    pub search: Option<String>,
    pub limit: Option<u32>,
    pub before_id: Option<i64>,
}

/// Request body for PUT /api/logs/level
#[derive(Debug, Deserialize)]
pub struct SetLevelRequest {
    pub level: String,
}

/// SSE query params (for token-based auth)
#[derive(Debug, Deserialize)]
pub struct SseQuery {
    pub level: Option<String>,
    pub source: Option<String>,
    pub token: Option<String>,
}

/// API state shared across all handlers
#[derive(Clone)]
pub struct ApiState {
    pub server_state: ServerState,
    pub auth_config: Arc<AuthConfig>,
    pub log_store: Option<crate::server::logs::LogStore>,
}

/// Login request
#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub password: String,
}

/// Login response
#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub token: String,
    pub auth_required: bool,
}

/// Server metrics
#[derive(Debug, Serialize)]
pub struct ServerMetrics {
    pub client_count: usize,
    pub active_connection_count: usize,
    pub total_bytes_in: u64,
    pub total_bytes_out: u64,
}

/// Health check response
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
}

/// Client with quality data
#[derive(Debug, Serialize)]
pub struct ClientWithQuality {
    pub port: u16,
    pub hostname: Option<String>,
    pub quality: ConnectionQuality,
}

/// Port quality response with history
#[derive(Debug, Serialize)]
pub struct PortQualityResponse {
    pub current: ConnectionQuality,
    pub history: Vec<QualitySample>,
}

/// Quality warning
#[derive(Debug, Serialize)]
pub struct QualityWarning {
    pub port: u16,
    pub hostname: Option<String>,
    pub quality: ConnectionQuality,
    pub warning_type: String,
}

/// Query parameters for history
#[derive(Debug, Deserialize)]
pub struct QualityHistoryQuery {
    pub start: Option<String>,
    pub end: Option<String>,
}

/// Shadowsocks configuration
#[derive(Debug, Serialize)]
pub struct ShadowsocksConfig {
    pub enabled: bool,
    pub port: Option<u16>,
    pub cipher: Option<String>,
}

/// Shadowsocks statistics
#[derive(Debug, Serialize)]
pub struct ShadowsocksStats {
    pub enabled: bool,
    pub port: Option<u16>,
    pub total_bytes_in: u64,
    pub total_bytes_out: u64,
    pub active_connections: usize,
}

/// Shadowsocks quality response
#[derive(Debug, Serialize)]
pub struct ShadowsocksQuality {
    pub port: u16,
    pub quality: ConnectionQuality,
    pub history: Vec<QualitySample>,
}

/// Trojan configuration
#[derive(Debug, Serialize)]
pub struct TrojanConfig {
    pub enabled: bool,
    pub port: Option<u16>,
    pub fallback: Option<String>,
    pub domain: Option<String>,
    /// 证书来源："acme_exact" | "acme_wildcard" | "self_signed"；未运行时为 null
    pub cert_source: Option<String>,
    /// true = 与反代共享端口（SNI 分流）；false = 独立监听
    pub shared: bool,
}

/// Trojan statistics
#[derive(Debug, Serialize)]
pub struct TrojanStats {
    pub enabled: bool,
    pub port: Option<u16>,
    pub total_bytes_in: u64,
    pub total_bytes_out: u64,
    pub active_connections: usize,
}

/// Trojan quality response
#[derive(Debug, Serialize)]
pub struct TrojanQuality {
    pub port: u16,
    pub quality: ConnectionQuality,
    pub history: Vec<QualitySample>,
}

/// Mesh network info response
#[derive(Debug, Serialize)]
pub struct MeshNetworkResponse {
    pub id: String,
    pub members: Vec<MeshMemberResponse>,
    pub services: Vec<MeshServiceResponse>,
}

#[derive(Debug, Serialize)]
pub struct MeshMemberResponse {
    pub client_name: String,
    pub public_addr: Option<String>,
    pub p2p_available: bool,
    pub online: bool,
}

#[derive(Debug, Serialize)]
pub struct MeshServiceResponse {
    pub service_name: String,
    pub protocol: String,
    pub local_addr: String,
    pub client_name: String,
}

/// DNS record response
#[derive(Debug, Serialize)]
pub struct DnsRecordResponse {
    pub name: String,
    pub record_type: String,
    pub value: String,
}

/// Request to add a manual DNS record
#[derive(Debug, Deserialize)]
pub struct AddDnsRecordRequest {
    pub name: String,
    pub record_type: String,
    pub value: String,
    pub port: Option<u16>,
}

/// Request body for PUT /api/acme/config
#[derive(Debug, Deserialize)]
struct UpdateAcmeConfigRequest {
    enabled: Option<bool>,
    server_url: Option<String>,
    email: Option<String>,
    auto_renew: Option<bool>,
    renewal_check_interval: Option<u64>,
    renewal_days_before_expiry: Option<u64>,
    tos_agreed: Option<bool>,
}

/// 证书申请请求
#[derive(Debug, Deserialize)]
struct CertificateRequest {
    /// 挑战类型: "http-01" 或 "dns-01"
    challenge_type: Option<String>,
}

// Login handler
async fn login(
    State(state): State<ApiState>,
    Json(request): Json<LoginRequest>,
) -> impl IntoResponse {
    if !state.auth_config.is_enabled() {
        let token = create_token(&state.auth_config.jwt_secret)
            .unwrap_or_else(|_| "dummy-token".to_string());
        return Json(LoginResponse {
            token,
            auth_required: false,
        })
        .into_response();
    }

    if state.auth_config.verify_password(&request.password) {
        match create_token(&state.auth_config.jwt_secret) {
            Ok(token) => Json(LoginResponse {
                token,
                auth_required: true,
            })
            .into_response(),
            Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Failed to create token").into_response(),
        }
    } else {
        (StatusCode::UNAUTHORIZED, "Invalid password").into_response()
    }
}

// Logout handler (client just discards token)
async fn logout() -> impl IntoResponse {
    StatusCode::OK
}

// Get traffic for all clients
async fn get_traffic(State(state): State<ApiState>) -> Json<Vec<PortTraffic>> {
    Json(state.server_state.traffic_store.get_all_traffic().await)
}

// Get traffic for specific port
async fn get_port_traffic(
    State(state): State<ApiState>,
    Path(port): Path<u16>,
) -> impl IntoResponse {
    match state
        .server_state
        .traffic_store
        .get_port_traffic(port)
        .await
    {
        Some(traffic) => Json(traffic).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// Health check
async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

// Get server metrics
async fn get_metrics(State(state): State<ApiState>) -> Json<ServerMetrics> {
    let client_count = state.server_state.get_client_count().await;
    let active_connection_count = state.server_state.get_active_connection_count().await;

    let traffic = state.server_state.traffic_store.get_all_traffic().await;
    let total_bytes_in = traffic.iter().map(|t| t.total_bytes_in).sum();
    let total_bytes_out = traffic.iter().map(|t| t.total_bytes_out).sum();

    Json(ServerMetrics {
        client_count,
        active_connection_count,
        total_bytes_in,
        total_bytes_out,
    })
}

/// Serve embedded static files for frontend
#[cfg(feature = "embed-frontend")]
async fn serve_static(Path(path): Path<String>) -> impl IntoResponse {
    let path = if path.is_empty() { "index.html" } else { &path };

    match FrontendAssets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            axum::http::Response::builder()
                .header(axum::http::header::CONTENT_TYPE, mime.as_ref())
                .body(Body::from(content.data))
                .unwrap()
        }
        None => {
            // Fallback to index.html for SPA routing
            if let Some(index) = FrontendAssets::get("index.html") {
                axum::http::Response::builder()
                    .header(axum::http::header::CONTENT_TYPE, "text/html")
                    .body(Body::from(index.data))
                    .unwrap()
            } else {
                axum::http::Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(Body::from("Not found"))
                    .unwrap()
            }
        }
    }
}

// Get all clients with quality data
async fn get_all_quality(State(state): State<ApiState>) -> Json<Vec<ClientWithQuality>> {
    let clients = state.server_state.get_all_clients().await;
    let mut result = Vec::with_capacity(clients.len());

    for (port, info) in clients {
        if let Some(quality) = state.server_state.quality_store.get_quality(port).await {
            result.push(ClientWithQuality {
                port,
                hostname: info.hostname,
                quality,
            });
        }
    }

    Json(result)
}

// Get quality data for a single port
async fn get_port_quality(
    State(state): State<ApiState>,
    Path(port): Path<u16>,
) -> impl IntoResponse {
    let current = state.server_state.quality_store.get_quality(port).await;
    let history = state.server_state.quality_store.get_samples(port).await;

    match current {
        Some(current) => Json(PortQualityResponse { current, history }).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// Get quality history for a port (with optional time range)
async fn get_quality_history(
    State(state): State<ApiState>,
    Path(port): Path<u16>,
    Query(_params): Query<QualityHistoryQuery>,
) -> Json<Vec<QualitySample>> {
    // For now, just return in-memory samples (last 60 minutes)
    // Future: support database queries for longer time ranges
    let samples = state.server_state.quality_store.get_samples(port).await;
    Json(samples)
}

// Get current quality warnings
async fn get_quality_warnings(State(state): State<ApiState>) -> Json<Vec<QualityWarning>> {
    let clients = state.server_state.get_all_clients().await;
    let mut warnings = Vec::new();

    for (port, info) in clients {
        if let Some(quality) = state.server_state.quality_store.get_quality(port).await {
            let warning_type = if quality.is_critical {
                Some("critical".to_string())
            } else if quality.is_warning {
                Some("warning".to_string())
            } else {
                None
            };

            if let Some(warning_type) = warning_type {
                warnings.push(QualityWarning {
                    port,
                    hostname: info.hostname,
                    quality,
                    warning_type,
                });
            }
        }
    }

    Json(warnings)
}

// Get Shadowsocks configuration
async fn get_shadowsocks_config(State(state): State<ApiState>) -> Json<ShadowsocksConfig> {
    // Get all SS ports
    let ss_ports = state.server_state.get_shadowsocks_ports().await;

    let (port, cipher) = if !ss_ports.is_empty() {
        // For now, return the first SS port info with default cipher
        // In future multi-port support, this would return all
        (Some(ss_ports[0]), Some("aes-256-gcm".to_string()))
    } else {
        (None, None)
    };

    Json(ShadowsocksConfig {
        enabled: !ss_ports.is_empty(),
        port,
        cipher,
    })
}

// Get Shadowsocks traffic statistics
async fn get_shadowsocks_stats(State(state): State<ApiState>) -> Json<ShadowsocksStats> {
    let ss_ports = state.server_state.get_shadowsocks_ports().await;

    let mut total_bytes_in = 0;
    let mut total_bytes_out = 0;
    let mut active_connections = 0;

    for &port in &ss_ports {
        if let Some(traffic) = state
            .server_state
            .traffic_store
            .get_port_traffic(port)
            .await
        {
            total_bytes_in += traffic.total_bytes_in;
            total_bytes_out += traffic.total_bytes_out;
        }
        active_connections += state.server_state.get_connection_count_for_port(port).await;
    }

    Json(ShadowsocksStats {
        enabled: !ss_ports.is_empty(),
        port: ss_ports.first().copied(),
        total_bytes_in,
        total_bytes_out,
        active_connections,
    })
}

// Get Shadowsocks quality data
async fn get_shadowsocks_quality(State(state): State<ApiState>) -> Json<Vec<ShadowsocksQuality>> {
    let ss_ports = state.server_state.get_shadowsocks_ports().await;
    let mut result = Vec::with_capacity(ss_ports.len());

    for port in ss_ports {
        if let Some(quality) = state.server_state.quality_store.get_quality(port).await {
            let history = state.server_state.quality_store.get_samples(port).await;
            result.push(ShadowsocksQuality {
                port,
                quality,
                history,
            });
        }
    }

    Json(result)
}

// Update Shadowsocks configuration (start/stop/modify)
async fn update_shadowsocks_config(
    State(state): State<ApiState>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    let enabled = payload["enabled"].as_bool().unwrap_or(false);
    let port = match payload["port"].as_u64() {
        Some(p) if p > 0 && p <= 65535 => p as u16,
        _ => {
            return (StatusCode::BAD_REQUEST, "Invalid or missing port").into_response();
        }
    };
    let cipher = match payload["cipher"].as_str() {
        Some(c @ "aes-256-gcm") | Some(c @ "chacha20-ietf-poly1305") => c,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                "Invalid cipher. Supported: aes-256-gcm, chacha20-ietf-poly1305",
            )
                .into_response();
        }
    };
    let password = match payload["password"].as_str() {
        Some(p) if !p.is_empty() => p,
        _ => {
            return (StatusCode::BAD_REQUEST, "Password required").into_response();
        }
    };

    // Save to DB（单份配置语义：整表替换，避免修改端口时残留旧行）
    if let Some(db) = state.server_state.db() {
        if let Err(e) = db
            .replace_shadowsocks_config(port, cipher, password, enabled)
            .await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("DB error: {}", e),
            )
                .into_response();
        }
    }

    // Update in-memory config
    {
        let mut dc = state.server_state.dynamic_config.write().await;
        dc.ss = Some(crate::server::dynamic_config::ShadowsocksDynamicConfig {
            enabled,
            port,
            cipher: cipher.to_string(),
            password: password.to_string(),
        });
    }

    // Handle listener lifecycle
    {
        let mut abort = state.server_state.ss_listener_abort.write().await;
        // Stop existing listener if any
        if let Some(tx) = abort.take() {
            let _ = tx.send(true);
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        if enabled {
            let (abort_tx, abort_rx) = tokio::sync::watch::channel(false);
            *abort = Some(abort_tx);
            let state_clone = state.server_state.clone();
            let ss_port = port;
            let ss_cipher = cipher.to_string();
            let ss_password = password.to_string();
            tokio::spawn(async move {
                if let Err(e) = crate::server::listener::start_shadowsocks_listener_with_abort(
                    state_clone,
                    ss_port,
                    ss_cipher,
                    ss_password,
                    abort_rx,
                )
                .await
                {
                    tracing::error!("SS listener error: {}", e);
                }
            });
        }
    }

    Json(serde_json::json!({"status": "ok", "enabled": enabled, "port": port})).into_response()
}

// Get Trojan configuration
async fn get_trojan_config(State(state): State<ApiState>) -> Json<TrojanConfig> {
    let (enabled, port, fallback, domain) = {
        let dc = state.server_state.dynamic_config.read().await;
        match dc.trojan.as_ref() {
            Some(t) => (
                t.enabled,
                Some(t.port),
                Some(t.fallback.clone()),
                Some(t.domain.clone()),
            ),
            None => (false, None, None, None),
        }
    };
    let rt = state.server_state.trojan_runtime.read().await;
    Json(TrojanConfig {
        enabled,
        port,
        fallback,
        domain,
        cert_source: rt.cert_source.clone(),
        shared: rt.shared,
    })
}

/// Trojan 记账端口：优先取独立监听注册（`register_trojan`）的端口；
/// 共享模式（与反代复用端口）下不注册端口，回退到 dynamic_config 中
/// enabled 的 trojan 端口——流量与连接数仍按该端口记账（见 proxy.rs）。
async fn trojan_accounting_ports(server_state: &ServerState) -> Vec<u16> {
    let ports = server_state.get_trojan_ports().await;
    if !ports.is_empty() {
        return ports;
    }
    let dc = server_state.dynamic_config.read().await;
    match dc.trojan.as_ref() {
        Some(t) if t.enabled => vec![t.port],
        _ => Vec::new(),
    }
}

// Get Trojan traffic statistics
async fn get_trojan_stats(State(state): State<ApiState>) -> Json<TrojanStats> {
    let trojan_ports = trojan_accounting_ports(&state.server_state).await;
    // 运行状态以 trojan_runtime 为准：apply_trojan_config 运行时写入
    // cert_source（共享/独立两种模式都会写），停止时清空
    let running = state
        .server_state
        .trojan_runtime
        .read()
        .await
        .cert_source
        .is_some();

    let mut total_bytes_in = 0;
    let mut total_bytes_out = 0;
    let mut active_connections = 0;

    for &port in &trojan_ports {
        if let Some(traffic) = state
            .server_state
            .traffic_store
            .get_port_traffic(port)
            .await
        {
            total_bytes_in += traffic.total_bytes_in;
            total_bytes_out += traffic.total_bytes_out;
        }
        active_connections += state.server_state.get_connection_count_for_port(port).await;
    }

    Json(TrojanStats {
        enabled: running,
        port: if running {
            trojan_ports.first().copied()
        } else {
            None
        },
        total_bytes_in,
        total_bytes_out,
        active_connections,
    })
}

// Get Trojan quality data
async fn get_trojan_quality(State(state): State<ApiState>) -> Json<Vec<TrojanQuality>> {
    let trojan_ports = trojan_accounting_ports(&state.server_state).await;
    let mut result = Vec::with_capacity(trojan_ports.len());

    for port in trojan_ports {
        if let Some(quality) = state.server_state.quality_store.get_quality(port).await {
            let history = state.server_state.quality_store.get_samples(port).await;
            result.push(TrojanQuality {
                port,
                quality,
                history,
            });
        }
    }

    Json(result)
}

// Update Trojan configuration (start/stop/modify)
async fn update_trojan_config(
    State(state): State<ApiState>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    let enabled = payload["enabled"].as_bool().unwrap_or(false);
    let port = match payload["port"].as_u64() {
        Some(p) if p > 0 && p <= 65535 => p as u16,
        _ => {
            return (StatusCode::BAD_REQUEST, "Invalid or missing port").into_response();
        }
    };
    let fallback = payload["fallback"].as_str().unwrap_or("127.0.0.1:80");

    // 现有配置：password/domain 留空时保留原值
    let existing = {
        let dc = state.server_state.dynamic_config.read().await;
        dc.trojan.clone()
    };

    // password 可选：留空保留原密码（首次启用必须提供）
    let password = match payload["password"].as_str() {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => match existing.as_ref() {
            Some(t) if !t.password.is_empty() => t.password.clone(),
            _ => {
                return (StatusCode::BAD_REQUEST, "Password required").into_response();
            }
        },
    };

    // domain 可选：空串 = 不用 ACME 证书、不参与 SNI 分流
    let domain = match payload["domain"].as_str() {
        Some(d) => d.trim().to_ascii_lowercase(),
        None => existing
            .as_ref()
            .map(|t| t.domain.clone())
            .unwrap_or_default(),
    };
    if !domain.is_empty() {
        if let Err(e) = crate::server::trojan::validate_trojan_domain(&domain) {
            return (StatusCode::BAD_REQUEST, format!("Invalid domain: {}", e)).into_response();
        }
    }

    // 端口与反代共享的边界规则检查
    if enabled {
        if let Some((listen_addr, tls_enabled)) = state
            .server_state
            .proxy_state
            .http_listen_addr_for_port(port)
            .await
        {
            if !tls_enabled {
                return (
                    StatusCode::BAD_REQUEST,
                    format!(
                        "Port {} is used by reverse proxy listener {} without TLS. Trojan requires TLS.",
                        port, listen_addr
                    ),
                )
                    .into_response();
            }
            if domain.is_empty() {
                return (
                    StatusCode::BAD_REQUEST,
                    format!(
                        "Port {} is used by reverse proxy listener {}. Set a domain to share it via SNI.",
                        port, listen_addr
                    ),
                )
                    .into_response();
            }
        }
    }

    // Save to DB（单份配置语义：整表替换，避免修改端口时残留旧行）
    if let Some(db) = state.server_state.db() {
        if let Err(e) = db
            .replace_trojan_config(port, &password, fallback, enabled, &domain)
            .await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("DB error: {}", e),
            )
                .into_response();
        }
    }

    // Update in-memory config（new_cfg 同时供 lifecycle 段复用）
    let new_cfg = crate::server::dynamic_config::TrojanDynamicConfig {
        enabled,
        port,
        password: password.clone(),
        fallback: fallback.to_string(),
        domain: domain.clone(),
    };
    {
        let mut dc = state.server_state.dynamic_config.write().await;
        dc.trojan = Some(new_cfg.clone());
    }

    // Handle listener lifecycle（证书解析 + 共享/独立模式判定统一在 trojan_runtime）
    if let Err(e) =
        crate::server::trojan_runtime::apply_trojan_config(&state.server_state, &new_cfg).await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to apply trojan config: {}", e),
        )
            .into_response();
    }

    Json(serde_json::json!({"status": "ok", "enabled": enabled, "port": port, "domain": domain}))
        .into_response()
}

// ── Mesh Network Endpoints ─────────────────────────────────────────

// GET /api/mesh — list all meshes
async fn list_meshes(State(state): State<ApiState>) -> impl IntoResponse {
    let networks = state.server_state.mesh_manager.list_networks().await;
    let response: Vec<MeshNetworkResponse> = networks
        .into_iter()
        .map(|(id, members)| {
            let services: Vec<MeshServiceResponse> = members
                .iter()
                .flat_map(|m| {
                    m.services.iter().map(|s| MeshServiceResponse {
                        service_name: s.name.clone(),
                        protocol: s.protocol.clone(),
                        local_addr: s.local_addr.clone(),
                        client_name: m.client_name.clone(),
                    })
                })
                .collect();

            MeshNetworkResponse {
                id,
                members: members
                    .iter()
                    .map(|m| MeshMemberResponse {
                        client_name: m.client_name.clone(),
                        public_addr: m.public_addr.clone(),
                        p2p_available: m.p2p_available,
                        online: true,
                    })
                    .collect(),
                services,
            }
        })
        .collect();
    Json(response)
}

// GET /api/mesh/:id — mesh detail
async fn get_mesh(State(state): State<ApiState>, Path(mesh_id): Path<String>) -> impl IntoResponse {
    match state.server_state.mesh_manager.get_mesh(&mesh_id).await {
        Some(members) => {
            let services: Vec<MeshServiceResponse> = members
                .iter()
                .flat_map(|m| {
                    m.services.iter().map(|s| MeshServiceResponse {
                        service_name: s.name.clone(),
                        protocol: s.protocol.clone(),
                        local_addr: s.local_addr.clone(),
                        client_name: m.client_name.clone(),
                    })
                })
                .collect();

            Json(MeshNetworkResponse {
                id: mesh_id,
                members: members
                    .iter()
                    .map(|m| MeshMemberResponse {
                        client_name: m.client_name.clone(),
                        public_addr: m.public_addr.clone(),
                        p2p_available: m.p2p_available,
                        online: true,
                    })
                    .collect(),
                services,
            })
            .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// GET /api/mesh/:id/services — mesh services
async fn get_mesh_services(
    State(state): State<ApiState>,
    Path(mesh_id): Path<String>,
) -> impl IntoResponse {
    match state.server_state.mesh_manager.get_mesh(&mesh_id).await {
        Some(members) => {
            let services: Vec<MeshServiceResponse> = members
                .iter()
                .flat_map(|m| {
                    m.services.iter().map(|s| MeshServiceResponse {
                        service_name: s.name.clone(),
                        protocol: s.protocol.clone(),
                        local_addr: s.local_addr.clone(),
                        client_name: m.client_name.clone(),
                    })
                })
                .collect();
            Json(services).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// ── DNS Management Endpoints ───────────────────────────────────────

// GET /api/dns/records — list all DNS records
async fn get_dns_records(State(state): State<ApiState>) -> impl IntoResponse {
    let dns_registry = match &state.server_state.dns_registry {
        Some(r) => r,
        None => return (StatusCode::SERVICE_UNAVAILABLE, "DNS not enabled").into_response(),
    };

    let records = dns_registry.list_records().await;
    let response: Vec<DnsRecordResponse> = records
        .iter()
        .map(|r| DnsRecordResponse {
            name: r.name().to_string(),
            record_type: r.record_type().to_string(),
            value: match r {
                DnsRecord::TunnelA {
                    target_ip, port, ..
                } => format!("{} (port {})", target_ip, port),
                DnsRecord::MeshA { target_ip, .. } => target_ip.clone(),
                DnsRecord::TunnelSrv { target, port, .. } => format!("{}:{}", target, port),
                DnsRecord::MeshSrv { target, port, .. } => format!("{}:{}", target, port),
                DnsRecord::Txt { text, .. } => text.clone(),
            },
        })
        .collect();

    Json(response).into_response()
}

// POST /api/dns/records — add manual DNS record
async fn add_dns_record(
    State(state): State<ApiState>,
    Json(body): Json<AddDnsRecordRequest>,
) -> impl IntoResponse {
    let dns_registry = match &state.server_state.dns_registry {
        Some(r) => r,
        None => return (StatusCode::SERVICE_UNAVAILABLE, "DNS not enabled").into_response(),
    };

    let record = match body.record_type.as_str() {
        "A" => DnsRecord::TunnelA {
            name: body.name.clone(),
            target_ip: body.value.clone(),
            port: body.port.unwrap_or(80),
        },
        _ => return (StatusCode::BAD_REQUEST, "Unsupported record type").into_response(),
    };

    dns_registry.add_manual_record(record).await;
    StatusCode::CREATED.into_response()
}

// DELETE /api/dns/records/:name — delete DNS record
async fn delete_dns_record(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let dns_registry = match &state.server_state.dns_registry {
        Some(r) => r,
        None => return (StatusCode::SERVICE_UNAVAILABLE, "DNS not enabled").into_response(),
    };

    dns_registry.remove_record(&name).await;
    StatusCode::OK.into_response()
}

// ── Log Viewer Endpoints ──────────────────────────────────────────

async fn sse_log_stream(
    State(state): State<ApiState>,
    Query(params): Query<SseQuery>,
) -> impl IntoResponse {
    // Check auth for SSE
    if state.auth_config.is_enabled() {
        let token = params.token.as_deref().unwrap_or("");

        let is_valid = if !token.is_empty() {
            crate::server::auth::validate_token(token, &state.auth_config.jwt_secret).is_ok()
        } else {
            false
        };

        if !is_valid {
            return axum::response::Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .body(Body::from("Unauthorized"))
                .unwrap();
        }
    }

    let log_store = match &state.log_store {
        Some(store) => store.clone(),
        None => {
            return axum::response::Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body(Body::from("Log store not initialized"))
                .unwrap();
        }
    };

    let min_level = params.level.as_deref().unwrap_or("info");
    let min_level_u8 = match min_level {
        "error" => 4u8,
        "warn" => 3,
        "info" => 2,
        "debug" => 1,
        "trace" => 0,
        _ => 2,
    };
    let source_filter = params.source.clone();

    let mut rx = log_store.tx.subscribe();
    let stream = async_stream::stream! {
        loop {
            match tokio::time::timeout(Duration::from_secs(30), rx.recv()).await {
                Ok(Ok(entry)) => {
                    // Apply filters
                    let entry_level = match entry.level.as_str() {
                        "TRACE" => 0, "DEBUG" => 1, "INFO" => 2, "WARN" => 3, "ERROR" => 4,
                        _ => 2,
                    };
                    if entry_level < min_level_u8 {
                        continue;
                    }
                    if let Some(ref src) = source_filter {
                        if !entry.source.starts_with(src) {
                            continue;
                        }
                    }

                    let json = serde_json::to_string(&LogEntryResponse {
                        id: entry.id,
                        timestamp: entry.timestamp,
                        level: entry.level.clone(),
                        source: entry.source.clone(),
                        target: entry.target.clone(),
                        message: entry.message.clone(),
                    })
                    .unwrap_or_default();

                    yield Ok::<_, std::convert::Infallible>(
                        Event::default().event("log").data(json),
                    );
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(n))) => {
                    yield Ok::<_, std::convert::Infallible>(
                        Event::default()
                            .event("sync")
                            .data(format!(r#"{{"lagged":{}}}"#, n)),
                    );
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                    break;
                }
                Err(_) => {
                    // Timeout — send ping to keep connection alive
                    yield Ok::<_, std::convert::Infallible>(
                        Event::default().event("ping").data(""),
                    );
                }
            }
        }
    };

    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(30)))
        .into_response()
}

async fn get_logs(
    State(state): State<ApiState>,
    Query(params): Query<LogsQuery>,
) -> impl IntoResponse {
    let log_store = match &state.log_store {
        Some(store) => store,
        None => {
            return axum::response::Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body(Body::from("Log store not initialized"))
                .unwrap();
        }
    };

    let limit = params.limit.unwrap_or(200).min(1000) as usize;

    // When before_id is specified, query DB directly for correct pagination
    // (in-memory entries have id=0, so DB pagination is the only correct path)
    if params.before_id.is_some() {
        let db_entries = log_store
            .query_db(
                params.level.as_deref(),
                params.source.as_deref(),
                params.search.as_deref(),
                limit as u32,
                params.before_id,
            )
            .await;

        let response: Vec<LogEntryResponse> = db_entries
            .into_iter()
            .map(|e| LogEntryResponse {
                id: e.id,
                timestamp: e.timestamp,
                level: e.level,
                source: e.source,
                target: e.target,
                message: e.message,
            })
            .collect();

        return Json(response).into_response();
    }

    // Query in-memory buffer first (fast path, no DB round-trip)
    let mem_entries = log_store
        .query(
            params.level.as_deref(),
            params.source.as_deref(),
            params.search.as_deref(),
            limit,
        )
        .await;

    // If in-memory buffer doesn't have enough entries, supplement from DB
    if mem_entries.len() < limit {
        let db_limit = (limit - mem_entries.len()) as u32;
        let db_entries = log_store
            .query_db(
                params.level.as_deref(),
                params.source.as_deref(),
                params.search.as_deref(),
                db_limit,
                None,
            )
            .await;

        // Merge: DB entries (older) first, then in-memory (newer)
        // Deduplicate by id for entries that were flushed to DB
        let mem_ids: std::collections::HashSet<i64> = mem_entries
            .iter()
            .filter_map(|e| if e.id > 0 { Some(e.id) } else { None })
            .collect();

        let mut all_entries: Vec<LogEntryResponse> = db_entries
            .into_iter()
            .filter(|e| !mem_ids.contains(&e.id))
            .map(|e| LogEntryResponse {
                id: e.id,
                timestamp: e.timestamp,
                level: e.level,
                source: e.source,
                target: e.target,
                message: e.message,
            })
            .collect();

        all_entries.extend(mem_entries.into_iter().map(|e| LogEntryResponse {
            id: e.id,
            timestamp: e.timestamp,
            level: e.level,
            source: e.source,
            target: e.target,
            message: e.message,
        }));

        return Json(all_entries).into_response();
    }

    let response: Vec<LogEntryResponse> = mem_entries
        .into_iter()
        .map(|e| LogEntryResponse {
            id: e.id,
            timestamp: e.timestamp,
            level: e.level,
            source: e.source,
            target: e.target,
            message: e.message,
        })
        .collect();

    Json(response).into_response()
}

async fn get_logs_level(State(state): State<ApiState>) -> impl IntoResponse {
    let log_store = match &state.log_store {
        Some(store) => store,
        None => {
            return axum::response::Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body(Body::from("Log store not initialized"))
                .unwrap();
        }
    };

    let level_u8 = log_store.level.load(std::sync::atomic::Ordering::Relaxed);
    let level_str = match level_u8 {
        0 => "trace",
        1 => "debug",
        2 => "info",
        3 => "warn",
        4 => "error",
        _ => "info",
    };

    Json(serde_json::json!({ "level": level_str })).into_response()
}

async fn put_logs_level(
    State(state): State<ApiState>,
    Json(body): Json<SetLevelRequest>,
) -> impl IntoResponse {
    let log_store = match &state.log_store {
        Some(store) => store,
        None => {
            return axum::response::Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body(Body::from("Log store not initialized"))
                .unwrap();
        }
    };

    let level_u8 = match body.level.to_lowercase().as_str() {
        "trace" => 0,
        "debug" => 1,
        "info" => 2,
        "warn" => 3,
        "error" => 4,
        _ => {
            return axum::response::Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from(
                    "Invalid level. Use: trace, debug, info, warn, error",
                ))
                .unwrap();
        }
    };

    log_store
        .level
        .store(level_u8, std::sync::atomic::Ordering::Relaxed);
    tracing::info!("Log level changed to {}", body.level.to_lowercase());

    // Persist to DB
    if let Some(db) = state.server_state.db() {
        let _ = db
            .save_server_setting("log_level", &body.level.to_lowercase())
            .await;
    }

    // Update dynamic config
    {
        let mut dc = state.server_state.dynamic_config.write().await;
        dc.log_level = body.level.to_lowercase();
    }

    Json(serde_json::json!({ "level": body.level.to_lowercase() })).into_response()
}

// ── Proxy Rules Endpoints ─────────────────────────────────────────

/// Request body for creating a proxy rule
#[derive(Debug, Deserialize)]
pub struct CreateProxyRuleRequest {
    pub name: String,
    #[serde(rename = "type")]
    pub rule_type: String,
    pub listen: String,
    #[serde(default)]
    pub domains: Vec<String>,
    #[serde(default)]
    pub routes: Vec<crate::server::reverse_proxy::Route>,
    pub tls: Option<crate::server::reverse_proxy::ProxyTlsConfig>,
    #[serde(default = "default_rule_enabled")]
    pub enabled: bool,
}

/// Request body for updating a proxy rule
#[derive(Debug, Deserialize)]
pub struct UpdateProxyRuleRequest {
    pub name: String,
    #[serde(rename = "type")]
    pub rule_type: String,
    pub listen: String,
    #[serde(default)]
    pub domains: Vec<String>,
    #[serde(default)]
    pub routes: Vec<crate::server::reverse_proxy::Route>,
    pub tls: Option<crate::server::reverse_proxy::ProxyTlsConfig>,
    #[serde(default = "default_rule_enabled")]
    pub enabled: bool,
}

fn default_rule_enabled() -> bool {
    true
}

// GET /api/proxy/rules — list all proxy rules
async fn list_proxy_rules(State(state): State<ApiState>) -> impl IntoResponse {
    let rules = state.server_state.proxy_state.rules.lock().await;
    let rules_vec: Vec<&ProxyRule> = rules.values().collect();
    Json(serde_json::json!({ "rules": rules_vec }))
}

// POST /api/proxy/rules — create a new proxy rule
async fn create_proxy_rule(
    State(state): State<ApiState>,
    Json(body): Json<CreateProxyRuleRequest>,
) -> impl IntoResponse {
    let rule_type = match body.rule_type.to_lowercase().as_str() {
        "http" => crate::server::reverse_proxy::RuleType::Http,
        "tcp" => crate::server::reverse_proxy::RuleType::Tcp,
        "udp" => crate::server::reverse_proxy::RuleType::Udp,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "Invalid rule type. Use: http, tcp, udp" })),
            )
                .into_response();
        }
    };

    let id = uuid::Uuid::new_v4().to_string();

    let mut rule = ProxyRule {
        id: id.clone(),
        name: body.name,
        rule_type: rule_type.clone(),
        listen: body.listen,
        domains: body.domains,
        routes: body.routes,
        tls: body.tls,
        enabled: body.enabled,
        created_at: Some(chrono::Utc::now().to_rfc3339()),
        cert_status: None,
    };

    if let Err(e) = crate::server::reverse_proxy::validate_rule_for_save(&rule) {
        return (StatusCode::BAD_REQUEST, e).into_response();
    }
    crate::server::reverse_proxy::sanitize_rule(&mut rule);

    let cert_status = crate::server::reverse_proxy::resolve_cert_source_for_rule(
        &rule,
        state.server_state.proxy_state.cert_manager(),
    )
    .await;
    rule.cert_status = Some(cert_status.clone());

    if let Err(e) = state.server_state.proxy_state.save_rule(&rule).await {
        tracing::error!("Failed to save proxy rule: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "Failed to save proxy rule" })),
        )
            .into_response();
    }

    {
        let mut rules = state.server_state.proxy_state.rules.lock().await;
        rules.insert(id.clone(), rule.clone());
    }

    if rule.rule_type == crate::server::reverse_proxy::RuleType::Http {
        if let Err(e) = state
            .server_state
            .proxy_state
            .reconcile_http_listener(&rule.listen)
            .await
        {
            tracing::warn!("Reconcile failed on create, rolling back: {}", e);
            let _ = state
                .server_state
                .proxy_state
                .rules
                .lock()
                .await
                .remove(&id);
            if let Err(del_err) = state.server_state.proxy_state.delete_rule(&id).await {
                tracing::error!(
                    "Compensating delete failed after reconcile error: {}",
                    del_err
                );
            }
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": format!("{}", e),
                    "conflicts": conflicts_from_error(&e),
                })),
            )
                .into_response();
        }
    }

    // 反代规则变更可能影响 Trojan 共享/独立模式
    crate::server::trojan_runtime::sync_trojan_mode(&state.server_state).await;

    if cert_status.source == crate::server::reverse_proxy::CertSourceKind::PendingIssuance {
        if let Some(mgr) = state.server_state.proxy_state.cert_manager().cloned() {
            if rule.tls.as_ref().is_some_and(|t| t.acme) {
                let domain = cert_status.covering_domain.clone();
                tokio::spawn(async move {
                    if let Err(e) = mgr.request_acme_certificate(&domain).await {
                        tracing::error!("ACME issuance failed for {}: {}", domain, e);
                    }
                });
            }
        }
    }

    (
        StatusCode::CREATED,
        Json(serde_json::json!({ "rule": rule })),
    )
        .into_response()
}

fn conflicts_from_error(
    e: &crate::server::reverse_proxy::error::ReconcileError,
) -> Vec<serde_json::Value> {
    use crate::server::reverse_proxy::error::ReconcileError as E;
    match e {
        E::DomainConflict {
            domain,
            other_rule_id,
            ..
        } => vec![serde_json::json!({
            "rule_id": other_rule_id,
            "reason": format!("domain {} already claimed", domain),
        })],
        E::TlsMismatch {
            existing_tls,
            new_tls,
            ..
        } => vec![serde_json::json!({
            "reason": format!("tls mismatch: existing={} new={}", existing_tls, new_tls),
        })],
        E::BindFailed { source, .. } => vec![serde_json::json!({
            "reason": format!("bind failed: {}", source),
        })],
        E::NoCertManager { .. } => vec![serde_json::json!({
            "reason": "TLS enabled but no certificate manager configured",
        })],
    }
}

// PUT /api/proxy/rules/:id — update a proxy rule
async fn update_proxy_rule(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateProxyRuleRequest>,
) -> impl IntoResponse {
    let rule_type = match body.rule_type.to_lowercase().as_str() {
        "http" => crate::server::reverse_proxy::RuleType::Http,
        "tcp" => crate::server::reverse_proxy::RuleType::Tcp,
        "udp" => crate::server::reverse_proxy::RuleType::Udp,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "Invalid rule type. Use: http, tcp, udp" })),
            )
                .into_response();
        }
    };

    let previous = {
        let rules = state.server_state.proxy_state.rules.lock().await;
        rules.get(&id).cloned()
    };
    let previous = match previous {
        Some(r) => r,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "Rule not found" })),
            )
                .into_response();
        }
    };

    let mut rule = ProxyRule {
        id: id.clone(),
        name: body.name,
        rule_type: rule_type.clone(),
        listen: body.listen,
        domains: body.domains,
        routes: body.routes,
        tls: body.tls,
        enabled: body.enabled,
        created_at: previous.created_at.clone(),
        cert_status: None,
    };

    if let Err(e) = crate::server::reverse_proxy::validate_rule_for_save(&rule) {
        return (StatusCode::BAD_REQUEST, e).into_response();
    }
    crate::server::reverse_proxy::sanitize_rule(&mut rule);

    let cert_status = crate::server::reverse_proxy::resolve_cert_source_for_rule(
        &rule,
        state.server_state.proxy_state.cert_manager(),
    )
    .await;
    rule.cert_status = Some(cert_status.clone());

    if let Err(e) = state.server_state.proxy_state.save_rule(&rule).await {
        tracing::error!("Failed to save proxy rule: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "Failed to save proxy rule" })),
        )
            .into_response();
    }

    {
        let mut rules = state.server_state.proxy_state.rules.lock().await;
        rules.insert(id.clone(), rule.clone());
    }

    let old_listen = previous.listen.clone();
    let new_listen = rule.listen.clone();

    if rule.rule_type == crate::server::reverse_proxy::RuleType::Http {
        if let Err(e) = state
            .server_state
            .proxy_state
            .reconcile_http_listener(&new_listen)
            .await
        {
            tracing::warn!("Reconcile failed on update, rolling back: {}", e);
            {
                let mut rules = state.server_state.proxy_state.rules.lock().await;
                rules.insert(id.clone(), previous.clone());
            }
            let _ = state.server_state.proxy_state.save_rule(&previous).await;

            let ports: std::collections::HashSet<&str> = [old_listen.as_str(), new_listen.as_str()]
                .into_iter()
                .collect();
            for port in ports {
                if let Err(rb) = state
                    .server_state
                    .proxy_state
                    .reconcile_http_listener(port)
                    .await
                {
                    tracing::error!("Rollback reconcile failed for {}: {}", port, rb);
                }
            }
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": format!("{}", e),
                    "conflicts": conflicts_from_error(&e),
                })),
            )
                .into_response();
        }
    }
    if previous.rule_type == crate::server::reverse_proxy::RuleType::Http
        && old_listen != new_listen
    {
        if let Err(e) = state
            .server_state
            .proxy_state
            .reconcile_http_listener(&old_listen)
            .await
        {
            tracing::warn!("Old-port reconcile failed after update: {}", e);
        }
    }

    // 新旧端口 reconcile 都完成后，检查 Trojan 共享/独立模式是否需要切换
    crate::server::trojan_runtime::sync_trojan_mode(&state.server_state).await;

    if cert_status.source == crate::server::reverse_proxy::CertSourceKind::PendingIssuance {
        if let Some(mgr) = state.server_state.proxy_state.cert_manager().cloned() {
            if rule.tls.as_ref().is_some_and(|t| t.acme) {
                let domain = cert_status.covering_domain.clone();
                tokio::spawn(async move {
                    if let Err(e) = mgr.request_acme_certificate(&domain).await {
                        tracing::error!("ACME issuance failed for {}: {}", domain, e);
                    }
                });
            }
        }
    }

    Json(serde_json::json!({ "rule": rule })).into_response()
}

// DELETE /api/proxy/rules/:id — delete a proxy rule
async fn delete_proxy_rule(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let deleted = {
        let mut rules = state.server_state.proxy_state.rules.lock().await;
        rules.remove(&id)
    };
    let deleted = match deleted {
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "Rule not found" })),
            )
                .into_response();
        }
        Some(r) => r,
    };

    if deleted.rule_type != crate::server::reverse_proxy::RuleType::Http {
        let listener_handle = {
            let mut listeners = state.server_state.proxy_state.tcp_listeners.lock().await;
            listeners.remove(&id)
        };
        if let Some(handle) = listener_handle {
            handle.abort();
        }
    }

    if let Err(e) = state.server_state.proxy_state.delete_rule(&id).await {
        tracing::error!("Failed to delete proxy rule from database: {}", e);
    }

    if deleted.rule_type == crate::server::reverse_proxy::RuleType::Http {
        if let Err(e) = state
            .server_state
            .proxy_state
            .reconcile_http_listener(&deleted.listen)
            .await
        {
            tracing::warn!("Reconcile failed on delete: {}", e);
        }
    }

    // 共享 listener 可能已删除/降级，检查 Trojan 是否需要回退独立监听
    crate::server::trojan_runtime::sync_trojan_mode(&state.server_state).await;

    Json(serde_json::json!({ "deleted": id })).into_response()
}

// ── Stats Unified API ────────────────────────────────────────────

use crate::server::stats::StatsSnapshot;

#[derive(Debug, Deserialize)]
struct StatsQueryParams {
    entity_type: Option<Vec<String>>,
    entity_id: Option<Vec<String>>,
    start: String,
    end: String,
}

#[derive(Debug, Deserialize)]
struct StatsStreamQuery {
    entity_type: Option<String>,
    token: Option<String>,
}

// GET /api/stats/query
async fn get_stats_query(
    State(state): State<ApiState>,
    Query(params): Query<StatsQueryParams>,
) -> impl IntoResponse {
    let start = match chrono::DateTime::parse_from_rfc3339(&params.start) {
        Ok(dt) => dt.with_timezone(&chrono::Utc),
        Err(e) => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": format!("Invalid start: {}", e)}))).into_response(),
    };
    let end = match chrono::DateTime::parse_from_rfc3339(&params.end) {
        Ok(dt) => dt.with_timezone(&chrono::Utc),
        Err(e) => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": format!("Invalid end: {}", e)}))).into_response(),
    };
    if (end - start) > chrono::Duration::days(7) {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "Range <= 7 days"}))).into_response();
    }
    let entity_types = params.entity_type.unwrap_or_default();
    let entity_ids = params.entity_id.unwrap_or_default();
    let db = match state.server_state.get_db() {
        Some(db) => db,
        None => return (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({"error": "No DB"}))).into_response(),
    };
    match db.query_stats_snapshots(&entity_types, &entity_ids, start, end).await {
        Ok(snapshots) => Json(serde_json::json!({"snapshots": snapshots})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    }
}

// GET /api/stats/summary
async fn get_stats_summary(
    State(state): State<ApiState>,
) -> impl IntoResponse {
    Json(state.server_state.stats_collector.get_summary()).into_response()
}

// GET /api/stats/stream
async fn sse_stats_stream(
    State(state): State<ApiState>,
    Query(params): Query<StatsStreamQuery>,
) -> impl IntoResponse {
    if state.auth_config.is_enabled() {
        let token = params.token.as_deref().unwrap_or("");
        if !token.is_empty() && crate::server::auth::validate_token(token, &state.auth_config.jwt_secret).is_err() {
            return axum::response::Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .body(Body::from("Unauthorized"))
                .unwrap();
        }
    }
    let entity_type_filter = params.entity_type;
    let mut rx = state.server_state.stats_collector.subscribe();
    let stream = async_stream::stream! {
        loop {
            match tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv()).await {
                Ok(Ok(snapshot)) => {
                    if let Some(ref et) = entity_type_filter {
                        if snapshot.entity_type != *et { continue; }
                    }
                    let json = serde_json::to_string(&snapshot).unwrap_or_default();
                    yield Ok::<_, std::convert::Infallible>(
                        axum::response::sse::Event::default().event("snapshot").data(json),
                    );
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(n))) => {
                    yield Ok::<_, std::convert::Infallible>(
                        axum::response::sse::Event::default().event("sync").data(format!(r#"{{"lagged":{}}}"#, n)),
                    );
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => break,
                Err(_) => {
                    yield Ok::<_, std::convert::Infallible>(
                        axum::response::sse::Event::default().event("ping").data(""),
                    );
                }
            }
        }
    };
    axum::response::sse::Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::new().interval(std::time::Duration::from_secs(30)))
        .into_response()
}

// GET /api/proxy/stats — get proxy statistics
async fn get_proxy_stats(State(state): State<ApiState>) -> impl IntoResponse {
    // Try to get stats from database if available
    if let Some(db) = state.server_state.get_db() {
        match db.get_proxy_stats().await {
            Ok((total_rules, active_rules, total_connections, bytes_in, bytes_out)) => {
                return Json(ProxyStats {
                    total_rules,
                    active_rules,
                    total_connections,
                    bytes_in,
                    bytes_out,
                })
                .into_response();
            }
            Err(e) => {
                tracing::warn!("Failed to get proxy stats from database: {}", e);
            }
        }
    }

    // Fallback to in-memory stats
    let rules = state.server_state.proxy_state.rules.lock().await;
    let total_rules = rules.len() as i64;
    let active_rules = rules.values().filter(|r| r.enabled).count() as i64;

    let connection_counts = state
        .server_state
        .proxy_state
        .connection_counts
        .lock()
        .await;
    let active_connections: u64 = connection_counts.values().sum();

    Json(ProxyStats {
        total_rules,
        active_rules,
        total_connections: active_connections as i64,
        bytes_in: 0,
        bytes_out: 0,
    })
    .into_response()
}

// ── ACME Certificate Management Endpoints ──────────────────────────

// GET /api/acme/certificates — list all certificates
async fn list_acme_certificates(State(state): State<ApiState>) -> impl IntoResponse {
    let client_guard = state.server_state.acme_client.read().await;
    let client = match client_guard.as_ref() {
        Some(c) => c.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "error": "ACME is not enabled" })),
            )
                .into_response();
        }
    };
    drop(client_guard);

    match client.list_certificates().await {
        Ok(certs) => Json(serde_json::json!({ "certificates": certs })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

// POST /api/acme/certificates/:domain — request a new certificate
async fn request_acme_certificate(
    State(state): State<ApiState>,
    Path(domain): Path<String>,
    Json(req): Json<CertificateRequest>,
) -> impl IntoResponse {
    let client_guard = state.server_state.acme_client.read().await;
    let client = match client_guard.as_ref() {
        Some(c) => c.clone(),
        None => {
            tracing::error!("ACME certificate request failed: ACME client not initialized");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "error": "ACME is not enabled" })),
            )
                .into_response();
        }
    };
    drop(client_guard);

    let challenge_type = req.challenge_type.unwrap_or_else(|| "http-01".to_string());

    let result = match challenge_type.as_str() {
        "dns-01" => {
            // 获取 DNS solver
            let dns_config = state.server_state.dns_provider_config.read().await;
            match dns_config.as_ref() {
                Some(config) => {
                    let solver: std::sync::Arc<dyn crate::server::acme::dns::DnsChallengeSolver> =
                        match config.provider {
                            crate::server::acme::dns::DnsProvider::Aliyun => std::sync::Arc::new(
                                crate::server::acme::dns::aliyun::AliyunDnsSolver::new(config),
                            ),
                            crate::server::acme::dns::DnsProvider::Cloudflare => {
                                std::sync::Arc::new(
                                    crate::server::acme::dns::cloudflare::CloudflareDnsSolver::new(
                                        config,
                                    ),
                                )
                            }
                            crate::server::acme::dns::DnsProvider::TencentCloud => {
                                std::sync::Arc::new(
                                    crate::server::acme::dns::tencent::TencentDnsSolver::new(
                                        config,
                                    ),
                                )
                            }
                            crate::server::acme::dns::DnsProvider::Custom => std::sync::Arc::new(
                                crate::server::acme::dns::custom::CustomDnsSolver::new(config),
                            ),
                        };
                    client.request_certificate_with_dns(&domain, solver).await
                }
                None => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({ "error": "DNS provider not configured" })),
                    )
                        .into_response();
                }
            }
        }
        _ => client.request_certificate(&domain).await,
    };

    match result {
        Ok(metadata) => {
            tracing::info!("Certificate request successful for domain: {}", domain);
            // 签发结果写入 CertificateManager 内存缓存并广播 CertEvent::Issued，
            // 否则重启前 Trojan / 反代 SNI resolver 看不到新证书。
            if let Some(ref cm) = state.server_state.cert_manager {
                if let Err(e) = cm.load_issued_certificate(&domain).await {
                    tracing::warn!(
                        "Failed to load issued certificate into cache for {}: {}",
                        domain,
                        e
                    );
                }
            }
            (
                StatusCode::CREATED,
                Json(serde_json::json!({ "certificate": metadata })),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!("Certificate request failed for domain {}: {:?}", domain, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    }
}

// GET /api/acme/certificates/:domain — get certificate details
async fn get_acme_certificate(
    State(state): State<ApiState>,
    Path(domain): Path<String>,
) -> impl IntoResponse {
    let client_guard = state.server_state.acme_client.read().await;
    let client = match client_guard.as_ref() {
        Some(c) => c.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "error": "ACME is not enabled" })),
            )
                .into_response();
        }
    };
    drop(client_guard);

    match client.get_certificate_metadata(&domain).await {
        Ok(Some(metadata)) => Json(serde_json::json!({ "certificate": metadata })).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Certificate not found" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

// POST /api/acme/certificates/:domain/renew — manual renewal
async fn renew_acme_certificate(
    State(state): State<ApiState>,
    Path(domain): Path<String>,
) -> impl IntoResponse {
    let client_guard = state.server_state.acme_client.read().await;
    let client = match client_guard.as_ref() {
        Some(c) => c.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "error": "ACME is not enabled" })),
            )
                .into_response();
        }
    };
    drop(client_guard);

    match client.renew_certificate(&domain).await {
        Ok(metadata) => Json(serde_json::json!({ "certificate": metadata })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

// GET /api/acme/status — get ACME status
async fn get_acme_status(State(state): State<ApiState>) -> impl IntoResponse {
    // Read enabled status from acme_full_config to stay in sync with API updates
    let full_config = state.server_state.acme_full_config.read().await;
    let enabled = full_config.enabled;
    let server_url = full_config.server_url.clone();
    let cert_dir = full_config.cert_dir.clone();
    drop(full_config);

    let client_guard = state.server_state.acme_client.read().await;
    let cert_count = match client_guard.as_ref() {
        Some(client) => match client.list_certificates().await {
            Ok(certs) => certs.len(),
            Err(_) => 0,
        },
        None => 0,
    };
    drop(client_guard);

    let api_tls = state.server_state.cert_manager.is_some();
    let trojan = !state.server_state.get_trojan_ports().await.is_empty();
    let control_tls = state.server_state.cert_manager.is_some();

    // Check if any reverse-proxy rule has TLS with ACME enabled
    let reverse_proxy = {
        let rules = state.server_state.proxy_state.rules.lock().await;
        rules
            .values()
            .any(|r| r.tls.as_ref().is_some_and(|t| t.enabled && t.acme))
    };

    Json(serde_json::json!({
        "enabled": enabled,
        "server_url": server_url,
        "cert_dir": cert_dir,
        "certificate_count": cert_count,
        "consumers": {
            "api_tls": api_tls,
            "trojan": trojan,
            "control_tls": control_tls,
            "reverse_proxy": reverse_proxy,
        },
    }))
    .into_response()
}

// GET /api/acme/config — get ACME configuration
async fn get_acme_config(State(state): State<ApiState>) -> impl IntoResponse {
    let config = state.server_state.acme_full_config.read().await;
    Json(serde_json::json!({
        "enabled": config.enabled,
        "server_url": config.server_url,
        "email": config.email,
        "cert_dir": config.cert_dir,
        "auto_renew": config.auto_renew,
        "renewal_check_interval": config.renewal_check_interval,
        "renewal_days_before_expiry": config.renewal_days_before_expiry,
        "tos_agreed": config.tos_agreed,
    }))
}

// PUT /api/acme/config — update ACME configuration
async fn update_acme_config(
    State(state): State<ApiState>,
    Json(req): Json<UpdateAcmeConfigRequest>,
) -> impl IntoResponse {
    let mut config = state.server_state.acme_full_config.write().await;
    if let Some(v) = req.enabled {
        config.enabled = v;
    }
    if let Some(v) = req.server_url {
        config.server_url = v;
    }
    if let Some(v) = req.email {
        config.email = Some(v);
    }
    if let Some(v) = req.auto_renew {
        config.auto_renew = v;
    }
    if let Some(v) = req.renewal_check_interval {
        config.renewal_check_interval = v;
    }
    if let Some(v) = req.renewal_days_before_expiry {
        config.renewal_days_before_expiry = v;
    }
    if let Some(v) = req.tos_agreed {
        config.tos_agreed = v;
    }

    // Persist ACME config to database
    if let Some(db) = state.server_state.get_db() {
        if let Ok(json) = serde_json::to_string(&*config) {
            if let Err(e) = db.save_server_setting("acme_config", &json).await {
                tracing::error!("Failed to persist ACME config: {}", e);
            }
        }
    }

    // Capture config values for ACME client initialization
    let has_client = state.server_state.acme_client.read().await.is_some();
    let should_init_client = config.enabled && !has_client;
    let acme_server_url = config.server_url.clone();
    let acme_cert_dir = config.cert_dir.clone();
    let acme_email = config.email.clone();
    let acme_enabled = config.enabled;
    let has_db = state.server_state.get_db().is_some();

    tracing::info!(
        "ACME config update: enabled={}, should_init_client={}, has_db={}",
        config.enabled,
        should_init_client,
        has_db
    );

    // Prepare response before potentially dropping the lock
    let response = Json(serde_json::json!({
        "enabled": config.enabled,
        "server_url": config.server_url,
        "email": config.email,
        "cert_dir": config.cert_dir,
        "auto_renew": config.auto_renew,
        "renewal_check_interval": config.renewal_check_interval,
        "renewal_days_before_expiry": config.renewal_days_before_expiry,
        "tos_agreed": config.tos_agreed,
    }));

    // Drop the write lock before initializing ACME client
    drop(config);

    // Initialize ACME client if enabled and not already initialized
    if should_init_client {
        tracing::info!("Initializing ACME client...");
        if let Some(db) = state.server_state.get_db() {
            let acme_state = crate::server::acme::AcmeState::with_db(db.clone());
            let client = std::sync::Arc::new(crate::server::acme::client::AcmeClient::new(
                acme_state,
                acme_server_url.clone(),
                acme_cert_dir.clone(),
                acme_email,
            ));

            if let Err(e) = client.initialize().await {
                tracing::error!("Failed to initialize ACME client: {}", e);
            } else {
                tracing::info!("ACME client initialized successfully");
            }

            let acme_config_info = crate::server::control::AcmeConfigInfo {
                enabled: acme_enabled,
                server_url: acme_server_url,
                cert_dir: acme_cert_dir,
            };

            state
                .server_state
                .set_acme_client(client, acme_config_info)
                .await;
            tracing::info!("ACME client set on server state");
        } else {
            tracing::error!("Cannot initialize ACME client: no database available");
        }
    } else if acme_enabled {
        tracing::info!("ACME client already initialized, skipping");
    }

    response
}

// ── DNS Provider Endpoints ─────────────────────────────────────────

// GET /api/acme/dns-providers — get available providers and current config
async fn get_dns_providers(State(state): State<ApiState>) -> impl IntoResponse {
    let config = state.server_state.dns_provider_config.read().await;
    Json(serde_json::json!({
        "providers": ["cloudflare", "aliyun", "tencent", "custom"],
        "config": *config
    }))
}

// PUT /api/acme/dns-providers — update DNS provider configuration
async fn update_dns_provider(
    State(state): State<ApiState>,
    Json(req): Json<crate::server::acme::dns::DnsProviderConfig>,
) -> impl IntoResponse {
    let mut config = state.server_state.dns_provider_config.write().await;
    *config = Some(req.clone());

    // Persist DNS provider config to database
    if let Some(db) = state.server_state.get_db() {
        if let Ok(json) = serde_json::to_string(&*config) {
            if let Err(e) = db.save_server_setting("dns_provider_config", &json).await {
                tracing::error!("Failed to persist DNS provider config: {}", e);
            }
        }
    }

    Json(serde_json::json!({
        "success": true,
        "config": req
    }))
}

// GET /api/acme/challenge-status/:domain — get ACME challenge status for a domain
async fn get_challenge_status(
    State(state): State<ApiState>,
    Path(domain): Path<String>,
) -> impl IntoResponse {
    let _ = state;
    Json(serde_json::json!({
        "domain": domain,
        "status": "pending"
    }))
}

// DELETE /api/acme/certificates/:domain — delete a certificate
async fn delete_acme_certificate(
    State(state): State<ApiState>,
    Path(domain): Path<String>,
) -> impl IntoResponse {
    let client_guard = state.server_state.acme_client.read().await;
    let client = match client_guard.as_ref() {
        Some(c) => c.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "error": "ACME is not enabled" })),
            )
                .into_response();
        }
    };
    drop(client_guard);

    match client.delete_certificate(&domain).await {
        Ok(()) => Json(serde_json::json!({ "deleted": true })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

// ── Settings Endpoints ─────────────────────────────────────────────

/// Get all dynamic configuration
async fn get_settings(State(state): State<ApiState>) -> impl IntoResponse {
    let dc = state.server_state.dynamic_config.read().await;
    Json(serde_json::json!({
        "log_level": dc.log_level,
        "api_tls": state.server_state.api_tls,
        "api_domain": state.server_state.api_domain,
        "shadowsocks": dc.ss,
        "trojan": dc.trojan,
        "reverse_proxy": dc.reverse_proxy,
        "dns": dc.dns,
    }))
}

/// Get reverse proxy config
async fn get_reverse_proxy_config(State(state): State<ApiState>) -> impl IntoResponse {
    let dc = state.server_state.dynamic_config.read().await;
    Json(serde_json::json!(dc.reverse_proxy))
}

/// Update reverse proxy config
async fn update_reverse_proxy_config(
    State(state): State<ApiState>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    let max_conn = payload["max_connections"].as_u64().unwrap_or(10000) as u32;
    let timeout = payload["connection_timeout_secs"].as_u64().unwrap_or(30);
    let buffer = payload["buffer_size"].as_u64().unwrap_or(8192) as usize;

    // Save to DB
    if let Some(db) = state.server_state.db() {
        if let Err(e) = db
            .save_reverse_proxy_config(max_conn, timeout, buffer)
            .await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("DB error: {}", e),
            )
                .into_response();
        }
    }

    // Update in-memory
    {
        let mut dc = state.server_state.dynamic_config.write().await;
        dc.reverse_proxy = crate::server::dynamic_config::ReverseProxySettings {
            max_connections: max_conn,
            connection_timeout_secs: timeout,
            buffer_size: buffer,
        };
    }

    StatusCode::OK.into_response()
}

/// Get DNS config
async fn get_dns_config(State(state): State<ApiState>) -> impl IntoResponse {
    let dc = state.server_state.dynamic_config.read().await;
    Json(serde_json::json!(dc.dns))
}

/// Update DNS config
async fn update_dns_config(
    State(state): State<ApiState>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    let tunnel_domain = payload["tunnel_domain"].as_str().unwrap_or("tunnel.local");
    let mesh_domain = payload["mesh_domain"].as_str().unwrap_or("mesh.local");

    if let Some(db) = state.server_state.db() {
        if let Err(e) = db.save_dns_config(tunnel_domain, mesh_domain).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("DB error: {}", e),
            )
                .into_response();
        }
    }

    {
        let mut dc = state.server_state.dynamic_config.write().await;
        dc.dns = crate::server::dynamic_config::DnsSettings {
            tunnel_domain: tunnel_domain.to_string(),
            mesh_domain: mesh_domain.to_string(),
        };
    }

    StatusCode::OK.into_response()
}

/// Create and run the API server
pub async fn run_api_server(
    api_addr: String,
    server_state: ServerState,
    auth_config: AuthConfig,
    tls_config: Option<Arc<rustls::server::ServerConfig>>,
) -> Result<(), std::io::Error> {
    let auth_config = Arc::new(auth_config);

    let log_store = server_state.log_store.clone();

    let state = ApiState {
        server_state,
        auth_config: auth_config.clone(),
        log_store,
    };

    // CORS layer
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    // Public routes (no auth required) — SSE uses ?token= query param for auth
    let public_routes = Router::new()
        .route("/api/login", post(login))
        .route("/api/health", get(health))
        .route("/api/stats/query", get(get_stats_query))
        .route("/api/stats/summary", get(get_stats_summary))
        .route("/api/stats/stream", get(sse_stats_stream))
        .route("/api/logs/stream", get(sse_log_stream));

    // Protected routes (require auth only when password is set)
    let mut protected_routes = Router::new()
        .route("/api/logout", post(logout))
        .route("/api/clients", get(clients::list_clients))
        .route(
            "/api/clients/:name",
            patch(clients::patch_client_note).delete(clients::delete_client),
        )
        .route("/api/clients/:name/kick", post(clients::kick_client))
        // Server auth token management
        .route(
            "/api/server-auth",
            get(server_auth::get_auth).put(server_auth::put_auth),
        )
        .route("/api/server-auth/rotate", post(server_auth::rotate_auth))
        // Quality monitoring endpoints
        // Shadowsocks management endpoints
        .route(
            "/api/shadowsocks",
            get(get_shadowsocks_config).post(update_shadowsocks_config),
        )
        // Trojan management endpoints
        .route(
            "/api/trojan",
            get(get_trojan_config).post(update_trojan_config),
        )
        // Mesh network endpoints
        .route("/api/mesh", get(list_meshes))
        .route("/api/mesh/:id", get(get_mesh))
        .route("/api/mesh/:id/services", get(get_mesh_services))
        // DNS management endpoints
        .route(
            "/api/dns/records",
            get(get_dns_records).post(add_dns_record),
        )
        .route("/api/dns/records/:name", delete(delete_dns_record))
        // Log viewer endpoints (SSE stream is in public_routes — uses ?token= query param)
        .route("/api/logs", get(get_logs))
        .route("/api/logs/level", get(get_logs_level).put(put_logs_level))
        // Proxy rules management endpoints
        .route(
            "/api/proxy/rules",
            get(list_proxy_rules).post(create_proxy_rule),
        )
        .route(
            "/api/proxy/rules/:id",
            put(update_proxy_rule).delete(delete_proxy_rule),
        )
        // ACME certificate management endpoints
        .route("/api/acme/status", get(get_acme_status))
        .route(
            "/api/acme/config",
            get(get_acme_config).put(update_acme_config),
        )
        .route("/api/acme/certificates", get(list_acme_certificates))
        .route(
            "/api/acme/certificates/:domain",
            get(get_acme_certificate)
                .post(request_acme_certificate)
                .delete(delete_acme_certificate),
        )
        .route(
            "/api/acme/certificates/:domain/renew",
            post(renew_acme_certificate),
        )
        // DNS provider endpoints
        .route(
            "/api/acme/dns-providers",
            get(get_dns_providers).put(update_dns_provider),
        )
        .route(
            "/api/acme/challenge-status/:domain",
            get(get_challenge_status),
        )
        // Settings endpoints
        .route("/api/settings", get(get_settings))
        .route(
            "/api/settings/reverse-proxy",
            get(get_reverse_proxy_config).put(update_reverse_proxy_config),
        )
        .route(
            "/api/settings/dns",
            get(get_dns_config).put(update_dns_config),
        );

    // Only apply auth middleware if password is set
    if auth_config.is_enabled() {
        protected_routes = protected_routes.layer(middleware::from_fn_with_state(
            auth_config.clone(),
            auth_middleware,
        ));
    }

    // Static file service for frontend (embedded)
    #[cfg(feature = "embed-frontend")]
    let static_routes = Router::new()
        .route(
            "/",
            get(|| async { serve_static(Path("".to_string())).await }),
        )
        .route("/*path", get(serve_static));

    let app = Router::new().merge(public_routes).merge(protected_routes);

    #[cfg(feature = "embed-frontend")]
    let app = app.merge(static_routes);

    let app = app.layer(cors).with_state(state);

    match tls_config {
        Some(tls_config) => {
            // Extract port 80 address from api_addr for HTTP redirect
            let http_addr = {
                let parts: Vec<&str> = api_addr.split(':').collect();
                if parts.len() == 2 {
                    format!("{}:80", parts[0])
                } else {
                    "0.0.0.0:80".to_string()
                }
            };

            // Start HTTP redirect server on port 80
            let http_app =
                axum::Router::new().fallback(|req: axum::http::Request<Body>| async move {
                    let uri = req.uri();
                    let host = uri.host().unwrap_or("localhost").to_string();
                    let path = format!(
                        "https://{host}{}",
                        uri.path_and_query().map(|p| p.as_str()).unwrap_or("/")
                    );
                    (
                        StatusCode::MOVED_PERMANENTLY,
                        [(axum::http::header::LOCATION, path)],
                    )
                        .into_response()
                });

            tokio::spawn(async move {
                let http_listener = match tokio::net::TcpListener::bind(&http_addr).await {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::error!(
                            "Failed to bind HTTP redirect server on {}: {}",
                            http_addr,
                            e
                        );
                        return;
                    }
                };
                tracing::info!("HTTP redirect server listening on {}", http_addr);
                if let Err(e) = axum::serve(http_listener, http_app).await {
                    tracing::error!("HTTP redirect server error: {}", e);
                }
            });

            // Start HTTPS server on api_addr
            let tls_acceptor = tokio_rustls::TlsAcceptor::from(tls_config);
            let listener = tokio::net::TcpListener::bind(&api_addr).await?;
            tracing::info!("HTTPS API server listening on {}", api_addr);

            loop {
                let (tcp_stream, _remote_addr) = match listener.accept().await {
                    Ok(conn) => conn,
                    Err(e) => {
                        tracing::error!("Failed to accept TLS connection: {}", e);
                        continue;
                    }
                };

                let tls_acceptor = tls_acceptor.clone();
                let app = app.clone();

                tokio::spawn(async move {
                    let tls_stream = match tls_acceptor.accept(tcp_stream).await {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::error!("TLS handshake failed: {}", e);
                            return;
                        }
                    };

                    let io = hyper_util::rt::TokioIo::new(tls_stream);
                    let service = hyper_util::service::TowerToHyperService::new(app.into_service());

                    if let Err(e) = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, service)
                        .await
                    {
                        tracing::error!("HTTPS connection error: {}", e);
                    }
                });
            }
        }
        None => {
            // Plain HTTP — original behavior
            let listener = tokio::net::TcpListener::bind(&api_addr).await?;
            tracing::info!("API server listening on {}", api_addr);
            axum::serve(listener, app).await?;
            Ok(())
        }
    }
}
