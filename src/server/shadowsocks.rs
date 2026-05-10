use tokio::net::TcpStream;
use tokio::io::AsyncReadExt;
use tracing::{debug, error};
use crate::common::{TunnelError, TunnelResult};

/// Shadowsocks connection context holding encryption state and target info
#[derive(Debug, Clone)]
pub struct SSConnectionContext {
    pub cipher_type: String,
    pub key: Vec<u8>,
    pub target_addr: String,
    pub connection_id: u64,
    pub port: u16,
}

/// Trait for SS cipher operations (placeholder for actual crypto)
pub trait SSCipher: Send + Sync {
    fn encrypt(&mut self, data: &[u8]) -> TunnelResult<Vec<u8>>;
    fn decrypt(&mut self, data: &[u8]) -> TunnelResult<Vec<u8>>;
}

/// Simple pass-through cipher (placeholder for actual shadowsocks crypto)
/// NOTE: In a real implementation, this would use shadowsocks-rust crypto APIs
#[derive(Debug, Clone)]
pub struct PassThroughCipher;

impl SSCipher for PassThroughCipher {
    fn encrypt(&mut self, data: &[u8]) -> TunnelResult<Vec<u8>> {
        // Placeholder: return data as-is
        Ok(data.to_vec())
    }

    fn decrypt(&mut self, data: &[u8]) -> TunnelResult<Vec<u8>> {
        // Placeholder: return data as-is
        Ok(data.to_vec())
    }
}

/// Derive encryption key from password (placeholder for actual key derivation)
pub fn derive_key(password: &str, cipher: &str) -> TunnelResult<Vec<u8>> {
    // Placeholder implementation - in a real implementation this would use
    // the proper key derivation method (e.g., EVP_BytesToKey for shadowsocks)
    let key_len = match cipher {
        "aes-256-gcm" => 32,
        "chacha20-ietf-poly1305" => 32,
        _ => return Err(TunnelError::Protocol(format!("Unsupported cipher: {}", cipher))),
    };

    // Simple placeholder: use password bytes padded to key length
    let mut key = Vec::with_capacity(key_len);
    key.extend_from_slice(password.as_bytes());
    key.resize(key_len, 0u8);

    Ok(key)
}

/// Create cipher from config (placeholder)
pub fn create_cipher(cipher_type: &str, _key: &[u8]) -> TunnelResult<Box<dyn SSCipher>> {
    match cipher_type {
        "aes-256-gcm" | "chacha20-ietf-poly1305" => {
            // Placeholder: return pass-through cipher
            // In a real implementation, this would use shadowsocks-rust::crypto
            Ok(Box::new(PassThroughCipher))
        }
        _ => Err(TunnelError::Protocol(format!("Unsupported cipher: {}", cipher_type))),
    }
}

/// Parse Shadowsocks target address from decrypted header
/// Address format:
/// - [0x01] [4 bytes IPv4] [2 bytes port BE]
/// - [0x03] [1 byte domain len] [N bytes domain] [2 bytes port BE]
/// - [0x04] [16 bytes IPv6] [2 bytes port BE]
pub fn parse_target_address(data: &[u8]) -> TunnelResult<(String, usize)> {
    if data.is_empty() {
        return Err(TunnelError::Protocol("Empty address data".to_string()));
    }

    let addr_type = data[0];
    let mut offset = 1;

    let host = match addr_type {
        0x01 => { // IPv4
            if data.len() < offset + 4 {
                return Err(TunnelError::Protocol("Incomplete IPv4 address".to_string()));
            }
            let ip = format!("{}.{}.{}.{}",
                data[offset], data[offset+1], data[offset+2], data[offset+3]);
            offset += 4;
            ip
        }
        0x03 => { // Domain name
            if data.len() < offset + 1 {
                return Err(TunnelError::Protocol("Missing domain length".to_string()));
            }
            let len = data[offset] as usize;
            offset += 1;
            if data.len() < offset + len {
                return Err(TunnelError::Protocol("Incomplete domain name".to_string()));
            }
            let domain = String::from_utf8_lossy(&data[offset..offset+len]).to_string();
            offset += len;
            domain
        }
        0x04 => { // IPv6
            if data.len() < offset + 16 {
                return Err(TunnelError::Protocol("Incomplete IPv6 address".to_string()));
            }
            let mut segments = Vec::new();
            for i in 0..8 {
                let seg = u16::from_be_bytes([data[offset + i*2], data[offset + i*2 + 1]]);
                segments.push(format!("{:x}", seg));
            }
            offset += 16;
            segments.join(":")
        }
        _ => return Err(TunnelError::Protocol(format!("Unknown address type: {}", addr_type))),
    };

    if data.len() < offset + 2 {
        return Err(TunnelError::Protocol("Missing port".to_string()));
    }

    let port = u16::from_be_bytes([data[offset], data[offset+1]]);
    offset += 2;

    Ok((format!("{}:{}", host, port), offset))
}

/// Handle SS handshake - placeholder for actual decryption and parsing
/// In the real implementation, this would:
/// 1. Read IV/salt from stream
/// 2. Initialize cipher with key + IV/salt
/// 3. Read and decrypt the encrypted address header
/// 4. Parse target address from decrypted data
pub async fn handle_ss_handshake(
    stream: &mut TcpStream,
    cipher: &str,
    password: &str,
    connection_id: u64,
    port: u16,
) -> TunnelResult<(SSConnectionContext, Box<dyn SSCipher>)> {
    debug!("Starting SS handshake for connection {}, port {}", connection_id, port);

    // Derive key from password
    let key = derive_key(password, cipher)?;

    // Create cipher
    let ss_cipher = create_cipher(cipher, &key)?;

    // Placeholder: Read and decrypt handshake data
    // For now, we expect the target address in plaintext (for testing)
    // Actual implementation will:
    // - Read IV/salt
    // - Initialize cipher
    // - Read and decrypt address header
    // - Parse target address

    let mut buf = [0u8; 512];
    let n = stream.peek(&mut buf).await?;

    if n == 0 {
        return Err(TunnelError::Protocol("Empty handshake".to_string()));
    }

    // Try to parse as plaintext for testing (will be replaced with decrypted data)
    match parse_target_address(&buf[..n]) {
        Ok((target_addr, consumed)) => {
            // Consume the bytes we peeked
            let mut consume_buf = vec![0u8; consumed];
            stream.read_exact(&mut consume_buf).await?;

            debug!("Parsed SS target address: {}", target_addr);

            let ctx = SSConnectionContext {
                cipher_type: cipher.to_string(),
                key,
                target_addr,
                connection_id,
                port,
            };

            Ok((ctx, ss_cipher))
        }
        Err(e) => {
            error!("Failed to parse SS address: {}", e);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_derive_key() {
        let key = derive_key("test-password", "aes-256-gcm").unwrap();
        assert_eq!(key.len(), 32);
    }

    #[test]
    fn test_derive_key_unsupported_cipher() {
        let result = derive_key("test-password", "unsupported-cipher");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_ipv4_address() {
        // Type 0x01, IP 192.168.1.1, port 80 (0x0050)
        let data = [0x01, 192, 168, 1, 1, 0x00, 0x50];
        let (addr, consumed) = parse_target_address(&data).unwrap();
        assert_eq!(addr, "192.168.1.1:80");
        assert_eq!(consumed, 7); // 1 (type) + 4 (ip) + 2 (port)
    }

    #[test]
    fn test_parse_domain_address() {
        // Type 0x03, domain length 11, "example.com", port 443 (0x01BB)
        let domain = b"example.com";
        let mut data = vec![0x03, domain.len() as u8];
        data.extend_from_slice(domain);
        data.extend_from_slice(&[0x01, 0xBB]); // port 443

        let (addr, consumed) = parse_target_address(&data).unwrap();
        assert_eq!(addr, "example.com:443");
        assert_eq!(consumed, 1 + 1 + domain.len() + 2);
    }

    #[test]
    fn test_parse_empty_address() {
        let result = parse_target_address(&[]);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_incomplete_ipv4() {
        // Type 0x01 but only 2 bytes for IP (incomplete)
        let data = [0x01, 192, 168];
        let result = parse_target_address(&data);
        assert!(result.is_err());
    }

    #[test]
    fn test_pass_through_cipher() {
        let mut cipher = PassThroughCipher;
        let data = b"hello world";
        let encrypted = cipher.encrypt(data).unwrap();
        let decrypted = cipher.decrypt(&encrypted).unwrap();
        assert_eq!(data, decrypted.as_slice());
    }
}