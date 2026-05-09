use tokio::io::{AsyncReadExt, AsyncWrite};
use tokio::net::TcpStream;
use tracing::{debug, warn};

use crate::common::ControlMessage;
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
    let (mut user_reader, user_writer) = tokio::io::split(user_stream);
    // Box and wrap writer for trait object
    let boxed_writer: Box<dyn AsyncWrite + Unpin + Send> = Box::new(user_writer);
    let user_writer = std::sync::Arc::new(tokio::sync::Mutex::new(boxed_writer));

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
                if let Err(e) = client_info.control_sender.send(ControlMessage::Data {
                    connection_id,
                    data: buf[..n].to_vec(),
                }).await {
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
    let _ = client_info.control_sender.send(ControlMessage::Close { connection_id }).await;

    // Remove from active connections
    state.remove_active_connection(connection_id).await;
}
