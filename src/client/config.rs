use clap::Parser;

/// Client endpoint for rust-tunnel intranet penetration tool
#[derive(Parser, Debug, Clone)]
pub struct ClientConfig {
    /// Server address to connect (e.g., server-ip:8000)
    #[clap(long)]
    pub server_addr: String,

    /// Local address to forward to (e.g., 127.0.0.1:8080)
    #[clap(long, default_value = "127.0.0.1:8080")]
    pub local_addr: String,

    /// Remote port on server to expose the service
    #[clap(long)]
    pub remote_port: u16,
}
