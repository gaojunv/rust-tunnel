use tokio::io::{AsyncReadExt, AsyncWrite};
use tokio::net::TcpStream;
use tracing::{debug, warn};

pub use crate::client::control::ClientState;
use crate::common::{ControlMessage, TunnelError, TunnelResult};

/// Handle a new connection request from server
pub async fn handle_new_connection(
    state: ClientState,
    connection_id: u64,
    remote_port: u16,
) -> TunnelResult<()> {
    // Find the local address for this remote port
    let forward_rule = state.forwards.iter().find(|r| r.remote_port == remote_port);
    let local_addr = match forward_rule {
        Some(r) => &r.local_addr,
        None => {
            warn!("No forward rule found for remote port {}", remote_port);
            state.remove_connection(connection_id).await;
            let _ = state
                .control_sender
                .send(ControlMessage::Close { connection_id })
                .await;
            return Err(TunnelError::Config(format!(
                "No forward rule for remote port {}",
                remote_port
            )));
        }
    };

    // Connect to local target using async Tokio TcpStream
    debug!("Connecting to local target {}", local_addr);

    let local_stream = match TcpStream::connect(local_addr).await {
        Ok(stream) => {
            debug!("Successfully connected to {}", local_addr);
            stream
        }
        Err(e) => {
            warn!("Failed to connect to local target {}: {}", local_addr, e);
            state.remove_connection(connection_id).await;
            let _ = state
                .control_sender
                .send(ControlMessage::Close { connection_id })
                .await;
            return Err(e.into());
        }
    };

    // Split stream: reading in this task, writing done by control loop
    let (mut local_reader, local_writer) = tokio::io::split(local_stream);
    // Box the writer for trait object
    let boxed_writer: Box<dyn AsyncWrite + Unpin + Send> = Box::new(local_writer);

    // Activate the pending connection (flushes any buffered data received
    // while we were connecting). If activation fails, the connection was
    // closed while we were connecting.
    if !state.activate_connection(connection_id, boxed_writer).await {
        debug!(
            "Connection {} was closed while connecting to local target",
            connection_id
        );
        return Ok(());
    }

    // Notify server we're ready
    state
        .control_sender
        .send(ControlMessage::ConnectionReady { connection_id })
        .await
        .map_err(|_| TunnelError::Protocol("Failed to send connection ready".into()))?;

    let mut buf = vec![0u8; 8192];

    loop {
        match local_reader.read(&mut buf).await {
            Ok(0) => {
                debug!("Local connection {} closed", connection_id);
                break;
            }
            Ok(n) => {
                // Send data from local to server via control channel
                if let Err(e) = state
                    .control_sender
                    .send(ControlMessage::Data {
                        connection_id,
                        data: buf[..n].to_vec(),
                    })
                    .await
                {
                    warn!(
                        "Failed to send data from local {} to server: {}",
                        connection_id, e
                    );
                    break;
                }
            }
            Err(e) => {
                warn!(
                    "Error reading from local connection {}: {}",
                    connection_id, e
                );
                break;
            }
        }
    }

    // Notify server connection is closed
    let _ = state
        .control_sender
        .send(ControlMessage::Close { connection_id })
        .await;

    // Remove from active connections
    state.remove_connection(connection_id).await;

    Ok(())
}
