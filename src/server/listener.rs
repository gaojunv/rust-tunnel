use tokio::net::{TcpListener, TcpStream};
use tracing::{info, debug, warn};
use rand::Rng;

use crate::common::{ControlMessage, TunnelResult};
use crate::server::control::ServerState;
use crate::server::proxy;

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

async fn handle_inbound_connection(
    state: ServerState,
    remote_port: u16,
    connection_id: u64,
    user_stream: TcpStream,
) -> TunnelResult<()> {
    // Get the client info for this port
    let client_info = match state.get_client(remote_port).await {
        Some(info) => info,
        None => {
            warn!("No client registered for port {}, closing connection", remote_port);
            return Ok(());
        }
    };

    // Notify client about the new connection
    let mut control_guard = client_info.control_writer.lock().await;
    ControlMessage::NewConnection { connection_id, remote_port }.write_to_stream(&mut *control_guard).await?;
    drop(control_guard);

    // Wait for client to indicate it's ready
    // Client will send ConnectionReady, then we start proxying
    // The message will be read by the main control loop, which will wake us up?
    // Actually, we just wait for the client to send data through the channel.
    // We need to wait for the ConnectionReady message. Let's just give client some time.
    // Wait, actually the main control loop handles all messages. We need a different approach.
    // Let's just directly proxy and expect that client connects and we start getting data.

    // Start proxying: user stream -> control channel -> client -> local service
    // Data from client -> control channel -> main control loop -> user stream
    proxy::proxy_user_connection(connection_id, remote_port, user_stream, client_info, state).await;
    Ok(())
}
