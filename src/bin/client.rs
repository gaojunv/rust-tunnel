use rust_tunnel::common::{init_logging_with_level, TunnelResult};
use rust_tunnel::client::{ClientConfig, control};

#[tokio::main]
async fn main() -> TunnelResult<()> {
    let config = ClientConfig::load()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    init_logging_with_level(&config.log);
    let forwards = config.parse_forwards()
        .expect("Invalid forward configuration");
    tracing::info!(
        "Starting rust-tunnel client, connecting to server {}, {} forward rules configured",
        config.server,
        forwards.len()
    );
    control::run_client(config, forwards).await?;
    Ok(())
}
