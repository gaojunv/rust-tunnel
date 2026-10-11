//! 服务端密钥：Ed25519 签名与 `licence_key` 明文比对。
//!
//! ## 密钥在协议里的两个角色
//!
//! RustDesk 的 `--key` 同时承担两件事，两者强度完全不同，必须分清：
//!
//! 1. **设备认证 = 明文比对**。客户端在 `PunchHoleRequest::licence_key` /
//!    `RequestRelay::licence_key` 里带上这个字符串，服务端做 `==` 比较。这不是
//!    密码学认证，只是一个「共享口令」——所以它**必须走加密信道**（本项目的
//!    隧道），且 key 一旦泄露等同于服务被冒用。
//! 2. **ID ↔ 公钥绑定 = Ed25519 签名**。服务端用自己的私钥签名
//!    [`crate::proto::IdPk`]，客户端据此确认「这个 ID 确实对应这把公钥」，防止
//!    汇合/中继服务端把对端公钥换成自己的（中间人）。这才是真正的密码学保护。
//!
//! ## 与上游的字节布局兼容
//!
//! 上游用 libsodium 的 `crypto_sign`（NaCl Ed25519）。NaCl 私钥是 64 字节：
//! **前 32 字节 seed ‖ 后 32 字节公钥**，与 `ed25519-dalek` 的
//! `SigningKey::to_keypair_bytes()` 布局**逐字节相同**，签名也是标准 64 字节
//! Ed25519。因此可以直接互操作，且不必引入 libsodium 这个 C 依赖（musl 静态
//! 构建受益）。
//!
//! 上游约定：`--key` 若 base64 解出 64 字节，则视为完整私钥，对外展示的
//! 「公钥字符串」= base64(私钥后 32 字节)。本模块遵循同一约定，这样从上游
//! 迁移过来的 key 可以原样继续用。

use crate::proto::{IdPk, ProstMessage};
use base64::Engine as _;
use ed25519_dalek::{Signer as _, SigningKey, VerifyingKey};

/// NaCl/libsodium `crypto_sign` 私钥长度：seed(32) ‖ pubkey(32)。
const NACL_SECRET_KEY_BYTES: usize = 64;

/// Ed25519 seed 长度。
const SEED_BYTES: usize = 32;

/// 密钥解析错误。
#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    /// base64 解码失败。
    #[error("server key is not valid base64: {0}")]
    Base64(#[from] base64::DecodeError),
    /// base64 解出的长度既不是 32 也不是 64。
    #[error("server key decodes to {0} bytes; expected 32 (public key) or 64 (secret key)")]
    BadLength(usize),
    /// 64 字节私钥内部不自洽：后 32 字节不是前 32 字节 seed 推导出的公钥。
    #[error("server key's embedded public key does not match its seed")]
    InconsistentSecretKey,
}

/// 服务端密钥：一个可选的签名私钥 + 对外展示的密钥字符串。
///
/// 只持有字符串（未提供合法私钥）时，服务器仍可运行——此时只能做 `licence_key`
/// 明文比对，无法签名 [`IdPk`]，客户端在安全模式下会拒绝连接。这种模式适合
/// 只想验证协议连通性的场景。
#[derive(Clone)]
pub struct ServerKey {
    /// 对外展示、并用于 `licence_key` 明文比对的密钥字符串。
    key: String,
    /// 签名私钥；密钥为空或非法时为 `None`。
    signing: Option<SigningKey>,
}

impl std::fmt::Debug for ServerKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 私钥不进日志。
        f.debug_struct("ServerKey")
            .field("key", &self.key)
            .field("can_sign", &self.signing.is_some())
            .finish()
    }
}

impl ServerKey {
    /// 从配置里的密钥字符串构造。
    ///
    /// 接受五种形态（与上游 `get_server_sk` 语义保持一致）：
    /// - **空串**：`licence_key` 比对被跳过（不认证），且无签名能力——没有公开的
    ///   公钥，签名也没人验得了。仅供本地调试。
    /// - **`-` / `_`**：生成一把新私钥，对外 key = 该私钥的公钥字符串。
    /// - **base64 解出 64 字节**：视为完整 NaCl 私钥，校验自洽后启用签名。
    /// - **base64 解出 32 字节**：只有公钥，无签名能力。
    /// - **其它字符串**：当作纯共享口令，无签名能力。
    ///
    /// # Errors
    /// base64 可解但长度既非 32 也非 64，或 64 字节私钥内部不自洽时返回
    /// [`KeyError`]。
    pub fn parse(key: &str) -> Result<Self, KeyError> {
        if key.is_empty() {
            return Ok(Self {
                key: String::new(),
                signing: None,
            });
        }
        if key == "-" || key == "_" {
            let signing = Self::generate_signing_key();
            let public = base64_std(signing.verifying_key().as_bytes());
            return Ok(Self {
                key: public,
                signing: Some(signing),
            });
        }
        match base64::engine::general_purpose::STANDARD.decode(key) {
            Ok(raw) if raw.len() == NACL_SECRET_KEY_BYTES => {
                let mut seed = [0u8; SEED_BYTES];
                seed.copy_from_slice(&raw[..SEED_BYTES]);
                let signing = SigningKey::from_bytes(&seed);
                // 校验「后 32 字节 = 公钥」这一 NaCl 布局假设。不成立说明 key 被
                // 篡改或格式不符，此时用它签名会与客户端预期不符。
                if signing.verifying_key().as_bytes() != &raw[SEED_BYTES..] {
                    return Err(KeyError::InconsistentSecretKey);
                }
                Ok(Self {
                    key: base64_std(&raw[SEED_BYTES..]),
                    signing: Some(signing),
                })
            }
            // 32 字节 = 只有公钥，服务端拿不到私钥，无法签名。
            Ok(raw) if raw.len() == SEED_BYTES => Ok(Self {
                key: key.to_owned(),
                signing: None,
            }),
            Ok(raw) => Err(KeyError::BadLength(raw.len())),
            // 不是 base64：当作任意共享口令使用。
            Err(_) => Ok(Self {
                key: key.to_owned(),
                signing: None,
            }),
        }
    }

    /// 生成一把新的随机私钥。
    #[must_use]
    pub fn generate() -> Self {
        let signing = Self::generate_signing_key();
        let public = base64_std(signing.verifying_key().as_bytes());
        Self {
            key: public,
            signing: Some(signing),
        }
    }

    /// 用于 `licence_key` 明文比对、并展示给用户的密钥字符串。
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// 是否能签名 [`IdPk`]。
    #[must_use]
    pub fn can_sign(&self) -> bool {
        self.signing.is_some()
    }

    /// 私钥的 NaCl 布局序列化（seed ‖ pubkey，64 字节），供持久化使用。
    ///
    /// 返回 `None` 表示当前只有共享口令、没有私钥。
    #[must_use]
    pub fn to_nacl_secret(&self) -> Option<[u8; NACL_SECRET_KEY_BYTES]> {
        self.signing.as_ref().map(SigningKey::to_keypair_bytes)
    }

    /// 私钥的 base64 编码（NaCl 布局），可直接写回配置。
    #[must_use]
    pub fn to_secret_base64(&self) -> Option<String> {
        self.to_nacl_secret().map(|k| base64_std(&k))
    }

    /// 对 `ID ‖ 公钥` 绑定签名；无签名能力时返回 `None`。
    ///
    /// **签名的输入是 [`IdPk`] 的 protobuf 编码**，因此编码必须逐字节稳定：proto3
    /// 对空可选字段不产生字节，`id`、`pk` 两字段按 tag 升序排列，所以同一对
    /// `(id, pk)` 永远得到同一串字节，签名可被客户端复现校验。
    #[must_use]
    pub fn sign_id_pk(&self, id: &str, pk: &[u8]) -> Option<Vec<u8>> {
        let signing = self.signing.as_ref()?;
        let payload = IdPk {
            id: id.to_owned(),
            pk: pk.to_vec(),
            ..Default::default()
        }
        .encode_to_vec();
        Some(signing.sign(&payload).to_bytes().to_vec())
    }

    /// 用本密钥验证一个 `ID ‖ 公钥` 签名（自检与单测使用）。
    ///
    /// # Errors
    /// 无签名能力、签名长度非法或验签失败时返回 `Err`。
    pub fn verify_id_pk(&self, id: &str, pk: &[u8], signature: &[u8]) -> Result<(), String> {
        let signing = self
            .signing
            .as_ref()
            .ok_or_else(|| "no signing key available".to_owned())?;
        let payload = IdPk {
            id: id.to_owned(),
            pk: pk.to_vec(),
            ..Default::default()
        }
        .encode_to_vec();
        let sig_bytes: [u8; 64] = signature
            .try_into()
            .map_err(|_| format!("signature must be 64 bytes, got {}", signature.len()))?;
        signing
            .verifying_key()
            .verify_strict(&payload, &ed25519_dalek::Signature::from_bytes(&sig_bytes))
            .map_err(|e| e.to_string())
    }

    /// 对应私钥的公钥（无法签名时返回 `None`）。
    #[must_use]
    pub fn verifying_key(&self) -> Option<VerifyingKey> {
        self.signing.as_ref().map(SigningKey::verifying_key)
    }

    fn generate_signing_key() -> SigningKey {
        use rand::RngCore as _;
        let mut seed = [0u8; SEED_BYTES];
        rand::rngs::OsRng.fill_bytes(&mut seed);
        SigningKey::from_bytes(&seed)
    }
}

fn base64_std(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_key_can_sign_and_verify() {
        let key = ServerKey::generate();
        assert!(key.can_sign());
        let sig = key.sign_id_pk("123456", &[9, 9, 9]).expect("应能签名");
        assert_eq!(sig.len(), 64, "Ed25519 签名固定 64 字节");
        assert!(key.verify_id_pk("123456", &[9, 9, 9], &sig).is_ok());
    }

    #[test]
    fn signature_is_bound_to_id_and_pk() {
        let key = ServerKey::generate();
        let sig = key.sign_id_pk("123456", &[1, 2, 3]).unwrap();
        // 换 ID 或换公钥都必须验签失败，否则绑定就形同虚设。
        assert!(key.verify_id_pk("654321", &[1, 2, 3], &sig).is_err());
        assert!(key.verify_id_pk("123456", &[3, 2, 1], &sig).is_err());
    }

    #[test]
    fn nacl_secret_layout_roundtrips_through_parse() {
        // 这是与上游互操作的关键：导出的 64 字节私钥必须能被重新解析，
        // 且公钥字符串保持不变。
        let original = ServerKey::generate();
        let secret_b64 = original.to_secret_base64().expect("应有私钥");
        let reparsed = ServerKey::parse(&secret_b64).expect("应能解析");

        assert_eq!(reparsed.key(), original.key(), "公钥字符串应一致");
        assert!(reparsed.can_sign());

        // 两把私钥对同一输入产生同一签名（Ed25519 确定性签名）。
        assert_eq!(
            original.sign_id_pk("abc", &[7]).unwrap(),
            reparsed.sign_id_pk("abc", &[7]).unwrap()
        );
    }

    #[test]
    fn key_string_is_base64_of_last_32_bytes() {
        let key = ServerKey::generate();
        let raw = key.to_nacl_secret().unwrap();
        assert_eq!(raw.len(), 64);
        let expected = base64_std(&raw[32..]);
        assert_eq!(key.key(), expected);
        // 后 32 字节确实等于公钥。
        assert_eq!(
            key.verifying_key().unwrap().as_bytes(),
            &raw[32..],
            "NaCl 私钥后 32 字节应为公钥"
        );
    }

    #[test]
    fn inconsistent_secret_key_is_rejected() {
        // 构造一个「后 32 字节不是公钥」的 64 字节串。
        let mut bad = [0u8; 64];
        bad[0] = 1; // seed
        bad[32..].fill(0xAB); // 与 seed 推导出的公钥必然不符
        let b64 = base64_std(&bad);
        assert!(matches!(
            ServerKey::parse(&b64),
            Err(KeyError::InconsistentSecretKey)
        ));
    }

    #[test]
    fn public_key_only_disables_signing() {
        let key = ServerKey::generate();
        let pub_b64 = base64_std(key.verifying_key().unwrap().as_bytes());
        let parsed = ServerKey::parse(&pub_b64).expect("32 字节应可解析");
        assert!(!parsed.can_sign(), "只给公钥不应能签名");
        assert_eq!(parsed.key(), pub_b64);
        assert!(parsed.sign_id_pk("x", &[]).is_none());
    }

    #[test]
    fn arbitrary_shared_secret_is_accepted_as_opaque() {
        // 非 base64 的普通口令：可用作 licence_key 比对，但无签名能力。
        let parsed = ServerKey::parse("my-shared-passphrase").expect("应接受任意口令");
        assert_eq!(parsed.key(), "my-shared-passphrase");
        assert!(!parsed.can_sign());
    }

    #[test]
    fn empty_key_disables_auth_and_signing() {
        // 空 key 是「不认证」的退化配置，而不是「生成一把新密钥」。
        let parsed = ServerKey::parse("").expect("空 key 应可解析");
        assert_eq!(parsed.key(), "");
        assert!(!parsed.can_sign());
    }

    #[test]
    fn dash_and_underscore_keys_generate_a_keypair() {
        for input in ["-", "_"] {
            let parsed = ServerKey::parse(input).expect("应生成新密钥");
            assert!(parsed.can_sign(), "input={input:?} 应生成可签名密钥");
            assert!(!parsed.key().is_empty(), "input={input:?} 公钥串不应为空");
        }
    }

    #[test]
    fn bad_length_is_reported() {
        let b64 = base64_std(&[0u8; 16]);
        assert!(matches!(ServerKey::parse(&b64), Err(KeyError::BadLength(16))));
    }
}
