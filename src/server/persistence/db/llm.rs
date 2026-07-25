use super::Database;

// ── Record types ──────────────────────────────────────────────

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct LlmProviderRecord {
    pub id: String,
    pub name: String,
    pub provider_type: String,
    pub base_url: String,
    pub api_key: String,
    pub extra_config: Option<String>,
    pub anthropic_base_url: Option<String>,
    pub enabled: i32,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct LlmModelRecord {
    pub id: String,
    pub provider_id: String,
    pub model_name: String,
    pub alias: String,
    pub tags: String,
    pub enabled: i32,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct LlmApiKeyRecord {
    pub id: String,
    pub key_hash: String,
    pub key_prefix: String,
    pub name: String,
    pub enabled: i32,
    pub created_at: String,
    pub last_used_at: Option<String>,
}

/// 一条 LLM 网关请求的用量日志。
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct LlmUsageLogRecord {
    pub id: String,
    pub timestamp: String,
    pub api_key_id: Option<String>,
    pub api_key_name: String,
    pub provider_id: Option<String>,
    pub provider_name: String,
    pub model_id: Option<String>,
    pub model_name: String,
    pub requested_model: String,
    pub protocol: String,
    pub stream: i32,
    pub status_code: i32,
    pub success: i32,
    pub prompt_tokens: i64,
    pub cache_hit_tokens: i64,
    pub cache_miss_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub latency_ms: i64,
    pub error_type: Option<String>,
}

/// 待插入的用量日志（各标识可空——认证/路由失败时部分字段缺失）。
#[derive(Debug, Clone, Default)]
pub struct LlmUsageInsert {
    pub api_key_id: Option<String>,
    pub api_key_name: String,
    pub provider_id: Option<String>,
    pub provider_name: String,
    pub model_id: Option<String>,
    pub model_name: String,
    pub requested_model: String,
    pub protocol: String,
    pub stream: bool,
    pub status_code: i32,
    pub success: bool,
    pub prompt_tokens: i64,
    pub cache_hit_tokens: i64,
    pub cache_miss_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub latency_ms: i64,
    pub error_type: Option<String>,
}

/// 一个聚合维度的用量汇总行。
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct LlmUsageAggregateRow {
    /// 维度标识（api_key_id / model_id / provider_id）；可空表示未归类。
    pub dimension_id: Option<String>,
    /// 维度展示名（冗余存的 *_name）。
    pub dimension_name: String,
    pub requests: i64,
    pub success: i64,
    pub prompt_tokens: i64,
    pub cache_hit_tokens: i64,
    pub cache_miss_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
}

/// 时间范围内的用量总览。
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct LlmUsageSummary {
    pub requests: i64,
    pub success: i64,
    pub prompt_tokens: i64,
    pub cache_hit_tokens: i64,
    pub cache_miss_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
}

// ── Provider CRUD ─────────────────────────────────────────────

impl Database {
    pub async fn llm_list_providers(&self) -> Result<Vec<LlmProviderRecord>, sqlx::Error> {
        sqlx::query_as::<_, LlmProviderRecord>("SELECT * FROM llm_providers ORDER BY created_at")
            .fetch_all(&self.pool)
            .await
    }

    pub async fn llm_list_enabled_providers(&self) -> Result<Vec<LlmProviderRecord>, sqlx::Error> {
        sqlx::query_as::<_, LlmProviderRecord>(
            "SELECT * FROM llm_providers WHERE enabled = 1 ORDER BY created_at",
        )
        .fetch_all(&self.pool)
        .await
    }

    pub async fn llm_get_provider(
        &self,
        id: &str,
    ) -> Result<Option<LlmProviderRecord>, sqlx::Error> {
        sqlx::query_as::<_, LlmProviderRecord>("SELECT * FROM llm_providers WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn llm_save_provider(
        &self,
        id: &str,
        name: &str,
        provider_type: &str,
        base_url: &str,
        api_key: &str,
        extra_config: Option<&str>,
        anthropic_base_url: Option<&str>,
        enabled: bool,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO llm_providers (id, name, provider_type, base_url, api_key, extra_config, anthropic_base_url, enabled, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, datetime('now'))
            ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                provider_type = excluded.provider_type,
                base_url = excluded.base_url,
                api_key = excluded.api_key,
                extra_config = excluded.extra_config,
                anthropic_base_url = excluded.anthropic_base_url,
                enabled = excluded.enabled,
                updated_at = datetime('now')
            "#,
        )
        .bind(id)
        .bind(name)
        .bind(provider_type)
        .bind(base_url)
        .bind(api_key)
        .bind(extra_config)
        .bind(anthropic_base_url)
        .bind(enabled as i32)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn llm_delete_provider(&self, id: &str) -> Result<(), sqlx::Error> {
        // Models are cascade-deleted via FK ON DELETE CASCADE
        sqlx::query("DELETE FROM llm_providers WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn llm_toggle_provider(&self, id: &str, enabled: bool) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE llm_providers SET enabled = ?, updated_at = datetime('now') WHERE id = ?",
        )
        .bind(enabled as i32)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ── Model CRUD ───────────────────────────────────────────────

    pub async fn llm_list_models(&self) -> Result<Vec<LlmModelRecord>, sqlx::Error> {
        sqlx::query_as::<_, LlmModelRecord>("SELECT * FROM llm_models ORDER BY created_at")
            .fetch_all(&self.pool)
            .await
    }

    pub async fn llm_list_models_for_provider(
        &self,
        provider_id: &str,
    ) -> Result<Vec<LlmModelRecord>, sqlx::Error> {
        sqlx::query_as::<_, LlmModelRecord>(
            "SELECT * FROM llm_models WHERE provider_id = ? ORDER BY created_at",
        )
        .bind(provider_id)
        .fetch_all(&self.pool)
        .await
    }

    pub async fn llm_find_model_by_name_or_alias(
        &self,
        name_or_alias: &str,
    ) -> Result<Option<LlmModelRecord>, sqlx::Error> {
        sqlx::query_as::<_, LlmModelRecord>(
            "SELECT * FROM llm_models WHERE enabled = 1 AND (model_name = ? OR alias = ?) \
             ORDER BY (model_name = ?) DESC LIMIT 1",
        )
        .bind(name_or_alias)
        .bind(name_or_alias)
        .bind(name_or_alias)
        .fetch_optional(&self.pool)
        .await
    }

    pub async fn llm_save_model(
        &self,
        id: &str,
        provider_id: &str,
        model_name: &str,
        alias: &str,
        tags: &str,
        enabled: bool,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO llm_models (id, provider_id, model_name, alias, tags, enabled, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, datetime('now'))
            ON CONFLICT(id) DO UPDATE SET
                model_name = excluded.model_name,
                alias = excluded.alias,
                tags = excluded.tags,
                enabled = excluded.enabled,
                updated_at = datetime('now')
            "#,
        )
        .bind(id)
        .bind(provider_id)
        .bind(model_name)
        .bind(alias)
        .bind(tags)
        .bind(enabled as i32)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn llm_update_model(
        &self,
        id: &str,
        model_name: &str,
        alias: &str,
        tags: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE llm_models SET model_name = ?, alias = ?, tags = ?, updated_at = datetime('now') WHERE id = ?"
        )
        .bind(model_name)
        .bind(alias)
        .bind(tags)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn llm_delete_model(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM llm_models WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ── API Key CRUD ─────────────────────────────────────────────

    pub async fn llm_list_api_keys(&self) -> Result<Vec<LlmApiKeyRecord>, sqlx::Error> {
        sqlx::query_as::<_, LlmApiKeyRecord>("SELECT * FROM llm_api_keys ORDER BY created_at")
            .fetch_all(&self.pool)
            .await
    }

    pub async fn llm_find_api_key_by_hash(
        &self,
        hash: &str,
    ) -> Result<Option<LlmApiKeyRecord>, sqlx::Error> {
        sqlx::query_as::<_, LlmApiKeyRecord>(
            "SELECT * FROM llm_api_keys WHERE key_hash = ? AND enabled = 1",
        )
        .bind(hash)
        .fetch_optional(&self.pool)
        .await
    }

    pub async fn llm_save_api_key(
        &self,
        id: &str,
        key_hash: &str,
        key_prefix: &str,
        name: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO llm_api_keys (id, key_hash, key_prefix, name)
            VALUES (?, ?, ?, ?)
            "#,
        )
        .bind(id)
        .bind(key_hash)
        .bind(key_prefix)
        .bind(name)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn llm_toggle_api_key(&self, id: &str, enabled: bool) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE llm_api_keys SET enabled = ? WHERE id = ?")
            .bind(enabled as i32)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn llm_delete_api_key(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM llm_api_keys WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn llm_touch_api_key(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE llm_api_keys SET last_used_at = datetime('now') WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ── Usage logs ────────────────────────────────────────────────

    /// 插入一条用量日志。timestamp 由 DB 用 datetime('now') 填充（UTC）。
    pub async fn llm_insert_usage_log(&self, u: &LlmUsageInsert) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO llm_usage_logs (
                id, timestamp, api_key_id, api_key_name, provider_id, provider_name,
                model_id, model_name, requested_model, protocol, stream, status_code,
                success, prompt_tokens, cache_hit_tokens, cache_miss_tokens,
                completion_tokens, total_tokens, latency_ms, error_type
            ) VALUES (?, datetime('now'), ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(&u.api_key_id)
        .bind(&u.api_key_name)
        .bind(&u.provider_id)
        .bind(&u.provider_name)
        .bind(&u.model_id)
        .bind(&u.model_name)
        .bind(&u.requested_model)
        .bind(&u.protocol)
        .bind(u.stream as i32)
        .bind(u.status_code)
        .bind(u.success as i32)
        .bind(u.prompt_tokens)
        .bind(u.cache_hit_tokens)
        .bind(u.cache_miss_tokens)
        .bind(u.completion_tokens)
        .bind(u.total_tokens)
        .bind(u.latency_ms)
        .bind(&u.error_type)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 时间范围内的用量总览。
    pub async fn llm_usage_summary(
        &self,
        start: &str,
        end: &str,
    ) -> Result<LlmUsageSummary, sqlx::Error> {
        sqlx::query_as::<_, LlmUsageSummary>(
            r#"
            SELECT
                COUNT(*) AS requests,
                COALESCE(SUM(success), 0) AS success,
                COALESCE(SUM(prompt_tokens), 0) AS prompt_tokens,
                COALESCE(SUM(cache_hit_tokens), 0) AS cache_hit_tokens,
                COALESCE(SUM(cache_miss_tokens), 0) AS cache_miss_tokens,
                COALESCE(SUM(completion_tokens), 0) AS completion_tokens,
                COALESCE(SUM(total_tokens), 0) AS total_tokens
            FROM llm_usage_logs
            WHERE timestamp >= ? AND timestamp <= ?
            "#,
        )
        .bind(start)
        .bind(end)
        .fetch_one(&self.pool)
        .await
    }

    /// 按维度聚合用量。`group_by ∈ {"api_key", "model", "provider"}`。
    pub async fn llm_aggregate_usage(
        &self,
        start: &str,
        end: &str,
        group_by: &str,
    ) -> Result<Vec<LlmUsageAggregateRow>, sqlx::Error> {
        // 列名不能参数化，白名单映射避免 SQL 注入。
        let (id_col, name_col) = match group_by {
            "model" => ("model_id", "model_name"),
            "provider" => ("provider_id", "provider_name"),
            _ => ("api_key_id", "api_key_name"),
        };
        let sql = format!(
            r#"
            SELECT
                {id_col} AS dimension_id,
                COALESCE(MAX({name_col}), '') AS dimension_name,
                COUNT(*) AS requests,
                COALESCE(SUM(success), 0) AS success,
                COALESCE(SUM(prompt_tokens), 0) AS prompt_tokens,
                COALESCE(SUM(cache_hit_tokens), 0) AS cache_hit_tokens,
                COALESCE(SUM(cache_miss_tokens), 0) AS cache_miss_tokens,
                COALESCE(SUM(completion_tokens), 0) AS completion_tokens,
                COALESCE(SUM(total_tokens), 0) AS total_tokens
            FROM llm_usage_logs
            WHERE timestamp >= ? AND timestamp <= ?
            GROUP BY {id_col}
            ORDER BY total_tokens DESC
            "#
        );
        sqlx::query_as::<_, LlmUsageAggregateRow>(&sql)
            .bind(start)
            .bind(end)
            .fetch_all(&self.pool)
            .await
    }

    /// 分页明细日志（按时间倒序）。
    pub async fn llm_query_usage_logs(
        &self,
        start: &str,
        end: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<LlmUsageLogRecord>, sqlx::Error> {
        sqlx::query_as::<_, LlmUsageLogRecord>(
            r#"
            SELECT * FROM llm_usage_logs
            WHERE timestamp >= ? AND timestamp <= ?
            ORDER BY timestamp DESC
            LIMIT ? OFFSET ?
            "#,
        )
        .bind(start)
        .bind(end)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await
    }

    /// 删除早于 `before`（ISO 8601 字符串）的用量日志，返回删除行数。
    pub async fn cleanup_old_llm_usage_logs(&self, before: &str) -> Result<u64, sqlx::Error> {
        let result = sqlx::query("DELETE FROM llm_usage_logs WHERE timestamp < ?")
            .bind(before)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use crate::server::db::Database;

    async fn fresh_db() -> (Database, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("test.db");
        let db = Database::new(path.to_str().unwrap()).await.unwrap();
        (db, tmp)
    }

    #[tokio::test]
    async fn test_provider_crud() {
        let (db, _tmp) = fresh_db().await;

        // Create
        let id = uuid::Uuid::new_v4().to_string();
        db.llm_save_provider(
            &id,
            "TestDeepSeek",
            "deepseek",
            "https://api.deepseek.com",
            "sk-test",
            None::<&str>,
            None::<&str>,
            true,
        )
        .await
        .unwrap();

        // List
        let providers = db.llm_list_providers().await.unwrap();
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].name, "TestDeepSeek");

        // Get
        let p = db.llm_get_provider(&id).await.unwrap().unwrap();
        assert_eq!(p.api_key, "sk-test");

        // Toggle
        db.llm_toggle_provider(&id, false).await.unwrap();
        let p = db.llm_get_provider(&id).await.unwrap().unwrap();
        assert_eq!(p.enabled, 0);

        // List enabled
        let enabled = db.llm_list_enabled_providers().await.unwrap();
        assert!(enabled.is_empty());

        // Delete
        db.llm_delete_provider(&id).await.unwrap();
        assert!(db.llm_get_provider(&id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_model_crud() {
        let (db, _tmp) = fresh_db().await;

        let pid = uuid::Uuid::new_v4().to_string();
        db.llm_save_provider(
            &pid,
            "Test",
            "deepseek",
            "https://api.deepseek.com",
            "sk-test",
            None::<&str>,
            None::<&str>,
            true,
        )
        .await
        .unwrap();

        // Add model
        let mid = uuid::Uuid::new_v4().to_string();
        db.llm_save_model(&mid, &pid, "deepseek-chat", "fast", "[\"coding\"]", true)
            .await
            .unwrap();

        // List models for provider
        let models = db.llm_list_models_for_provider(&pid).await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].model_name, "deepseek-chat");
        assert_eq!(models[0].alias, "fast");

        // Find by name
        let found = db
            .llm_find_model_by_name_or_alias("deepseek-chat")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.id, mid);

        // Find by alias
        let found = db
            .llm_find_model_by_name_or_alias("fast")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.id, mid);

        // Find non-existent
        assert!(db
            .llm_find_model_by_name_or_alias("nonexistent")
            .await
            .unwrap()
            .is_none());

        // Update model
        db.llm_update_model(&mid, "deepseek-chat", "fast-v2", "[\"coding\",\"cheap\"]")
            .await
            .unwrap();
        let models = db.llm_list_models_for_provider(&pid).await.unwrap();
        assert_eq!(models[0].alias, "fast-v2");

        // Delete model
        db.llm_delete_model(&mid).await.unwrap();
        assert!(db
            .llm_list_models_for_provider(&pid)
            .await
            .unwrap()
            .is_empty());

        // Cascade delete: delete provider should delete models
        let mid2 = uuid::Uuid::new_v4().to_string();
        db.llm_save_model(&mid2, &pid, "deepseek-r1", "", "[]", true)
            .await
            .unwrap();
        db.llm_delete_provider(&pid).await.unwrap();
        assert!(db.llm_list_models().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_find_model_prefers_exact_name_over_alias() {
        let (db, _tmp) = fresh_db().await;

        let p1 = uuid::Uuid::new_v4().to_string();
        let p2 = uuid::Uuid::new_v4().to_string();
        db.llm_save_provider(&p1, "P1", "deepseek", "https://a", "k", None::<&str>, None::<&str>, true)
            .await
            .unwrap();
        db.llm_save_provider(&p2, "P2", "kimi", "https://b", "k", None::<&str>, None::<&str>, true)
            .await
            .unwrap();

        // 先插入“别名冲突”的模型，制造无序查询返回它的机会
        let alias_model = uuid::Uuid::new_v4().to_string();
        db.llm_save_model(&alias_model, &p2, "moonshot-v1-8k", "fast", "[]", true)
            .await
            .unwrap();
        // 后插入“名称精确匹配”的模型
        let name_model = uuid::Uuid::new_v4().to_string();
        db.llm_save_model(&name_model, &p1, "fast", "", "[]", true)
            .await
            .unwrap();

        // spec: 优先精确匹配 model_name，其次匹配 alias
        let found = db
            .llm_find_model_by_name_or_alias("fast")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            found.id, name_model,
            "model_name 精确匹配应优先于 alias 匹配"
        );
    }

    #[tokio::test]
    async fn test_api_key_crud() {
        let (db, _tmp) = fresh_db().await;

        let id = uuid::Uuid::new_v4().to_string();
        db.llm_save_api_key(&id, "hash123", "sk-abc...xyz", "Cursor")
            .await
            .unwrap();

        let keys = db.llm_list_api_keys().await.unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].name, "Cursor");
        assert_eq!(keys[0].key_prefix, "sk-abc...xyz");

        // Find by hash
        let found = db
            .llm_find_api_key_by_hash("hash123")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.id, id);

        // Wrong hash
        assert!(db
            .llm_find_api_key_by_hash("wrong")
            .await
            .unwrap()
            .is_none());

        // Toggle
        db.llm_toggle_api_key(&id, false).await.unwrap();
        assert!(db
            .llm_find_api_key_by_hash("hash123")
            .await
            .unwrap()
            .is_none());

        // Touch
        db.llm_toggle_api_key(&id, true).await.unwrap();
        db.llm_touch_api_key(&id).await.unwrap();
        let keys = db.llm_list_api_keys().await.unwrap();
        assert!(keys[0].last_used_at.is_some());

        // Delete
        db.llm_delete_api_key(&id).await.unwrap();
        assert!(db.llm_list_api_keys().await.unwrap().is_empty());
    }

    // ── Usage logs ────────────────────────────────────────────────

    use super::LlmUsageInsert;

    fn sample_usage(model: &str, prompt: i64, cache_hit: i64, completion: i64) -> LlmUsageInsert {
        LlmUsageInsert {
            api_key_id: Some("k1".into()),
            api_key_name: "Cursor".into(),
            provider_id: Some("p1".into()),
            provider_name: "DeepSeek".into(),
            model_id: Some("m1".into()),
            model_name: model.into(),
            requested_model: "fast".into(),
            protocol: "openai".into(),
            stream: false,
            status_code: 200,
            success: true,
            prompt_tokens: prompt,
            cache_hit_tokens: cache_hit,
            cache_miss_tokens: prompt - cache_hit,
            completion_tokens: completion,
            total_tokens: prompt + completion,
            latency_ms: 123,
            error_type: None,
        }
    }

    #[tokio::test]
    async fn test_usage_insert_summary_and_logs() {
        let (db, _tmp) = fresh_db().await;

        db.llm_insert_usage_log(&sample_usage("deepseek-chat", 100, 30, 50))
            .await
            .unwrap();
        db.llm_insert_usage_log(&sample_usage("deepseek-chat", 200, 0, 80))
            .await
            .unwrap();

        let full = ("1970-01-01T00:00:00Z", "2999-01-01T00:00:00Z");
        let summary = db.llm_usage_summary(full.0, full.1).await.unwrap();
        assert_eq!(summary.requests, 2);
        assert_eq!(summary.success, 2);
        assert_eq!(summary.prompt_tokens, 300);
        assert_eq!(summary.cache_hit_tokens, 30);
        assert_eq!(summary.cache_miss_tokens, 270);
        assert_eq!(summary.completion_tokens, 130);
        assert_eq!(summary.total_tokens, 430);

        let logs = db.llm_query_usage_logs(full.0, full.1, 10, 0).await.unwrap();
        assert_eq!(logs.len(), 2);

        // 分页
        let page = db.llm_query_usage_logs(full.0, full.1, 1, 0).await.unwrap();
        assert_eq!(page.len(), 1);
    }

    #[tokio::test]
    async fn test_usage_aggregate_by_dimensions() {
        let (db, _tmp) = fresh_db().await;
        db.llm_insert_usage_log(&sample_usage("deepseek-chat", 100, 20, 50))
            .await
            .unwrap();
        db.llm_insert_usage_log(&sample_usage("deepseek-chat", 100, 20, 50))
            .await
            .unwrap();

        let full = ("1970-01-01T00:00:00Z", "2999-01-01T00:00:00Z");

        for dim in ["api_key", "model", "provider"] {
            let rows = db.llm_aggregate_usage(full.0, full.1, dim).await.unwrap();
            assert_eq!(rows.len(), 1, "dim {dim} should collapse to one group");
            assert_eq!(rows[0].requests, 2);
            assert_eq!(rows[0].total_tokens, 300);
            assert_eq!(rows[0].cache_hit_tokens, 40);
            assert!(!rows[0].dimension_name.is_empty());
        }
    }

    #[tokio::test]
    async fn test_usage_cleanup_old_logs() {
        let (db, _tmp) = fresh_db().await;
        // 手工插入一条 timestamp 很旧的记录
        sqlx::query(
            "INSERT INTO llm_usage_logs (id, timestamp, protocol) VALUES ('old', '2000-01-01T00:00:00Z', 'openai')",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        db.llm_insert_usage_log(&sample_usage("m", 10, 0, 5))
            .await
            .unwrap();

        let deleted = db
            .cleanup_old_llm_usage_logs("2020-01-01T00:00:00Z")
            .await
            .unwrap();
        assert_eq!(deleted, 1);

        let remaining = db
            .llm_query_usage_logs("1970-01-01T00:00:00Z", "2999-01-01T00:00:00Z", 10, 0)
            .await
            .unwrap();
        assert_eq!(remaining.len(), 1);
    }
}
