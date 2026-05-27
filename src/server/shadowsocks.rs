use crate::common::{TunnelError, TunnelResult};
use std::sync::Arc;
use tokio::net::TcpStream;
use tracing::debug;

// Re-export shadowsocks types
pub use shadowsocks::{
    config::{ServerConfig, ServerType},
    context::Context,
    crypto::CipherKind,
    relay::{
        socks5::Address,
        tcprelay::{
            proxy_stream::server::ProxyServerStream,
            utils::{
                copy_bidirectional, copy_encrypted_bidirectional, copy_from_encrypted,
                copy_to_encrypted,
            },
        },
    },
};

/// Type alias for SharedContext
pub type SharedContext = Arc<Context>;

/// Shadowsocks connection context holding encryption state and target info
#[derive(Debug, Clone)]
pub struct SSConnectionContext {
    pub cipher_type: String,
    pub key: Vec<u8>,
    pub target_addr: String,
    pub connection_id: u64,
    pub port: u16,
}

/// Shared context for shadowsocks operations
pub fn create_shared_context() -> SharedContext {
    Context::new_shared(ServerType::Server)
}

/// Create cipher kind from config string
pub fn parse_cipher_kind(cipher: &str) -> TunnelResult<CipherKind> {
    match cipher {
        "aes-256-gcm" => Ok(CipherKind::AES_256_GCM),
        "chacha20-ietf-poly1305" => Ok(CipherKind::CHACHA20_POLY1305),
        _ => Err(TunnelError::Protocol(format!(
            "Unsupported cipher: {}",
            cipher
        ))),
    }
}

/// Derive encryption key from password using shadowsocks' EVP_BytesToKey
pub fn derive_key(password: &str, cipher: &str) -> TunnelResult<Vec<u8>> {
    let kind = parse_cipher_kind(cipher)?;
    let mut key = vec![0u8; kind.key_len()];
    openssl_bytes_to_key(password.as_bytes(), &mut key);
    Ok(key)
}

/// Key derivation of OpenSSL's EVP_BytesToKey - exactly matching shadowsocks implementation
fn openssl_bytes_to_key(password: &[u8], key: &mut [u8]) {
    use md5::{Digest, Md5};

    let key_len = key.len();
    let mut last_digest = None;
    let mut offset = 0usize;

    while offset < key_len {
        let mut m = Md5::new();
        if let Some(digest) = last_digest {
            m.update(digest);
        }
        m.update(password);
        let digest = m.finalize();

        let amt = std::cmp::min(key_len - offset, digest.len());
        key[offset..offset + amt].copy_from_slice(&digest[..amt]);

        offset += amt;
        last_digest = Some(digest);
    }
}

/// Handle SS handshake using shadowsocks-rust ProxyServerStream
pub async fn handle_ss_handshake(
    stream: TcpStream,
    cipher: &str,
    password: &str,
    connection_id: u64,
    port: u16,
    ss_context: Arc<Context>,
) -> TunnelResult<(SSConnectionContext, ProxyServerStream<TcpStream>)> {
    debug!(
        "Starting SS handshake for connection {}, port {}",
        connection_id, port
    );

    let kind = parse_cipher_kind(cipher)?;

    // Use ServerConfig to derive the key correctly to match shadowsocks-rust expectations
    let dummy_addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    let svr_cfg = ServerConfig::new(dummy_addr, password, kind)
        .map_err(|e| TunnelError::Protocol(format!("Failed to create server config: {}", e)))?;
    let key = svr_cfg.key().to_vec();

    // Create proxy server stream that handles encryption/decryption
    let mut proxy_stream = ProxyServerStream::from_stream(ss_context, stream, kind, &key);

    // Perform handshake and get target address
    let target_addr = proxy_stream
        .handshake()
        .await
        .map_err(|e| TunnelError::Protocol(format!("Handshake failed: {}", e)))?;

    let target_addr_str = target_addr.to_string();
    debug!("Parsed SS target address: {}", target_addr_str);

    let ctx = SSConnectionContext {
        cipher_type: cipher.to_string(),
        key,
        target_addr: target_addr_str,
        connection_id,
        port,
    };

    Ok((ctx, proxy_stream))
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn test_parse_cipher_kind_aes256gcm() {
        let result = parse_cipher_kind("aes-256-gcm");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), CipherKind::AES_256_GCM);
    }

    #[test]
    fn test_parse_cipher_kind_chacha20() {
        let result = parse_cipher_kind("chacha20-ietf-poly1305");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), CipherKind::CHACHA20_POLY1305);
    }

    #[test]
    fn test_parse_cipher_kind_unsupported() {
        let result = parse_cipher_kind("aes-128-cfb");
        assert!(result.is_err());
        match result.unwrap_err() {
            TunnelError::Protocol(msg) => assert!(msg.contains("Unsupported cipher")),
            _ => panic!("Expected Protocol error"),
        }
    }

    #[test]
    fn test_parse_cipher_kind_empty() {
        let result = parse_cipher_kind("");
        assert!(result.is_err());
    }

    #[test]
    fn test_derive_key_aes256gcm() {
        let result = derive_key("testpassword", "aes-256-gcm");
        assert!(result.is_ok());
        let key = result.unwrap();
        // AES-256-GCM uses a 32-byte key
        assert_eq!(key.len(), 32);
    }

    #[test]
    fn test_derive_key_chacha20() {
        let result = derive_key("testpassword", "chacha20-ietf-poly1305");
        assert!(result.is_ok());
        let key = result.unwrap();
        // ChaCha20-Poly1305 uses a 32-byte key
        assert_eq!(key.len(), 32);
    }

    #[test]
    fn test_derive_key_deterministic() {
        let key1 = derive_key("samepassword", "aes-256-gcm").unwrap();
        let key2 = derive_key("samepassword", "aes-256-gcm").unwrap();
        assert_eq!(key1, key2, "Same password should produce same key");
    }

    #[test]
    fn test_derive_key_different_passwords() {
        let key1 = derive_key("password1", "aes-256-gcm").unwrap();
        let key2 = derive_key("password2", "aes-256-gcm").unwrap();
        assert_ne!(
            key1, key2,
            "Different passwords should produce different keys"
        );
    }

    #[test]
    fn test_derive_key_unsupported_cipher() {
        let result = derive_key("testpassword", "invalid-cipher");
        assert!(result.is_err());
    }

    #[test]
    fn test_ss_connection_context_debug() {
        let ctx = SSConnectionContext {
            cipher_type: "aes-256-gcm".to_string(),
            key: vec![0u8; 32],
            target_addr: "127.0.0.1:80".to_string(),
            connection_id: 42,
            port: 8388,
        };
        // Should implement Debug
        let debug_str = format!("{:?}", ctx);
        assert!(debug_str.contains("aes-256-gcm"));
        assert!(debug_str.contains("127.0.0.1:80"));
    }

    #[test]
    fn test_ss_connection_context_clone() {
        let ctx = SSConnectionContext {
            cipher_type: "aes-256-gcm".to_string(),
            key: vec![1, 2, 3, 4],
            target_addr: "127.0.0.1:80".to_string(),
            connection_id: 42,
            port: 8388,
        };
        let cloned = ctx.clone();
        assert_eq!(ctx.cipher_type, cloned.cipher_type);
        assert_eq!(ctx.key, cloned.key);
        assert_eq!(ctx.target_addr, cloned.target_addr);
        assert_eq!(ctx.connection_id, cloned.connection_id);
        assert_eq!(ctx.port, cloned.port);
    }

    #[test]
    fn test_create_shared_context() {
        let ctx = create_shared_context();
        // Should not panic
        assert!(Arc::strong_count(&ctx) >= 1);
    }

    #[test]
    fn test_derive_key_empty_password() {
        let result = derive_key("", "aes-256-gcm");
        assert!(result.is_ok());
        let key = result.unwrap();
        assert_eq!(key.len(), 32);
    }
}
