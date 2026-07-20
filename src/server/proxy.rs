use std::time::Instant;
use tokio::net::TcpStream;
use tokio_rustls::server::TlsStream;
use tracing::{debug, error, warn};

use crate::common::TunnelResult;
use crate::server::control::ServerState;
use crate::server::shadowsocks::{ProxyServerStream, SSConnectionContext};
use crate::server::stats::EntityType;
use crate::server::trojan::{TrojanCommand, TrojanConnectionContext};

/// Bidirectional copy with Shadowsocks encryption/decryption using ProxyServerStream.
///
/// Uses `copy_encrypted_bidirectional` which properly handles TCP shutdown: when one
/// direction observes EOF, it calls `shutdown()` on the opposing writer so the other
/// direction unblocks instead of hanging indefinitely.
async fn copy_bidirectional_with_ss_crypto(
    _connection_id: u64,
    _port: u16,
    mut proxy_stream: ProxyServerStream<TcpStream>,
    mut target_stream: TcpStream,
    _state: ServerState,
) -> TunnelResult<(u64, u64)> {
    use crate::server::shadowsocks::{copy_encrypted_bidirectional, CipherKind};

    // encrypted_to_plain: client → target (upload), plain_to_encrypted: target → client (download)
    let (encrypted_to_plain, plain_to_encrypted) = copy_encrypted_bidirectional(
        CipherKind::AES_256_GCM,
        &mut proxy_stream,
        &mut target_stream,
    )
    .await?;

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
    // 统一统计：shadowsocks 桶活跃连接 +1（entity_id 约定为 ss:{port}）
    let entity_id = format!("ss:{}", ss_port);
    state
        .stats_collector
        .incr_conns(EntityType::Shadowsocks, &entity_id);

    // Record start time for measuring connection setup time (RTT estimate)
    let start = Instant::now();

    // Connect to target server
    let target_stream = match TcpStream::connect(&ss_ctx.target_addr).await {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to connect to target {}: {}", ss_ctx.target_addr, e);
            state.decrement_ss_connections(ss_port).await;
            // 统一统计：活跃连接 -1（覆盖目标连接失败的错误退出路径）
            state
                .stats_collector
                .decr_conns(EntityType::Shadowsocks, &entity_id);
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
    // 统一统计：活跃连接 -1（覆盖正常与错误退出）
    state
        .stats_collector
        .decr_conns(EntityType::Shadowsocks, &entity_id);

    match result {
        Ok((uploaded, downloaded)) => {
            let elapsed_secs = proxy_start.elapsed().as_secs_f64();

            debug!(
                "SS connection {} completed: uploaded {} bytes, downloaded {} bytes in {:.2}s",
                connection_id, uploaded, downloaded, elapsed_secs
            );

            // 统一统计：双向字节一次性入账（bytes_in = 客户端→目标，bytes_out = 目标→客户端）
            state.stats_collector.record_bytes(
                EntityType::Shadowsocks,
                &entity_id,
                uploaded,
                downloaded,
            );
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
    // 统一统计：trojan 桶活跃连接 +1（entity_id 约定为 trojan:{port}）
    let entity_id = format!("trojan:{}", trojan_port);
    state
        .stats_collector
        .incr_conns(EntityType::Trojan, &entity_id);

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
            // 统一统计：活跃连接 -1（覆盖目标连接失败的错误退出路径）
            state
                .stats_collector
                .decr_conns(EntityType::Trojan, &entity_id);
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
            // 统一统计：活跃连接 -1（覆盖初始负载写失败的错误退出路径）
            state
                .stats_collector
                .decr_conns(EntityType::Trojan, &entity_id);
            return;
        }
    }

    let proxy_start = Instant::now();

    // Bidirectional copy: TLS stream (already decrypted) <-> target TCP stream
    let result = tokio::io::copy_bidirectional(&mut tls_stream, &mut target_stream).await;

    // Decrement active Trojan connection count
    state.decrement_trojan_connections(trojan_port).await;
    // 统一统计：活跃连接 -1（覆盖正常与错误退出）
    state
        .stats_collector
        .decr_conns(EntityType::Trojan, &entity_id);

    match result {
        Ok((client_to_target, target_to_client)) => {
            let elapsed_secs = proxy_start.elapsed().as_secs_f64();

            debug!(
                "Trojan connection {} completed: uploaded {} bytes, downloaded {} bytes in {:.2}s",
                connection_id, client_to_target, target_to_client, elapsed_secs
            );

            // 统一统计：双向字节一次性入账（bytes_in = 客户端→目标，bytes_out = 目标→客户端）
            state.stats_collector.record_bytes(
                EntityType::Trojan,
                &entity_id,
                client_to_target,
                target_to_client,
            );
        }
        Err(e) => {
            warn!("Trojan connection {} error: {}", connection_id, e);
        }
    }
}
