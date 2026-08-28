//! rust-tunnel 客户端二进制：解析配置、启动 PTY 服务与控制通道重连循环。

use std::io::IsTerminal;
use std::time::Duration;

use rust_tunnel_client::control;
use rust_tunnel_client::{ClientConfig, ReconnectPolicy};
use rust_tunnel_common::{init_logging_with_level, TunnelResult};

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
        eprintln!("启动失败: {e}");
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

    let mut policy = ReconnectPolicy::new();

    loop {
        match control::run_client(config.clone()).await {
            Ok(()) => {
                tracing::warn!("Control connection closed.");
                policy.reset();
            }
            Err(e) if !ReconnectPolicy::should_reconnect(&e) => {
                tracing::error!("registration rejected by server: {e}");
                eprintln!("register failed: {e}, exiting");
                std::process::exit(2);
            }
            Err(e) => {
                tracing::warn!("Connection error: {}.", e);
            }
        }

        let backoff_secs = policy.next_backoff();
        tracing::info!("Reconnecting in {}s... (Ctrl+C to quit)", backoff_secs);

        tokio::select! {
            () = tokio::time::sleep(Duration::from_secs(backoff_secs)) => {}
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("Received Ctrl+C. Exiting.");
                return Ok(());
            }
        }
    }
}
