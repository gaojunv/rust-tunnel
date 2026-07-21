use sqlx::Row;

pub use crate::server::persistence::db::records::*;
pub use crate::server::persistence::db::Database;

impl Database {

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
    async fn test_delete_client() {
        let db = Database::new(":memory:").await.unwrap();
        db.upsert_client("home-nas", None).await.unwrap();
        db.delete_client("home-nas").await.unwrap();
        let list = db.list_clients().await.unwrap();
        assert!(list.is_empty());
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
