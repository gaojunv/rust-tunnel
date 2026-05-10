use tokio::net::{TcpListener, TcpStream};
use tracing::{info, debug, warn};
use rand::Rng;

use crate::common::{ControlMessage, TunnelResult};
use crate::server::shadowsocks::handle_ss_handshake;
use crate::server::proxy;
use crate::server::proxy::proxy_ss_connection;
use crate::server::control::{ServerState, PortInfo};

/// Generate a unique connection ID
fn generate_connection_id() -> u64 {
    rand::thread_rng().gen()
}

/// Run the listener for a specific remote port
pub async fn run_listener(state: ServerState, remote_port: u16) -> TunnelResult<()> {
    let bind_addr = format!("0.0.0.0:{}", remote_port);
    let listener = TcpListener::bind(&bind_addr).await?;
    info!("Started listening for public connections on {}", bind_addr);

    loop {
        let (inbound, client_addr) = listener.accept().await?;
        debug!("New public connection from {}", client_addr);

        let connection_id = generate_connection_id();
        let state_clone = state.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_inbound_connection(state_clone, remote_port, connection_id, inbound).await {
                warn!("Failed to handle inbound connection {}: {}", connection_id, e);
            }
        });
    }
}

/// Start Shadowsocks listener if enabled
pub async fn start_shadowsocks_listener(
    state: ServerState,
    port: u16,
    cipher: String,
    password: String,
) -> TunnelResult<()> {
    // Register SS port in ServerState
    if !state.register_shadowsocks(port, cipher, password).await {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            format!("Port {} already in use", port),
        ).into());
    }

    // Start the listener (reuses existing run_listener logic)
    run_listener(state, port).await
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
        PortInfo::Tunnel(client_info) => {
            // Existing tunnel proxy logic
            debug!("Handling Tunnel connection on port {}", remote_port);
            // Notify client about the new connection
            client_info.control_sender.send(ControlMessage::NewConnection { connection_id, remote_port }).await
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("Failed to send message: {}", e)))?;
            proxy::proxy_user_connection(connection_id, remote_port, user_stream, client_info, state).await;
        }
        PortInfo::Shadowsocks { cipher, password, .. } => {
            // New Shadowsocks proxy logic
            debug!("Handling Shadowsocks connection on port {}", remote_port);

            // Handle SS handshake
            let mut stream_mut = user_stream;
            match handle_ss_handshake(&mut stream_mut, &cipher, &password, connection_id, remote_port).await {
                Ok((ss_ctx, ss_cipher)) => {
                    proxy_ss_connection(connection_id, remote_port, stream_mut, ss_ctx, ss_cipher, state).await;
                }
                Err(e) => {
                    warn!("SS handshake failed for connection {}: {}", connection_id, e);
                }
            }
        }
    }

    Ok(())
}
