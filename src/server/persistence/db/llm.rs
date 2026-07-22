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

// ── Provider CRUD ─────────────────────────────────────────────

impl Database {
    pub async fn llm_list_providers(&self) -> Result<Vec<LlmProviderRecord>, sqlx::Error> {
        sqlx::query_as::<_, LlmProviderRecord>(
            "SELECT * FROM llm_providers ORDER BY created_at",
        )
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

    pub async fn llm_get_provider(&self, id: &str) -> Result<Option<LlmProviderRecord>, sqlx::Error> {
        sqlx::query_as::<_, LlmProviderRecord>(
            "SELECT * FROM llm_providers WHERE id = ?",
        )
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
        enabled: bool,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO llm_providers (id, name, provider_type, base_url, api_key, extra_config, enabled, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, datetime('now'))
            ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                provider_type = excluded.provider_type,
                base_url = excluded.base_url,
                api_key = excluded.api_key,
                extra_config = excluded.extra_config,
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
        sqlx::query("UPDATE llm_providers SET enabled = ?, updated_at = datetime('now') WHERE id = ?")
            .bind(enabled as i32)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ── Model CRUD ───────────────────────────────────────────────

    pub async fn llm_list_models(&self) -> Result<Vec<LlmModelRecord>, sqlx::Error> {
        sqlx::query_as::<_, LlmModelRecord>(
            "SELECT * FROM llm_models ORDER BY created_at",
        )
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
            "SELECT * FROM llm_models WHERE enabled = 1 AND (model_name = ? OR alias = ?) LIMIT 1",
        )
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
        sqlx::query_as::<_, LlmApiKeyRecord>(
            "SELECT * FROM llm_api_keys ORDER BY created_at",
        )
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
        db.llm_save_provider(&id, "TestDeepSeek", "deepseek", "https://api.deepseek.com", "sk-test", None::<&str>, true).await.unwrap();

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
        db.llm_save_provider(&pid, "Test", "deepseek", "https://api.deepseek.com", "sk-test", None::<&str>, true).await.unwrap();

        // Add model
        let mid = uuid::Uuid::new_v4().to_string();
        db.llm_save_model(&mid, &pid, "deepseek-chat", "fast", "[\"coding\"]", true).await.unwrap();

        // List models for provider
        let models = db.llm_list_models_for_provider(&pid).await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].model_name, "deepseek-chat");
        assert_eq!(models[0].alias, "fast");

        // Find by name
        let found = db.llm_find_model_by_name_or_alias("deepseek-chat").await.unwrap().unwrap();
        assert_eq!(found.id, mid);

        // Find by alias
        let found = db.llm_find_model_by_name_or_alias("fast").await.unwrap().unwrap();
        assert_eq!(found.id, mid);

        // Find non-existent
        assert!(db.llm_find_model_by_name_or_alias("nonexistent").await.unwrap().is_none());

        // Update model
        db.llm_update_model(&mid, "deepseek-chat", "fast-v2", "[\"coding\",\"cheap\"]").await.unwrap();
        let models = db.llm_list_models_for_provider(&pid).await.unwrap();
        assert_eq!(models[0].alias, "fast-v2");

        // Delete model
        db.llm_delete_model(&mid).await.unwrap();
        assert!(db.llm_list_models_for_provider(&pid).await.unwrap().is_empty());

        // Cascade delete: delete provider should delete models
        let mid2 = uuid::Uuid::new_v4().to_string();
        db.llm_save_model(&mid2, &pid, "deepseek-r1", "", "[]", true).await.unwrap();
        db.llm_delete_provider(&pid).await.unwrap();
        assert!(db.llm_list_models().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_api_key_crud() {
        let (db, _tmp) = fresh_db().await;

        let id = uuid::Uuid::new_v4().to_string();
        db.llm_save_api_key(&id, "hash123", "sk-abc...xyz", "Cursor").await.unwrap();

        let keys = db.llm_list_api_keys().await.unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].name, "Cursor");
        assert_eq!(keys[0].key_prefix, "sk-abc...xyz");

        // Find by hash
        let found = db.llm_find_api_key_by_hash("hash123").await.unwrap().unwrap();
        assert_eq!(found.id, id);

        // Wrong hash
        assert!(db.llm_find_api_key_by_hash("wrong").await.unwrap().is_none());

        // Toggle
        db.llm_toggle_api_key(&id, false).await.unwrap();
        assert!(db.llm_find_api_key_by_hash("hash123").await.unwrap().is_none());

        // Touch
        db.llm_toggle_api_key(&id, true).await.unwrap();
        db.llm_touch_api_key(&id).await.unwrap();
        let keys = db.llm_list_api_keys().await.unwrap();
        assert!(keys[0].last_used_at.is_some());

        // Delete
        db.llm_delete_api_key(&id).await.unwrap();
        assert!(db.llm_list_api_keys().await.unwrap().is_empty());
    }
}
