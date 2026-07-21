use sqlx::{
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    Pool, Sqlite,
};
use std::path::Path;
use std::str::FromStr;

pub mod acme;
pub mod clients;
pub mod dns;
pub mod logs;
pub mod mesh;
pub mod proxy_rules;
pub mod records;
pub mod reverse_proxy;
pub mod schema;
pub mod settings;
pub mod server_auth;
pub mod shadowsocks;
pub mod stats;
pub mod trojan;
pub use records::*;

/// Database wrapper for persistence
#[derive(Clone)]
pub struct Database {
    pub(crate) pool: Pool<Sqlite>,
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

    /// Return a reference to the connection pool
    pub fn pool(&self) -> &sqlx::SqlitePool {
        &self.pool
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