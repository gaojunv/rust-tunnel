use std::io::IsTerminal;
use std::time::Duration;

use rust_tunnel_client::control;
use rust_tunnel_client::ClientConfig;
use rust_tunnel_common::{init_logging_with_level, TunnelError, TunnelResult};

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
    tracing::info!(
        "Starting rust-tunnel client, connecting to server {}",
        config.server
    );
    if config.enable_agent {
        tracing::info!("Agent executor enabled (accepting AgentExecRequest)");
    }

    // PTY 服务生命周期 = 客户端进程生命周期：enable_agent 时启动一次，不随控制
    // 通道重连重启。listen 失败只 warn 不退出——服务端会按客户端版本门控降级。
    if config.enable_agent {
        let port = config.agent_pty_port;
        tokio::spawn(async move {
            if let Err(e) = rust_tunnel_client::pty::serve(port).await {
                tracing::warn!("agent PTY service disabled: {e}");
            }
        });
    }

    let mut backoff_secs = INITIAL_BACKOFF_SECS;

    loop {
        match control::run_client(config.clone()).await {
            Ok(()) => {
                tracing::warn!("Control connection closed.");
                // Connection was established successfully before dropping — reset backoff
                backoff_secs = INITIAL_BACKOFF_SECS;
            }
            Err(TunnelError::ControlChannel(msg)) if msg.contains("register failed") => {
                tracing::error!("registration rejected by server: {msg}");
                std::process::exit(2);
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
