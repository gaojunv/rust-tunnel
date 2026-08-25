//! 提供商 API Key 的对称加密存储（AES-256-GCM）。
//!
//! spec 要求：提供商 API Key 与火山方舟 AK/SK 入库前加密，密钥不写入 DB。
//! 主密钥持久化在 DB 同目录的 `llm_master.key`（0600 权限），与 TLS 自签名
//! 证书存放在 `./data/` 的做法一致。密文格式：
//!
//! ```text
//! enc:v1:<base64(nonce ‖ ciphertext‖tag)>
//! ```
//!
//! 无前缀的值按历史明文兼容读取（解密时原样返回），便于滚动升级。

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;

/// 密文前缀：版本化的加密标记
pub const ENC_PREFIX: &str = "enc:v1:";

/// AES-256-GCM 字段加密器。
#[derive(Clone)]
pub struct LlmCipher {
    cipher: Aes256Gcm,
}

impl std::fmt::Debug for LlmCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmCipher").finish_non_exhaustive()
    }
}

impl LlmCipher {
    /// 使用 32 字节主密钥构造加密器。
    #[must_use]
    pub fn from_master_key(key: [u8; 32]) -> Self {
        Self {
            cipher: Aes256Gcm::new((&key).into()),
        }
    }

    /// 加密一个字段，返回带 `enc:v1:` 前缀的密文。
    ///
    /// # Panics
    /// AES-256-GCM encrypt 仅在 nonce 长度非法时失败；nonce 为固定 `[u8; 12]`，
    /// 实际不可达，保持 panic 语义。
    #[must_use]
    pub fn encrypt(&self, plaintext: &str) -> String {
        let nonce_bytes: [u8; 12] = rand::random();
        #[expect(clippy::panic)]
        let ciphertext = self
            .cipher
            .encrypt(Nonce::from_slice(&nonce_bytes), plaintext.as_bytes())
            .unwrap_or_else(|e| panic!("AES-256-GCM encrypt failed: {e}"));

        let mut blob = Vec::with_capacity(12 + ciphertext.len());
        blob.extend_from_slice(&nonce_bytes);
        blob.extend_from_slice(&ciphertext);
        format!(
            "{}{}",
            ENC_PREFIX,
            base64::engine::general_purpose::STANDARD.encode(blob)
        )
    }

    /// 解密字段。无 `enc:v1:` 前缀的值视为历史明文，原样返回。
    pub fn decrypt(&self, stored: &str) -> Result<String, String> {
        let Some(encoded) = stored.strip_prefix(ENC_PREFIX) else {
            return Ok(stored.to_string());
        };
        let blob = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|e| format!("invalid ciphertext encoding: {e}"))?;
        if blob.len() < 13 {
            return Err("ciphertext too short".into());
        }
        let (nonce, ciphertext) = blob.split_at(12);
        let plaintext = self
            .cipher
            .decrypt(Nonce::from_slice(nonce), ciphertext)
            .map_err(|_| "decryption failed (wrong key or corrupted data)".to_string())?;
        String::from_utf8(plaintext).map_err(|e| format!("plaintext is not UTF-8: {e}"))
    }
}

/// 判断一个已存储的值是否为密文。
#[must_use]
pub fn is_encrypted(stored: &str) -> bool {
    stored.starts_with(ENC_PREFIX)
}

/// 加密可选字段：有 cipher 就加密，否则原样返回（并调用方应记日志）。
#[must_use]
pub fn encrypt_field(cipher: Option<&LlmCipher>, plaintext: &str) -> String {
    match cipher {
        Some(c) => c.encrypt(plaintext),
        None => plaintext.to_string(),
    }
}

/// 解密字段：无明文前缀时原样返回；有密文前缀但无 cipher 时报错。
pub fn decrypt_field(cipher: Option<&LlmCipher>, stored: &str) -> Result<String, String> {
    if !is_encrypted(stored) {
        return Ok(stored.to_string());
    }
    match cipher {
        Some(c) => c.decrypt(stored),
        None => Err("value is encrypted but no master key is configured".into()),
    }
}

/// 加载或生成主密钥：存放在 DB 同目录的 `llm_master.key`（0600）。
/// 密钥本身不写入 DB，符合 spec 的"密钥从服务端配置派生，不写入 DB"。
///
/// 该函数通过原子写-重读策略处理并发场景：当两个服务实例同时启动时，
/// 只有一个能成功创建密钥文件，另一个会重新读取并使用相同的密钥。
pub fn load_or_create_master_key(db_path: &str) -> std::io::Result<[u8; 32]> {
    let dir = std::path::Path::new(db_path)
        .parent()
        .unwrap_or(std::path::Path::new("."));
    std::fs::create_dir_all(dir)?;
    let key_path = dir.join("llm_master.key");

    // 先尝试读取已有密钥
    if let Ok(raw) = std::fs::read(&key_path) {
        if raw.len() == 32 {
            let mut key = [0u8; 32];
            key.copy_from_slice(&raw);
            return Ok(key);
        }
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "{}: corrupt master key (expected 32 bytes, got {})",
                key_path.display(),
                raw.len()
            ),
        ));
    }

    // 密钥文件不存在 -> 生成新密钥并写入
    let key: [u8; 32] = rand::random();
    match write_master_key(&key_path, &key) {
        Ok(()) => Ok(key),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // 并发场景：另一个实例先创建了文件 -> 重新读取
            let raw = std::fs::read(&key_path)?;
            if raw.len() == 32 {
                let mut k = [0u8; 32];
                k.copy_from_slice(&raw);
                return Ok(k);
            }
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "{}: corrupt master key after concurrent creation (expected 32 bytes, got {})",
                    key_path.display(),
                    raw.len()
                ),
            ))
        }
        Err(e) => Err(e),
    }
}

/// 以原子方式写入主密钥文件，确保：
/// - Unix: 使用 `create_new(true)` + `mode(0o600)`
/// - 其他平台: 使用 `create_new(true)` 写入
fn write_master_key(key_path: &std::path::Path, key: &[u8; 32]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(key_path)?;
        use std::io::Write;
        f.write_all(key)?;
    }
    #[cfg(not(unix))]
    {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(key_path)
            .and_then(|mut f| {
                use std::io::Write;
                f.write_all(key)?;
                Ok(())
            })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cipher() -> LlmCipher {
        LlmCipher::from_master_key([42u8; 32])
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let c = test_cipher();
        let ct = c.encrypt("sk-secret-key-123");
        assert!(is_encrypted(&ct));
        assert!(
            !ct.contains("sk-secret-key-123"),
            "ciphertext must not contain plaintext"
        );
        assert_eq!(c.decrypt(&ct).unwrap(), "sk-secret-key-123");
    }

    #[test]
    fn encrypt_is_nondeterministic() {
        let c = test_cipher();
        // 随机 nonce：同明文两次加密结果不同
        assert_ne!(c.encrypt("same"), c.encrypt("same"));
    }

    #[test]
    fn decrypt_legacy_plaintext_passthrough() {
        let c = test_cipher();
        assert_eq!(c.decrypt("sk-plaintext").unwrap(), "sk-plaintext");
    }

    #[test]
    fn decrypt_with_wrong_key_fails() {
        let c1 = LlmCipher::from_master_key([1u8; 32]);
        let c2 = LlmCipher::from_master_key([2u8; 32]);
        let ct = c1.encrypt("secret");
        assert!(c2.decrypt(&ct).is_err());
    }

    #[test]
    fn decrypt_tampered_ciphertext_fails() {
        let c = test_cipher();
        let ct = c.encrypt("secret");
        let mut blob = base64::engine::general_purpose::STANDARD
            .decode(ct.strip_prefix(ENC_PREFIX).unwrap())
            .unwrap();
        let last = blob.len() - 1;
        blob[last] ^= 0xff;
        let tampered = format!(
            "{}{}",
            ENC_PREFIX,
            base64::engine::general_purpose::STANDARD.encode(blob)
        );
        assert!(c.decrypt(&tampered).is_err());
    }

    #[test]
    fn encrypt_empty_string_roundtrip() {
        let c = test_cipher();
        let ct = c.encrypt("");
        assert_eq!(c.decrypt(&ct).unwrap(), "");
    }

    #[test]
    fn decrypt_field_without_cipher_handles_plaintext() {
        assert_eq!(decrypt_field(None, "sk-plain").unwrap(), "sk-plain");
        assert!(decrypt_field(None, "enc:v1:AAAA").is_err());
    }

    #[test]
    fn master_key_load_or_create() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        let k1 = load_or_create_master_key(db_path.to_str().unwrap()).unwrap();
        // 第二次加载得到同一密钥
        let k2 = load_or_create_master_key(db_path.to_str().unwrap()).unwrap();
        assert_eq!(k1, k2);

        // 文件权限为 0600（unix）
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = std::fs::metadata(tmp.path().join("llm_master.key")).unwrap();
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn master_key_corrupt_file_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        std::fs::write(tmp.path().join("llm_master.key"), b"short").unwrap();
        assert!(load_or_create_master_key(db_path.to_str().unwrap()).is_err());
    }

    #[test]
    fn master_key_concurrent_creation_recovers() {
        // 模拟并发场景：预先把密钥文件写入，然后调用 load_or_create_master_key
        // 应该正常读取已有密钥。这覆盖了"非第一个创建者"的路径。
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        let preexisting: [u8; 32] = [77u8; 32];

        // 手动写入一个有效密钥文件（模拟另一个进程先创建）
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(tmp.path().join("llm_master.key"))
                .unwrap();
            f.write_all(&preexisting).unwrap();
        }
        #[cfg(not(unix))]
        {
            std::fs::write(tmp.path().join("llm_master.key"), &preexisting).unwrap();
        }

        let key = load_or_create_master_key(db_path.to_str().unwrap()).unwrap();
        assert_eq!(key, preexisting, "should read the pre-existing key");
    }

    // ── encrypt_field / decrypt_field 边界测试 ──────────────────

    #[test]
    fn encrypt_field_without_cipher_returns_plaintext() {
        let result = encrypt_field(None, "sk-plain-key");
        assert_eq!(result, "sk-plain-key");
        assert!(!is_encrypted(&result));
    }

    #[test]
    fn encrypt_field_with_cipher_produces_encrypted() {
        let c = test_cipher();
        let result = encrypt_field(Some(&c), "my-secret");
        assert!(is_encrypted(&result));
        assert!(!result.contains("my-secret"));
    }

    #[test]
    fn encrypt_field_roundtrip_via_decrypt_field() {
        let c = test_cipher();
        let ct = encrypt_field(Some(&c), "roundtrip-test");
        let pt = decrypt_field(Some(&c), &ct).unwrap();
        assert_eq!(pt, "roundtrip-test");
    }

    #[test]
    fn decrypt_field_plaintext_passthrough() {
        assert_eq!(decrypt_field(None, "sk-plain").unwrap(), "sk-plain");
        let c = test_cipher();
        assert_eq!(decrypt_field(Some(&c), "sk-plain").unwrap(), "sk-plain");
    }

    #[test]
    fn decrypt_field_encrypted_without_cipher_errors() {
        let c = test_cipher();
        let ct = c.encrypt("secret");
        assert!(is_encrypted(&ct));
        let err = decrypt_field(None, &ct).unwrap_err();
        assert!(err.contains("no master key"));
    }

    #[test]
    fn decrypt_field_wrong_cipher_errors() {
        let c1 = LlmCipher::from_master_key([1u8; 32]);
        let c2 = LlmCipher::from_master_key([2u8; 32]);
        let ct = encrypt_field(Some(&c1), "secret");
        assert!(decrypt_field(Some(&c2), &ct).is_err());
    }
}
