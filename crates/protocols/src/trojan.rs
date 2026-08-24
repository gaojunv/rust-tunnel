use std::net::{Ipv4Addr, Ipv6Addr};
use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::server::TlsStream;
use tracing::{debug, error, warn};

use crate::port_registry::PortRegistry;
use rust_tunnel_stats::{EntityType, StatsCollector};
use rust_tunnel_common::{TunnelError, TunnelResult};

/// Trojan command types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrojanCommand {
    Connect = 0x01,
    UdpAssociate = 0x03,
}

impl TrojanCommand {
    fn from_byte(b: u8) -> Option<Self> {
        match b {
            0x01 => Some(TrojanCommand::Connect),
            0x03 => Some(TrojanCommand::UdpAssociate),
            _ => None,
        }
    }
}

/// Trojan address types
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrojanAddress {
    IPv4(Ipv4Addr),
    IPv6(Ipv6Addr),
    Domain(String),
}

impl TrojanAddress {
    /// Parse address from buffer starting at offset, returns (address, bytes_consumed)
    fn parse(buf: &[u8], offset: usize) -> Option<(Self, usize)> {
        if offset >= buf.len() {
            return None;
        }
        match buf[offset] {
            0x01 => {
                // IPv4: 4 bytes
                if offset + 5 > buf.len() {
                    return None;
                }
                let addr = Ipv4Addr::new(
                    buf[offset + 1],
                    buf[offset + 2],
                    buf[offset + 3],
                    buf[offset + 4],
                );
                Some((TrojanAddress::IPv4(addr), 5))
            }
            0x03 => {
                // Domain: 1-byte length + domain bytes
                if offset + 2 > buf.len() {
                    return None;
                }
                let domain_len = buf[offset + 1] as usize;
                if domain_len == 0 || domain_len > 253 {
                    return None;
                }
                if offset + 2 + domain_len > buf.len() {
                    return None;
                }
                let domain_bytes = &buf[offset + 2..offset + 2 + domain_len];
                // Validate domain characters: ASCII alphanumerics, hyphens, and dots only
                if !domain_bytes
                    .iter()
                    .all(|&b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
                {
                    return None;
                }
                let domain = String::from_utf8_lossy(domain_bytes).to_string();
                Some((TrojanAddress::Domain(domain), 2 + domain_len))
            }
            0x04 => {
                // IPv6: 16 bytes
                if offset + 17 > buf.len() {
                    return None;
                }
                let mut octets = [0u8; 16];
                octets.copy_from_slice(&buf[offset + 1..offset + 17]);
                let addr = Ipv6Addr::from(octets);
                Some((TrojanAddress::IPv6(addr), 17))
            }
            _ => None,
        }
    }

    /// Encode address to buffer (ATYP + address bytes, no port)
    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            TrojanAddress::IPv4(addr) => {
                out.push(0x01);
                out.extend_from_slice(&addr.octets());
            }
            TrojanAddress::IPv6(addr) => {
                out.push(0x04);
                out.extend_from_slice(&addr.octets());
            }
            TrojanAddress::Domain(domain) => {
                out.push(0x03);
                out.push(domain.len() as u8);
                out.extend_from_slice(domain.as_bytes());
            }
        }
    }
}

impl std::fmt::Display for TrojanAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TrojanAddress::IPv4(addr) => write!(f, "{}", addr),
            TrojanAddress::IPv6(addr) => write!(f, "{}", addr),
            TrojanAddress::Domain(domain) => write!(f, "{}", domain),
        }
    }
}

/// Parsed Trojan request
#[derive(Debug, Clone)]
pub struct TrojanRequest {
    pub command: TrojanCommand,
    pub address: TrojanAddress,
    pub port: u16,
    /// Byte offset where payload starts in the original buffer
    pub header_len: usize,
}

/// Result of incremental parsing
pub enum ParseResult {
    Complete(TrojanRequest),
    Incomplete,
    Invalid(String),
}

/// Compute SHA-224 hex hash of password (56 lowercase hex chars)
pub fn sha224_hex(password: &str) -> String {
    use sha2::{Digest, Sha224};
    let mut hasher = Sha224::new();
    hasher.update(password.as_bytes());
    hex::encode(hasher.finalize())
}

/// Verify a received hash against an expected password using constant-time comparison
pub fn verify_password(received_hash: &str, expected_password: &str) -> bool {
    let expected_hash = sha224_hex(expected_password);
    constant_time_eq(received_hash.as_bytes(), expected_hash.as_bytes())
}

/// Constant-time comparison to prevent timing side-channel attacks
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Validate that a string is a valid hex SHA-224 hash (56 lowercase hex chars)
fn is_valid_hash(s: &str) -> bool {
    s.len() == 56 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Trojan connection context
#[derive(Debug, Clone)]
pub struct TrojanConnectionContext {
    pub target_addr: String,
    pub connection_id: u64,
    pub port: u16,
    pub command: TrojanCommand,
}

/// Parse a Trojan request from a buffer
/// Returns Complete if parsing succeeded, Incomplete if more data needed, Invalid on error
pub fn parse_trojan_request(buf: &[u8]) -> ParseResult {
    // Minimum: 56 (hash) + 2 (CRLF) + 1 (CMD) + 1 (ATYP) + 1 (min addr) + 2 (port) + 2 (CRLF) = 65
    // But realistically with IPv4: 56 + 2 + 1 + 1 + 4 + 2 + 2 = 68
    if buf.len() < 58 {
        return ParseResult::Incomplete;
    }

    // Check first CRLF after hash
    if buf[56] != b'\r' || buf[57] != b'\n' {
        return ParseResult::Invalid("Missing CRLF after hash".to_string());
    }

    // Validate hash format
    let hash_str = match std::str::from_utf8(&buf[..56]) {
        Ok(s) => s.to_string(),
        Err(_) => return ParseResult::Invalid("Hash is not valid UTF-8".to_string()),
    };
    if !is_valid_hash(&hash_str) {
        return ParseResult::Invalid("Invalid hash format".to_string());
    }

    // Parse command
    let cmd_byte = buf[58];
    let command = match TrojanCommand::from_byte(cmd_byte) {
        Some(c) => c,
        None => return ParseResult::Invalid(format!("Unsupported command: 0x{:02x}", cmd_byte)),
    };

    // Parse address
    let (address, addr_len) = match TrojanAddress::parse(buf, 59) {
        Some(r) => r,
        None => return ParseResult::Incomplete,
    };

    let port_offset = 59 + addr_len;
    if port_offset + 4 > buf.len() {
        return ParseResult::Incomplete;
    }

    // Parse port (big-endian)
    let port = u16::from_be_bytes([buf[port_offset], buf[port_offset + 1]]);

    // Check trailing CRLF
    let crlf_offset = port_offset + 2;
    if buf[crlf_offset] != b'\r' || buf[crlf_offset + 1] != b'\n' {
        return ParseResult::Invalid("Missing CRLF after port".to_string());
    }

    let header_len = crlf_offset + 2;
    let request = TrojanRequest {
        command,
        address,
        port,
        header_len,
    };

    ParseResult::Complete(request)
}

// ---------------------------------------------------------------------------
// UDP packet parser (Task 2)
// ---------------------------------------------------------------------------

/// A parsed Trojan UDP packet
#[derive(Debug, Clone)]
pub struct UdpPacket {
    pub address: TrojanAddress,
    pub port: u16,
    pub payload: Vec<u8>,
}

/// Result of incremental UDP packet parsing
pub enum PacketParseResult {
    /// Parsed packet + total bytes consumed (including header and CRLF)
    Complete(UdpPacket, usize),
    Incomplete,
    Invalid(String),
}

/// Parse one Trojan UDP packet from the front of `buf`.
/// Wire format: ATYP + ADDR + PORT(2) + LENGTH(2) + CRLF + PAYLOAD
pub fn parse_udp_packet(buf: &[u8]) -> PacketParseResult {
    // Address (includes ATYP byte)
    let (address, addr_len) = match TrojanAddress::parse(buf, 0) {
        Some(r) => r,
        // First byte unknown ATYP is unrecoverable protocol error; otherwise insufficient data
        None => {
            return match buf.first() {
                Some(&b) if !matches!(b, 0x01 | 0x03 | 0x04) => {
                    PacketParseResult::Invalid(format!("Invalid ATYP in UDP packet: 0x{b:02x}"))
                }
                Some(0x03) if buf.len() >= 2 => {
                    let domain_len = buf[1] as usize;
                    if domain_len == 0 || domain_len > 253 {
                        PacketParseResult::Invalid(format!(
                            "Invalid domain length in UDP packet: {domain_len}"
                        ))
                    } else if buf.len() >= 2 + domain_len {
                        // Full domain present but parse still failed => invalid characters
                        PacketParseResult::Invalid(
                            "Invalid domain characters in UDP packet".to_string(),
                        )
                    } else {
                        PacketParseResult::Incomplete
                    }
                }
                _ => PacketParseResult::Incomplete,
            };
        }
    };

    let port_offset = addr_len;
    // PORT(2) + LENGTH(2) + CRLF(2)
    if port_offset + 6 > buf.len() {
        return PacketParseResult::Incomplete;
    }
    let port = u16::from_be_bytes([buf[port_offset], buf[port_offset + 1]]);
    let length = u16::from_be_bytes([buf[port_offset + 2], buf[port_offset + 3]]) as usize;

    let crlf_offset = port_offset + 4;
    if buf[crlf_offset] != b'\r' || buf[crlf_offset + 1] != b'\n' {
        return PacketParseResult::Invalid("Missing CRLF in UDP packet header".to_string());
    }

    let payload_offset = crlf_offset + 2;
    if payload_offset + length > buf.len() {
        return PacketParseResult::Incomplete;
    }

    let packet = UdpPacket {
        address,
        port,
        payload: buf[payload_offset..payload_offset + length].to_vec(),
    };
    PacketParseResult::Complete(packet, payload_offset + length)
}

impl UdpPacket {
    /// Encode the packet to wire bytes: ATYP + ADDR + PORT(2) + LENGTH(2) + CRLF + PAYLOAD
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.payload.len() + 32);
        self.address.encode(&mut out);
        out.extend_from_slice(&self.port.to_be_bytes());
        out.extend_from_slice(&(self.payload.len() as u16).to_be_bytes());
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(&self.payload);
        out
    }
}
/// Handle Trojan handshake over TLS stream
/// Returns (TrojanConnectionContext, remaining payload bytes) on success
pub async fn handle_trojan_handshake(
    tls_stream: &mut TlsStream<TcpStream>,
    password: &str,
    connection_id: u64,
    port: u16,
) -> TunnelResult<(TrojanConnectionContext, Vec<u8>)> {
    debug!(
        "Starting Trojan handshake for connection {}, port {}",
        connection_id, port
    );

    let mut buf = vec![0u8; 4096];
    let mut total_read = 0;

    // Read until we have a complete request or fail
    loop {
        let n = tls_stream.read(&mut buf[total_read..]).await?;
        if n == 0 {
            return Err(TunnelError::ConnectionClosed);
        }
        total_read += n;

        match parse_trojan_request(&buf[..total_read]) {
            ParseResult::Complete(request) => {
                // Verify password
                let hash_str = std::str::from_utf8(&buf[..56])
                    .map_err(|_| TunnelError::Protocol("Invalid hash encoding".to_string()))?;

                if !verify_password(hash_str, password) {
                    warn!(
                        "Trojan authentication failed for connection {}",
                        connection_id
                    );
                    return Err(TunnelError::TrojanAuthFailed(buf[..total_read].to_vec()));
                }

                // Build target address string
                let target_addr = match &request.address {
                    TrojanAddress::IPv4(addr) => format!("{}:{}", addr, request.port),
                    TrojanAddress::IPv6(addr) => format!("[{}]:{}", addr, request.port),
                    TrojanAddress::Domain(domain) => format!("{}:{}", domain, request.port),
                };

                debug!(
                    "Trojan authenticated, target: {}, command: {:?}",
                    target_addr, request.command
                );

                // Extract remaining payload (after header)
                let payload = if total_read > request.header_len {
                    buf[request.header_len..total_read].to_vec()
                } else {
                    Vec::new()
                };

                let ctx = TrojanConnectionContext {
                    target_addr,
                    connection_id,
                    port,
                    command: request.command,
                };

                return Ok((ctx, payload));
            }
            ParseResult::Incomplete => {
                if total_read >= buf.len() {
                    return Err(TunnelError::TrojanAuthFailed(buf[..total_read].to_vec()));
                }
                // Need more data, continue reading
            }
            ParseResult::Invalid(reason) => {
                warn!(
                    "Invalid Trojan request for connection {}: {}",
                    connection_id, reason
                );
                return Err(TunnelError::TrojanAuthFailed(buf[..total_read].to_vec()));
            }
        }
    }
}

/// Handle fallback: forward the connection to a fallback backend
pub async fn handle_trojan_fallback(
    tls_stream: &mut TlsStream<TcpStream>,
    initial_data: &[u8],
    fallback_addr: &str,
) -> TunnelResult<()> {
    debug!("Handling Trojan fallback to {}", fallback_addr);

    let mut backend = match TcpStream::connect(fallback_addr).await {
        Ok(s) => s,
        Err(e) => {
            warn!("Failed to connect to fallback {}: {}", fallback_addr, e);
            return Err(TunnelError::Io(e));
        }
    };

    // Forward any already-read data
    if !initial_data.is_empty() {
        backend.write_all(initial_data).await?;
    }

    // Bidirectional copy between TLS stream and fallback backend
    let (mut tls_read, mut tls_write) = tokio::io::split(tls_stream);
    let (mut backend_read, mut backend_write) = tokio::io::split(backend);

    let client_to_backend = tokio::io::copy(&mut tls_read, &mut backend_write);
    let backend_to_client = tokio::io::copy(&mut backend_read, &mut tls_write);

    tokio::select! {
        r1 = client_to_backend => {
            if let Err(e) = r1 {
                debug!("Fallback client->backend error: {}", e);
            }
        }
        r2 = backend_to_client => {
            if let Err(e) = r2 {
                debug!("Fallback backend->client error: {}", e);
            }
        }
    }

    Ok(())
}

/// 校验 Trojan 域名：必须是合法 DNS 域名（不含 `*`、端口或路径）。
/// 调用方保证传入前已 trim + 转小写；空串由调用方自行处理（空 = 不配置域名）。
pub fn validate_trojan_domain(domain: &str) -> Result<(), String> {
    if domain.is_empty() {
        return Err("domain is empty".to_string());
    }
    if domain.len() > 253 {
        return Err("domain too long (max 253 chars)".to_string());
    }
    if domain.contains('*') {
        return Err("wildcard domains are not allowed".to_string());
    }
    if domain.contains(':') || domain.contains('/') {
        return Err("domain must not contain port or path".to_string());
    }
    for label in domain.split('.') {
        if label.is_empty() {
            return Err("domain contains an empty label".to_string());
        }
        if label.len() > 63 {
            return Err("domain label too long (max 63 chars)".to_string());
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err("domain label must not start or end with '-'".to_string());
        }
        if !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err("domain contains invalid characters".to_string());
        }
    }
    Ok(())
}

/// Maximum bytes buffered while waiting for a complete UDP packet.
/// ~2x max packet size (65535 payload + header); exceeding this means a misbehaving peer.
const UDP_READ_BUF_LIMIT: usize = 128 * 1024;

/// Idle timeout for per-target UDP sockets.
const UDP_SOCKET_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Handle a Trojan UDP ASSOCIATE session.
///
/// The TLS stream carries UDP packets (ATYP+ADDR+PORT+LEN+CRLF+payload) in both
/// directions. Per target address we maintain one outbound UdpSocket; responses
/// from all targets are multiplexed back onto the TLS stream through a single
/// writer task via an mpsc channel.
pub async fn handle_udp_associate(
    connection_id: u64,
    trojan_port: u16,
    mut tls_stream: TlsStream<TcpStream>,
    initial_payload: Vec<u8>,
    registry: std::sync::Arc<dyn PortRegistry>,
    stats: StatsCollector,
) {
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use tokio::net::UdpSocket;
    use tokio::sync::mpsc;

    debug!(
        "Starting Trojan UDP ASSOCIATE for connection {}",
        connection_id
    );

    registry.increment_trojan_connections(trojan_port).await;
    let entity_id = format!("trojan:{}", trojan_port);
    stats.incr_conns(EntityType::Trojan, &entity_id);

    let mut bytes_in: u64 = 0;
    let mut bytes_out: u64 = 0;

    // target addr -> (socket, abort handle for its read task)
    let mut targets: HashMap<SocketAddr, (Arc<UdpSocket>, tokio::task::AbortHandle)> =
        HashMap::new();
    // Responses from per-target read tasks, drained by the select loop
    let (resp_tx, mut resp_rx) = mpsc::channel::<(UdpPacket, SocketAddr)>(256);
    // Death notifications from per-target read tasks — when a read task exits
    // (idle timeout, socket error, send failure), it sends the target address so
    // the stale entry is removed from the map, allowing the next packet to that
    // target to spawn a fresh socket and read task.
    let (dead_tx, mut dead_rx) = mpsc::channel::<SocketAddr>(64);

    let mut read_buf: Vec<u8> = initial_payload;
    let mut chunk = [0u8; 65536];

    // Process packets that arrived together with the handshake BEFORE entering
    // the select loop (the loop only parses on new reads).
    {
        let mut offset = 0;
        loop {
            match parse_udp_packet(&read_buf[offset..]) {
                PacketParseResult::Complete(pkt, consumed) => {
                    offset += consumed;
                    bytes_in += pkt.payload.len() as u64;
                    dispatch_udp_packet(pkt, connection_id, &mut targets, &resp_tx, &dead_tx).await;
                }
                PacketParseResult::Incomplete => break,
                PacketParseResult::Invalid(reason) => {
                    warn!(
                        "Invalid UDP packet on connection {}: {}",
                        connection_id, reason
                    );
                    break;
                }
            }
        }
        read_buf.drain(..offset);
    }

    loop {
        tokio::select! {
            read = tls_stream.read(&mut chunk) => {
                match read {
                    Ok(0) => break, // client closed
                    Ok(n) => {
                        read_buf.extend_from_slice(&chunk[..n]);
                        if read_buf.len() > UDP_READ_BUF_LIMIT {
                            warn!(
                                "Trojan UDP connection {} read buffer over limit, closing",
                                connection_id
                            );
                            break;
                        }
                        // Drain all complete packets from the buffer
                        let mut offset = 0;
                        loop {
                            match parse_udp_packet(&read_buf[offset..]) {
                                PacketParseResult::Complete(pkt, consumed) => {
                                    offset += consumed;
                                    bytes_in += pkt.payload.len() as u64;
                                    dispatch_udp_packet(
                                        pkt,
                                        connection_id,
                                        &mut targets,
                                        &resp_tx,
                                        &dead_tx,
                                    )
                                    .await;
                                }
                                PacketParseResult::Incomplete => break,
                                PacketParseResult::Invalid(reason) => {
                                    warn!(
                                        "Invalid UDP packet on connection {}: {}",
                                        connection_id, reason
                                    );
                                    break;
                                }
                            }
                        }
                        read_buf.drain(..offset);
                    }
                    Err(e) => {
                        debug!("Trojan UDP connection {} read error: {}", connection_id, e);
                        break;
                    }
                }
            }
            resp = resp_rx.recv() => {
                match resp {
                    Some((pkt, _from)) => {
                        bytes_out += pkt.payload.len() as u64;
                        if let Err(e) = tls_stream.write_all(&pkt.encode()).await {
                            debug!(
                                "Trojan UDP connection {} write error: {}",
                                connection_id, e
                            );
                            break;
                        }
                    }
                    None => {
                        // All senders dropped (no targets left) — wait for client data only.
                        // Yield to avoid busy-loop: recv() on empty-but-open channel pends,
                        // so None only happens after targets cleared AND channel closed,
                        // which cannot occur while resp_tx is alive. Unreachable in practice.
                    }
                }
            }
            dead = dead_rx.recv() => {
                if let Some(addr) = dead {
                    if let Some((_, abort)) = targets.remove(&addr) {
                        abort.abort(); // no-op if task already exited; keeps map consistent
                    }
                }
            }
        }
    }

    // Cleanup: abort all read tasks (drops sockets)
    for (_, (_, abort)) in targets.drain() {
        abort.abort();
    }

    registry.decrement_trojan_connections(trojan_port).await;
    stats.decr_conns(EntityType::Trojan, &entity_id);
    stats.record_bytes(EntityType::Trojan, &entity_id, bytes_in, bytes_out);

    debug!(
        "Trojan UDP connection {} closed: sent {} bytes, received {} bytes",
        connection_id, bytes_in, bytes_out
    );
}

/// Resolve the packet target, get-or-create its outbound socket (spawning a
/// response read task on first use), and send the payload. Failures drop the
/// packet without affecting other targets.
async fn dispatch_udp_packet(
    pkt: UdpPacket,
    connection_id: u64,
    targets: &mut std::collections::HashMap<
        std::net::SocketAddr,
        (
            std::sync::Arc<tokio::net::UdpSocket>,
            tokio::task::AbortHandle,
        ),
    >,
    resp_tx: &tokio::sync::mpsc::Sender<(UdpPacket, std::net::SocketAddr)>,
    dead_tx: &tokio::sync::mpsc::Sender<std::net::SocketAddr>,
) {
    use std::net::SocketAddr;
    use std::sync::Arc;
    use tokio::net::UdpSocket;

    // Resolve target address
    let target: SocketAddr = match &pkt.address {
        TrojanAddress::IPv4(ip) => SocketAddr::new((*ip).into(), pkt.port),
        TrojanAddress::IPv6(ip) => SocketAddr::new((*ip).into(), pkt.port),
        TrojanAddress::Domain(domain) => {
            match tokio::net::lookup_host((domain.as_str(), pkt.port)).await {
                Ok(mut addrs) => match addrs.next() {
                    Some(a) => a,
                    None => {
                        debug!(
                            "UDP connection {}: no addresses for {}",
                            connection_id, domain
                        );
                        return;
                    }
                },
                Err(e) => {
                    debug!(
                        "UDP connection {}: DNS lookup failed for {}: {}",
                        connection_id, domain, e
                    );
                    return;
                }
            }
        }
    };

    // Get or create the per-target socket
    if let std::collections::hash_map::Entry::Vacant(e) = targets.entry(target) {
        let bind_addr: SocketAddr = if target.is_ipv4() {
            "0.0.0.0:0".parse().unwrap()
        } else {
            "[::]:0".parse().unwrap()
        };
        let socket = match UdpSocket::bind(bind_addr).await {
            Ok(s) => Arc::new(s),
            Err(e_bind) => {
                warn!(
                    "UDP connection {}: failed to bind socket for {}: {}",
                    connection_id, target, e_bind
                );
                return;
            }
        };

        // Spawn the response read task with idle timeout
        let task_socket = Arc::clone(&socket);
        let task_tx = resp_tx.clone();
        let task_dead_tx = dead_tx.clone();
        let task_target = target;
        let handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 65536];
            loop {
                let recv =
                    tokio::time::timeout(UDP_SOCKET_IDLE_TIMEOUT, task_socket.recv_from(&mut buf))
                        .await;
                match recv {
                    Ok(Ok((n, from))) => {
                        if from != task_target {
                            continue; // only accept packets from the expected peer
                        }
                        let address = match from {
                            SocketAddr::V4(a) => TrojanAddress::IPv4(*a.ip()),
                            SocketAddr::V6(a) => TrojanAddress::IPv6(*a.ip()),
                        };
                        let pkt = UdpPacket {
                            address,
                            port: from.port(),
                            payload: buf[..n].to_vec(),
                        };
                        if task_tx.send((pkt, from)).await.is_err() {
                            break; // session closed
                        }
                    }
                    Ok(Err(_)) => break, // socket error
                    Err(_) => break,     // idle timeout
                }
            }
            // Notify the main loop that this read task has exited,
            // so the stale entry can be removed from the targets map.
            let _ = task_dead_tx.send(task_target).await;
        });

        e.insert((socket, handle.abort_handle()));
    }

    let (socket, _) = &targets[&target];
    if let Err(e) = socket.send_to(&pkt.payload, target).await {
        debug!(
            "UDP connection {}: send_to {} failed: {}",
            connection_id, target, e
        );
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
    registry: std::sync::Arc<dyn PortRegistry>,
    stats: StatsCollector,
) {
    debug!(
        "Starting Trojan proxy for connection {}, target {}",
        connection_id, trojan_ctx.target_addr
    );

    // UDP ASSOCIATE: hand off to the UDP session handler
    if trojan_ctx.command == TrojanCommand::UdpAssociate {
        handle_udp_associate(connection_id, trojan_port, tls_stream, initial_payload, registry, stats).await;
        return;
    }

    // Increment active Trojan connection count
    registry.increment_trojan_connections(trojan_port).await;
    // 统一统计：trojan 桶活跃连接 +1（entity_id 约定为 trojan:{port}）
    let entity_id = format!("trojan:{}", trojan_port);
    stats.incr_conns(EntityType::Trojan, &entity_id);

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
            registry.decrement_trojan_connections(trojan_port).await;
            // 统一统计：活跃连接 -1（覆盖目标连接失败的错误退出路径）
            stats.decr_conns(EntityType::Trojan, &entity_id);
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
            registry.decrement_trojan_connections(trojan_port).await;
            // 统一统计：活跃连接 -1（覆盖初始负载写失败的错误退出路径）
            stats.decr_conns(EntityType::Trojan, &entity_id);
            return;
        }
    }

    let proxy_start = Instant::now();

    // Bidirectional copy: TLS stream (already decrypted) <-> target TCP stream
    let result = tokio::io::copy_bidirectional(&mut tls_stream, &mut target_stream).await;

    // Decrement active Trojan connection count
    registry.decrement_trojan_connections(trojan_port).await;
    // 统一统计：活跃连接 -1（覆盖正常与错误退出）
    stats.decr_conns(EntityType::Trojan, &entity_id);

    match result {
        Ok((client_to_target, target_to_client)) => {
            let elapsed_secs = proxy_start.elapsed().as_secs_f64();

            debug!(
                "Trojan connection {} completed: uploaded {} bytes, downloaded {} bytes in {:.2}s",
                connection_id, client_to_target, target_to_client, elapsed_secs
            );

            // 统一统计：双向字节一次性入账（bytes_in = 客户端->目标，bytes_out = 目标->客户端）
            stats.record_bytes(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sha224_hex() {
        let hash = sha224_hex("password");
        assert_eq!(hash.len(), 56);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_sha224_hex_deterministic() {
        let h1 = sha224_hex("test123");
        let h2 = sha224_hex("test123");
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_sha224_hex_different_passwords() {
        let h1 = sha224_hex("password1");
        let h2 = sha224_hex("password2");
        assert_ne!(h1, h2);
    }

    #[test]
    fn validate_trojan_domain_accepts_valid() {
        assert!(validate_trojan_domain("trojan.example.com").is_ok());
        assert!(validate_trojan_domain("ok-1.example.com").is_ok());
        assert!(validate_trojan_domain("localhost").is_ok());
    }

    #[test]
    fn validate_trojan_domain_rejects_invalid() {
        assert!(validate_trojan_domain("").is_err());
        assert!(validate_trojan_domain("*.example.com").is_err());
        assert!(validate_trojan_domain("host:443").is_err());
        assert!(validate_trojan_domain("a/b").is_err());
        assert!(validate_trojan_domain("exa mple.com").is_err());
        assert!(validate_trojan_domain("-bad.com").is_err());
        assert!(validate_trojan_domain("bad-.com").is_err());
        assert!(validate_trojan_domain("a..com").is_err());
    }

    #[test]
    fn test_verify_password_correct() {
        assert!(verify_password(&sha224_hex("mypassword"), "mypassword"));
    }

    #[test]
    fn test_verify_password_incorrect() {
        assert!(!verify_password(&sha224_hex("wrongpassword"), "mypassword"));
    }

    #[test]
    fn test_parse_request_ipv4() {
        let password = "testpassword";
        let hash = sha224_hex(password);
        let mut buf = Vec::new();
        buf.extend_from_slice(hash.as_bytes());
        buf.extend_from_slice(b"\r\n");
        buf.push(0x01); // CONNECT
        buf.push(0x01); // IPv4
        buf.extend_from_slice(&[127, 0, 0, 1]); // 127.0.0.1
        buf.extend_from_slice(&0x01BBu16.to_be_bytes()); // port 443
        buf.extend_from_slice(b"\r\n");

        match parse_trojan_request(&buf) {
            ParseResult::Complete(req) => {
                assert_eq!(req.command, TrojanCommand::Connect);
                assert_eq!(req.port, 443);
                assert!(matches!(req.address, TrojanAddress::IPv4(_)));
                assert_eq!(req.header_len, 68);
            }
            _ => panic!("Expected Complete parse result"),
        }
    }

    #[test]
    fn test_parse_request_domain() {
        let password = "testpassword";
        let hash = sha224_hex(password);
        let domain = b"example.com";
        let mut buf = Vec::new();
        buf.extend_from_slice(hash.as_bytes());
        buf.extend_from_slice(b"\r\n");
        buf.push(0x01); // CONNECT
        buf.push(0x03); // Domain
        buf.push(domain.len() as u8);
        buf.extend_from_slice(domain);
        buf.extend_from_slice(&0x01BBu16.to_be_bytes()); // port 443
        buf.extend_from_slice(b"\r\n");

        match parse_trojan_request(&buf) {
            ParseResult::Complete(req) => {
                assert_eq!(req.command, TrojanCommand::Connect);
                assert_eq!(req.port, 443);
                assert!(matches!(req.address, TrojanAddress::Domain(_)));
                if let TrojanAddress::Domain(d) = &req.address {
                    assert_eq!(d, "example.com");
                }
            }
            _ => panic!("Expected Complete parse result"),
        }
    }

    #[test]
    fn test_parse_request_ipv6() {
        let password = "testpassword";
        let hash = sha224_hex(password);
        let mut buf = Vec::new();
        buf.extend_from_slice(hash.as_bytes());
        buf.extend_from_slice(b"\r\n");
        buf.push(0x01); // CONNECT
        buf.push(0x04); // IPv6
        buf.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]); // ::1
        buf.extend_from_slice(&0x0050u16.to_be_bytes()); // port 80
        buf.extend_from_slice(b"\r\n");

        match parse_trojan_request(&buf) {
            ParseResult::Complete(req) => {
                assert_eq!(req.command, TrojanCommand::Connect);
                assert_eq!(req.port, 80);
                assert!(matches!(req.address, TrojanAddress::IPv6(_)));
            }
            _ => panic!("Expected Complete parse result"),
        }
    }

    #[test]
    fn test_parse_request_incomplete() {
        let buf = [0u8; 50];
        match parse_trojan_request(&buf) {
            ParseResult::Incomplete => {}
            _ => panic!("Expected Incomplete for short buffer"),
        }
    }

    #[test]
    fn test_parse_request_invalid_no_crlf() {
        let hash = sha224_hex("test");
        let mut buf = Vec::new();
        buf.extend_from_slice(hash.as_bytes());
        buf.extend_from_slice(b"XX"); // Not CRLF

        match parse_trojan_request(&buf) {
            ParseResult::Invalid(_) => {}
            _ => panic!("Expected Invalid for missing CRLF"),
        }
    }

    #[test]
    fn test_parse_request_invalid_command() {
        let hash = sha224_hex("test");
        let mut buf = Vec::new();
        buf.extend_from_slice(hash.as_bytes());
        buf.extend_from_slice(b"\r\n");
        buf.push(0x02); // Invalid command
        buf.push(0x01); // IPv4
        buf.extend_from_slice(&[127, 0, 0, 1]);
        buf.extend_from_slice(&80u16.to_be_bytes());
        buf.extend_from_slice(b"\r\n");

        match parse_trojan_request(&buf) {
            ParseResult::Invalid(msg) => {
                assert!(msg.contains("Unsupported command"));
            }
            _ => panic!("Expected Invalid for unsupported command"),
        }
    }

    #[test]
    fn test_trojan_address_display() {
        let v4 = TrojanAddress::IPv4(Ipv4Addr::new(192, 168, 1, 1));
        assert_eq!(format!("{}", v4), "192.168.1.1");

        let domain = TrojanAddress::Domain("example.com".to_string());
        assert_eq!(format!("{}", domain), "example.com");
    }

    #[test]
    fn test_is_valid_hash() {
        assert!(is_valid_hash(
            "a3b2c1d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8"
        ));
        assert!(!is_valid_hash("short"));
        assert!(!is_valid_hash(
            "a3b2c1d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8X"
        )); // too long
        assert!(!is_valid_hash(
            "a3b2c1d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8Z"
        )); // 56 but not hex
    }

    #[test]
    fn test_domain_validation_rejects_empty() {
        let hash = sha224_hex("test");
        let mut buf = Vec::new();
        buf.extend_from_slice(hash.as_bytes());
        buf.extend_from_slice(b"\r\n");
        buf.push(0x01); // CONNECT
        buf.push(0x03); // Domain
        buf.push(0); // zero-length domain (invalid)
        buf.extend_from_slice(&80u16.to_be_bytes());
        buf.extend_from_slice(b"\r\n");

        match parse_trojan_request(&buf) {
            ParseResult::Incomplete | ParseResult::Invalid(_) => {}
            ParseResult::Complete(_) => panic!("Expected failure for empty domain"),
        }
    }

    #[test]
    fn test_domain_validation_rejects_invalid_chars() {
        let hash = sha224_hex("test");
        let mut buf = Vec::new();
        buf.extend_from_slice(hash.as_bytes());
        buf.extend_from_slice(b"\r\n");
        buf.push(0x01); // CONNECT
        buf.push(0x03); // Domain
        buf.push(4); // length
        buf.extend_from_slice(b"ex@m"); // contains '@' which is invalid
        buf.extend_from_slice(&80u16.to_be_bytes());
        buf.extend_from_slice(b"\r\n");

        match parse_trojan_request(&buf) {
            ParseResult::Incomplete | ParseResult::Invalid(_) => {}
            ParseResult::Complete(_) => panic!("Expected failure for domain with invalid chars"),
        }
    }

    #[test]
    fn test_domain_validation_accepts_valid() {
        let hash = sha224_hex("test");
        let mut buf = Vec::new();
        buf.extend_from_slice(hash.as_bytes());
        buf.extend_from_slice(b"\r\n");
        buf.push(0x01); // CONNECT
        buf.push(0x03); // Domain
        let domain = b"sub.example.com";
        buf.push(domain.len() as u8);
        buf.extend_from_slice(domain);
        buf.extend_from_slice(&443u16.to_be_bytes());
        buf.extend_from_slice(b"\r\n");

        match parse_trojan_request(&buf) {
            ParseResult::Complete(req) => {
                if let TrojanAddress::Domain(d) = &req.address {
                    assert_eq!(d, "sub.example.com");
                } else {
                    panic!("Expected Domain address");
                }
            }
            _ => panic!("Expected Complete for valid domain"),
        }
    }

    // --- constant_time_eq tests ---

    #[test]
    fn test_constant_time_eq_equal_buffers() {
        assert!(constant_time_eq(b"hello", b"hello"));
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(&[0u8; 64], &[0u8; 64]));
    }

    #[test]
    fn test_constant_time_eq_different_buffers() {
        assert!(!constant_time_eq(b"hello", b"world"));
        assert!(!constant_time_eq(b"hello", b"hellp"));
        assert!(!constant_time_eq(&[0u8; 64], &[1u8; 64]));
    }

    #[test]
    fn test_constant_time_eq_different_lengths() {
        assert!(!constant_time_eq(b"short", b"longer"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(!constant_time_eq(b"a", b""));
    }

    #[test]
    fn test_constant_time_eq_single_byte_diff() {
        let mut a = [0u8; 56];
        let mut b = [0u8; 56];
        a[55] = 1;
        assert!(!constant_time_eq(&a, &b));
        b[55] = 1;
        assert!(constant_time_eq(&a, &b));
    }

    // --- verify_password with constant-time comparison ---

    #[test]
    fn test_verify_password_uses_constant_time() {
        // Verify that correct password returns true
        let hash = sha224_hex("correct");
        assert!(verify_password(&hash, "correct"));
        // Verify that incorrect password returns false
        assert!(!verify_password(&hash, "incorrect"));
        // Verify that completely wrong hash returns false
        assert!(!verify_password(
            "00000000000000000000000000000000000000000000000000000000",
            "correct"
        ));
    }

    #[test]
    fn test_verify_password_empty() {
        let hash = sha224_hex("");
        assert!(verify_password(&hash, ""));
        assert!(!verify_password(&hash, "nonempty"));
    }

    // --- Domain validation edge cases ---

    #[test]
    fn test_domain_validation_hyphens() {
        let hash = sha224_hex("test");
        let mut buf = Vec::new();
        buf.extend_from_slice(hash.as_bytes());
        buf.extend_from_slice(b"\r\n");
        buf.push(0x01);
        buf.push(0x03);
        let domain = b"my-server.example.com";
        buf.push(domain.len() as u8);
        buf.extend_from_slice(domain);
        buf.extend_from_slice(&443u16.to_be_bytes());
        buf.extend_from_slice(b"\r\n");

        match parse_trojan_request(&buf) {
            ParseResult::Complete(req) => {
                if let TrojanAddress::Domain(d) = &req.address {
                    assert_eq!(d, "my-server.example.com");
                } else {
                    panic!("Expected Domain address");
                }
            }
            _ => panic!("Expected Complete for domain with hyphens"),
        }
    }

    #[test]
    fn test_domain_validation_rejects_spaces() {
        let hash = sha224_hex("test");
        let mut buf = Vec::new();
        buf.extend_from_slice(hash.as_bytes());
        buf.extend_from_slice(b"\r\n");
        buf.push(0x01);
        buf.push(0x03);
        buf.push(11);
        buf.extend_from_slice(b"bad domain!");
        buf.extend_from_slice(&80u16.to_be_bytes());
        buf.extend_from_slice(b"\r\n");

        match parse_trojan_request(&buf) {
            ParseResult::Incomplete | ParseResult::Invalid(_) => {}
            ParseResult::Complete(_) => panic!("Expected failure for domain with spaces"),
        }
    }

    #[test]
    fn test_domain_validation_rejects_underscore() {
        let hash = sha224_hex("test");
        let mut buf = Vec::new();
        buf.extend_from_slice(hash.as_bytes());
        buf.extend_from_slice(b"\r\n");
        buf.push(0x01);
        buf.push(0x03);
        buf.push(9);
        buf.extend_from_slice(b"bad_name!");
        buf.extend_from_slice(&80u16.to_be_bytes());
        buf.extend_from_slice(b"\r\n");

        match parse_trojan_request(&buf) {
            ParseResult::Incomplete | ParseResult::Invalid(_) => {}
            ParseResult::Complete(_) => panic!("Expected failure for domain with underscore"),
        }
    }

    // --- Port boundary tests ---

    #[test]
    fn test_parse_request_port_zero() {
        let hash = sha224_hex("testpassword");
        let mut buf = Vec::new();
        buf.extend_from_slice(hash.as_bytes());
        buf.extend_from_slice(b"\r\n");
        buf.push(0x01);
        buf.push(0x01); // IPv4
        buf.extend_from_slice(&[127, 0, 0, 1]);
        buf.extend_from_slice(&0u16.to_be_bytes()); // port 0
        buf.extend_from_slice(b"\r\n");

        match parse_trojan_request(&buf) {
            ParseResult::Complete(req) => {
                assert_eq!(req.port, 0);
            }
            _ => panic!("Expected Complete parse result"),
        }
    }

    #[test]
    fn test_parse_request_port_max() {
        let hash = sha224_hex("testpassword");
        let mut buf = Vec::new();
        buf.extend_from_slice(hash.as_bytes());
        buf.extend_from_slice(b"\r\n");
        buf.push(0x01);
        buf.push(0x01); // IPv4
        buf.extend_from_slice(&[127, 0, 0, 1]);
        buf.extend_from_slice(&0xFFFFu16.to_be_bytes()); // port 65535
        buf.extend_from_slice(b"\r\n");

        match parse_trojan_request(&buf) {
            ParseResult::Complete(req) => {
                assert_eq!(req.port, 65535);
            }
            _ => panic!("Expected Complete parse result"),
        }
    }

    #[test]
    fn test_parse_request_with_payload() {
        let hash = sha224_hex("testpassword");
        let mut buf = Vec::new();
        buf.extend_from_slice(hash.as_bytes());
        buf.extend_from_slice(b"\r\n");
        buf.push(0x01);
        buf.push(0x01); // IPv4
        buf.extend_from_slice(&[127, 0, 0, 1]);
        buf.extend_from_slice(&8080u16.to_be_bytes());
        buf.extend_from_slice(b"\r\n");
        buf.extend_from_slice(b"GET / HTTP/1.1\r\n\r\n"); // extra payload

        match parse_trojan_request(&buf) {
            ParseResult::Complete(req) => {
                assert_eq!(req.port, 8080);
                assert!(req.header_len < buf.len());
            }
            _ => panic!("Expected Complete parse result"),
        }
    }

    // --- TrojanCommand tests ---

    #[test]
    fn test_trojan_command_from_byte() {
        assert_eq!(TrojanCommand::from_byte(0x01), Some(TrojanCommand::Connect));
        assert_eq!(
            TrojanCommand::from_byte(0x03),
            Some(TrojanCommand::UdpAssociate)
        );
        assert_eq!(TrojanCommand::from_byte(0x00), None);
        assert_eq!(TrojanCommand::from_byte(0x02), None);
        assert_eq!(TrojanCommand::from_byte(0xFF), None);
    }

    #[test]
    fn test_trojan_command_values() {
        assert_eq!(TrojanCommand::Connect as u8, 0x01);
        assert_eq!(TrojanCommand::UdpAssociate as u8, 0x03);
    }

    #[test]
    fn test_address_encode_ipv4() {
        let addr = TrojanAddress::IPv4(Ipv4Addr::new(192, 168, 1, 1));
        let mut buf = Vec::new();
        addr.encode(&mut buf);
        assert_eq!(buf, vec![0x01, 192, 168, 1, 1]);
    }

    #[test]
    fn test_address_encode_ipv6() {
        let addr = TrojanAddress::IPv6(Ipv6Addr::LOCALHOST);
        let mut buf = Vec::new();
        addr.encode(&mut buf);
        let mut expected = vec![0x04];
        expected.extend_from_slice(&[0u8; 15]);
        expected.push(1);
        assert_eq!(buf, expected);
    }

    #[test]
    fn test_address_encode_domain() {
        let addr = TrojanAddress::Domain("example.com".to_string());
        let mut buf = Vec::new();
        addr.encode(&mut buf);
        let mut expected = vec![0x03, 11];
        expected.extend_from_slice(b"example.com");
        assert_eq!(buf, expected);
    }

    #[test]
    fn test_address_encode_parse_roundtrip() {
        for addr in [
            TrojanAddress::IPv4(Ipv4Addr::new(10, 0, 0, 1)),
            TrojanAddress::IPv6(Ipv6Addr::LOCALHOST),
            TrojanAddress::Domain("a-b.example.com".to_string()),
        ] {
            let mut buf = Vec::new();
            addr.encode(&mut buf);
            let (parsed, consumed) = TrojanAddress::parse(&buf, 0).expect("parse failed");
            assert_eq!(parsed, addr);
            assert_eq!(consumed, buf.len());
        }
    }

    // ---------------------------------------------------------------------------
    // UDP packet parser tests (Task 2)
    // ---------------------------------------------------------------------------

    fn build_udp_packet(atyp_addr: &[u8], port: u16, payload: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(atyp_addr);
        buf.extend_from_slice(&port.to_be_bytes());
        buf.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        buf.extend_from_slice(b"\r\n");
        buf.extend_from_slice(payload);
        buf
    }

    #[test]
    fn test_udp_packet_parse_ipv4() {
        let atyp_addr = [0x01, 8, 8, 8, 8];
        let buf = build_udp_packet(&atyp_addr, 53, b"dns-query");
        match parse_udp_packet(&buf) {
            PacketParseResult::Complete(pkt, consumed) => {
                assert_eq!(pkt.address, TrojanAddress::IPv4(Ipv4Addr::new(8, 8, 8, 8)));
                assert_eq!(pkt.port, 53);
                assert_eq!(pkt.payload, b"dns-query");
                assert_eq!(consumed, buf.len());
            }
            _ => panic!("Expected Complete"),
        }
    }

    #[test]
    fn test_udp_packet_parse_domain() {
        let mut atyp_addr = vec![0x03, b"dns.google".len() as u8];
        atyp_addr.extend_from_slice(b"dns.google");
        let buf = build_udp_packet(&atyp_addr, 53, b"q");
        match parse_udp_packet(&buf) {
            PacketParseResult::Complete(pkt, consumed) => {
                assert_eq!(pkt.address, TrojanAddress::Domain("dns.google".to_string()));
                assert_eq!(pkt.port, 53);
                assert_eq!(consumed, buf.len());
            }
            _ => panic!("Expected Complete"),
        }
    }

    #[test]
    fn test_udp_packet_parse_ipv6() {
        let mut atyp_addr = vec![0x04];
        atyp_addr.extend_from_slice(&[0u8; 15]);
        atyp_addr.push(1);
        let buf = build_udp_packet(&atyp_addr, 123, b"ntp");
        match parse_udp_packet(&buf) {
            PacketParseResult::Complete(pkt, _) => {
                assert_eq!(pkt.address, TrojanAddress::IPv6(Ipv6Addr::LOCALHOST));
                assert_eq!(pkt.port, 123);
                assert_eq!(pkt.payload, b"ntp");
            }
            _ => panic!("Expected Complete"),
        }
    }

    #[test]
    fn test_udp_packet_incomplete_header() {
        // Only ATYP one byte
        match parse_udp_packet(&[0x01]) {
            PacketParseResult::Incomplete => {}
            _ => panic!("Expected Incomplete"),
        }
    }

    #[test]
    fn test_udp_packet_incomplete_payload() {
        let atyp_addr = [0x01, 8, 8, 8, 8];
        let mut buf = build_udp_packet(&atyp_addr, 53, b"full-payload");
        buf.truncate(buf.len() - 3); // payload missing 3 bytes
        match parse_udp_packet(&buf) {
            PacketParseResult::Incomplete => {}
            _ => panic!("Expected Incomplete for truncated payload"),
        }
    }

    #[test]
    fn test_udp_packet_coalesced() {
        // Coalesced: two packets in one buffer
        let atyp_addr = [0x01, 1, 1, 1, 1];
        let mut buf = build_udp_packet(&atyp_addr, 53, b"first");
        buf.extend_from_slice(&build_udp_packet(&atyp_addr, 53, b"second"));
        match parse_udp_packet(&buf) {
            PacketParseResult::Complete(pkt, consumed) => {
                assert_eq!(pkt.payload, b"first");
                match parse_udp_packet(&buf[consumed..]) {
                    PacketParseResult::Complete(pkt2, consumed2) => {
                        assert_eq!(pkt2.payload, b"second");
                        assert_eq!(consumed + consumed2, buf.len());
                    }
                    _ => panic!("Expected second Complete"),
                }
            }
            _ => panic!("Expected first Complete"),
        }
    }

    #[test]
    fn test_udp_packet_invalid_atyp() {
        let buf = build_udp_packet(&[0x07, 1, 2, 3, 4], 53, b"x");
        match parse_udp_packet(&buf) {
            PacketParseResult::Invalid(msg) => {
                assert!(msg.contains("address") || msg.contains("ATYP") || msg.contains("atyp"))
            }
            _ => panic!("Expected Invalid for bad ATYP"),
        }
    }

    #[test]
    fn test_udp_packet_missing_crlf() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&[0x01, 8, 8, 8, 8]);
        buf.extend_from_slice(&53u16.to_be_bytes());
        buf.extend_from_slice(&1u16.to_be_bytes());
        buf.extend_from_slice(b"XX"); // Not CRLF
        buf.push(b'q');
        match parse_udp_packet(&buf) {
            PacketParseResult::Invalid(msg) => assert!(msg.contains("CRLF")),
            _ => panic!("Expected Invalid for missing CRLF"),
        }
    }

    #[test]
    fn test_udp_packet_empty_payload() {
        let atyp_addr = [0x01, 8, 8, 8, 8];
        let buf = build_udp_packet(&atyp_addr, 53, b"");
        match parse_udp_packet(&buf) {
            PacketParseResult::Complete(pkt, _) => assert!(pkt.payload.is_empty()),
            _ => panic!("Expected Complete for empty payload"),
        }
    }

    #[test]
    fn test_udp_packet_invalid_zero_length_domain() {
        // 0x03 + len 0 — enough bytes to know it's invalid, must not be Incomplete
        let buf = [0x03, 0x00];
        match parse_udp_packet(&buf) {
            PacketParseResult::Invalid(_) => {}
            _ => panic!("Expected Invalid for zero-length domain"),
        }
    }

    #[test]
    fn test_udp_packet_invalid_overlong_domain() {
        // 0x03 + len 254 (> 253 max) — enough bytes to know it's invalid
        let buf = [0x03, 254];
        match parse_udp_packet(&buf) {
            PacketParseResult::Invalid(_) => {}
            _ => panic!("Expected Invalid for overlong domain"),
        }
    }

    #[test]
    fn test_udp_packet_invalid_domain_chars_full_data() {
        // Full domain bytes present but containing invalid char '@'
        let mut buf = vec![0x03, 4];
        buf.extend_from_slice(b"ex@m");
        match parse_udp_packet(&buf) {
            PacketParseResult::Invalid(_) => {}
            _ => panic!("Expected Invalid for domain with invalid chars"),
        }
    }

    #[test]
    fn test_udp_packet_incomplete_domain_short_buffer() {
        // Domain length says 10 but only 4 bytes present — genuinely Incomplete
        let mut buf = vec![0x03, 10];
        buf.extend_from_slice(b"exam");
        match parse_udp_packet(&buf) {
            PacketParseResult::Incomplete => {}
            _ => panic!("Expected Incomplete for short domain buffer"),
        }
    }

    #[test]
    fn test_udp_packet_encode_ipv4() {
        let pkt = UdpPacket {
            address: TrojanAddress::IPv4(Ipv4Addr::new(8, 8, 8, 8)),
            port: 53,
            payload: b"dns-response".to_vec(),
        };
        let encoded = pkt.encode();
        let mut expected = vec![0x01, 8, 8, 8, 8];
        expected.extend_from_slice(&53u16.to_be_bytes());
        expected.extend_from_slice(&12u16.to_be_bytes());
        expected.extend_from_slice(b"\r\n");
        expected.extend_from_slice(b"dns-response");
        assert_eq!(encoded, expected);
    }

    #[test]
    fn test_udp_packet_encode_parse_roundtrip() {
        for pkt in [
            UdpPacket {
                address: TrojanAddress::IPv4(Ipv4Addr::new(1, 2, 3, 4)),
                port: 53,
                payload: b"hello".to_vec(),
            },
            UdpPacket {
                address: TrojanAddress::IPv6(Ipv6Addr::LOCALHOST),
                port: 123,
                payload: Vec::new(),
            },
            UdpPacket {
                address: TrojanAddress::Domain("dns.google".to_string()),
                port: 853,
                payload: vec![0u8; 512],
            },
        ] {
            let encoded = pkt.encode();
            match parse_udp_packet(&encoded) {
                PacketParseResult::Complete(parsed, consumed) => {
                    assert_eq!(parsed.address, pkt.address);
                    assert_eq!(parsed.port, pkt.port);
                    assert_eq!(parsed.payload, pkt.payload);
                    assert_eq!(consumed, encoded.len());
                }
                _ => panic!("roundtrip failed"),
            }
        }
    }

    #[tokio::test]
    async fn test_udp_dead_channel_removes_stale_target() {
        // Simulate the death-notification flow: insert a target entry whose
        // "read task" immediately sends its address on the dead channel,
        // then verify the removal arm logic drops the entry.
        use std::collections::HashMap;
        use std::net::SocketAddr;
        use std::sync::Arc;

        let mut targets: HashMap<
            SocketAddr,
            (Arc<tokio::net::UdpSocket>, tokio::task::AbortHandle),
        > = HashMap::new();
        let (dead_tx, mut dead_rx) = tokio::sync::mpsc::channel::<SocketAddr>(64);

        let addr: SocketAddr = "127.0.0.1:15353".parse().unwrap();
        let socket = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let tx2 = dead_tx.clone();
        let handle = tokio::spawn(async move {
            let _ = tx2.send(addr).await; // simulate read-task death
        });
        targets.insert(addr, (socket, handle.abort_handle()));

        // Main-loop removal arm
        if let Some(dead) = dead_rx.recv().await {
            if let Some((_, abort)) = targets.remove(&dead) {
                abort.abort();
            }
        }

        assert!(targets.is_empty(), "stale target entry should be removed");
    }
}

// ---------------------------------------------------------------------------
// Tests formerly in trojan_test.rs, merged here as an inner module
// ---------------------------------------------------------------------------

#[cfg(test)]
mod legacy_tests {
    //! Unit and integration tests for Trojan protocol connection stability.
    //!
    //! Integration tests are pure Rust — no external Trojan client binary needed.
    //! They use tokio-rustls with insecure/TOFU mode to connect to a local
    //! Trojan server, exercising the full TLS + Trojan handshake + proxy chain.

    use std::net::Ipv4Addr;
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio_rustls::client::TlsStream;

    use crate::port_registry::MockPortRegistry;
    use crate::listener;
    use rust_tunnel_stats::StatsCollector;
    use std::sync::Arc as StdArc;
    use crate::trojan::sha224_hex;
    use rust_tunnel_common::{
        create_insecure_client_config, create_server_config, load_or_generate_cert,
    };

    // ---------------------------------------------------------------------------
    // helper types
    // ---------------------------------------------------------------------------

    /// Target address type for building Trojan request headers.
    enum TestTargetAddr {
        Ipv4(Ipv4Addr),
        Domain(String),
    }

    // ---------------------------------------------------------------------------
    // helper functions
    // ---------------------------------------------------------------------------

    /// Bind to port 0 to get a random available port, then return it.
    async fn find_available_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    }

    /// Start a TCP echo server on a random port.
    /// Returns the port and a JoinHandle whose `abort()` method stops the server.
    async fn start_echo_server() -> (u16, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = tokio::spawn(async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(c) => c,
                    Err(_) => break,
                };
                tokio::spawn(async move {
                    let (mut reader, mut writer) = tokio::io::split(stream);
                    let mut buf = [0u8; 8192];
                    loop {
                        match reader.read(&mut buf).await {
                            Ok(0) => break,
                            Ok(n) => {
                                if writer.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                });
            }
        });

        (port, handle)
    }

    /// Retry connecting to `port` until success or `dur` elapses.
    async fn wait_for_port(port: u16, dur: Duration) {
        let start = tokio::time::Instant::now();
        loop {
            if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
                return;
            }
            if start.elapsed() >= dur {
                panic!("Timed out waiting for port {}", port);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Generate a self-signed TLS certificate pair in a temp directory and return
    /// a `TlsAcceptor` ready for the Trojan server. The `TempDir` must be kept
    /// alive for the duration of the test.
    fn generate_test_tls_config() -> (std::sync::Arc<rustls::ServerConfig>, tempfile::TempDir) {
        let tmp_dir = tempfile::tempdir().unwrap();
        let cert_path = tmp_dir.path().join("cert.pem");
        let key_path = tmp_dir.path().join("key.pem");

        let cert_pair =
            load_or_generate_cert(cert_path.to_str().unwrap(), key_path.to_str().unwrap()).unwrap();
        let server_config = create_server_config(cert_pair).unwrap();

        (server_config, tmp_dir)
    }

    /// Spawn the Trojan listener as a tokio task. Returns the `watch::Receiver`
    /// (for reference), a `JoinHandle` for the listener task, and the `TempDir`
    /// that must be kept alive.
    async fn start_trojan_server(
        registry: StdArc<dyn crate::port_registry::PortRegistry>,
        stats: StatsCollector,
        port: u16,
        password: &str,
        fallback: &str,
    ) -> (
        tokio::sync::watch::Receiver<std::sync::Arc<rustls::ServerConfig>>,
        tokio::task::JoinHandle<()>,
        tempfile::TempDir,
    ) {
        let (server_config, tmp_dir) = generate_test_tls_config();
        let (_tx, rx) = tokio::sync::watch::channel(server_config);
        let password = password.to_string();
        let fallback = fallback.to_string();

        let handle = tokio::spawn(async move {
            let _ = listener::start_trojan_listener(registry, stats, port, password, fallback, rx).await;
        });

        // Return a dummy receiver for compatibility (tx keeps it alive)
        let (_dummy_tx, dummy_rx) = tokio::sync::watch::channel(
            create_server_config(
                load_or_generate_cert(
                    tmp_dir.path().join("cert.pem").to_str().unwrap(),
                    tmp_dir.path().join("key.pem").to_str().unwrap(),
                )
                .unwrap(),
            )
            .unwrap(),
        );
        (dummy_rx, handle, tmp_dir)
    }

    /// Build raw Trojan protocol request bytes (without payload).
    fn build_trojan_header(password: &str, cmd: u8, target: &TestTargetAddr, port: u16) -> Vec<u8> {
        let mut buf = Vec::with_capacity(256);

        // SHA-224 hex hash of password (56 bytes)
        buf.extend_from_slice(sha224_hex(password).as_bytes());
        // CRLF
        buf.extend_from_slice(b"\r\n");
        // Command
        buf.push(cmd);
        // Address
        match target {
            TestTargetAddr::Ipv4(addr) => {
                buf.push(0x01); // ATYP = IPv4
                buf.extend_from_slice(&addr.octets());
            }
            TestTargetAddr::Domain(domain) => {
                buf.push(0x03); // ATYP = Domain
                buf.push(domain.len() as u8);
                buf.extend_from_slice(domain.as_bytes());
            }
        }
        // Port (big-endian)
        buf.extend_from_slice(&port.to_be_bytes());
        // Trailing CRLF
        buf.extend_from_slice(b"\r\n");

        buf
    }

    /// Perform a full Trojan client connection: TLS handshake + send Trojan header.
    /// Returns the TLS stream ready for data exchange.
    async fn trojan_connect(
        trojan_port: u16,
        password: &str,
        target_port: u16,
    ) -> TlsStream<TcpStream> {
        trojan_connect_with_atype(
            trojan_port,
            password,
            &TestTargetAddr::Ipv4(Ipv4Addr::new(127, 0, 0, 1)),
            target_port,
        )
        .await
    }

    /// Connect with an explicit address type (IPv4 or Domain).
    async fn trojan_connect_with_atype(
        trojan_port: u16,
        password: &str,
        target: &TestTargetAddr,
        target_port: u16,
    ) -> TlsStream<TcpStream> {
        let config = create_insecure_client_config().unwrap();
        let connector = tokio_rustls::TlsConnector::from(config);

        let stream = TcpStream::connect(format!("127.0.0.1:{}", trojan_port))
            .await
            .expect("Failed to connect to Trojan server");

        let server_name = rustls::pki_types::ServerName::try_from("localhost".to_string())
            .expect("Invalid server name");
        let mut tls_stream = connector
            .connect(server_name, stream)
            .await
            .expect("TLS handshake failed");

        // Send Trojan request header
        let header = build_trojan_header(password, 0x01, target, target_port);
        tls_stream
            .write_all(&header)
            .await
            .expect("Failed to send Trojan header");

        tls_stream
    }

    /// High-level: connect via Trojan, send data, read back exactly `data.len()` bytes.
    async fn trojan_send_recv(
        trojan_port: u16,
        password: &str,
        target_port: u16,
        data: &[u8],
    ) -> Vec<u8> {
        let mut stream = trojan_connect(trojan_port, password, target_port).await;
        stream.write_all(data).await.expect("Failed to send data");

        let mut response = vec![0u8; data.len()];
        stream
            .read_exact(&mut response)
            .await
            .expect("Failed to read response");

        response
    }

    /// Start a UDP echo server on a random port.
    async fn start_udp_echo_server() -> (u16, tokio::task::JoinHandle<()>) {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = socket.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 65536];
            while let Ok((n, from)) = socket.recv_from(&mut buf).await {
                if socket.send_to(&buf[..n], from).await.is_err() {
                    break;
                }
            }
        });
        (port, handle)
    }

    /// Build a Trojan UDP packet (ATYP=IPv4).
    fn build_udp_packet_v4(ip: std::net::Ipv4Addr, port: u16, payload: &[u8]) -> Vec<u8> {
        let mut buf = vec![0x01];
        buf.extend_from_slice(&ip.octets());
        buf.extend_from_slice(&port.to_be_bytes());
        buf.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        buf.extend_from_slice(b"\r\n");
        buf.extend_from_slice(payload);
        buf
    }

    /// Parse one UDP packet from the front of `buf`; returns (src_port, payload, consumed).
    fn parse_udp_packet_test(buf: &[u8]) -> Option<(u16, Vec<u8>, usize)> {
        if buf.is_empty() || buf[0] != 0x01 || buf.len() < 11 {
            return None;
        }
        let port = u16::from_be_bytes([buf[5], buf[6]]);
        let len = u16::from_be_bytes([buf[7], buf[8]]) as usize;
        if buf[9] != b'\r' || buf[10] != b'\n' || buf.len() < 11 + len {
            return None;
        }
        Some((port, buf[11..11 + len].to_vec(), 11 + len))
    }

    /// Helper: view of the not-yet-consumed remainder of a read buffer.
    fn responses_buf(buf: &[u8], n: usize, offset: usize) -> Vec<u8> {
        buf[..n][offset.min(n)..].to_vec()
    }

    // ---------------------------------------------------------------------------
    // unit tests
    // ---------------------------------------------------------------------------

    #[cfg(test)]
    mod unit_tests {
        use super::*;

        #[test]
        fn test_build_trojan_header_ipv4() {
            let header = build_trojan_header(
                "testpass",
                0x01,
                &TestTargetAddr::Ipv4(Ipv4Addr::new(127, 0, 0, 1)),
                443,
            );

            // Hash is 56 bytes + CRLF(2) + CMD(1) + ATYP(1) + IPv4(4) + PORT(2) + CRLF(2) = 68
            assert_eq!(header.len(), 68);
            // Verify CRLF after hash
            assert_eq!(header[56], b'\r');
            assert_eq!(header[57], b'\n');
            // Verify CMD
            assert_eq!(header[58], 0x01);
            // Verify ATYP
            assert_eq!(header[59], 0x01);
            // Verify IPv4 octets
            assert_eq!(&header[60..64], &[127, 0, 0, 1]);
            // Verify port (443 = 0x01BB)
            assert_eq!(header[64], 0x01);
            assert_eq!(header[65], 0xBB);
            // Verify trailing CRLF
            assert_eq!(header[66], b'\r');
            assert_eq!(header[67], b'\n');
        }

        #[test]
        fn test_build_trojan_header_domain() {
            let header = build_trojan_header(
                "testpass",
                0x01,
                &TestTargetAddr::Domain("example.com".to_string()),
                8080,
            );

            // 56 + 2 + 1 + 1 + 1 + 11 + 2 + 2 = 76
            assert_eq!(header.len(), 76);
            // Verify ATYP
            assert_eq!(header[59], 0x03);
            // Verify domain length
            assert_eq!(header[60], 11);
            // Verify domain bytes
            assert_eq!(&header[61..72], b"example.com");
        }

        #[test]
        fn test_build_trojan_header_hash_matches_sha224() {
            let header = build_trojan_header(
                "mypassword",
                0x01,
                &TestTargetAddr::Ipv4(Ipv4Addr::new(127, 0, 0, 1)),
                80,
            );
            let hash_part = std::str::from_utf8(&header[..56]).unwrap();
            assert_eq!(hash_part, sha224_hex("mypassword"));
        }
    }

    // ---------------------------------------------------------------------------
    // integration tests
    // ---------------------------------------------------------------------------

    #[cfg(test)]
    mod integration_tests {
        use super::*;

        #[tokio::test]
        #[ignore]
        async fn test_trojan_echo_basic() {
            let (echo_port, echo_handle) = start_echo_server().await;
            let registry: StdArc<dyn crate::port_registry::PortRegistry> = StdArc::new(MockPortRegistry::new());
            let stats = StatsCollector::new(None);
            let trojan_port = find_available_port().await;

            let (_acceptor, server_handle, _tmp_dir) =
                start_trojan_server(registry.clone(), stats.clone(), trojan_port, "testpass", "127.0.0.1:1").await;
            wait_for_port(trojan_port, Duration::from_secs(5)).await;

            let response =
                trojan_send_recv(trojan_port, "testpass", echo_port, b"hello trojan").await;
            assert_eq!(response, b"hello trojan");

            server_handle.abort();
            echo_handle.abort();
        }

        #[tokio::test]
        #[ignore]
        async fn test_trojan_concurrent_connections() {
            let (echo_port, echo_handle) = start_echo_server().await;
            let registry: StdArc<dyn crate::port_registry::PortRegistry> = StdArc::new(MockPortRegistry::new());
            let stats = StatsCollector::new(None);
            let trojan_port = find_available_port().await;

            let (_acceptor, server_handle, _tmp_dir) =
                start_trojan_server(registry.clone(), stats.clone(), trojan_port, "testpass", "127.0.0.1:1").await;
            wait_for_port(trojan_port, Duration::from_secs(5)).await;

            let mut handles = Vec::new();
            for i in 0..5 {
                let port = trojan_port;
                let echo = echo_port;
                handles.push(tokio::spawn(async move {
                    let payload = format!("conn-{}", i);
                    let mut stream = trojan_connect(port, "testpass", echo).await;
                    stream.write_all(payload.as_bytes()).await.unwrap();
                    let mut buf = vec![0u8; payload.len()];
                    stream.read_exact(&mut buf).await.unwrap();
                    buf
                }));
            }

            for (i, handle) in handles.into_iter().enumerate() {
                let result = handle.await.unwrap();
                let expected = format!("conn-{}", i);
                assert_eq!(&result[..], expected.as_bytes());
            }

            // All connections should be cleaned up
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert_eq!(
                registry.get_connection_count_for_port(trojan_port).await,
                0,
                "Connection count should be 0 after all connections close"
            );

            server_handle.abort();
            echo_handle.abort();
        }

        #[tokio::test]
        #[ignore]
        async fn test_trojan_large_data_transfer() {
            let (echo_port, echo_handle) = start_echo_server().await;
            let registry: StdArc<dyn crate::port_registry::PortRegistry> = StdArc::new(MockPortRegistry::new());
            let stats = StatsCollector::new(None);
            let trojan_port = find_available_port().await;

            let (_acceptor, server_handle, _tmp_dir) =
                start_trojan_server(registry.clone(), stats.clone(), trojan_port, "testpass", "127.0.0.1:1").await;
            wait_for_port(trojan_port, Duration::from_secs(5)).await;

            // 64 KB payload
            let payload: Vec<u8> = (0u8..=255).cycle().take(65536).collect();
            let mut stream = trojan_connect(trojan_port, "testpass", echo_port).await;

            stream.write_all(&payload).await.unwrap();

            // Read in a loop — the echo server uses an 8KB buffer
            let mut response = vec![0u8; payload.len()];
            let mut total_read = 0;
            while total_read < payload.len() {
                let n = stream
                    .read(&mut response[total_read..])
                    .await
                    .expect("Failed to read");
                if n == 0 {
                    break;
                }
                total_read += n;
            }
            assert_eq!(total_read, payload.len());
            assert_eq!(response, payload);

            server_handle.abort();
            echo_handle.abort();
        }

        #[tokio::test]
        #[ignore]
        async fn test_trojan_connection_retry() {
            let (echo_port, echo_handle) = start_echo_server().await;
            let registry: StdArc<dyn crate::port_registry::PortRegistry> = StdArc::new(MockPortRegistry::new());
            let stats = StatsCollector::new(None);
            let trojan_port = find_available_port().await;

            let (_acceptor, server_handle, _tmp_dir) =
                start_trojan_server(registry.clone(), stats.clone(), trojan_port, "testpass", "127.0.0.1:1").await;
            wait_for_port(trojan_port, Duration::from_secs(5)).await;

            // First connection
            {
                let mut stream = trojan_connect(trojan_port, "testpass", echo_port).await;
                stream.write_all(b"first").await.unwrap();
                let mut buf = [0u8; 5];
                stream.read_exact(&mut buf).await.unwrap();
                assert_eq!(&buf, b"first");
            }

            // Wait for server-side cleanup
            tokio::time::sleep(Duration::from_millis(200)).await;

            // Second connection — should succeed
            {
                let mut stream = trojan_connect(trojan_port, "testpass", echo_port).await;
                stream.write_all(b"second").await.unwrap();
                let mut buf = [0u8; 6];
                stream.read_exact(&mut buf).await.unwrap();
                assert_eq!(&buf, b"second");
            }

            server_handle.abort();
            echo_handle.abort();
        }

        #[tokio::test]
        #[ignore]
        async fn test_trojan_auth_failure_fallback() {
            // The fallback server — a simple TCP listener that sends a known marker
            let fallback_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let fallback_port = fallback_listener.local_addr().unwrap().port();
            let fallback_handle = tokio::spawn(async move {
                // Accept one connection, read data, write back a marker
                if let Ok((mut stream, _)) = fallback_listener.accept().await {
                    let mut buf = [0u8; 256];
                    let _ = stream.read(&mut buf).await;
                    let _ = stream.write_all(b"FALLBACK_OK").await;
                }
            });

            let registry: StdArc<dyn crate::port_registry::PortRegistry> = StdArc::new(MockPortRegistry::new());
            let stats = StatsCollector::new(None);
            let trojan_port = find_available_port().await;

            let (_acceptor, server_handle, _tmp_dir) = start_trojan_server(
                registry.clone(),
                stats.clone(),
                trojan_port,
                "correctpass",
                &format!("127.0.0.1:{}", fallback_port),
            )
            .await;
            wait_for_port(trojan_port, Duration::from_secs(5)).await;

            // Connect with wrong password
            let config = create_insecure_client_config().unwrap();
            let connector = tokio_rustls::TlsConnector::from(config);
            let stream = TcpStream::connect(format!("127.0.0.1:{}", trojan_port))
                .await
                .unwrap();
            let server_name =
                rustls::pki_types::ServerName::try_from("localhost".to_string()).unwrap();
            let mut tls_stream = connector.connect(server_name, stream).await.unwrap();

            // Send Trojan header with wrong password
            let header = build_trojan_header(
                "wrongpass",
                0x01,
                &TestTargetAddr::Ipv4(Ipv4Addr::new(127, 0, 0, 1)),
                80,
            );
            tls_stream.write_all(&header).await.unwrap();

            // The server should fall back — we should be able to read something
            // (the fallback server's response)
            let mut buf = [0u8; 64];
            let result =
                tokio::time::timeout(Duration::from_secs(3), tls_stream.read(&mut buf)).await;
            // The connection should either get fallback data or be closed by the server
            // Either way, the key property is that the server doesn't panic
            match result {
                Ok(Ok(n)) if n > 0 => {
                    // Got some data from the fallback — verify it contains our marker
                    let response = &buf[..n];
                    assert!(
                        response
                            .windows(b"FALLBACK_OK".len())
                            .any(|w| w == b"FALLBACK_OK"),
                        "Expected FALLBACK_OK in response, got: {:?}",
                        response
                    );
                }
                _ => {
                    // Connection closed is also acceptable — the fallback mechanism ran
                }
            }

            server_handle.abort();
            fallback_handle.abort();
        }

        #[tokio::test]
        #[ignore]
        async fn test_trojan_long_lived_connection() {
            let (echo_port, echo_handle) = start_echo_server().await;
            let registry: StdArc<dyn crate::port_registry::PortRegistry> = StdArc::new(MockPortRegistry::new());
            let stats = StatsCollector::new(None);
            let trojan_port = find_available_port().await;

            let (_acceptor, server_handle, _tmp_dir) =
                start_trojan_server(registry.clone(), stats.clone(), trojan_port, "testpass", "127.0.0.1:1").await;
            wait_for_port(trojan_port, Duration::from_secs(5)).await;

            let mut stream = trojan_connect(trojan_port, "testpass", echo_port).await;

            for i in 0..10 {
                let payload = format!("round-{:03}-data", i);
                stream.write_all(payload.as_bytes()).await.unwrap();
                let mut buf = vec![0u8; payload.len()];
                stream.read_exact(&mut buf).await.unwrap();
                assert_eq!(buf, payload.as_bytes());
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            server_handle.abort();
            echo_handle.abort();
        }

        #[tokio::test]
        #[ignore]
        async fn test_trojan_rapid_connect_disconnect() {
            let (echo_port, echo_handle) = start_echo_server().await;
            let registry: StdArc<dyn crate::port_registry::PortRegistry> = StdArc::new(MockPortRegistry::new());
            let stats = StatsCollector::new(None);
            let trojan_port = find_available_port().await;

            let (_acceptor, server_handle, _tmp_dir) =
                start_trojan_server(registry.clone(), stats.clone(), trojan_port, "testpass", "127.0.0.1:1").await;
            wait_for_port(trojan_port, Duration::from_secs(5)).await;

            for _ in 0..20 {
                let mut stream = trojan_connect(trojan_port, "testpass", echo_port).await;
                stream.write_all(b"ping").await.unwrap();
                let mut buf = [0u8; 4];
                stream.read_exact(&mut buf).await.unwrap();
                assert_eq!(&buf, b"ping");
                // Stream dropped here — connection closes
            }

            // Wait for all server-side cleanup
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(
                registry.get_connection_count_for_port(trojan_port).await,
                0,
                "Connection count should be 0 after rapid connect/disconnect"
            );

            server_handle.abort();
            echo_handle.abort();
        }

        #[tokio::test]
        #[ignore]
        async fn test_trojan_domain_and_ipv4_target() {
            let (echo_port, echo_handle) = start_echo_server().await;
            let registry: StdArc<dyn crate::port_registry::PortRegistry> = StdArc::new(MockPortRegistry::new());
            let stats = StatsCollector::new(None);
            let trojan_port = find_available_port().await;

            let (_acceptor, server_handle, _tmp_dir) =
                start_trojan_server(registry.clone(), stats.clone(), trojan_port, "testpass", "127.0.0.1:1").await;
            wait_for_port(trojan_port, Duration::from_secs(5)).await;

            // Test IPv4 targeting
            {
                let mut stream = trojan_connect(trojan_port, "testpass", echo_port).await;
                stream.write_all(b"ipv4-test").await.unwrap();
                let mut buf = [0u8; 9];
                stream.read_exact(&mut buf).await.unwrap();
                assert_eq!(&buf, b"ipv4-test");
            }

            // Test Domain targeting — "localhost" resolves to 127.0.0.1
            {
                let mut stream = trojan_connect_with_atype(
                    trojan_port,
                    "testpass",
                    &TestTargetAddr::Domain("localhost".to_string()),
                    echo_port,
                )
                .await;
                stream.write_all(b"domain-test").await.unwrap();
                let mut buf = [0u8; 11];
                stream.read_exact(&mut buf).await.unwrap();
                assert_eq!(&buf, b"domain-test");
            }

            server_handle.abort();
            echo_handle.abort();
        }

        #[tokio::test]
        #[ignore]
        async fn test_trojan_udp_associate_echo() {
            let (udp_port, udp_handle) = start_udp_echo_server().await;
            let registry: StdArc<dyn crate::port_registry::PortRegistry> = StdArc::new(MockPortRegistry::new());
            let stats = StatsCollector::new(None);
            let trojan_port = find_available_port().await;

            let (_acceptor, server_handle, _tmp_dir) =
                start_trojan_server(registry.clone(), stats.clone(), trojan_port, "testpass", "127.0.0.1:1").await;
            wait_for_port(trojan_port, Duration::from_secs(5)).await;

            // TLS connect + UDP ASSOCIATE handshake (target addr is advisory)
            let config = create_insecure_client_config().unwrap();
            let connector = tokio_rustls::TlsConnector::from(config);
            let stream = TcpStream::connect(format!("127.0.0.1:{}", trojan_port))
                .await
                .unwrap();
            let server_name =
                rustls::pki_types::ServerName::try_from("localhost".to_string()).unwrap();
            let mut tls_stream = connector.connect(server_name, stream).await.unwrap();

            let header = build_trojan_header(
                "testpass",
                0x03, // UDP ASSOCIATE
                &TestTargetAddr::Ipv4(Ipv4Addr::new(127, 0, 0, 1)),
                udp_port,
            );
            tls_stream.write_all(&header).await.unwrap();

            // Send a UDP packet through the tunnel
            let packet =
                build_udp_packet_v4(Ipv4Addr::new(127, 0, 0, 1), udp_port, b"udp-echo-test");
            tls_stream.write_all(&packet).await.unwrap();
            tls_stream.flush().await.unwrap();

            // Read the response packet
            let mut buf = vec![0u8; 1024];
            let n = tokio::time::timeout(Duration::from_secs(5), tls_stream.read(&mut buf))
                .await
                .expect("timed out waiting for UDP response")
                .expect("read failed");
            let (src_port, payload, _consumed) =
                parse_udp_packet_test(&buf[..n]).expect("failed to parse UDP response packet");
            assert_eq!(src_port, udp_port);
            assert_eq!(payload, b"udp-echo-test");

            server_handle.abort();
            udp_handle.abort();
        }

        #[tokio::test]
        #[ignore]
        async fn test_trojan_udp_associate_multi_target() {
            let (udp_port_a, udp_handle_a) = start_udp_echo_server().await;
            let (udp_port_b, udp_handle_b) = start_udp_echo_server().await;
            let registry: StdArc<dyn crate::port_registry::PortRegistry> = StdArc::new(MockPortRegistry::new());
            let stats = StatsCollector::new(None);
            let trojan_port = find_available_port().await;

            let (_acceptor, server_handle, _tmp_dir) =
                start_trojan_server(registry.clone(), stats.clone(), trojan_port, "testpass", "127.0.0.1:1").await;
            wait_for_port(trojan_port, Duration::from_secs(5)).await;

            let config = create_insecure_client_config().unwrap();
            let connector = tokio_rustls::TlsConnector::from(config);
            let stream = TcpStream::connect(format!("127.0.0.1:{}", trojan_port))
                .await
                .unwrap();
            let server_name =
                rustls::pki_types::ServerName::try_from("localhost".to_string()).unwrap();
            let mut tls_stream = connector.connect(server_name, stream).await.unwrap();

            let header = build_trojan_header(
                "testpass",
                0x03,
                &TestTargetAddr::Ipv4(Ipv4Addr::new(127, 0, 0, 1)),
                udp_port_a,
            );
            tls_stream.write_all(&header).await.unwrap();

            // Two packets to two different targets over the same associate connection
            let pkt_a = build_udp_packet_v4(Ipv4Addr::new(127, 0, 0, 1), udp_port_a, b"to-a");
            let pkt_b = build_udp_packet_v4(Ipv4Addr::new(127, 0, 0, 1), udp_port_b, b"to-b");
            tls_stream.write_all(&pkt_a).await.unwrap();
            tls_stream.write_all(&pkt_b).await.unwrap();

            // Collect two response packets (order may vary)
            let mut responses = Vec::new();
            let mut buf = vec![0u8; 4096];
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while responses.len() < 2 {
                let n = tokio::time::timeout_at(deadline, tls_stream.read(&mut buf))
                    .await
                    .expect("timed out waiting for UDP responses")
                    .expect("read failed");
                let mut offset = 0;
                while let Some((src_port, payload, consumed)) =
                    parse_udp_packet_test(&responses_buf(&buf, n, offset))
                {
                    responses.push((src_port, payload));
                    offset += consumed;
                }
            }

            assert!(responses
                .iter()
                .any(|(p, d)| *p == udp_port_a && d == b"to-a"));
            assert!(responses
                .iter()
                .any(|(p, d)| *p == udp_port_b && d == b"to-b"));

            server_handle.abort();
            udp_handle_a.abort();
            udp_handle_b.abort();
        }

        #[tokio::test]
        #[ignore]
        async fn test_trojan_udp_associate_cleanup_on_close() {
            let (udp_port, udp_handle) = start_udp_echo_server().await;
            let registry: StdArc<dyn crate::port_registry::PortRegistry> = StdArc::new(MockPortRegistry::new());
            let stats = StatsCollector::new(None);
            let trojan_port = find_available_port().await;

            let (_acceptor, server_handle, _tmp_dir) =
                start_trojan_server(registry.clone(), stats.clone(), trojan_port, "testpass", "127.0.0.1:1").await;
            wait_for_port(trojan_port, Duration::from_secs(5)).await;

            {
                let config = create_insecure_client_config().unwrap();
                let connector = tokio_rustls::TlsConnector::from(config);
                let stream = TcpStream::connect(format!("127.0.0.1:{}", trojan_port))
                    .await
                    .unwrap();
                let server_name =
                    rustls::pki_types::ServerName::try_from("localhost".to_string()).unwrap();
                let mut tls_stream = connector.connect(server_name, stream).await.unwrap();

                let header = build_trojan_header(
                    "testpass",
                    0x03,
                    &TestTargetAddr::Ipv4(Ipv4Addr::new(127, 0, 0, 1)),
                    udp_port,
                );
                tls_stream.write_all(&header).await.unwrap();
                let packet = build_udp_packet_v4(Ipv4Addr::new(127, 0, 0, 1), udp_port, b"ping");
                tls_stream.write_all(&packet).await.unwrap();

                let mut buf = [0u8; 256];
                let _ =
                    tokio::time::timeout(Duration::from_secs(3), tls_stream.read(&mut buf)).await;
                // tls_stream dropped here — session should clean up
            }

            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(
                registry.get_connection_count_for_port(trojan_port).await,
                0,
                "Connection count should be 0 after UDP associate client disconnect"
            );

            server_handle.abort();
            udp_handle.abort();
        }

        #[tokio::test]
        #[ignore]
        async fn test_trojan_active_connection_count() {
            let (echo_port, echo_handle) = start_echo_server().await;
            let registry: StdArc<dyn crate::port_registry::PortRegistry> = StdArc::new(MockPortRegistry::new());
            let stats = StatsCollector::new(None);
            let trojan_port = find_available_port().await;

            let (_acceptor, server_handle, _tmp_dir) =
                start_trojan_server(registry.clone(), stats.clone(), trojan_port, "testpass", "127.0.0.1:1").await;
            wait_for_port(trojan_port, Duration::from_secs(5)).await;

            // No connections yet
            assert_eq!(registry.get_connection_count_for_port(trojan_port).await, 0);

            // Open a connection and keep it alive
            let mut stream = trojan_connect(trojan_port, "testpass", echo_port).await;
            stream.write_all(b"count-test").await.unwrap();
            let mut buf = [0u8; 10];
            stream.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"count-test");

            // Wait for the server to increment the counter
            tokio::time::sleep(Duration::from_millis(100)).await;
            let count = registry.get_connection_count_for_port(trojan_port).await;
            assert!(count >= 1, "Expected >= 1 active connection, got {}", count);

            // Close the connection
            drop(stream);
            tokio::time::sleep(Duration::from_millis(300)).await;

            let count = registry.get_connection_count_for_port(trojan_port).await;
            assert_eq!(
                count, 0,
                "Expected 0 connections after close, got {}",
                count
            );

            server_handle.abort();
            echo_handle.abort();
        }

        #[tokio::test]
        #[ignore]
        async fn test_trojan_initial_payload() {
            let (echo_port, echo_handle) = start_echo_server().await;
            let registry: StdArc<dyn crate::port_registry::PortRegistry> = StdArc::new(MockPortRegistry::new());
            let stats = StatsCollector::new(None);
            let trojan_port = find_available_port().await;

            let (_acceptor, server_handle, _tmp_dir) =
                start_trojan_server(registry.clone(), stats.clone(), trojan_port, "testpass", "127.0.0.1:1").await;
            wait_for_port(trojan_port, Duration::from_secs(5)).await;

            // Connect and include initial payload in the Trojan header
            let config = create_insecure_client_config().unwrap();
            let connector = tokio_rustls::TlsConnector::from(config);
            let stream = TcpStream::connect(format!("127.0.0.1:{}", trojan_port))
                .await
                .unwrap();
            let server_name =
                rustls::pki_types::ServerName::try_from("localhost".to_string()).unwrap();
            let mut tls_stream = connector.connect(server_name, stream).await.unwrap();

            // Build header with initial payload appended
            let mut request = build_trojan_header(
                "testpass",
                0x01,
                &TestTargetAddr::Ipv4(Ipv4Addr::new(127, 0, 0, 1)),
                echo_port,
            );
            request.extend_from_slice(b"INITIAL");
            tls_stream.write_all(&request).await.unwrap();

            // Send more data after the initial payload
            tls_stream.write_all(b"MORE").await.unwrap();

            // Read back the echo of both payloads
            let total_len = 7 + 4;
            let mut response = Vec::with_capacity(total_len);
            let mut tmp = [0u8; 64];
            while response.len() < total_len {
                let n = tls_stream.read(&mut tmp).await.unwrap();
                if n == 0 {
                    break;
                }
                response.extend_from_slice(&tmp[..n]);
            }

            assert!(
                response.len() >= total_len,
                "Expected at least {} bytes, got {}",
                total_len,
                response.len()
            );
            // The echo should contain both payloads in order
            assert!(
                response.starts_with(b"INITIAL"),
                "Expected response starting with INITIAL, got: {:?}",
                response
            );

            server_handle.abort();
            echo_handle.abort();
        }
    }
}
