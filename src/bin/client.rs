use std::io::IsTerminal;
use std::time::Duration;

use rust_tunnel::client::{control, ClientConfig};
use rust_tunnel::common::{init_logging_with_level, TunnelResult};

const INITIAL_BACKOFF_SECS: u64 = 1;
const MAX_BACKOFF_SECS: u64 = 30;

fn wait_for_exit() {
    if std::io::stdout().is_terminal() {
        eprintln!("按 Enter 键退出...");
        let mut input = String::new();
        let _ = std::io::stdin().read_line(&mut input);
    }
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("启动失败: {}", e);
        wait_for_exit();
        std::process::exit(1);
    }
}

async fn run() -> TunnelResult<()> {
    let config = ClientConfig::load().map_err(std::io::Error::other)?;
    init_logging_with_level(&config.log);
    let forwards = config
        .parse_forwards()
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
                // Connection was established successfully before dropping — reset backoff
                backoff_secs = INITIAL_BACKOFF_SECS;
            }
            Err(e) => {
                tracing::warn!("Connection error: {}.", e);
                // Connection never established — backoff will keep doubling
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
    }
}
