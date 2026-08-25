use super::cli::ServerCli;
use super::file::ServerConfigFile;
use super::ServerConfig;
use clap::Parser;
use figment::{
    providers::{Format, Toml},
    Figment,
};
use std::path::Path;

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
                // API TLS config file values
                if let Some(v) = file_config.api_tls {
                    config.api_tls = v;
                }
                if let Some(v) = file_config.api_domain {
                    config.api_domain = Some(v);
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

        // Environment variables for API TLS
        if let Ok(v) = std::env::var("API_TLS") {
            config.api_tls = v.to_lowercase() == "true" || v == "1";
        }
        if let Ok(v) = std::env::var("API_DOMAIN") {
            config.api_domain = Some(v);
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

        // API TLS command line arguments
        if let Some(v) = cli.api_tls {
            config.api_tls = v;
        }
        if let Some(v) = cli.api_domain {
            config.api_domain = Some(v);
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
            let Some(cipher) = config.ss_cipher.as_ref() else {
                return Err("ss_cipher is required when ss_enabled is true".to_string());
            };
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
