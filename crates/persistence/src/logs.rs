use std::fmt::Write as _;

use super::records::DbLogEntry;
use super::Database;

impl Database {
    /// 插入单条日志，返回自增 id。
    ///
    /// # Errors
    ///
    /// 数据库连接不可用、SQL 执行失败或连接池已关闭时返回 `sqlx::Error`。
    pub async fn insert_log(&self, entry: &DbLogEntry) -> Result<i64, sqlx::Error> {
        let result = sqlx::query(
            r"
            INSERT INTO server_logs (timestamp, level, source, target, message)
            VALUES (?, ?, ?, ?, ?)
            ",
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

    /// 批量插入日志，空切片直接返回。
    ///
    /// # Errors
    ///
    /// 事务开启、任一条插入执行或事务提交失败时返回 `sqlx::Error`。
    pub async fn insert_logs_batch(&self, entries: &[DbLogEntry]) -> Result<(), sqlx::Error> {
        if entries.is_empty() {
            return Ok(());
        }

        let mut tx = self.pool.begin().await?;

        for entry in entries {
            sqlx::query(
                r"
                INSERT INTO server_logs (timestamp, level, source, target, message)
                VALUES (?, ?, ?, ?, ?)
                ",
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

    /// 按条件查询日志，返回按 id 升序（时间正序）排列的结果。
    ///
    /// `level` 为等级下界过滤（error/warn/info/debug/trace，未知值回退为全量）；
    /// `source`/`search` 分别对 `source` 前缀和 `message` 模糊匹配；`before_id` 限制
    /// `id < before_id` 的分页游标；`limit` 控制返回条数。
    ///
    /// # Errors
    ///
    /// SQL 构造或执行失败、数据库连接不可用时返回 `sqlx::Error`。
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
                _ => vec!["ERROR", "WARN", "INFO", "DEBUG", "TRACE"],
            };
            let placeholders: Vec<String> = levels
                .iter()
                .enumerate()
                .map(|(i, _)| format!("?{}", i + 1))
                .collect();
            let _ = write!(query_str, " AND level IN ({})", placeholders.join(","));
            for l in levels {
                params.push(l.to_string());
            }
        }

        if let Some(src) = source {
            params.push(format!("{src}%"));
            let idx = params.len();
            let _ = write!(query_str, " AND source LIKE ?{idx}");
        }

        if let Some(s) = search {
            params.push(format!("%{s}%"));
            let idx = params.len();
            let _ = write!(query_str, " AND message LIKE ?{idx}");
        }

        if let Some(before) = before_id {
            params.push(before.to_string());
            let idx = params.len();
            let _ = write!(query_str, " AND id < ?{idx}");
        }

        let idx = params.len() + 1;
        let _ = write!(query_str, " ORDER BY id DESC LIMIT ?{idx}");
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

    /// 删除指定时间戳之前的旧日志，返回被删除的行数。
    ///
    /// # Errors
    ///
    /// SQL 执行失败或数据库连接不可用时返回 `sqlx::Error`。
    pub async fn cleanup_old_logs(&self, older_than_micros: i64) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            r"
            DELETE FROM server_logs
            WHERE timestamp < ?
            ",
        )
        .bind(older_than_micros)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected())
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
        let entry = DbLogEntry {
            id: 0,
            timestamp: 1_000_000,
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
        let info_entry = DbLogEntry {
            id: 0,
            timestamp: 1_000_000,
            level: "INFO".into(),
            source: "server".into(),
            target: "test".into(),
            message: "info msg".into(),
        };
        let error_entry = DbLogEntry {
            id: 0,
            timestamp: 2_000_000,
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
        let entry = DbLogEntry {
            id: 0,
            timestamp: 1_000_000,
            level: "INFO".into(),
            source: "server".into(),
            target: "test".into(),
            message: "old".into(),
        };
        db.insert_log(&entry).await.unwrap();

        // Cleanup anything older than 2000000
        let deleted = db.cleanup_old_logs(2_000_000).await.unwrap();
        assert_eq!(deleted, 1);

        let results = db.query_logs(None, None, None, 10, None).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_insert_logs_batch() {
        let db = create_test_db().await;
        let entries: Vec<DbLogEntry> = (0..3)
            .map(|i| DbLogEntry {
                id: 0,
                timestamp: 1_000_000 + i * 1000,
                level: "INFO".into(),
                source: "server".into(),
                target: "test".into(),
                message: format!("msg {i}"),
            })
            .collect();
        db.insert_logs_batch(&entries).await.unwrap();

        let results = db.query_logs(None, None, None, 10, None).await.unwrap();
        assert_eq!(results.len(), 3);
    }
}
