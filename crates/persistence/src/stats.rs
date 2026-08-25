use chrono::{DateTime, Utc};

use super::Database;
use std::fmt::Write as _;

use super::StatsSnapshotRow;

impl Database {
    // ============================================================
    // Stats snapshots methods
    // ============================================================

    /// Query stats snapshots within a time range, optionally filtered.
    ///
    /// # Errors
    /// 当数据库查询执行失败时返回 `sqlx::Error`。
    pub async fn query_stats_snapshots(
        &self,
        entity_types: &[String],
        entity_ids: &[String],
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<StatsSnapshotRow>, sqlx::Error> {
        let mut sql = String::from(
            "SELECT entity_type, entity_id, timestamp, bytes_in, bytes_out, \
             bytes_in_rate, bytes_out_rate, rtt_ms, loss_pct, active_conns \
             FROM stats_snapshots WHERE timestamp >= ? AND timestamp <= ?",
        );
        let mut param_idx = 3;
        if !entity_types.is_empty() {
            let placeholders: Vec<String> = (0..entity_types.len())
                .map(|i| format!("?{}", param_idx + i))
                .collect();
            let _ = write!(sql, " AND entity_type IN ({})", placeholders.join(","));
            param_idx += entity_types.len();
        }
        if !entity_ids.is_empty() {
            let placeholders: Vec<String> = (0..entity_ids.len())
                .map(|i| format!("?{}", param_idx + i))
                .collect();
            let _ = write!(sql, " AND entity_id IN ({})", placeholders.join(","));
        }
        sql.push_str(" ORDER BY timestamp");

        let mut query = sqlx::query_as::<_, StatsSnapshotRow>(&sql)
            .bind(start)
            .bind(end);
        for et in entity_types {
            query = query.bind(et);
        }
        for eid in entity_ids {
            query = query.bind(eid);
        }
        query.fetch_all(&self.pool).await
    }

    /// Delete stats snapshots older than the given timestamp.
    ///
    /// # Errors
    /// 当数据库删除执行失败时返回 `sqlx::Error`。
    pub async fn cleanup_old_stats_snapshots(
        &self,
        before: DateTime<Utc>,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query("DELETE FROM stats_snapshots WHERE timestamp < ?")
            .bind(before)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::Database;
    use chrono::Timelike;

    async fn create_test_db() -> Database {
        Database::new(":memory:")
            .await
            .expect("Failed to create in-memory database")
    }

    #[tokio::test]
    async fn test_stats_snapshots_empty_query() {
        let db = create_test_db().await;
        let rows = db
            .query_stats_snapshots(
                &[],
                &[],
                chrono::Utc::now() - chrono::Duration::hours(1),
                chrono::Utc::now(),
            )
            .await
            .unwrap();
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn test_stats_snapshots_filter_by_type() {
        let db = create_test_db().await;
        let now = chrono::Utc::now();
        let ts = now
            - chrono::Duration::seconds(i64::from(now.second()))
            - chrono::Duration::nanoseconds(i64::from(now.nanosecond()));
        // Insert test data directly
        sqlx::query(
            "INSERT INTO stats_snapshots (entity_type, entity_id, timestamp, bytes_in, bytes_out, bytes_in_rate, bytes_out_rate, rtt_ms, loss_pct, active_conns) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind("proxy")
        .bind("rule1")
        .bind(ts)
        .bind(100_i64)
        .bind(200_i64)
        .bind(10.0_f64)
        .bind(20.0_f64)
        .bind(None::<f64>)
        .bind(None::<f64>)
        .bind(3_i32)
        .execute(&db.pool)
        .await
        .unwrap();

        // Query with type filter
        let rows = db
            .query_stats_snapshots(
                &["proxy".to_string()],
                &[],
                ts - chrono::Duration::minutes(1),
                ts + chrono::Duration::minutes(1),
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].entity_type, "proxy");
        assert_eq!(rows[0].entity_id, "rule1");
        assert_eq!(rows[0].bytes_in, 100);
        assert_eq!(rows[0].bytes_out, 200);

        // Query with wrong type should return empty
        let rows = db
            .query_stats_snapshots(
                &["shadowsocks".to_string()],
                &[],
                ts - chrono::Duration::minutes(1),
                ts + chrono::Duration::minutes(1),
            )
            .await
            .unwrap();
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn test_stats_snapshots_filter_by_id() {
        let db = create_test_db().await;
        let now = chrono::Utc::now();
        let ts = now
            - chrono::Duration::seconds(i64::from(now.second()))
            - chrono::Duration::nanoseconds(i64::from(now.nanosecond()));
        sqlx::query(
            "INSERT INTO stats_snapshots (entity_type, entity_id, timestamp, bytes_in, bytes_out, bytes_in_rate, bytes_out_rate, rtt_ms, loss_pct, active_conns) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind("client")
        .bind("home-nas")
        .bind(ts)
        .bind(50_i64)
        .bind(60_i64)
        .bind(0.0_f64)
        .bind(0.0_f64)
        .bind(Some(5.2_f64))
        .bind(None::<f64>)
        .bind(1_i32)
        .execute(&db.pool)
        .await
        .unwrap();

        let rows = db
            .query_stats_snapshots(
                &[],
                &["home-nas".to_string()],
                ts - chrono::Duration::minutes(1),
                ts + chrono::Duration::minutes(1),
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].entity_type, "client");
        assert_eq!(rows[0].rtt_ms, Some(5.2));
    }

    #[tokio::test]
    async fn test_cleanup_old_stats_snapshots() {
        let db = create_test_db().await;
        let now = chrono::Utc::now();
        let ts_old = now - chrono::Duration::days(10);
        let ts_new = now - chrono::Duration::minutes(30);

        for (ts, label) in [(ts_old, "old"), (ts_new, "new")] {
            sqlx::query(
                "INSERT INTO stats_snapshots (entity_type, entity_id, timestamp, bytes_in, bytes_out, bytes_in_rate, bytes_out_rate, rtt_ms, loss_pct, active_conns) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(label)
            .bind("test")
            .bind(ts)
            .bind(0_i64)
            .bind(0_i64)
            .bind(0.0_f64)
            .bind(0.0_f64)
            .bind(None::<f64>)
            .bind(None::<f64>)
            .bind(0_i32)
            .execute(&db.pool)
            .await
            .unwrap();
        }

        let cutoff = now - chrono::Duration::days(7);
        let deleted = db.cleanup_old_stats_snapshots(cutoff).await.unwrap();
        assert_eq!(deleted, 1);

        let rows = db
            .query_stats_snapshots(
                &[],
                &[],
                now - chrono::Duration::days(30),
                now + chrono::Duration::minutes(1),
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].entity_type, "new");
    }

    #[tokio::test]
    async fn test_stats_snapshots_time_range() {
        let db = create_test_db().await;
        let now = chrono::Utc::now();
        let ts1 = now - chrono::Duration::hours(3);
        let ts2 = now - chrono::Duration::hours(2);
        let ts3 = now - chrono::Duration::hours(1);

        for (ts, idx) in [(ts1, 1), (ts2, 2), (ts3, 3)] {
            sqlx::query(
                "INSERT INTO stats_snapshots (entity_type, entity_id, timestamp, bytes_in, bytes_out, bytes_in_rate, bytes_out_rate, rtt_ms, loss_pct, active_conns) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind("proxy")
            .bind(format!("rule{idx}"))
            .bind(ts)
            .bind(i64::from(idx) * 100)
            .bind(0_i64)
            .bind(0.0_f64)
            .bind(0.0_f64)
            .bind(None::<f64>)
            .bind(None::<f64>)
            .bind(0_i32)
            .execute(&db.pool)
            .await
            .unwrap();
        }

        // Query middle period: only ts2 at -120min should be in [-150min, -90min]
        let rows = db
            .query_stats_snapshots(
                &[],
                &[],
                now - chrono::Duration::minutes(150),
                now - chrono::Duration::minutes(90),
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1); // only ts2
        assert_eq!(rows[0].entity_id, "rule2");
    }
}
