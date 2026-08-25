use chrono::Utc;

use super::records::ShadowsocksConfigRecord;
use super::Database;

impl Database {
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
            r"
            INSERT INTO shadowsocks_config (port, cipher, password, enabled, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?)
            ON CONFLICT(port) DO UPDATE SET
                cipher = excluded.cipher,
                password = excluded.password,
                enabled = excluded.enabled,
                updated_at = excluded.updated_at
            ",
        )
        .bind(i32::from(port))
        .bind(cipher)
        .bind(password)
        .bind(i32::from(enabled))
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
            r"
            INSERT INTO shadowsocks_config (port, cipher, password, enabled, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?)
            ",
        )
        .bind(i32::from(port))
        .bind(cipher)
        .bind(password)
        .bind(i32::from(enabled))
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
            r"
            SELECT id, port, cipher, password, enabled, created_at, updated_at
            FROM shadowsocks_config
            ORDER BY port
            ",
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
            r"
            SELECT id, port, cipher, password, enabled, created_at, updated_at
            FROM shadowsocks_config
            WHERE enabled = 1
            ORDER BY port
            ",
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
            r"
            SELECT id, port, cipher, password, enabled, created_at, updated_at
            FROM shadowsocks_config
            WHERE port = ?
            ",
        )
        .bind(i32::from(port))
        .fetch_optional(&self.pool)
        .await?;

        Ok(record)
    }

    /// Delete Shadowsocks configuration
    pub async fn delete_shadowsocks_config(&self, port: u16) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"
            DELETE FROM shadowsocks_config
            WHERE port = ?
            ",
        )
        .bind(i32::from(port))
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
    async fn test_delete_nonexistent_shadowsocks_config() {
        let db = create_test_db().await;
        // Should not error
        db.delete_shadowsocks_config(9999).await.unwrap();
    }
}
