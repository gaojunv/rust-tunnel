use clap::{ArgAction, Parser};
use figment::{
    providers::{Format, Toml},
    Figment,
};
use serde::Deserialize;
use std::path::Path;

/// Client endpoint for rust-tunnel intranet penetration tool
#[derive(Parser, Debug, Clone)]
pub struct ClientCli {
    /// Path to configuration file (TOML format)
    #[clap(long = "config")]
    pub config_file: Option<String>,

    /// Server address to connect (e.g., server-ip:8080)
    #[clap(long)]
    pub server: Option<String>,

    /// Forward rule: REMOTE_PORT:LOCAL_HOST:LOCAL_PORT
    /// Example: 8080:localhost:80
    #[clap(long = "forward", action = ArgAction::Append)]
    pub forwards: Vec<String>,

    /// Authentication token for server (required if server enables client auth)
    #[clap(long = "auth-token")]
    pub auth_token: Option<String>,

    /// Enable TLS encryption for control channel (should match server setting)
    #[clap(long = "tls")]
    pub tls: Option<bool>,

    /// TLS server name for SNI (Server Name Indication)
    /// If not provided, the hostname from server address will be used
    #[clap(long = "tls-server-name")]
    pub tls_server_name: Option<String>,

    /// Skip TLS certificate verification (insecure, for self-signed certs)
    /// This enables TOFU (Trust On First Use) mode for self-signed certificates
    #[clap(long = "tls-insecure")]
    pub tls_insecure: Option<bool>,

    /// Mesh network ID to join
    #[clap(long = "mesh")]
    pub mesh: Option<String>,

    /// Mesh client display name in the mesh network
    #[clap(long = "mesh-name")]
    pub mesh_name: Option<String>,

    /// Mesh service definitions: NAME:PROTOCOL:LOCAL_ADDR
    /// Example: db:mysql:localhost:3306
    #[clap(long = "mesh-service", action = ArgAction::Append)]
    pub mesh_services: Vec<String>,

    /// Log level (trace, debug, info, warn, error)
    #[clap(long)]
    pub log: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct ClientConfigFile {
    pub server: Option<String>,
    pub forwards: Option<Vec<String>>,
    pub auth_token: Option<String>,
    pub tls: Option<bool>,
    pub tls_server_name: Option<String>,
    pub tls_insecure: Option<bool>,
    pub mesh: Option<String>,
    pub mesh_name: Option<String>,
    pub mesh_services: Option<Vec<String>>,
    pub log: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub server: String,
    pub forwards: Vec<String>,
    /// Mesh network ID to join
    pub mesh: Option<String>,
    /// Mesh client display name in the mesh network
    pub mesh_name: Option<String>,
    /// Mesh service definitions: NAME:PROTOCOL:LOCAL_ADDR
    pub mesh_services: Vec<String>,
    /// Authentication token for server (required if server enables client auth)
    pub auth_token: Option<String>,
    /// Enable TLS encryption for control channel
    pub tls: bool,
    /// TLS server name for SNI
    pub tls_server_name: Option<String>,
    /// Skip TLS certificate verification (insecure, for self-signed certs)
    pub tls_insecure: bool,
    pub log: String,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            server: String::new(),
            forwards: Vec::new(),
            mesh: None,
            mesh_name: None,
            mesh_services: Vec::new(),
            auth_token: None,
            tls: true, // TLS enabled by default for security
            tls_server_name: None,
            tls_insecure: true, // Accept self-signed certs by default (TOFU mode)
            log: "info".to_string(),
        }
    }
}

impl ClientConfig {
    pub fn load() -> Result<Self, String> {
        let cli = ClientCli::parse();
        Self::from_cli(cli)
    }

    pub fn from_cli(cli: ClientCli) -> Result<Self, String> {
        let mut config = Self::default();

        // 1. Load from config file if specified
        if let Some(config_path) = &cli.config_file {
            if Path::new(config_path).exists() {
                let file_config: ClientConfigFile = Figment::new()
                    .merge(Toml::file(config_path))
                    .extract()
                    .map_err(|e| format!("Failed to parse config file: {}", e))?;

                if let Some(v) = file_config.server {
                    config.server = v;
                }
                if let Some(v) = file_config.forwards {
                    config.forwards = v;
                }
                if let Some(v) = file_config.auth_token {
                    config.auth_token = Some(v);
                }
                if let Some(v) = file_config.tls {
                    config.tls = v;
                }
                if let Some(v) = file_config.tls_server_name {
                    config.tls_server_name = Some(v);
                }
                if let Some(v) = file_config.tls_insecure {
                    config.tls_insecure = v;
                }
                if let Some(v) = file_config.mesh {
                    config.mesh = Some(v);
                }
                if let Some(v) = file_config.mesh_name {
                    config.mesh_name = Some(v);
                }
                if let Some(v) = file_config.mesh_services {
                    config.mesh_services = v;
                }
                if let Some(v) = file_config.log {
                    config.log = v;
                }
            } else {
                return Err(format!("Config file not found: {}", config_path));
            }
        }

        // 2. Load from environment variables (override file)
        if let Ok(v) = std::env::var("SERVER_ADDR") {
            config.server = v;
        }
        if let Ok(v) = std::env::var("FORWARDS") {
            // Split comma-separated forwards
            config.forwards = v.split(',').map(|s| s.trim().to_string()).collect();
        }
        if let Ok(v) = std::env::var("AUTH_TOKEN") {
            config.auth_token = Some(v);
        }
        if let Ok(v) = std::env::var("TLS") {
            config.tls = v.to_lowercase() == "true" || v == "1";
        }
        if let Ok(v) = std::env::var("TLS_SERVER_NAME") {
            config.tls_server_name = Some(v);
        }
        if let Ok(v) = std::env::var("TLS_INSECURE") {
            config.tls_insecure = v.to_lowercase() == "true" || v == "1";
        }
        if let Ok(v) = std::env::var("LOG_LEVEL") {
            config.log = v;
        }
        if let Ok(v) = std::env::var("MESH_ID") {
            config.mesh = Some(v);
        }
        if let Ok(v) = std::env::var("MESH_NAME") {
            config.mesh_name = Some(v);
        }
        if let Ok(v) = std::env::var("MESH_SERVICES") {
            config.mesh_services = v.split(',').map(|s| s.trim().to_string()).collect();
        }

        // 3. Command line arguments (highest priority)
        if let Some(v) = cli.server {
            config.server = v;
        }
        if !cli.forwards.is_empty() {
            config.forwards = cli.forwards;
        }
        if let Some(v) = cli.auth_token {
            config.auth_token = Some(v);
        }
        if let Some(v) = cli.tls {
            config.tls = v;
        }
        if let Some(v) = cli.tls_server_name {
            config.tls_server_name = Some(v);
        }
        if let Some(v) = cli.tls_insecure {
            config.tls_insecure = v;
        }
        if let Some(v) = cli.log {
            config.log = v;
        }
        if let Some(v) = cli.mesh {
            config.mesh = Some(v);
        }
        if let Some(v) = cli.mesh_name {
            config.mesh_name = Some(v);
        }
        if !cli.mesh_services.is_empty() {
            config.mesh_services = cli.mesh_services;
        }

        // Validate required fields
        if config.server.is_empty() {
            return Err(
                "Server address is required. Use --server, SERVER_ADDR env, or set in config file"
                    .to_string(),
            );
        }

        Ok(config)
    }
}

#[derive(Debug, Clone)]
pub struct ForwardRule {
    pub remote_port: u16,
    pub local_addr: String,
    /// Custom DNS name for this forward (e.g. "webapp" -> webapp.tunnel.local)
    pub dns_name: Option<String>,
}

impl ClientConfig {
    pub fn parse_forwards(&self) -> Result<Vec<ForwardRule>, String> {
        let mut rules = Vec::new();
        for forward in &self.forwards {
            // Use rsplitn to split from the end: REMOTE_PORT:LOCAL_HOST:LOCAL_PORT
            // This allows LOCAL_HOST to contain colons (IPv6)
            let mut parts = forward.rsplitn(2, ':');
            let local_port_str = parts.next().ok_or_else(|| {
                format!(
                    "Invalid forward format: '{}', expected: REMOTE_PORT:LOCAL_HOST:LOCAL_PORT",
                    forward
                )
            })?;
            let remaining = parts.next().ok_or_else(|| {
                format!(
                    "Invalid forward format: '{}', expected: REMOTE_PORT:LOCAL_HOST:LOCAL_PORT",
                    forward
                )
            })?;

            let mut parts2 = remaining.splitn(2, ':');
            let remote_port_str = parts2.next().unwrap();
            let local_host = parts2.next().ok_or_else(|| {
                format!(
                    "Invalid forward format: '{}', expected: REMOTE_PORT:LOCAL_HOST:LOCAL_PORT",
                    forward
                )
            })?;

            let remote_port = remote_port_str
                .parse::<u16>()
                .map_err(|e| format!("Invalid remote port: {}", e))?;
            let local_port = local_port_str
                .parse::<u16>()
                .map_err(|e| format!("Invalid local port: {}", e))?;
            let local_addr = format!("{}:{}", local_host, local_port);
            rules.push(ForwardRule {
                remote_port,
                local_addr,
                dns_name: None,
            });
        }
        Ok(rules)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = ClientConfig::default();
        assert_eq!(config.server, "");
        assert!(config.forwards.is_empty());
        assert!(config.auth_token.is_none());
        assert!(config.tls); // TLS enabled by default
        assert!(config.tls_insecure); // Accept self-signed certs by default
        assert_eq!(config.log, "info");
    }

    #[test]
    fn test_config_from_cli_no_file() {
        let cli = ClientCli {
            config_file: None,
            server: Some("localhost:8080".to_string()),
            forwards: vec!["8080:localhost:80".to_string()],
            auth_token: Some("secret-token".to_string()),
            tls: Some(true),
            tls_server_name: Some("tunnel.example.com".to_string()),
            tls_insecure: Some(true),
            mesh: None,
            mesh_name: None,
            mesh_services: vec![],
            log: Some("debug".to_string()),
        };

        let config = ClientConfig::from_cli(cli).unwrap();
        assert_eq!(config.server, "localhost:8080");
        assert_eq!(config.forwards, vec!["8080:localhost:80"]);
        assert_eq!(config.auth_token, Some("secret-token".to_string()));
        assert!(config.tls);
        assert_eq!(
            config.tls_server_name,
            Some("tunnel.example.com".to_string())
        );
        assert!(config.tls_insecure);
        assert_eq!(config.log, "debug");
    }

    #[test]
    fn test_config_missing_server() {
        let cli = ClientCli {
            config_file: None,
            server: None,
            forwards: vec![],
            auth_token: None,
            tls: None,
            tls_server_name: None,
            tls_insecure: None,
            mesh: None,
            mesh_name: None,
            mesh_services: vec![],
            log: None,
        };

        let result = ClientConfig::from_cli(cli);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Server address is required"));
    }

    #[test]
    fn test_parse_forwards_single() {
        let config = ClientConfig {
            server: "localhost:8080".into(),
            forwards: vec!["8080:localhost:80".into()],
            auth_token: None,
            tls: true,
            tls_server_name: None,
            tls_insecure: true,
            mesh: None,
            mesh_name: None,
            mesh_services: vec![],
            log: "info".into(),
        };

        let rules = config.parse_forwards().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].remote_port, 8080);
        assert_eq!(rules[0].local_addr, "localhost:80");
    }

    #[test]
    fn test_parse_forwards_multiple() {
        let config = ClientConfig {
            server: "localhost:8080".into(),
            forwards: vec!["8080:localhost:80".into(), "9000:127.0.0.1:3000".into()],
            auth_token: None,
            tls: true,
            tls_server_name: None,
            tls_insecure: true,
            mesh: None,
            mesh_name: None,
            mesh_services: vec![],
            log: "info".into(),
        };

        let rules = config.parse_forwards().unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].remote_port, 8080);
        assert_eq!(rules[0].local_addr, "localhost:80");
        assert_eq!(rules[1].remote_port, 9000);
        assert_eq!(rules[1].local_addr, "127.0.0.1:3000");
    }

    #[test]
    fn test_parse_forwards_empty() {
        let config = ClientConfig {
            server: "localhost:8080".into(),
            forwards: vec![],
            auth_token: None,
            tls: true,
            tls_server_name: None,
            tls_insecure: true,
            mesh: None,
            mesh_name: None,
            mesh_services: vec![],
            log: "info".into(),
        };

        let rules = config.parse_forwards().unwrap();
        assert!(rules.is_empty());
    }

    #[test]
    fn test_parse_forwards_ipv6() {
        let config = ClientConfig {
            server: "localhost:8080".into(),
            forwards: vec!["8080:::1:80".into()],
            auth_token: None,
            tls: true,
            tls_server_name: None,
            tls_insecure: true,
            mesh: None,
            mesh_name: None,
            mesh_services: vec![],
            log: "info".into(),
        };

        let rules = config.parse_forwards().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].remote_port, 8080);
        assert_eq!(rules[0].local_addr, "::1:80");
    }

    #[test]
    fn test_parse_forwards_invalid_format() {
        let config = ClientConfig {
            server: "localhost:8080".into(),
            forwards: vec!["invalid".into()],
            auth_token: None,
            tls: true,
            tls_server_name: None,
            tls_insecure: true,
            mesh: None,
            mesh_name: None,
            mesh_services: vec![],
            log: "info".into(),
        };

        let result = config.parse_forwards();
        assert!(result.is_err());
    }

    #[test]
    fn test_config_clone() {
        let config = ClientConfig {
            server: "localhost:8080".into(),
            forwards: vec!["8080:localhost:80".into()],
            auth_token: Some("secret".to_string()),
            tls: true,
            tls_server_name: Some("test-server".to_string()),
            tls_insecure: true,
            mesh: Some("mesh-net".to_string()),
            mesh_name: Some("my-client".to_string()),
            mesh_services: vec!["db:mysql:localhost:3306".to_string()],
            log: "debug".into(),
        };

        let cloned = config.clone();
        assert_eq!(config.server, cloned.server);
        assert_eq!(config.forwards, cloned.forwards);
        assert_eq!(config.auth_token, cloned.auth_token);
        assert_eq!(config.tls, cloned.tls);
        assert_eq!(config.tls_server_name, cloned.tls_server_name);
        assert_eq!(config.tls_insecure, cloned.tls_insecure);
        assert_eq!(config.mesh, cloned.mesh);
        assert_eq!(config.mesh_name, cloned.mesh_name);
        assert_eq!(config.mesh_services, cloned.mesh_services);
        assert_eq!(config.log, cloned.log);
    }

    #[test]
    fn test_forward_rule_clone() {
        let rule = ForwardRule {
            remote_port: 8080,
            local_addr: "localhost:80".into(),
            dns_name: Some("webapp".to_string()),
        };

        let cloned = rule.clone();
        assert_eq!(rule.remote_port, cloned.remote_port);
        assert_eq!(rule.local_addr, cloned.local_addr);
        assert_eq!(rule.dns_name, cloned.dns_name);
    }

    #[test]
    fn test_config_file_not_found() {
        let cli = ClientCli {
            config_file: Some("/nonexistent/config.toml".to_string()),
            server: Some("test".to_string()),
            forwards: vec![],
            auth_token: None,
            tls: None,
            tls_server_name: None,
            tls_insecure: None,
            mesh: None,
            mesh_name: None,
            mesh_services: vec![],
            log: None,
        };

        let result = ClientConfig::from_cli(cli);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }
}
