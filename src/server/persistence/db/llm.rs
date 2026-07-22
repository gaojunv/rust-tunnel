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
