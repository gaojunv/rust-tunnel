// 测试代码豁免 panic 风险 lint（生产代码仍告警）
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! 持久化层：基于 SQLite 的仓储实现，封装连接池、表结构初始化与各域数据的读写访问。

use sqlx::{
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    Pool, Sqlite,
};
use std::path::Path;
use std::str::FromStr;

/// ACME 证书与挑战记录的持久化操作。
pub mod acme;
/// Agent 工作区、会话与消息的持久化操作。
pub mod agent;
/// 客户端注册表与连接会话历史的持久化操作。
pub mod clients;
/// DNS 配置的持久化操作。
pub mod dns;
/// LLM 网关（提供商、模型、密钥、用量）的持久化操作。
pub mod llm;
/// 服务端日志的持久化操作。
pub mod logs;
/// AI 记忆体的持久化操作。
pub mod memory;
/// Mesh 网络与服务的持久化操作。
pub mod mesh;
/// 代理规则的持久化操作。
pub mod proxy_rules;
/// RAG 知识库、文档与分块的持久化操作。
pub mod rag;
/// 数据库行记录类型定义。
pub mod records;
/// 反向代理全局配置的持久化操作。
pub mod reverse_proxy;
/// Agent 角色定义的持久化操作。
pub mod roles;
/// 数据库表结构初始化与迁移。
pub mod schema;
/// 服务端鉴权 token 的持久化操作。
pub mod server_auth;
/// 服务端通用键值设置的持久化操作。
pub mod settings;
/// Shadowsocks 配置的持久化操作。
pub mod shadowsocks;
/// Agent Skill 库的持久化操作。
pub mod skills;
/// 统计快照的持久化操作。
pub mod stats;
/// 工具调用结果的辅助持久化逻辑。
pub mod tool_result;
/// Trojan 配置的持久化操作。
pub mod trojan;
/// Wiki 容器、文档与页面的持久化操作。
pub mod wiki;
pub use llm::*;
pub use records::*;

/// Database wrapper for persistence
#[derive(Debug, Clone)]
pub struct Database {
    /// 连接池。pub 可见性：server 侧 12+ 处直接 `&db.pool` 传入 sqlx query，
    /// 与 `pool()` 方法并存（方法返回 `&SqlitePool` 等价类型）。
    pub pool: Pool<Sqlite>,
}

impl Database {
    /// Create a new database instance and initialize schema
    ///
    /// # Errors
    /// 当父目录创建、数据库连接或表结构初始化失败时返回 `sqlx::Error`。
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
    #[must_use]
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
            Some(r"[]"),
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

    /// 旧库（proxy_rules 的 CHECK 不含 'llm'）经 Database::new 迁移后应重建表
    /// 并接受 'llm' 类型。回归：迁移曾把 BEGIN EXCLUSIVE 等多语句打散在连接池上，
    /// 不同连接互相锁死报 "database is locked"，进程启动即退出。
    #[tokio::test]
    async fn migration_rebuilds_proxy_rules_check_for_llm() {
        let tempdir = tempfile::TempDir::new().unwrap();
        let db_path = tempdir.path().join("migrate_llm_check.db");

        // 用旧 schema（CHECK 不含 'llm'）手工建表并插入一行存量数据
        {
            let opts = sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&db_path)
                .create_if_missing(true);
            let pool = sqlx::SqlitePool::connect_with(opts).await.unwrap();
            sqlx::query(
                r"
                CREATE TABLE proxy_rules (
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
                ",
            )
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO proxy_rules (id, name, type, listen_addr, created_at, updated_at) \
                 VALUES ('r-old', 'old-rule', 'http', '0.0.0.0:8080', '2026-01-01 00:00:00', '2026-01-01 00:00:00')",
            )
            .execute(&pool)
            .await
            .unwrap();
            pool.close().await;
        }

        // Database::new 必须成功完成迁移（此前报 database is locked）
        let db = Database::new(db_path.to_str().unwrap()).await.unwrap();

        // 迁移后 'llm' 类型可插入，存量行保留
        sqlx::query(
            "INSERT INTO proxy_rules (id, name, type, listen_addr, created_at, updated_at) \
             VALUES ('r-llm', 'llm-rule', 'llm', '0.0.0.0:9000', datetime('now'), datetime('now'))",
        )
        .execute(db.pool())
        .await
        .expect("迁移后 proxy_rules 应接受 'llm' 类型");

        let rules = db.load_proxy_rules().await.unwrap();
        assert_eq!(rules.len(), 2);
        assert!(rules.iter().any(|r| r.id == "r-old"), "存量行应保留");
        // 不应残留迁移中间表
        let leftover: Vec<(String,)> = sqlx::query_as(
            "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'proxy_rules_new%'",
        )
        .fetch_all(db.pool())
        .await
        .unwrap();
        assert!(leftover.is_empty(), "不应残留 proxy_rules_new 中间表");
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
                r"
                CREATE TABLE trojan_config (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    port INTEGER NOT NULL UNIQUE,
                    password TEXT NOT NULL,
                    fallback TEXT NOT NULL DEFAULT '127.0.0.1:80',
                    enabled INTEGER NOT NULL DEFAULT 1,
                    created_at DATETIME NOT NULL,
                    updated_at DATETIME NOT NULL
                )
                ",
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
