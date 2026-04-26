use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use std::sync::Arc;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use tracing::{debug, warn};

use crate::common::{ControlMessage, TunnelError, TunnelResult};
pub use crate::client::control::ClientState;

/// Handle a new connection request from server
pub async fn handle_new_connection(state: ClientState, connection_id: u64, remote_port: u16) -> TunnelResult<()> {
    // Find the local address for this remote port
    let forward_rule = state.forwards.iter().find(|r| r.remote_port == remote_port);
    let local_addr = match forward_rule {
        Some(r) => &r.local_addr,
        None => {
            warn!("No forward rule found for remote port {}", remote_port);
            let mut control_guard = state.control_writer.lock().await;
            let _ = (ControlMessage::Close { connection_id }).write_to_stream(&mut *control_guard).await;
            return Err(TunnelError::Config(format!("No forward rule for remote port {}", remote_port)));
        }
    };

    // Connect to local target using blocking std connect (same behavior as curl/nc)
    // This avoids macOS-specific "No route to host" issue when connecting to gateway
    debug!("Connecting to local target {}", local_addr);

    // Use std::net::TcpStream to connect - this has the same behavior as curl
    let std_stream = std::net::TcpStream::connect(local_addr);
    let local_stream = match std_stream {
        Ok(std_stream) => {
            // Set non-blocking and convert to tokio TcpStream
            std_stream.set_nonblocking(true)?;
            match TcpStream::from_std(std_stream) {
                Ok(stream) => {
                    debug!("Successfully connected to {}", local_addr);
                    stream
                }
                Err(e) => {
                    warn!("Failed to convert to async stream: {}", e);
                    let mut control_guard = state.control_writer.lock().await;
                    let _ = (ControlMessage::Close { connection_id }).write_to_stream(&mut *control_guard).await;
                    return Err(e.into());
                }
            }
        }
        Err(e) => {
            warn!("Failed to connect to local target {}: {}", local_addr, e);
            let mut control_guard = state.control_writer.lock().await;
            let _ = (ControlMessage::Close { connection_id }).write_to_stream(&mut *control_guard).await;
            return Err(e.into());
        }
    };

    // Notify server we're ready
    let mut control_guard = state.control_writer.lock().await;
    (ControlMessage::ConnectionReady { connection_id }).write_to_stream(&mut *control_guard).await?;
    drop(control_guard);

    // Split stream: reading in this task, writing done by control loop
    let (mut local_reader, local_writer) = local_stream.into_split();
    // Put writer half in Arc for sharing with control loop
    let local_writer = Arc::new(Mutex::new(local_writer));

    // Add to active connections so data from server can be delivered
    state.add_connection(connection_id, local_writer.clone()).await;

    let mut buf = vec![0u8; 8192];

    loop {
        match local_reader.read(&mut buf).await {
            Ok(0) => {
                debug!("Local connection {} closed", connection_id);
                break;
            }
            Ok(n) => {
                // Send data from local to server via control channel
                let mut control_guard = state.control_writer.lock().await;
                if let Err(e) = (ControlMessage::Data {
                    connection_id,
                    data: buf[..n].to_vec(),
                }).write_to_stream(&mut *control_guard).await {
                    warn!("Failed to send data from local {} to server: {}", connection_id, e);
                    break;
                }
            }
            Err(e) => {
                warn!("Error reading from local connection {}: {}", connection_id, e);
                break;
            }
        }
    }

    // Notify server connection is closed
    let mut control_guard = state.control_writer.lock().await;
    let _ = (ControlMessage::Close { connection_id }).write_to_stream(&mut *control_guard).await;

    // Remove from active connections
    state.remove_connection(connection_id).await;

    Ok(())
}
