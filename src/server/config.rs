use clap::Parser;

/// Server endpoint for rust-tunnel intranet penetration tool
#[derive(Parser, Debug, Clone)]
pub struct ServerConfig {
    /// Address to listen for control connections from clients
    /// Format: 0.0.0.0:8080
    #[clap(long = "bind", default_value = "0.0.0.0:8080")]
    pub control_addr: String,

    /// Log level (trace, debug, info, warn, error)
    #[clap(long, default_value = "info")]
    pub log: String,
}
