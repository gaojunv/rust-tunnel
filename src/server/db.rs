use chrono::{DateTime, Duration, Utc};
use sqlx::{sqlite::{SqliteConnectOptions, SqlitePoolOptions}, FromRow, Row, Sqlite, Pool};
use std::path::Path;
use std::str::FromStr;
use crate::server::quality::QualitySample;

/// Database wrapper for persistence
#[derive(Clone)]
pub struct Database {
    pool: Pool<Sqlite>,
}

impl Database {
    /// Create a new database instance and initialize schema
    pub async fn new<P: AsRef<Path>>(path: P) -> Result<Self, sqlx::Error> {
        let path = path.as_ref();

        // Create parent directory if it doesn't exist
        if let Some(parent) = path.parent() {
            if !parent.exists() && parent != Path::new("") {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    return Err(sqlx::Error::Io(e));
                }
            }
        }

        // Use SqliteConnectOptions instead of URL to avoid issues
        let options = if path == Path::new(":memory:") {
            SqliteConnectOptions::from_str("sqlite::memory:")?
        } else {
            SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(true)
        };

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;

        // Initialize schema
        Self::initialize_schema(&pool).await?;

        Ok(Self { pool })
    }

    /// Initialize database tables
    async fn initialize_schema(pool: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
        // Port traffic aggregate table
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS port_traffic (
                port INTEGER PRIMARY KEY,
                total_bytes_in BIGINT NOT NULL DEFAULT 0,
                total_bytes_out BIGINT NOT NULL DEFAULT 0,
                created_at DATETIME NOT NULL,
                updated_at DATETIME NOT NULL
            )
            "#,
        )
        .execute(pool)
        .await?;

        // Traffic buckets (minute-level)
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS traffic_buckets (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                port INTEGER NOT NULL,
                timestamp DATETIME NOT NULL,
                bytes_in BIGINT NOT NULL DEFAULT 0,
                bytes_out BIGINT NOT NULL DEFAULT 0,
                UNIQUE(port, timestamp)
            )
            "#,
        )
        .execute(pool)
        .await?;

        // Client session history
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS client_sessions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                port INTEGER NOT NULL,
                hostname TEXT,
                connected_at DATETIME NOT NULL,
                disconnected_at DATETIME,
                duration_seconds INTEGER
            )
            "#,
        )
        .execute(pool)
        .await?;

        // Connection quality history
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS connection_quality_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                port INTEGER NOT NULL,
                timestamp TEXT NOT NULL,
                avg_rtt_ms REAL NOT NULL,
                min_rtt_ms REAL NOT NULL,
                max_rtt_ms REAL NOT NULL,
                loss_rate REAL NOT NULL,
                bytes_in_per_sec REAL NOT NULL,
                bytes_out_per_sec REAL NOT NULL,
                quality_score INTEGER NOT NULL,
                is_warning INTEGER NOT NULL,
                is_critical INTEGER NOT NULL
            )
            "#,
        )
        .execute(pool)
        .await?;

        // Create indexes
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_traffic_buckets_port ON traffic_buckets(port)")
            .execute(pool)
            .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_traffic_buckets_timestamp ON traffic_buckets(timestamp)",
        )
        .execute(pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_client_sessions_port ON client_sessions(port)")
            .execute(pool)
            .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_quality_port ON connection_quality_history(port)")
            .execute(pool)
            .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_quality_timestamp ON connection_quality_history(timestamp)",
        )
        .execute(pool)
        .await?;

        // Shadowsocks configuration table
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS shadowsocks_config (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                port INTEGER NOT NULL UNIQUE,
                cipher TEXT NOT NULL,
                password TEXT NOT NULL,
                enabled INTEGER NOT NULL DEFAULT 1,
                created_at DATETIME NOT NULL,
                updated_at DATETIME NOT NULL
            )
            "#,
        )
        .execute(pool)
        .await?;

        Ok(())
    }

    /// Update or insert port traffic aggregate
    pub async fn upsert_port_traffic(
        &self,
        port: u16,
        bytes_in: u64,
        bytes_out: u64,
    ) -> Result<(), sqlx::Error> {
        let now = Utc::now();

        sqlx::query(
            r#"
            INSERT INTO port_traffic (port, total_bytes_in, total_bytes_out, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?)
            ON CONFLICT(port) DO UPDATE SET
                total_bytes_in = total_bytes_in + excluded.total_bytes_in,
                total_bytes_out = total_bytes_out + excluded.total_bytes_out,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(port as i32)
        .bind(bytes_in as i64)
        .bind(bytes_out as i64)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Update or insert traffic bucket
    pub async fn upsert_traffic_bucket(
        &self,
        port: u16,
        timestamp: DateTime<Utc>,
        bytes_in: u64,
        bytes_out: u64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO traffic_buckets (port, timestamp, bytes_in, bytes_out)
            VALUES (?, ?, ?, ?)
            ON CONFLICT(port, timestamp) DO UPDATE SET
                bytes_in = bytes_in + excluded.bytes_in,
                bytes_out = bytes_out + excluded.bytes_out
            "#,
        )
        .bind(port as i32)
        .bind(timestamp)
        .bind(bytes_in as i64)
        .bind(bytes_out as i64)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Record client connection
    pub async fn record_client_connect(&self, port: u16, hostname: Option<String>) -> Result<(), sqlx::Error> {
        let now = Utc::now();

        sqlx::query(
            r#"
            INSERT INTO client_sessions (port, hostname, connected_at, disconnected_at, duration_seconds)
            VALUES (?, ?, ?, NULL, NULL)
            "#,
        )
        .bind(port as i32)
        .bind(hostname)
        .bind(now)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Record client disconnection
    pub async fn record_client_disconnect(&self, port: u16) -> Result<(), sqlx::Error> {
        let now = Utc::now();

        // Update the most recent session for this port that's still connected
        // Use a subquery to find the latest session since SQLite < 3.33 doesn't support
        // UPDATE ... ORDER BY ... LIMIT
        sqlx::query(
            r#"
            UPDATE client_sessions
            SET disconnected_at = ?,
                duration_seconds = CAST(strftime('%s', ?) AS INTEGER) - CAST(strftime('%s', connected_at) AS INTEGER)
            WHERE id = (
                SELECT id FROM client_sessions
                WHERE port = ? AND disconnected_at IS NULL
                ORDER BY connected_at DESC
                LIMIT 1
            )
            "#,
        )
        .bind(now)
        .bind(now)
        .bind(port as i32)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Load all port traffic aggregates
    pub async fn load_port_traffic(&self) -> Result<Vec<PortTrafficRecord>, sqlx::Error> {
        let records = sqlx::query_as::<_, PortTrafficRecord>(
            r#"
            SELECT port, total_bytes_in, total_bytes_out, created_at, updated_at
            FROM port_traffic
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(records)
    }

    /// Load recent buckets (last N hours)
    pub async fn load_recent_buckets(&self, hours: u32) -> Result<Vec<TrafficBucketRecord>, sqlx::Error> {
        let since = Utc::now() - Duration::hours(hours as i64);

        let records = sqlx::query_as::<_, TrafficBucketRecord>(
            r#"
            SELECT port, timestamp, bytes_in, bytes_out
            FROM traffic_buckets
            WHERE timestamp >= ?
            ORDER BY port, timestamp
            "#,
        )
        .bind(since)
        .fetch_all(&self.pool)
        .await?;

        Ok(records)
    }

    /// Get aggregated traffic for a specific port
    pub async fn get_port_aggregates(&self, port: u16) -> Result<Option<(u64, u64)>, sqlx::Error> {
        #[derive(FromRow)]
        struct AggregateRow {
            total_bytes_in: i64,
            total_bytes_out: i64,
        }

        let record = sqlx::query_as::<_, AggregateRow>(
            r#"
            SELECT total_bytes_in, total_bytes_out
            FROM port_traffic
            WHERE port = ?
            "#,
        )
        .bind(port as i32)
        .fetch_optional(&self.pool)
        .await?;

        Ok(record.map(|r| (r.total_bytes_in as u64, r.total_bytes_out as u64)))
    }

    /// Insert quality history record
    pub async fn insert_quality_history(
        &self,
        port: u16,
        timestamp: DateTime<Utc>,
        avg_rtt_ms: f32,
        min_rtt_ms: f32,
        max_rtt_ms: f32,
        loss_rate: f32,
        bytes_in_per_sec: f64,
        bytes_out_per_sec: f64,
        quality_score: u8,
        is_warning: bool,
        is_critical: bool,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO connection_quality_history (
                port, timestamp, avg_rtt_ms, min_rtt_ms, max_rtt_ms,
                loss_rate, bytes_in_per_sec, bytes_out_per_sec,
                quality_score, is_warning, is_critical
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(port as i32)
        .bind(timestamp.to_rfc3339())
        .bind(avg_rtt_ms)
        .bind(min_rtt_ms)
        .bind(max_rtt_ms)
        .bind(loss_rate)
        .bind(bytes_in_per_sec)
        .bind(bytes_out_per_sec)
        .bind(quality_score as i32)
        .bind(is_warning as i32)
        .bind(is_critical as i32)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Get quality history for a port within time range
    pub async fn get_quality_history(
        &self,
        port: u16,
        start_time: DateTime<Utc>,
        end_time: DateTime<Utc>,
    ) -> Result<Vec<QualitySample>, sqlx::Error> {
        let rows = sqlx::query_as::<_, QualityHistoryRow>(
            r#"
            SELECT timestamp, avg_rtt_ms, loss_rate,
                   bytes_in_per_sec, bytes_out_per_sec, quality_score
            FROM connection_quality_history
            WHERE port = ? AND timestamp >= ? AND timestamp <= ?
            ORDER BY timestamp ASC
            "#,
        )
        .bind(port as i32)
        .bind(start_time.to_rfc3339())
        .bind(end_time.to_rfc3339())
        .fetch_all(&self.pool)
        .await?;

        let samples = rows
            .into_iter()
            .filter_map(|row| {
                DateTime::parse_from_rfc3339(&row.timestamp)
                    .ok()
                    .map(|dt| QualitySample {
                        timestamp: dt.with_timezone(&Utc),
                        avg_rtt_ms: row.avg_rtt_ms,
                        loss_rate: row.loss_rate,
                        bytes_in_per_sec: row.bytes_in_per_sec,
                        bytes_out_per_sec: row.bytes_out_per_sec,
                        quality_score: row.quality_score as u8,
                    })
            })
            .collect();

        Ok(samples)
    }

    /// Clean up old quality history records older than the given time
    pub async fn cleanup_old_quality_history(
        &self,
        older_than: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            DELETE FROM connection_quality_history
            WHERE timestamp < ?
            "#,
        )
        .bind(older_than.to_rfc3339())
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Get all distinct ports that have quality history within the last N hours
    pub async fn get_quality_ports(&self, hours: u32) -> Result<Vec<u16>, sqlx::Error> {
        let since = Utc::now() - Duration::hours(hours as i64);

        let rows = sqlx::query(
            r#"
            SELECT DISTINCT port FROM connection_quality_history
            WHERE timestamp >= ?
            "#,
        )
        .bind(since.to_rfc3339())
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.iter().map(|row| row.get::<i32, _>("port") as u16).collect())
    }

    /// Save or update Shadowsocks configuration
    pub async fn save_shadowsocks_config(
        &self,
        port: u16,
        cipher: &str,
        password: &str,
        enabled: bool,
    ) -> Result<(), sqlx::Error> {
        let now = Utc::now();

        sqlx::query(
            r#"
            INSERT INTO shadowsocks_config (port, cipher, password, enabled, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?)
            ON CONFLICT(port) DO UPDATE SET
                cipher = excluded.cipher,
                password = excluded.password,
                enabled = excluded.enabled,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(port as i32)
        .bind(cipher)
        .bind(password)
        .bind(enabled as i32)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Load all Shadowsocks configurations
    pub async fn load_shadowsocks_configs(&self) -> Result<Vec<ShadowsocksConfigRecord>, sqlx::Error> {
        let records = sqlx::query_as::<_, ShadowsocksConfigRecord>(
            r#"
            SELECT id, port, cipher, password, enabled, created_at, updated_at
            FROM shadowsocks_config
            ORDER BY port
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(records)
    }

    /// Load enabled Shadowsocks configurations
    pub async fn load_enabled_shadowsocks_configs(&self) -> Result<Vec<ShadowsocksConfigRecord>, sqlx::Error> {
        let records = sqlx::query_as::<_, ShadowsocksConfigRecord>(
            r#"
            SELECT id, port, cipher, password, enabled, created_at, updated_at
            FROM shadowsocks_config
            WHERE enabled = 1
            ORDER BY port
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(records)
    }

    /// Get Shadowsocks config for a specific port
    pub async fn get_shadowsocks_config(&self, port: u16) -> Result<Option<ShadowsocksConfigRecord>, sqlx::Error> {
        let record = sqlx::query_as::<_, ShadowsocksConfigRecord>(
            r#"
            SELECT id, port, cipher, password, enabled, created_at, updated_at
            FROM shadowsocks_config
            WHERE port = ?
            "#,
        )
        .bind(port as i32)
        .fetch_optional(&self.pool)
        .await?;

        Ok(record)
    }

    /// Delete Shadowsocks configuration
    pub async fn delete_shadowsocks_config(&self, port: u16) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            DELETE FROM shadowsocks_config
            WHERE port = ?
            "#,
        )
        .bind(port as i32)
        .execute(&self.pool)
        .await?;

        Ok(())
    }
}

/// Port traffic record from database
#[derive(FromRow)]
pub struct PortTrafficRecord {
    pub port: i32,
    pub total_bytes_in: i64,
    pub total_bytes_out: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Traffic bucket record from database
#[derive(FromRow)]
pub struct TrafficBucketRecord {
    pub port: i32,
    pub timestamp: DateTime<Utc>,
    pub bytes_in: i64,
    pub bytes_out: i64,
}

/// Quality history record from database
#[derive(FromRow)]
struct QualityHistoryRow {
    pub timestamp: String,
    pub avg_rtt_ms: f32,
    pub loss_rate: f32,
    pub bytes_in_per_sec: f64,
    pub bytes_out_per_sec: f64,
    pub quality_score: i32,
}

/// Shadowsocks config record from database
#[derive(FromRow, Debug)]
pub struct ShadowsocksConfigRecord {
    pub id: i32,
    pub port: i32,
    pub cipher: String,
    pub password: String,
    pub enabled: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn create_test_db() -> Database {
        Database::new(":memory:").await.expect("Failed to create in-memory database")
    }

    #[tokio::test]
    async fn test_database_new() {
        let db = create_test_db().await;
        // Verify it works by doing a simple operation
        db.upsert_port_traffic(8080, 0, 0).await.unwrap();
    }

    #[tokio::test]
    async fn test_upsert_port_traffic_insert() {
        let db = create_test_db().await;

        db.upsert_port_traffic(8080, 100, 200).await.unwrap();

        let records = db.load_port_traffic().await.unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].port, 8080);
        assert_eq!(records[0].total_bytes_in, 100);
        assert_eq!(records[0].total_bytes_out, 200);
    }

    #[tokio::test]
    async fn test_upsert_port_traffic_update() {
        let db = create_test_db().await;

        db.upsert_port_traffic(8080, 100, 200).await.unwrap();
        db.upsert_port_traffic(8080, 50, 75).await.unwrap();

        let records = db.load_port_traffic().await.unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].total_bytes_in, 150); // 100 + 50
        assert_eq!(records[0].total_bytes_out, 275); // 200 + 75
    }

    #[tokio::test]
    async fn test_upsert_port_traffic_multiple_ports() {
        let db = create_test_db().await;

        db.upsert_port_traffic(8080, 100, 200).await.unwrap();
        db.upsert_port_traffic(9000, 300, 400).await.unwrap();

        let records = db.load_port_traffic().await.unwrap();
        assert_eq!(records.len(), 2);
    }

    #[tokio::test]
    async fn test_get_port_aggregates() {
        let db = create_test_db().await;

        db.upsert_port_traffic(8080, 100, 200).await.unwrap();

        let result = db.get_port_aggregates(8080).await.unwrap();
        assert!(result.is_some());
        let (bytes_in, bytes_out) = result.unwrap();
        assert_eq!(bytes_in, 100);
        assert_eq!(bytes_out, 200);

        let result = db.get_port_aggregates(9999).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_upsert_traffic_bucket() {
        let db = create_test_db().await;

        let ts = Utc::now();
        db.upsert_traffic_bucket(8080, ts, 100, 200).await.unwrap();

        let records = db.load_recent_buckets(24).await.unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].port, 8080);
        assert_eq!(records[0].bytes_in, 100);
        assert_eq!(records[0].bytes_out, 200);
    }

    #[tokio::test]
    async fn test_upsert_traffic_bucket_accumulate() {
        let db = create_test_db().await;

        let ts = Utc::now();
        db.upsert_traffic_bucket(8080, ts, 100, 200).await.unwrap();
        db.upsert_traffic_bucket(8080, ts, 50, 75).await.unwrap();

        let records = db.load_recent_buckets(24).await.unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].bytes_in, 150);
        assert_eq!(records[0].bytes_out, 275);
    }

    #[tokio::test]
    async fn test_record_client_connect_and_disconnect() {
        let db = create_test_db().await;

        db.record_client_connect(8080, Some("test-host".to_string())).await.unwrap();

        // Give a small delay so duration > 0
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        db.record_client_disconnect(8080).await.unwrap();
    }

    #[tokio::test]
    async fn test_record_client_connect_without_hostname() {
        let db = create_test_db().await;

        db.record_client_connect(8080, None).await.unwrap();
    }

    #[tokio::test]
    async fn test_load_recent_buckets_empty() {
        let db = create_test_db().await;

        let records = db.load_recent_buckets(24).await.unwrap();
        assert!(records.is_empty());
    }

    #[tokio::test]
    async fn test_load_port_traffic_empty() {
        let db = create_test_db().await;

        let records = db.load_port_traffic().await.unwrap();
        assert!(records.is_empty());
    }

    #[tokio::test]
    async fn test_insert_and_get_quality_history() {
        let db = create_test_db().await;

        let now = Utc::now();
        db.insert_quality_history(
            8080,
            now,
            50.0,  // avg_rtt_ms
            30.0,  // min_rtt_ms
            100.0, // max_rtt_ms
            0.02,  // loss_rate
            1024.0,  // bytes_in_per_sec
            2048.0,  // bytes_out_per_sec
            95,    // quality_score
            false, // is_warning
            false, // is_critical
        ).await.unwrap();

        let start = now - chrono::Duration::hours(1);
        let end = now + chrono::Duration::hours(1);
        let samples = db.get_quality_history(8080, start, end).await.unwrap();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].avg_rtt_ms, 50.0);
        assert_eq!(samples[0].loss_rate, 0.02);
        assert_eq!(samples[0].quality_score, 95);
    }

    #[tokio::test]
    async fn test_get_quality_history_empty() {
        let db = create_test_db().await;

        let now = Utc::now();
        let samples = db.get_quality_history(8080, now - chrono::Duration::hours(1), now).await.unwrap();
        assert!(samples.is_empty());
    }

    #[tokio::test]
    async fn test_get_quality_ports() {
        let db = create_test_db().await;

        let now = Utc::now();
        db.insert_quality_history(8080, now, 50.0, 30.0, 100.0, 0.02, 1024.0, 2048.0, 95, false, false).await.unwrap();
        db.insert_quality_history(9000, now, 60.0, 40.0, 120.0, 0.05, 2048.0, 4096.0, 90, true, false).await.unwrap();

        let ports = db.get_quality_ports(24).await.unwrap();
        assert_eq!(ports.len(), 2);
        assert!(ports.contains(&8080));
        assert!(ports.contains(&9000));
    }

    #[tokio::test]
    async fn test_cleanup_old_quality_history() {
        let db = create_test_db().await;

        let old_time = Utc::now() - chrono::Duration::hours(48);
        let recent_time = Utc::now();

        db.insert_quality_history(8080, old_time, 50.0, 30.0, 100.0, 0.02, 1024.0, 2048.0, 95, false, false).await.unwrap();
        db.insert_quality_history(8080, recent_time, 60.0, 40.0, 120.0, 0.05, 2048.0, 4096.0, 90, true, false).await.unwrap();

        // Cleanup records older than 24 hours
        let cutoff = Utc::now() - chrono::Duration::hours(24);
        db.cleanup_old_quality_history(cutoff).await.unwrap();

        let start = old_time - chrono::Duration::hours(1);
        let end = recent_time + chrono::Duration::hours(1);
        let samples = db.get_quality_history(8080, start, end).await.unwrap();
        assert_eq!(samples.len(), 1); // Only the recent one should remain
    }

    #[tokio::test]
    async fn test_shadowsocks_config_crud() {
        let db = create_test_db().await;

        // Create
        db.save_shadowsocks_config(8388, "aes-256-gcm", "password123", true).await.unwrap();

        // Read
        let config = db.get_shadowsocks_config(8388).await.unwrap();
        assert!(config.is_some());
        let config = config.unwrap();
        assert_eq!(config.port, 8388);
        assert_eq!(config.cipher, "aes-256-gcm");
        assert_eq!(config.password, "password123");
        assert_eq!(config.enabled, 1);

        // Update
        db.save_shadowsocks_config(8388, "chacha20-ietf-poly1305", "newpass", false).await.unwrap();
        let config = db.get_shadowsocks_config(8388).await.unwrap().unwrap();
        assert_eq!(config.cipher, "chacha20-ietf-poly1305");
        assert_eq!(config.password, "newpass");
        assert_eq!(config.enabled, 0);

        // Delete
        db.delete_shadowsocks_config(8388).await.unwrap();
        let config = db.get_shadowsocks_config(8388).await.unwrap();
        assert!(config.is_none());
    }

    #[tokio::test]
    async fn test_load_shadowsocks_configs() {
        let db = create_test_db().await;

        db.save_shadowsocks_config(8388, "aes-256-gcm", "pass1", true).await.unwrap();
        db.save_shadowsocks_config(8389, "chacha20-ietf-poly1305", "pass2", true).await.unwrap();
        db.save_shadowsocks_config(8390, "aes-256-gcm", "pass3", false).await.unwrap();

        let all_configs = db.load_shadowsocks_configs().await.unwrap();
        assert_eq!(all_configs.len(), 3);

        let enabled_configs = db.load_enabled_shadowsocks_configs().await.unwrap();
        assert_eq!(enabled_configs.len(), 2);
    }

    #[tokio::test]
    async fn test_shadowsocks_config_unique_port() {
        let db = create_test_db().await;

        db.save_shadowsocks_config(8388, "aes-256-gcm", "pass1", true).await.unwrap();
        // Upsert on same port should update, not duplicate
        db.save_shadowsocks_config(8388, "chacha20-ietf-poly1305", "pass2", true).await.unwrap();

        let configs = db.load_shadowsocks_configs().await.unwrap();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].cipher, "chacha20-ietf-poly1305");
    }

    #[tokio::test]
    async fn test_delete_nonexistent_shadowsocks_config() {
        let db = create_test_db().await;
        // Should not error
        db.delete_shadowsocks_config(9999).await.unwrap();
    }
}
