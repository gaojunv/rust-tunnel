use clap::Parser;
use rust_tunnel::common::{init_logging, TunnelResult};
use rust_tunnel::client::{ClientConfig, control};

#[tokio::main]
async fn main() -> TunnelResult<()> {
    init_logging();
    let config = ClientConfig::parse();
    tracing::info!(
        "Starting rust-tunnel client, connecting to server {}, forwarding {} -> server:{}",
        config.server_addr,
        config.local_addr,
        config.remote_port
    );
    control::run_client(config).await?;
    Ok(())
}
