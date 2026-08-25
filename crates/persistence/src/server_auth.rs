use chrono::Utc;
use sqlx::Row;

use super::Database;

impl Database {
    // ============================================================
    // Server auth methods
    // ============================================================

    /// 读取服务端鉴权 token（单行表 id=1，不存在则返回 None）。
    pub async fn load_server_auth(&self) -> Result<Option<String>, sqlx::Error> {
        let row = sqlx::query("SELECT client_token FROM server_auth WHERE id = 1")
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get::<String, _>("client_token")))
    }

    /// 保存或更新服务端鉴权 token（upsert 语义，写入 id=1 单行）。
    pub async fn save_server_auth(&self, token: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"
            INSERT INTO server_auth (id, client_token, updated_at)
            VALUES (1, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                client_token = excluded.client_token,
                updated_at = excluded.updated_at
            ",
        )
        .bind(token)
        .bind(Utc::now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
