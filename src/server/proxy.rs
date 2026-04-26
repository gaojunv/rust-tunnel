use tokio::io::{AsyncReadExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use std::sync::Arc;
use tracing::{debug, warn};

use crate::common::{ControlMessage};
use crate::server::control::{ClientInfo, ServerState};

/// Proxy data from user connection to client over control channel.
/// Data from client is handled by the main control loop which writes directly to user stream.
pub async fn proxy_user_connection(
    connection_id: u64,
    remote_port: u16,
    user_stream: TcpStream,
    client_info: ClientInfo,
    state: ServerState,
) {
    // Split: read from user in this task, write handled by main control loop
    let (mut user_reader, user_writer) = user_stream.into_split();
    // Put writer half in Arc for sharing with control loop
    let user_writer = Arc::new(Mutex::new(user_writer));

    // Add this connection to active connections map so data from client can be delivered
    state.add_active_connection(connection_id, remote_port, user_writer.clone()).await;

    let mut buf = vec![0u8; 8192];

    loop {
        match user_reader.read(&mut buf).await {
            Ok(0) => {
                debug!("User connection {} closed by user", connection_id);
                break;
            }
            Ok(n) => {
                // Record incoming traffic (from user to server)
                state.traffic_store.record_bytes_in(remote_port, n as u64).await;

                // Send data from user to client via control channel
                let mut control_guard = client_info.control_writer.lock().await;
                if let Err(e) = (ControlMessage::Data {
                    connection_id,
                    data: buf[..n].to_vec(),
                }).write_to_stream(&mut *control_guard).await {
                    warn!("Failed to send data from user {} to client: {}", connection_id, e);
                    break;
                }
            }
            Err(e) => {
                warn!("Error reading from user connection {}: {}", connection_id, e);
                break;
            }
        }
    }

    // Notify client the connection is closed
    let mut control_guard = client_info.control_writer.lock().await;
    let _ = (ControlMessage::Close { connection_id }).write_to_stream(&mut *control_guard).await;

    // Remove from active connections
    state.remove_active_connection(connection_id).await;
}
