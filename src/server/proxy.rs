use std::time::Instant;
use tokio::io::AsyncWrite;
use tokio::net::TcpStream;
use tokio_rustls::server::TlsStream;
use tracing::{debug, error, warn};

use crate::common::TunnelResult;
use crate::server::control::ServerState;
use crate::server::shadowsocks::{ProxyServerStream, SSConnectionContext};
use crate::server::trojan::{TrojanCommand, TrojanConnectionContext};

/// Bidirectional copy between two streams with traffic accounting.
///
/// Uses `copy_bidirectional` which properly handles TCP shutdown: when one direction
/// observes EOF, it calls `shutdown()` on the opposing writer so the other direction
/// unblocks instead of hanging indefinitely.
pub async fn copy_bidirectional_with_stats(
    _connection_id: u64,
    port: u16,
    mut client_stream: TcpStream,
    mut target_stream: TcpStream,
    state: ServerState,
) -> TunnelResult<(u64, u64)> {
    use crate::server::shadowsocks::copy_bidirectional;

    let (client_to_target, target_to_client) =
        copy_bidirectional(&mut client_stream, &mut target_stream).await?;

    state
        .traffic_store
        .record_bytes_in(port, client_to_target)
        .await;
    state
        .traffic_store
        .record_bytes_out(port, target_to_client)
        .await;

    Ok((client_to_target, target_to_client))
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
    use crate::server::quality::{
        calculate_quality_score, check_warnings, ConnectionQuality, QualityThresholds,
    };
    use chrono::Utc;

    let mut quality = ConnectionQuality {
        last_rtt_ms: connect_time_ms as f32,
        avg_rtt_ms: connect_time_ms as f32,
        min_rtt_ms: connect_time_ms as f32,
        max_rtt_ms: connect_time_ms as f32,
        ..Default::default()
    };

    // Calculate throughput
    if elapsed_secs > 0.0 {
        quality.bytes_in_per_sec = bytes_in as f64 / elapsed_secs;
        quality.bytes_out_per_sec = bytes_out as f64 / elapsed_secs;
    }

    // Calculate quality score
    quality.quality_score = calculate_quality_score(quality.avg_rtt_ms, quality.loss_rate);

    // Check warnings
    let thresholds = QualityThresholds::default();
    let (is_warning, is_critical) =
        check_warnings(quality.avg_rtt_ms, quality.loss_rate, &thresholds);
    quality.is_warning = is_warning;
    quality.is_critical = is_critical;
    quality.last_update = Utc::now();

    state.quality_store.update_quality(port, quality).await;
}

/// Bidirectional copy with Shadowsocks encryption/decryption using ProxyServerStream.
///
/// Uses `copy_encrypted_bidirectional` which properly handles TCP shutdown: when one
/// direction observes EOF, it calls `shutdown()` on the opposing writer so the other
/// direction unblocks instead of hanging indefinitely.
async fn copy_bidirectional_with_ss_crypto(
    _connection_id: u64,
    port: u16,
    mut proxy_stream: ProxyServerStream<TcpStream>,
    mut target_stream: TcpStream,
    state: ServerState,
) -> TunnelResult<(u64, u64)> {
    use crate::server::shadowsocks::{copy_encrypted_bidirectional, CipherKind};

    // encrypted_to_plain: client → target (upload), plain_to_encrypted: target → client (download)
    let (encrypted_to_plain, plain_to_encrypted) = copy_encrypted_bidirectional(
        CipherKind::AES_256_GCM,
        &mut proxy_stream,
        &mut target_stream,
    )
    .await?;

    state
        .traffic_store
        .record_bytes_in(port, encrypted_to_plain)
        .await;
    state
        .traffic_store
        .record_bytes_out(port, plain_to_encrypted)
        .await;

    Ok((encrypted_to_plain, plain_to_encrypted))
}

/// Proxy a Shadowsocks connection to target
pub async fn proxy_ss_connection(
    connection_id: u64,
    ss_port: u16,
    proxy_stream: ProxyServerStream<TcpStream>,
    ss_ctx: SSConnectionContext,
    state: ServerState,
) {
    debug!(
        "Starting SS proxy for connection {}, target {}",
        connection_id, ss_ctx.target_addr
    );

    // Increment active SS connection count
    state.increment_ss_connections(ss_port).await;

    // Record start time for measuring connection setup time (RTT estimate)
    let start = Instant::now();

    // Connect to target server
    let target_stream = match TcpStream::connect(&ss_ctx.target_addr).await {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to connect to target {}: {}", ss_ctx.target_addr, e);
            state.decrement_ss_connections(ss_port).await;
            return;
        }
    };

    // Calculate connection establishment time as RTT estimate
    let connect_time_ms = start.elapsed().as_millis() as u64;
    debug!(
        "Connected to target {} for SS connection {} in {}ms",
        ss_ctx.target_addr, connection_id, connect_time_ms
    );

    let proxy_start = Instant::now();
    let result = copy_bidirectional_with_ss_crypto(
        connection_id,
        ss_port,
        proxy_stream,
        target_stream,
        state.clone(),
    )
    .await;

    // Decrement active SS connection count (always run)
    state.decrement_ss_connections(ss_port).await;

    match result {
        Ok((uploaded, downloaded)) => {
            let elapsed = proxy_start.elapsed();
            let elapsed_secs = elapsed.as_secs_f64();

            debug!(
                "SS connection {} completed: uploaded {} bytes, downloaded {} bytes in {:.2}s",
                connection_id, uploaded, downloaded, elapsed_secs
            );

            // Update quality metrics
            update_ss_quality(
                &state,
                ss_port,
                connect_time_ms,
                uploaded,
                downloaded,
                elapsed_secs,
            )
            .await;
        }
        Err(e) => {
            warn!("SS connection {} error: {}", connection_id, e);
        }
    }
}

/// Proxy a Trojan connection to target.
/// Trojan data is already decrypted by TLS, so we just do raw TCP bidirectional copy.
pub async fn proxy_trojan_connection(
    connection_id: u64,
    trojan_port: u16,
    mut tls_stream: TlsStream<TcpStream>,
    trojan_ctx: TrojanConnectionContext,
    initial_payload: Vec<u8>,
    state: ServerState,
) {
    debug!(
        "Starting Trojan proxy for connection {}, target {}",
        connection_id, trojan_ctx.target_addr
    );

    // Reject UDP ASSOCIATE — only CONNECT is supported
    if trojan_ctx.command == TrojanCommand::UdpAssociate {
        warn!(
            "Trojan UDP ASSOCIATE is not supported for connection {}",
            connection_id
        );
        return;
    }

    // Increment active Trojan connection count
    state.increment_trojan_connections(trojan_port).await;

    // Record start time for measuring connection setup time (RTT estimate)
    let start = Instant::now();

    // Connect to target server
    let mut target_stream = match TcpStream::connect(&trojan_ctx.target_addr).await {
        Ok(s) => s,
        Err(e) => {
            error!(
                "Failed to connect to target {}: {}",
                trojan_ctx.target_addr, e
            );
            state.decrement_trojan_connections(trojan_port).await;
            return;
        }
    };

    let connect_time_ms = start.elapsed().as_millis() as u64;
    debug!(
        "Connected to target {} for Trojan connection {} in {}ms",
        trojan_ctx.target_addr, connection_id, connect_time_ms
    );

    // Write any initial payload from the Trojan handshake
    if !initial_payload.is_empty() {
        if let Err(e) =
            tokio::io::AsyncWriteExt::write_all(&mut target_stream, &initial_payload).await
        {
            warn!(
                "Failed to write initial payload for Trojan connection {}: {}",
                connection_id, e
            );
            state.decrement_trojan_connections(trojan_port).await;
            return;
        }
    }

    let proxy_start = Instant::now();

    // Bidirectional copy: TLS stream (already decrypted) <-> target TCP stream
    let result = tokio::io::copy_bidirectional(&mut tls_stream, &mut target_stream).await;

    // Decrement active Trojan connection count
    state.decrement_trojan_connections(trojan_port).await;

    match result {
        Ok((client_to_target, target_to_client)) => {
            let elapsed = proxy_start.elapsed();
            let elapsed_secs = elapsed.as_secs_f64();

            debug!(
                "Trojan connection {} completed: uploaded {} bytes, downloaded {} bytes in {:.2}s",
                connection_id, client_to_target, target_to_client, elapsed_secs
            );

            state
                .traffic_store
                .record_bytes_in(trojan_port, client_to_target)
                .await;
            state
                .traffic_store
                .record_bytes_out(trojan_port, target_to_client)
                .await;

            // Update quality metrics
            update_trojan_quality(
                &state,
                trojan_port,
                connect_time_ms,
                client_to_target,
                target_to_client,
                elapsed_secs,
            )
            .await;
        }
        Err(e) => {
            warn!("Trojan connection {} error: {}", connection_id, e);
        }
    }
}

/// Update quality metrics for Trojan connection
async fn update_trojan_quality(
    state: &ServerState,
    port: u16,
    connect_time_ms: u64,
    bytes_in: u64,
    bytes_out: u64,
    elapsed_secs: f64,
) {
    use crate::server::quality::{
        calculate_quality_score, check_warnings, ConnectionQuality, QualityThresholds,
    };
    use chrono::Utc;

    let mut quality = ConnectionQuality {
        last_rtt_ms: connect_time_ms as f32,
        avg_rtt_ms: connect_time_ms as f32,
        min_rtt_ms: connect_time_ms as f32,
        max_rtt_ms: connect_time_ms as f32,
        ..Default::default()
    };

    if elapsed_secs > 0.0 {
        quality.bytes_in_per_sec = bytes_in as f64 / elapsed_secs;
        quality.bytes_out_per_sec = bytes_out as f64 / elapsed_secs;
    }

    quality.quality_score = calculate_quality_score(quality.avg_rtt_ms, quality.loss_rate);

    let thresholds = QualityThresholds::default();
    let (is_warning, is_critical) =
        check_warnings(quality.avg_rtt_ms, quality.loss_rate, &thresholds);
    quality.is_warning = is_warning;
    quality.is_critical = is_critical;
    quality.last_update = Utc::now();

    state.quality_store.update_quality(port, quality).await;
}

#[cfg(test)]
mod tests {
    use crate::server::control::ServerState;

    #[tokio::test]
    async fn test_ss_traffic_statistics() {
        let state = ServerState::new();

        // Register a shadowsocks port
        assert!(
            state
                .register_shadowsocks(8388, "aes-256-gcm".into(), "password".into())
                .await
        );

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

    }
