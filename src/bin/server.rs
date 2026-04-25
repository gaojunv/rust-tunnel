use clap::Parser;
use rust_tunnel::common::{init_logging, TunnelResult};
use rust_tunnel::server::{ServerConfig, control};

#[tokio::main]
async fn main() -> TunnelResult<()> {
    init_logging();
    let config = ServerConfig::parse();
    tracing::info!("Starting rust-tunnel server on {}", config.control_addr);
    control::run_server(config).await?;
    Ok(())
}
