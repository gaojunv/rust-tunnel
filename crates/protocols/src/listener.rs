use rand::Rng;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, warn};

use crate::port_registry::{PortInfo, PortRegistry};
use crate::shadowsocks::handle_ss_handshake;
use crate::shadowsocks::proxy_ss_connection;
use crate::trojan::proxy_trojan_connection;
use crate::trojan::{handle_trojan_fallback, handle_trojan_handshake};
use rust_tunnel_common::{TunnelError, TunnelResult};
use rust_tunnel_stats::StatsCollector;

/// Generate a unique connection ID
fn generate_connection_id() -> u64 {
    rand::thread_rng().gen()
}

/// Start Shadowsocks listener if enabled
pub async fn start_shadowsocks_listener(
    registry: std::sync::Arc<dyn PortRegistry>,
    stats: StatsCollector,
    port: u16,
    cipher: String,
    password: String,
) -> TunnelResult<()> {
    if !registry.register_shadowsocks(port, cipher, password).await {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            format!("Port {port} already in use"),
        )
        .into());
    }

    let bind_addr = format!("0.0.0.0:{port}");
    let listener = TcpListener::bind(&bind_addr).await?;
    info!("Shadowsocks listener started on {}", bind_addr);

    loop {
        let (inbound, client_addr) = listener.accept().await?;
        debug!("New SS connection from {}", client_addr);
        let reg = registry.clone();
        let st = stats.clone();
        tokio::spawn(async move {
            let connection_id = generate_connection_id();
            if let Err(e) = handle_inbound_connection(reg, st, port, connection_id, inbound).await {
                debug!("SS connection error: {}", e);
            }
        });
    }
}

/// Start Shadowsocks listener with abort support
pub async fn start_shadowsocks_listener_with_abort(
    registry: std::sync::Arc<dyn PortRegistry>,
    stats: StatsCollector,
    port: u16,
    cipher: String,
    password: String,
    mut abort_rx: tokio::sync::watch::Receiver<bool>,
) -> TunnelResult<()> {
    if !registry.register_shadowsocks(port, cipher, password).await {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            format!("Port {port} already in use"),
        )
        .into());
    }

    let bind_addr = format!("0.0.0.0:{port}");
    let listener = TcpListener::bind(&bind_addr).await?;
    info!("Shadowsocks listener started on {}", bind_addr);

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (inbound, client_addr) = result?;
                debug!("New SS connection from {}", client_addr);
                let reg = registry.clone();
                let st = stats.clone();
                tokio::spawn(async move {
                    let connection_id = generate_connection_id();
                    if let Err(e) = handle_inbound_connection(reg.clone(), st.clone(), port, connection_id, inbound).await {
                        debug!("SS connection error: {}", e);
                    }
                });
            }
            _ = abort_rx.changed() => {
                if *abort_rx.borrow() {
                    info!("Shadowsocks listener on port {} shutting down", port);
                    registry.unregister_port(port).await;
                    return Ok(());
                }
            }
        }
    }
}

/// Handle one Trojan connection: TLS 终止 → Trojan 握手 → 代理/回退。
///
/// 从独立 listener 的每连接逻辑提取，供独立监听模式与反代 SNI 分流模式共用。
/// 每连接从 watch channel borrow 最新的 TLS 配置（证书热更新机制）。
pub async fn handle_trojan_connection(
    inbound: TcpStream,
    client_addr: std::net::SocketAddr,
    port: u16,
    password: String,
    fallback: String,
    tls_config_rx: watch::Receiver<Arc<rustls::server::ServerConfig>>,
    registry: std::sync::Arc<dyn PortRegistry>,
    stats: StatsCollector,
) {
    let connection_id = generate_connection_id();
    // Read the latest TLS config from the watch channel
    let current_config = tls_config_rx.borrow().clone();
    let tls_acceptor = TlsAcceptor::from(current_config);

    // TLS handshake first
    let mut tls_stream = match tls_acceptor.accept(inbound).await {
        Ok(s) => s,
        Err(e) => {
            warn!("Trojan TLS handshake failed for {}: {}", client_addr, e);
            return;
        }
    };

    // Trojan handshake over TLS
    match handle_trojan_handshake(&mut tls_stream, &password, connection_id, port).await {
        Ok((ctx, payload)) => {
            debug!(
                "Trojan authenticated: target={}, cmd={:?}",
                ctx.target_addr, ctx.command
            );
            proxy_trojan_connection(
                connection_id,
                port,
                tls_stream,
                ctx,
                payload,
                registry,
                stats,
            )
            .await;
        }
        Err(e) => {
            warn!(
                "Trojan handshake failed for connection {}: {}",
                connection_id, e
            );
            // Fallback: forward to fallback backend
            debug!("Attempting Trojan fallback to {}", fallback);
            // Extract initial data from the error if available
            let initial_data = match &e {
                TunnelError::TrojanAuthFailed(data) => data.as_slice(),
                _ => &[],
            };
            if let Err(fe) = handle_trojan_fallback(&mut tls_stream, initial_data, &fallback).await
            {
                warn!("Trojan fallback also failed: {}", fe);
            }
        }
    }
}

/// Start Trojan listener with TLS
pub async fn start_trojan_listener(
    registry: std::sync::Arc<dyn PortRegistry>,
    stats: StatsCollector,
    port: u16,
    password: String,
    fallback: String,
    tls_config_rx: watch::Receiver<Arc<rustls::server::ServerConfig>>,
) -> TunnelResult<()> {
    // Register Trojan port
    if !registry
        .register_trojan(port, password.clone(), fallback.clone())
        .await
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            format!("Port {port} already in use"),
        )
        .into());
    }

    let bind_addr = format!("0.0.0.0:{port}");
    let listener = TcpListener::bind(&bind_addr).await?;
    info!("Trojan TLS listener started on {}", bind_addr);

    loop {
        let (inbound, client_addr) = listener.accept().await?;
        debug!("New Trojan connection from {}", client_addr);
        let reg = registry.clone();
        let st = stats.clone();
        let password_clone = password.clone();
        let fallback_clone = fallback.clone();
        let tls_config_rx_clone = tls_config_rx.clone();

        tokio::spawn(async move {
            handle_trojan_connection(
                inbound,
                client_addr,
                port,
                password_clone,
                fallback_clone,
                tls_config_rx_clone,
                reg,
                st,
            )
            .await;
        });
    }
}

/// Start Trojan listener with abort support
pub async fn start_trojan_listener_with_abort(
    registry: std::sync::Arc<dyn PortRegistry>,
    stats: StatsCollector,
    port: u16,
    password: String,
    fallback: String,
    tls_config_rx: tokio::sync::watch::Receiver<Arc<rustls::server::ServerConfig>>,
    mut abort_rx: tokio::sync::watch::Receiver<bool>,
) -> TunnelResult<()> {
    if !registry
        .register_trojan(port, password.clone(), fallback.clone())
        .await
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            format!("Port {port} already in use"),
        )
        .into());
    }

    let bind_addr = format!("0.0.0.0:{port}");
    let listener = TcpListener::bind(&bind_addr).await?;
    info!("Trojan TLS listener started on {}", bind_addr);

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (inbound, client_addr) = result?;
                debug!("New Trojan connection from {}", client_addr);
                let reg = registry.clone();
                let st = stats.clone();
                let password_clone = password.clone();
                let fallback_clone = fallback.clone();
                let tls_config_rx_clone = tls_config_rx.clone();

                tokio::spawn(async move {
                    handle_trojan_connection(
                        inbound,
                        client_addr,
                        port,
                        password_clone,
                        fallback_clone,
                        tls_config_rx_clone,
                        reg,
                        st,
                    )
                    .await;
                });
            }
            _ = abort_rx.changed() => {
                if *abort_rx.borrow() {
                    info!("Trojan listener on port {} shutting down", port);
                    registry.unregister_port(port).await;
                    return Ok(());
                }
            }
        }
    }
}

async fn handle_inbound_connection(
    registry: std::sync::Arc<dyn PortRegistry>,
    stats: StatsCollector,
    remote_port: u16,
    connection_id: u64,
    user_stream: TcpStream,
) -> TunnelResult<()> {
    // Get port info
    let port_info = if let Some(info) = registry.get_port(remote_port).await { info } else {
        warn!("No port registered for {}, closing connection", remote_port);
        return Ok(());
    };

    match port_info {
        PortInfo::Shadowsocks {
            cipher, password, ..
        } => {
            // New Shadowsocks proxy logic
            debug!("Handling Shadowsocks connection on port {}", remote_port);

            // Create SS context and handle handshake
            use crate::shadowsocks::create_shared_context;
            let ss_context = create_shared_context();

            match handle_ss_handshake(
                user_stream,
                &cipher,
                &password,
                connection_id,
                remote_port,
                ss_context,
            )
            .await
            {
                Ok((ss_ctx, proxy_stream)) => {
                    proxy_ss_connection(
                        connection_id,
                        remote_port,
                        proxy_stream,
                        ss_ctx,
                        registry,
                        stats,
                    )
                    .await;
                }
                Err(e) => {
                    warn!(
                        "SS handshake failed for connection {}: {}",
                        connection_id, e
                    );
                }
            }
        }
        PortInfo::Trojan { .. } => {
            // Trojan connections are handled in start_trojan_listener with TLS
            // This branch shouldn't normally be reached since Trojan has its own listener
            warn!(
                "Trojan connection reached tunnel listener for port {} - this should not happen",
                remote_port
            );
        }
    }

    Ok(())
}
