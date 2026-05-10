use tokio::io::{AsyncReadExt, AsyncWrite};
use tokio::net::TcpStream;
use tracing::{debug, warn, error};

use crate::common::{ControlMessage, TunnelResult};
use crate::server::control::{ClientInfo, ServerState};
use crate::server::shadowsocks::{SSConnectionContext, SSCipher};

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

/// Bidirectional copy between two streams with traffic accounting
pub async fn copy_bidirectional_with_stats(
    _connection_id: u64,
    port: u16,
    mut client_stream: TcpStream,
    mut target_stream: TcpStream,
    state: ServerState,
) -> TunnelResult<(u64, u64)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (mut client_read, mut client_write) = client_stream.split();
    let (mut target_read, mut target_write) = target_stream.split();

    // Client -> Target upload (for SS: client is user, target is destination)
    let upload = async {
        let mut buf = [0u8; 8192];
        let mut total = 0u64;
        loop {
            let n = match client_read.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    debug!("Upload read error: {}", e);
                    break;
                }
            };

            if target_write.write_all(&buf[..n]).await.is_err() {
                break;
            }

            total += n as u64;
            state.traffic_store.record_bytes_in(port, n as u64).await;
        }
        total
    };

    // Target -> Client download
    let download = async {
        let mut buf = [0u8; 8192];
        let mut total = 0u64;
        loop {
            let n = match target_read.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    debug!("Download read error: {}", e);
                    break;
                }
            };

            if client_write.write_all(&buf[..n]).await.is_err() {
                break;
            }

            total += n as u64;
            state.traffic_store.record_bytes_out(port, n as u64).await;
        }
        total
    };

    let (uploaded, downloaded) = tokio::join!(upload, download);
    Ok((uploaded, downloaded))
}

/// Proxy a Shadowsocks connection to target
pub async fn proxy_ss_connection(
    connection_id: u64,
    ss_port: u16,
    user_stream: TcpStream,
    ss_ctx: SSConnectionContext,
    _cipher: Box<dyn SSCipher>,
    state: ServerState,
) {
    debug!("Starting SS proxy for connection {}, target {}", connection_id, ss_ctx.target_addr);

    // Connect to target server
    let target_stream = match TcpStream::connect(&ss_ctx.target_addr).await {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to connect to target {}: {}", ss_ctx.target_addr, e);
            return;
        }
    };

    debug!("Connected to target {} for SS connection {}", ss_ctx.target_addr, connection_id);

    // NOTE: Actual Shadowsocks implementation would:
    // 1. Decrypt data from user_stream before sending to target_stream
    // 2. Encrypt data from target_stream before sending to user_stream
    //
    // For now, we're doing plain pass-through (placeholder for encryption layer)
    // The cipher wrapper will be integrated in the future

    match copy_bidirectional_with_stats(connection_id, ss_port, user_stream, target_stream, state).await {
        Ok((uploaded, downloaded)) => {
            debug!("SS connection {} completed: uploaded {} bytes, downloaded {} bytes",
                   connection_id, uploaded, downloaded);
        }
        Err(e) => {
            warn!("SS connection {} error: {}", connection_id, e);
        }
    }
}
