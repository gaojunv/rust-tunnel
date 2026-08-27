//! AI 记忆体数据访问层：`agent_memory_settings`（单行全局设置）+ `agent_memories`
//! （记忆主表）+ `agent_sessions.distilled` 蒸馏防重 CAS。
//!
//! 向量本体在 qdrant-edge shard（kb_id 常量 `"memory"`，见 `agent/memory/mod.rs` 的
//! `MEMORY_KB_ID`），此处只存元数据与原文。**向量点约定**：`ChunkPoint` 的 `id` 与
//! `doc_id` 均取记忆 id（删除走 `delete_by_doc("memory", dim, memory_id)`）。本层不
//! 触碰向量，该约定由后续 distill/remember 模块落实。

use super::agent::ser_de_normalized_dt;
use super::Database;

/// AI 记忆体全局设置（单行 id=1）。`emb_api_key` 为 LlmCipher 加密存储的原始
/// 字符串；`emb_dimension` 首次 test-embedding 探测后固定，改动需清空重建。
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct AgentMemorySettingsRecord {
    /// 主键 id（固定为 1，单行表）。
    pub id: i64,
    /// 总开关（1 启用记忆体，0 关闭）。
    pub enabled: i32,
    /// Embedding 服务 Base URL。
    pub emb_base_url: String,
    /// Embedding API Key（LlmCipher 加密存储的原始字符串）。
    pub emb_api_key: String,
    /// Embedding 模型名。
    pub emb_model: String,
    /// Embedding 向量维度（0 表示未探测，首次 test-embedding 后固定）。
    pub emb_dimension: i64,
    /// 蒸馏所用 LLM 模型（空串表示未配置）。
    pub distill_model: String,
    /// 检索 top_k（默认 8）。
    pub top_k: i64,
    /// 检索分数阈值（默认 0.40）。
    pub score_threshold: f64,
    /// 注入 token 预算上限（默认 1500）。
    pub inject_budget_tokens: i64,
    /// 是否总是注入置顶记忆（1 是，0 否，默认 1）。
    pub pin_always_inject: i32,
    /// Skill 库总闸（opt-in 默认关：避免零配置用户产生非预期蒸馏 LLM 开销）。
    pub skill_enabled: i32,
    /// 会话开始注入的技能清单条数上限（默认 20）。
    pub skill_list_max: i64,
    /// Wiki 总闸（默认 1：显式上传才有 LLM 开销，无非预期成本）。
    pub wiki_enabled: i32,
    /// 会话开始注入的 Wiki 清单条数上限（默认 20）。
    pub wiki_list_max: i64,
    /// 创建时间（DB datetime）。
    #[serde(serialize_with = "ser_de_normalized_dt")]
    pub created_at: String,
    /// 更新时间（DB datetime）。
    #[serde(serialize_with = "ser_de_normalized_dt")]
    pub updated_at: String,
}

impl AgentMemorySettingsRecord {
    /// 未配置时的默认构造（enabled=false，其余为表默认值）。`memory_get_settings`
    /// 无行时返回它；时间字段置空（表中无对应行，正常序列化为空串）。
    #[must_use]
    pub fn default_disabled() -> Self {
        Self {
            id: 1,
            enabled: 0,
            emb_base_url: String::new(),
            emb_api_key: String::new(),
            emb_model: String::new(),
            emb_dimension: 0,
            distill_model: String::new(),
            top_k: 8,
            score_threshold: 0.40,
            inject_budget_tokens: 1500,
            pin_always_inject: 1,
            skill_enabled: 0,
            skill_list_max: 20,
            wiki_enabled: 1,
            wiki_list_max: 20,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }
}

/// 一条 AI 记忆（原子事实）。`tags` 为 JSON 数组字符串（`'["rust","clean"]'`），
/// `scope_type` ∈ global|client|workspace。
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct AgentMemoryRecord {
    /// 主键 id（UUID）。
    pub id: String,
    /// 记忆正文（原子事实）。
    pub content: String,
    /// 作用域类型（global|client|workspace）。
    pub scope_type: String,
    /// 所属客户端 id（global 时为空串）。
    pub client_id: String,
    /// 所属工作区 id（非 workspace 作用域时为空串）。
    pub workspace_id: String,
    /// 标签 JSON 数组字符串（`'["rust","clean"]'`）。
    pub tags: String,
    /// 置信度（0.0–1.0）。
    pub confidence: f64,
    /// 来源会话 id（手动创建时为空）。
    pub source_session_id: String,
    /// 来源触发器（distill|remember|manual 等）。
    pub source_trigger: String,
    /// 是否置顶（1 置顶，0 未置顶）。
    pub pinned: i32,
    /// 检索命中次数。
    pub hit_count: i64,
    /// 最后命中时间（未命中过为 None）。
    pub last_hit_at: Option<String>,
    /// 创建时间（DB datetime）。
    #[serde(serialize_with = "ser_de_normalized_dt")]
    pub created_at: String,
    /// 更新时间（DB datetime）。
    #[serde(serialize_with = "ser_de_normalized_dt")]
    pub updated_at: String,
}

/// `memory_insert` 参数包：记忆写入的全部字段（10 项）。
#[derive(Debug, Clone, Default)]
pub struct MemoryInsertOpts {
    /// 记忆主键 id。
    pub id: String,
    /// 记忆正文（原子事实）。
    pub content: String,
    /// 作用域类型（global|client|workspace）。
    pub scope_type: String,
    /// 所属客户端 id。
    pub client_id: String,
    /// 所属工作区 id。
    pub workspace_id: String,
    /// 标签 JSON 数组字符串。
    pub tags: String,
    /// 置信度（0.0–1.0）。
    pub confidence: f64,
    /// 来源会话 id。
    pub source_session_id: String,
    /// 来源触发器（distill|remember|manual）。
    pub source_trigger: String,
    /// 是否置顶（仅插入时生效，后续用 toggle 翻转）。
    pub pinned: bool,
}

/// `memory_list` 查询条件：记忆列表的全部过滤参数（8 项）。
#[derive(Debug, Clone, Default)]
pub struct MemoryListFilter {
    /// 作用域类型过滤（global|client|workspace，None 不过滤）。
    pub scope_type: Option<String>,
    /// 客户端 id 过滤（空串等同 None）。
    pub client_id: Option<String>,
    /// 工作区 id 过滤（空串等同 None）。
    pub workspace_id: Option<String>,
    /// 模糊搜索关键字（匹配 content，None 不过滤）。
    pub q: Option<String>,
    /// 置顶过滤（Some(true) 仅置顶，Some(false) 仅未置顶，None 不过滤）。
    pub pinned: Option<bool>,
    /// 排序方式（recent|created|confidence|hits，None 取 recent）。
    pub order: Option<String>,
    /// 分页条数。
    pub limit: i64,
    /// 分页偏移。
    pub offset: i64,
}

impl Database {
    // ── Settings（单行 id=1）─────────────────────────────────────

    /// 读取全局设置；无行（从未写入）时返回默认构造（enabled=false）。
    ///
    /// # Errors
    ///
    /// 数据库连接不可用或 SQL 执行失败时返回 `sqlx::Error`。
    pub async fn memory_get_settings(&self) -> Result<AgentMemorySettingsRecord, sqlx::Error> {
        match sqlx::query_as::<_, AgentMemorySettingsRecord>(
            "SELECT * FROM agent_memory_settings WHERE id = 1",
        )
        .fetch_optional(&self.pool)
        .await?
        {
            Some(s) => Ok(s),
            None => Ok(AgentMemorySettingsRecord::default_disabled()),
        }
    }

    /// upsert 全局设置（单行 id=1）。`created_at` 由 DB 保持（INSERT OR REPLACE
    /// 下用 COALESCE 子查询保留首建时间，仅首次插入取 `datetime('now')`）；
    /// 记录里传入的 `created_at`/`updated_at` 字段被忽略，调用方可随便填。
    /// # Errors
    ///
    /// 数据库连接不可用或 SQL 执行失败时返回 `sqlx::Error`。
    pub async fn memory_upsert_settings(
        &self,
        s: &AgentMemorySettingsRecord,
    ) -> Result<(), sqlx::Error> {
        // 注意：INSERT OR REPLACE 是显式列绑定——加列必须同步 INSERT 列清单与
        // bind 序列（INSERT 列清单与下方 bind 一一对应），否则每次 upsert 会把
        // 新列重置为 DEFAULT。
        sqlx::query(
            r"
            INSERT OR REPLACE INTO agent_memory_settings (
                id, enabled, emb_base_url, emb_api_key, emb_model, emb_dimension,
                distill_model, top_k, score_threshold, inject_budget_tokens,
                pin_always_inject, skill_enabled, skill_list_max,
                wiki_enabled, wiki_list_max, created_at, updated_at
            ) VALUES (
                1, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?,
                COALESCE((SELECT created_at FROM agent_memory_settings WHERE id = 1), datetime('now')),
                datetime('now')
            )
            ",
        )
        .bind(s.enabled)
        .bind(&s.emb_base_url)
        .bind(&s.emb_api_key)
        .bind(&s.emb_model)
        .bind(s.emb_dimension)
        .bind(&s.distill_model)
        .bind(s.top_k)
        .bind(s.score_threshold)
        .bind(s.inject_budget_tokens)
        .bind(s.pin_always_inject)
        .bind(s.skill_enabled)
        .bind(s.skill_list_max)
        .bind(s.wiki_enabled)
        .bind(s.wiki_list_max)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ── Memories CRUD ───────────────────────────────────────────

    /// 插入一条记忆（全列）。`pinned` 仅插入时生效（false）；后续用
    /// [`Self::memory_toggle_pin`] 翻转。
    /// # Errors
    ///
    /// 数据库连接不可用、约束冲突或 SQL 执行失败时返回 `sqlx::Error`。
    pub async fn memory_insert(&self, opts: &MemoryInsertOpts) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"
            INSERT INTO agent_memories (
                id, content, scope_type, client_id, workspace_id, tags, confidence,
                source_session_id, source_trigger, pinned
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ",
        )
        .bind(&opts.id)
        .bind(&opts.content)
        .bind(&opts.scope_type)
        .bind(&opts.client_id)
        .bind(&opts.workspace_id)
        .bind(&opts.tags)
        .bind(opts.confidence)
        .bind(&opts.source_session_id)
        .bind(&opts.source_trigger)
        .bind(i32::from(opts.pinned))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 按 id 查询单条记忆，不存在返回 None。
    /// # Errors
    ///
    /// 数据库连接不可用、SQL 执行失败或结果反序列化失败时返回 `sqlx::Error`。
    pub async fn memory_get_by_id(
        &self,
        id: &str,
    ) -> Result<Option<AgentMemoryRecord>, sqlx::Error> {
        sqlx::query_as::<_, AgentMemoryRecord>("SELECT * FROM agent_memories WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
    }

    /// 批量读取（注入回填用：向量检索命中后按 id 拉原文）。空切片返回空。
    /// # Errors
    ///
    /// 数据库连接不可用、SQL 执行失败或结果反序列化失败时返回 `sqlx::Error`。
    pub async fn memory_get_by_ids(
        &self,
        ids: &[String],
    ) -> Result<Vec<AgentMemoryRecord>, sqlx::Error> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut qb = sqlx::QueryBuilder::new("SELECT * FROM agent_memories WHERE id IN (");
        let mut sep = qb.separated(", ");
        for id in ids {
            sep.push_bind(id);
        }
        qb.push(") ORDER BY updated_at DESC");
        qb.build_query_as::<AgentMemoryRecord>()
            .fetch_all(&self.pool)
            .await
    }

    /// 更新可变字段：content / tags / scope_type / confidence。
    /// # Errors
    ///
    /// 数据库连接不可用或 SQL 执行失败时返回 `sqlx::Error`。
    pub async fn memory_update(
        &self,
        id: &str,
        content: &str,
        tags: &str,
        scope_type: &str,
        confidence: f64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE agent_memories SET content = ?, tags = ?, scope_type = ?, \
             confidence = ?, updated_at = datetime('now') WHERE id = ?",
        )
        .bind(content)
        .bind(tags)
        .bind(scope_type)
        .bind(confidence)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 按 id 删除记忆，不存在时无副作用（向量删除由上层另行处理）。
    /// # Errors
    ///
    /// 数据库连接不可用或 SQL 执行失败时返回 `sqlx::Error`。
    pub async fn memory_delete(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM agent_memories WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 翻转 pinned（1 ↔ 0）。不存在的 id 无副作用。
    /// # Errors
    ///
    /// 数据库连接不可用或 SQL 执行失败时返回 `sqlx::Error`。
    pub async fn memory_toggle_pin(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE agent_memories SET pinned = 1 - pinned, updated_at = datetime('now') \
             WHERE id = ?",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 列出记忆，支持作用域 / client / workspace / content 模糊 / pinned 过滤与排序。
    ///
    /// `scope_type` 精确过滤（'global'|'client'|'workspace'）；`client_id` /
    /// `workspace_id` 传空串等同不过滤；`q` 对 `content` 做 `LIKE %q%` 模糊匹配；
    /// `pinned`：Some(true) 只看置顶、Some(false) 只看未置顶、None 不过滤。
    /// `order` 白名单："recent"（updated_at DESC，默认）/ "created"（created_at DESC）/
    /// "confidence"（confidence DESC）/ "hits"（hit_count DESC）。
    /// # Errors
    ///
    /// 数据库连接不可用、SQL 执行失败或结果反序列化失败时返回 `sqlx::Error`。
    pub async fn memory_list(
        &self,
        filter: &MemoryListFilter,
    ) -> Result<Vec<AgentMemoryRecord>, sqlx::Error> {
        let mut qb = sqlx::QueryBuilder::new("SELECT * FROM agent_memories WHERE 1=1");
        if let Some(s) = filter.scope_type.as_deref().filter(|s| !s.is_empty()) {
            qb.push(" AND scope_type = ").push_bind(s);
        }
        if let Some(c) = filter.client_id.as_deref().filter(|c| !c.is_empty()) {
            qb.push(" AND client_id = ").push_bind(c);
        }
        if let Some(w) = filter.workspace_id.as_deref().filter(|w| !w.is_empty()) {
            qb.push(" AND workspace_id = ").push_bind(w);
        }
        if let Some(q) = filter.q.as_deref().filter(|q| !q.is_empty()) {
            qb.push(" AND content LIKE ").push_bind(format!("%{q}%"));
        }
        if filter.pinned == Some(true) {
            qb.push(" AND pinned = 1");
        } else if filter.pinned == Some(false) {
            qb.push(" AND pinned = 0");
        }
        let order_clause = match filter.order.as_deref() {
            Some("created") => "created_at DESC",
            Some("confidence") => "confidence DESC",
            Some("hits") => "hit_count DESC",
            _ => "updated_at DESC",
        };
        qb.push(" ORDER BY ").push(order_clause);
        qb.push(" LIMIT ")
            .push_bind(filter.limit)
            .push(" OFFSET ")
            .push_bind(filter.offset);
        qb.build_query_as::<AgentMemoryRecord>()
            .fetch_all(&self.pool)
            .await
    }

    /// 批量 +1 hit_count 并刷新 last_hit_at（注入命中回写）。空切片直接返回。
    /// # Errors
    ///
    /// 数据库连接不可用或 SQL 执行失败时返回 `sqlx::Error`。
    pub async fn memory_bump_hits(&self, ids: &[String]) -> Result<(), sqlx::Error> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut qb = sqlx::QueryBuilder::new(
            "UPDATE agent_memories SET hit_count = hit_count + 1, \
             last_hit_at = datetime('now') WHERE id IN (",
        );
        let mut sep = qb.separated(", ");
        for id in ids {
            sep.push_bind(id);
        }
        qb.push(")");
        qb.build().execute(&self.pool).await?;
        Ok(())
    }

    // ── 蒸馏防重 CAS ─────────────────────────────────────────────

    /// 原子标记会话已蒸馏：`UPDATE agent_sessions SET distilled=1 WHERE id=? AND
    /// distilled=0`。返回 true 表示本次调用是赢家（可触发蒸馏）；false 表示已蒸馏过
    /// 或会话不存在。归档/删除/断线/idle 多路并发只会有一个赢家。
    /// # Errors
    ///
    /// 数据库连接不可用或 SQL 执行失败时返回 `sqlx::Error`。
    pub async fn memory_mark_distilled_if_not(
        &self,
        session_id: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE agent_sessions SET distilled = 1, updated_at = datetime('now') \
             WHERE id = ? AND distilled = 0",
        )
        .bind(session_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 建一个 workspace + session，返回 session id（CAS 测试用，agent_sessions 有
    /// 到 agent_workspaces 的 FK）。
    async fn seed_session(db: &Database) -> String {
        db.agent_create_workspace(&crate::agent::AgentWorkspaceCreateOpts {
            id: "w1".to_owned(),
            name: "w".to_owned(),
            client_id: "c1".to_owned(),
            runtime_type: "host".to_owned(),
            root_path: "/tmp".to_owned(),
            docker_image: None,
            docker_container_id: None,
            agent_type: String::new(),
            agent_path: None,
            llm_model_id: None,
            agent_config_overrides: None,
            claude_tier_models: None,
        })
        .await
        .unwrap();
        db.agent_create_session("s1", "w1", None, None)
            .await
            .unwrap();
        "s1".to_string()
    }

    #[allow(
        clippy::float_cmp,
        reason = "测试中对字面量阈值 0.40 做精确相等校验，值为写入前确定的常量，经 SQLite REAL 往返应逐位相等"
    )]
    #[tokio::test]
    async fn test_settings_default_and_roundtrip() {
        let db = Database::new(":memory:").await.unwrap();

        // 无行 → 默认构造（enabled=false）
        let s = db.memory_get_settings().await.unwrap();
        assert_eq!(s.enabled, 0);
        assert_eq!(s.top_k, 8);
        assert_eq!(s.inject_budget_tokens, 1500);
        assert_eq!(s.score_threshold, 0.40);
        assert_eq!(s.pin_always_inject, 1);
        assert_eq!(s.skill_enabled, 0, "skill_enabled 默认 0（opt-in）");
        assert_eq!(s.skill_list_max, 20, "skill_list_max 默认 20");
        assert_eq!(s.wiki_enabled, 1, "wiki_enabled 默认 1");
        assert_eq!(s.wiki_list_max, 20, "wiki_list_max 默认 20");

        // upsert → 读回
        let mut s2 = s.clone();
        s2.enabled = 1;
        s2.emb_base_url = "http://localhost:11434/v1".to_string();
        s2.emb_api_key = "encrypted-key".to_string();
        s2.emb_model = "nomic-embed-text".to_string();
        s2.emb_dimension = 768;
        s2.top_k = 16;
        s2.skill_enabled = 1;
        s2.skill_list_max = 12;
        s2.wiki_enabled = 0;
        s2.wiki_list_max = 8;
        db.memory_upsert_settings(&s2).await.unwrap();
        let s3 = db.memory_get_settings().await.unwrap();
        assert_eq!(s3.enabled, 1);
        assert_eq!(s3.emb_base_url, "http://localhost:11434/v1");
        assert_eq!(s3.emb_api_key, "encrypted-key");
        assert_eq!(s3.emb_dimension, 768);
        assert_eq!(s3.top_k, 16);
        assert_eq!(s3.skill_enabled, 1);
        assert_eq!(s3.skill_list_max, 12);
        assert_eq!(s3.wiki_enabled, 0);
        assert_eq!(s3.wiki_list_max, 8);

        // 再次 upsert：created_at 保持、top_k 更新；**INSERT OR REPLACE 不重置
        // 新列**（显式列绑定遗漏会在每次 upsert 把 skill_*/wiki_* 重置为 DEFAULT）
        let mut s4 = s3.clone();
        s4.top_k = 32;
        db.memory_upsert_settings(&s4).await.unwrap();
        let s5 = db.memory_get_settings().await.unwrap();
        assert_eq!(s5.top_k, 32);
        assert_eq!(s5.skill_enabled, 1, "upsert 不应重置 skill_enabled");
        assert_eq!(s5.skill_list_max, 12, "upsert 不应重置 skill_list_max");
        assert_eq!(s5.wiki_enabled, 0, "upsert 不应重置 wiki_enabled");
        assert_eq!(s5.wiki_list_max, 8, "upsert 不应重置 wiki_list_max");
        assert_eq!(s5.created_at, s3.created_at, "created_at 应保持首建时间");
    }

    #[allow(
        clippy::float_cmp,
        reason = "测试中对字面量置信度做精确相等校验，值为写入前确定的常量，往返应相等"
    )]
    #[tokio::test]
    async fn test_memories_crud() {
        let db = Database::new(":memory:").await.unwrap();
        db.memory_insert(&MemoryInsertOpts {
            id: "m1".to_owned(),
            content: "用户喜欢简洁的代码".to_owned(),
            scope_type: "workspace".to_owned(),
            client_id: "c1".to_owned(),
            workspace_id: "w1".to_owned(),
            tags: r#"["rust"]"#.to_owned(),
            confidence: 0.9,
            source_session_id: "s1".to_owned(),
            source_trigger: "distill".to_owned(),
            pinned: false,
        })
        .await
        .unwrap();

        let m = db.memory_get_by_id("m1").await.unwrap().unwrap();
        assert_eq!(m.content, "用户喜欢简洁的代码");
        assert_eq!(m.scope_type, "workspace");
        assert_eq!(m.client_id, "c1");
        assert_eq!(m.workspace_id, "w1");
        assert_eq!(m.tags, r#"["rust"]"#);
        assert_eq!(m.confidence, 0.9);
        assert_eq!(m.source_session_id, "s1");
        assert_eq!(m.source_trigger, "distill");
        assert_eq!(m.pinned, 0);
        assert_eq!(m.hit_count, 0);
        assert!(m.last_hit_at.is_none());

        // update：content/tags/scope/confidence
        db.memory_update(
            "m1",
            "用户偏好简洁实现",
            r#"["rust","clean"]"#,
            "global",
            0.85,
        )
        .await
        .unwrap();
        let m = db.memory_get_by_id("m1").await.unwrap().unwrap();
        assert_eq!(m.content, "用户偏好简洁实现");
        assert_eq!(m.tags, r#"["rust","clean"]"#);
        assert_eq!(m.scope_type, "global");
        assert_eq!(m.confidence, 0.85);
        // 不相关字段保持
        assert_eq!(m.source_session_id, "s1");

        // delete
        db.memory_delete("m1").await.unwrap();
        assert!(db.memory_get_by_id("m1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_memory_pin_toggle() {
        let db = Database::new(":memory:").await.unwrap();
        db.memory_insert(&MemoryInsertOpts {
            id: "m1".to_owned(),
            content: "fact".to_owned(),
            scope_type: "global".to_owned(),
            client_id: String::new(),
            workspace_id: String::new(),
            tags: "[]".to_owned(),
            confidence: 0.8,
            source_session_id: "s1".to_owned(),
            source_trigger: "remember".to_owned(),
            pinned: false,
        })
        .await
        .unwrap();
        db.memory_toggle_pin("m1").await.unwrap();
        assert_eq!(db.memory_get_by_id("m1").await.unwrap().unwrap().pinned, 1);
        db.memory_toggle_pin("m1").await.unwrap();
        assert_eq!(db.memory_get_by_id("m1").await.unwrap().unwrap().pinned, 0);
    }

    #[tokio::test]
    async fn test_distilled_cas_idempotent() {
        let db = Database::new(":memory:").await.unwrap();
        let sid = seed_session(&db).await;

        // 首次：赢家返回 true
        assert!(db.memory_mark_distilled_if_not(&sid).await.unwrap());
        // 二次：已被标记，返回 false
        assert!(!db.memory_mark_distilled_if_not(&sid).await.unwrap());
        // 不存在的会话：false
        assert!(!db.memory_mark_distilled_if_not("no-such").await.unwrap());

        // 列值已落 1
        let v: i64 = sqlx::query_scalar("SELECT distilled FROM agent_sessions WHERE id = ?")
            .bind(&sid)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(v, 1);
    }

    #[allow(
        clippy::too_many_lines,
        reason = "多作用域/标签/分页的串联断言测试，长但内聚，拆分会割裂用例"
    )]
    #[tokio::test]
    async fn test_memory_list_scope_filter() {
        let db = Database::new(":memory:").await.unwrap();
        db.memory_insert(&MemoryInsertOpts {
            id: "m1".to_owned(),
            content: "全局事实".to_owned(),
            scope_type: "global".to_owned(),
            client_id: String::new(),
            workspace_id: String::new(),
            tags: "[]".to_owned(),
            confidence: 0.8,
            source_session_id: String::new(),
            source_trigger: "manual".to_owned(),
            pinned: false,
        })
        .await
        .unwrap();
        db.memory_insert(&MemoryInsertOpts {
            id: "m2".to_owned(),
            content: "客户端 c1 事实".to_owned(),
            scope_type: "client".to_owned(),
            client_id: "c1".to_owned(),
            workspace_id: String::new(),
            tags: "[]".to_owned(),
            confidence: 0.8,
            source_session_id: String::new(),
            source_trigger: "manual".to_owned(),
            pinned: false,
        })
        .await
        .unwrap();
        db.memory_insert(&MemoryInsertOpts {
            id: "m3".to_owned(),
            content: "工作区 w1 事实".to_owned(),
            scope_type: "workspace".to_owned(),
            client_id: "c1".to_owned(),
            workspace_id: "w1".to_owned(),
            tags: "[]".to_owned(),
            confidence: 0.8,
            source_session_id: String::new(),
            source_trigger: "manual".to_owned(),
            pinned: true,
        })
        .await
        .unwrap();
        db.memory_insert(&MemoryInsertOpts {
            id: "m4".to_owned(),
            content: "其他工作区事实".to_owned(),
            scope_type: "workspace".to_owned(),
            client_id: "c1".to_owned(),
            workspace_id: "w2".to_owned(),
            tags: "[]".to_owned(),
            confidence: 0.8,
            source_session_id: String::new(),
            source_trigger: "manual".to_owned(),
            pinned: false,
        })
        .await
        .unwrap();

        // 全量
        let all = db
            .memory_list(&MemoryListFilter {
                scope_type: None,
                client_id: None,
                workspace_id: None,
                q: None,
                pinned: None,
                order: None,
                limit: 100,
                offset: 0,
            })
            .await
            .unwrap();
        assert_eq!(all.len(), 4);

        // scope_type 过滤
        let ws = db
            .memory_list(&MemoryListFilter {
                scope_type: Some("workspace".to_owned()),
                client_id: None,
                workspace_id: None,
                q: None,
                pinned: None,
                order: None,
                limit: 100,
                offset: 0,
            })
            .await
            .unwrap();
        assert_eq!(ws.len(), 2);
        assert!(ws.iter().all(|m| m.scope_type == "workspace"));

        // workspace_id 过滤
        let w1 = db
            .memory_list(&MemoryListFilter {
                scope_type: None,
                client_id: None,
                workspace_id: Some("w1".to_owned()),
                q: None,
                pinned: None,
                order: None,
                limit: 100,
                offset: 0,
            })
            .await
            .unwrap();
        assert_eq!(w1.len(), 1);
        assert_eq!(w1[0].id, "m3");

        // client_id 过滤（m2/m3/m4 均属 c1）
        let c1 = db
            .memory_list(&MemoryListFilter {
                scope_type: None,
                client_id: Some("c1".to_owned()),
                workspace_id: None,
                q: None,
                pinned: None,
                order: None,
                limit: 100,
                offset: 0,
            })
            .await
            .unwrap();
        assert_eq!(c1.len(), 3);

        // pinned 过滤
        let pinned = db
            .memory_list(&MemoryListFilter {
                scope_type: None,
                client_id: None,
                workspace_id: None,
                q: None,
                pinned: Some(true),
                order: None,
                limit: 100,
                offset: 0,
            })
            .await
            .unwrap();
        assert_eq!(pinned.len(), 1);
        assert_eq!(pinned[0].id, "m3");
        let unpinned = db
            .memory_list(&MemoryListFilter {
                scope_type: None,
                client_id: None,
                workspace_id: None,
                q: None,
                pinned: Some(false),
                order: None,
                limit: 100,
                offset: 0,
            })
            .await
            .unwrap();
        assert_eq!(unpinned.len(), 3);

        // content 模糊
        let q = db
            .memory_list(&MemoryListFilter {
                scope_type: None,
                client_id: None,
                workspace_id: None,
                q: Some("客户端".to_owned()),
                pinned: None,
                order: None,
                limit: 100,
                offset: 0,
            })
            .await
            .unwrap();
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].id, "m2");
    }

    #[allow(
        clippy::float_cmp,
        reason = "测试中对字面量置信度 0.4 做精确相等校验，值为写入前确定的常量"
    )]
    #[tokio::test]
    async fn test_memory_list_order_and_paging() {
        let db = Database::new(":memory:").await.unwrap();
        for i in 0..5 {
            db.memory_insert(&MemoryInsertOpts {
                id: format!("m{i}").to_owned(),
                content: format!("fact {i}").to_owned(),
                scope_type: "global".to_owned(),
                client_id: String::new(),
                workspace_id: String::new(),
                tags: "[]".to_owned(),
                confidence: 0.1 * f64::from(i),
                source_session_id: String::new(),
                source_trigger: "manual".to_owned(),
                pinned: false,
            })
            .await
            .unwrap();
        }

        // limit/offset 分页
        let page = db
            .memory_list(&MemoryListFilter {
                scope_type: None,
                client_id: None,
                workspace_id: None,
                q: None,
                pinned: None,
                order: None,
                limit: 2,
                offset: 1,
            })
            .await
            .unwrap();
        assert_eq!(page.len(), 2);

        // 排序白名单：hits / confidence 不 panic 且按各自键序
        let by_conf = db
            .memory_list(&MemoryListFilter {
                scope_type: None,
                client_id: None,
                workspace_id: None,
                q: None,
                pinned: None,
                order: Some("confidence".to_owned()),
                limit: 100,
                offset: 0,
            })
            .await
            .unwrap();
        assert_eq!(by_conf[0].confidence, 0.4, "confidence DESC 首条应为 0.4");

        // 非法 order 回退默认（recent），不 panic
        let fallback = db
            .memory_list(&MemoryListFilter {
                scope_type: None,
                client_id: None,
                workspace_id: None,
                q: None,
                pinned: None,
                order: Some("bogus".to_owned()),
                limit: 100,
                offset: 0,
            })
            .await
            .unwrap();
        assert_eq!(fallback.len(), 5);
    }

    #[allow(
        clippy::float_cmp,
        reason = "测试中对置信度的精确写入/读回校验，往返应相等"
    )]
    #[tokio::test]
    async fn test_memory_batch_and_hits() {
        let db = Database::new(":memory:").await.unwrap();
        db.memory_insert(&MemoryInsertOpts {
            id: "m1".to_owned(),
            content: "a".to_owned(),
            scope_type: "global".to_owned(),
            client_id: String::new(),
            workspace_id: String::new(),
            tags: "[]".to_owned(),
            confidence: 0.8,
            source_session_id: String::new(),
            source_trigger: "manual".to_owned(),
            pinned: false,
        })
        .await
        .unwrap();
        db.memory_insert(&MemoryInsertOpts {
            id: "m2".to_owned(),
            content: "b".to_owned(),
            scope_type: "global".to_owned(),
            client_id: String::new(),
            workspace_id: String::new(),
            tags: "[]".to_owned(),
            confidence: 0.8,
            source_session_id: String::new(),
            source_trigger: "manual".to_owned(),
            pinned: false,
        })
        .await
        .unwrap();

        // get_by_ids：含不存在的 id，只回存在的
        let rows = db
            .memory_get_by_ids(&["m1".to_string(), "m2".to_string(), "missing".to_string()])
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);

        // bump_hits
        db.memory_bump_hits(&["m1".to_string(), "m2".to_string()])
            .await
            .unwrap();
        let m1 = db.memory_get_by_id("m1").await.unwrap().unwrap();
        assert_eq!(m1.hit_count, 1);
        assert!(m1.last_hit_at.is_some());

        // 空切片：无副作用
        db.memory_bump_hits(&[]).await.unwrap();
        db.memory_get_by_ids(&[]).await.unwrap();
    }
}
