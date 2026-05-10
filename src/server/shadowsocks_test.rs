//! Integration tests for Shadowsocks handshake

#[cfg(test)]
mod tests {
    use super::super::shadowsocks::*;
    use crate::server::shadowsocks::parse_cipher_kind;
    use shadowsocks::config::ServerConfig;

    #[tokio::test]
    async fn test_key_derivation_consistency() {
        // Test that our key derivation produces the same result as expected
        let password = "testpassword";
        let cipher = "aes-256-gcm";

        let key = derive_key(password, cipher).unwrap();
        assert_eq!(key.len(), 32); // AES-256-GCM needs 32 bytes

        // Compare with what shadowsocks ServerConfig would produce
        let kind = parse_cipher_kind(cipher).unwrap();
        let server_addr: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let cfg = ServerConfig::new(server_addr, password, kind).unwrap();
        assert_eq!(key, cfg.key());
    }
}
