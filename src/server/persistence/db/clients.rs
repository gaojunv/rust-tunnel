use chrono::Utc;
use sqlx::Row;

use super::Database;
use super::records::ClientRecord;

impl Database {
    /// Record client connection
    pub async fn record_client_connect(
        &self,
        port: u16,
        hostname: Option<String>,
    ) -> Result<(), sqlx::Error> {
        let now = Utc::now();

        sqlx::query(
            r#"
            INSERT INTO client_sessions (port, hostname, connected_at, disconnected_at, duration_seconds)
            VALUES (?, ?, ?, NULL, NULL)
            "#,
        )
        .bind(port as i32)
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
            r#"
            UPDATE client_sessions
            SET disconnected_at = ?,
                duration_seconds = CAST(strftime('%s', ?) AS INTEGER) - CAST(strftime('%s', connected_at) AS INTEGER)
            WHERE id = (
                SELECT id FROM client_sessions
                WHERE port = ? AND disconnected_at IS NULL
                ORDER BY connected_at DESC
                LIMIT 1
            )
            "#,
        )
        .bind(now)
        .bind(now)
        .bind(port as i32)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    // ============================================================
    // Client registry methods
    // ============================================================

    pub async fn upsert_client(
        &self,
        name: &str,
        hostname: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        let now = Utc::now();
        sqlx::query(
            r#"
            INSERT INTO clients (name, hostname, first_seen_at, last_seen_at)
            VALUES (?, ?, ?, ?)
            ON CONFLICT(name) DO UPDATE SET
                hostname = excluded.hostname,
                last_seen_at = excluded.last_seen_at
            "#,
        )
        .bind(name)
        .bind(hostname)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn touch_client_last_seen(&self, name: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE clients SET last_seen_at = ? WHERE name = ?")
            .bind(Utc::now())
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn list_clients(&self) -> Result<Vec<ClientRecord>, sqlx::Error> {
        let rows = sqlx::query_as::<_, ClientRecord>(
            "SELECT name, hostname, first_seen_at, last_seen_at, note FROM clients ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn get_client(&self, name: &str) -> Result<Option<ClientRecord>, sqlx::Error> {
        sqlx::query_as::<_, ClientRecord>(
            "SELECT name, hostname, first_seen_at, last_seen_at, note FROM clients WHERE name = ?",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await
    }

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
}