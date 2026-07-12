use clap::Parser;
use figment::{
    providers::{Format, Toml},
    Figment,
};
use serde::Deserialize;
use std::path::Path;

/// Server endpoint for rust-tunnel intranet penetration tool
#[derive(Parser, Debug, Clone)]
pub struct ServerCli {
    /// Path to configuration file (TOML format)
    #[clap(long = "config")]
    pub config_file: Option<String>,

    /// Address to listen for control connections from clients
    /// Format: 0.0.0.0:8080
    #[clap(long = "bind")]
    pub control_addr: Option<String>,

    /// Address to listen for HTTP API connections
    /// Format: 0.0.0.0:3000
    #[clap(long = "api-bind")]
    pub api_addr: Option<String>,

    /// Password for web interface authentication (optional)
    #[clap(long = "admin-password")]
    pub admin_password: Option<String>,

    /// Secret key for JWT tokens (auto-generated if not provided)
    #[clap(long = "jwt-secret")]
    pub jwt_secret: Option<String>,

    /// Authentication token for client connections (optional but recommended)
    /// If set, clients must provide this token to register
    #[clap(long = "client-auth-token")]
    pub client_auth_token: Option<String>,

    /// Enable TLS encryption for control channel
    /// If true, clients must connect using TLS
    #[clap(long = "tls")]
    pub tls: Option<bool>,

    /// Path to TLS certificate file (PEM format)
    /// If not provided and TLS is enabled, a self-signed cert will be generated
    #[clap(long = "tls-cert")]
    pub tls_cert: Option<String>,

    /// Path to TLS private key file (PEM format)
    /// If not provided and TLS is enabled, a self-signed key will be generated
    #[clap(long = "tls-key")]
    pub tls_key: Option<String>,

    /// Log level (trace, debug, info, warn, error)
    #[clap(long)]
    pub log: Option<String>,

    /// Path to SQLite database file
    #[clap(long = "db-path")]
    pub db_path: Option<String>,

    /// Enable Shadowsocks proxy service
    #[clap(long = "ss-enabled")]
    pub ss_enabled: Option<bool>,

    /// Shadowsocks listen port
    #[clap(long = "ss-port")]
    pub ss_port: Option<u16>,

    /// Shadowsocks encryption method (aes-256-gcm, chacha20-ietf-poly1305)
    #[clap(long = "ss-cipher")]
    pub ss_cipher: Option<String>,

    /// Shadowsocks password
    #[clap(long = "ss-password")]
    pub ss_password: Option<String>,

    /// Enable Trojan proxy service
    #[clap(long = "trojan-enabled")]
    pub trojan_enabled: Option<bool>,

    /// Trojan listen port
    #[clap(long = "trojan-port")]
    pub trojan_port: Option<u16>,

    /// Trojan password
    #[clap(long = "trojan-password")]
    pub trojan_password: Option<String>,

    /// Trojan fallback address for non-Trojan traffic (default: 127.0.0.1:80)
    #[clap(long = "trojan-fallback")]
    pub trojan_fallback: Option<String>,

    /// Enable embedded DNS server
    #[clap(long = "dns-enabled")]
    pub dns_enabled: Option<bool>,

    /// DNS server bind address (default: 0.0.0.0:53)
    #[clap(long = "dns-bind")]
    pub dns_bind: Option<String>,

    /// Tunnel domain suffix (default: tunnel.local)
    #[clap(long = "dns-tunnel-domain")]
    pub dns_tunnel_domain: Option<String>,

    /// Mesh domain suffix (default: mesh.local)
    #[clap(long = "dns-mesh-domain")]
    pub dns_mesh_domain: Option<String>,

    // Reverse Proxy options
    /// Enable reverse proxy service
    #[clap(long = "reverse-proxy-enabled")]
    pub reverse_proxy_enabled: Option<bool>,

    /// Maximum connections for reverse proxy
    #[clap(long = "reverse-proxy-max-connections")]
    pub reverse_proxy_max_connections: Option<u32>,

    /// Connection timeout in seconds for reverse proxy
    #[clap(long = "reverse-proxy-connection-timeout")]
    pub reverse_proxy_connection_timeout: Option<u64>,

    /// Buffer size for reverse proxy
    #[clap(long = "reverse-proxy-buffer-size")]
    pub reverse_proxy_buffer_size: Option<usize>,

    // ACME options
    /// Enable ACME certificate management
    #[clap(long = "acme-enabled")]
    pub acme_enabled: Option<bool>,

    /// ACME server URL
    #[clap(long = "acme-server-url")]
    pub acme_server_url: Option<String>,

    /// Certificate storage directory
    #[clap(long = "acme-cert-dir")]
    pub acme_cert_dir: Option<String>,

    /// Enable automatic certificate renewal
    #[clap(long = "acme-auto-renew")]
    pub acme_auto_renew: Option<bool>,

    /// Renewal check interval in hours
    #[clap(long = "acme-renewal-check-interval")]
    pub acme_renewal_check_interval: Option<u64>,

    /// Days before expiry to trigger renewal
    #[clap(long = "acme-renewal-days-before-expiry")]
    pub acme_renewal_days_before_expiry: Option<u64>,

    /// Contact email for ACME/Let's Encrypt
    #[clap(long = "acme-email")]
    pub acme_email: Option<String>,

    /// Agree to Let's Encrypt Terms of Service
    #[clap(long = "acme-tos-agreed")]
    pub acme_tos_agreed: Option<bool>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct ServerConfigFile {
    pub control_addr: Option<String>,
    pub api_addr: Option<String>,
    pub admin_password: Option<String>,
    pub jwt_secret: Option<String>,
    pub client_auth_token: Option<String>,
    pub tls: Option<bool>,
    pub tls_cert: Option<String>,
    pub tls_key: Option<String>,
    pub log: Option<String>,
    pub db_path: Option<String>,
    pub ss_enabled: Option<bool>,
    pub ss_port: Option<u16>,
    pub ss_cipher: Option<String>,
    pub ss_password: Option<String>,
    pub trojan_enabled: Option<bool>,
    pub trojan_port: Option<u16>,
    pub trojan_password: Option<String>,
    pub trojan_fallback: Option<String>,
    pub dns_enabled: Option<bool>,
    pub dns_bind: Option<String>,
    pub dns_tunnel_domain: Option<String>,
    pub dns_mesh_domain: Option<String>,
    pub reverse_proxy_enabled: Option<bool>,
    pub reverse_proxy_max_connections: Option<u32>,
    pub reverse_proxy_connection_timeout: Option<u64>,
    pub reverse_proxy_buffer_size: Option<usize>,
    pub acme_enabled: Option<bool>,
    pub acme_server_url: Option<String>,
    pub acme_cert_dir: Option<String>,
    pub acme_auto_renew: Option<bool>,
    pub acme_renewal_check_interval: Option<u64>,
    pub acme_renewal_days_before_expiry: Option<u64>,
    pub acme_email: Option<String>,
    pub acme_tos_agreed: Option<bool>,
}

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

impl ServerConfig {
    pub fn load() -> Result<Self, String> {
        let cli = ServerCli::parse();
        Self::from_cli(cli)
    }

    pub fn from_cli(cli: ServerCli) -> Result<Self, String> {
        let mut config = Self::default();

        // 1. Load from config file if specified
        if let Some(config_path) = &cli.config_file {
            if Path::new(config_path).exists() {
                let file_config: ServerConfigFile = Figment::new()
                    .merge(Toml::file(config_path))
                    .extract()
                    .map_err(|e| format!("Failed to parse config file: {}", e))?;

                if let Some(v) = file_config.control_addr {
                    config.control_addr = v;
                }
                if let Some(v) = file_config.api_addr {
                    config.api_addr = v;
                }
                if let Some(v) = file_config.admin_password {
                    config.admin_password = Some(v);
                }
                if let Some(v) = file_config.jwt_secret {
                    config.jwt_secret = Some(v);
                }
                if let Some(v) = file_config.client_auth_token {
                    config.client_auth_token = Some(v);
                }
                if let Some(v) = file_config.tls {
                    config.tls = v;
                }
                if let Some(v) = file_config.tls_cert {
                    config.tls_cert = v;
                }
                if let Some(v) = file_config.tls_key {
                    config.tls_key = v;
                }
                if let Some(v) = file_config.log {
                    config.log = v;
                }
                if let Some(v) = file_config.db_path {
                    config.db_path = v;
                }
                if let Some(v) = file_config.ss_enabled {
                    config.ss_enabled = v;
                }
                if let Some(v) = file_config.ss_port {
                    config.ss_port = Some(v);
                }
                if let Some(v) = file_config.ss_cipher {
                    config.ss_cipher = Some(v);
                }
                if let Some(v) = file_config.ss_password {
                    config.ss_password = Some(v);
                }
                if let Some(v) = file_config.trojan_enabled {
                    config.trojan_enabled = v;
                }
                if let Some(v) = file_config.trojan_port {
                    config.trojan_port = Some(v);
                }
                if let Some(v) = file_config.trojan_password {
                    config.trojan_password = Some(v);
                }
                if let Some(v) = file_config.trojan_fallback {
                    config.trojan_fallback = v;
                }
                if let Some(v) = file_config.dns_enabled {
                    config.dns_enabled = v;
                }
                if let Some(v) = file_config.dns_bind {
                    config.dns_bind = v;
                }
                if let Some(v) = file_config.dns_tunnel_domain {
                    config.dns_tunnel_domain = v;
                }
                if let Some(v) = file_config.dns_mesh_domain {
                    config.dns_mesh_domain = v;
                }
                // Reverse Proxy config file values
                if let Some(v) = file_config.reverse_proxy_enabled {
                    config.reverse_proxy_enabled = v;
                }
                if let Some(v) = file_config.reverse_proxy_max_connections {
                    config.reverse_proxy_max_connections = v;
                }
                if let Some(v) = file_config.reverse_proxy_connection_timeout {
                    config.reverse_proxy_connection_timeout = v;
                }
                if let Some(v) = file_config.reverse_proxy_buffer_size {
                    config.reverse_proxy_buffer_size = v;
                }
                // ACME config file values
                if let Some(v) = file_config.acme_enabled {
                    config.acme_enabled = v;
                }
                if let Some(v) = file_config.acme_server_url {
                    config.acme_server_url = v;
                }
                if let Some(v) = file_config.acme_cert_dir {
                    config.acme_cert_dir = v;
                }
                if let Some(v) = file_config.acme_auto_renew {
                    config.acme_auto_renew = v;
                }
                if let Some(v) = file_config.acme_renewal_check_interval {
                    config.acme_renewal_check_interval = v;
                }
                if let Some(v) = file_config.acme_renewal_days_before_expiry {
                    config.acme_renewal_days_before_expiry = v;
                }
                if let Some(v) = file_config.acme_email {
                    config.acme_email = Some(v);
                }
                if let Some(v) = file_config.acme_tos_agreed {
                    config.acme_tos_agreed = v;
                }
            } else {
                return Err(format!("Config file not found: {}", config_path));
            }
        }

        // 2. Load from environment variables (override file)
        if let Ok(v) = std::env::var("CONTROL_ADDR") {
            config.control_addr = v;
        }
        if let Ok(v) = std::env::var("API_BIND") {
            config.api_addr = v;
        }
        if let Ok(v) = std::env::var("ADMIN_PASSWORD") {
            config.admin_password = Some(v);
        }
        if let Ok(v) = std::env::var("JWT_SECRET") {
            config.jwt_secret = Some(v);
        }
        if let Ok(v) = std::env::var("CLIENT_AUTH_TOKEN") {
            config.client_auth_token = Some(v);
        }
        if let Ok(v) = std::env::var("TLS") {
            config.tls = v.to_lowercase() == "true" || v == "1";
        }
        if let Ok(v) = std::env::var("TLS_CERT") {
            config.tls_cert = v;
        }
        if let Ok(v) = std::env::var("TLS_KEY") {
            config.tls_key = v;
        }
        if let Ok(v) = std::env::var("LOG_LEVEL") {
            config.log = v;
        }
        if let Ok(v) = std::env::var("DB_PATH") {
            config.db_path = v;
        }

        // Environment variables for Shadowsocks
        if let Ok(v) = std::env::var("SS_ENABLED") {
            config.ss_enabled = v.to_lowercase() == "true" || v == "1";
        }
        if let Ok(v) = std::env::var("SS_PORT") {
            if let Ok(port) = v.parse::<u16>() {
                config.ss_port = Some(port);
            }
        }
        if let Ok(v) = std::env::var("SS_CIPHER") {
            config.ss_cipher = Some(v);
        }
        if let Ok(v) = std::env::var("SS_PASSWORD") {
            config.ss_password = Some(v);
        }

        // Environment variables for Trojan
        if let Ok(v) = std::env::var("TROJAN_ENABLED") {
            config.trojan_enabled = v.to_lowercase() == "true" || v == "1";
        }
        if let Ok(v) = std::env::var("TROJAN_PORT") {
            if let Ok(port) = v.parse::<u16>() {
                config.trojan_port = Some(port);
            }
        }
        if let Ok(v) = std::env::var("TROJAN_PASSWORD") {
            config.trojan_password = Some(v);
        }
        if let Ok(v) = std::env::var("TROJAN_FALLBACK") {
            config.trojan_fallback = v;
        }

        // Environment variables for DNS
        if let Ok(v) = std::env::var("DNS_ENABLED") {
            config.dns_enabled = v.to_lowercase() == "true" || v == "1";
        }
        if let Ok(v) = std::env::var("DNS_BIND") {
            config.dns_bind = v;
        }
        if let Ok(v) = std::env::var("DNS_TUNNEL_DOMAIN") {
            config.dns_tunnel_domain = v;
        }
        if let Ok(v) = std::env::var("DNS_MESH_DOMAIN") {
            config.dns_mesh_domain = v;
        }

        // Environment variables for Reverse Proxy
        if let Ok(v) = std::env::var("REVERSE_PROXY_ENABLED") {
            config.reverse_proxy_enabled = v.to_lowercase() == "true" || v == "1";
        }
        if let Ok(v) = std::env::var("REVERSE_PROXY_MAX_CONNECTIONS") {
            if let Ok(val) = v.parse::<u32>() {
                config.reverse_proxy_max_connections = val;
            }
        }
        if let Ok(v) = std::env::var("REVERSE_PROXY_CONNECTION_TIMEOUT") {
            if let Ok(val) = v.parse::<u64>() {
                config.reverse_proxy_connection_timeout = val;
            }
        }
        if let Ok(v) = std::env::var("REVERSE_PROXY_BUFFER_SIZE") {
            if let Ok(val) = v.parse::<usize>() {
                config.reverse_proxy_buffer_size = val;
            }
        }

        // Environment variables for ACME
        if let Ok(v) = std::env::var("ACME_ENABLED") {
            config.acme_enabled = v.to_lowercase() == "true" || v == "1";
        }
        if let Ok(v) = std::env::var("ACME_SERVER_URL") {
            config.acme_server_url = v;
        }
        if let Ok(v) = std::env::var("ACME_CERT_DIR") {
            config.acme_cert_dir = v;
        }
        if let Ok(v) = std::env::var("ACME_AUTO_RENEW") {
            config.acme_auto_renew = v.to_lowercase() == "true" || v == "1";
        }
        if let Ok(v) = std::env::var("ACME_RENEWAL_CHECK_INTERVAL") {
            if let Ok(val) = v.parse::<u64>() {
                config.acme_renewal_check_interval = val;
            }
        }
        if let Ok(v) = std::env::var("ACME_RENEWAL_DAYS_BEFORE_EXPIRY") {
            if let Ok(val) = v.parse::<u64>() {
                config.acme_renewal_days_before_expiry = val;
            }
        }
        if let Ok(v) = std::env::var("ACME_EMAIL") {
            config.acme_email = Some(v);
        }
        if let Ok(v) = std::env::var("ACME_TOS_AGREED") {
            config.acme_tos_agreed = v.to_lowercase() == "true" || v == "1";
        }

        // 3. Command line arguments (highest priority)
        if let Some(v) = cli.control_addr {
            config.control_addr = v;
        }
        if let Some(v) = cli.api_addr {
            config.api_addr = v;
        }
        if let Some(v) = cli.admin_password {
            config.admin_password = Some(v);
        }
        if let Some(v) = cli.jwt_secret {
            config.jwt_secret = Some(v);
        }
        if let Some(v) = cli.client_auth_token {
            config.client_auth_token = Some(v);
        }
        if let Some(v) = cli.tls {
            config.tls = v;
        }
        if let Some(v) = cli.tls_cert {
            config.tls_cert = v;
        }
        if let Some(v) = cli.tls_key {
            config.tls_key = v;
        }
        if let Some(v) = cli.log {
            config.log = v;
        }
        if let Some(v) = cli.db_path {
            config.db_path = v;
        }
        if let Some(v) = cli.ss_enabled {
            config.ss_enabled = v;
        }
        if let Some(v) = cli.ss_port {
            config.ss_port = Some(v);
        }
        if let Some(v) = cli.ss_cipher {
            config.ss_cipher = Some(v);
        }
        if let Some(v) = cli.ss_password {
            config.ss_password = Some(v);
        }
        if let Some(v) = cli.trojan_enabled {
            config.trojan_enabled = v;
        }
        if let Some(v) = cli.trojan_port {
            config.trojan_port = Some(v);
        }
        if let Some(v) = cli.trojan_password {
            config.trojan_password = Some(v);
        }
        if let Some(v) = cli.trojan_fallback {
            config.trojan_fallback = v;
        }

        // DNS command line arguments
        if let Some(v) = cli.dns_enabled {
            config.dns_enabled = v;
        }
        if let Some(v) = cli.dns_bind {
            config.dns_bind = v;
        }
        if let Some(v) = cli.dns_tunnel_domain {
            config.dns_tunnel_domain = v;
        }
        if let Some(v) = cli.dns_mesh_domain {
            config.dns_mesh_domain = v;
        }

        // Reverse Proxy command line arguments
        if let Some(v) = cli.reverse_proxy_enabled {
            config.reverse_proxy_enabled = v;
        }
        if let Some(v) = cli.reverse_proxy_max_connections {
            config.reverse_proxy_max_connections = v;
        }
        if let Some(v) = cli.reverse_proxy_connection_timeout {
            config.reverse_proxy_connection_timeout = v;
        }
        if let Some(v) = cli.reverse_proxy_buffer_size {
            config.reverse_proxy_buffer_size = v;
        }

        // ACME command line arguments
        if let Some(v) = cli.acme_enabled {
            config.acme_enabled = v;
        }
        if let Some(v) = cli.acme_server_url {
            config.acme_server_url = v;
        }
        if let Some(v) = cli.acme_cert_dir {
            config.acme_cert_dir = v;
        }
        if let Some(v) = cli.acme_auto_renew {
            config.acme_auto_renew = v;
        }
        if let Some(v) = cli.acme_renewal_check_interval {
            config.acme_renewal_check_interval = v;
        }
        if let Some(v) = cli.acme_renewal_days_before_expiry {
            config.acme_renewal_days_before_expiry = v;
        }
        if let Some(v) = cli.acme_email {
            config.acme_email = Some(v);
        }
        if let Some(v) = cli.acme_tos_agreed {
            config.acme_tos_agreed = v;
        }

        // Validate Shadowsocks configuration
        if config.ss_enabled {
            if config.ss_port.is_none() {
                return Err("ss_port is required when ss_enabled is true".to_string());
            }
            if config.ss_cipher.is_none() {
                return Err("ss_cipher is required when ss_enabled is true".to_string());
            }
            if config.ss_password.is_none() {
                return Err("ss_password is required when ss_enabled is true".to_string());
            }
            // Validate cipher method
            let cipher = config.ss_cipher.as_ref().unwrap();
            if cipher != "aes-256-gcm" && cipher != "chacha20-ietf-poly1305" {
                return Err(format!(
                    "Unsupported cipher: {}. Supported: aes-256-gcm, chacha20-ietf-poly1305",
                    cipher
                ));
            }
        }

        // Validate Trojan configuration
        if config.trojan_enabled {
            if config.trojan_port.is_none() {
                return Err("trojan_port is required when trojan_enabled is true".to_string());
            }
            if config.trojan_password.is_none() {
                return Err("trojan_password is required when trojan_enabled is true".to_string());
            }
            // Trojan requires TLS
            if !config.tls {
                return Err(
                    "TLS must be enabled for Trojan protocol (trojan requires TLS transport)"
                        .to_string(),
                );
            }
        }

        // Validate DNS configuration
        if config.dns_enabled {
            config
                .dns_bind
                .parse::<std::net::SocketAddr>()
                .map_err(|e| format!("Invalid dns_bind '{}': {}", config.dns_bind, e))?;
        }

        // Validate ACME configuration
        if config.acme_enabled {
            if config.acme_email.is_none() {
                return Err("acme_email is required when acme_enabled is true".to_string());
            }
            if !config.acme_tos_agreed {
                return Err(
                    "acme_tos_agreed must be true when acme_enabled is true (Let's Encrypt ToS)"
                        .to_string(),
                );
            }
        }

        Ok(config)
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
        assert_eq!(config.tls, false);
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
