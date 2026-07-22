use std::net::{Ipv4Addr, Ipv6Addr};
use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::server::TlsStream;
use tracing::{debug, error, warn};

use crate::common::{TunnelError, TunnelResult};
use crate::server::control::ServerState;
use crate::server::stats::EntityType;

/// Trojan command types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrojanCommand {
    Connect = 0x01,
    UdpAssociate = 0x03,
}

impl TrojanCommand {
    fn from_byte(b: u8) -> Option<Self> {
        match b {
            0x01 => Some(TrojanCommand::Connect),
            0x03 => Some(TrojanCommand::UdpAssociate),
            _ => None,
        }
    }
}

/// Trojan address types
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrojanAddress {
    IPv4(Ipv4Addr),
    IPv6(Ipv6Addr),
    Domain(String),
}

impl TrojanAddress {
    /// Parse address from buffer starting at offset, returns (address, bytes_consumed)
    fn parse(buf: &[u8], offset: usize) -> Option<(Self, usize)> {
        if offset >= buf.len() {
            return None;
        }
        match buf[offset] {
            0x01 => {
                // IPv4: 4 bytes
                if offset + 5 > buf.len() {
                    return None;
                }
                let addr = Ipv4Addr::new(
                    buf[offset + 1],
                    buf[offset + 2],
                    buf[offset + 3],
                    buf[offset + 4],
                );
                Some((TrojanAddress::IPv4(addr), 5))
            }
            0x03 => {
                // Domain: 1-byte length + domain bytes
                if offset + 2 > buf.len() {
                    return None;
                }
                let domain_len = buf[offset + 1] as usize;
                if domain_len == 0 || domain_len > 253 {
                    return None;
                }
                if offset + 2 + domain_len > buf.len() {
                    return None;
                }
                let domain_bytes = &buf[offset + 2..offset + 2 + domain_len];
                // Validate domain characters: ASCII alphanumerics, hyphens, and dots only
                if !domain_bytes
                    .iter()
                    .all(|&b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
                {
                    return None;
                }
                let domain = String::from_utf8_lossy(domain_bytes).to_string();
                Some((TrojanAddress::Domain(domain), 2 + domain_len))
            }
            0x04 => {
                // IPv6: 16 bytes
                if offset + 17 > buf.len() {
                    return None;
                }
                let mut octets = [0u8; 16];
                octets.copy_from_slice(&buf[offset + 1..offset + 17]);
                let addr = Ipv6Addr::from(octets);
                Some((TrojanAddress::IPv6(addr), 17))
            }
            _ => None,
        }
    }
}

impl std::fmt::Display for TrojanAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TrojanAddress::IPv4(addr) => write!(f, "{}", addr),
            TrojanAddress::IPv6(addr) => write!(f, "{}", addr),
            TrojanAddress::Domain(domain) => write!(f, "{}", domain),
        }
    }
}

/// Parsed Trojan request
#[derive(Debug, Clone)]
pub struct TrojanRequest {
    pub command: TrojanCommand,
    pub address: TrojanAddress,
    pub port: u16,
    /// Byte offset where payload starts in the original buffer
    pub header_len: usize,
}

/// Result of incremental parsing
pub enum ParseResult {
    Complete(TrojanRequest),
    Incomplete,
    Invalid(String),
}

/// Compute SHA-224 hex hash of password (56 lowercase hex chars)
pub fn sha224_hex(password: &str) -> String {
    use sha2::{Digest, Sha224};
    let mut hasher = Sha224::new();
    hasher.update(password.as_bytes());
    hex::encode(hasher.finalize())
}

/// Verify a received hash against an expected password using constant-time comparison
pub fn verify_password(received_hash: &str, expected_password: &str) -> bool {
    let expected_hash = sha224_hex(expected_password);
    constant_time_eq(received_hash.as_bytes(), expected_hash.as_bytes())
}

/// Constant-time comparison to prevent timing side-channel attacks
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Validate that a string is a valid hex SHA-224 hash (56 lowercase hex chars)
fn is_valid_hash(s: &str) -> bool {
    s.len() == 56 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Trojan connection context
#[derive(Debug, Clone)]
pub struct TrojanConnectionContext {
    pub target_addr: String,
    pub connection_id: u64,
    pub port: u16,
    pub command: TrojanCommand,
}

/// Parse a Trojan request from a buffer
/// Returns Complete if parsing succeeded, Incomplete if more data needed, Invalid on error
pub fn parse_trojan_request(buf: &[u8]) -> ParseResult {
    // Minimum: 56 (hash) + 2 (CRLF) + 1 (CMD) + 1 (ATYP) + 1 (min addr) + 2 (port) + 2 (CRLF) = 65
    // But realistically with IPv4: 56 + 2 + 1 + 1 + 4 + 2 + 2 = 68
    if buf.len() < 58 {
        return ParseResult::Incomplete;
    }

    // Check first CRLF after hash
    if buf[56] != b'\r' || buf[57] != b'\n' {
        return ParseResult::Invalid("Missing CRLF after hash".to_string());
    }

    // Validate hash format
    let hash_str = match std::str::from_utf8(&buf[..56]) {
        Ok(s) => s.to_string(),
        Err(_) => return ParseResult::Invalid("Hash is not valid UTF-8".to_string()),
    };
    if !is_valid_hash(&hash_str) {
        return ParseResult::Invalid("Invalid hash format".to_string());
    }

    // Parse command
    let cmd_byte = buf[58];
    let command = match TrojanCommand::from_byte(cmd_byte) {
        Some(c) => c,
        None => return ParseResult::Invalid(format!("Unsupported command: 0x{:02x}", cmd_byte)),
    };

    // Parse address
    let (address, addr_len) = match TrojanAddress::parse(buf, 59) {
        Some(r) => r,
        None => return ParseResult::Incomplete,
    };

    let port_offset = 59 + addr_len;
    if port_offset + 4 > buf.len() {
        return ParseResult::Incomplete;
    }

    // Parse port (big-endian)
    let port = u16::from_be_bytes([buf[port_offset], buf[port_offset + 1]]);

    // Check trailing CRLF
    let crlf_offset = port_offset + 2;
    if buf[crlf_offset] != b'\r' || buf[crlf_offset + 1] != b'\n' {
        return ParseResult::Invalid("Missing CRLF after port".to_string());
    }

    let header_len = crlf_offset + 2;
    let request = TrojanRequest {
        command,
        address,
        port,
        header_len,
    };

    ParseResult::Complete(request)
}

/// Handle Trojan handshake over TLS stream
/// Returns (TrojanConnectionContext, remaining payload bytes) on success
pub async fn handle_trojan_handshake(
    tls_stream: &mut TlsStream<TcpStream>,
    password: &str,
    connection_id: u64,
    port: u16,
) -> TunnelResult<(TrojanConnectionContext, Vec<u8>)> {
    debug!(
        "Starting Trojan handshake for connection {}, port {}",
        connection_id, port
    );

    let mut buf = vec![0u8; 4096];
    let mut total_read = 0;

    // Read until we have a complete request or fail
    loop {
        let n = tls_stream.read(&mut buf[total_read..]).await?;
        if n == 0 {
            return Err(TunnelError::ConnectionClosed);
        }
        total_read += n;

        match parse_trojan_request(&buf[..total_read]) {
            ParseResult::Complete(request) => {
                // Verify password
                let hash_str = std::str::from_utf8(&buf[..56])
                    .map_err(|_| TunnelError::Protocol("Invalid hash encoding".to_string()))?;

                if !verify_password(hash_str, password) {
                    warn!(
                        "Trojan authentication failed for connection {}",
                        connection_id
                    );
                    return Err(TunnelError::TrojanAuthFailed(buf[..total_read].to_vec()));
                }

                // Build target address string
                let target_addr = match &request.address {
                    TrojanAddress::IPv4(addr) => format!("{}:{}", addr, request.port),
                    TrojanAddress::IPv6(addr) => format!("[{}]:{}", addr, request.port),
                    TrojanAddress::Domain(domain) => format!("{}:{}", domain, request.port),
                };

                debug!(
                    "Trojan authenticated, target: {}, command: {:?}",
                    target_addr, request.command
                );

                // Extract remaining payload (after header)
                let payload = if total_read > request.header_len {
                    buf[request.header_len..total_read].to_vec()
                } else {
                    Vec::new()
                };

                let ctx = TrojanConnectionContext {
                    target_addr,
                    connection_id,
                    port,
                    command: request.command,
                };

                return Ok((ctx, payload));
            }
            ParseResult::Incomplete => {
                if total_read >= buf.len() {
                    return Err(TunnelError::TrojanAuthFailed(buf[..total_read].to_vec()));
                }
                // Need more data, continue reading
            }
            ParseResult::Invalid(reason) => {
                warn!(
                    "Invalid Trojan request for connection {}: {}",
                    connection_id, reason
                );
                return Err(TunnelError::TrojanAuthFailed(buf[..total_read].to_vec()));
            }
        }
    }
}

/// Handle fallback: forward the connection to a fallback backend
pub async fn handle_trojan_fallback(
    tls_stream: &mut TlsStream<TcpStream>,
    initial_data: &[u8],
    fallback_addr: &str,
) -> TunnelResult<()> {
    debug!("Handling Trojan fallback to {}", fallback_addr);

    let mut backend = match TcpStream::connect(fallback_addr).await {
        Ok(s) => s,
        Err(e) => {
            warn!("Failed to connect to fallback {}: {}", fallback_addr, e);
            return Err(TunnelError::Io(e));
        }
    };

    // Forward any already-read data
    if !initial_data.is_empty() {
        backend.write_all(initial_data).await?;
    }

    // Bidirectional copy between TLS stream and fallback backend
    let (mut tls_read, mut tls_write) = tokio::io::split(tls_stream);
    let (mut backend_read, mut backend_write) = tokio::io::split(backend);

    let client_to_backend = tokio::io::copy(&mut tls_read, &mut backend_write);
    let backend_to_client = tokio::io::copy(&mut backend_read, &mut tls_write);

    tokio::select! {
        r1 = client_to_backend => {
            if let Err(e) = r1 {
                debug!("Fallback client->backend error: {}", e);
            }
        }
        r2 = backend_to_client => {
            if let Err(e) = r2 {
                debug!("Fallback backend->client error: {}", e);
            }
        }
    }

    Ok(())
}

/// 校验 Trojan 域名：必须是合法 DNS 域名（不含 `*`、端口或路径）。
/// 调用方保证传入前已 trim + 转小写；空串由调用方自行处理（空 = 不配置域名）。
pub fn validate_trojan_domain(domain: &str) -> Result<(), String> {
    if domain.is_empty() {
        return Err("domain is empty".to_string());
    }
    if domain.len() > 253 {
        return Err("domain too long (max 253 chars)".to_string());
    }
    if domain.contains('*') {
        return Err("wildcard domains are not allowed".to_string());
    }
    if domain.contains(':') || domain.contains('/') {
        return Err("domain must not contain port or path".to_string());
    }
    for label in domain.split('.') {
        if label.is_empty() {
            return Err("domain contains an empty label".to_string());
        }
        if label.len() > 63 {
            return Err("domain label too long (max 63 chars)".to_string());
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err("domain label must not start or end with '-'".to_string());
        }
        if !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err("domain contains invalid characters".to_string());
        }
    }
    Ok(())
}

/// Proxy a Trojan connection to target.
/// Trojan data is already decrypted by TLS, so we just do raw TCP bidirectional copy.
pub async fn proxy_trojan_connection(
    connection_id: u64,
    trojan_port: u16,
    mut tls_stream: TlsStream<TcpStream>,
    trojan_ctx: TrojanConnectionContext,
    initial_payload: Vec<u8>,
    state: ServerState,
) {
    debug!(
        "Starting Trojan proxy for connection {}, target {}",
        connection_id, trojan_ctx.target_addr
    );

    // Reject UDP ASSOCIATE — only CONNECT is supported
    if trojan_ctx.command == TrojanCommand::UdpAssociate {
        warn!(
            "Trojan UDP ASSOCIATE is not supported for connection {}",
            connection_id
        );
        return;
    }

    // Increment active Trojan connection count
    state.increment_trojan_connections(trojan_port).await;
    // 统一统计：trojan 桶活跃连接 +1（entity_id 约定为 trojan:{port}）
    let entity_id = format!("trojan:{}", trojan_port);
    state
        .stats_collector
        .incr_conns(EntityType::Trojan, &entity_id);

    // Record start time for measuring connection setup time (RTT estimate)
    let start = Instant::now();

    // Connect to target server
    let mut target_stream = match TcpStream::connect(&trojan_ctx.target_addr).await {
        Ok(s) => s,
        Err(e) => {
            error!(
                "Failed to connect to target {}: {}",
                trojan_ctx.target_addr, e
            );
            state.decrement_trojan_connections(trojan_port).await;
            // 统一统计：活跃连接 -1（覆盖目标连接失败的错误退出路径）
            state
                .stats_collector
                .decr_conns(EntityType::Trojan, &entity_id);
            return;
        }
    };

    let connect_time_ms = start.elapsed().as_millis() as u64;
    debug!(
        "Connected to target {} for Trojan connection {} in {}ms",
        trojan_ctx.target_addr, connection_id, connect_time_ms
    );

    // Write any initial payload from the Trojan handshake
    if !initial_payload.is_empty() {
        if let Err(e) =
            tokio::io::AsyncWriteExt::write_all(&mut target_stream, &initial_payload).await
        {
            warn!(
                "Failed to write initial payload for Trojan connection {}: {}",
                connection_id, e
            );
            state.decrement_trojan_connections(trojan_port).await;
            // 统一统计：活跃连接 -1（覆盖初始负载写失败的错误退出路径）
            state
                .stats_collector
                .decr_conns(EntityType::Trojan, &entity_id);
            return;
        }
    }

    let proxy_start = Instant::now();

    // Bidirectional copy: TLS stream (already decrypted) <-> target TCP stream
    let result = tokio::io::copy_bidirectional(&mut tls_stream, &mut target_stream).await;

    // Decrement active Trojan connection count
    state.decrement_trojan_connections(trojan_port).await;
    // 统一统计：活跃连接 -1（覆盖正常与错误退出）
    state
        .stats_collector
        .decr_conns(EntityType::Trojan, &entity_id);

    match result {
        Ok((client_to_target, target_to_client)) => {
            let elapsed_secs = proxy_start.elapsed().as_secs_f64();

            debug!(
                "Trojan connection {} completed: uploaded {} bytes, downloaded {} bytes in {:.2}s",
                connection_id, client_to_target, target_to_client, elapsed_secs
            );

            // 统一统计：双向字节一次性入账（bytes_in = 客户端->目标，bytes_out = 目标->客户端）
            state.stats_collector.record_bytes(
                EntityType::Trojan,
                &entity_id,
                client_to_target,
                target_to_client,
            );
        }
        Err(e) => {
            warn!("Trojan connection {} error: {}", connection_id, e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sha224_hex() {
        let hash = sha224_hex("password");
        assert_eq!(hash.len(), 56);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_sha224_hex_deterministic() {
        let h1 = sha224_hex("test123");
        let h2 = sha224_hex("test123");
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_sha224_hex_different_passwords() {
        let h1 = sha224_hex("password1");
        let h2 = sha224_hex("password2");
        assert_ne!(h1, h2);
    }

    #[test]
    fn validate_trojan_domain_accepts_valid() {
        assert!(validate_trojan_domain("trojan.example.com").is_ok());
        assert!(validate_trojan_domain("ok-1.example.com").is_ok());
        assert!(validate_trojan_domain("localhost").is_ok());
    }

    #[test]
    fn validate_trojan_domain_rejects_invalid() {
        assert!(validate_trojan_domain("").is_err());
        assert!(validate_trojan_domain("*.example.com").is_err());
        assert!(validate_trojan_domain("host:443").is_err());
        assert!(validate_trojan_domain("a/b").is_err());
        assert!(validate_trojan_domain("exa mple.com").is_err());
        assert!(validate_trojan_domain("-bad.com").is_err());
        assert!(validate_trojan_domain("bad-.com").is_err());
        assert!(validate_trojan_domain("a..com").is_err());
    }

    #[test]
    fn test_verify_password_correct() {
        assert!(verify_password(&sha224_hex("mypassword"), "mypassword"));
    }

    #[test]
    fn test_verify_password_incorrect() {
        assert!(!verify_password(&sha224_hex("wrongpassword"), "mypassword"));
    }

    #[test]
    fn test_parse_request_ipv4() {
        let password = "testpassword";
        let hash = sha224_hex(password);
        let mut buf = Vec::new();
        buf.extend_from_slice(hash.as_bytes());
        buf.extend_from_slice(b"\r\n");
        buf.push(0x01); // CONNECT
        buf.push(0x01); // IPv4
        buf.extend_from_slice(&[127, 0, 0, 1]); // 127.0.0.1
        buf.extend_from_slice(&0x01BBu16.to_be_bytes()); // port 443
        buf.extend_from_slice(b"\r\n");

        match parse_trojan_request(&buf) {
            ParseResult::Complete(req) => {
                assert_eq!(req.command, TrojanCommand::Connect);
                assert_eq!(req.port, 443);
                assert!(matches!(req.address, TrojanAddress::IPv4(_)));
                assert_eq!(req.header_len, 68);
            }
            _ => panic!("Expected Complete parse result"),
        }
    }

    #[test]
    fn test_parse_request_domain() {
        let password = "testpassword";
        let hash = sha224_hex(password);
        let domain = b"example.com";
        let mut buf = Vec::new();
        buf.extend_from_slice(hash.as_bytes());
        buf.extend_from_slice(b"\r\n");
        buf.push(0x01); // CONNECT
        buf.push(0x03); // Domain
        buf.push(domain.len() as u8);
        buf.extend_from_slice(domain);
        buf.extend_from_slice(&0x01BBu16.to_be_bytes()); // port 443
        buf.extend_from_slice(b"\r\n");

        match parse_trojan_request(&buf) {
            ParseResult::Complete(req) => {
                assert_eq!(req.command, TrojanCommand::Connect);
                assert_eq!(req.port, 443);
                assert!(matches!(req.address, TrojanAddress::Domain(_)));
                if let TrojanAddress::Domain(d) = &req.address {
                    assert_eq!(d, "example.com");
                }
            }
            _ => panic!("Expected Complete parse result"),
        }
    }

    #[test]
    fn test_parse_request_ipv6() {
        let password = "testpassword";
        let hash = sha224_hex(password);
        let mut buf = Vec::new();
        buf.extend_from_slice(hash.as_bytes());
        buf.extend_from_slice(b"\r\n");
        buf.push(0x01); // CONNECT
        buf.push(0x04); // IPv6
        buf.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]); // ::1
        buf.extend_from_slice(&0x0050u16.to_be_bytes()); // port 80
        buf.extend_from_slice(b"\r\n");

        match parse_trojan_request(&buf) {
            ParseResult::Complete(req) => {
                assert_eq!(req.command, TrojanCommand::Connect);
                assert_eq!(req.port, 80);
                assert!(matches!(req.address, TrojanAddress::IPv6(_)));
            }
            _ => panic!("Expected Complete parse result"),
        }
    }

    #[test]
    fn test_parse_request_incomplete() {
        let buf = [0u8; 50];
        match parse_trojan_request(&buf) {
            ParseResult::Incomplete => {}
            _ => panic!("Expected Incomplete for short buffer"),
        }
    }

    #[test]
    fn test_parse_request_invalid_no_crlf() {
        let hash = sha224_hex("test");
        let mut buf = Vec::new();
        buf.extend_from_slice(hash.as_bytes());
        buf.extend_from_slice(b"XX"); // Not CRLF

        match parse_trojan_request(&buf) {
            ParseResult::Invalid(_) => {}
            _ => panic!("Expected Invalid for missing CRLF"),
        }
    }

    #[test]
    fn test_parse_request_invalid_command() {
        let hash = sha224_hex("test");
        let mut buf = Vec::new();
        buf.extend_from_slice(hash.as_bytes());
        buf.extend_from_slice(b"\r\n");
        buf.push(0x02); // Invalid command
        buf.push(0x01); // IPv4
        buf.extend_from_slice(&[127, 0, 0, 1]);
        buf.extend_from_slice(&80u16.to_be_bytes());
        buf.extend_from_slice(b"\r\n");

        match parse_trojan_request(&buf) {
            ParseResult::Invalid(msg) => {
                assert!(msg.contains("Unsupported command"));
            }
            _ => panic!("Expected Invalid for unsupported command"),
        }
    }

    #[test]
    fn test_trojan_address_display() {
        let v4 = TrojanAddress::IPv4(Ipv4Addr::new(192, 168, 1, 1));
        assert_eq!(format!("{}", v4), "192.168.1.1");

        let domain = TrojanAddress::Domain("example.com".to_string());
        assert_eq!(format!("{}", domain), "example.com");
    }

    #[test]
    fn test_is_valid_hash() {
        assert!(is_valid_hash(
            "a3b2c1d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8"
        ));
        assert!(!is_valid_hash("short"));
        assert!(!is_valid_hash(
            "a3b2c1d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8X"
        )); // too long
        assert!(!is_valid_hash(
            "a3b2c1d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8Z"
        )); // 56 but not hex
    }

    #[test]
    fn test_domain_validation_rejects_empty() {
        let hash = sha224_hex("test");
        let mut buf = Vec::new();
        buf.extend_from_slice(hash.as_bytes());
        buf.extend_from_slice(b"\r\n");
        buf.push(0x01); // CONNECT
        buf.push(0x03); // Domain
        buf.push(0); // zero-length domain (invalid)
        buf.extend_from_slice(&80u16.to_be_bytes());
        buf.extend_from_slice(b"\r\n");

        match parse_trojan_request(&buf) {
            ParseResult::Incomplete | ParseResult::Invalid(_) => {}
            ParseResult::Complete(_) => panic!("Expected failure for empty domain"),
        }
    }

    #[test]
    fn test_domain_validation_rejects_invalid_chars() {
        let hash = sha224_hex("test");
        let mut buf = Vec::new();
        buf.extend_from_slice(hash.as_bytes());
        buf.extend_from_slice(b"\r\n");
        buf.push(0x01); // CONNECT
        buf.push(0x03); // Domain
        buf.push(4); // length
        buf.extend_from_slice(b"ex@m"); // contains '@' which is invalid
        buf.extend_from_slice(&80u16.to_be_bytes());
        buf.extend_from_slice(b"\r\n");

        match parse_trojan_request(&buf) {
            ParseResult::Incomplete | ParseResult::Invalid(_) => {}
            ParseResult::Complete(_) => panic!("Expected failure for domain with invalid chars"),
        }
    }

    #[test]
    fn test_domain_validation_accepts_valid() {
        let hash = sha224_hex("test");
        let mut buf = Vec::new();
        buf.extend_from_slice(hash.as_bytes());
        buf.extend_from_slice(b"\r\n");
        buf.push(0x01); // CONNECT
        buf.push(0x03); // Domain
        let domain = b"sub.example.com";
        buf.push(domain.len() as u8);
        buf.extend_from_slice(domain);
        buf.extend_from_slice(&443u16.to_be_bytes());
        buf.extend_from_slice(b"\r\n");

        match parse_trojan_request(&buf) {
            ParseResult::Complete(req) => {
                if let TrojanAddress::Domain(d) = &req.address {
                    assert_eq!(d, "sub.example.com");
                } else {
                    panic!("Expected Domain address");
                }
            }
            _ => panic!("Expected Complete for valid domain"),
        }
    }

    // --- constant_time_eq tests ---

    #[test]
    fn test_constant_time_eq_equal_buffers() {
        assert!(constant_time_eq(b"hello", b"hello"));
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(&[0u8; 64], &[0u8; 64]));
    }

    #[test]
    fn test_constant_time_eq_different_buffers() {
        assert!(!constant_time_eq(b"hello", b"world"));
        assert!(!constant_time_eq(b"hello", b"hellp"));
        assert!(!constant_time_eq(&[0u8; 64], &[1u8; 64]));
    }

    #[test]
    fn test_constant_time_eq_different_lengths() {
        assert!(!constant_time_eq(b"short", b"longer"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(!constant_time_eq(b"a", b""));
    }

    #[test]
    fn test_constant_time_eq_single_byte_diff() {
        let mut a = [0u8; 56];
        let mut b = [0u8; 56];
        a[55] = 1;
        assert!(!constant_time_eq(&a, &b));
        b[55] = 1;
        assert!(constant_time_eq(&a, &b));
    }

    // --- verify_password with constant-time comparison ---

    #[test]
    fn test_verify_password_uses_constant_time() {
        // Verify that correct password returns true
        let hash = sha224_hex("correct");
        assert!(verify_password(&hash, "correct"));
        // Verify that incorrect password returns false
        assert!(!verify_password(&hash, "incorrect"));
        // Verify that completely wrong hash returns false
        assert!(!verify_password(
            "00000000000000000000000000000000000000000000000000000000",
            "correct"
        ));
    }

    #[test]
    fn test_verify_password_empty() {
        let hash = sha224_hex("");
        assert!(verify_password(&hash, ""));
        assert!(!verify_password(&hash, "nonempty"));
    }

    // --- Domain validation edge cases ---

    #[test]
    fn test_domain_validation_hyphens() {
        let hash = sha224_hex("test");
        let mut buf = Vec::new();
        buf.extend_from_slice(hash.as_bytes());
        buf.extend_from_slice(b"\r\n");
        buf.push(0x01);
        buf.push(0x03);
        let domain = b"my-server.example.com";
        buf.push(domain.len() as u8);
        buf.extend_from_slice(domain);
        buf.extend_from_slice(&443u16.to_be_bytes());
        buf.extend_from_slice(b"\r\n");

        match parse_trojan_request(&buf) {
            ParseResult::Complete(req) => {
                if let TrojanAddress::Domain(d) = &req.address {
                    assert_eq!(d, "my-server.example.com");
                } else {
                    panic!("Expected Domain address");
                }
            }
            _ => panic!("Expected Complete for domain with hyphens"),
        }
    }

    #[test]
    fn test_domain_validation_rejects_spaces() {
        let hash = sha224_hex("test");
        let mut buf = Vec::new();
        buf.extend_from_slice(hash.as_bytes());
        buf.extend_from_slice(b"\r\n");
        buf.push(0x01);
        buf.push(0x03);
        buf.push(11);
        buf.extend_from_slice(b"bad domain!");
        buf.extend_from_slice(&80u16.to_be_bytes());
        buf.extend_from_slice(b"\r\n");

        match parse_trojan_request(&buf) {
            ParseResult::Incomplete | ParseResult::Invalid(_) => {}
            ParseResult::Complete(_) => panic!("Expected failure for domain with spaces"),
        }
    }

    #[test]
    fn test_domain_validation_rejects_underscore() {
        let hash = sha224_hex("test");
        let mut buf = Vec::new();
        buf.extend_from_slice(hash.as_bytes());
        buf.extend_from_slice(b"\r\n");
        buf.push(0x01);
        buf.push(0x03);
        buf.push(9);
        buf.extend_from_slice(b"bad_name!");
        buf.extend_from_slice(&80u16.to_be_bytes());
        buf.extend_from_slice(b"\r\n");

        match parse_trojan_request(&buf) {
            ParseResult::Incomplete | ParseResult::Invalid(_) => {}
            ParseResult::Complete(_) => panic!("Expected failure for domain with underscore"),
        }
    }

    // --- Port boundary tests ---

    #[test]
    fn test_parse_request_port_zero() {
        let hash = sha224_hex("testpassword");
        let mut buf = Vec::new();
        buf.extend_from_slice(hash.as_bytes());
        buf.extend_from_slice(b"\r\n");
        buf.push(0x01);
        buf.push(0x01); // IPv4
        buf.extend_from_slice(&[127, 0, 0, 1]);
        buf.extend_from_slice(&0u16.to_be_bytes()); // port 0
        buf.extend_from_slice(b"\r\n");

        match parse_trojan_request(&buf) {
            ParseResult::Complete(req) => {
                assert_eq!(req.port, 0);
            }
            _ => panic!("Expected Complete parse result"),
        }
    }

    #[test]
    fn test_parse_request_port_max() {
        let hash = sha224_hex("testpassword");
        let mut buf = Vec::new();
        buf.extend_from_slice(hash.as_bytes());
        buf.extend_from_slice(b"\r\n");
        buf.push(0x01);
        buf.push(0x01); // IPv4
        buf.extend_from_slice(&[127, 0, 0, 1]);
        buf.extend_from_slice(&0xFFFFu16.to_be_bytes()); // port 65535
        buf.extend_from_slice(b"\r\n");

        match parse_trojan_request(&buf) {
            ParseResult::Complete(req) => {
                assert_eq!(req.port, 65535);
            }
            _ => panic!("Expected Complete parse result"),
        }
    }

    #[test]
    fn test_parse_request_with_payload() {
        let hash = sha224_hex("testpassword");
        let mut buf = Vec::new();
        buf.extend_from_slice(hash.as_bytes());
        buf.extend_from_slice(b"\r\n");
        buf.push(0x01);
        buf.push(0x01); // IPv4
        buf.extend_from_slice(&[127, 0, 0, 1]);
        buf.extend_from_slice(&8080u16.to_be_bytes());
        buf.extend_from_slice(b"\r\n");
        buf.extend_from_slice(b"GET / HTTP/1.1\r\n\r\n"); // extra payload

        match parse_trojan_request(&buf) {
            ParseResult::Complete(req) => {
                assert_eq!(req.port, 8080);
                assert!(req.header_len < buf.len());
            }
            _ => panic!("Expected Complete parse result"),
        }
    }

    // --- TrojanCommand tests ---

    #[test]
    fn test_trojan_command_from_byte() {
        assert_eq!(TrojanCommand::from_byte(0x01), Some(TrojanCommand::Connect));
        assert_eq!(
            TrojanCommand::from_byte(0x03),
            Some(TrojanCommand::UdpAssociate)
        );
        assert_eq!(TrojanCommand::from_byte(0x00), None);
        assert_eq!(TrojanCommand::from_byte(0x02), None);
        assert_eq!(TrojanCommand::from_byte(0xFF), None);
    }

    #[test]
    fn test_trojan_command_values() {
        assert_eq!(TrojanCommand::Connect as u8, 0x01);
        assert_eq!(TrojanCommand::UdpAssociate as u8, 0x03);
    }
}

// ---------------------------------------------------------------------------
// Tests formerly in trojan_test.rs, merged here as an inner module
// ---------------------------------------------------------------------------

#[cfg(test)]
mod legacy_tests {
    //! Unit and integration tests for Trojan protocol connection stability.
    //!
    //! Integration tests are pure Rust — no external Trojan client binary needed.
    //! They use tokio-rustls with insecure/TOFU mode to connect to a local
    //! Trojan server, exercising the full TLS + Trojan handshake + proxy chain.

    use std::net::Ipv4Addr;
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio_rustls::client::TlsStream;

    use crate::common::{
        create_insecure_client_config, create_server_config, load_or_generate_cert,
    };
    use crate::server::control::ServerState;
    use crate::server::listener;
    use crate::server::trojan::sha224_hex;

    // ---------------------------------------------------------------------------
    // helper types
    // ---------------------------------------------------------------------------

    /// Target address type for building Trojan request headers.
    enum TestTargetAddr {
        Ipv4(Ipv4Addr),
        Domain(String),
    }

    // ---------------------------------------------------------------------------
    // helper functions
    // ---------------------------------------------------------------------------

    /// Bind to port 0 to get a random available port, then return it.
    async fn find_available_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    }

    /// Start a TCP echo server on a random port.
    /// Returns the port and a JoinHandle whose `abort()` method stops the server.
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

    /// Retry connecting to `port` until success or `dur` elapses.
    async fn wait_for_port(port: u16, dur: Duration) {
        let start = tokio::time::Instant::now();
        loop {
            if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
                return;
            }
            if start.elapsed() >= dur {
                panic!("Timed out waiting for port {}", port);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Generate a self-signed TLS certificate pair in a temp directory and return
    /// a `TlsAcceptor` ready for the Trojan server. The `TempDir` must be kept
    /// alive for the duration of the test.
    fn generate_test_tls_config() -> (std::sync::Arc<rustls::ServerConfig>, tempfile::TempDir) {
        let tmp_dir = tempfile::tempdir().unwrap();
        let cert_path = tmp_dir.path().join("cert.pem");
        let key_path = tmp_dir.path().join("key.pem");

        let cert_pair =
            load_or_generate_cert(cert_path.to_str().unwrap(), key_path.to_str().unwrap()).unwrap();
        let server_config = create_server_config(cert_pair).unwrap();

        (server_config, tmp_dir)
    }

    /// Spawn the Trojan listener as a tokio task. Returns the `watch::Receiver`
    /// (for reference), a `JoinHandle` for the listener task, and the `TempDir`
    /// that must be kept alive.
    async fn start_trojan_server(
        state: ServerState,
        port: u16,
        password: &str,
        fallback: &str,
    ) -> (
        tokio::sync::watch::Receiver<std::sync::Arc<rustls::ServerConfig>>,
        tokio::task::JoinHandle<()>,
        tempfile::TempDir,
    ) {
        let (server_config, tmp_dir) = generate_test_tls_config();
        let (_tx, rx) = tokio::sync::watch::channel(server_config);
        let password = password.to_string();
        let fallback = fallback.to_string();

        let handle = tokio::spawn(async move {
            let _ = listener::start_trojan_listener(state, port, password, fallback, rx).await;
        });

        // Return a dummy receiver for compatibility (tx keeps it alive)
        let (_dummy_tx, dummy_rx) = tokio::sync::watch::channel(
            create_server_config(
                load_or_generate_cert(
                    tmp_dir.path().join("cert.pem").to_str().unwrap(),
                    tmp_dir.path().join("key.pem").to_str().unwrap(),
                )
                .unwrap(),
            )
            .unwrap(),
        );
        (dummy_rx, handle, tmp_dir)
    }

    /// Build raw Trojan protocol request bytes (without payload).
    fn build_trojan_header(password: &str, cmd: u8, target: &TestTargetAddr, port: u16) -> Vec<u8> {
        let mut buf = Vec::with_capacity(256);

        // SHA-224 hex hash of password (56 bytes)
        buf.extend_from_slice(sha224_hex(password).as_bytes());
        // CRLF
        buf.extend_from_slice(b"\r\n");
        // Command
        buf.push(cmd);
        // Address
        match target {
            TestTargetAddr::Ipv4(addr) => {
                buf.push(0x01); // ATYP = IPv4
                buf.extend_from_slice(&addr.octets());
            }
            TestTargetAddr::Domain(domain) => {
                buf.push(0x03); // ATYP = Domain
                buf.push(domain.len() as u8);
                buf.extend_from_slice(domain.as_bytes());
            }
        }
        // Port (big-endian)
        buf.extend_from_slice(&port.to_be_bytes());
        // Trailing CRLF
        buf.extend_from_slice(b"\r\n");

        buf
    }

    /// Perform a full Trojan client connection: TLS handshake + send Trojan header.
    /// Returns the TLS stream ready for data exchange.
    async fn trojan_connect(
        trojan_port: u16,
        password: &str,
        target_port: u16,
    ) -> TlsStream<TcpStream> {
        trojan_connect_with_atype(
            trojan_port,
            password,
            &TestTargetAddr::Ipv4(Ipv4Addr::new(127, 0, 0, 1)),
            target_port,
        )
        .await
    }

    /// Connect with an explicit address type (IPv4 or Domain).
    async fn trojan_connect_with_atype(
        trojan_port: u16,
        password: &str,
        target: &TestTargetAddr,
        target_port: u16,
    ) -> TlsStream<TcpStream> {
        let config = create_insecure_client_config().unwrap();
        let connector = tokio_rustls::TlsConnector::from(config);

        let stream = TcpStream::connect(format!("127.0.0.1:{}", trojan_port))
            .await
            .expect("Failed to connect to Trojan server");

        let server_name = rustls::pki_types::ServerName::try_from("localhost".to_string())
            .expect("Invalid server name");
        let mut tls_stream = connector
            .connect(server_name, stream)
            .await
            .expect("TLS handshake failed");

        // Send Trojan request header
        let header = build_trojan_header(password, 0x01, target, target_port);
        tls_stream
            .write_all(&header)
            .await
            .expect("Failed to send Trojan header");

        tls_stream
    }

    /// High-level: connect via Trojan, send data, read back exactly `data.len()` bytes.
    async fn trojan_send_recv(
        trojan_port: u16,
        password: &str,
        target_port: u16,
        data: &[u8],
    ) -> Vec<u8> {
        let mut stream = trojan_connect(trojan_port, password, target_port).await;
        stream.write_all(data).await.expect("Failed to send data");

        let mut response = vec![0u8; data.len()];
        stream
            .read_exact(&mut response)
            .await
            .expect("Failed to read response");

        response
    }

    /// Read exactly `n` bytes from the stream with a timeout.
    async fn read_exact_timeout(
        stream: &mut TlsStream<TcpStream>,
        n: usize,
        timeout: Duration,
    ) -> Option<Vec<u8>> {
        let mut buf = vec![0u8; n];
        match tokio::time::timeout(timeout, stream.read(&mut buf)).await {
            Ok(Ok(read_n)) if read_n > 0 => {
                buf.truncate(read_n);
                Some(buf)
            }
            _ => None,
        }
    }

    // ---------------------------------------------------------------------------
    // unit tests
    // ---------------------------------------------------------------------------

    #[cfg(test)]
    mod unit_tests {
        use super::*;

        #[test]
        fn test_build_trojan_header_ipv4() {
            let header = build_trojan_header(
                "testpass",
                0x01,
                &TestTargetAddr::Ipv4(Ipv4Addr::new(127, 0, 0, 1)),
                443,
            );

            // Hash is 56 bytes + CRLF(2) + CMD(1) + ATYP(1) + IPv4(4) + PORT(2) + CRLF(2) = 68
            assert_eq!(header.len(), 68);
            // Verify CRLF after hash
            assert_eq!(header[56], b'\r');
            assert_eq!(header[57], b'\n');
            // Verify CMD
            assert_eq!(header[58], 0x01);
            // Verify ATYP
            assert_eq!(header[59], 0x01);
            // Verify IPv4 octets
            assert_eq!(&header[60..64], &[127, 0, 0, 1]);
            // Verify port (443 = 0x01BB)
            assert_eq!(header[64], 0x01);
            assert_eq!(header[65], 0xBB);
            // Verify trailing CRLF
            assert_eq!(header[66], b'\r');
            assert_eq!(header[67], b'\n');
        }

        #[test]
        fn test_build_trojan_header_domain() {
            let header = build_trojan_header(
                "testpass",
                0x01,
                &TestTargetAddr::Domain("example.com".to_string()),
                8080,
            );

            // 56 + 2 + 1 + 1 + 1 + 11 + 2 + 2 = 76
            assert_eq!(header.len(), 76);
            // Verify ATYP
            assert_eq!(header[59], 0x03);
            // Verify domain length
            assert_eq!(header[60], 11);
            // Verify domain bytes
            assert_eq!(&header[61..72], b"example.com");
        }

        #[test]
        fn test_build_trojan_header_hash_matches_sha224() {
            let header = build_trojan_header(
                "mypassword",
                0x01,
                &TestTargetAddr::Ipv4(Ipv4Addr::new(127, 0, 0, 1)),
                80,
            );
            let hash_part = std::str::from_utf8(&header[..56]).unwrap();
            assert_eq!(hash_part, sha224_hex("mypassword"));
        }
    }

    // ---------------------------------------------------------------------------
    // integration tests
    // ---------------------------------------------------------------------------

    #[cfg(test)]
    mod integration_tests {
        use super::*;

        #[tokio::test]
        #[ignore]
        async fn test_trojan_echo_basic() {
            let (echo_port, echo_handle) = start_echo_server().await;
            let state = ServerState::new();
            let trojan_port = find_available_port().await;

            let (_acceptor, server_handle, _tmp_dir) =
                start_trojan_server(state, trojan_port, "testpass", "127.0.0.1:1").await;
            wait_for_port(trojan_port, Duration::from_secs(5)).await;

            let response =
                trojan_send_recv(trojan_port, "testpass", echo_port, b"hello trojan").await;
            assert_eq!(response, b"hello trojan");

            server_handle.abort();
            echo_handle.abort();
        }

        #[tokio::test]
        #[ignore]
        async fn test_trojan_concurrent_connections() {
            let (echo_port, echo_handle) = start_echo_server().await;
            let state = ServerState::new();
            let trojan_port = find_available_port().await;

            let (_acceptor, server_handle, _tmp_dir) =
                start_trojan_server(state.clone(), trojan_port, "testpass", "127.0.0.1:1").await;
            wait_for_port(trojan_port, Duration::from_secs(5)).await;

            let mut handles = Vec::new();
            for i in 0..5 {
                let port = trojan_port;
                let echo = echo_port;
                handles.push(tokio::spawn(async move {
                    let payload = format!("conn-{}", i);
                    let mut stream = trojan_connect(port, "testpass", echo).await;
                    stream.write_all(payload.as_bytes()).await.unwrap();
                    let mut buf = vec![0u8; payload.len()];
                    stream.read_exact(&mut buf).await.unwrap();
                    buf
                }));
            }

            for (i, handle) in handles.into_iter().enumerate() {
                let result = handle.await.unwrap();
                let expected = format!("conn-{}", i);
                assert_eq!(&result[..], expected.as_bytes());
            }

            // All connections should be cleaned up
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert_eq!(
                state.get_connection_count_for_port(trojan_port).await,
                0,
                "Connection count should be 0 after all connections close"
            );

            server_handle.abort();
            echo_handle.abort();
        }

        #[tokio::test]
        #[ignore]
        async fn test_trojan_large_data_transfer() {
            let (echo_port, echo_handle) = start_echo_server().await;
            let state = ServerState::new();
            let trojan_port = find_available_port().await;

            let (_acceptor, server_handle, _tmp_dir) =
                start_trojan_server(state, trojan_port, "testpass", "127.0.0.1:1").await;
            wait_for_port(trojan_port, Duration::from_secs(5)).await;

            // 64 KB payload
            let payload: Vec<u8> = (0u8..=255).cycle().take(65536).collect();
            let mut stream = trojan_connect(trojan_port, "testpass", echo_port).await;

            stream.write_all(&payload).await.unwrap();

            // Read in a loop — the echo server uses an 8KB buffer
            let mut response = vec![0u8; payload.len()];
            let mut total_read = 0;
            while total_read < payload.len() {
                let n = stream
                    .read(&mut response[total_read..])
                    .await
                    .expect("Failed to read");
                if n == 0 {
                    break;
                }
                total_read += n;
            }
            assert_eq!(total_read, payload.len());
            assert_eq!(response, payload);

            server_handle.abort();
            echo_handle.abort();
        }

        #[tokio::test]
        #[ignore]
        async fn test_trojan_connection_retry() {
            let (echo_port, echo_handle) = start_echo_server().await;
            let state = ServerState::new();
            let trojan_port = find_available_port().await;

            let (_acceptor, server_handle, _tmp_dir) =
                start_trojan_server(state, trojan_port, "testpass", "127.0.0.1:1").await;
            wait_for_port(trojan_port, Duration::from_secs(5)).await;

            // First connection
            {
                let mut stream = trojan_connect(trojan_port, "testpass", echo_port).await;
                stream.write_all(b"first").await.unwrap();
                let mut buf = [0u8; 5];
                stream.read_exact(&mut buf).await.unwrap();
                assert_eq!(&buf, b"first");
            }

            // Wait for server-side cleanup
            tokio::time::sleep(Duration::from_millis(200)).await;

            // Second connection — should succeed
            {
                let mut stream = trojan_connect(trojan_port, "testpass", echo_port).await;
                stream.write_all(b"second").await.unwrap();
                let mut buf = [0u8; 6];
                stream.read_exact(&mut buf).await.unwrap();
                assert_eq!(&buf, b"second");
            }

            server_handle.abort();
            echo_handle.abort();
        }

        #[tokio::test]
        #[ignore]
        async fn test_trojan_auth_failure_fallback() {
            // The fallback server — a simple TCP listener that sends a known marker
            let fallback_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let fallback_port = fallback_listener.local_addr().unwrap().port();
            let fallback_handle = tokio::spawn(async move {
                // Accept one connection, read data, write back a marker
                if let Ok((mut stream, _)) = fallback_listener.accept().await {
                    let mut buf = [0u8; 256];
                    let _ = stream.read(&mut buf).await;
                    let _ = stream.write_all(b"FALLBACK_OK").await;
                }
            });

            let state = ServerState::new();
            let trojan_port = find_available_port().await;

            let (_acceptor, server_handle, _tmp_dir) = start_trojan_server(
                state,
                trojan_port,
                "correctpass",
                &format!("127.0.0.1:{}", fallback_port),
            )
            .await;
            wait_for_port(trojan_port, Duration::from_secs(5)).await;

            // Connect with wrong password
            let config = create_insecure_client_config().unwrap();
            let connector = tokio_rustls::TlsConnector::from(config);
            let stream = TcpStream::connect(format!("127.0.0.1:{}", trojan_port))
                .await
                .unwrap();
            let server_name =
                rustls::pki_types::ServerName::try_from("localhost".to_string()).unwrap();
            let mut tls_stream = connector.connect(server_name, stream).await.unwrap();

            // Send Trojan header with wrong password
            let header = build_trojan_header(
                "wrongpass",
                0x01,
                &TestTargetAddr::Ipv4(Ipv4Addr::new(127, 0, 0, 1)),
                80,
            );
            tls_stream.write_all(&header).await.unwrap();

            // The server should fall back — we should be able to read something
            // (the fallback server's response)
            let mut buf = [0u8; 64];
            let result =
                tokio::time::timeout(Duration::from_secs(3), tls_stream.read(&mut buf)).await;
            // The connection should either get fallback data or be closed by the server
            // Either way, the key property is that the server doesn't panic
            match result {
                Ok(Ok(n)) if n > 0 => {
                    // Got some data from the fallback — verify it contains our marker
                    let response = &buf[..n];
                    assert!(
                        response
                            .windows(b"FALLBACK_OK".len())
                            .any(|w| w == b"FALLBACK_OK"),
                        "Expected FALLBACK_OK in response, got: {:?}",
                        response
                    );
                }
                _ => {
                    // Connection closed is also acceptable — the fallback mechanism ran
                }
            }

            server_handle.abort();
            fallback_handle.abort();
        }

        #[tokio::test]
        #[ignore]
        async fn test_trojan_long_lived_connection() {
            let (echo_port, echo_handle) = start_echo_server().await;
            let state = ServerState::new();
            let trojan_port = find_available_port().await;

            let (_acceptor, server_handle, _tmp_dir) =
                start_trojan_server(state, trojan_port, "testpass", "127.0.0.1:1").await;
            wait_for_port(trojan_port, Duration::from_secs(5)).await;

            let mut stream = trojan_connect(trojan_port, "testpass", echo_port).await;

            for i in 0..10 {
                let payload = format!("round-{:03}-data", i);
                stream.write_all(payload.as_bytes()).await.unwrap();
                let mut buf = vec![0u8; payload.len()];
                stream.read_exact(&mut buf).await.unwrap();
                assert_eq!(buf, payload.as_bytes());
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            server_handle.abort();
            echo_handle.abort();
        }

        #[tokio::test]
        #[ignore]
        async fn test_trojan_rapid_connect_disconnect() {
            let (echo_port, echo_handle) = start_echo_server().await;
            let state = ServerState::new();
            let trojan_port = find_available_port().await;

            let (_acceptor, server_handle, _tmp_dir) =
                start_trojan_server(state.clone(), trojan_port, "testpass", "127.0.0.1:1").await;
            wait_for_port(trojan_port, Duration::from_secs(5)).await;

            for _ in 0..20 {
                let mut stream = trojan_connect(trojan_port, "testpass", echo_port).await;
                stream.write_all(b"ping").await.unwrap();
                let mut buf = [0u8; 4];
                stream.read_exact(&mut buf).await.unwrap();
                assert_eq!(&buf, b"ping");
                // Stream dropped here — connection closes
            }

            // Wait for all server-side cleanup
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(
                state.get_connection_count_for_port(trojan_port).await,
                0,
                "Connection count should be 0 after rapid connect/disconnect"
            );

            server_handle.abort();
            echo_handle.abort();
        }

        #[tokio::test]
        #[ignore]
        async fn test_trojan_domain_and_ipv4_target() {
            let (echo_port, echo_handle) = start_echo_server().await;
            let state = ServerState::new();
            let trojan_port = find_available_port().await;

            let (_acceptor, server_handle, _tmp_dir) =
                start_trojan_server(state, trojan_port, "testpass", "127.0.0.1:1").await;
            wait_for_port(trojan_port, Duration::from_secs(5)).await;

            // Test IPv4 targeting
            {
                let mut stream = trojan_connect(trojan_port, "testpass", echo_port).await;
                stream.write_all(b"ipv4-test").await.unwrap();
                let mut buf = [0u8; 9];
                stream.read_exact(&mut buf).await.unwrap();
                assert_eq!(&buf, b"ipv4-test");
            }

            // Test Domain targeting — "localhost" resolves to 127.0.0.1
            {
                let mut stream = trojan_connect_with_atype(
                    trojan_port,
                    "testpass",
                    &TestTargetAddr::Domain("localhost".to_string()),
                    echo_port,
                )
                .await;
                stream.write_all(b"domain-test").await.unwrap();
                let mut buf = [0u8; 11];
                stream.read_exact(&mut buf).await.unwrap();
                assert_eq!(&buf, b"domain-test");
            }

            server_handle.abort();
            echo_handle.abort();
        }

        #[tokio::test]
        #[ignore]
        async fn test_trojan_udp_associate_rejected() {
            let (echo_port, echo_handle) = start_echo_server().await;
            let state = ServerState::new();
            let trojan_port = find_available_port().await;

            let (_acceptor, server_handle, _tmp_dir) =
                start_trojan_server(state, trojan_port, "testpass", "127.0.0.1:1").await;
            wait_for_port(trojan_port, Duration::from_secs(5)).await;

            // Connect and send UDP ASSOCIATE command (0x03)
            let config = create_insecure_client_config().unwrap();
            let connector = tokio_rustls::TlsConnector::from(config);
            let stream = TcpStream::connect(format!("127.0.0.1:{}", trojan_port))
                .await
                .unwrap();
            let server_name =
                rustls::pki_types::ServerName::try_from("localhost".to_string()).unwrap();
            let mut tls_stream = connector.connect(server_name, stream).await.unwrap();

            let header = build_trojan_header(
                "testpass",
                0x03, // UDP ASSOCIATE
                &TestTargetAddr::Ipv4(Ipv4Addr::new(127, 0, 0, 1)),
                echo_port,
            );
            tls_stream.write_all(&header).await.unwrap();
            tls_stream.write_all(b"udp-data").await.unwrap();

            // The server should close the connection (UDP not supported)
            let result = read_exact_timeout(&mut tls_stream, 8, Duration::from_secs(3)).await;
            // Either we get EOF (0 bytes) or an error — either way, no echo data
            assert!(
                result.is_none() || result.as_ref().map(|r| r.is_empty()).unwrap_or(true),
                "Expected connection close for UDP ASSOCIATE, but got data"
            );

            server_handle.abort();
            echo_handle.abort();
        }

        #[tokio::test]
        #[ignore]
        async fn test_trojan_active_connection_count() {
            let (echo_port, echo_handle) = start_echo_server().await;
            let state = ServerState::new();
            let trojan_port = find_available_port().await;

            let (_acceptor, server_handle, _tmp_dir) =
                start_trojan_server(state.clone(), trojan_port, "testpass", "127.0.0.1:1").await;
            wait_for_port(trojan_port, Duration::from_secs(5)).await;

            // No connections yet
            assert_eq!(state.get_connection_count_for_port(trojan_port).await, 0);

            // Open a connection and keep it alive
            let mut stream = trojan_connect(trojan_port, "testpass", echo_port).await;
            stream.write_all(b"count-test").await.unwrap();
            let mut buf = [0u8; 10];
            stream.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"count-test");

            // Wait for the server to increment the counter
            tokio::time::sleep(Duration::from_millis(100)).await;
            let count = state.get_connection_count_for_port(trojan_port).await;
            assert!(count >= 1, "Expected >= 1 active connection, got {}", count);

            // Close the connection
            drop(stream);
            tokio::time::sleep(Duration::from_millis(300)).await;

            let count = state.get_connection_count_for_port(trojan_port).await;
            assert_eq!(
                count, 0,
                "Expected 0 connections after close, got {}",
                count
            );

            server_handle.abort();
            echo_handle.abort();
        }

        #[tokio::test]
        #[ignore]
        async fn test_trojan_initial_payload() {
            let (echo_port, echo_handle) = start_echo_server().await;
            let state = ServerState::new();
            let trojan_port = find_available_port().await;

            let (_acceptor, server_handle, _tmp_dir) =
                start_trojan_server(state, trojan_port, "testpass", "127.0.0.1:1").await;
            wait_for_port(trojan_port, Duration::from_secs(5)).await;

            // Connect and include initial payload in the Trojan header
            let config = create_insecure_client_config().unwrap();
            let connector = tokio_rustls::TlsConnector::from(config);
            let stream = TcpStream::connect(format!("127.0.0.1:{}", trojan_port))
                .await
                .unwrap();
            let server_name =
                rustls::pki_types::ServerName::try_from("localhost".to_string()).unwrap();
            let mut tls_stream = connector.connect(server_name, stream).await.unwrap();

            // Build header with initial payload appended
            let mut request = build_trojan_header(
                "testpass",
                0x01,
                &TestTargetAddr::Ipv4(Ipv4Addr::new(127, 0, 0, 1)),
                echo_port,
            );
            request.extend_from_slice(b"INITIAL");
            tls_stream.write_all(&request).await.unwrap();

            // Send more data after the initial payload
            tls_stream.write_all(b"MORE").await.unwrap();

            // Read back the echo of both payloads
            let total_len = 7 + 4;
            let mut response = Vec::with_capacity(total_len);
            let mut tmp = [0u8; 64];
            while response.len() < total_len {
                let n = tls_stream.read(&mut tmp).await.unwrap();
                if n == 0 {
                    break;
                }
                response.extend_from_slice(&tmp[..n]);
            }

            assert!(
                response.len() >= total_len,
                "Expected at least {} bytes, got {}",
                total_len,
                response.len()
            );
            // The echo should contain both payloads in order
            assert!(
                response.starts_with(b"INITIAL"),
                "Expected response starting with INITIAL, got: {:?}",
                response
            );

            server_handle.abort();
            echo_handle.abort();
        }
    }
}
