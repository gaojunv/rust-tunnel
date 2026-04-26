use clap::Parser;

/// Server endpoint for rust-tunnel intranet penetration tool
#[derive(Parser, Debug, Clone)]
pub struct ServerConfig {
    /// Address to listen for control connections from clients
    /// Format: 0.0.0.0:8080
    #[clap(long = "bind", default_value = "0.0.0.0:8080")]
    pub control_addr: String,

    /// Address to listen for HTTP API connections
    /// Format: 0.0.0.0:3000
    #[clap(long = "api-bind", default_value = "0.0.0.0:3000")]
    pub api_addr: String,

    /// Password for web interface authentication (optional)
    #[clap(long = "admin-password")]
    pub admin_password: Option<String>,

    /// Secret key for JWT tokens (auto-generated if not provided)
    #[clap(long = "jwt-secret")]
    pub jwt_secret: Option<String>,

    /// Log level (trace, debug, info, warn, error)
    #[clap(long, default_value = "info")]
    pub log: String,

    /// Path to SQLite database file (default: ./data/rust-tunnel.db)
    #[clap(long = "db-path", default_value = "./data/rust-tunnel.db")]
    pub db_path: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        // Test that defaults work by parsing empty args
        let config = ServerConfig::try_parse_from(vec!["rust-tunnel-server"]);
        assert!(config.is_ok());

        let config = config.unwrap();
        assert_eq!(config.control_addr, "0.0.0.0:8080");
        assert_eq!(config.api_addr, "0.0.0.0:3000");
        assert_eq!(config.log, "info");
        assert!(config.admin_password.is_none());
        assert!(config.jwt_secret.is_none());
    }

    #[test]
    fn test_custom_config() {
        let config = ServerConfig::try_parse_from(vec![
            "rust-tunnel-server",
            "--bind", "127.0.0.1:9000",
            "--api-bind", "127.0.0.1:9001",
            "--admin-password", "secret123",
            "--jwt-secret", "test-secret",
            "--log", "debug",
        ]);
        assert!(config.is_ok());

        let config = config.unwrap();
        assert_eq!(config.control_addr, "127.0.0.1:9000");
        assert_eq!(config.api_addr, "127.0.0.1:9001");
        assert_eq!(config.admin_password, Some("secret123".into()));
        assert_eq!(config.jwt_secret, Some("test-secret".into()));
        assert_eq!(config.log, "debug");
    }

    #[test]
    fn test_config_clone() {
        let config = ServerConfig::try_parse_from(vec![
            "rust-tunnel-server",
            "--bind", "127.0.0.1:9000",
        ]).unwrap();

        let cloned = config.clone();
        assert_eq!(config.control_addr, cloned.control_addr);
        assert_eq!(config.api_addr, cloned.api_addr);
    }
}
