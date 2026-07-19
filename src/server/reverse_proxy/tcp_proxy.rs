use super::{Backend, ReverseProxyState};
use std::net::SocketAddr;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, info, warn};

/// TCP reverse proxy handler
pub struct TcpProxy {
    state: ReverseProxyState,
}

impl TcpProxy {
    /// Create a new TCP proxy
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

            match domain {
                Some(domain) => match self.state.cert_provider() {
                    Some(provider) => match provider.get_tls_server_config(&domain).await {
                        Some(config) => {
                            info!("TCP proxy TLS enabled for domain '{}'", domain);
                            Some(TlsAcceptor::from(config))
                        }
                        None => {
                            warn!(
                                "No certificate found for domain '{}' on rule {}, running without TLS",
                                domain, rule_id
                            );
                            None
                        }
                    },
                    None => {
                        warn!(
                            "TLS enabled for rule {} but no certificate provider configured, running without TLS",
                            rule_id
                        );
                        None
                    }
                },
                None => {
                    warn!(
                        "TLS enabled for rule {} but no domain configured, running without TLS",
                        rule_id
                    );
                    None
                }
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
                            state.increment_connections(&rule_id).await;

                            if let Err(e) = handle_tcp_connection(
                                stream,
                                peer_addr,
                                state.clone(),
                                backend,
                                tls_acceptor,
                            )
                            .await
                            {
                                debug!("TCP connection error from {}: {}", peer_addr, e);
                            }

                            state.decrement_connections(&rule_id).await;
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
) -> Result<(), Box<dyn std::error::Error>> {
    // Dial backend via appropriate connector
    let connector = state.connector_for(&backend).await?;
    let mut backend_stream = connector.connect(&backend).await?;

    if let Some(acceptor) = tls_acceptor {
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
    }

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
                        state.increment_connections(&rule_id_clone).await;

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
                                            }
                                        }
                                        _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                                            debug!("UDP response timeout from backend");
                                        }
                                    }
                                }
                            }
                        }

                        state.decrement_connections(&rule_id_clone).await;
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
