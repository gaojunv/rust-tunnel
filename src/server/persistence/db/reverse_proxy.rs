use super::records::ReverseProxyConfigRecord;
use super::Database;

impl Database {
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
}