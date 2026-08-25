use rust_tunnel_common::{TunnelError, TunnelResult};
use std::sync::Arc;
use std::time::Instant;
use tokio::net::TcpStream;
use tracing::debug;
use tracing::{error, warn};

use crate::port_registry::PortRegistry;
use rust_tunnel_stats::{EntityType, StatsCollector};

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
#[must_use]
pub fn create_shared_context() -> SharedContext {
    Context::new_shared(ServerType::Server)
}

/// Create cipher kind from config string
pub fn parse_cipher_kind(cipher: &str) -> TunnelResult<CipherKind> {
    match cipher {
        "aes-256-gcm" => Ok(CipherKind::AES_256_GCM),
        "chacha20-ietf-poly1305" => Ok(CipherKind::CHACHA20_POLY1305),
        _ => Err(TunnelError::Protocol(format!(
            "Unsupported cipher: {cipher}"
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
        .map_err(|e| TunnelError::with_source("Failed to create server config", e))?;
    let key = svr_cfg.key().to_vec();

    // Create proxy server stream that handles encryption/decryption
    let mut proxy_stream = ProxyServerStream::from_stream(ss_context, stream, kind, &key);

    // Perform handshake and get target address
    let target_addr = proxy_stream
        .handshake()
        .await
        .map_err(|e| TunnelError::with_source("Handshake failed", e))?;

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

/// Bidirectional copy with Shadowsocks encryption/decryption using ProxyServerStream.
///
/// Uses `copy_encrypted_bidirectional` which properly handles TCP shutdown: when one
/// direction observes EOF, it calls `shutdown()` on the opposing writer so the other
/// direction unblocks instead of hanging indefinitely.
async fn copy_bidirectional_with_ss_crypto(
    mut proxy_stream: ProxyServerStream<TcpStream>,
    mut target_stream: TcpStream,
) -> TunnelResult<(u64, u64)> {
    use crate::shadowsocks::{copy_encrypted_bidirectional, CipherKind};

    // encrypted_to_plain: client -> target (upload), plain_to_encrypted: target -> client (download)
    let (encrypted_to_plain, plain_to_encrypted) = copy_encrypted_bidirectional(
        CipherKind::AES_256_GCM,
        &mut proxy_stream,
        &mut target_stream,
    )
    .await?;

    Ok((encrypted_to_plain, plain_to_encrypted))
}

/// Proxy a Shadowsocks connection to target
pub async fn proxy_ss_connection(
    connection_id: u64,
    ss_port: u16,
    proxy_stream: ProxyServerStream<TcpStream>,
    ss_ctx: SSConnectionContext,
    registry: std::sync::Arc<dyn PortRegistry>,
    stats: StatsCollector,
) {
    debug!(
        "Starting SS proxy for connection {}, target {}",
        connection_id, ss_ctx.target_addr
    );

    // Increment active SS connection count
    registry.increment_ss_connections(ss_port).await;
    // 统一统计：shadowsocks 桶活跃连接 +1（entity_id 约定为 ss:{port}）
    let entity_id = format!("ss:{ss_port}");
    stats.incr_conns(EntityType::Shadowsocks, &entity_id);

    // Record start time for measuring connection setup time (RTT estimate)
    let start = Instant::now();

    // Connect to target server
    let target_stream = match TcpStream::connect(&ss_ctx.target_addr).await {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to connect to target {}: {}", ss_ctx.target_addr, e);
            registry.decrement_ss_connections(ss_port).await;
            // 统一统计：活跃连接 -1（覆盖目标连接失败的错误退出路径）
            stats.decr_conns(EntityType::Shadowsocks, &entity_id);
            return;
        }
    };

    // Calculate connection establishment time as RTT estimate
    let connect_time_ms = start.elapsed().as_millis() as u64;
    debug!(
        "Connected to target {} for SS connection {} in {}ms",
        ss_ctx.target_addr, connection_id, connect_time_ms
    );

    let proxy_start = Instant::now();
    let result = copy_bidirectional_with_ss_crypto(proxy_stream, target_stream).await;

    // Decrement active SS connection count (always run)
    registry.decrement_ss_connections(ss_port).await;
    // 统一统计：活跃连接 -1（覆盖正常与错误退出）
    stats.decr_conns(EntityType::Shadowsocks, &entity_id);

    match result {
        Ok((uploaded, downloaded)) => {
            let elapsed_secs = proxy_start.elapsed().as_secs_f64();

            debug!(
                "SS connection {} completed: uploaded {} bytes, downloaded {} bytes in {:.2}s",
                connection_id, uploaded, downloaded, elapsed_secs
            );

            // 统一统计：双向字节一次性入账（bytes_in = 客户端->目标，bytes_out = 目标->客户端）
            stats.record_bytes(EntityType::Shadowsocks, &entity_id, uploaded, downloaded);
        }
        Err(e) => {
            warn!("SS connection {} error: {}", connection_id, e);
        }
    }
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
        let debug_str = format!("{ctx:?}");
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

// ---------------------------------------------------------------------------
// Tests formerly in shadowsocks_test.rs, merged here as an inner module
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    //! Unit and integration tests for Shadowsocks handshake and data relay.
    //!
    //! Integration tests require `ss-local` from shadowsocks-libev to be installed.

    use std::net::SocketAddr;
    use std::process::{Child, Command, Stdio};
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    use crate::listener;
    use crate::port_registry::MockPortRegistry;
    use rust_tunnel_stats::StatsCollector;
    use std::sync::Arc;

    // ---------------------------------------------------------------------------
    // helpers
    // ---------------------------------------------------------------------------

    /// Bind to port 0 to get a random available port, then return it.
    async fn find_available_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    }

    /// Start a TCP echo server on a random port. Returns the port and a handle
    /// that aborts the server on drop (just abort the JoinHandle).
    async fn start_echo_server() -> (u16, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = tokio::spawn(async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(c) => c,
                    Err(_) => break,
                };
                tokio::spawn(async move {
                    let (mut reader, mut writer) = tokio::io::split(stream);
                    let mut buf = [0u8; 8192];
                    loop {
                        match reader.read(&mut buf).await {
                            Ok(0) => break,
                            Ok(n) => {
                                if writer.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                });
            }
        });

        (port, handle)
    }

    /// Spawn `ss-local` as a child process.
    fn start_ss_local(
        ss_server_port: u16,
        socks5_port: u16,
        cipher: &str,
        password: &str,
    ) -> Child {
        Command::new("ss-local")
            .args([
                "-s",
                "127.0.0.1",
                "-p",
                &ss_server_port.to_string(),
                "-l",
                &socks5_port.to_string(),
                "-k",
                password,
                "-m",
                cipher,
            ])
            .stderr(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .expect("Failed to start ss-local - is shadowsocks-libev installed?")
    }

    /// Retry connecting to `port` until success or `dur` elapses.
    async fn wait_for_port(port: u16, dur: Duration) {
        let start = tokio::time::Instant::now();
        loop {
            if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
                return;
            }
            assert!(start.elapsed() < dur, "Timed out waiting for port {port}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Perform a minimal SOCKS5 CONNECT handshake through `proxy_port`, then
    /// send `data` to `target_addr` and return the echoed response.
    async fn socks5_send_recv(proxy_port: u16, target_addr: SocketAddr, data: &[u8]) -> Vec<u8> {
        let mut stream = TcpStream::connect(("127.0.0.1", proxy_port))
            .await
            .expect("Failed to connect to SOCKS5 proxy");

        // ---- Step 1: method negotiation ----
        // Client -> Proxy: [VER=0x05, NMETHODS=1, METHOD=0x00 (no-auth)]
        stream.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        // Proxy -> Client: [VER=0x05, METHOD=0x00]
        let mut buf = [0u8; 2];
        stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf, [0x05, 0x00], "SOCKS5 auth method mismatch");

        // ---- Step 2: CONNECT request ----
        let ip = match target_addr.ip() {
            std::net::IpAddr::V4(v4) => v4.octets().to_vec(),
            std::net::IpAddr::V6(_v6) => panic!("IPv6 not used in tests"),
        };
        let port = target_addr.port();
        let connect_req = [
            &[0x05, 0x01, 0x00, 0x01][..], // VER, CMD=CONNECT, RSV, ATYP=IPv4
            &ip,
            &[(port >> 8) as u8, (port & 0xff) as u8],
        ]
        .concat();
        stream.write_all(&connect_req).await.unwrap();

        // Read SOCKS5 reply: VER, REP, RSV, ATYP, BND.ADDR (4 or 16), BND.PORT (2)
        let mut reply = [0u8; 4]; // fixed header
        stream.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[0], 0x05, "SOCKS5 VER mismatch");
        assert_eq!(reply[1], 0x00, "SOCKS5 REP error: {:02x}", reply[1]);

        let bind_addr_len = match reply[3] {
            0x01 => 4,  // IPv4
            0x04 => 16, // IPv6
            other => panic!("Unexpected ATYP: {other}"),
        };
        let mut bind_rest = vec![0u8; bind_addr_len + 2];
        stream.read_exact(&mut bind_rest).await.unwrap();
        // bind_rest = [BND.ADDR..., BND.PORT(2)]

        // ---- Step 3: send data and read exact-length echo ----
        stream.write_all(data).await.unwrap();

        // Read back exactly the same number of bytes we sent.
        // Avoid half-close (shutdown) because ss-local may tear down the SS tunnel
        // before the response fully propagates.
        let expected_len = data.len();
        let mut response = vec![0u8; expected_len];
        stream.read_exact(&mut response).await.unwrap();

        // Now close cleanly
        stream.shutdown().await.ok();

        response
    }

    // ---------------------------------------------------------------------------
    // unit tests
    // ---------------------------------------------------------------------------

    #[cfg(test)]
    mod unit_tests {
        use crate::shadowsocks::{derive_key, parse_cipher_kind};

        #[tokio::test]
        async fn test_key_derivation_consistency() {
            let password = "testpassword";
            let cipher = "aes-256-gcm";

            let key = derive_key(password, cipher).unwrap();
            assert_eq!(key.len(), 32);

            let kind = parse_cipher_kind(cipher).unwrap();
            let server_addr: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
            let cfg =
                ::shadowsocks::config::ServerConfig::new(server_addr, password, kind).unwrap();
            assert_eq!(key, cfg.key());
        }
    }

    // ---------------------------------------------------------------------------
    // integration tests — require ss-local
    // ---------------------------------------------------------------------------

    #[cfg(test)]
    mod integration_tests {
        use super::*;

        #[tokio::test]
        #[ignore] // Requires ss-local from shadowsocks-libev
        async fn test_ss_echo_aes256gcm_via_ss_local() {
            // 1. echo server
            let (echo_port, echo_handle) = start_echo_server().await;

            // 2. SS listener
            let registry: Arc<dyn crate::port_registry::PortRegistry> =
                Arc::new(MockPortRegistry::new());
            let stats = StatsCollector::new(None);
            let ss_port = find_available_port().await;
            let reg_c = registry.clone();
            let stats_c = stats.clone();
            tokio::spawn(async move {
                let _ = listener::start_shadowsocks_listener(
                    reg_c,
                    stats_c,
                    ss_port,
                    "aes-256-gcm".into(),
                    "testpass".into(),
                )
                .await;
            });

            // give the SS listener a moment to bind
            wait_for_port(ss_port, Duration::from_secs(3)).await;

            // 3. ss-local
            let socks5_port = find_available_port().await;
            let mut ss_local = start_ss_local(ss_port, socks5_port, "aes-256-gcm", "testpass");
            wait_for_port(socks5_port, Duration::from_secs(5)).await;

            // 4. SOCKS5 -> echo server
            let echo_addr: SocketAddr = ([127, 0, 0, 1], echo_port).into();
            let echoed = socks5_send_recv(socks5_port, echo_addr, b"hello shadowsocks").await;
            assert_eq!(echoed, b"hello shadowsocks");

            // 5. cleanup
            ss_local.kill().ok();
            ss_local.wait().ok();
            echo_handle.abort();
        }

        #[tokio::test]
        #[ignore] // Requires ss-local from shadowsocks-libev
        async fn test_ss_chacha20_poly1305_via_ss_local() {
            let (echo_port, echo_handle) = start_echo_server().await;

            let registry: Arc<dyn crate::port_registry::PortRegistry> =
                Arc::new(MockPortRegistry::new());
            let stats = StatsCollector::new(None);
            let ss_port = find_available_port().await;
            let reg_c = registry.clone();
            let stats_c = stats.clone();
            tokio::spawn(async move {
                let _ = listener::start_shadowsocks_listener(
                    reg_c,
                    stats_c,
                    ss_port,
                    "chacha20-ietf-poly1305".into(),
                    "testpass".into(),
                )
                .await;
            });
            wait_for_port(ss_port, Duration::from_secs(3)).await;

            let socks5_port = find_available_port().await;
            let mut ss_local =
                start_ss_local(ss_port, socks5_port, "chacha20-ietf-poly1305", "testpass");
            wait_for_port(socks5_port, Duration::from_secs(5)).await;

            let echo_addr: SocketAddr = ([127, 0, 0, 1], echo_port).into();
            let echoed = socks5_send_recv(socks5_port, echo_addr, b"chacha20 test").await;
            assert_eq!(echoed, b"chacha20 test");

            ss_local.kill().ok();
            ss_local.wait().ok();
            echo_handle.abort();
        }

        #[tokio::test]
        #[ignore] // Requires ss-local from shadowsocks-libev
        async fn test_ss_large_data_transfer() {
            let (echo_port, echo_handle) = start_echo_server().await;

            let registry: Arc<dyn crate::port_registry::PortRegistry> =
                Arc::new(MockPortRegistry::new());
            let stats = StatsCollector::new(None);
            let ss_port = find_available_port().await;
            let reg_c = registry.clone();
            let stats_c = stats.clone();
            tokio::spawn(async move {
                let _ = listener::start_shadowsocks_listener(
                    reg_c,
                    stats_c,
                    ss_port,
                    "aes-256-gcm".into(),
                    "testpass".into(),
                )
                .await;
            });
            wait_for_port(ss_port, Duration::from_secs(3)).await;

            let socks5_port = find_available_port().await;
            let mut ss_local = start_ss_local(ss_port, socks5_port, "aes-256-gcm", "testpass");
            wait_for_port(socks5_port, Duration::from_secs(5)).await;

            // 64 KB payload
            let payload: Vec<u8> = (0u8..=255).cycle().take(65536).collect();
            let echo_addr: SocketAddr = ([127, 0, 0, 1], echo_port).into();
            let echoed = socks5_send_recv(socks5_port, echo_addr, &payload).await;
            assert_eq!(echoed, payload);

            ss_local.kill().ok();
            ss_local.wait().ok();
            echo_handle.abort();
        }

        #[tokio::test]
        #[ignore] // Requires ss-local from shadowsocks-libev
        async fn test_ss_active_connection_count() {
            let (echo_port, echo_handle) = start_echo_server().await;

            let registry: Arc<dyn crate::port_registry::PortRegistry> =
                Arc::new(MockPortRegistry::new());
            let stats = StatsCollector::new(None);
            let ss_port = find_available_port().await;
            let reg_c = registry.clone();
            let stats_c = stats.clone();
            tokio::spawn(async move {
                let _ = listener::start_shadowsocks_listener(
                    reg_c,
                    stats_c,
                    ss_port,
                    "aes-256-gcm".into(),
                    "testpass".into(),
                )
                .await;
            });
            wait_for_port(ss_port, Duration::from_secs(3)).await;

            // No active SS connections yet
            assert_eq!(
                registry.get_connection_count_for_port(ss_port).await,
                0,
                "expected 0 SS connections before any connection"
            );

            let socks5_port = find_available_port().await;
            let mut ss_local = start_ss_local(ss_port, socks5_port, "aes-256-gcm", "testpass");
            wait_for_port(socks5_port, Duration::from_secs(5)).await;

            // Open a connection through the SS tunnel
            let echo_addr: SocketAddr = ([127, 0, 0, 1], echo_port).into();
            let echoed = socks5_send_recv(socks5_port, echo_addr, b"connection count test").await;
            assert_eq!(echoed, b"connection count test");

            // After communicating, the SS connection counter should have been
            // incremented (the SS proxy calls increment_ss_connections on start).
            let count = registry.get_connection_count_for_port(ss_port).await;
            assert!(
                count >= 1,
                "SS connection count should be >= 1 after data transfer, got {count}"
            );

            ss_local.kill().ok();
            ss_local.wait().ok();
            echo_handle.abort();
        }
    }
}
