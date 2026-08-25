use super::{Backend, ReverseProxyState};
use rust_tunnel_stats::EntityType;
use std::net::SocketAddr;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, info, warn};

/// UDP 回包等待超时：5s，超时即视为后端无响应。
const UDP_RESPONSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// TCP reverse proxy handler
pub struct TcpProxy {
    state: ReverseProxyState,
}

impl TcpProxy {
    /// Create a new TCP proxy
    #[must_use]
    pub fn new(state: ReverseProxyState) -> Self {
        Self { state }
    }

    /// Start TCP proxy listener on the given address
    pub async fn start(
        &self,
        addr: SocketAddr,
        backend: Backend,
        rule_id: String,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let listener = TcpListener::bind(addr).await?;
        info!("TCP proxy listening on {} -> {}", addr, backend.addr);

        // Look up TLS configuration for this rule
        let tls_config = {
            let rules = self.state.rules.lock().await;
            rules
                .get(&rule_id)
                .and_then(|r| r.tls.clone())
                .filter(|t| t.enabled)
        };

        // Get TLS acceptor if TLS is enabled
        let tls_acceptor = if let Some(tls_cfg) = tls_config {
            let domain = tls_cfg.domain.clone().or_else(|| {
                let rules = self.state.rules.blocking_lock();
                rules.get(&rule_id).and_then(|r| r.domains.first().cloned())
            });

            if let Some(domain) = domain {
                if let Some(provider) = self.state.cert_provider() {
                    if let Some(config) = provider.get_tls_server_config(&domain).await {
                        info!("TCP proxy TLS enabled for domain '{}'", domain);
                        Some(TlsAcceptor::from(config))
                    } else {
                        warn!(
                            "No certificate found for domain '{}' on rule {}, running without TLS",
                            domain, rule_id
                        );
                        None
                    }
                } else {
                    warn!(
                    "TLS enabled for rule {} but no certificate provider configured, running without TLS",
                    rule_id
                );
                    None
                }
            } else {
                warn!(
                    "TLS enabled for rule {} but no domain configured, running without TLS",
                    rule_id
                );
                None
            }
        } else {
            None
        };

        let state = self.state.clone();
        let rule_id_clone = rule_id.clone();

        let handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, peer_addr)) => {
                        let state = state.clone();
                        let backend = backend.clone();
                        let rule_id = rule_id_clone.clone();
                        let tls_acceptor = tls_acceptor.clone();

                        tokio::spawn(async move {
                            // 统一统计：proxy 桶活跃连接 +1
                            state
                                .stats_collector
                                .incr_conns(EntityType::Proxy, &rule_id);

                            if let Err(e) = handle_tcp_connection(
                                stream,
                                peer_addr,
                                state.clone(),
                                backend,
                                tls_acceptor,
                                rule_id.clone(),
                            )
                            .await
                            {
                                debug!("TCP connection error from {}: {}", peer_addr, e);
                            }

                            // 统一统计：proxy 桶活跃连接 -1（覆盖正常与错误退出）
                            state
                                .stats_collector
                                .decr_conns(EntityType::Proxy, &rule_id);
                        });
                    }
                    Err(e) => {
                        error!("Failed to accept TCP connection: {}", e);
                    }
                }
            }
        });

        // Store the listener handle
        let mut listeners = self.state.tcp_listeners.lock().await;
        listeners.insert(rule_id, handle);

        Ok(())
    }
}

/// Handle a TCP connection, optionally performing TLS termination
async fn handle_tcp_connection(
    client_stream: TcpStream,
    peer_addr: SocketAddr,
    state: ReverseProxyState,
    backend: Backend,
    tls_acceptor: Option<TlsAcceptor>,
    rule_id: String,
) -> Result<(), Box<dyn std::error::Error>> {
    // Dial backend via appropriate connector
    let connector = state.connector_for(&backend).await?;
    let mut backend_stream = connector.connect(&backend).await?;

    // (client→backend, backend→client) 双向字节数，连接结束时统一入账
    let (bytes_c2b, bytes_b2c) = if let Some(acceptor) = tls_acceptor {
        // TLS termination: accept TLS, then bidirectional copy with backend
        let mut tls_stream = match acceptor.accept(client_stream).await {
            Ok(s) => s,
            Err(e) => {
                warn!("TLS handshake failed for {}: {}", peer_addr, e);
                return Ok(());
            }
        };

        let (bytes_c2b, bytes_b2c) =
            tokio::io::copy_bidirectional(&mut tls_stream, &mut *backend_stream)
                .await
                .unwrap_or((0, 0));
        debug!(
            "TCP TLS connection closed from {}: {} bytes client->backend, {} bytes backend->client",
            peer_addr, bytes_c2b, bytes_b2c
        );
        (bytes_c2b, bytes_b2c)
    } else {
        // Plain TCP: direct bidirectional copy
        let mut client_stream = client_stream;
        let (bytes_c2b, bytes_b2c) =
            tokio::io::copy_bidirectional(&mut client_stream, &mut *backend_stream)
                .await
                .unwrap_or((0, 0));
        debug!(
            "TCP connection closed from {}: {} bytes client->backend, {} bytes backend->client",
            peer_addr, bytes_c2b, bytes_b2c
        );
        (bytes_c2b, bytes_b2c)
    };

    // 统一统计：双向字节一次性入账（bytes_in = 客户端→后端，bytes_out = 后端→客户端）
    state
        .stats_collector
        .record_bytes(EntityType::Proxy, &rule_id, bytes_c2b, bytes_b2c);

    // Ensure backend stream is properly closed
    let _ = backend_stream.shutdown().await;

    debug!("TCP connection closed from {}", peer_addr);
    Ok(())
}

/// UDP proxy (simplified - forwards UDP datagrams)
pub struct UdpProxy {
    state: ReverseProxyState,
}

impl UdpProxy {
    /// Create a new UDP proxy
    #[must_use]
    pub fn new(state: ReverseProxyState) -> Self {
        Self { state }
    }

    /// Start UDP proxy on the given address
    pub async fn start(
        &self,
        addr: SocketAddr,
        backend: Backend,
        rule_id: String,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let socket = tokio::net::UdpSocket::bind(addr).await?;
        info!("UDP proxy listening on {} -> {}", addr, backend.addr);

        let state = self.state.clone();
        let rule_id_clone = rule_id.clone();
        let backend_addr = backend.addr; // UDP always uses Direct (Client rejected at rule-save time)

        let handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            loop {
                match socket.recv_from(&mut buf).await {
                    Ok((len, peer_addr)) => {
                        // 统一统计：每个数据报视为一次"连接"，请求字节立即入账
                        state
                            .stats_collector
                            .incr_conns(EntityType::Proxy, &rule_id_clone);
                        state.stats_collector.record_bytes(
                            EntityType::Proxy,
                            &rule_id_clone,
                            len as u64,
                            0,
                        );

                        // Forward to backend
                        if let Ok(backend_socket) = tokio::net::UdpSocket::bind("0.0.0.0:0").await {
                            if let Ok(backend_addr) = backend_addr.parse::<SocketAddr>() {
                                if backend_socket
                                    .send_to(&buf[..len], backend_addr)
                                    .await
                                    .is_ok()
                                {
                                    // Wait for response
                                    let mut response_buf = vec![0u8; 65535];
                                    tokio::select! {
                                        result = backend_socket.recv_from(&mut response_buf) => {
                                            if let Ok((resp_len, _)) = result {
                                                let _ = socket.send_to(&response_buf[..resp_len], peer_addr).await;
                                                // 统一统计：后端响应字节入账
                                                state.stats_collector.record_bytes(
                                                    EntityType::Proxy,
                                                    &rule_id_clone,
                                                    0,
                                                    resp_len as u64,
                                                );
                                            }
                                        }
                                        () = tokio::time::sleep(UDP_RESPONSE_TIMEOUT) => {
                                            debug!("UDP response timeout from backend");
                                        }
                                    }
                                }
                            }
                        }

                        state
                            .stats_collector
                            .decr_conns(EntityType::Proxy, &rule_id_clone);
                    }
                    Err(e) => {
                        error!("UDP receive error: {}", e);
                    }
                }
            }
        });

        // Store the listener handle
        let mut listeners = self.state.tcp_listeners.lock().await;
        listeners.insert(rule_id, handle);

        Ok(())
    }
}
