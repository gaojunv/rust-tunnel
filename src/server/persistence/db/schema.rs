use super::Database;
use sqlx::{Pool, Sqlite};

impl Database {
    /// Initialize database tables
    pub(crate) async fn initialize_schema(pool: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
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
                type TEXT NOT NULL CHECK(type IN ('http', 'tcp', 'udp', 'llm')),
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

        // ============================================================
        // LLM Gateway tables
        // ============================================================

        // LLM providers table
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS llm_providers (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                provider_type TEXT NOT NULL CHECK(provider_type IN ('deepseek', 'volcengine', 'kimi', 'mimo')),
                base_url TEXT NOT NULL,
                api_key TEXT NOT NULL DEFAULT '',
                extra_config TEXT,
                enabled INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )
            "#,
        )
        .execute(pool)
        .await?;

        // LLM models table
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS llm_models (
                id TEXT PRIMARY KEY,
                provider_id TEXT NOT NULL REFERENCES llm_providers(id) ON DELETE CASCADE,
                model_name TEXT NOT NULL,
                alias TEXT NOT NULL DEFAULT '',
                tags TEXT NOT NULL DEFAULT '[]',
                enabled INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )
            "#,
        )
        .execute(pool)
        .await?;

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_llm_models_provider ON llm_models(provider_id)")
            .execute(pool)
            .await?;

        // LLM API keys table (gateway-level keys for external callers)
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS llm_api_keys (
                id TEXT PRIMARY KEY,
                key_hash TEXT NOT NULL UNIQUE,
                key_prefix TEXT NOT NULL,
                name TEXT NOT NULL DEFAULT '',
                enabled INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                last_used_at TEXT
            )
            "#,
        )
        .execute(pool)
        .await?;

        Ok(())
    }
}
