//! 路由实体（provider / model / group）的内存缓存。
//!
//! 网关请求热路径此前每次都要做多趟 DB 查询：`llm_find_model_by_name_or_alias` +
//! `llm_get_provider`，模型组场景每个成员还要再查一次 provider（N+1）。模型/提供商的
//! 数量级很小（几十个），但请求可能很密集，DB 往返很快成为瓶颈。
//!
//! 本模块提供一个惰性全量快照缓存：首次访问时从 DB 装载全部路由实体（此时解密 provider
//! 的敏感字段），在管理 API 写入（provider/model/group CRUD）后通过 [`crate::route_cache::RouteCache::invalidate`]
//! 失效，下一次访问自动重载。写入是低频操作，全量失效代价可忽略。

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use super::crypto::LlmCipher;
use super::ProviderConfig;
use rust_tunnel_persistence::Database;

/// 缓存中的模型条目。
#[derive(Debug, Clone)]
pub struct CachedModel {
    /// 模型 id。
    pub id: String,
    /// 所属提供商 id。
    pub provider_id: String,
    /// 上游真实模型名。
    pub model_name: String,
    /// 对外别名（为空则直接暴露 `model_name`）。
    pub alias: String,
    /// 是否启用。
    pub enabled: bool,
    /// per-model 额外配置 JSON。
    pub extra_config: Option<String>,
}

/// 缓存中的模型组（成员按 priority 升序）。
#[derive(Debug, Clone)]
pub struct CachedGroup {
    /// 组 id。
    pub id: String,
    /// 组名。
    pub name: String,
    /// 是否启用。
    pub enabled: bool,
    /// `(model_id, priority)`，按 priority 升序。
    pub members: Vec<(String, i32)>,
}

/// 一次全量装载的路由快照。
#[derive(Debug, Clone, Default)]
pub struct RouteSnapshot {
    /// 提供商快照（`provider_id` → 配置）。
    pub providers: HashMap<String, ProviderConfig>,
    /// 模型快照（`model_id` → 条目）。
    pub models: HashMap<String, CachedModel>,
    /// 模型装载顺序（对应 DB `ORDER BY created_at`，/v1/models 按此稳定排序）。
    pub model_order: Vec<String>,
    /// `model_name` 或 `alias` → model_id（仅收录已启用模型）。
    pub name_lookup: HashMap<String, String>,
    /// 模型组快照（`group_id` → 组）。
    pub groups: HashMap<String, CachedGroup>,
    /// 组名 → group id（仅收录已启用组）。
    pub group_name_lookup: HashMap<String, String>,
}

impl RouteSnapshot {
    /// 按 id 取 provider（含已解密的关键字段）。
    #[must_use]
    pub fn provider(&self, id: &str) -> Option<&ProviderConfig> {
        self.providers.get(id)
    }

    /// 按 id 取模型（含被禁用的）。
    #[must_use]
    pub fn model(&self, id: &str) -> Option<&CachedModel> {
        self.models.get(id)
    }

    /// 模型名/别名解析（语义与 `llm_find_model_by_name_or_alias` 一致）：
    /// 只命中已启用模型，且 `model_name` 精确匹配优先于 `alias` 匹配。
    #[must_use]
    pub fn find_model_by_name_or_alias(&self, name: &str) -> Option<&CachedModel> {
        self.name_lookup
            .get(name)
            .and_then(|id| self.models.get(id))
            .filter(|m| m.enabled)
    }

    /// 按组名解析（只命中已启用组）。
    #[must_use]
    pub fn group_by_name(&self, name: &str) -> Option<&CachedGroup> {
        self.group_name_lookup
            .get(name)
            .and_then(|id| self.groups.get(id))
            .filter(|g| g.enabled)
    }

    /// 组内成员（`(model_id, priority)`，priority 升序）。
    #[must_use]
    pub fn group_members(&self, group_id: &str) -> &[(String, i32)] {
        self.groups
            .get(group_id)
            .map(|g| g.members.as_slice())
            .unwrap_or_default()
    }

    /// provider 显示名（/v1/models 的 owned_by 用）。
    #[must_use]
    pub fn provider_name(&self, id: &str) -> &str {
        self.providers
            .get(id)
            .map_or("unknown", |p| p.name.as_str())
    }

    /// 生成本网关可用的模型列表（OpenAI `/v1/models` 格式）。
    /// 仅包含已启用且所属 provider 已启用的模型；alias 非空时展示 alias；按装载顺序稳定排序。
    #[must_use]
    pub fn available_models(&self) -> Vec<serde_json::Value> {
        self.model_order
            .iter()
            .filter_map(|id| self.models.get(id))
            .filter(|m| m.enabled)
            .filter(|m| {
                self.providers
                    .get(&m.provider_id)
                    .is_some_and(|p| p.enabled)
            })
            .map(|m| {
                serde_json::json!({
                    "id": if m.alias.is_empty() { &m.model_name } else { &m.alias },
                    "object": "model",
                    "created": 0,
                    "owned_by": self.provider_name(&m.provider_id),
                })
            })
            .collect()
    }
}

/// 快照装载的失败在启动/失效后首次命中时静默降级为空快照（caller 保守处理）。
async fn load_snapshot(db: &Database, cipher: Option<&LlmCipher>) -> RouteSnapshot {
    let mut snap = RouteSnapshot::default();

    // 1) providers（解密 api_key / extra_config；解密失败视为不可用，跳过）
    if let Ok(providers) = db.llm_list_providers().await {
        for p in providers {
            let api_key = match super::crypto::decrypt_field(cipher, &p.api_key) {
                Ok(k) => k,
                Err(e) => {
                    tracing::warn!(
                        provider_id = %p.id,
                        "route cache: failed to decrypt provider api_key, provider skipped: {}",
                        e
                    );
                    continue;
                }
            };
            let extra_config = match p.extra_config {
                Some(ec) => super::crypto::decrypt_field(cipher, &ec).ok(),
                None => None,
            };
            snap.providers.insert(
                p.id.clone(),
                ProviderConfig {
                    id: p.id,
                    name: p.name,
                    provider_type: p.provider_type,
                    base_url: p.base_url,
                    api_key,
                    extra_config,
                    // 防御性归一：库里历史脏数据可能存 ""（前端旧版本清空时写入），
                    // 归一成 None 后不再触发 Anthropic 直通（Some("") 的 is_some() 为 true）。
                    anthropic_base_url: super::normalize_anthropic_base_url(p.anthropic_base_url),
                    enabled: p.enabled != 0,
                    created_at: p.created_at,
                    updated_at: p.updated_at,
                },
            );
        }
    }

    // 2) models + name/alias 索引
    if let Ok(models) = db.llm_list_models().await {
        for m in models {
            let cached = CachedModel {
                id: m.id,
                provider_id: m.provider_id,
                model_name: m.model_name,
                alias: m.alias,
                enabled: m.enabled != 0,
                extra_config: m.extra_config,
            };
            snap.models.insert(cached.id.clone(), cached.clone());
            snap.model_order.push(cached.id.clone());
            if cached.enabled {
                if !cached.model_name.is_empty() {
                    snap.name_lookup
                        .insert(cached.model_name.clone(), cached.id.clone());
                }
                if !cached.alias.is_empty() {
                    // alias 不得覆盖已有的 model_name 精确匹配（与 DB 查询的
                    // ORDER BY (model_name = ?) DESC 语义一致）。
                    snap.name_lookup
                        .entry(cached.alias.clone())
                        .or_insert_with(|| cached.id.clone());
                }
            }
        }
    }

    // 3) groups + 组名索引 + 成员
    if let Ok(groups) = db.llm_list_model_groups().await {
        for g in groups {
            let members = db
                .llm_list_group_members(&g.id)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|m| (m.model_id, m.priority))
                .collect();
            let group = CachedGroup {
                id: g.id.clone(),
                name: g.name.clone(),
                enabled: g.enabled != 0,
                members,
            };
            snap.groups.insert(g.id.clone(), group);
            if g.enabled != 0 && !g.name.is_empty() {
                snap.group_name_lookup
                    .entry(g.name.clone())
                    .or_insert_with(|| g.id.clone());
            }
        }
    }

    snap
}

/// 路由缓存：惰性全量装载 + 写时失效。
///
/// 内部维护一个 generation 计数：`invalidate` 递增 generation 并保留旧快照，
/// 下一次读取发现 generation 不匹配时重新装载。并发安全（tokio::RwLock +
/// 双检锁），陡峭的并发请求流下最多重复装载一次。
#[derive(Debug, Clone, Default)]
pub struct RouteCache {
    inner: Arc<RwLock<CacheInner>>,
}

#[derive(Debug, Clone)]
struct CacheInner {
    generation: u64,
    loaded_generation: u64,
    snapshot: Arc<RouteSnapshot>,
}

impl Default for CacheInner {
    fn default() -> Self {
        Self {
            generation: 0,
            // 哨兵：保证首次读取（generation=0）触发一次装载。
            loaded_generation: u64::MAX,
            snapshot: Arc::new(RouteSnapshot::default()),
        }
    }
}

impl RouteCache {
    /// 新建空缓存（首次读取时才从 DB 装载）。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 使缓存失效：任何 provider/model/group 写入后调用，
    /// 下一次访问按最新 DB 状态重新装载。
    pub async fn invalidate(&self) {
        let mut inner = self.inner.write().await;
        inner.generation = inner.generation.wrapping_add(1);
    }

    /// 获取当前快照（必要时先装载）。`db` 为 None 时返回空快照。
    pub async fn snapshot(
        &self,
        db: Option<&Database>,
        cipher: Option<&LlmCipher>,
    ) -> Arc<RouteSnapshot> {
        // 快速路径：generation 未变，直接复用已装载快照。
        {
            let inner = self.inner.read().await;
            if inner.loaded_generation == inner.generation {
                return inner.snapshot.clone();
            }
        }
        // 慢路径：拿写锁后二次确认，避免并发重复装载。
        let mut inner = self.inner.write().await;
        if inner.loaded_generation != inner.generation {
            let snap = match db {
                Some(db) => load_snapshot(db, cipher).await,
                None => RouteSnapshot::default(),
            };
            inner.snapshot = Arc::new(snap);
            inner.loaded_generation = inner.generation;
        }
        inner.snapshot.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_tunnel_persistence::Database;

    async fn seeded_db() -> Database {
        let db = Database::new(":memory:").await.unwrap();
        let cipher = super::super::crypto::LlmCipher::from_master_key([1u8; 32]);
        let enc = |s: &str| cipher.encrypt(s);

        db.llm_save_provider(
            "p1",
            "DeepSeek",
            "deepseek",
            "https://api.deepseek.com",
            &enc("sk-p1"),
            None,
            None,
            true,
        )
        .await
        .unwrap();
        db.llm_save_provider(
            "p2",
            "Kimi",
            "kimi",
            "https://api.moonshot.cn",
            &enc("sk-p2"),
            None,
            None,
            true,
        )
        .await
        .unwrap();
        // 单模型链
        db.llm_save_model("m1", "p1", "deepseek-chat", "", "[]", true, None)
            .await
            .unwrap();
        // 带 alias 的模型
        db.llm_save_model(
            "m2",
            "p1",
            "deepseek-reasoner",
            "fast-reason",
            "[]",
            true,
            None,
        )
        .await
        .unwrap();
        // 禁用模型（不应出现在 name_lookup）
        db.llm_save_model("m3", "p2", "kimi-disabled", "", "[]", false, None)
            .await
            .unwrap();

        // 模型组 router = [m1(1), m2(2)]
        db.llm_create_model_group("g1", "router", true)
            .await
            .unwrap();
        db.llm_replace_group_members("g1", &[("m1".into(), 1), ("m2".into(), 2)])
            .await
            .unwrap();

        db
    }

    #[tokio::test]
    async fn snapshot_loads_decrypted_providers_and_lookups() {
        let db = seeded_db().await;
        let cipher = LlmCipher::from_master_key([1u8; 32]);
        let cache = RouteCache::new();

        let snap = cache.snapshot(Some(&db), Some(&cipher)).await;
        // provider 已解密
        assert_eq!(snap.provider("p1").unwrap().api_key, "sk-p1");
        assert_eq!(snap.provider("p2").unwrap().api_key, "sk-p2");
        assert!(snap.provider("missing").is_none());

        // model_name / alias 命中
        assert!(snap.find_model_by_name_or_alias("deepseek-chat").is_some());
        assert!(snap.find_model_by_name_or_alias("fast-reason").is_some());
        // 禁用模型不应命中
        assert!(snap.find_model_by_name_or_alias("kimi-disabled").is_none());
        assert!(snap.find_model_by_name_or_alias("nope").is_none());

        // 组与成员
        let g = snap.group_by_name("router").unwrap();
        assert_eq!(
            g.members,
            vec![("m1".to_string(), 1), ("m2".to_string(), 2)]
        );

        // available_models：m3 禁用，p1/p2 启用 → 2 个
        let models = snap.available_models();
        let ids: Vec<&str> = models.iter().filter_map(|m| m["id"].as_str()).collect();
        assert_eq!(ids, vec!["deepseek-chat", "fast-reason"]);
    }

    #[tokio::test]
    async fn alias_does_not_override_model_name_match() {
        let db = seeded_db().await;
        let cipher = LlmCipher::from_master_key([1u8; 32]);
        let cache = RouteCache::new();
        // 首次装载（触发一次快照加载），随后插入撞名的模型并失效
        let _ = cache.snapshot(Some(&db), Some(&cipher)).await;

        // 再插入一个 alias 撞名的模型后重载，验证 model_name 优先
        db.llm_save_model(
            "m4",
            "p1",
            "other-model",
            "deepseek-chat", // alias 与 m1 的 model_name 相同
            "[]",
            true,
            None,
        )
        .await
        .unwrap();
        cache.invalidate().await;
        let snap = cache.snapshot(Some(&db), Some(&cipher)).await;

        let hit = snap.find_model_by_name_or_alias("deepseek-chat").unwrap();
        assert_eq!(hit.id, "m1", "model_name 精确匹配应优先于 alias");
    }

    #[tokio::test]
    async fn cache_reloads_after_invalidate() {
        let db = seeded_db().await;
        let cipher = LlmCipher::from_master_key([1u8; 32]);
        let cache = RouteCache::new();

        let snap = cache.snapshot(Some(&db), Some(&cipher)).await;
        assert!(snap.find_model_by_name_or_alias("new-model").is_none());

        // 直接写库（绕过 API）后 invalidate → 下次读取反映最新状态
        db.llm_save_model("m9", "p1", "new-model", "", "[]", true, None)
            .await
            .unwrap();
        cache.invalidate().await;
        let snap = cache.snapshot(Some(&db), Some(&cipher)).await;
        assert!(snap.find_model_by_name_or_alias("new-model").is_some());
    }

    #[tokio::test]
    async fn no_db_returns_empty_snapshot() {
        let cache = RouteCache::new();
        let snap = cache.snapshot(None, None).await;
        assert!(snap.provider("any").is_none());
        assert!(snap.find_model_by_name_or_alias("any").is_none());
        assert!(snap.available_models().is_empty());
    }
}
