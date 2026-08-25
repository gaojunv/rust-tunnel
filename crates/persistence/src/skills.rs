//! Skill 库数据访问层：`agent_skills` 表。
//!
//! Skill **不向量化**（数量少、清单注入无需语义检索、按 name+scope 文本去重），
//! 纯 SQLite + SQL，embedding 未配置也能工作。作用域隔离对齐 `agent_memories`
//! （global|client|workspace，坐标 = scope_type + client_id + workspace_id）。
//!
//! 本层不触碰向量（无任何 VectorStore 引用），去重/注入/use_skill 均由
//! `agent/skill/mod.rs` 组合本层 DAO 实现。

use super::agent::ser_de_normalized_dt;
use super::Database;

/// 一条 Skill 的完整行（含 content Markdown 全文）。`tags` 为 JSON 数组字符串
/// （`'["部署","linux"]'`），`scope_type` ∈ global|client|workspace。
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct AgentSkillRecord {
    pub id: String,
    pub name: String,
    pub description: String,
    pub content: String,
    pub scope_type: String,
    pub client_id: String,
    pub workspace_id: String,
    pub tags: String,
    pub enabled: i32,
    pub source_session_id: String,
    pub source_trigger: String,
    pub use_count: i64,
    pub last_used_at: Option<String>,
    #[serde(serialize_with = "ser_de_normalized_dt")]
    pub created_at: String,
    #[serde(serialize_with = "ser_de_normalized_dt")]
    pub updated_at: String,
}

/// Skill 列表/清单行（**不含 content**）。清单注入与列表展示都不需要全文——
/// content 最大 16KB，SELECT 时排除避免大字段拖慢查询与响应体积。
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct AgentSkillSummary {
    pub id: String,
    pub name: String,
    pub description: String,
    pub scope_type: String,
    pub client_id: String,
    pub workspace_id: String,
    pub tags: String,
    pub enabled: i32,
    pub source_session_id: String,
    pub source_trigger: String,
    pub use_count: i64,
    pub last_used_at: Option<String>,
    #[serde(serialize_with = "ser_de_normalized_dt")]
    pub created_at: String,
    #[serde(serialize_with = "ser_de_normalized_dt")]
    pub updated_at: String,
}

/// `AgentSkillSummary` 的 SELECT 列清单（不含 content）。
const SKILL_SUMMARY_COLS: &str = "id, name, description, scope_type, client_id, workspace_id, \
     tags, enabled, source_session_id, source_trigger, use_count, last_used_at, created_at, updated_at";

impl Database {
    /// 插入一条 Skill（全列）。`enabled` 默认 1、`use_count` 默认 0（不显式提供）。
    #[allow(clippy::too_many_arguments)]
    pub async fn skill_insert(
        &self,
        id: &str,
        name: &str,
        description: &str,
        content: &str,
        scope_type: &str,
        client_id: &str,
        workspace_id: &str,
        tags: &str,
        source_session_id: &str,
        source_trigger: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"
            INSERT INTO agent_skills (
                id, name, description, content, scope_type, client_id, workspace_id, tags,
                source_session_id, source_trigger
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ",
        )
        .bind(id)
        .bind(name)
        .bind(description)
        .bind(content)
        .bind(scope_type)
        .bind(client_id)
        .bind(workspace_id)
        .bind(tags)
        .bind(source_session_id)
        .bind(source_trigger)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn skill_get_by_id(&self, id: &str) -> Result<Option<AgentSkillRecord>, sqlx::Error> {
        sqlx::query_as::<_, AgentSkillRecord>("SELECT * FROM agent_skills WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
    }

    /// 同作用域同名查重（去重用）。`name` 必须已由调用方 normalize（trim + lowercase）——
    /// 本方法做精确匹配，不在此归一化。
    pub async fn skill_get_by_name_scope(
        &self,
        name: &str,
        scope_type: &str,
        client_id: &str,
        workspace_id: &str,
    ) -> Result<Option<AgentSkillRecord>, sqlx::Error> {
        sqlx::query_as::<_, AgentSkillRecord>(
            "SELECT * FROM agent_skills \
             WHERE name = ? AND scope_type = ? AND client_id = ? AND workspace_id = ?",
        )
        .bind(name)
        .bind(scope_type)
        .bind(client_id)
        .bind(workspace_id)
        .fetch_optional(&self.pool)
        .await
    }

    /// 更新可变字段：name / description / content / tags / scope 三元组（scope 变更
    /// 同步坐标，调用方已按 `scope_coords` 归一化）。**不动** enabled / use_count
    /// （去重 upsert 与手动编辑都不应重置使用统计与开关状态）。
    #[allow(clippy::too_many_arguments)]
    pub async fn skill_update(
        &self,
        id: &str,
        name: &str,
        description: &str,
        content: &str,
        tags: &str,
        scope_type: &str,
        client_id: &str,
        workspace_id: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE agent_skills SET name = ?, description = ?, content = ?, tags = ?, \
             scope_type = ?, client_id = ?, workspace_id = ?, updated_at = datetime('now') \
             WHERE id = ?",
        )
        .bind(name)
        .bind(description)
        .bind(content)
        .bind(tags)
        .bind(scope_type)
        .bind(client_id)
        .bind(workspace_id)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn skill_delete(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM agent_skills WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 翻转 enabled（1 ↔ 0）。不存在的 id 无副作用。
    pub async fn skill_toggle_enabled(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE agent_skills SET enabled = 1 - enabled, updated_at = datetime('now') \
             WHERE id = ?",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 列出 Skill（列表/清单视图，不含 content）。支持作用域 / client / workspace /
    /// name+description 模糊 / enabled 过滤与排序分页。
    ///
    /// `scope_type` 精确过滤；`client_id` / `workspace_id` 传空串等同不过滤；`q` 对
    /// `name`/`description` 做 `LIKE %q%` 模糊匹配；`enabled`：Some(true) 只看启用、
    /// Some(false) 只看停用、None 不过滤。`sort` 白名单："recent"（updated_at DESC，
    /// 默认）/ "created"（created_at DESC）/ "uses"（use_count DESC）。
    #[allow(clippy::too_many_arguments)]
    pub async fn skill_list(
        &self,
        scope_type: Option<&str>,
        client_id: Option<&str>,
        workspace_id: Option<&str>,
        q: Option<&str>,
        enabled: Option<bool>,
        sort: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<AgentSkillSummary>, sqlx::Error> {
        let mut qb = sqlx::QueryBuilder::new("SELECT ");
        qb.push(SKILL_SUMMARY_COLS)
            .push(" FROM agent_skills WHERE 1=1");
        if let Some(s) = scope_type.filter(|s| !s.is_empty()) {
            qb.push(" AND scope_type = ").push_bind(s);
        }
        if let Some(c) = client_id.filter(|c| !c.is_empty()) {
            qb.push(" AND client_id = ").push_bind(c);
        }
        if let Some(w) = workspace_id.filter(|w| !w.is_empty()) {
            qb.push(" AND workspace_id = ").push_bind(w);
        }
        if let Some(q) = q.filter(|q| !q.is_empty()) {
            qb.push(" AND (name LIKE ")
                .push_bind(format!("%{q}%"))
                .push(" OR description LIKE ")
                .push_bind(format!("%{q}%"))
                .push(")");
        }
        if enabled == Some(true) {
            qb.push(" AND enabled = 1");
        } else if enabled == Some(false) {
            qb.push(" AND enabled = 0");
        }
        let order_clause = match sort {
            Some("created") => "created_at DESC",
            Some("uses") => "use_count DESC",
            _ => "updated_at DESC",
        };
        qb.push(" ORDER BY ").push(order_clause);
        qb.push(" LIMIT ")
            .push_bind(limit)
            .push(" OFFSET ")
            .push_bind(offset);
        qb.build_query_as::<AgentSkillSummary>()
            .fetch_all(&self.pool)
            .await
    }

    /// use_skill 命中回写：+1 use_count 并刷新 last_used_at。
    pub async fn skill_bump_use(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE agent_skills SET use_count = use_count + 1, \
             last_used_at = datetime('now') WHERE id = ?",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 清单专用查询（会话开始注入）：enabled=1 且对 (client_id, workspace_id)
    /// **作用域可见**（global 恒可见 / client 匹配 client_id / workspace 需
    /// client_id + workspace_id 都匹配），按 use_count DESC 取前 `limit` 条。
    /// **不 SELECT content**（清单只需 name+description）。
    pub async fn skill_injectable(
        &self,
        client_id: &str,
        workspace_id: &str,
        limit: i64,
    ) -> Result<Vec<AgentSkillSummary>, sqlx::Error> {
        sqlx::query_as::<_, AgentSkillSummary>(&format!(
            "SELECT {SKILL_SUMMARY_COLS} FROM agent_skills \
             WHERE enabled = 1 AND ( \
                 scope_type = 'global' \
                 OR (scope_type = 'client' AND client_id = ?) \
                 OR (scope_type = 'workspace' AND client_id = ? AND workspace_id = ?) \
             ) ORDER BY use_count DESC LIMIT ?",
        ))
        .bind(client_id)
        .bind(client_id)
        .bind(workspace_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn seed(db: &Database, id: &str, name: &str, scope: &str, client: &str, ws: &str) {
        db.skill_insert(
            id,
            name,
            "desc",
            "content",
            scope,
            client,
            ws,
            r#"["rust"]"#,
            "",
            "manual",
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn skills_crud_and_scope() {
        let db = Database::new(":memory:").await.unwrap();
        db.skill_insert(
            "s1",
            "deploy-app",
            "发布应用",
            "## 发布步骤\n1. build",
            "workspace",
            "c1",
            "w1",
            r#"["deploy","linux"]"#,
            "src-sess",
            "distill",
        )
        .await
        .unwrap();

        let row = db.skill_get_by_id("s1").await.unwrap().unwrap();
        assert_eq!(row.name, "deploy-app");
        assert_eq!(row.description, "发布应用");
        assert_eq!(row.content, "## 发布步骤\n1. build");
        assert_eq!(row.scope_type, "workspace");
        assert_eq!(row.client_id, "c1");
        assert_eq!(row.workspace_id, "w1");
        assert_eq!(row.tags, r#"["deploy","linux"]"#);
        assert_eq!(row.enabled, 1);
        assert_eq!(row.source_session_id, "src-sess");
        assert_eq!(row.source_trigger, "distill");
        assert_eq!(row.use_count, 0);
        assert!(row.last_used_at.is_none());

        // 同名同作用域查重命中；异作用域未命中
        assert!(db
            .skill_get_by_name_scope("deploy-app", "workspace", "c1", "w1")
            .await
            .unwrap()
            .is_some());
        assert!(db
            .skill_get_by_name_scope("deploy-app", "global", "", "")
            .await
            .unwrap()
            .is_none());

        // update：content/tags/scope 坐标同步，enabled/use_count 保持
        db.skill_update(
            "s1",
            "deploy-app",
            "新描述",
            "新内容",
            "[]",
            "global",
            "",
            "", // scope 变更
        )
        .await
        .unwrap();
        let row = db.skill_get_by_id("s1").await.unwrap().unwrap();
        assert_eq!(row.description, "新描述");
        assert_eq!(row.content, "新内容");
        assert_eq!(row.scope_type, "global");
        assert_eq!(row.client_id, "");
        assert_eq!(row.workspace_id, "");
        assert_eq!(row.enabled, 1, "enabled 不被 update 重置");
        assert_eq!(row.use_count, 0, "use_count 不被 update 重置");

        // toggle
        db.skill_toggle_enabled("s1").await.unwrap();
        assert_eq!(db.skill_get_by_id("s1").await.unwrap().unwrap().enabled, 0);
        db.skill_toggle_enabled("s1").await.unwrap();
        assert_eq!(db.skill_get_by_id("s1").await.unwrap().unwrap().enabled, 1);

        // bump_use
        db.skill_bump_use("s1").await.unwrap();
        let row = db.skill_get_by_id("s1").await.unwrap().unwrap();
        assert_eq!(row.use_count, 1);
        assert!(row.last_used_at.is_some());
        db.skill_bump_use("s1").await.unwrap();
        assert_eq!(
            db.skill_get_by_id("s1").await.unwrap().unwrap().use_count,
            2
        );

        // delete → 404 语义
        db.skill_delete("s1").await.unwrap();
        assert!(db.skill_get_by_id("s1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn skill_list_filters_and_order() {
        let db = Database::new(":memory:").await.unwrap();
        seed(&db, "g1", "global-skill", "global", "", "").await;
        seed(&db, "c1", "client-skill", "client", "c1", "").await;
        seed(&db, "w1a", "release-check", "workspace", "c1", "w1").await;
        seed(&db, "w2", "other-ws", "workspace", "c1", "w2").await;
        // 一个停用 + 高使用
        db.skill_insert(
            "c2",
            "disabled-skill",
            "desc",
            "content",
            "global",
            "",
            "",
            "[]",
            "",
            "manual",
        )
        .await
        .unwrap();
        db.skill_toggle_enabled("c2").await.unwrap();
        db.skill_bump_use("w1a").await.unwrap();
        db.skill_bump_use("w1a").await.unwrap();

        // 全量（summary 序列化不含 content 字段）
        let all = db
            .skill_list(None, None, None, None, None, None, 100, 0)
            .await
            .unwrap();
        assert_eq!(all.len(), 5);
        for s in &all {
            let json = serde_json::to_value(s).unwrap();
            assert!(json.get("content").is_none(), "summary 不应含 content 字段");
        }

        // scope 过滤（seed：2 个 workspace + 2 个 global + 1 个 client）
        let ws = db
            .skill_list(Some("workspace"), None, None, None, None, None, 100, 0)
            .await
            .unwrap();
        assert_eq!(ws.len(), 2);
        assert!(ws.iter().all(|s| s.scope_type == "workspace"));

        // q 对 name / description 模糊
        let q = db
            .skill_list(None, None, None, Some("release"), None, None, 100, 0)
            .await
            .unwrap();
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].name, "release-check");
        let q_desc = db
            .skill_list(None, None, None, Some("desc"), None, None, 100, 0)
            .await
            .unwrap();
        assert_eq!(q_desc.len(), 5, "所有 description 都是 'desc'");

        // enabled 过滤
        let en = db
            .skill_list(None, None, None, None, Some(true), None, 100, 0)
            .await
            .unwrap();
        assert_eq!(en.len(), 4);
        let dis = db
            .skill_list(None, None, None, None, Some(false), None, 100, 0)
            .await
            .unwrap();
        assert_eq!(dis.len(), 1);
        assert_eq!(dis[0].id, "c2");

        // uses 排序：w1a use_count=2 最前
        let by_uses = db
            .skill_list(None, None, None, None, None, Some("uses"), 100, 0)
            .await
            .unwrap();
        assert_eq!(by_uses[0].id, "w1a");
        // created 排序：created_at 同秒可能并列，不 panic 即可
        let _ = db
            .skill_list(None, None, None, None, None, Some("created"), 100, 0)
            .await
            .unwrap();
        // 非法 sort 回退 recent，不 panic
        let _ = db
            .skill_list(None, None, None, None, None, Some("bogus"), 100, 0)
            .await
            .unwrap();

        // 分页
        let page = db
            .skill_list(None, None, None, None, None, None, 2, 1)
            .await
            .unwrap();
        assert_eq!(page.len(), 2);
    }

    #[tokio::test]
    async fn skill_injectable_scope_visibility() {
        let db = Database::new(":memory:").await.unwrap();
        seed(&db, "g1", "global-skill", "global", "", "").await;
        seed(&db, "c1", "client-skill", "client", "c1", "").await;
        seed(&db, "c2", "other-client", "client", "c2", "").await;
        seed(&db, "w1a", "ws-skill", "workspace", "c1", "w1").await;
        seed(&db, "w2", "other-ws", "workspace", "c1", "w2").await;
        seed(&db, "d1", "disabled", "global", "", "").await;
        db.skill_toggle_enabled("d1").await.unwrap();
        // use_count 影响排序
        db.skill_bump_use("w1a").await.unwrap();
        db.skill_bump_use("w1a").await.unwrap();
        db.skill_bump_use("g1").await.unwrap();

        let rows = db.skill_injectable("c1", "w1", 100).await.unwrap();
        // 可见：global g1 / client c1 / workspace w1a；不可见：c2、w2、disabled
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids.len(), 3, "rows: {rows:?}");
        // use_count DESC：w1a(2) > g1(1) > c1(0)
        assert_eq!(ids[0], "w1a");
        assert_eq!(ids[1], "g1");
        assert_eq!(ids[2], "c1");
        // summary 不含 content 字段
        let json = serde_json::to_value(&rows[0]).unwrap();
        assert!(json.get("content").is_none(), "清单行不应含 content");

        // limit
        let limited = db.skill_injectable("c1", "w1", 2).await.unwrap();
        assert_eq!(limited.len(), 2);

        // 其他 workspace：w2 可见、w1a 不可见
        let other = db.skill_injectable("c1", "w2", 100).await.unwrap();
        let ids2: Vec<&str> = other.iter().map(|r| r.id.as_str()).collect();
        assert!(ids2.contains(&"w2"));
        assert!(!ids2.contains(&"w1a"));
    }
}
