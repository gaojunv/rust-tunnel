pub mod cli;
pub mod file;
pub mod merge;

pub use cli::ServerCli;
pub use file::ServerConfigFile;

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub control_addr: String,
    pub api_addr: String,
    pub admin_password: Option<String>,
    pub jwt_secret: Option<String>,
    /// Authentication token for client connections
    /// If Some, clients must provide this token to register
    pub client_auth_token: Option<String>,
    /// Enable TLS encryption for control channel
    pub tls: bool,
    /// Path to TLS certificate file (PEM format)
    pub tls_cert: String,
    /// Path to TLS private key file (PEM format)
    pub tls_key: String,
    pub log: String,
    pub db_path: String,
    pub ss_enabled: bool,
    pub ss_port: Option<u16>,
    pub ss_cipher: Option<String>,
    pub ss_password: Option<String>,
    pub trojan_enabled: bool,
    pub trojan_port: Option<u16>,
    pub trojan_password: Option<String>,
    pub trojan_fallback: String,
    pub dns_enabled: bool,
    pub dns_bind: String,
    pub dns_tunnel_domain: String,
    pub dns_mesh_domain: String,
    // Reverse Proxy configuration
    pub reverse_proxy_enabled: bool,
    pub reverse_proxy_max_connections: u32,
    pub reverse_proxy_connection_timeout: u64,
    pub reverse_proxy_buffer_size: usize,
    // API TLS configuration
    pub api_tls: bool,
    pub api_domain: Option<String>,
    // ACME configuration
    pub acme_enabled: bool,
    pub acme_server_url: String,
    pub acme_cert_dir: String,
    pub acme_auto_renew: bool,
    pub acme_renewal_check_interval: u64,
    pub acme_renewal_days_before_expiry: u64,
    pub acme_email: Option<String>,
    pub acme_tos_agreed: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            control_addr: "0.0.0.0:8080".to_string(),
            api_addr: "0.0.0.0:3000".to_string(),
            admin_password: None,
            jwt_secret: None,
            client_auth_token: None,
            tls: true, // TLS enabled by default for security
            tls_cert: "./data/tls/cert.pem".to_string(),
            tls_key: "./data/tls/key.pem".to_string(),
            log: "info".to_string(),
            db_path: "./data/rust-tunnel.db".to_string(),
            ss_enabled: false,
            ss_port: None,
            ss_cipher: None,
            ss_password: None,
            trojan_enabled: false,
            trojan_port: None,
            trojan_password: None,
            trojan_fallback: "127.0.0.1:80".to_string(),
            dns_enabled: false,
            dns_bind: "0.0.0.0:53".to_string(),
            dns_tunnel_domain: "tunnel.local".to_string(),
            dns_mesh_domain: "mesh.local".to_string(),
            // Reverse Proxy defaults
            reverse_proxy_enabled: false,
            reverse_proxy_max_connections: 10000,
            reverse_proxy_connection_timeout: 30,
            reverse_proxy_buffer_size: 8192,
            // API TLS defaults
            api_tls: false,
            api_domain: None,
            // ACME defaults
            acme_enabled: false,
            acme_server_url: "https://acme-staging-v02.api.letsencrypt.org/directory".to_string(),
            acme_cert_dir: "./data/certs".to_string(),
            acme_auto_renew: true,
            acme_renewal_check_interval: 24,
            acme_renewal_days_before_expiry: 30,
            acme_email: None,
            acme_tos_agreed: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = ServerConfig::default();
        assert_eq!(config.control_addr, "0.0.0.0:8080");
        assert_eq!(config.api_addr, "0.0.0.0:3000");
        assert_eq!(config.log, "info");
        assert!(config.admin_password.is_none());
        assert!(config.jwt_secret.is_none());
        assert!(config.client_auth_token.is_none());
        assert!(config.tls); // TLS enabled by default
        assert_eq!(config.tls_cert, "./data/tls/cert.pem");
        assert_eq!(config.tls_key, "./data/tls/key.pem");
        assert!(!config.ss_enabled);
        assert!(config.ss_port.is_none());
        assert!(config.ss_cipher.is_none());
        assert!(config.ss_password.is_none());
        assert!(!config.trojan_enabled);
        assert!(config.trojan_port.is_none());
        assert!(config.trojan_password.is_none());
        assert_eq!(config.trojan_fallback, "127.0.0.1:80");
        assert!(!config.dns_enabled);
        assert_eq!(config.dns_bind, "0.0.0.0:53");
        assert_eq!(config.dns_tunnel_domain, "tunnel.local");
        assert_eq!(config.dns_mesh_domain, "mesh.local");
    }

    #[test]
    fn test_config_from_cli_no_file() {
        let cli = ServerCli {
            config_file: None,
            control_addr: Some("127.0.0.1:9000".to_string()),
            api_addr: Some("127.0.0.1:9001".to_string()),
            admin_password: Some("secret123".to_string()),
            jwt_secret: Some("test-secret".to_string()),
            client_auth_token: Some("client-secret".to_string()),
            tls: Some(false),
            tls_cert: Some("/custom/cert.pem".to_string()),
            tls_key: Some("/custom/key.pem".to_string()),
            log: Some("debug".to_string()),
            db_path: Some("./test.db".to_string()),
            ss_enabled: Some(true),
            ss_port: Some(8388),
            ss_cipher: Some("aes-256-gcm".to_string()),
            ss_password: Some("ss-password".to_string()),
            trojan_enabled: None,
            trojan_port: None,
            trojan_password: None,
            trojan_fallback: None,
            dns_enabled: None,
            dns_bind: None,
            dns_tunnel_domain: None,
            dns_mesh_domain: None,
            reverse_proxy_enabled: None,
            reverse_proxy_max_connections: None,
            reverse_proxy_connection_timeout: None,
            reverse_proxy_buffer_size: None,
            api_tls: None,
            api_domain: None,
            acme_enabled: None,
            acme_server_url: None,
            acme_cert_dir: None,
            acme_auto_renew: None,
            acme_renewal_check_interval: None,
            acme_renewal_days_before_expiry: None,
            acme_email: None,
            acme_tos_agreed: None,
        };

        let config = ServerConfig::from_cli(cli).unwrap();
        assert_eq!(config.control_addr, "127.0.0.1:9000");
        assert_eq!(config.api_addr, "127.0.0.1:9001");
        assert_eq!(config.admin_password, Some("secret123".into()));
        assert_eq!(config.jwt_secret, Some("test-secret".into()));
        assert_eq!(config.client_auth_token, Some("client-secret".into()));
        assert!(!config.tls);
        assert_eq!(config.tls_cert, "/custom/cert.pem");
        assert_eq!(config.tls_key, "/custom/key.pem");
        assert_eq!(config.log, "debug");
        assert_eq!(config.db_path, "./test.db");
        assert!(config.ss_enabled);
        assert_eq!(config.ss_port, Some(8388));
        assert_eq!(config.ss_cipher, Some("aes-256-gcm".into()));
        assert_eq!(config.ss_password, Some("ss-password".into()));
    }

    #[test]
    fn test_config_clone() {
        let config = ServerConfig {
            control_addr: "127.0.0.1:9000".to_string(),
            api_addr: "127.0.0.1:9001".to_string(),
            admin_password: Some("test".to_string()),
            jwt_secret: Some("secret".to_string()),
            client_auth_token: Some("client-token".to_string()),
            tls: true,
            tls_cert: "./test-cert.pem".to_string(),
            tls_key: "./test-key.pem".to_string(),
            log: "debug".to_string(),
            db_path: "./test.db".to_string(),
            ss_enabled: true,
            ss_port: Some(8388),
            ss_cipher: Some("aes-256-gcm".to_string()),
            ss_password: Some("test".to_string()),
            trojan_enabled: true,
            trojan_port: Some(443),
            trojan_password: Some("trojan-pass".to_string()),
            trojan_fallback: "127.0.0.1:8080".to_string(),
            dns_enabled: true,
            dns_bind: "0.0.0.0:53".to_string(),
            dns_tunnel_domain: "tunnel.local".to_string(),
            dns_mesh_domain: "mesh.local".to_string(),
            reverse_proxy_enabled: false,
            reverse_proxy_max_connections: 10000,
            reverse_proxy_connection_timeout: 30,
            reverse_proxy_buffer_size: 8192,
            api_tls: false,
            api_domain: None,
            acme_enabled: false,
            acme_server_url: "https://acme-staging-v02.api.letsencrypt.org/directory".to_string(),
            acme_cert_dir: "./data/certs".to_string(),
            acme_auto_renew: true,
            acme_renewal_check_interval: 24,
            acme_renewal_days_before_expiry: 30,
            acme_email: None,
            acme_tos_agreed: false,
        };

        let cloned = config.clone();
        assert_eq!(config.control_addr, cloned.control_addr);
        assert_eq!(config.api_addr, cloned.api_addr);
        assert_eq!(config.admin_password, cloned.admin_password);
        assert_eq!(config.jwt_secret, cloned.jwt_secret);
        assert_eq!(config.client_auth_token, cloned.client_auth_token);
        assert_eq!(config.tls, cloned.tls);
        assert_eq!(config.tls_cert, cloned.tls_cert);
        assert_eq!(config.tls_key, cloned.tls_key);
        assert_eq!(config.log, cloned.log);
        assert_eq!(config.db_path, cloned.db_path);
        assert_eq!(config.ss_enabled, cloned.ss_enabled);
        assert_eq!(config.ss_port, cloned.ss_port);
        assert_eq!(config.ss_cipher, cloned.ss_cipher);
        assert_eq!(config.ss_password, cloned.ss_password);
        assert_eq!(config.trojan_enabled, cloned.trojan_enabled);
        assert_eq!(config.trojan_port, cloned.trojan_port);
        assert_eq!(config.trojan_password, cloned.trojan_password);
        assert_eq!(config.trojan_fallback, cloned.trojan_fallback);
        assert_eq!(config.dns_enabled, cloned.dns_enabled);
        assert_eq!(config.dns_bind, cloned.dns_bind);
        assert_eq!(config.dns_tunnel_domain, cloned.dns_tunnel_domain);
        assert_eq!(config.dns_mesh_domain, cloned.dns_mesh_domain);
    }

    #[test]
    fn test_config_file_not_found() {
        let cli = ServerCli {
            config_file: Some("/nonexistent/config.toml".to_string()),
            control_addr: None,
            api_addr: None,
            admin_password: None,
            jwt_secret: None,
            client_auth_token: None,
            tls: None,
            tls_cert: None,
            tls_key: None,
            log: None,
            db_path: None,
            ss_enabled: None,
            ss_port: None,
            ss_cipher: None,
            ss_password: None,
            trojan_enabled: None,
            trojan_port: None,
            trojan_password: None,
            trojan_fallback: None,
            dns_enabled: None,
            dns_bind: None,
            dns_tunnel_domain: None,
            dns_mesh_domain: None,
            reverse_proxy_enabled: None,
            reverse_proxy_max_connections: None,
            reverse_proxy_connection_timeout: None,
            reverse_proxy_buffer_size: None,
            api_tls: None,
            api_domain: None,
            acme_enabled: None,
            acme_server_url: None,
            acme_cert_dir: None,
            acme_auto_renew: None,
            acme_renewal_check_interval: None,
            acme_renewal_days_before_expiry: None,
            acme_email: None,
            acme_tos_agreed: None,
        };

        let result = ServerConfig::from_cli(cli);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn test_ss_config_validation_missing_port() {
        let cli = ServerCli {
            config_file: None,
            control_addr: None,
            api_addr: None,
            admin_password: None,
            jwt_secret: None,
            client_auth_token: None,
            tls: None,
            tls_cert: None,
            tls_key: None,
            log: None,
            db_path: None,
            ss_enabled: Some(true),
            ss_port: None,
            ss_cipher: Some("aes-256-gcm".to_string()),
            ss_password: Some("password".to_string()),
            trojan_enabled: None,
            trojan_port: None,
            trojan_password: None,
            trojan_fallback: None,
            dns_enabled: None,
            dns_bind: None,
            dns_tunnel_domain: None,
            dns_mesh_domain: None,
            reverse_proxy_enabled: None,
            reverse_proxy_max_connections: None,
            reverse_proxy_connection_timeout: None,
            reverse_proxy_buffer_size: None,
            api_tls: None,
            api_domain: None,
            acme_enabled: None,
            acme_server_url: None,
            acme_cert_dir: None,
            acme_auto_renew: None,
            acme_renewal_check_interval: None,
            acme_renewal_days_before_expiry: None,
            acme_email: None,
            acme_tos_agreed: None,
        };

        let result = ServerConfig::from_cli(cli);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("ss_port is required"));
    }

    #[test]
    fn test_ss_config_validation_invalid_cipher() {
        let cli = ServerCli {
            config_file: None,
            control_addr: None,
            api_addr: None,
            admin_password: None,
            jwt_secret: None,
            client_auth_token: None,
            tls: None,
            tls_cert: None,
            tls_key: None,
            log: None,
            db_path: None,
            ss_enabled: Some(true),
            ss_port: Some(8388),
            ss_cipher: Some("invalid-cipher".to_string()),
            ss_password: Some("password".to_string()),
            trojan_enabled: None,
            trojan_port: None,
            trojan_password: None,
            trojan_fallback: None,
            dns_enabled: None,
            dns_bind: None,
            dns_tunnel_domain: None,
            dns_mesh_domain: None,
            reverse_proxy_enabled: None,
            reverse_proxy_max_connections: None,
            reverse_proxy_connection_timeout: None,
            reverse_proxy_buffer_size: None,
            api_tls: None,
            api_domain: None,
            acme_enabled: None,
            acme_server_url: None,
            acme_cert_dir: None,
            acme_auto_renew: None,
            acme_renewal_check_interval: None,
            acme_renewal_days_before_expiry: None,
            acme_email: None,
            acme_tos_agreed: None,
        };

        let result = ServerConfig::from_cli(cli);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Unsupported cipher"));
    }

    #[test]
    fn test_trojan_config_validation_missing_port() {
        let cli = ServerCli {
            config_file: None,
            control_addr: None,
            api_addr: None,
            admin_password: None,
            jwt_secret: None,
            client_auth_token: None,
            tls: None,
            tls_cert: None,
            tls_key: None,
            log: None,
            db_path: None,
            ss_enabled: None,
            ss_port: None,
            ss_cipher: None,
            ss_password: None,
            trojan_enabled: Some(true),
            trojan_port: None,
            trojan_password: Some("password".to_string()),
            trojan_fallback: None,
            dns_enabled: None,
            dns_bind: None,
            dns_tunnel_domain: None,
            dns_mesh_domain: None,
            reverse_proxy_enabled: None,
            reverse_proxy_max_connections: None,
            reverse_proxy_connection_timeout: None,
            reverse_proxy_buffer_size: None,
            api_tls: None,
            api_domain: None,
            acme_enabled: None,
            acme_server_url: None,
            acme_cert_dir: None,
            acme_auto_renew: None,
            acme_renewal_check_interval: None,
            acme_renewal_days_before_expiry: None,
            acme_email: None,
            acme_tos_agreed: None,
        };

        let result = ServerConfig::from_cli(cli);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("trojan_port is required"));
    }

    #[test]
    fn test_trojan_config_validation_missing_password() {
        let cli = ServerCli {
            config_file: None,
            control_addr: None,
            api_addr: None,
            admin_password: None,
            jwt_secret: None,
            client_auth_token: None,
            tls: None,
            tls_cert: None,
            tls_key: None,
            log: None,
            db_path: None,
            ss_enabled: None,
            ss_port: None,
            ss_cipher: None,
            ss_password: None,
            trojan_enabled: Some(true),
            trojan_port: Some(443),
            trojan_password: None,
            trojan_fallback: None,
            dns_enabled: None,
            dns_bind: None,
            dns_tunnel_domain: None,
            dns_mesh_domain: None,
            reverse_proxy_enabled: None,
            reverse_proxy_max_connections: None,
            reverse_proxy_connection_timeout: None,
            reverse_proxy_buffer_size: None,
            api_tls: None,
            api_domain: None,
            acme_enabled: None,
            acme_server_url: None,
            acme_cert_dir: None,
            acme_auto_renew: None,
            acme_renewal_check_interval: None,
            acme_renewal_days_before_expiry: None,
            acme_email: None,
            acme_tos_agreed: None,
        };

        let result = ServerConfig::from_cli(cli);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("trojan_password is required"));
    }

    #[test]
    fn test_dns_config_validation_invalid_bind() {
        let cli = ServerCli {
            config_file: None,
            control_addr: None,
            api_addr: None,
            admin_password: None,
            jwt_secret: None,
            client_auth_token: None,
            tls: None,
            tls_cert: None,
            tls_key: None,
            log: None,
            db_path: None,
            ss_enabled: None,
            ss_port: None,
            ss_cipher: None,
            ss_password: None,
            trojan_enabled: None,
            trojan_port: None,
            trojan_password: None,
            trojan_fallback: None,
            dns_enabled: Some(true),
            dns_bind: Some("not-an-address".to_string()),
            dns_tunnel_domain: None,
            dns_mesh_domain: None,
            reverse_proxy_enabled: None,
            reverse_proxy_max_connections: None,
            reverse_proxy_connection_timeout: None,
            reverse_proxy_buffer_size: None,
            api_tls: None,
            api_domain: None,
            acme_enabled: None,
            acme_server_url: None,
            acme_cert_dir: None,
            acme_auto_renew: None,
            acme_renewal_check_interval: None,
            acme_renewal_days_before_expiry: None,
            acme_email: None,
            acme_tos_agreed: None,
        };

        let result = ServerConfig::from_cli(cli);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid dns_bind"));
    }
}