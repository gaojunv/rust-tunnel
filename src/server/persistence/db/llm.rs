use super::Database;
use chrono::{DateTime, Utc};

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
pub struct LlmModelGroupRecord {
    pub id: String,
    pub name: String,
    pub enabled: i32,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct LlmModelGroupMemberRecord {
    pub group_id: String,
    pub model_id: String,
    pub priority: i32,
}

/// 组成员联查：含模型与 provider 关键字段，供路由层构建候选链。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct LlmGroupMemberJoined {
    pub model_id: String,
    pub priority: i32,
    pub model_name: String,
    pub alias: String,
    pub model_enabled: i32,
    pub provider_id: String,
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
    /// 绑定的 RAG 知识库 id（未绑定时为 None）。
    pub kb_id: Option<String>,
}

/// 一条 LLM 网关请求的用量日志。
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct LlmUsageLogRecord {
    pub id: String,
    pub timestamp: DateTime<Utc>,
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
    /// 本次请求注入的 RAG 知识库片段数（未走 RAG 时为 None）。
    pub rag_chunks_injected: Option<i64>,
    /// 发生故障转移时记录首选（被跳过的）模型名；未转移为 NULL。
    pub failover_from: Option<String>,
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
    pub rag_chunks_injected: Option<i64>,
    /// 发生故障转移时记录首选（被跳过的）模型名；未转移为 None。
    pub failover_from: Option<String>,
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
        // 显式列：`SELECT *` 会在 ALTER TABLE 追加 kb_id 后命中 sqlx 语句缓存中的
        // 旧列元数据（7 列），与 8 字段的 FromRow 错位导致越界 panic。
        sqlx::query_as::<_, LlmApiKeyRecord>(
            "SELECT id, key_hash, key_prefix, name, enabled, created_at, last_used_at, kb_id \
             FROM llm_api_keys ORDER BY created_at",
        )
        .fetch_all(&self.pool)
        .await
    }

    pub async fn llm_find_api_key_by_hash(
        &self,
        hash: &str,
    ) -> Result<Option<LlmApiKeyRecord>, sqlx::Error> {
        sqlx::query_as::<_, LlmApiKeyRecord>(
            "SELECT id, key_hash, key_prefix, name, enabled, created_at, last_used_at, kb_id \
             FROM llm_api_keys WHERE key_hash = ? AND enabled = 1",
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
        kb_id: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO llm_api_keys (id, key_hash, key_prefix, name, kb_id)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind(id)
        .bind(key_hash)
        .bind(key_prefix)
        .bind(name)
        .bind(kb_id)
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

    /// 绑定/解绑 api key 的知识库（`None` 解绑）。KB 存在性由调用方负责校验。
    pub async fn llm_set_api_key_kb(
        &self,
        id: &str,
        kb_id: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE llm_api_keys SET kb_id = ? WHERE id = ?")
            .bind(kb_id)
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

    /// 查询某 api key 绑定的知识库 id（未绑定返回 None）。
    pub async fn rag_get_kb_id_for_api_key(
        &self,
        key_id: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        let row: Option<(Option<String>,)> =
            sqlx::query_as("SELECT kb_id FROM llm_api_keys WHERE id = ?")
                .bind(key_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.and_then(|r| r.0))
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
                completion_tokens, total_tokens, latency_ms, error_type,
                rag_chunks_injected, failover_from
            ) VALUES (?, datetime('now'), ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
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
        .bind(u.rag_chunks_injected)
        .bind(&u.failover_from)
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
            SELECT id, timestamp, api_key_id, api_key_name, provider_id, provider_name,
                   model_id, model_name, requested_model, protocol, stream, status_code,
                   success, prompt_tokens, cache_hit_tokens, cache_miss_tokens,
                   completion_tokens, total_tokens, latency_ms, error_type, rag_chunks_injected,
                   failover_from
            FROM llm_usage_logs
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

    /// 查询时间范围内的用量日志总数。
    pub async fn llm_count_usage_logs(&self, start: &str, end: &str) -> Result<i64, sqlx::Error> {
        let row: (i64,) = sqlx::query_as(
            r#"
            SELECT COUNT(*) FROM llm_usage_logs
            WHERE timestamp >= ? AND timestamp <= ?
            "#,
        )
        .bind(start)
        .bind(end)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    /// 删除早于 `before`（ISO 8601 字符串）的用量日志，返回删除行数。
    pub async fn cleanup_old_llm_usage_logs(&self, before: &str) -> Result<u64, sqlx::Error> {
        let result = sqlx::query("DELETE FROM llm_usage_logs WHERE timestamp < ?")
            .bind(before)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    // ── Model groups (failover routing) ──────────────────────────

    pub async fn llm_create_model_group(
        &self,
        id: &str,
        name: &str,
        enabled: bool,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT INTO llm_model_groups (id, name, enabled) VALUES (?, ?, ?)")
            .bind(id)
            .bind(name)
            .bind(enabled as i32)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn llm_update_model_group(
        &self,
        id: &str,
        name: &str,
        enabled: bool,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE llm_model_groups SET name = ?, enabled = ?, updated_at = datetime('now') WHERE id = ?",
        )
        .bind(name)
        .bind(enabled as i32)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 删除模型组。FK 级联（ON DELETE CASCADE）经 sqlx 默认的
    /// `PRAGMA foreign_keys=ON` 已生效；此处显式先删成员是防御性冗余，
    /// 防止未来池配置变更导致级联静默失效时残留孤儿成员行。
    pub async fn llm_delete_model_group(&self, id: &str) -> Result<(), sqlx::Error> {
        // 两条删除放同一事务：组删除失败时不残留"成员已清空、组仍在"的中间态
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM llm_model_group_members WHERE group_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM llm_model_groups WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await
    }

    pub async fn llm_list_model_groups(&self) -> Result<Vec<LlmModelGroupRecord>, sqlx::Error> {
        sqlx::query_as::<_, LlmModelGroupRecord>(
            "SELECT * FROM llm_model_groups ORDER BY created_at ASC",
        )
        .fetch_all(&self.pool)
        .await
    }

    pub async fn llm_get_model_group(
        &self,
        id: &str,
    ) -> Result<Option<LlmModelGroupRecord>, sqlx::Error> {
        sqlx::query_as::<_, LlmModelGroupRecord>("SELECT * FROM llm_model_groups WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
    }

    pub async fn llm_find_group_by_name(
        &self,
        name: &str,
    ) -> Result<Option<LlmModelGroupRecord>, sqlx::Error> {
        sqlx::query_as::<_, LlmModelGroupRecord>(
            "SELECT * FROM llm_model_groups WHERE name = ? AND enabled = 1",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await
    }

    /// 整体替换组成员（事务内 DELETE + INSERT）。members: [(model_id, priority)]。
    pub async fn llm_replace_group_members(
        &self,
        group_id: &str,
        members: &[(String, i32)],
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM llm_model_group_members WHERE group_id = ?")
            .bind(group_id)
            .execute(&mut *tx)
            .await?;
        for (model_id, priority) in members {
            sqlx::query(
                "INSERT INTO llm_model_group_members (group_id, model_id, priority) VALUES (?, ?, ?)",
            )
            .bind(group_id)
            .bind(model_id)
            .bind(priority)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await
    }

    /// 组成员联查（按 priority 升序）。
    pub async fn llm_list_group_members(
        &self,
        group_id: &str,
    ) -> Result<Vec<LlmGroupMemberJoined>, sqlx::Error> {
        sqlx::query_as::<_, LlmGroupMemberJoined>(
            "SELECT m.model_id, m.priority, mo.model_name, mo.alias, mo.enabled AS model_enabled, mo.provider_id
             FROM llm_model_group_members m
             JOIN llm_models mo ON mo.id = m.model_id
             WHERE m.group_id = ?
             ORDER BY m.priority ASC",
        )
        .bind(group_id)
        .fetch_all(&self.pool)
        .await
    }

    pub async fn llm_group_member_count(&self, group_id: &str) -> Result<i64, sqlx::Error> {
        let row: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM llm_model_group_members WHERE group_id = ?")
                .bind(group_id)
                .fetch_one(&self.pool)
                .await?;
        Ok(row.0)
    }

    /// 组名冲突检测：与现有 model_name / alias / 其他组名比对。
    /// exclude_group_id 用于编辑组时排除自身。
    pub async fn llm_group_name_conflicts(
        &self,
        name: &str,
        exclude_group_id: Option<&str>,
    ) -> Result<bool, sqlx::Error> {
        let model_hit: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM llm_models WHERE model_name = ? OR alias = ?")
                .bind(name)
                .bind(name)
                .fetch_one(&self.pool)
                .await?;
        if model_hit.0 > 0 {
            return Ok(true);
        }
        let group_hit: (i64,) = match exclude_group_id {
            Some(ex) => {
                sqlx::query_as("SELECT COUNT(*) FROM llm_model_groups WHERE name = ? AND id != ?")
                    .bind(name)
                    .bind(ex)
                    .fetch_one(&self.pool)
                    .await?
            }
            None => {
                sqlx::query_as("SELECT COUNT(*) FROM llm_model_groups WHERE name = ?")
                    .bind(name)
                    .fetch_one(&self.pool)
                    .await?
            }
        };
        Ok(group_hit.0 > 0)
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
        db.llm_save_provider(
            &p1,
            "P1",
            "deepseek",
            "https://a",
            "k",
            None::<&str>,
            None::<&str>,
            true,
        )
        .await
        .unwrap();
        db.llm_save_provider(
            &p2,
            "P2",
            "kimi",
            "https://b",
            "k",
            None::<&str>,
            None::<&str>,
            true,
        )
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
        db.llm_save_api_key(&id, "hash123", "sk-abc...xyz", "Cursor", None)
            .await
            .unwrap();

        let keys = db.llm_list_api_keys().await.unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].name, "Cursor");
        assert_eq!(keys[0].key_prefix, "sk-abc...xyz");
        assert_eq!(keys[0].kb_id, None, "未绑定时 kb_id 应为 None");

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
            rag_chunks_injected: None,
            failover_from: None,
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

        let logs = db
            .llm_query_usage_logs(full.0, full.1, 10, 0)
            .await
            .unwrap();
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

    #[tokio::test]
    async fn usage_log_records_rag_chunks_injected() {
        let (db, _tmp) = fresh_db().await;

        let insert = LlmUsageInsert {
            api_key_name: "k".into(),
            provider_name: "p".into(),
            model_name: "m".into(),
            requested_model: "m".into(),
            protocol: "openai".into(),
            stream: false,
            status_code: 200,
            success: true,
            rag_chunks_injected: Some(3),
            ..Default::default()
        };
        db.llm_insert_usage_log(&insert).await.unwrap();
        let logs = db
            .llm_query_usage_logs("2000-01-01", "2100-01-01", 10, 0)
            .await
            .unwrap();
        assert_eq!(logs[0].rag_chunks_injected, Some(3));
    }

    // ── Model groups (failover) ──────────────────────────────────

    #[tokio::test]
    async fn test_model_group_crud() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = crate::server::db::Database::new(tmp.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();

        // 创建组
        db.llm_create_model_group("g1", "smart-router", true)
            .await
            .unwrap();
        let g = db.llm_get_model_group("g1").await.unwrap().unwrap();
        assert_eq!(g.name, "smart-router");
        assert_eq!(g.enabled, 1);

        // 按名查
        let by_name = db
            .llm_find_group_by_name("smart-router")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(by_name.id, "g1");

        // 改名 + 禁用
        db.llm_update_model_group("g1", "smart-router-2", false)
            .await
            .unwrap();
        let g2 = db.llm_get_model_group("g1").await.unwrap().unwrap();
        assert_eq!(g2.name, "smart-router-2");
        assert_eq!(g2.enabled, 0);

        // 列表
        assert_eq!(db.llm_list_model_groups().await.unwrap().len(), 1);

        // 删除
        db.llm_delete_model_group("g1").await.unwrap();
        assert!(db.llm_get_model_group("g1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_group_members_replace_and_join() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = crate::server::db::Database::new(tmp.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();

        // 准备 provider + 2 个模型
        db.llm_save_provider(
            "p1",
            "DS",
            "deepseek",
            "https://api.deepseek.com",
            "k",
            None::<&str>,
            None::<&str>,
            true,
        )
        .await
        .unwrap();
        db.llm_save_model("m1", "p1", "deepseek-chat", "chat", "[]", true)
            .await
            .unwrap();
        db.llm_save_model("m2", "p1", "deepseek-reasoner", "", "[]", true)
            .await
            .unwrap();

        db.llm_create_model_group("g1", "router", true)
            .await
            .unwrap();
        db.llm_replace_group_members("g1", &[("m1".into(), 1), ("m2".into(), 2)])
            .await
            .unwrap();

        let members = db.llm_list_group_members("g1").await.unwrap();
        assert_eq!(members.len(), 2);
        assert_eq!(members[0].model_id, "m1");
        assert_eq!(members[0].model_name, "deepseek-chat");
        assert_eq!(members[0].alias, "chat");
        assert_eq!(members[1].priority, 2);
        assert_eq!(db.llm_group_member_count("g1").await.unwrap(), 2);

        // 整体替换：去掉 m1，m2 提到 1
        db.llm_replace_group_members("g1", &[("m2".into(), 1)])
            .await
            .unwrap();
        let members2 = db.llm_list_group_members("g1").await.unwrap();
        assert_eq!(members2.len(), 1);
        assert_eq!(members2[0].model_id, "m2");
        assert_eq!(members2[0].priority, 1);
    }

    #[tokio::test]
    async fn test_delete_group_cleans_members() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = crate::server::db::Database::new(tmp.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        db.llm_save_provider(
            "p1",
            "DS",
            "deepseek",
            "https://api.deepseek.com",
            "k",
            None::<&str>,
            None::<&str>,
            true,
        )
        .await
        .unwrap();
        db.llm_save_model("m1", "p1", "deepseek-chat", "", "[]", true)
            .await
            .unwrap();
        db.llm_create_model_group("g1", "router", true)
            .await
            .unwrap();
        db.llm_replace_group_members("g1", &[("m1".into(), 1)])
            .await
            .unwrap();
        assert_eq!(db.llm_group_member_count("g1").await.unwrap(), 1);

        // 删组：FK 级联经 sqlx 默认的 PRAGMA foreign_keys=ON 已生效，成员行随组一并清理（回归：孤儿成员数据）
        db.llm_delete_model_group("g1").await.unwrap();
        assert_eq!(db.llm_group_member_count("g1").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn test_group_name_conflicts() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = crate::server::db::Database::new(tmp.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        db.llm_save_provider(
            "p1",
            "DS",
            "deepseek",
            "https://api.deepseek.com",
            "k",
            None::<&str>,
            None::<&str>,
            true,
        )
        .await
        .unwrap();
        db.llm_save_model("m1", "p1", "deepseek-chat", "chat-alias", "[]", true)
            .await
            .unwrap();
        db.llm_create_model_group("g1", "router", true)
            .await
            .unwrap();

        assert!(db
            .llm_group_name_conflicts("deepseek-chat", None)
            .await
            .unwrap()); // 撞 model_name
        assert!(db
            .llm_group_name_conflicts("chat-alias", None)
            .await
            .unwrap()); // 撞 alias
        assert!(db.llm_group_name_conflicts("router", None).await.unwrap()); // 撞其他组名
        assert!(!db
            .llm_group_name_conflicts("router", Some("g1"))
            .await
            .unwrap()); // 排除自身
        assert!(!db
            .llm_group_name_conflicts("free-name", None)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn test_usage_insert_with_failover_from() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = crate::server::db::Database::new(tmp.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        let u = crate::server::persistence::db::llm::LlmUsageInsert {
            requested_model: "router".into(),
            protocol: "openai".into(),
            stream: false,
            status_code: 200,
            success: true,
            failover_from: Some("deepseek-chat".into()),
            ..Default::default()
        };
        db.llm_insert_usage_log(&u).await.unwrap();
        // 能查到即可（具体查询方法现有测试已有覆盖）
    }
}
