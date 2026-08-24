use axum::http::{header, HeaderMap};

use crate::LlmState;

/// 从请求头提取网关 API Key：
/// 优先 `Authorization: Bearer <key>`（OpenAI 风格），
/// 其次 `x-api-key: <key>`（Anthropic 原生风格）。
pub fn extract_api_token(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(t) = v.strip_prefix("Bearer ") {
            let t = t.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

/// 校验请求头中的网关 API Key（两种认证头都支持）。
/// Returns Some((key_id, key_name)) if valid, None if invalid/missing.
pub async fn authenticate(state: &LlmState, headers: &HeaderMap) -> Option<(String, String)> {
    let token = extract_api_token(headers)?;
    validate_api_key(state, &token).await
}

/// Validate a bare gateway API key token (e.g. "sk-...").
/// Returns Some((key_id, key_name)) if valid, None if invalid.
pub async fn validate_api_key(state: &LlmState, token: &str) -> Option<(String, String)> {
    if token.is_empty() {
        return None;
    }

    let db = state.db.as_ref()?;

    // Hash the provided token with SHA-256 to match stored hash.
    let hash = sha256_hex(token);

    let record = db.llm_find_api_key_by_hash(&hash).await.ok()??;

    if record.enabled == 0 {
        return None;
    }

    // Touch last_used_at (fire-and-forget)
    let id = record.id.clone();
    let db_clone = state.db.clone();
    tokio::spawn(async move {
        if let Some(db) = db_clone {
            let _ = db.llm_touch_api_key(&id).await;
        }
    });

    Some((record.id, record.name))
}

fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Generate a new API key in sk- format.
/// Returns (full_key, hash, prefix).
pub fn generate_api_key() -> (String, String, String) {
    let random_bytes: [u8; 24] = rand::random();
    let key = format!("sk-{}", hex::encode(random_bytes));
    let hash = sha256_hex(&key);
    let prefix = format!("{}...{}", &key[..8], &key[key.len() - 4..]);
    (key, hash, prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_api_key_format() {
        let (key, hash, prefix) = generate_api_key();

        // Key format: sk- + 48 hex chars = 51 chars
        assert!(key.starts_with("sk-"));
        assert_eq!(key.len(), 51);

        // Hash is 64 hex chars (SHA-256)
        assert_eq!(hash.len(), 64);

        // Prefix format: sk-xxxx...xxxx (first 8 chars ... last 4 chars)
        assert!(prefix.starts_with("sk-"));
        assert!(prefix.contains("..."));
        assert_eq!(prefix.len(), 15); // sk-xxxxx...xxxx
    }

    #[test]
    fn test_generated_key_validates() {
        let (key, hash, _prefix) = generate_api_key();
        let computed = sha256_hex(&key);
        assert_eq!(computed, hash);
    }

    #[test]
    fn test_different_keys_have_different_hashes() {
        let (key1, hash1, _) = generate_api_key();
        let (key2, hash2, _) = generate_api_key();
        assert_ne!(key1, key2);
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_extract_api_token_bearer() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer sk-abc".parse().unwrap());
        assert_eq!(extract_api_token(&headers).as_deref(), Some("sk-abc"));
    }

    #[test]
    fn test_extract_api_token_x_api_key() {
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", "sk-xyz".parse().unwrap());
        assert_eq!(extract_api_token(&headers).as_deref(), Some("sk-xyz"));
    }

    #[test]
    fn test_extract_api_token_bearer_preferred_over_x_api_key() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer sk-abc".parse().unwrap());
        headers.insert("x-api-key", "sk-xyz".parse().unwrap());
        assert_eq!(extract_api_token(&headers).as_deref(), Some("sk-abc"));
    }

    #[test]
    fn test_extract_api_token_missing_or_empty() {
        assert_eq!(extract_api_token(&HeaderMap::new()), None);

        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer ".parse().unwrap());
        assert_eq!(extract_api_token(&headers), None);
    }
}
