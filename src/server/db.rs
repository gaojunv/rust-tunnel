use crate::server::quality::QualitySample;
use chrono::{DateTime, Duration, Timelike, Utc};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    FromRow, Pool, Row, Sqlite,
};
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
        // Enable WAL mode for concurrent reads/writes and set synchronous to NORMAL
        // (NORMAL is safe in WAL mode and avoids extra fsync on every write)
        sqlx::query("PRAGMA journal_mode=WAL").execute(pool).await?;
        sqlx::query("PRAGMA synchronous=NORMAL")
            .execute(pool)
            .await?;

        // Schema v2 migration: drop legacy stats tables replaced by stats_snapshots
        for tbl in &[
            "traffic_buckets",
            "port_traffic",
            "proxy_traffic",
            "connection_quality_history",
        ] {
            sqlx::query(&format!("DROP TABLE IF EXISTS {}", tbl))
                .execute(pool)
                .await?;
        }

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

        // Create indexes
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_client_sessions_port ON client_sessions(port)")
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

        // Trojan configuration table
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS trojan_config (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                port INTEGER NOT NULL UNIQUE,
                password TEXT NOT NULL,
                fallback TEXT NOT NULL DEFAULT '127.0.0.1:80',
                enabled INTEGER NOT NULL DEFAULT 1,
                domain TEXT NOT NULL DEFAULT '',
                created_at DATETIME NOT NULL,
                updated_at DATETIME NOT NULL
            )
            "#,
        )
        .execute(pool)
        .await?;

        // Migrate: 旧库补 domain 列（幂等——报 "duplicate column" 时忽略）
        if let Err(e) =
            sqlx::query("ALTER TABLE trojan_config ADD COLUMN domain TEXT NOT NULL DEFAULT ''")
                .execute(pool)
                .await
        {
            let msg = e.to_string();
            if !msg.contains("duplicate column name") {
                return Err(e);
            }
        }

        // Server logs table
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS server_logs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp INTEGER NOT NULL,
                level TEXT NOT NULL,
                source TEXT NOT NULL,
                target TEXT NOT NULL,
                message TEXT NOT NULL
            )
            "#,
        )
        .execute(pool)
        .await?;

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_logs_timestamp ON server_logs(timestamp)")
            .execute(pool)
            .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_logs_level ON server_logs(level)")
            .execute(pool)
            .await?;

        // Mesh networks table
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS mesh_networks (
                id TEXT PRIMARY KEY,
                created_at DATETIME NOT NULL,
                description TEXT
            )
            "#,
        )
        .execute(pool)
        .await?;

        // Mesh services table
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS mesh_services (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                mesh_id TEXT NOT NULL REFERENCES mesh_networks(id),
                client_name TEXT NOT NULL,
                service_name TEXT NOT NULL,
                protocol TEXT NOT NULL,
                local_addr TEXT NOT NULL,
                dns_record TEXT NOT NULL,
                UNIQUE(mesh_id, service_name)
            )
            "#,
        )
        .execute(pool)
        .await?;

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_mesh_services_mesh ON mesh_services(mesh_id)")
            .execute(pool)
            .await?;

        // ============================================================
        // Reverse Proxy tables
        // ============================================================

        // Proxy rules table
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS proxy_rules (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                type TEXT NOT NULL CHECK(type IN ('http', 'tcp', 'udp')),
                listen_addr TEXT NOT NULL,
                domains TEXT,
                routes TEXT,
                tls_enabled INTEGER NOT NULL DEFAULT 0,
                tls_acme INTEGER NOT NULL DEFAULT 0,
                tls_domain TEXT,
                enabled INTEGER NOT NULL DEFAULT 1,
                created_at DATETIME NOT NULL,
                updated_at DATETIME NOT NULL,
                cert_source TEXT,
                cert_covering_domain TEXT,
                cert_status_updated_at DATETIME
            )
            "#,
        )
        .execute(pool)
        .await?;

        // Migrate: add columns if missing (idempotent — errors ignored on "duplicate column")
        for col_sql in [
            "ALTER TABLE proxy_rules ADD COLUMN cert_source TEXT",
            "ALTER TABLE proxy_rules ADD COLUMN cert_covering_domain TEXT",
            "ALTER TABLE proxy_rules ADD COLUMN cert_status_updated_at DATETIME",
        ] {
            if let Err(e) = sqlx::query(col_sql).execute(pool).await {
                let msg = e.to_string();
                if !msg.contains("duplicate column name") {
                    return Err(e);
                }
            }
        }

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_proxy_rules_type ON proxy_rules(type)")
            .execute(pool)
            .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_proxy_rules_enabled ON proxy_rules(enabled)")
            .execute(pool)
            .await?;

        // ── Unified stats snapshots (replaces proxy_traffic / connection_quality_history) ──
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS stats_snapshots (
                entity_type  TEXT NOT NULL,
                entity_id    TEXT NOT NULL,
                timestamp    DATETIME NOT NULL,
                bytes_in     BIGINT NOT NULL DEFAULT 0,
                bytes_out    BIGINT NOT NULL DEFAULT 0,
                bytes_in_rate  REAL NOT NULL DEFAULT 0.0,
                bytes_out_rate REAL NOT NULL DEFAULT 0.0,
                rtt_ms       REAL,
                loss_pct     REAL,
                active_conns INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (entity_type, entity_id, timestamp)
            )
            "#,
        )
        .execute(pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_stats_snapshots_type ON stats_snapshots(entity_type, timestamp)",
        )
        .execute(pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_stats_snapshots_entity ON stats_snapshots(entity_type, entity_id, timestamp)",
        )
        .execute(pool)
        .await?;

        // ============================================================
        // ACME Certificate tables
        // ============================================================

        // ACME certificates table
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS acme_certificates (
                domain TEXT PRIMARY KEY,
                status TEXT NOT NULL CHECK(status IN ('pending', 'active', 'expired', 'failed')),
                cert_pem TEXT,
                key_pem TEXT,
                chain_pem TEXT,
                issued_at DATETIME,
                expires_at DATETIME,
                auto_renew INTEGER NOT NULL DEFAULT 1,
                last_renewal_attempt DATETIME,
                error_message TEXT,
                created_at DATETIME NOT NULL
            )
            "#,
        )
        .execute(pool)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_acme_certificates_status ON acme_certificates(status)",
        )
        .execute(pool)
        .await?;

        // ACME challenges table
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS acme_challenges (
                token TEXT PRIMARY KEY,
                domain TEXT NOT NULL,
                authorization TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending' CHECK(status IN ('pending', 'valid', 'invalid')),
                created_at DATETIME NOT NULL,
                expires_at DATETIME
            )
            "#,
        )
        .execute(pool)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_acme_challenges_domain ON acme_challenges(domain)",
        )
        .execute(pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_acme_challenges_expires ON acme_challenges(expires_at)",
        )
        .execute(pool)
        .await?;

        // ============================================================
        // Dynamic configuration tables
        // ============================================================

        // Reverse proxy global config (singleton)
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS reverse_proxy_config (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                max_connections INTEGER NOT NULL DEFAULT 10000,
                connection_timeout_secs INTEGER NOT NULL DEFAULT 30,
                buffer_size INTEGER NOT NULL DEFAULT 8192,
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )
            "#,
        )
        .execute(pool)
        .await?;

        // DNS config (singleton)
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS dns_config (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                tunnel_domain TEXT NOT NULL DEFAULT 'tunnel.local',
                mesh_domain TEXT NOT NULL DEFAULT 'mesh.local',
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )
            "#,
        )
        .execute(pool)
        .await?;

        // General server settings (key-value)
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS server_settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )
            "#,
        )
        .execute(pool)
        .await?;

        // Client registry table (spec §2.1)
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS clients (
                name          TEXT PRIMARY KEY,
                hostname      TEXT,
                first_seen_at DATETIME NOT NULL,
                last_seen_at  DATETIME NOT NULL,
                note          TEXT
            )
            "#,
        )
        .execute(pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_clients_last_seen ON clients(last_seen_at)")
            .execute(pool)
            .await?;

        // Single-row server auth table (spec §2.2)
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS server_auth (
                id           INTEGER PRIMARY KEY CHECK(id = 1),
                client_token TEXT NOT NULL,
                updated_at   DATETIME NOT NULL
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

    /// Set port traffic aggregate to absolute values (replaces, does not increment).
    /// Used by the periodic batch flush which writes the in-memory snapshot.
    pub async fn replace_port_traffic(
        &self,
        port: u16,
        total_bytes_in: u64,
        total_bytes_out: u64,
    ) -> Result<(), sqlx::Error> {
        let now = Utc::now();

        sqlx::query(
            r#"
            INSERT INTO port_traffic (port, total_bytes_in, total_bytes_out, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?)
            ON CONFLICT(port) DO UPDATE SET
                total_bytes_in = excluded.total_bytes_in,
                total_bytes_out = excluded.total_bytes_out,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(port as i32)
        .bind(total_bytes_in as i64)
        .bind(total_bytes_out as i64)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Set traffic bucket to absolute values (replaces, does not increment).
    /// Used by the periodic batch flush which writes the in-memory snapshot.
    pub async fn replace_traffic_bucket(
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
                bytes_in = excluded.bytes_in,
                bytes_out = excluded.bytes_out
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
    pub async fn record_client_connect(
        &self,
        port: u16,
        hostname: Option<String>,
    ) -> Result<(), sqlx::Error> {
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
    pub async fn load_recent_buckets(
        &self,
        hours: u32,
    ) -> Result<Vec<TrafficBucketRecord>, sqlx::Error> {
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
    #[allow(clippy::too_many_arguments)]
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

        Ok(rows
            .iter()
            .map(|row| row.get::<i32, _>("port") as u16)
            .collect())
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

    /// Replace the entire Shadowsocks configuration with a single record.
    ///
    /// 运行时只支持单份 SS 配置（API/动态配置均如此），而按端口 upsert 的
    /// `save_shadowsocks_config` 在修改端口时会残留旧行，导致重启后读到旧配置。
    /// 配置更新接口应使用本方法：先清空再插入，保证表中始终只有一份配置。
    pub async fn replace_shadowsocks_config(
        &self,
        port: u16,
        cipher: &str,
        password: &str,
        enabled: bool,
    ) -> Result<(), sqlx::Error> {
        let now = Utc::now();

        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM shadowsocks_config")
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            r#"
            INSERT INTO shadowsocks_config (port, cipher, password, enabled, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(port as i32)
        .bind(cipher)
        .bind(password)
        .bind(enabled as i32)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        Ok(())
    }

    /// Load all Shadowsocks configurations
    pub async fn load_shadowsocks_configs(
        &self,
    ) -> Result<Vec<ShadowsocksConfigRecord>, sqlx::Error> {
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
    pub async fn load_enabled_shadowsocks_configs(
        &self,
    ) -> Result<Vec<ShadowsocksConfigRecord>, sqlx::Error> {
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
    pub async fn get_shadowsocks_config(
        &self,
        port: u16,
    ) -> Result<Option<ShadowsocksConfigRecord>, sqlx::Error> {
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

    /// Save or update Trojan configuration
    pub async fn save_trojan_config(
        &self,
        port: u16,
        password: &str,
        fallback: &str,
        enabled: bool,
        domain: &str,
    ) -> Result<(), sqlx::Error> {
        let now = Utc::now();

        sqlx::query(
            r#"
            INSERT INTO trojan_config (port, password, fallback, enabled, domain, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(port) DO UPDATE SET
                password = excluded.password,
                fallback = excluded.fallback,
                enabled = excluded.enabled,
                domain = excluded.domain,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(port as i32)
        .bind(password)
        .bind(fallback)
        .bind(enabled as i32)
        .bind(domain)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Replace the entire Trojan configuration with a single record.
    ///
    /// 与 `replace_shadowsocks_config` 同理：修改端口时按端口 upsert 会残留旧行，
    /// 配置更新接口应使用本方法保证表中始终只有一份配置。
    pub async fn replace_trojan_config(
        &self,
        port: u16,
        password: &str,
        fallback: &str,
        enabled: bool,
        domain: &str,
    ) -> Result<(), sqlx::Error> {
        let now = Utc::now();

        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM trojan_config")
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            r#"
            INSERT INTO trojan_config (port, password, fallback, enabled, domain, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(port as i32)
        .bind(password)
        .bind(fallback)
        .bind(enabled as i32)
        .bind(domain)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        Ok(())
    }

    /// Load all Trojan configurations
    pub async fn load_trojan_configs(&self) -> Result<Vec<TrojanConfigRecord>, sqlx::Error> {
        let records = sqlx::query_as::<_, TrojanConfigRecord>(
            r#"
            SELECT id, port, password, fallback, enabled, domain, created_at, updated_at
            FROM trojan_config
            ORDER BY port
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(records)
    }

    /// Load enabled Trojan configurations
    pub async fn load_enabled_trojan_configs(
        &self,
    ) -> Result<Vec<TrojanConfigRecord>, sqlx::Error> {
        let records = sqlx::query_as::<_, TrojanConfigRecord>(
            r#"
            SELECT id, port, password, fallback, enabled, domain, created_at, updated_at
            FROM trojan_config
            WHERE enabled = 1
            ORDER BY port
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(records)
    }

    /// Get Trojan config for a specific port
    pub async fn get_trojan_config(
        &self,
        port: u16,
    ) -> Result<Option<TrojanConfigRecord>, sqlx::Error> {
        let record = sqlx::query_as::<_, TrojanConfigRecord>(
            r#"
            SELECT id, port, password, fallback, enabled, domain, created_at, updated_at
            FROM trojan_config
            WHERE port = ?
            "#,
        )
        .bind(port as i32)
        .fetch_optional(&self.pool)
        .await?;

        Ok(record)
    }

    /// Delete Trojan configuration
    pub async fn delete_trojan_config(&self, port: u16) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            DELETE FROM trojan_config
            WHERE port = ?
            "#,
        )
        .bind(port as i32)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Insert a log entry into the database
    pub async fn insert_log(
        &self,
        entry: &crate::server::logs::LogEntry,
    ) -> Result<i64, sqlx::Error> {
        let result = sqlx::query(
            r#"
            INSERT INTO server_logs (timestamp, level, source, target, message)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind(entry.timestamp)
        .bind(&entry.level)
        .bind(&entry.source)
        .bind(&entry.target)
        .bind(&entry.message)
        .execute(&self.pool)
        .await?;

        Ok(result.last_insert_rowid())
    }

    /// Insert a batch of log entries
    pub async fn insert_logs_batch(
        &self,
        entries: &[crate::server::logs::LogEntry],
    ) -> Result<(), sqlx::Error> {
        if entries.is_empty() {
            return Ok(());
        }

        let mut tx = self.pool.begin().await?;

        for entry in entries {
            sqlx::query(
                r#"
                INSERT INTO server_logs (timestamp, level, source, target, message)
                VALUES (?, ?, ?, ?, ?)
                "#,
            )
            .bind(entry.timestamp)
            .bind(&entry.level)
            .bind(&entry.source)
            .bind(&entry.target)
            .bind(&entry.message)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    /// Query logs with filters
    pub async fn query_logs(
        &self,
        level: Option<&str>,
        source: Option<&str>,
        search: Option<&str>,
        limit: u32,
        before_id: Option<i64>,
    ) -> Result<Vec<DbLogEntry>, sqlx::Error> {
        let mut query_str = String::from(
            "SELECT id, timestamp, level, source, target, message FROM server_logs WHERE 1=1",
        );
        let mut params: Vec<String> = Vec::new();

        if let Some(lvl) = level {
            let levels = match lvl.to_lowercase().as_str() {
                "error" => vec!["ERROR"],
                "warn" => vec!["ERROR", "WARN"],
                "info" => vec!["ERROR", "WARN", "INFO"],
                "debug" => vec!["ERROR", "WARN", "INFO", "DEBUG"],
                "trace" => vec!["ERROR", "WARN", "INFO", "DEBUG", "TRACE"],
                _ => vec!["ERROR", "WARN", "INFO", "DEBUG", "TRACE"],
            };
            let placeholders: Vec<String> = levels
                .iter()
                .enumerate()
                .map(|(i, _)| format!("?{}", i + 1))
                .collect();
            query_str.push_str(&format!(" AND level IN ({})", placeholders.join(",")));
            for l in levels {
                params.push(l.to_string());
            }
        }

        if let Some(ref src) = source {
            params.push(format!("{}%", src));
            query_str.push_str(&format!(" AND source LIKE ?{}", params.len()));
        }

        if let Some(ref s) = search {
            params.push(format!("%{}%", s));
            query_str.push_str(&format!(" AND message LIKE ?{}", params.len()));
        }

        if let Some(before) = before_id {
            params.push(before.to_string());
            query_str.push_str(&format!(" AND id < ?{}", params.len()));
        }

        query_str.push_str(&format!(" ORDER BY id DESC LIMIT ?{}", params.len() + 1));
        params.push(limit.to_string());

        // Build the dynamic query
        let mut query = sqlx::query_as::<_, DbLogEntry>(&query_str);
        for p in &params {
            query = query.bind(p);
        }

        let mut rows = query.fetch_all(&self.pool).await?;
        // Reverse to get chronological order
        rows.reverse();
        Ok(rows)
    }

    /// Delete logs older than the given timestamp
    pub async fn cleanup_old_logs(&self, older_than_micros: i64) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            r#"
            DELETE FROM server_logs
            WHERE timestamp < ?
            "#,
        )
        .bind(older_than_micros)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected())
    }

    /// Save a mesh network record
    pub async fn save_mesh_network(
        &self,
        id: &str,
        description: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        let now = Utc::now();
        sqlx::query(
            r#"
            INSERT INTO mesh_networks (id, created_at, description)
            VALUES (?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET description = excluded.description
            "#,
        )
        .bind(id)
        .bind(now)
        .bind(description)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load all mesh networks
    pub async fn load_mesh_networks(&self) -> Result<Vec<MeshNetworkRecord>, sqlx::Error> {
        sqlx::query_as::<_, MeshNetworkRecord>(
            "SELECT id, created_at, description FROM mesh_networks ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Save or update a mesh service
    pub async fn save_mesh_service(
        &self,
        mesh_id: &str,
        client_name: &str,
        service_name: &str,
        protocol: &str,
        local_addr: &str,
        dns_record: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO mesh_services (mesh_id, client_name, service_name, protocol, local_addr, dns_record)
            VALUES (?, ?, ?, ?, ?, ?)
            ON CONFLICT(mesh_id, service_name) DO UPDATE SET
                client_name = excluded.client_name,
                protocol = excluded.protocol,
                local_addr = excluded.local_addr,
                dns_record = excluded.dns_record
            "#,
        )
        .bind(mesh_id)
        .bind(client_name)
        .bind(service_name)
        .bind(protocol)
        .bind(local_addr)
        .bind(dns_record)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load services for a mesh
    pub async fn load_mesh_services(
        &self,
        mesh_id: &str,
    ) -> Result<Vec<MeshServiceRecord>, sqlx::Error> {
        sqlx::query_as::<_, MeshServiceRecord>(
            "SELECT id, mesh_id, client_name, service_name, protocol, local_addr, dns_record \
             FROM mesh_services WHERE mesh_id = ? ORDER BY service_name",
        )
        .bind(mesh_id)
        .fetch_all(&self.pool)
        .await
    }

    /// Delete a mesh service
    pub async fn delete_mesh_service(
        &self,
        mesh_id: &str,
        service_name: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM mesh_services WHERE mesh_id = ? AND service_name = ?")
            .bind(mesh_id)
            .bind(service_name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ============================================================
    // Reverse Proxy methods
    // ============================================================

    /// Save or update a proxy rule
    #[allow(clippy::too_many_arguments)]
    pub async fn save_proxy_rule(
        &self,
        id: &str,
        name: &str,
        rule_type: &str,
        listen_addr: &str,
        domains: Option<&str>,
        routes: Option<&str>,
        tls_enabled: bool,
        tls_acme: bool,
        tls_domain: Option<&str>,
        enabled: bool,
        cert_source: Option<&str>,
        cert_covering_domain: Option<&str>,
        cert_status_updated_at: Option<&DateTime<Utc>>,
    ) -> Result<(), sqlx::Error> {
        let now = Utc::now();
        sqlx::query(
            r#"
            INSERT INTO proxy_rules (id, name, type, listen_addr, domains, routes,
                tls_enabled, tls_acme, tls_domain, enabled, created_at, updated_at,
                cert_source, cert_covering_domain, cert_status_updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                type = excluded.type,
                listen_addr = excluded.listen_addr,
                domains = excluded.domains,
                routes = excluded.routes,
                tls_enabled = excluded.tls_enabled,
                tls_acme = excluded.tls_acme,
                tls_domain = excluded.tls_domain,
                enabled = excluded.enabled,
                updated_at = excluded.updated_at,
                cert_source = excluded.cert_source,
                cert_covering_domain = excluded.cert_covering_domain,
                cert_status_updated_at = excluded.cert_status_updated_at
            "#,
        )
        .bind(id)
        .bind(name)
        .bind(rule_type)
        .bind(listen_addr)
        .bind(domains)
        .bind(routes)
        .bind(tls_enabled as i32)
        .bind(tls_acme as i32)
        .bind(tls_domain)
        .bind(enabled as i32)
        .bind(now)
        .bind(now)
        .bind(cert_source)
        .bind(cert_covering_domain)
        .bind(cert_status_updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load all proxy rules
    pub async fn load_proxy_rules(&self) -> Result<Vec<ProxyRuleRecord>, sqlx::Error> {
        sqlx::query_as::<_, ProxyRuleRecord>(
            r#"
            SELECT id, name, type, listen_addr, domains, routes,
                   tls_enabled, tls_acme, tls_domain, enabled, created_at, updated_at,
                   cert_source, cert_covering_domain, cert_status_updated_at
            FROM proxy_rules
            ORDER BY created_at
            "#,
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Load enabled proxy rules
    pub async fn load_enabled_proxy_rules(&self) -> Result<Vec<ProxyRuleRecord>, sqlx::Error> {
        sqlx::query_as::<_, ProxyRuleRecord>(
            r#"
            SELECT id, name, type, listen_addr, domains, routes,
                   tls_enabled, tls_acme, tls_domain, enabled, created_at, updated_at,
                   cert_source, cert_covering_domain, cert_status_updated_at
            FROM proxy_rules
            WHERE enabled = 1
            ORDER BY created_at
            "#,
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Get a proxy rule by ID
    pub async fn get_proxy_rule(&self, id: &str) -> Result<Option<ProxyRuleRecord>, sqlx::Error> {
        sqlx::query_as::<_, ProxyRuleRecord>(
            r#"
            SELECT id, name, type, listen_addr, domains, routes,
                   tls_enabled, tls_acme, tls_domain, enabled, created_at, updated_at,
                   cert_source, cert_covering_domain, cert_status_updated_at
            FROM proxy_rules
            WHERE id = ?
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
    }

    /// Delete a proxy rule
    pub async fn delete_proxy_rule(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM proxy_rules WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Insert proxy traffic record
    ///
    /// The timestamp is bucketed to the minute so repeated flushes within the
    /// same minute merge into one row via the upsert below (the table doubles
    /// as a per-minute time series for charts).
    pub async fn insert_proxy_traffic(
        &self,
        rule_id: &str,
        bytes_in: u64,
        bytes_out: u64,
        connections: i32,
    ) -> Result<(), sqlx::Error> {
        let now = Utc::now();
        let bucket = now
            - chrono::Duration::seconds(i64::from(now.second()))
            - chrono::Duration::nanoseconds(i64::from(now.nanosecond()));
        sqlx::query(
            r#"
            INSERT INTO proxy_traffic (rule_id, timestamp, bytes_in, bytes_out, connections)
            VALUES (?, ?, ?, ?, ?)
            ON CONFLICT(rule_id, timestamp) DO UPDATE SET
                bytes_in = bytes_in + excluded.bytes_in,
                bytes_out = bytes_out + excluded.bytes_out,
                connections = connections + excluded.connections
            "#,
        )
        .bind(rule_id)
        .bind(bucket)
        .bind(bytes_in as i64)
        .bind(bytes_out as i64)
        .bind(connections)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Get proxy traffic for a rule within time range
    pub async fn get_proxy_traffic(
        &self,
        rule_id: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<ProxyTrafficRecord>, sqlx::Error> {
        sqlx::query_as::<_, ProxyTrafficRecord>(
            r#"
            SELECT rule_id, timestamp, bytes_in, bytes_out, connections
            FROM proxy_traffic
            WHERE rule_id = ? AND timestamp >= ? AND timestamp <= ?
            ORDER BY timestamp
            "#,
        )
        .bind(rule_id)
        .bind(start)
        .bind(end)
        .fetch_all(&self.pool)
        .await
    }

    /// Get proxy stats summary
    pub async fn get_proxy_stats(&self) -> Result<(i64, i64, i64, i64, i64), sqlx::Error> {
        #[derive(FromRow)]
        struct StatsRow {
            total_rules: i64,
            active_rules: i64,
        }

        let stats = sqlx::query_as::<_, StatsRow>(
            r#"
            SELECT
                COUNT(*) as total_rules,
                SUM(CASE WHEN enabled = 1 THEN 1 ELSE 0 END) as active_rules
            FROM proxy_rules
            "#,
        )
        .fetch_one(&self.pool)
        .await?;

        #[derive(FromRow)]
        struct TrafficStatsRow {
            total_connections: i64,
            total_bytes_in: i64,
            total_bytes_out: i64,
        }

        let traffic = sqlx::query_as::<_, TrafficStatsRow>(
            r#"
            SELECT
                COALESCE(SUM(connections), 0) as total_connections,
                COALESCE(SUM(bytes_in), 0) as total_bytes_in,
                COALESCE(SUM(bytes_out), 0) as total_bytes_out
            FROM proxy_traffic
            "#,
        )
        .fetch_one(&self.pool)
        .await?;

        Ok((
            stats.total_rules,
            stats.active_rules,
            traffic.total_connections,
            traffic.total_bytes_in,
            traffic.total_bytes_out,
        ))
    }

    // ============================================================
    // Stats snapshots methods
    // ============================================================

    /// Query stats snapshots within a time range, optionally filtered.
    pub async fn query_stats_snapshots(
        &self,
        entity_types: &[String],
        entity_ids: &[String],
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<StatsSnapshotRow>, sqlx::Error> {
        let mut sql = String::from(
            "SELECT entity_type, entity_id, timestamp, bytes_in, bytes_out, \
             bytes_in_rate, bytes_out_rate, rtt_ms, loss_pct, active_conns \
             FROM stats_snapshots WHERE timestamp >= ? AND timestamp <= ?",
        );
        let mut param_idx = 3;
        if !entity_types.is_empty() {
            let placeholders: Vec<String> =
                (0..entity_types.len()).map(|i| format!("?{}", param_idx + i)).collect();
            sql.push_str(&format!(" AND entity_type IN ({})", placeholders.join(",")));
            param_idx += entity_types.len();
        }
        if !entity_ids.is_empty() {
            let placeholders: Vec<String> =
                (0..entity_ids.len()).map(|i| format!("?{}", param_idx + i)).collect();
            sql.push_str(&format!(" AND entity_id IN ({})", placeholders.join(",")));
        }
        sql.push_str(" ORDER BY timestamp");

        let mut query = sqlx::query_as::<_, StatsSnapshotRow>(&sql).bind(start).bind(end);
        for et in entity_types {
            query = query.bind(et);
        }
        for eid in entity_ids {
            query = query.bind(eid);
        }
        query.fetch_all(&self.pool).await
    }

    /// Delete stats snapshots older than the given timestamp.
    pub async fn cleanup_old_stats_snapshots(
        &self,
        before: DateTime<Utc>,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query("DELETE FROM stats_snapshots WHERE timestamp < ?")
            .bind(before)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    // ============================================================
    // ACME Certificate methods
    // ============================================================

    /// Save or update an ACME certificate
    #[allow(clippy::too_many_arguments)]
    pub async fn save_acme_certificate(
        &self,
        domain: &str,
        status: &str,
        cert_pem: Option<&str>,
        key_pem: Option<&str>,
        chain_pem: Option<&str>,
        issued_at: Option<DateTime<Utc>>,
        expires_at: Option<DateTime<Utc>>,
        auto_renew: bool,
    ) -> Result<(), sqlx::Error> {
        let now = Utc::now();
        sqlx::query(
            r#"
            INSERT INTO acme_certificates (domain, status, cert_pem, key_pem, chain_pem,
                issued_at, expires_at, auto_renew, created_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(domain) DO UPDATE SET
                status = excluded.status,
                cert_pem = excluded.cert_pem,
                key_pem = excluded.key_pem,
                chain_pem = excluded.chain_pem,
                issued_at = excluded.issued_at,
                expires_at = excluded.expires_at,
                auto_renew = excluded.auto_renew
            "#,
        )
        .bind(domain)
        .bind(status)
        .bind(cert_pem)
        .bind(key_pem)
        .bind(chain_pem)
        .bind(issued_at)
        .bind(expires_at)
        .bind(auto_renew as i32)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load all ACME certificates
    pub async fn load_acme_certificates(&self) -> Result<Vec<AcmeCertificateRecord>, sqlx::Error> {
        sqlx::query_as::<_, AcmeCertificateRecord>(
            r#"
            SELECT domain, status, cert_pem, key_pem, chain_pem,
                   issued_at, expires_at, auto_renew, last_renewal_attempt,
                   error_message, created_at
            FROM acme_certificates
            ORDER BY created_at
            "#,
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Get an ACME certificate by domain
    pub async fn get_acme_certificate(
        &self,
        domain: &str,
    ) -> Result<Option<AcmeCertificateRecord>, sqlx::Error> {
        sqlx::query_as::<_, AcmeCertificateRecord>(
            r#"
            SELECT domain, status, cert_pem, key_pem, chain_pem,
                   issued_at, expires_at, auto_renew, last_renewal_attempt,
                   error_message, created_at
            FROM acme_certificates
            WHERE domain = ?
            "#,
        )
        .bind(domain)
        .fetch_optional(&self.pool)
        .await
    }

    /// Update ACME certificate status
    pub async fn update_acme_certificate_status(
        &self,
        domain: &str,
        status: &str,
        error_message: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            UPDATE acme_certificates
            SET status = ?, error_message = ?
            WHERE domain = ?
            "#,
        )
        .bind(status)
        .bind(error_message)
        .bind(domain)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Update ACME certificate renewal attempt
    pub async fn update_acme_certificate_renewal_attempt(
        &self,
        domain: &str,
    ) -> Result<(), sqlx::Error> {
        let now = Utc::now();
        sqlx::query(
            r#"
            UPDATE acme_certificates
            SET last_renewal_attempt = ?
            WHERE domain = ?
            "#,
        )
        .bind(now)
        .bind(domain)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Delete an ACME certificate
    pub async fn delete_acme_certificate(&self, domain: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM acme_certificates WHERE domain = ?")
            .bind(domain)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Save an ACME challenge
    pub async fn save_acme_challenge(
        &self,
        token: &str,
        domain: &str,
        authorization: &str,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<(), sqlx::Error> {
        let now = Utc::now();
        sqlx::query(
            r#"
            INSERT INTO acme_challenges (token, domain, authorization, status, created_at, expires_at)
            VALUES (?, ?, ?, 'pending', ?, ?)
            ON CONFLICT(token) DO UPDATE SET
                domain = excluded.domain,
                authorization = excluded.authorization,
                status = 'pending',
                expires_at = excluded.expires_at
            "#,
        )
        .bind(token)
        .bind(domain)
        .bind(authorization)
        .bind(now)
        .bind(expires_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Get an ACME challenge by token
    pub async fn get_acme_challenge(
        &self,
        token: &str,
    ) -> Result<Option<AcmeChallengeRecord>, sqlx::Error> {
        sqlx::query_as::<_, AcmeChallengeRecord>(
            r#"
            SELECT token, domain, authorization, status, created_at, expires_at
            FROM acme_challenges
            WHERE token = ?
            "#,
        )
        .bind(token)
        .fetch_optional(&self.pool)
        .await
    }

    /// Update ACME challenge status
    pub async fn update_acme_challenge_status(
        &self,
        token: &str,
        status: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            UPDATE acme_challenges
            SET status = ?
            WHERE token = ?
            "#,
        )
        .bind(status)
        .bind(token)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Delete expired ACME challenges
    pub async fn cleanup_expired_acme_challenges(&self) -> Result<u64, sqlx::Error> {
        let now = Utc::now();
        let result = sqlx::query(
            r#"
            DELETE FROM acme_challenges
            WHERE expires_at IS NOT NULL AND expires_at < ?
            "#,
        )
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Delete an ACME challenge
    pub async fn delete_acme_challenge(&self, token: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM acme_challenges WHERE token = ?")
            .bind(token)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Load ACME certificates that need renewal
    pub async fn load_acme_certificates_needing_renewal(
        &self,
        days_before_expiry: i64,
    ) -> Result<Vec<AcmeCertificateRecord>, sqlx::Error> {
        let cutoff = Utc::now() + chrono::Duration::days(days_before_expiry);
        sqlx::query_as::<_, AcmeCertificateRecord>(
            r#"
            SELECT domain, status, cert_pem, key_pem, chain_pem,
                   issued_at, expires_at, auto_renew, last_renewal_attempt,
                   error_message, created_at
            FROM acme_certificates
            WHERE status = 'active'
              AND auto_renew = 1
              AND expires_at IS NOT NULL
              AND expires_at <= ?
            "#,
        )
        .bind(cutoff)
        .fetch_all(&self.pool)
        .await
    }

    // ============================================================
    // Dynamic configuration methods
    // ============================================================

    /// Load reverse proxy config (singleton)
    pub async fn load_reverse_proxy_config(
        &self,
    ) -> Result<Option<ReverseProxyConfigRecord>, sqlx::Error> {
        sqlx::query_as::<_, ReverseProxyConfigRecord>(
            "SELECT max_connections, connection_timeout_secs, buffer_size FROM reverse_proxy_config WHERE id = 1",
        )
        .fetch_optional(&self.pool)
        .await
    }

    /// Save reverse proxy config (singleton, upsert)
    pub async fn save_reverse_proxy_config(
        &self,
        max_connections: u32,
        connection_timeout_secs: u64,
        buffer_size: usize,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO reverse_proxy_config (id, max_connections, connection_timeout_secs, buffer_size, updated_at)
            VALUES (1, ?, ?, ?, datetime('now'))
            ON CONFLICT(id) DO UPDATE SET
                max_connections = excluded.max_connections,
                connection_timeout_secs = excluded.connection_timeout_secs,
                buffer_size = excluded.buffer_size,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(max_connections as i64)
        .bind(connection_timeout_secs as i64)
        .bind(buffer_size as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load DNS config (singleton)
    pub async fn load_dns_config(&self) -> Result<Option<DnsConfigRecord>, sqlx::Error> {
        sqlx::query_as::<_, DnsConfigRecord>(
            "SELECT tunnel_domain, mesh_domain FROM dns_config WHERE id = 1",
        )
        .fetch_optional(&self.pool)
        .await
    }

    /// Save DNS config (singleton, upsert)
    pub async fn save_dns_config(
        &self,
        tunnel_domain: &str,
        mesh_domain: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO dns_config (id, tunnel_domain, mesh_domain, updated_at)
            VALUES (1, ?, ?, datetime('now'))
            ON CONFLICT(id) DO UPDATE SET
                tunnel_domain = excluded.tunnel_domain,
                mesh_domain = excluded.mesh_domain,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(tunnel_domain)
        .bind(mesh_domain)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load a server setting by key
    pub async fn load_server_setting(&self, key: &str) -> Result<Option<String>, sqlx::Error> {
        let row = sqlx::query("SELECT value FROM server_settings WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get::<String, _>("value")))
    }

    /// Save a server setting (upsert)
    pub async fn save_server_setting(&self, key: &str, value: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO server_settings (key, value, updated_at)
            VALUES (?, ?, datetime('now'))
            ON CONFLICT(key) DO UPDATE SET
                value = excluded.value,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ============================================================
    // Client registry methods
    // ============================================================

    pub async fn upsert_client(
        &self,
        name: &str,
        hostname: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        let now = Utc::now();
        sqlx::query(
            r#"
            INSERT INTO clients (name, hostname, first_seen_at, last_seen_at)
            VALUES (?, ?, ?, ?)
            ON CONFLICT(name) DO UPDATE SET
                hostname = excluded.hostname,
                last_seen_at = excluded.last_seen_at
            "#,
        )
        .bind(name)
        .bind(hostname)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn touch_client_last_seen(&self, name: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE clients SET last_seen_at = ? WHERE name = ?")
            .bind(Utc::now())
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn list_clients(&self) -> Result<Vec<ClientRecord>, sqlx::Error> {
        let rows = sqlx::query_as::<_, ClientRecord>(
            "SELECT name, hostname, first_seen_at, last_seen_at, note FROM clients ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn get_client(&self, name: &str) -> Result<Option<ClientRecord>, sqlx::Error> {
        sqlx::query_as::<_, ClientRecord>(
            "SELECT name, hostname, first_seen_at, last_seen_at, note FROM clients WHERE name = ?",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await
    }

    pub async fn update_client_note(
        &self,
        name: &str,
        note: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE clients SET note = ? WHERE name = ?")
            .bind(note)
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn delete_client(&self, name: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM clients WHERE name = ?")
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ============================================================
    // Server auth methods
    // ============================================================

    pub async fn load_server_auth(&self) -> Result<Option<String>, sqlx::Error> {
        let row = sqlx::query("SELECT client_token FROM server_auth WHERE id = 1")
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get::<String, _>("client_token")))
    }

    pub async fn save_server_auth(&self, token: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO server_auth (id, client_token, updated_at)
            VALUES (1, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                client_token = excluded.client_token,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(token)
        .bind(Utc::now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Return `(rule_id, rule_name)` pairs for every proxy rule whose routes
    /// JSON contains a backend with `kind == "client"` and matching
    /// `client_name`. Used to enforce "reject delete when referenced" (spec §2.4).
    pub async fn rules_referencing_client(
        &self,
        client_name: &str,
    ) -> Result<Vec<(String, String)>, sqlx::Error> {
        let rows = sqlx::query("SELECT id, name, routes FROM proxy_rules")
            .fetch_all(&self.pool)
            .await?;
        let mut out = Vec::new();
        for row in rows {
            let id: String = row.get("id");
            let name: String = row.get("name");
            let routes_json: Option<String> = row.get("routes");
            let Some(routes_json) = routes_json else {
                continue;
            };
            let Ok(routes) = serde_json::from_str::<serde_json::Value>(&routes_json) else {
                continue;
            };
            let Some(arr) = routes.as_array() else {
                continue;
            };
            let mut hit = false;
            'route: for r in arr {
                let Some(backends) = r.get("backends").and_then(|v| v.as_array()) else {
                    continue;
                };
                for b in backends {
                    let kind = b.get("kind").and_then(|v| v.as_str()).unwrap_or("direct");
                    let cn = b.get("client_name").and_then(|v| v.as_str()).unwrap_or("");
                    if kind == "client" && cn == client_name {
                        hit = true;
                        break 'route;
                    }
                }
            }
            if hit {
                out.push((id, name));
            }
        }
        Ok(out)
    }

    #[cfg(test)]
    pub fn pool(&self) -> &sqlx::SqlitePool {
        &self.pool
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

/// A log entry row from the database
#[derive(FromRow, Debug, Clone)]
pub struct DbLogEntry {
    pub id: i64,
    pub timestamp: i64,
    pub level: String,
    pub source: String,
    pub target: String,
    pub message: String,
}

/// Trojan config record from database
#[derive(FromRow, Debug)]
pub struct TrojanConfigRecord {
    pub id: i32,
    pub port: i32,
    pub password: String,
    pub fallback: String,
    pub enabled: i32,
    pub domain: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Mesh network record from database
#[derive(FromRow, Debug)]
pub struct MeshNetworkRecord {
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub description: Option<String>,
}

/// Mesh service record from database
#[derive(FromRow, Debug)]
pub struct MeshServiceRecord {
    pub id: i32,
    pub mesh_id: String,
    pub client_name: String,
    pub service_name: String,
    pub protocol: String,
    pub local_addr: String,
    pub dns_record: String,
}

/// Proxy rule record from database
#[derive(FromRow, Debug, Clone)]
pub struct ProxyRuleRecord {
    pub id: String,
    pub name: String,
    #[sqlx(rename = "type")]
    pub rule_type: String,
    pub listen_addr: String,
    pub domains: Option<String>,
    pub routes: Option<String>,
    pub tls_enabled: i32,
    pub tls_acme: i32,
    pub tls_domain: Option<String>,
    pub enabled: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub cert_source: Option<String>,
    pub cert_covering_domain: Option<String>,
    pub cert_status_updated_at: Option<DateTime<Utc>>,
}

/// DB row mirroring stats_snapshots
#[derive(Debug, Clone, FromRow)]
pub struct StatsSnapshotRow {
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

/// Proxy traffic record from database
#[derive(FromRow, Debug)]
pub struct ProxyTrafficRecord {
    pub rule_id: String,
    pub timestamp: DateTime<Utc>,
    pub bytes_in: i64,
    pub bytes_out: i64,
    pub connections: i32,
}

/// ACME certificate record from database
#[derive(FromRow, Debug, Clone)]
pub struct AcmeCertificateRecord {
    pub domain: String,
    pub status: String,
    pub cert_pem: Option<String>,
    pub key_pem: Option<String>,
    pub chain_pem: Option<String>,
    pub issued_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub auto_renew: i32,
    pub last_renewal_attempt: Option<DateTime<Utc>>,
    pub error_message: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// ACME challenge record from database
#[derive(FromRow, Debug)]
pub struct AcmeChallengeRecord {
    pub token: String,
    pub domain: String,
    pub authorization: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// Reverse proxy config record
#[derive(Debug, Clone, FromRow)]
pub struct ReverseProxyConfigRecord {
    pub max_connections: i64,
    pub connection_timeout_secs: i64,
    pub buffer_size: i64,
}

/// DNS config record
#[derive(Debug, Clone, FromRow)]
pub struct DnsConfigRecord {
    pub tunnel_domain: String,
    pub mesh_domain: String,
}

/// Client registry record from database
#[derive(Debug, Clone, FromRow)]
pub struct ClientRecord {
    pub name: String,
    pub hostname: Option<String>,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub note: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn create_test_db() -> Database {
        Database::new(":memory:")
            .await
            .expect("Failed to create in-memory database")
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

        db.record_client_connect(8080, Some("test-host".to_string()))
            .await
            .unwrap();

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
            8080, now, 50.0,   // avg_rtt_ms
            30.0,   // min_rtt_ms
            100.0,  // max_rtt_ms
            0.02,   // loss_rate
            1024.0, // bytes_in_per_sec
            2048.0, // bytes_out_per_sec
            95,     // quality_score
            false,  // is_warning
            false,  // is_critical
        )
        .await
        .unwrap();

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
        let samples = db
            .get_quality_history(8080, now - chrono::Duration::hours(1), now)
            .await
            .unwrap();
        assert!(samples.is_empty());
    }

    #[tokio::test]
    async fn test_get_quality_ports() {
        let db = create_test_db().await;

        let now = Utc::now();
        db.insert_quality_history(
            8080, now, 50.0, 30.0, 100.0, 0.02, 1024.0, 2048.0, 95, false, false,
        )
        .await
        .unwrap();
        db.insert_quality_history(
            9000, now, 60.0, 40.0, 120.0, 0.05, 2048.0, 4096.0, 90, true, false,
        )
        .await
        .unwrap();

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

        db.insert_quality_history(
            8080, old_time, 50.0, 30.0, 100.0, 0.02, 1024.0, 2048.0, 95, false, false,
        )
        .await
        .unwrap();
        db.insert_quality_history(
            8080,
            recent_time,
            60.0,
            40.0,
            120.0,
            0.05,
            2048.0,
            4096.0,
            90,
            true,
            false,
        )
        .await
        .unwrap();

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
        db.save_shadowsocks_config(8388, "aes-256-gcm", "password123", true)
            .await
            .unwrap();

        // Read
        let config = db.get_shadowsocks_config(8388).await.unwrap();
        assert!(config.is_some());
        let config = config.unwrap();
        assert_eq!(config.port, 8388);
        assert_eq!(config.cipher, "aes-256-gcm");
        assert_eq!(config.password, "password123");
        assert_eq!(config.enabled, 1);

        // Update
        db.save_shadowsocks_config(8388, "chacha20-ietf-poly1305", "newpass", false)
            .await
            .unwrap();
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

        db.save_shadowsocks_config(8388, "aes-256-gcm", "pass1", true)
            .await
            .unwrap();
        db.save_shadowsocks_config(8389, "chacha20-ietf-poly1305", "pass2", true)
            .await
            .unwrap();
        db.save_shadowsocks_config(8390, "aes-256-gcm", "pass3", false)
            .await
            .unwrap();

        let all_configs = db.load_shadowsocks_configs().await.unwrap();
        assert_eq!(all_configs.len(), 3);

        let enabled_configs = db.load_enabled_shadowsocks_configs().await.unwrap();
        assert_eq!(enabled_configs.len(), 2);
    }

    #[tokio::test]
    async fn test_shadowsocks_config_unique_port() {
        let db = create_test_db().await;

        db.save_shadowsocks_config(8388, "aes-256-gcm", "pass1", true)
            .await
            .unwrap();
        // Upsert on same port should update, not duplicate
        db.save_shadowsocks_config(8388, "chacha20-ietf-poly1305", "pass2", true)
            .await
            .unwrap();

        let configs = db.load_shadowsocks_configs().await.unwrap();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].cipher, "chacha20-ietf-poly1305");
    }

    #[tokio::test]
    async fn test_replace_shadowsocks_config_single_row() {
        let db = create_test_db().await;

        db.save_shadowsocks_config(8388, "aes-256-gcm", "pass1", true)
            .await
            .unwrap();
        // 修改端口：整表替换，不应残留旧行
        db.replace_shadowsocks_config(9999, "chacha20-ietf-poly1305", "pass2", true)
            .await
            .unwrap();

        let configs = db.load_shadowsocks_configs().await.unwrap();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].port, 9999);
        assert_eq!(configs[0].cipher, "chacha20-ietf-poly1305");
        assert_eq!(configs[0].password, "pass2");
        assert_eq!(configs[0].enabled, 1);
    }

    #[tokio::test]
    async fn test_replace_trojan_config_single_row() {
        let db = create_test_db().await;

        db.save_trojan_config(443, "pass1", "127.0.0.1:80", true, "")
            .await
            .unwrap();
        // 修改端口：整表替换，不应残留旧行
        db.replace_trojan_config(8443, "pass2", "127.0.0.1:8080", false, "")
            .await
            .unwrap();

        let configs = db.load_trojan_configs().await.unwrap();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].port, 8443);
        assert_eq!(configs[0].password, "pass2");
        assert_eq!(configs[0].fallback, "127.0.0.1:8080");
        assert_eq!(configs[0].enabled, 0);
    }

    #[tokio::test]
    async fn test_delete_nonexistent_shadowsocks_config() {
        let db = create_test_db().await;
        // Should not error
        db.delete_shadowsocks_config(9999).await.unwrap();
    }

    #[tokio::test]
    async fn test_server_logs_table_creation() {
        let db = create_test_db().await;
        // Just verify the table exists by doing an insert+query
        let result = db.query_logs(None, None, None, 10, None).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_insert_and_query_logs() {
        let db = create_test_db().await;
        let entry = crate::server::logs::LogEntry {
            id: 0,
            timestamp: 1000000,
            level: "INFO".into(),
            source: "server".into(),
            target: "test::module".into(),
            message: "test message".into(),
        };
        db.insert_log(&entry).await.unwrap();

        let results = db.query_logs(None, None, None, 10, None).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].level, "INFO");
        assert_eq!(results[0].message, "test message");
    }

    #[tokio::test]
    async fn test_query_logs_level_filter() {
        let db = create_test_db().await;
        let info_entry = crate::server::logs::LogEntry {
            id: 0,
            timestamp: 1000000,
            level: "INFO".into(),
            source: "server".into(),
            target: "test".into(),
            message: "info msg".into(),
        };
        let error_entry = crate::server::logs::LogEntry {
            id: 0,
            timestamp: 2000000,
            level: "ERROR".into(),
            source: "server".into(),
            target: "test".into(),
            message: "error msg".into(),
        };
        db.insert_log(&info_entry).await.unwrap();
        db.insert_log(&error_entry).await.unwrap();

        // Filter to WARN+ (includes WARN and ERROR)
        let results = db
            .query_logs(Some("warn"), None, None, 10, None)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].level, "ERROR");
    }

    #[tokio::test]
    async fn test_cleanup_old_logs() {
        let db = create_test_db().await;
        let entry = crate::server::logs::LogEntry {
            id: 0,
            timestamp: 1000000,
            level: "INFO".into(),
            source: "server".into(),
            target: "test".into(),
            message: "old".into(),
        };
        db.insert_log(&entry).await.unwrap();

        // Cleanup anything older than 2000000
        let deleted = db.cleanup_old_logs(2000000).await.unwrap();
        assert_eq!(deleted, 1);

        let results = db.query_logs(None, None, None, 10, None).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_insert_logs_batch() {
        let db = create_test_db().await;
        let entries: Vec<crate::server::logs::LogEntry> = (0..3)
            .map(|i| crate::server::logs::LogEntry {
                id: 0,
                timestamp: 1000000 + i * 1000,
                level: "INFO".into(),
                source: "server".into(),
                target: "test".into(),
                message: format!("msg {}", i),
            })
            .collect();
        db.insert_logs_batch(&entries).await.unwrap();

        let results = db.query_logs(None, None, None, 10, None).await.unwrap();
        assert_eq!(results.len(), 3);
    }

    #[tokio::test]
    async fn test_upsert_and_list_clients() {
        let db = Database::new(":memory:").await.unwrap();
        db.upsert_client("home-nas", Some("nas.local"))
            .await
            .unwrap();
        db.upsert_client("home-nas", Some("nas.local"))
            .await
            .unwrap(); // idempotent
        db.upsert_client("office-pc", None).await.unwrap();

        let list = db.list_clients().await.unwrap();
        assert_eq!(list.len(), 2);
        let nas = list.iter().find(|c| c.name == "home-nas").unwrap();
        assert_eq!(nas.hostname.as_deref(), Some("nas.local"));
    }

    #[tokio::test]
    async fn test_touch_client_last_seen() {
        let db = Database::new(":memory:").await.unwrap();
        db.upsert_client("home-nas", None).await.unwrap();
        let before = db
            .get_client("home-nas")
            .await
            .unwrap()
            .unwrap()
            .last_seen_at;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        db.touch_client_last_seen("home-nas").await.unwrap();
        let after = db
            .get_client("home-nas")
            .await
            .unwrap()
            .unwrap()
            .last_seen_at;
        assert!(after > before);
    }

    #[tokio::test]
    async fn test_update_client_note() {
        let db = Database::new(":memory:").await.unwrap();
        db.upsert_client("home-nas", None).await.unwrap();
        db.update_client_note("home-nas", Some("primary"))
            .await
            .unwrap();
        assert_eq!(
            db.get_client("home-nas")
                .await
                .unwrap()
                .unwrap()
                .note
                .as_deref(),
            Some("primary")
        );
        db.update_client_note("home-nas", None).await.unwrap();
        assert!(db
            .get_client("home-nas")
            .await
            .unwrap()
            .unwrap()
            .note
            .is_none());
    }

    #[tokio::test]
    async fn test_delete_client() {
        let db = Database::new(":memory:").await.unwrap();
        db.upsert_client("home-nas", None).await.unwrap();
        db.delete_client("home-nas").await.unwrap();
        let list = db.list_clients().await.unwrap();
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn test_server_auth_load_and_save() {
        let db = Database::new(":memory:").await.unwrap();
        assert!(db.load_server_auth().await.unwrap().is_none());
        db.save_server_auth("token-abc").await.unwrap();
        assert_eq!(
            db.load_server_auth().await.unwrap().as_deref(),
            Some("token-abc")
        );
        db.save_server_auth("token-def").await.unwrap();
        assert_eq!(
            db.load_server_auth().await.unwrap().as_deref(),
            Some("token-def")
        );
    }

    #[tokio::test]
    async fn test_rules_referencing_client() {
        let db = Database::new(":memory:").await.unwrap();

        // route JSON: 一个 backend 指向 home-nas，一个 direct
        let routes_json = serde_json::json!([
            {
                "path": "/",
                "backends": [
                    { "kind": "client", "addr": "localhost:80", "client_name": "home-nas",
                      "weight": 100, "protocol": "http1", "scheme": "http" },
                    { "kind": "direct", "addr": "10.0.0.1:80",
                      "weight": 100, "protocol": "http1", "scheme": "http" }
                ],
                "load_balancing": "round_robin"
            }
        ])
        .to_string();

        db.save_proxy_rule(
            "rule-1",
            "web",
            "http",
            "0.0.0.0:80",
            None,
            Some(&routes_json),
            false,
            false,
            None,
            true,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        let refs = db.rules_referencing_client("home-nas").await.unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].0, "rule-1");
        assert_eq!(refs[0].1, "web");

        let refs = db.rules_referencing_client("nonexistent").await.unwrap();
        assert!(refs.is_empty());
    }

    #[tokio::test]
    async fn test_rules_referencing_client_ignores_direct_only() {
        let db = Database::new(":memory:").await.unwrap();
        let routes_json = serde_json::json!([{
            "path": "/",
            "backends": [
                { "kind": "direct", "addr": "10.0.0.1:80",
                  "weight": 100, "protocol": "http1", "scheme": "http" }
            ],
            "load_balancing": "round_robin"
        }])
        .to_string();
        db.save_proxy_rule(
            "r1",
            "web",
            "http",
            "0.0.0.0:80",
            None,
            Some(&routes_json),
            false,
            false,
            None,
            true,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert!(db
            .rules_referencing_client("anything")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn test_rules_referencing_client_null_routes() {
        // rule with routes = NULL (e.g. tcp rule)
        let db = Database::new(":memory:").await.unwrap();
        db.save_proxy_rule(
            "r1",
            "tcp-rule",
            "tcp",
            "0.0.0.0:9000",
            None,
            None,
            false,
            false,
            None,
            true,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert!(db
            .rules_referencing_client("anyone")
            .await
            .unwrap()
            .is_empty());
    }
}

#[cfg(test)]
mod cert_status_migration_tests {
    use super::*;

    #[tokio::test]
    async fn migration_adds_cert_columns_to_empty_db() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("test.db");
        let db = Database::new(path.to_str().unwrap()).await.unwrap();

        // 查询 pragma_table_info 验证列存在
        let cols: Vec<(String,)> =
            sqlx::query_as("SELECT name FROM pragma_table_info('proxy_rules')")
                .fetch_all(db.pool())
                .await
                .unwrap();
        let names: Vec<String> = cols.into_iter().map(|(n,)| n).collect();
        assert!(names.contains(&"cert_source".to_string()));
        assert!(names.contains(&"cert_covering_domain".to_string()));
        assert!(names.contains(&"cert_status_updated_at".to_string()));
    }

    #[tokio::test]
    async fn migration_idempotent_on_existing_db() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("test.db");
        // 打开两次应无错
        let _db1 = Database::new(path.to_str().unwrap()).await.unwrap();
        let _db2 = Database::new(path.to_str().unwrap()).await.unwrap();
    }

    #[tokio::test]
    async fn save_and_load_rule_with_cert_status_fields() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("test.db");
        let db = Database::new(path.to_str().unwrap()).await.unwrap();

        db.save_proxy_rule(
            "r-1",
            "test",
            "http",
            "0.0.0.0:443",
            Some(r#"["a.example.com"]"#),
            Some(r#"[]"#),
            true,
            true,
            Some("a.example.com"),
            true,
            Some("exact"),
            Some("a.example.com"),
            Some(&chrono::Utc::now()),
        )
        .await
        .unwrap();

        let rules = db.load_proxy_rules().await.unwrap();
        assert_eq!(rules.len(), 1);
        let r = &rules[0];
        assert_eq!(r.cert_source.as_deref(), Some("exact"));
        assert_eq!(r.cert_covering_domain.as_deref(), Some("a.example.com"));
        assert!(r.cert_status_updated_at.is_some());
    }

    /// 旧库（无 domain 列）经 Database::new 迁移后应自动补上 domain 列，
    /// 存量行 domain 默认 ''。
    #[tokio::test]
    async fn trojan_config_domain_column_migration() {
        let tempdir = tempfile::TempDir::new().unwrap();
        let db_path = tempdir.path().join("migrate.db");

        // 用旧 schema（无 domain 列）手工建表并插入一行
        {
            let opts = sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&db_path)
                .create_if_missing(true);
            let pool = sqlx::SqlitePool::connect_with(opts).await.unwrap();
            sqlx::query(
                r#"
                CREATE TABLE trojan_config (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    port INTEGER NOT NULL UNIQUE,
                    password TEXT NOT NULL,
                    fallback TEXT NOT NULL DEFAULT '127.0.0.1:80',
                    enabled INTEGER NOT NULL DEFAULT 1,
                    created_at DATETIME NOT NULL,
                    updated_at DATETIME NOT NULL
                )
                "#,
            )
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO trojan_config (port, password, fallback, enabled, created_at, updated_at) \
                 VALUES (1443, 'old-pass', '127.0.0.1:80', 1, '2026-01-01 00:00:00', '2026-01-01 00:00:00')",
            )
            .execute(&pool)
            .await
            .unwrap();
            pool.close().await;
        }

        let db = Database::new(db_path.to_str().unwrap()).await.unwrap();
        let configs = db.load_trojan_configs().await.unwrap();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].port, 1443);
        assert_eq!(configs[0].domain, "", "迁移后存量行 domain 应为空串");
    }

    /// domain 随 save/replace/load 完整往返。
    #[tokio::test]
    async fn trojan_config_domain_roundtrip() {
        let tempdir = tempfile::TempDir::new().unwrap();
        let db_path = tempdir.path().join("roundtrip.db");
        let db = Database::new(db_path.to_str().unwrap()).await.unwrap();

        db.save_trojan_config(443, "pass1", "127.0.0.1:80", true, "trojan.example.com")
            .await
            .unwrap();
        let cfg = db.get_trojan_config(443).await.unwrap().unwrap();
        assert_eq!(cfg.domain, "trojan.example.com");

        // replace 整表替换语义保留，domain 一并更新
        db.replace_trojan_config(8443, "pass2", "127.0.0.1:8080", false, "t2.example.com")
            .await
            .unwrap();
        let all = db.load_trojan_configs().await.unwrap();
        assert_eq!(all.len(), 1, "replace 后表中应只有一份配置");
        assert_eq!(all[0].port, 8443);
        assert_eq!(all[0].domain, "t2.example.com");
        assert_eq!(all[0].enabled, 0);

        let enabled = db.load_enabled_trojan_configs().await.unwrap();
        assert!(
            enabled.is_empty(),
            "enabled=0 的行不应出现在 enabled 查询里"
        );
    }
}
