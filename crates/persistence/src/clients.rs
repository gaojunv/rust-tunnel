use chrono::Utc;
use sqlx::Row;

use super::records::ClientRecord;
use super::Database;

impl Database {
    /// 记录客户端连接会话（写入 `client_sessions`）。
    ///
    /// # Errors
    /// 当数据库写入或连接失败时返回 `sqlx::Error`。
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

    /// 记录客户端断开（更新最近一条未断开会话的 `disconnected_at` 与时长）。
    ///
    /// # Errors
    /// 当数据库查询或更新执行失败时返回 `sqlx::Error`。
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
    ///
    /// # Errors
    /// 当数据库写入或连接失败时返回 `sqlx::Error`。
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
    ///
    /// # Errors
    /// 当数据库更新执行失败时返回 `sqlx::Error`。
    pub async fn touch_client_last_seen(&self, name: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE clients SET last_seen_at = ? WHERE name = ?")
            .bind(Utc::now())
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 列出所有已注册的客户端记录（按名称排序）。
    ///
    /// # Errors
    /// 当数据库查询执行失败时返回 `sqlx::Error`。
    pub async fn list_clients(&self) -> Result<Vec<ClientRecord>, sqlx::Error> {
        let rows = sqlx::query_as::<_, ClientRecord>(
            "SELECT name, hostname, first_seen_at, last_seen_at, note FROM clients ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// 按名称查询单个客户端记录，不存在则返回 None。
    ///
    /// # Errors
    /// 当数据库查询执行失败时返回 `sqlx::Error`。
    pub async fn get_client(&self, name: &str) -> Result<Option<ClientRecord>, sqlx::Error> {
        sqlx::query_as::<_, ClientRecord>(
            "SELECT name, hostname, first_seen_at, last_seen_at, note FROM clients WHERE name = ?",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await
    }

    /// 更新指定客户端的备注（None 表示清空备注）。
    ///
    /// # Errors
    /// 当数据库更新执行失败时返回 `sqlx::Error`。
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

    // ============================================================
    // 远程控制：RustDesk ID 分配
    // ============================================================

    /// 读取客户端已分配的 RustDesk ID；未分配或客户端不存在时返回 `None`。
    ///
    /// # Errors
    /// 当数据库查询执行失败时返回 `sqlx::Error`。
    pub async fn get_remote_id(&self, name: &str) -> Result<Option<String>, sqlx::Error> {
        let row: Option<(Option<String>,)> =
            sqlx::query_as("SELECT remote_id FROM clients WHERE name = ?")
                .bind(name)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.and_then(|(id,)| id))
    }

    /// 分配（或复用）客户端的稳定 RustDesk ID：9 位数字。
    ///
    /// 位数下限来自 hbbs 的 `MIN_PEER_ID_LEN = 6`，故 9 位数字必然合法。ID 由
    /// 服务端分配而非客户端自选：同一 NAT 下多客户端共用一个公网 IP，而 hbbs 按
    /// IP 限制 24h 内出现的不同 ID 数，自选 ID 极易撞限流。
    ///
    /// 幂等：已分配则原样返回。随机 ID 撞唯一索引时重试，最多 8 次。
    ///
    /// # Errors
    /// 数据库读写失败时返回 `sqlx::Error`；客户端记录不存在时返回
    /// [`sqlx::Error::RowNotFound`]（调用方须先 [`Database::upsert_client`]）；
    /// 重试耗尽时返回 [`sqlx::Error::Protocol`]。
    pub async fn allocate_remote_id(&self, name: &str) -> Result<String, sqlx::Error> {
        for _ in 0..8 {
            if let Some(existing) = self.get_remote_id(name).await? {
                return Ok(existing);
            }
            // 先取模再取绝对值：random() 可能返回 i64::MIN，SQLite 的 abs(i64::MIN)
            // 是 NULL，会让 remote_id 被写成 NULL（唯一索引拦不住）。
            let outcome = sqlx::query(
                r"UPDATE clients
                  SET remote_id = printf('%09d', abs(random() % 1000000000))
                  WHERE name = ? AND remote_id IS NULL",
            )
            .bind(name)
            .execute(&self.pool)
            .await;
            match outcome {
                Ok(res) if res.rows_affected() == 1 => {
                    // 回读拿实际落库值，避免与 SQL 侧的格式化逻辑各写一份。
                    if let Some(id) = self.get_remote_id(name).await? {
                        return Ok(id);
                    }
                }
                Ok(_) => {
                    // 影响 0 行：行不存在，或并发下刚被别处分配（下一轮直接命中）。
                    if self.get_remote_id(name).await?.is_some() {
                        continue;
                    }
                    return Err(sqlx::Error::RowNotFound);
                }
                // 撞上唯一索引：不 return，落回循环顶部换一个随机值重试。
                Err(e) if is_unique_violation(&e) => {}
                Err(e) => return Err(e),
            }
        }
        Err(sqlx::Error::Protocol(
            "allocate_remote_id: 随机 ID 重试耗尽".into(),
        ))
    }

    /// 删除指定名称的客户端注册记录。
    ///
    /// # Errors
    /// 当数据库删除执行失败时返回 `sqlx::Error`。
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
    ///
    /// # Errors
    /// 当数据库查询执行失败时返回 `sqlx::Error`。
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

/// 判断一个 `sqlx` 错误是否为唯一索引冲突（SQLite `SQLITE_CONSTRAINT_UNIQUE`）。
///
/// 走错误文案而非错误码：`sqlx` 的 `DatabaseError::code()` 在 SQLite 后端返回的
/// 是扩展码字符串，不同版本表述不一，而文案在 SQLite 侧长期稳定。
fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.message().contains("UNIQUE constraint failed"))
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
    async fn test_allocate_remote_id_is_stable_and_nine_digits() {
        let db = Database::new(":memory:").await.unwrap();
        db.upsert_client("home-nas", None).await.unwrap();

        let first = db.allocate_remote_id("home-nas").await.unwrap();
        assert_eq!(first.len(), 9, "ID 应为 9 位数字：{first}");
        assert!(first.chars().all(|c| c.is_ascii_digit()), "ID 应全为数字");

        // 幂等：重复调用必须返回同一个 ID（换 ID 会撞 hbbs 的单 IP ID 数限制）。
        let second = db.allocate_remote_id("home-nas").await.unwrap();
        assert_eq!(first, second);

        // 落库可见。
        assert_eq!(db.get_remote_id("home-nas").await.unwrap().as_deref(), Some(&*first));
    }

    #[tokio::test]
    async fn test_allocate_remote_id_distinct_per_client() {
        let db = Database::new(":memory:").await.unwrap();
        db.upsert_client("a-client", None).await.unwrap();
        db.upsert_client("b-client", None).await.unwrap();
        let a = db.allocate_remote_id("a-client").await.unwrap();
        let b = db.allocate_remote_id("b-client").await.unwrap();
        assert_ne!(a, b, "不同客户端应拿到不同 ID");
    }

    #[tokio::test]
    async fn test_allocate_remote_id_unknown_client_is_row_not_found() {
        let db = Database::new(":memory:").await.unwrap();
        // 未 upsert 的客户端：分配应报 RowNotFound，而不是写入幽灵行。
        let err = db.allocate_remote_id("ghost").await.unwrap_err();
        assert!(matches!(err, sqlx::Error::RowNotFound), "got {err:?}");
        assert!(db.get_remote_id("ghost").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_get_remote_id_unallocated_is_none() {
        let db = Database::new(":memory:").await.unwrap();
        db.upsert_client("home-nas", None).await.unwrap();
        assert!(db.get_remote_id("home-nas").await.unwrap().is_none());
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
