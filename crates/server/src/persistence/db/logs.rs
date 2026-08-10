use super::records::DbLogEntry;
use super::Database;

impl Database {
    /// Insert a log entry into the database
    pub async fn insert_log(
        &self,
        entry: &crate::logs::LogEntry,
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
        entries: &[crate::logs::LogEntry],
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
        let entry = crate::logs::LogEntry {
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
        let info_entry = crate::logs::LogEntry {
            id: 0,
            timestamp: 1000000,
            level: "INFO".into(),
            source: "server".into(),
            target: "test".into(),
            message: "info msg".into(),
        };
        let error_entry = crate::logs::LogEntry {
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
        let entry = crate::logs::LogEntry {
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
        let entries: Vec<crate::logs::LogEntry> = (0..3)
            .map(|i| crate::logs::LogEntry {
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
}
