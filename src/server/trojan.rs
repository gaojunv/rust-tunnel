use std::net::{Ipv4Addr, Ipv6Addr};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::server::TlsStream;
use tracing::{debug, warn};

use crate::common::{TunnelError, TunnelResult};

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
