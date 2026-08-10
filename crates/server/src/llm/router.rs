use crate::llm::{LlmState, ProviderConfig};

/// 模型路由解析失败的类别。
#[derive(Debug)]
pub enum ResolveError {
    /// 模型名/别名未匹配（404，应附可用模型列表）
    ModelNotFound(String),
    /// 模型所属提供商被禁用
    ProviderDisabled(String),
    /// 数据库不可用或查询失败
    Db(String),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ModelNotFound(m) => write!(f, "model '{}' not found", m),
            Self::ProviderDisabled(m) => write!(f, "provider for model '{}' is disabled", m),
            Self::Db(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for ResolveError {}

/// 候选链中的单个候选（provider 敏感字段已解密，可直接发请求）。
#[derive(Debug, Clone)]
pub struct Candidate {
    /// 候选模型所属 provider 配置。
    pub provider: ProviderConfig,
    /// 上游真实 model 值（写入请求体 `model` 字段）。
    pub model_name: String,
    /// 模型记录 id（熔断器粒度键）。
    pub model_id: String,
    /// 组内尝试顺序（小者优先）；单模型链为 0。
    pub priority: i64,
}

/// 一次请求的有序候选链。
#[derive(Debug, Clone)]
pub struct CandidateChain {
    /// 按 priority 升序的候选列表。
    pub candidates: Vec<Candidate>,
    /// 命中模型组时为组名；单模型解析为 None。
    pub group_name: Option<String>,
}

/// 按 record 构建 ProviderConfig（api_key / extra_config 解密）。
async fn build_provider(
    state: &LlmState,
    provider_id: &str,
    model_for_err: &str,
) -> Result<Option<ProviderConfig>, ResolveError> {
    let db = state
        .db
        .as_ref()
        .ok_or_else(|| ResolveError::Db("database not available".into()))?;
    let provider_record = db
        .llm_get_provider(provider_id)
        .await
        .map_err(|e| ResolveError::Db(format!("database error: {}", e)))?
        .ok_or_else(|| ResolveError::ModelNotFound(model_for_err.to_string()))?;
    if provider_record.enabled == 0 {
        return Ok(None); // provider 禁用：候选被过滤（单模型场景由调用方转 ProviderDisabled）
    }
    let api_key = super::crypto::decrypt_field(state.cipher.as_ref(), &provider_record.api_key)
        .map_err(|e| ResolveError::Db(format!("failed to decrypt provider api key: {}", e)))?;
    let extra_config = match provider_record.extra_config {
        Some(ec) => Some(
            super::crypto::decrypt_field(state.cipher.as_ref(), &ec)
                .map_err(|e| ResolveError::Db(format!("failed to decrypt extra_config: {}", e)))?,
        ),
        None => None,
    };
    Ok(Some(ProviderConfig {
        id: provider_record.id,
        name: provider_record.name,
        provider_type: provider_record.provider_type,
        base_url: provider_record.base_url,
        api_key,
        extra_config,
        anthropic_base_url: provider_record.anthropic_base_url,
        enabled: provider_record.enabled != 0,
        created_at: provider_record.created_at,
        updated_at: provider_record.updated_at,
    }))
}

/// 解析模型名 / 别名 / 模型组名为有序候选链。
///
/// 解析顺序：`model_name` 精确 → `alias` → 组名（组 enabled）。
/// 组成员中模型禁用或 provider 禁用者被过滤；过滤后为空视同 `ModelNotFound`。
pub async fn resolve_with_failover(
    state: &LlmState,
    model: &str,
) -> Result<CandidateChain, ResolveError> {
    let db = state
        .db
        .as_ref()
        .ok_or_else(|| ResolveError::Db("database not available".into()))?;

    // 1) model_name / alias 精确命中 → 单元素链（现有行为）
    if let Some(model_record) = db
        .llm_find_model_by_name_or_alias(model)
        .await
        .map_err(|e| ResolveError::Db(format!("database error: {}", e)))?
    {
        let provider = build_provider(state, &model_record.provider_id, model)
            .await?
            .ok_or_else(|| ResolveError::ProviderDisabled(model.to_string()))?;
        return Ok(CandidateChain {
            candidates: vec![Candidate {
                provider,
                model_name: model_record.model_name,
                model_id: model_record.id,
                priority: 0,
            }],
            group_name: None,
        });
    }

    // 2) 组名命中
    let Some(group) = db
        .llm_find_group_by_name(model)
        .await
        .map_err(|e| ResolveError::Db(format!("database error: {}", e)))?
    else {
        return Err(ResolveError::ModelNotFound(model.to_string()));
    };

    let members = db
        .llm_list_group_members(&group.id)
        .await
        .map_err(|e| ResolveError::Db(format!("database error: {}", e)))?;

    let mut candidates = Vec::new();
    for m in members {
        if m.model_enabled == 0 {
            continue;
        }
        let Some(provider) = build_provider(state, &m.provider_id, model).await? else {
            continue; // provider 禁用：过滤
        };
        candidates.push(Candidate {
            provider,
            model_name: m.model_name,
            model_id: m.model_id,
            priority: i64::from(m.priority),
        });
    }

    if candidates.is_empty() {
        return Err(ResolveError::ModelNotFound(model.to_string()));
    }

    Ok(CandidateChain {
        candidates,
        group_name: Some(group.name),
    })
}

/// Resolve a model name (or alias) to a ProviderConfig with the actual model_name.
/// Returns (provider, actual_model_name, model_id) or a typed error.
///
/// 单模型薄封装：内部走 `resolve_with_failover`，取候选链首元素。
pub async fn resolve_model(
    state: &LlmState,
    model: &str,
) -> Result<(ProviderConfig, String, String), ResolveError> {
    let chain = resolve_with_failover(state, model).await?;
    let c = chain
        .candidates
        .into_iter()
        .next()
        .ok_or_else(|| ResolveError::ModelNotFound(model.to_string()))?;
    Ok((c.provider, c.model_name, c.model_id))
}

/// 把路由解析错误转成 HTTP 响应。
/// 模型未找到时按 spec 返回 404 + 可用模型列表；其余错误返回 404 + 错误信息。
pub async fn resolve_error_response(
    state: &LlmState,
    err: ResolveError,
) -> axum::response::Response {
    use super::upstream::{error_response, model_not_found_response};

    match err {
        ResolveError::ModelNotFound(_) => {
            let models = list_available_models(state).await.unwrap_or_default();
            let ids: Vec<String> = models
                .iter()
                .filter_map(|m| m["id"].as_str().map(String::from))
                .collect();
            model_not_found_response(err.to_string(), ids)
        }
        other => error_response(
            axum::http::StatusCode::NOT_FOUND,
            other.to_string(),
            "invalid_request_error",
        ),
    }
}

/// Get list of all enabled models (for /v1/models).
/// Only returns models whose provider is also enabled.
pub async fn list_available_models(state: &LlmState) -> Result<Vec<serde_json::Value>, String> {
    let db = state.db.as_ref().ok_or("database not available")?;

    let models = db
        .llm_list_models()
        .await
        .map_err(|e| format!("database error: {}", e))?;

    let providers = db
        .llm_list_providers()
        .await
        .map_err(|e| format!("database error: {}", e))?;

    let enabled_providers: std::collections::HashSet<String> = providers
        .iter()
        .filter(|p| p.enabled != 0)
        .map(|p| p.id.clone())
        .collect();

    let provider_map: std::collections::HashMap<String, String> = providers
        .iter()
        .map(|p| (p.id.clone(), p.name.clone()))
        .collect();

    let result: Vec<serde_json::Value> = models
        .into_iter()
        .filter(|m| m.enabled != 0 && enabled_providers.contains(&m.provider_id))
        .map(|m| {
            serde_json::json!({
                "id": if m.alias.is_empty() { &m.model_name } else { &m.alias },
                "object": "model",
                "created": 0,
                "owned_by": provider_map
                    .get(&m.provider_id)
                    .cloned()
                    .unwrap_or_else(|| "unknown".to_string()),
            })
        })
        .collect();

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::LlmState;

    #[test]
    fn test_resolve_without_db_returns_error() {
        let state = LlmState::new(None, None);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(resolve_model(&state, "any-model"));
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("database not available"));
    }

    #[tokio::test]
    async fn test_resolve_decrypts_provider_api_key() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = crate::db::Database::new(tmp.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();

        let cipher = crate::llm::crypto::LlmCipher::from_master_key([7u8; 32]);
        let encrypted = cipher.encrypt("sk-real-upstream-key");

        let pid = uuid::Uuid::new_v4().to_string();
        db.llm_save_provider(
            &pid,
            "DS",
            "deepseek",
            "https://api.deepseek.com",
            &encrypted,
            None::<&str>,
            None::<&str>,
            true,
        )
        .await
        .unwrap();
        let mid = uuid::Uuid::new_v4().to_string();
        db.llm_save_model(&mid, &pid, "deepseek-chat", "", "[]", true, None)
            .await
            .unwrap();

        // DB 里必须是密文
        let stored = db.llm_get_provider(&pid).await.unwrap().unwrap();
        assert!(stored.api_key.starts_with("enc:v1:"));

        let state = LlmState::new(Some(db), Some(cipher));
        let (provider, model, model_id) = resolve_model(&state, "deepseek-chat").await.unwrap();
        assert_eq!(model, "deepseek-chat");
        assert_eq!(model_id, mid);
        assert_eq!(
            provider.api_key, "sk-real-upstream-key",
            "resolve 应解密出明文 key"
        );
    }

    #[tokio::test]
    async fn test_resolve_legacy_plaintext_key_passthrough() {
        // 历史明文（无 enc 前缀）应原样可用
        let tmp = tempfile::TempDir::new().unwrap();
        let db = crate::db::Database::new(tmp.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        let pid = uuid::Uuid::new_v4().to_string();
        db.llm_save_provider(
            &pid,
            "DS",
            "deepseek",
            "https://api.deepseek.com",
            "sk-plain",
            None::<&str>,
            None::<&str>,
            true,
        )
        .await
        .unwrap();
        let mid = uuid::Uuid::new_v4().to_string();
        db.llm_save_model(&mid, &pid, "deepseek-chat", "", "[]", true, None)
            .await
            .unwrap();

        let state = LlmState::new(
            Some(db),
            Some(crate::llm::crypto::LlmCipher::from_master_key([9u8; 32])),
        );
        let (provider, _, _) = resolve_model(&state, "deepseek-chat").await.unwrap();
        assert_eq!(provider.api_key, "sk-plain");
    }

    #[tokio::test]
    async fn test_resolve_disabled_provider_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = crate::db::Database::new(tmp.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        let pid = uuid::Uuid::new_v4().to_string();
        db.llm_save_provider(
            &pid,
            "DS",
            "deepseek",
            "https://api.deepseek.com",
            "k",
            None::<&str>,
            None::<&str>,
            false,
        )
        .await
        .unwrap();
        let mid = uuid::Uuid::new_v4().to_string();
        db.llm_save_model(&mid, &pid, "deepseek-chat", "", "[]", true, None)
            .await
            .unwrap();

        let state = LlmState::new(Some(db), None);
        let err = resolve_model(&state, "deepseek-chat").await.unwrap_err();
        assert!(matches!(err, ResolveError::ProviderDisabled(_)));
    }

    #[tokio::test]
    async fn test_resolve_unknown_model_is_not_found() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = crate::db::Database::new(tmp.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        let state = LlmState::new(Some(db), None);
        let err = resolve_model(&state, "nope").await.unwrap_err();
        assert!(matches!(err, ResolveError::ModelNotFound(_)));
    }

    /// 测试辅助：建临时 DB + 一个 provider（p1, deepseek）+ 若干模型。
    async fn state_with_models(
        models: &[(&str, &str, &str)], // (model_id, model_name, alias)
    ) -> (tempfile::TempDir, LlmState) {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = crate::db::Database::new(tmp.path().join("t.db").to_str().unwrap())
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
        for (id, name, alias) in models {
            db.llm_save_model(id, "p1", name, alias, "[]", true, None)
                .await
                .unwrap();
        }
        let state = LlmState::new(Some(db), None);
        (tmp, state)
    }

    #[tokio::test]
    async fn test_resolve_single_model_chain() {
        let (_t, state) = state_with_models(&[("m1", "deepseek-chat", "")]).await;
        let chain = resolve_with_failover(&state, "deepseek-chat")
            .await
            .unwrap();
        assert!(chain.group_name.is_none());
        assert_eq!(chain.candidates.len(), 1);
        assert_eq!(chain.candidates[0].model_name, "deepseek-chat");
        assert_eq!(chain.candidates[0].model_id, "m1");
    }

    #[tokio::test]
    async fn test_resolve_group_chain_ordered() {
        let (_t, state) = state_with_models(&[
            ("m1", "model-a", ""),
            ("m2", "model-b", ""),
            ("m3", "model-c", ""),
        ])
        .await;
        let db = state.db.as_ref().unwrap();
        db.llm_create_model_group("g1", "router", true)
            .await
            .unwrap();
        db.llm_replace_group_members(
            "g1",
            &[("m3".into(), 3), ("m1".into(), 1), ("m2".into(), 2)],
        )
        .await
        .unwrap();

        let chain = resolve_with_failover(&state, "router").await.unwrap();
        assert_eq!(chain.group_name.as_deref(), Some("router"));
        let names: Vec<&str> = chain
            .candidates
            .iter()
            .map(|c| c.model_name.as_str())
            .collect();
        assert_eq!(names, ["model-a", "model-b", "model-c"]);
        let prios: Vec<i64> = chain.candidates.iter().map(|c| c.priority).collect();
        assert_eq!(prios, [1, 2, 3]);
    }

    #[tokio::test]
    async fn test_model_name_wins_over_group() {
        // 解析优先级：model_name 先于组名——同名时组被遮蔽。
        let (_t, state) = state_with_models(&[("m1", "router", "")]).await;
        let db = state.db.as_ref().unwrap();
        db.llm_create_model_group("g1", "router", true)
            .await
            .unwrap();

        let chain = resolve_with_failover(&state, "router").await.unwrap();
        assert!(chain.group_name.is_none(), "model_name 命中时组不生效");
        assert_eq!(chain.candidates[0].model_id, "m1");
    }

    #[tokio::test]
    async fn test_group_filters_disabled_member() {
        let (_t, state) = state_with_models(&[("m1", "model-a", ""), ("m2", "model-b", "")]).await;
        let db = state.db.as_ref().unwrap();
        db.llm_create_model_group("g1", "router", true)
            .await
            .unwrap();
        db.llm_replace_group_members("g1", &[("m1".into(), 1), ("m2".into(), 2)])
            .await
            .unwrap();
        // 禁用 m1
        db.llm_save_model("m1", "p1", "model-a", "", "[]", false, None)
            .await
            .unwrap();

        let chain = resolve_with_failover(&state, "router").await.unwrap();
        assert_eq!(chain.candidates.len(), 1);
        assert_eq!(chain.candidates[0].model_id, "m2");
    }

    #[tokio::test]
    async fn test_group_empty_after_filter_is_not_found() {
        let (_t, state) = state_with_models(&[("m1", "model-a", "")]).await;
        let db = state.db.as_ref().unwrap();
        db.llm_create_model_group("g1", "router", true)
            .await
            .unwrap();
        db.llm_replace_group_members("g1", &[("m1".into(), 1)])
            .await
            .unwrap();
        db.llm_save_model("m1", "p1", "model-a", "", "[]", false, None)
            .await
            .unwrap();

        let err = resolve_with_failover(&state, "router").await.unwrap_err();
        assert!(matches!(err, ResolveError::ModelNotFound(_)));
    }

    #[tokio::test]
    async fn test_group_disabled_is_not_found() {
        let (_t, state) = state_with_models(&[("m1", "model-a", "")]).await;
        let db = state.db.as_ref().unwrap();
        db.llm_create_model_group("g1", "router", false)
            .await
            .unwrap();
        db.llm_replace_group_members("g1", &[("m1".into(), 1)])
            .await
            .unwrap();

        let err = resolve_with_failover(&state, "router").await.unwrap_err();
        assert!(matches!(err, ResolveError::ModelNotFound(_)));
    }

    #[tokio::test]
    async fn test_group_member_provider_disabled_filtered() {
        // provider 被禁用时其下组成员被过滤
        let (_t, state) = state_with_models(&[("m1", "model-a", ""), ("m2", "model-b", "")]).await;
        let db = state.db.as_ref().unwrap();
        db.llm_save_provider(
            "p2",
            "Kimi",
            "kimi",
            "https://api.moonshot.cn",
            "k2",
            None::<&str>,
            None::<&str>,
            true,
        )
        .await
        .unwrap();
        db.llm_save_model("m3", "p2", "kimi-k2", "", "[]", true, None)
            .await
            .unwrap();
        db.llm_create_model_group("g1", "router", true)
            .await
            .unwrap();
        db.llm_replace_group_members("g1", &[("m1".into(), 1), ("m3".into(), 2)])
            .await
            .unwrap();
        // 禁用 p2
        db.llm_save_provider(
            "p2",
            "Kimi",
            "kimi",
            "https://api.moonshot.cn",
            "k2",
            None::<&str>,
            None::<&str>,
            false,
        )
        .await
        .unwrap();

        let chain = resolve_with_failover(&state, "router").await.unwrap();
        assert_eq!(chain.candidates.len(), 1);
        assert_eq!(chain.candidates[0].model_id, "m1");
    }
}
