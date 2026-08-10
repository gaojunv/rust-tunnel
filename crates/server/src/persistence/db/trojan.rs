use chrono::Utc;

use super::records::TrojanConfigRecord;
use super::Database;

impl Database {
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
}
