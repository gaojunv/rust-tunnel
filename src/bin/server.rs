use clap::Parser;
use rust_tunnel::common::{init_logging_with_level, TunnelResult};
use rust_tunnel::server::{ServerConfig, control};

#[tokio::main]
async fn main() -> TunnelResult<()> {
    let config = ServerConfig::parse();
    init_logging_with_level(&config.log);
    tracing::info!("Starting rust-tunnel server on {}", config.control_addr);
    control::run_server(config).await?;
    Ok(())
}
