use clap::Parser;

/// Server endpoint for rust-tunnel intranet penetration tool
#[derive(Parser, Debug, Clone)]
pub struct ServerConfig {
    /// Address to listen for control connections from clients
    /// Format: 0.0.0.0:8000
    #[clap(long, default_value = "0.0.0.0:8000")]
    pub control_addr: String,
}
