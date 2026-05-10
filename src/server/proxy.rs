use std::time::Instant;
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

/// Update quality metrics for SS connection
async fn update_ss_quality(
    state: &ServerState,
    port: u16,
    connect_time_ms: u64,
    bytes_in: u64,
    bytes_out: u64,
    elapsed_secs: f64,
) {
    use crate::server::quality::{ConnectionQuality, calculate_quality_score, check_warnings, QualityThresholds};
    use chrono::Utc;

    let mut quality = ConnectionQuality::default();

    // Use connect time as RTT estimate
    quality.last_rtt_ms = connect_time_ms as f32;
    quality.avg_rtt_ms = connect_time_ms as f32;
    quality.min_rtt_ms = connect_time_ms as f32;
    quality.max_rtt_ms = connect_time_ms as f32;

    // Calculate throughput
    if elapsed_secs > 0.0 {
        quality.bytes_in_per_sec = bytes_in as f64 / elapsed_secs;
        quality.bytes_out_per_sec = bytes_out as f64 / elapsed_secs;
    }

    // For SS, loss rate is 0 (we don't measure it directly)
    // But we can infer from connection errors
    quality.loss_rate = 0.0;

    // Calculate quality score
    quality.quality_score = calculate_quality_score(quality.avg_rtt_ms, quality.loss_rate);

    // Check warnings
    let thresholds = QualityThresholds::default();
    let (is_warning, is_critical) = check_warnings(quality.avg_rtt_ms, quality.loss_rate, &thresholds);
    quality.is_warning = is_warning;
    quality.is_critical = is_critical;
    quality.last_update = Utc::now();

    state.quality_store.update_quality(port, quality).await;
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

    // Record start time for measuring connection setup time (RTT estimate)
    let start = Instant::now();

    // Connect to target server
    let target_stream = match TcpStream::connect(&ss_ctx.target_addr).await {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to connect to target {}: {}", ss_ctx.target_addr, e);
            return;
        }
    };

    // Calculate connection establishment time as RTT estimate
    let connect_time_ms = start.elapsed().as_millis() as u64;
    debug!("Connected to target {} for SS connection {} in {}ms",
           ss_ctx.target_addr, connection_id, connect_time_ms);

    // NOTE: Actual Shadowsocks implementation would:
    // 1. Decrypt data from user_stream before sending to target_stream
    // 2. Encrypt data from target_stream before sending to user_stream
    //
    // For now, we're doing plain pass-through (placeholder for encryption layer)
    // The cipher wrapper will be integrated in the future

    let proxy_start = Instant::now();
    match copy_bidirectional_with_stats(connection_id, ss_port, user_stream, target_stream, state.clone()).await {
        Ok((uploaded, downloaded)) => {
            let elapsed = proxy_start.elapsed();
            let elapsed_secs = elapsed.as_secs_f64();

            debug!("SS connection {} completed: uploaded {} bytes, downloaded {} bytes in {:.2}s",
                   connection_id, uploaded, downloaded, elapsed_secs);

            // Update quality metrics
            update_ss_quality(&state, ss_port, connect_time_ms, uploaded, downloaded, elapsed_secs).await;
        }
        Err(e) => {
            warn!("SS connection {} error: {}", connection_id, e);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::server::control::ServerState;

    #[tokio::test]
    async fn test_ss_traffic_statistics() {
        let state = ServerState::new();

        // Register a shadowsocks port
        assert!(state.register_shadowsocks(8388, "aes-256-gcm".into(), "password".into()).await);

        // Check initial traffic doesn't exist (zero effective)
        assert!(state.traffic_store.get_port_traffic(8388).await.is_none());

        // Record some traffic manually (this is what copy_bidirectional_with_stats does)
        state.traffic_store.record_bytes_in(8388, 1000).await;
        state.traffic_store.record_bytes_out(8388, 2000).await;

        // Check stats are updated
        let traffic = state.traffic_store.get_port_traffic(8388).await.unwrap();
        assert_eq!(traffic.total_bytes_in, 1000);
        assert_eq!(traffic.total_bytes_out, 2000);

        // Remove the port
        assert!(state.unregister_port(8388).await);
        state.traffic_store.remove_port(8388).await;

        // After removal, port should be gone
        assert!(state.traffic_store.get_port_traffic(8388).await.is_none());
    }

    #[tokio::test]
    async fn test_mixed_traffic_statistics() {
        let state = ServerState::new();

        // Register both tunnel and SS
        let (sender, _) = tokio::sync::mpsc::channel(1);
        state.register_client(8080, None, sender).await;
        state.register_shadowsocks(8388, "aes-256-gcm".into(), "password".into()).await;

        // Record traffic to both
        state.traffic_store.record_bytes_in(8080, 1234).await;
        state.traffic_store.record_bytes_out(8080, 5678).await;
        state.traffic_store.record_bytes_in(8388, 10000).await;
        state.traffic_store.record_bytes_out(8388, 20000).await;

        // Check each port has correct stats
        let tunnel_traffic = state.traffic_store.get_port_traffic(8080).await.unwrap();
        let ss_traffic = state.traffic_store.get_port_traffic(8388).await.unwrap();

        assert_eq!(tunnel_traffic.total_bytes_in, 1234);
        assert_eq!(tunnel_traffic.total_bytes_out, 5678);
        assert_eq!(ss_traffic.total_bytes_in, 10000);
        assert_eq!(ss_traffic.total_bytes_out, 20000);
    }
}
