use crate::server::llm::LlmState;

/// Validate a gateway API key from the Authorization header.
/// Returns Some((key_id, key_name)) if valid, None if invalid/missing.
pub async fn validate_api_key(state: &LlmState, auth_header: Option<&str>) -> Option<(String, String)> {
    let header = auth_header?;
    let token = header.strip_prefix("Bearer ")?.trim();

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
}
