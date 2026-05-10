use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use sqlx::{sqlite::{SqliteConnectOptions, SqlitePoolOptions}, FromRow, Sqlite, Pool};
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
        sqlx::query(
            r#"
            UPDATE client_sessions
            SET disconnected_at = ?,
                duration_seconds = CAST(strftime('%s', ?) AS INTEGER) - CAST(strftime('%s', connected_at) AS INTEGER)
            WHERE port = ? AND disconnected_at IS NULL
            ORDER BY connected_at DESC
            LIMIT 1
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
