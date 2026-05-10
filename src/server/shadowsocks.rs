use std::sync::Arc;
use tokio::net::TcpStream;
use tracing::debug;
use crate::common::{TunnelError, TunnelResult};

// Re-export shadowsocks types
pub use shadowsocks::{
    config::{ServerConfig, ServerType},
    context::Context,
    crypto::CipherKind,
    relay::{
        socks5::Address,
        tcprelay::{
            proxy_stream::server::ProxyServerStream,
            utils::{copy_bidirectional, copy_encrypted_bidirectional, copy_from_encrypted, copy_to_encrypted},
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
        _ => Err(TunnelError::Protocol(format!("Unsupported cipher: {}", cipher))),
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
            m.update(&digest);
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
    debug!("Starting SS handshake for connection {}, port {}", connection_id, port);

    let kind = parse_cipher_kind(cipher)?;

    // Use ServerConfig to derive the key correctly to match shadowsocks-rust expectations
    let dummy_addr = std::net::SocketAddr::from(([0, 0, 0, 0], port as u16));
    let svr_cfg = ServerConfig::new(dummy_addr, password, kind)
        .map_err(|e| TunnelError::Protocol(format!("Failed to create server config: {}", e)))?;
    let key = svr_cfg.key().to_vec();

    // Create proxy server stream that handles encryption/decryption
    let mut proxy_stream = ProxyServerStream::from_stream(
        ss_context,
        stream,
        kind,
        &key,
    );

    // Perform handshake and get target address
    let target_addr = proxy_stream.handshake().await
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
