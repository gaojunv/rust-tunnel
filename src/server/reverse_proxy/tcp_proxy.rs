use super::ReverseProxyState;
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
        backend_addr: String,
        rule_id: String,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let listener = TcpListener::bind(addr).await?;
        info!("TCP proxy listening on {} -> {}", addr, backend_addr);

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
                        let backend = backend_addr.clone();
                        let rule_id = rule_id_clone.clone();
                        let tls_acceptor = tls_acceptor.clone();

                        tokio::spawn(async move {
                            state.increment_connections(&rule_id).await;

                            if let Err(e) =
                                handle_tcp_connection(stream, peer_addr, &backend, tls_acceptor)
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
    mut client_stream: TcpStream,
    peer_addr: SocketAddr,
    backend_addr: &str,
    tls_acceptor: Option<TlsAcceptor>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Connect to backend
    let mut backend_stream = TcpStream::connect(backend_addr).await?;
    let (mut backend_read, mut backend_write) = backend_stream.split();

    if let Some(acceptor) = tls_acceptor {
        // TLS termination: accept TLS, then bidirectional copy with backend
        let tls_stream = match acceptor.accept(client_stream).await {
            Ok(s) => s,
            Err(e) => {
                warn!("TLS handshake failed for {}: {}", peer_addr, e);
                return Ok(());
            }
        };

        let (mut tls_read, mut tls_write) = tokio::io::split(tls_stream);

        let client_to_backend = async {
            let mut buf = vec![0u8; 8192];
            loop {
                let n = tls_read.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                backend_write.write_all(&buf[..n]).await?;
            }
            backend_write.shutdown().await?;
            Ok::<(), std::io::Error>(())
        };

        let backend_to_client = async {
            let mut buf = vec![0u8; 8192];
            loop {
                let n = backend_read.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                tls_write.write_all(&buf[..n]).await?;
            }
            tls_write.shutdown().await?;
            Ok::<(), std::io::Error>(())
        };

        tokio::select! {
            result = client_to_backend => {
                if let Err(e) = result {
                    debug!("TLS client to backend error: {}", e);
                }
            }
            result = backend_to_client => {
                if let Err(e) = result {
                    debug!("Backend to TLS client error: {}", e);
                }
            }
        }
    } else {
        // Plain TCP: direct bidirectional copy
        let (mut client_read, mut client_write) = client_stream.split();

        let client_to_backend = async {
            let mut buf = vec![0u8; 8192];
            loop {
                let n = client_read.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                backend_write.write_all(&buf[..n]).await?;
            }
            backend_write.shutdown().await?;
            Ok::<(), std::io::Error>(())
        };

        let backend_to_client = async {
            let mut buf = vec![0u8; 8192];
            loop {
                let n = backend_read.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                client_write.write_all(&buf[..n]).await?;
            }
            client_write.shutdown().await?;
            Ok::<(), std::io::Error>(())
        };

        tokio::select! {
            result = client_to_backend => {
                if let Err(e) = result {
                    debug!("Client to backend error: {}", e);
                }
            }
            result = backend_to_client => {
                if let Err(e) = result {
                    debug!("Backend to client error: {}", e);
                }
            }
        }
    }

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
        backend_addr: String,
        rule_id: String,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let socket = tokio::net::UdpSocket::bind(addr).await?;
        info!("UDP proxy listening on {} -> {}", addr, backend_addr);

        let state = self.state.clone();
        let rule_id_clone = rule_id.clone();

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
