use chrono::Utc;
use sqlx::Row;

use super::records::ClientRecord;
use super::Database;

impl Database {
    /// Record client connection
    pub async fn record_client_connect(
        &self,
        port: u16,
        hostname: Option<String>,
    ) -> Result<(), sqlx::Error> {
        let now = Utc::now();

        sqlx::query(
            r"
            INSERT INTO client_sessions (port, hostname, connected_at, disconnected_at, duration_seconds)
            VALUES (?, ?, ?, NULL, NULL)
            ",
        )
        .bind(i32::from(port))
        .bind(hostname)
        .bind(now)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Record client disconnection
    pub async fn record_client_disconnect(&self, port: u16) -> Result<(), sqlx::Error> {
        let now = Utc::now();

        // Update the most recent session for this port that's still connected
        // Use a subquery to find the latest session since SQLite < 3.33 doesn't support
        // UPDATE ... ORDER BY ... LIMIT
        sqlx::query(
            r"
            UPDATE client_sessions
            SET disconnected_at = ?,
                duration_seconds = CAST(strftime('%s', ?) AS INTEGER) - CAST(strftime('%s', connected_at) AS INTEGER)
            WHERE id = (
                SELECT id FROM client_sessions
                WHERE port = ? AND disconnected_at IS NULL
                ORDER BY connected_at DESC
                LIMIT 1
            )
            ",
        )
        .bind(now)
        .bind(now)
        .bind(i32::from(port))
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    // ============================================================
    // Client registry methods
    // ============================================================

    /// 插入或更新客户端注册记录（按 name 去重，更新 hostname 与 last_seen_at）。
    pub async fn upsert_client(
        &self,
        name: &str,
        hostname: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        let now = Utc::now();
        sqlx::query(
            r"
            INSERT INTO clients (name, hostname, first_seen_at, last_seen_at)
            VALUES (?, ?, ?, ?)
            ON CONFLICT(name) DO UPDATE SET
                hostname = excluded.hostname,
                last_seen_at = excluded.last_seen_at
            ",
        )
        .bind(name)
        .bind(hostname)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 刷新指定客户端的最后可见时间（更新 last_seen_at 为当前时间）。
    pub async fn touch_client_last_seen(&self, name: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE clients SET last_seen_at = ? WHERE name = ?")
            .bind(Utc::now())
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 列出所有已注册的客户端记录（按名称排序）。
    pub async fn list_clients(&self) -> Result<Vec<ClientRecord>, sqlx::Error> {
        let rows = sqlx::query_as::<_, ClientRecord>(
            "SELECT name, hostname, first_seen_at, last_seen_at, note FROM clients ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// 按名称查询单个客户端记录，不存在则返回 None。
    pub async fn get_client(&self, name: &str) -> Result<Option<ClientRecord>, sqlx::Error> {
        sqlx::query_as::<_, ClientRecord>(
            "SELECT name, hostname, first_seen_at, last_seen_at, note FROM clients WHERE name = ?",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await
    }

    /// 更新指定客户端的备注（None 表示清空备注）。
    pub async fn update_client_note(
        &self,
        name: &str,
        note: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE clients SET note = ? WHERE name = ?")
            .bind(note)
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 删除指定名称的客户端注册记录。
    pub async fn delete_client(&self, name: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM clients WHERE name = ?")
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Return `(rule_id, rule_name)` pairs for every proxy rule whose routes
    /// JSON contains a backend with `kind == "client"` and matching
    /// `client_name`. Used to enforce "reject delete when referenced" (spec §2.4).
    pub async fn rules_referencing_client(
        &self,
        client_name: &str,
    ) -> Result<Vec<(String, String)>, sqlx::Error> {
        let rows = sqlx::query("SELECT id, name, routes FROM proxy_rules")
            .fetch_all(&self.pool)
            .await?;
        let mut out = Vec::new();
        for row in rows {
            let id: String = row.get("id");
            let name: String = row.get("name");
            let routes_json: Option<String> = row.get("routes");
            let Some(routes_json) = routes_json else {
                continue;
            };
            let Ok(routes) = serde_json::from_str::<serde_json::Value>(&routes_json) else {
                continue;
            };
            let Some(arr) = routes.as_array() else {
                continue;
            };
            let mut hit = false;
            'route: for r in arr {
                let Some(backends) = r.get("backends").and_then(|v| v.as_array()) else {
                    continue;
                };
                for b in backends {
                    let kind = b.get("kind").and_then(|v| v.as_str()).unwrap_or("direct");
                    let cn = b.get("client_name").and_then(|v| v.as_str()).unwrap_or("");
                    if kind == "client" && cn == client_name {
                        hit = true;
                        break 'route;
                    }
                }
            }
            if hit {
                out.push((id, name));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::Database;

    #[tokio::test]
    async fn test_upsert_and_list_clients() {
        let db = Database::new(":memory:").await.unwrap();
        db.upsert_client("home-nas", Some("nas.local"))
            .await
            .unwrap();
        db.upsert_client("home-nas", Some("nas.local"))
            .await
            .unwrap(); // idempotent
        db.upsert_client("office-pc", None).await.unwrap();

        let list = db.list_clients().await.unwrap();
        assert_eq!(list.len(), 2);
        let nas = list.iter().find(|c| c.name == "home-nas").unwrap();
        assert_eq!(nas.hostname.as_deref(), Some("nas.local"));
    }

    #[tokio::test]
    async fn test_touch_client_last_seen() {
        let db = Database::new(":memory:").await.unwrap();
        db.upsert_client("home-nas", None).await.unwrap();
        let before = db
            .get_client("home-nas")
            .await
            .unwrap()
            .unwrap()
            .last_seen_at;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        db.touch_client_last_seen("home-nas").await.unwrap();
        let after = db
            .get_client("home-nas")
            .await
            .unwrap()
            .unwrap()
            .last_seen_at;
        assert!(after > before);
    }

    #[tokio::test]
    async fn test_update_client_note() {
        let db = Database::new(":memory:").await.unwrap();
        db.upsert_client("home-nas", None).await.unwrap();
        db.update_client_note("home-nas", Some("primary"))
            .await
            .unwrap();
        assert_eq!(
            db.get_client("home-nas")
                .await
                .unwrap()
                .unwrap()
                .note
                .as_deref(),
            Some("primary")
        );
        db.update_client_note("home-nas", None).await.unwrap();
        assert!(db
            .get_client("home-nas")
            .await
            .unwrap()
            .unwrap()
            .note
            .is_none());
    }

    #[tokio::test]
    async fn test_delete_client() {
        let db = Database::new(":memory:").await.unwrap();
        db.upsert_client("home-nas", None).await.unwrap();
        db.delete_client("home-nas").await.unwrap();
        let list = db.list_clients().await.unwrap();
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn test_rules_referencing_client() {
        let db = Database::new(":memory:").await.unwrap();

        // route JSON: 一个 backend 指向 home-nas，一个 direct
        let routes_json = serde_json::json!([
            {
                "path": "/",
                "backends": [
                    { "kind": "client", "addr": "localhost:80", "client_name": "home-nas",
                      "weight": 100, "protocol": "http1", "scheme": "http" },
                    { "kind": "direct", "addr": "10.0.0.1:80",
                      "weight": 100, "protocol": "http1", "scheme": "http" }
                ],
                "load_balancing": "round_robin"
            }
        ])
        .to_string();

        db.save_proxy_rule(
            "rule-1",
            "web",
            "http",
            "0.0.0.0:80",
            None,
            Some(&routes_json),
            false,
            false,
            None,
            true,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        let refs = db.rules_referencing_client("home-nas").await.unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].0, "rule-1");
        assert_eq!(refs[0].1, "web");

        let refs = db.rules_referencing_client("nonexistent").await.unwrap();
        assert!(refs.is_empty());
    }

    #[tokio::test]
    async fn test_rules_referencing_client_ignores_direct_only() {
        let db = Database::new(":memory:").await.unwrap();
        let routes_json = serde_json::json!([{
            "path": "/",
            "backends": [
                { "kind": "direct", "addr": "10.0.0.1:80",
                  "weight": 100, "protocol": "http1", "scheme": "http" }
            ],
            "load_balancing": "round_robin"
        }])
        .to_string();
        db.save_proxy_rule(
            "r1",
            "web",
            "http",
            "0.0.0.0:80",
            None,
            Some(&routes_json),
            false,
            false,
            None,
            true,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert!(db
            .rules_referencing_client("anything")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn test_rules_referencing_client_null_routes() {
        // rule with routes = NULL (e.g. tcp rule)
        let db = Database::new(":memory:").await.unwrap();
        db.save_proxy_rule(
            "r1",
            "tcp-rule",
            "tcp",
            "0.0.0.0:9000",
            None,
            None,
            false,
            false,
            None,
            true,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert!(db
            .rules_referencing_client("anyone")
            .await
            .unwrap()
            .is_empty());
    }
}
