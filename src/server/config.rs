use clap::Parser;
use figment::{Figment, providers::{Toml, Format}};
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
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            control_addr: "0.0.0.0:8080".to_string(),
            api_addr: "0.0.0.0:3000".to_string(),
            admin_password: None,
            jwt_secret: None,
            client_auth_token: None,
            tls: true,  // TLS enabled by default for security
            tls_cert: "./data/tls/cert.pem".to_string(),
            tls_key: "./data/tls/key.pem".to_string(),
            log: "info".to_string(),
            db_path: "./data/rust-tunnel.db".to_string(),
            ss_enabled: false,
            ss_port: None,
            ss_cipher: None,
            ss_password: None,
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
                return Err(format!("Unsupported cipher: {}. Supported: aes-256-gcm, chacha20-ietf-poly1305", cipher));
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
        assert!(config.tls);  // TLS enabled by default
        assert_eq!(config.tls_cert, "./data/tls/cert.pem");
        assert_eq!(config.tls_key, "./data/tls/key.pem");
        assert!(!config.ss_enabled);
        assert!(config.ss_port.is_none());
        assert!(config.ss_cipher.is_none());
        assert!(config.ss_password.is_none());
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
        };

        let result = ServerConfig::from_cli(cli);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Unsupported cipher"));
    }
}
