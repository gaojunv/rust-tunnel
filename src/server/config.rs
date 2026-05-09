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

    /// Log level (trace, debug, info, warn, error)
    #[clap(long)]
    pub log: Option<String>,

    /// Path to SQLite database file
    #[clap(long = "db-path")]
    pub db_path: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct ServerConfigFile {
    pub control_addr: Option<String>,
    pub api_addr: Option<String>,
    pub admin_password: Option<String>,
    pub jwt_secret: Option<String>,
    pub client_auth_token: Option<String>,
    pub log: Option<String>,
    pub db_path: Option<String>,
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
    pub log: String,
    pub db_path: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            control_addr: "0.0.0.0:8080".to_string(),
            api_addr: "0.0.0.0:3000".to_string(),
            admin_password: None,
            jwt_secret: None,
            client_auth_token: None,
            log: "info".to_string(),
            db_path: "./data/rust-tunnel.db".to_string(),
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
                if let Some(v) = file_config.log {
                    config.log = v;
                }
                if let Some(v) = file_config.db_path {
                    config.db_path = v;
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
        if let Ok(v) = std::env::var("LOG_LEVEL") {
            config.log = v;
        }
        if let Ok(v) = std::env::var("DB_PATH") {
            config.db_path = v;
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
        if let Some(v) = cli.log {
            config.log = v;
        }
        if let Some(v) = cli.db_path {
            config.db_path = v;
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
            log: Some("debug".to_string()),
            db_path: Some("./test.db".to_string()),
        };

        let config = ServerConfig::from_cli(cli).unwrap();
        assert_eq!(config.control_addr, "127.0.0.1:9000");
        assert_eq!(config.api_addr, "127.0.0.1:9001");
        assert_eq!(config.admin_password, Some("secret123".into()));
        assert_eq!(config.jwt_secret, Some("test-secret".into()));
        assert_eq!(config.client_auth_token, Some("client-secret".into()));
        assert_eq!(config.log, "debug");
        assert_eq!(config.db_path, "./test.db");
    }

    #[test]
    fn test_config_clone() {
        let config = ServerConfig {
            control_addr: "127.0.0.1:9000".to_string(),
            api_addr: "127.0.0.1:9001".to_string(),
            admin_password: Some("test".to_string()),
            jwt_secret: Some("secret".to_string()),
            client_auth_token: Some("client-token".to_string()),
            log: "debug".to_string(),
            db_path: "./test.db".to_string(),
        };

        let cloned = config.clone();
        assert_eq!(config.control_addr, cloned.control_addr);
        assert_eq!(config.api_addr, cloned.api_addr);
        assert_eq!(config.admin_password, cloned.admin_password);
        assert_eq!(config.jwt_secret, cloned.jwt_secret);
        assert_eq!(config.client_auth_token, cloned.client_auth_token);
        assert_eq!(config.log, cloned.log);
        assert_eq!(config.db_path, cloned.db_path);
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
            log: None,
            db_path: None,
        };

        let result = ServerConfig::from_cli(cli);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }
}
