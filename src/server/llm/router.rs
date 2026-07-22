use crate::server::llm::{LlmState, ProviderConfig};

/// Resolve a model name (or alias) to a ProviderConfig with the actual model_name.
/// Returns (provider, actual_model_name) or error string.
pub async fn resolve_model(
    state: &LlmState,
    model: &str,
) -> Result<(ProviderConfig, String), String> {
    let db = state.db.as_ref().ok_or("database not available")?;

    // Find the model by name or alias (exact match, name first, then alias)
    let model_record = db
        .llm_find_model_by_name_or_alias(model)
        .await
        .map_err(|e| format!("database error: {}", e))?
        .ok_or_else(|| format!("model '{}' not found", model))?;

    // Find its provider
    let provider_record = db
        .llm_get_provider(&model_record.provider_id)
        .await
        .map_err(|e| format!("database error: {}", e))?
        .ok_or_else(|| format!("provider for model '{}' not found", model))?;

    if provider_record.enabled == 0 {
        return Err(format!("provider for model '{}' is disabled", model));
    }

    let provider = ProviderConfig {
        id: provider_record.id,
        name: provider_record.name,
        provider_type: provider_record.provider_type,
        base_url: provider_record.base_url,
        api_key: provider_record.api_key,
        extra_config: provider_record.extra_config,
        enabled: provider_record.enabled != 0,
        created_at: provider_record.created_at,
        updated_at: provider_record.updated_at,
    };

    Ok((provider, model_record.model_name))
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
    use crate::server::llm::LlmState;

    #[test]
    fn test_resolve_without_db_returns_error() {
        let state = LlmState::new(None);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(resolve_model(&state, "any-model"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("database not available"));
    }
}
