use rand::Rng;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, warn};

use crate::common::{TunnelError, TunnelResult};
use crate::server::control::{PortInfo, ServerState};
use crate::server::proxy::proxy_ss_connection;
use crate::server::proxy::proxy_trojan_connection;
use crate::server::shadowsocks::handle_ss_handshake;
use crate::server::trojan::{handle_trojan_fallback, handle_trojan_handshake};

/// Generate a unique connection ID
fn generate_connection_id() -> u64 {
    rand::thread_rng().gen()
}

/// Start Shadowsocks listener if enabled
pub async fn start_shadowsocks_listener(
    state: ServerState,
    port: u16,
    cipher: String,
    password: String,
) -> TunnelResult<()> {
    if !state.register_shadowsocks(port, cipher, password).await {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            format!("Port {} already in use", port),
        )
        .into());
    }

    let bind_addr = format!("0.0.0.0:{}", port);
    let listener = TcpListener::bind(&bind_addr).await?;
    info!("Shadowsocks listener started on {}", bind_addr);

    loop {
        let (inbound, client_addr) = listener.accept().await?;
        debug!("New SS connection from {}", client_addr);
        let state_clone = state.clone();
        tokio::spawn(async move {
            let connection_id = generate_connection_id();
            if let Err(e) = handle_inbound_connection(state_clone, port, connection_id, inbound).await
            {
                debug!("SS connection error: {}", e);
            }
        });
    }
}

/// Start Shadowsocks listener with abort support
pub async fn start_shadowsocks_listener_with_abort(
    state: ServerState,
    port: u16,
    cipher: String,
    password: String,
    mut abort_rx: tokio::sync::watch::Receiver<bool>,
) -> TunnelResult<()> {
    if !state.register_shadowsocks(port, cipher, password).await {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            format!("Port {} already in use", port),
        )
        .into());
    }

    let bind_addr = format!("0.0.0.0:{}", port);
    let listener = TcpListener::bind(&bind_addr).await?;
    info!("Shadowsocks listener started on {}", bind_addr);

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (inbound, client_addr) = result?;
                debug!("New SS connection from {}", client_addr);
                let state_clone = state.clone();
                tokio::spawn(async move {
                    let connection_id = generate_connection_id();
                    if let Err(e) = handle_inbound_connection(state_clone, port, connection_id, inbound).await {
                        debug!("SS connection error: {}", e);
                    }
                });
            }
            _ = abort_rx.changed() => {
                if *abort_rx.borrow() {
                    info!("Shadowsocks listener on port {} shutting down", port);
                    state.unregister_port(port).await;
                    return Ok(());
                }
            }
        }
    }
}

/// Start Trojan listener with TLS
pub async fn start_trojan_listener(
    state: ServerState,
    port: u16,
    password: String,
    fallback: String,
    tls_config_rx: watch::Receiver<Arc<rustls::server::ServerConfig>>,
) -> TunnelResult<()> {
    // Register Trojan port in ServerState
    if !state
        .register_trojan(port, password.clone(), fallback.clone())
        .await
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            format!("Port {} already in use", port),
        )
        .into());
    }

    let bind_addr = format!("0.0.0.0:{}", port);
    let listener = TcpListener::bind(&bind_addr).await?;
    info!("Trojan TLS listener started on {}", bind_addr);

    loop {
        let (inbound, client_addr) = listener.accept().await?;
        debug!("New Trojan connection from {}", client_addr);

        let connection_id = generate_connection_id();
        let state_clone = state.clone();
        let password_clone = password.clone();
        let fallback_clone = fallback.clone();
        let tls_config_rx_clone = tls_config_rx.clone();

        tokio::spawn(async move {
            // Read the latest TLS config from the watch channel
            let current_config = tls_config_rx_clone.borrow().clone();
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
            match handle_trojan_handshake(&mut tls_stream, &password_clone, connection_id, port)
                .await
            {
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
                        state_clone,
                    )
                    .await;
                }
                Err(e) => {
                    warn!(
                        "Trojan handshake failed for connection {}: {}",
                        connection_id, e
                    );
                    // Fallback: forward to fallback backend
                    debug!("Attempting Trojan fallback to {}", fallback_clone);
                    // Extract initial data from the error if available
                    let initial_data = match &e {
                        TunnelError::TrojanAuthFailed(data) => data.as_slice(),
                        _ => &[],
                    };
                    if let Err(fe) =
                        handle_trojan_fallback(&mut tls_stream, initial_data, &fallback_clone).await
                    {
                        warn!("Trojan fallback also failed: {}", fe);
                    }
                }
            }
        });
    }
}

/// Start Trojan listener with abort support
pub async fn start_trojan_listener_with_abort(
    state: ServerState,
    port: u16,
    password: String,
    fallback: String,
    tls_config_rx: tokio::sync::watch::Receiver<Arc<rustls::server::ServerConfig>>,
    mut abort_rx: tokio::sync::watch::Receiver<bool>,
) -> TunnelResult<()> {
    if !state
        .register_trojan(port, password.clone(), fallback.clone())
        .await
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            format!("Port {} already in use", port),
        )
        .into());
    }

    let bind_addr = format!("0.0.0.0:{}", port);
    let listener = TcpListener::bind(&bind_addr).await?;
    info!("Trojan TLS listener started on {}", bind_addr);

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (inbound, client_addr) = result?;
                debug!("New Trojan connection from {}", client_addr);

                let connection_id = generate_connection_id();
                let state_clone = state.clone();
                let password_clone = password.clone();
                let fallback_clone = fallback.clone();
                let tls_config_rx_clone = tls_config_rx.clone();

                tokio::spawn(async move {
                    // Read the latest TLS config from the watch channel
                    let current_config = tls_config_rx_clone.borrow().clone();
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
                    match handle_trojan_handshake(&mut tls_stream, &password_clone, connection_id, port)
                        .await
                    {
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
                                state_clone,
                            )
                            .await;
                        }
                        Err(e) => {
                            warn!(
                                "Trojan handshake failed for connection {}: {}",
                                connection_id, e
                            );
                            // Fallback: forward to fallback backend
                            debug!("Attempting Trojan fallback to {}", fallback_clone);
                            // Extract initial data from the error if available
                            let initial_data = match &e {
                                TunnelError::TrojanAuthFailed(data) => data.as_slice(),
                                _ => &[],
                            };
                            if let Err(fe) =
                                handle_trojan_fallback(&mut tls_stream, initial_data, &fallback_clone).await
                            {
                                warn!("Trojan fallback also failed: {}", fe);
                            }
                        }
                    }
                });
            }
            _ = abort_rx.changed() => {
                if *abort_rx.borrow() {
                    info!("Trojan listener on port {} shutting down", port);
                    state.unregister_port(port).await;
                    return Ok(());
                }
            }
        }
    }
}

async fn handle_inbound_connection(
    state: ServerState,
    remote_port: u16,
    connection_id: u64,
    user_stream: TcpStream,
) -> TunnelResult<()> {
    // Get port info
    let port_info = match state.get_port(remote_port).await {
        Some(info) => info,
        None => {
            warn!("No port registered for {}, closing connection", remote_port);
            return Ok(());
        }
    };

    match port_info {
        PortInfo::Shadowsocks {
            cipher, password, ..
        } => {
            // New Shadowsocks proxy logic
            debug!("Handling Shadowsocks connection on port {}", remote_port);

            // Create SS context and handle handshake
            use crate::server::shadowsocks::create_shared_context;
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
                    proxy_ss_connection(connection_id, remote_port, proxy_stream, ss_ctx, state)
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
