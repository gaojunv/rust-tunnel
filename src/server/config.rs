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
}
