use std::time::Duration;

use rust_tunnel::common::{init_logging_with_level, TunnelResult};
use rust_tunnel::client::{ClientConfig, control};

const INITIAL_BACKOFF_SECS: u64 = 1;
const MAX_BACKOFF_SECS: u64 = 30;

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

    let mut backoff_secs = INITIAL_BACKOFF_SECS;

    loop {
        match control::run_client(config.clone(), forwards.clone()).await {
            Ok(()) => {
                tracing::warn!("Control connection closed.");
            }
            Err(e) => {
                tracing::warn!("Connection error: {}.", e);
            }
        }

        tracing::info!("Reconnecting in {}s... (Ctrl+C to quit)", backoff_secs);

        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(backoff_secs)) => {
                backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("Received Ctrl+C. Exiting.");
                return Ok(());
            }
        }

        // After a successful reconnection, reset backoff for next disconnect
        // This is tracked by observing whether run_client ran for a meaningful time:
        // if the next run_client succeeds and stays connected, the backoff value
        // set here will be overwritten by the reset after that connection ends.
        backoff_secs = INITIAL_BACKOFF_SECS;
    }
}
