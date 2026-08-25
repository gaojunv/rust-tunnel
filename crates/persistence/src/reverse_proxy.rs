use super::records::ReverseProxyConfigRecord;
use super::Database;

impl Database {
    /// 加载反向代理全局配置（单例 id=1），不存在时返回 `None`。
    ///
    /// # Errors
    /// 当数据库查询执行失败时返回 `sqlx::Error`。
    pub async fn load_reverse_proxy_config(
        &self,
    ) -> Result<Option<ReverseProxyConfigRecord>, sqlx::Error> {
        sqlx::query_as::<_, ReverseProxyConfigRecord>(
            "SELECT max_connections, connection_timeout_secs, buffer_size FROM reverse_proxy_config WHERE id = 1",
        )
        .fetch_optional(&self.pool)
        .await
    }

    /// 保存反向代理全局配置（单例 upsert，id=1）。
    ///
    /// # Errors
    /// 当数据库写入或连接失败时返回 `sqlx::Error`。
    pub async fn save_reverse_proxy_config(
        &self,
        max_connections: u32,
        connection_timeout_secs: u64,
        buffer_size: usize,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"
            INSERT INTO reverse_proxy_config (id, max_connections, connection_timeout_secs, buffer_size, updated_at)
            VALUES (1, ?, ?, ?, datetime('now'))
            ON CONFLICT(id) DO UPDATE SET
                max_connections = excluded.max_connections,
                connection_timeout_secs = excluded.connection_timeout_secs,
                buffer_size = excluded.buffer_size,
                updated_at = excluded.updated_at
            ",
        )
        .bind(i64::from(max_connections))
        .bind(i64::try_from(connection_timeout_secs).unwrap_or(i64::MAX))
        .bind(i64::try_from(buffer_size).unwrap_or(i64::MAX))
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
