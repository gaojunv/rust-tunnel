use chrono::{DateTime, Duration, Utc};
use sqlx::{sqlite::{SqliteConnectOptions, SqlitePoolOptions}, FromRow, Sqlite, Pool};
use std::path::Path;
use std::str::FromStr;

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
                connected_at DATETIME NOT NULL,
                disconnected_at DATETIME,
                duration_seconds INTEGER
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
    pub async fn record_client_connect(&self, port: u16) -> Result<(), sqlx::Error> {
        let now = Utc::now();

        sqlx::query(
            r#"
            INSERT INTO client_sessions (port, connected_at, disconnected_at, duration_seconds)
            VALUES (?, ?, NULL, NULL)
            "#,
        )
        .bind(port as i32)
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
