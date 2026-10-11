use chrono::Utc;

use super::records::RustdeskConfigRecord;
use super::Database;

impl Database {
    /// 保存 RustDesk 会合/中继配置（单行表，按 id = 1 整行替换）。
    ///
    /// hbbs 会合 + hbbr 中继是同一个逻辑服务，表中恒只有一行配置，
    /// 故这里用 `INSERT OR REPLACE` 覆盖 id = 1，并刷新 `updated_at`。
    ///
    /// # Errors
    /// 当数据库写入或连接失败时返回 `sqlx::Error`。
    #[allow(
        clippy::too_many_arguments,
        reason = "RustDesk 配置字段本身就是 8 项扁平设置，显式传参比包装一次性结构体更贴近 replace_trojan_config 的既有写法"
    )]
    pub async fn save_rustdesk_config(
        &self,
        enabled: bool,
        rendezvous_port: u16,
        relay_port: u16,
        key: &str,
        relay_server: &str,
        always_use_relay: bool,
        local_ip: &str,
        mask: &str,
    ) -> Result<(), sqlx::Error> {
        let now = Utc::now();

        sqlx::query(
            r#"
            INSERT OR REPLACE INTO rustdesk_config
                (id, enabled, rendezvous_port, relay_port, "key", relay_server,
                 always_use_relay, local_ip, mask, created_at, updated_at)
            VALUES (1, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(i32::from(enabled))
        .bind(i32::from(rendezvous_port))
        .bind(i32::from(relay_port))
        .bind(key)
        .bind(relay_server)
        .bind(i32::from(always_use_relay))
        .bind(local_ip)
        .bind(mask)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// 加载 RustDesk 配置；表中无记录时返回 None。
    ///
    /// # Errors
    /// 当数据库查询执行失败时返回 `sqlx::Error`。
    pub async fn load_rustdesk_config(
        &self,
    ) -> Result<Option<RustdeskConfigRecord>, sqlx::Error> {
        let record = sqlx::query_as::<_, RustdeskConfigRecord>(
            r#"
            SELECT id, enabled, rendezvous_port, relay_port, "key", relay_server,
                   always_use_relay, local_ip, mask, created_at, updated_at
            FROM rustdesk_config
            WHERE id = 1
            "#,
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(record)
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
    async fn test_rustdesk_config_crud() {
        let db = create_test_db().await;

        // 初始无配置
        assert!(db.load_rustdesk_config().await.unwrap().is_none());

        // Create：写入首行
        db.save_rustdesk_config(
            true,
            21116,
            21117,
            "pubkey-1",
            "relay.example.com:21117",
            false,
            "192.168.1.10",
            "192.168.0.0/16",
        )
        .await
        .unwrap();

        let cfg = db.load_rustdesk_config().await.unwrap().unwrap();
        assert_eq!(cfg.id, 1);
        assert_eq!(cfg.enabled, 1);
        assert_eq!(cfg.rendezvous_port, 21116);
        assert_eq!(cfg.relay_port, 21117);
        assert_eq!(cfg.key, "pubkey-1");
        assert_eq!(cfg.relay_server, "relay.example.com:21117");
        assert_eq!(cfg.always_use_relay, 0);
        assert_eq!(cfg.local_ip, "192.168.1.10");
        assert_eq!(cfg.mask, "192.168.0.0/16");

        // Update：单行表整行替换，不应残留旧值
        db.save_rustdesk_config(false, 21118, 21119, "pubkey-2", "", true, "", "")
            .await
            .unwrap();

        let cfg = db.load_rustdesk_config().await.unwrap().unwrap();
        assert_eq!(cfg.enabled, 0);
        assert_eq!(cfg.rendezvous_port, 21118);
        assert_eq!(cfg.relay_port, 21119);
        assert_eq!(cfg.key, "pubkey-2");
        assert_eq!(cfg.relay_server, "");
        assert_eq!(cfg.always_use_relay, 1);

        // 多次保存后表中仍恒为单行
        db.save_rustdesk_config(true, 21116, 21117, "pubkey-3", "", false, "", "")
            .await
            .unwrap();
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM rustdesk_config")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }
}
