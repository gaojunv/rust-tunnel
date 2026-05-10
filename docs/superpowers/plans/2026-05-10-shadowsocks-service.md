# Shadowsocks Service Integration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Integrate Shadowsocks TCP proxy service into rust-tunnel server, with deep integration into existing traffic statistics and quality monitoring systems.

**Architecture:** Extend existing listener/proxy architecture to support Shadowsocks as a new port type. Only use shadowsocks-rust crypto layer APIs, reuse existing traffic accounting and quality tracking infrastructure.

**Tech Stack:** Rust, Tokio, shadowsocks-rust crate (crypto only), sqlx (SQLite), Axum web framework

---

## Phase 1: Core Functionality (MVP)

### Task 1: Add shadowsocks-rust dependency and extend configuration

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/server/config.rs`

- [ ] **Step 1: Add shadowsocks-rust dependency to Cargo.toml**

```toml
# Add to dependencies section
shadowsocks = { version = "1.24", default-features = false, features = ["aes-gcm", "chacha20-poly1305"] }
```

- [ ] **Step 2: Extend ServerCli struct with Shadowsocks command line arguments**

```rust
// Add to ServerCli in src/server/config.rs
/// Enable Shadowsocks proxy service
#[clap(long = "ss-enabled")]
pub ss_enabled: Option<bool>,

/// Shadowsocks listen port
#[clap(long = "ss-port")]
pub ss_port: Option<u16>,

/// Shadowsocks encryption method (aes-256-gcm, chacha20-ietf-poly1305)
#[clap(long = "ss-cipher")]
pub ss_cipher: Option<String>,

/// Shadowsocks password
#[clap(long = "ss-password")]
pub ss_password: Option<String>,
```

- [ ] **Step 3: Extend ServerConfigFile struct with Shadowsocks config file support**

```rust
// Add to ServerConfigFile in src/server/config.rs
pub ss_enabled: Option<bool>,
pub ss_port: Option<u16>,
pub ss_cipher: Option<String>,
pub ss_password: Option<String>,
```

- [ ] **Step 4: Extend ServerConfig struct with Shadowsocks runtime config**

```rust
// Add to ServerConfig in src/server/config.rs
pub ss_enabled: bool,
pub ss_port: Option<u16>,
pub ss_cipher: Option<String>,
pub ss_password: Option<String>,
```

- [ ] **Step 5: Update ServerConfig::default() with SS defaults**

```rust
// Add to ServerConfig::default()
ss_enabled: false,
ss_port: None,
ss_cipher: None,
ss_password: None,
```

- [ ] **Step 6: Update from_cli() to load SS config from all sources**

```rust
// In ServerConfig::from_cli(), add after loading TLS config:

// Environment variables for Shadowsocks
if let Ok(v) = std::env::var("SS_ENABLED") {
    config.ss_enabled = v.to_lowercase() == "true" || v == "1";
}
if let Ok(v) = std::env::var("SS_PORT") {
    if let Ok(port) = v.parse::<u16>() {
        config.ss_port = Some(port);
    }
}
if let Ok(v) = std::env::var("SS_CIPHER") {
    config.ss_cipher = Some(v);
}
if let Ok(v) = std::env::var("SS_PASSWORD") {
    config.ss_password = Some(v);
}

// Command line arguments (highest priority)
if let Some(v) = cli.ss_enabled {
    config.ss_enabled = v;
}
if let Some(v) = cli.ss_port {
    config.ss_port = Some(v);
}
if let Some(v) = cli.ss_cipher {
    config.ss_cipher = Some(v);
}
if let Some(v) = cli.ss_password {
    config.ss_password = Some(v);
}
```

- [ ] **Step 7: Add validation for SS configuration**

```rust
// Add at end of from_cli():
// Validate Shadowsocks configuration
if config.ss_enabled {
    if config.ss_port.is_none() {
        return Err("ss_port is required when ss_enabled is true".to_string());
    }
    if config.ss_cipher.is_none() {
        return Err("ss_cipher is required when ss_enabled is true".to_string());
    }
    if config.ss_password.is_none() {
        return Err("ss_password is required when ss_enabled is true".to_string());
    }
    // Validate cipher method
    let cipher = config.ss_cipher.as_ref().unwrap();
    if cipher != "aes-256-gcm" && cipher != "chacha20-ietf-poly1305" {
        return Err(format!("Unsupported cipher: {}. Supported: aes-256-gcm, chacha20-ietf-poly1305", cipher));
    }
}
```

- [ ] **Step 8: Run cargo check to verify compilation**

Run: `cargo check --bin rust-tunnel-server`
Expected: Compiles successfully with no errors

- [ ] **Step 9: Commit configuration changes**

```bash
git add Cargo.toml src/server/config.rs
git commit -m "feat(config): add shadowsocks configuration options"
```

---

### Task 2: Add PortType enum and refactor ClientInfo to PortInfo

**Files:**
- Modify: `src/server/control.rs`

- [ ] **Step 1: Add PortType enum definition**

```rust
// Add after ControlMessageSender definition
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortType {
    Tunnel,
    Shadowsocks,
}
```

- [ ] **Step 2: Add PortInfo enum wrapping ClientInfo and Shadowsocks config**

```rust
// Add after ClientInfo definition
#[derive(Debug, Clone)]
pub enum PortInfo {
    Tunnel(ClientInfo),
    Shadowsocks {
        port: u16,
        cipher: String,
        password: String,
        enabled: bool,
        created_at: i64,
    },
}

impl PortInfo {
    pub fn port_type(&self) -> PortType {
        match self {
            PortInfo::Tunnel(_) => PortType::Tunnel,
            PortInfo::Shadowsocks { .. } => PortType::Shadowsocks,
        }
    }

    pub fn port(&self) -> u16 {
        match self {
            PortInfo::Tunnel(info) => info.remote_port,
            PortInfo::Shadowsocks { port, .. } => *port,
        }
    }
}
```

- [ ] **Step 3: Update ServerState to use PortInfo instead of ClientInfo**

```rust
// Change in ServerState:
// From: clients: Arc<Mutex<HashMap<u16, ClientInfo>>>,
// To: ports: Arc<Mutex<HashMap<u16, PortInfo>>>,

// Update ServerState struct:
#[derive(Clone)]
pub struct ServerState {
    /// Map from port to port info (Tunnel or Shadowsocks)
    ports: Arc<Mutex<HashMap<u16, PortInfo>>>,
    /// Map from connection_id to active connection info
    active_connections: Arc<Mutex<HashMap<u64, ActiveConnection>>>,
    /// Traffic statistics store
    pub traffic_store: TrafficStore,
    /// Database connection (optional)
    db: Option<Database>,
    /// Connection quality store
    pub quality_store: QualityStore,
    /// Quality trackers per port
    quality_trackers: Arc<Mutex<HashMap<u16, QualityTracker>>>,
}
```

- [ ] **Step 4: Update ServerState::new() and ::with_db() initialization**

```rust
// In both new() and with_db():
// Change clients to ports
ports: Arc::new(Mutex::new(HashMap::new())),
```

- [ ] **Step 5: Add register_shadowsocks method to ServerState**

```rust
impl ServerState {
    // ... existing methods ...

    pub async fn register_shadowsocks(&self, port: u16, cipher: String, password: String) -> bool {
        let mut ports = self.ports.lock().await;
        if ports.contains_key(&port) {
            return false;
        }
        ports.insert(port, PortInfo::Shadowsocks {
            port,
            cipher,
            password,
            enabled: true,
            created_at: chrono::Utc::now().timestamp(),
        });
        true
    }

    pub async fn get_port(&self, port: u16) -> Option<PortInfo> {
        let ports = self.ports.lock().await;
        ports.get(&port).cloned()
    }

    pub async fn unregister_port(&self, port: u16) -> bool {
        let mut ports = self.ports.lock().await;
        ports.remove(&port).is_some()
    }

    // Update existing register_client to use PortInfo::Tunnel
    pub async fn register_client(&self, remote_port: u16, hostname: Option<String>, control_sender: ControlMessageSender) -> bool {
        let hostname_clone = hostname.clone();
        let mut ports = self.ports.lock().await;
        if ports.contains_key(&remote_port) {
            return false;
        }
        ports.insert(remote_port, PortInfo::Tunnel(ClientInfo {
            remote_port,
            hostname,
            control_sender,
        }));

        // Record client connection in database (keep existing logic)
        // ... existing db code ...

        true
    }

    // Update get_client to extract Tunnel variant
    pub async fn get_client(&self, remote_port: u16) -> Option<ClientInfo> {
        let ports = self.ports.lock().await;
        match ports.get(&remote_port) {
            Some(PortInfo::Tunnel(info)) => Some(info.clone()),
            _ => None,
        }
    }
}
```

- [ ] **Step 6: Run cargo check to verify compilation**

Run: `cargo check --bin rust-tunnel-server`
Expected: Fix any compilation errors related to renaming clients to ports

- [ ] **Step 7: Run existing tests to verify no regression**

Run: `cargo test server::config`
Run: `cargo test server::control`
Expected: All tests pass

- [ ] **Step 8: Commit port refactoring changes**

```bash
git add src/server/control.rs
git commit -m "refactor(server): add PortType and PortInfo for multi-port-type support"
```

---

### Task 3: Create shadowsocks protocol handling module

**Files:**
- Create: `src/server/shadowsocks.rs`
- Modify: `src/server/mod.rs`

- [ ] **Step 1: Add shadowsocks module declaration to src/server/mod.rs**

```rust
// Add after other module declarations
pub mod shadowsocks;
```

- [ ] **Step 2: Create shadowsocks.rs with SSConnectionContext struct and cipher traits**

```rust
use std::sync::Arc;
use tokio::net::TcpStream;
use tracing::{debug, error};
use crate::common::{TunnelError, TunnelResult};

/// Shadowsocks connection context holding encryption state and target info
#[derive(Debug, Clone)]
pub struct SSConnectionContext {
    pub cipher_type: String,
    pub key: Vec<u8>,
    pub target_addr: String,
    pub connection_id: u64,
    pub port: u16,
}

/// Trait for SS cipher operations
pub trait SSCipher: Send + Sync {
    fn encrypt(&mut self, data: &[u8]) -> TunnelResult<Vec<u8>>;
    fn decrypt(&mut self, data: &[u8]) -> TunnelResult<Vec<u8>>;
}

/// Derive encryption key from password using EVP_BytesToKey
pub fn derive_key(password: &str, cipher: &str) -> TunnelResult<Vec<u8>> {
    let key_len = match cipher {
        "aes-256-gcm" => 32,
        "chacha20-ietf-poly1305" => 32,
        _ => return Err(TunnelError::Protocol(format!("Unsupported cipher: {}", cipher))),
    };

    use md5::{Md5, Digest};
    let mut key = Vec::with_capacity(key_len);
    let mut prev = Vec::new();

    while key.len() < key_len {
        let mut hasher = Md5::new();
        if !prev.is_empty() {
            hasher.update(&prev);
        }
        hasher.update(password.as_bytes());
        let result = hasher.finalize();
        key.extend_from_slice(&result[..]);
        prev = result.to_vec();
    }

    key.truncate(key_len);
    Ok(key)
}
```

- [ ] **Step 3: Add cipher implementation using shadowsocks-rust crypto**

```rust
use shadowsocks::crypto::CipherKind;
use shadowsocks::crypto::aead::AeadCipher;

pub struct AeadCipherWrapper {
    cipher: AeadCipher,
    kind: CipherKind,
}

impl SSCipher for AeadCipherWrapper {
    fn encrypt(&mut self, data: &[u8]) -> TunnelResult<Vec<u8>> {
        // Implementation using shadowsocks-rust AeadCipher
        // Note: Actual implementation will use the proper shadowsocks-rust API
        Ok(data.to_vec()) // Placeholder - replace with real encryption
    }

    fn decrypt(&mut self, data: &[u8]) -> TunnelResult<Vec<u8>> {
        Ok(data.to_vec()) // Placeholder - replace with real decryption
    }
}

/// Create cipher from config
pub fn create_cipher(cipher_type: &str, key: &[u8]) -> TunnelResult<Box<dyn SSCipher>> {
    let kind = match cipher_type {
        "aes-256-gcm" => CipherKind::Aes256Gcm,
        "chacha20-ietf-poly1305" => CipherKind::ChaCha20IetfPoly1305,
        _ => return Err(TunnelError::Protocol(format!("Unsupported cipher: {}", cipher_type))),
    };

    // Placeholder: Create actual cipher instance using shadowsocks-rust API
    Ok(Box::new(AeadCipherWrapper {
        cipher: AeadCipher::new(kind, key),
        kind,
    }))
}
```

- [ ] **Step 4: Implement SS handshake and target address parsing**

```rust
/// Parse Shadowsocks target address from decrypted header
pub fn parse_target_address(data: &[u8]) -> TunnelResult<(String, usize)> {
    // Shadowsocks address format:
    // [1-byte type] [variable address] [2-byte port big-endian]
    // Type 0x01: IPv4 (4 bytes)
    // Type 0x03: Domain name (1-byte length + N bytes)
    // Type 0x04: IPv6 (16 bytes)

    if data.is_empty() {
        return Err(TunnelError::Protocol("Empty address data".to_string()));
    }

    let addr_type = data[0];
    let mut offset = 1;

    let host = match addr_type {
        0x01 => { // IPv4
            if data.len() < offset + 4 {
                return Err(TunnelError::Protocol("Incomplete IPv4 address".to_string()));
            }
            let ip = format!("{}.{}.{}.{}", data[offset], data[offset+1], data[offset+2], data[offset+3]);
            offset += 4;
            ip
        }
        0x03 => { // Domain name
            if data.len() < offset + 1 {
                return Err(TunnelError::Protocol("Missing domain length".to_string()));
            }
            let len = data[offset] as usize;
            offset += 1;
            if data.len() < offset + len {
                return Err(TunnelError::Protocol("Incomplete domain name".to_string()));
            }
            let domain = String::from_utf8_lossy(&data[offset..offset+len]).to_string();
            offset += len;
            domain
        }
        0x04 => { // IPv6
            if data.len() < offset + 16 {
                return Err(TunnelError::Protocol("Incomplete IPv6 address".to_string()));
            }
            let mut segments = Vec::new();
            for i in 0..8 {
                let seg = u16::from_be_bytes([data[offset + i*2], data[offset + i*2 + 1]]);
                segments.push(format!("{:x}", seg));
            }
            offset += 16;
            segments.join(":")
        }
        _ => return Err(TunnelError::Protocol(format!("Unknown address type: {}", addr_type))),
    };

    if data.len() < offset + 2 {
        return Err(TunnelError::Protocol("Missing port".to_string()));
    }

    let port = u16::from_be_bytes([data[offset], data[offset+1]]);
    offset += 2;

    Ok((format!("{}:{}", host, port), offset))
}

/// Handle SS handshake - placeholder for actual decryption and parsing
pub async fn handle_ss_handshake(
    stream: &mut TcpStream,
    cipher: &str,
    password: &str,
    connection_id: u64,
    port: u16,
) -> TunnelResult<(SSConnectionContext, Box<dyn SSCipher>)> {
    use tokio::io::AsyncReadExt;

    debug!("Starting SS handshake for connection {}, port {}", connection_id, port);

    // Derive key from password
    let key = derive_key(password, cipher)?;

    // Create cipher
    let ss_cipher = create_cipher(cipher, &key)?;

    // Placeholder: Read and decrypt handshake data
    // Actual implementation will:
    // 1. Read IV/salt
    // 2. Initialize cipher with IV/salt
    // 3. Read and decrypt encrypted address header
    // 4. Parse target address

    // For now, this is a placeholder that expects the target address in plaintext
    // Actual implementation will use the proper shadowsocks protocol
    let mut buf = [0u8; 512];
    let n = stream.peek(&mut buf).await?;

    if n == 0 {
        return Err(TunnelError::Protocol("Empty handshake".to_string()));
    }

    // Try to parse as plaintext for testing (will be replaced with decrypted data)
    match parse_target_address(&buf[..n]) {
        Ok((target_addr, consumed)) => {
            // Consume the bytes we peeked
            let mut consume_buf = vec![0u8; consumed];
            stream.read_exact(&mut consume_buf).await?;

            debug!("Parsed SS target address: {}", target_addr);

            let ctx = SSConnectionContext {
                cipher_type: cipher.to_string(),
                key,
                target_addr,
                connection_id,
                port,
            };

            Ok((ctx, ss_cipher))
        }
        Err(e) => {
            error!("Failed to parse SS address: {}", e);
            Err(e)
        }
    }
}
```

- [ ] **Step 5: Add unit tests for crypto and address parsing**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_derive_key() {
        let key = derive_key("test-password", "aes-256-gcm").unwrap();
        assert_eq!(key.len(), 32);
    }

    #[test]
    fn test_parse_ipv4_address() {
        // Type 0x01, IP 192.168.1.1, port 80
        let data = [0x01, 192, 168, 1, 1, 0x00, 0x50];
        let (addr, consumed) = parse_target_address(&data).unwrap();
        assert_eq!(addr, "192.168.1.1:80");
        assert_eq!(consumed, 7);
    }

    #[test]
    fn test_parse_domain_address() {
        // Type 0x03, domain length 11, "example.com", port 443
        let domain = b"example.com";
        let mut data = vec![0x03, domain.len() as u8];
        data.extend_from_slice(domain);
        data.extend_from_slice(&[0x01, 0xbb]); // port 443

        let (addr, consumed) = parse_target_address(&data).unwrap();
        assert_eq!(addr, "example.com:443");
        assert_eq!(consumed, 1 + 1 + domain.len() + 2);
    }
}
```

- [ ] **Step 6: Run tests to verify basic functionality**

Run: `cargo test server::shadowsocks`
Expected: All tests pass

- [ ] **Step 7: Commit shadowsocks module**

```bash
git add src/server/mod.rs src/server/shadowsocks.rs
git commit -m "feat(server): add shadowsocks protocol handling module"
```

---

### Task 4: Implement SS proxy connection handling

**Files:**
- Modify: `src/server/proxy.rs`

- [ ] **Step 1: Add SS proxy imports at top of proxy.rs**

```rust
use crate::server::shadowsocks::{SSConnectionContext, SSCipher};
use crate::server::control::ServerState;
use tokio::net::TcpStream;
use tracing::{debug, error, warn};
```

- [ ] **Step 2: Implement bidirectional copy with traffic counting**

```rust
/// Bidirectional copy between two streams with traffic accounting
async fn copy_bidirectional_with_stats(
    connection_id: u64,
    port: u16,
    mut client_stream: TcpStream,
    mut target_stream: TcpStream,
    state: ServerState,
) -> TunnelResult<(u64, u64)> {
    let (mut client_read, mut client_write) = client_stream.split();
    let (mut target_read, mut target_write) = target_stream.split();

    // Client -> Target upload
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
            state.traffic_store.record_tx(port, n as u64).await;
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
            state.traffic_store.record_rx(port, n as u64).await;
        }
        total
    };

    let (uploaded, downloaded) = tokio::join!(upload, download);
    Ok((uploaded, downloaded))
}
```

- [ ] **Step 3: Implement proxy_ss_connection main function**

```rust
/// Proxy a Shadowsocks connection to target
pub async fn proxy_ss_connection(
    connection_id: u64,
    ss_port: u16,
    user_stream: TcpStream,
    ss_ctx: SSConnectionContext,
    mut cipher: Box<dyn SSCipher>,
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

    // Note: Actual SS implementation will:
    // 1. Decrypt data from user_stream before sending to target_stream
    // 2. Encrypt data from target_stream before sending to user_stream
    //
    // For now, we're doing plain pass-through (placeholder for encryption layer)
    // The cipher wrapper will be integrated in the next iteration

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
```

- [ ] **Step 4: Run cargo check to verify compilation**

Run: `cargo check --bin rust-tunnel-server`
Expected: Compiles successfully

- [ ] **Step 5: Commit SS proxy implementation**

```bash
git add src/server/proxy.rs
git commit -m "feat(server): add shadowsocks proxy connection handler"
```

---

### Task 5: Extend listener to support Shadowsocks port type

**Files:**
- Modify: `src/server/listener.rs`

- [ ] **Step 1: Add imports for SS handling at top of listener.rs**

```rust
use crate::server::shadowsocks::{self, handle_ss_handshake};
use crate::server::proxy::proxy_ss_connection;
use crate::server::control::{PortInfo, PortType};
```

- [ ] **Step 2: Modify handle_inbound_connection to check port type**

```rust
async fn handle_inbound_connection(
    state: ServerState,
    remote_port: u16,
    connection_id: u64,
    user_stream: TcpStream,
) -> TunnelResult<()> {
    // Get port info
    let port_info = match state.get_port(remote_port).await {
        Some(info) => info,
        None => {
            warn!("No port registered for {}, closing connection", remote_port);
            return Ok(());
        }
    };

    match port_info {
        PortInfo::Tunnel(client_info) => {
            // Existing tunnel proxy logic
            debug!("Handling Tunnel connection on port {}", remote_port);
            proxy::proxy_user_connection(connection_id, remote_port, user_stream, client_info, state).await;
        }
        PortInfo::Shadowsocks { cipher, password, .. } => {
            // New Shadowsocks proxy logic
            debug!("Handling Shadowsocks connection on port {}", remote_port);

            // Handle SS handshake
            let mut stream_mut = user_stream;
            match handle_ss_handshake(&mut stream_mut, &cipher, &password, connection_id, remote_port).await {
                Ok((ss_ctx, ss_cipher)) => {
                    proxy_ss_connection(connection_id, remote_port, stream_mut, ss_ctx, ss_cipher, state).await;
                }
                Err(e) => {
                    warn!("SS handshake failed for connection {}: {}", connection_id, e);
                }
            }
        }
    }

    Ok(())
}
```

- [ ] **Step 3: Add function to start SS listener on server startup**

```rust
/// Start Shadowsocks listener if enabled
pub async fn start_shadowsocks_listener(
    state: ServerState,
    port: u16,
    cipher: String,
    password: String,
) -> TunnelResult<()> {
    // Register SS port in ServerState
    if !state.register_shadowsocks(port, cipher, password).await {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            format!("Port {} already in use", port),
        ).into());
    }

    // Start the listener (reuses existing run_listener logic)
    run_listener(state, port).await
}
```

- [ ] **Step 4: Run cargo check to verify compilation**

Run: `cargo check --bin rust-tunnel-server`
Expected: Compiles successfully

- [ ] **Step 5: Commit listener extensions**

```bash
git add src/server/listener.rs
git commit -m "feat(server): extend listener to support shadowsocks port type"
```

---

### Task 6: Integrate SS listener startup into server main

**Files:**
- Modify: `src/bin/server.rs`

- [ ] **Step 1: Add SS listener startup logic in server main**

```rust
// Find the section where server starts listeners
// After starting control server and before starting API server, add:

// Start Shadowsocks listener if enabled
if config.ss_enabled {
    let ss_port = config.ss_port.unwrap();
    let ss_cipher = config.ss_cipher.clone().unwrap();
    let ss_password = config.ss_password.clone().unwrap();

    info!("Starting Shadowsocks listener on port {}, cipher {}", ss_port, ss_cipher);

    let state_clone = state.clone();
    tokio::spawn(async move {
        if let Err(e) = listener::start_shadowsocks_listener(state_clone, ss_port, ss_cipher, ss_password).await {
            error!("Shadowsocks listener failed: {}", e);
        }
    });
}
```

- [ ] **Step 2: Build and test server startup**

Run: `cargo build --bin rust-tunnel-server`
Expected: Builds successfully

- [ ] **Step 3: Test SS configuration validation**

Run: `cargo run --bin rust-tunnel-server -- --ss-enabled --ss-port 8388 --ss-cipher aes-256-gcm`
Expected: Error: "ss_password is required when ss_enabled is true"

- [ ] **Step 4: Commit server integration**

```bash
git add src/bin/server.rs
git commit -m "feat(server): integrate shadowsocks listener startup"
```

---

## Phase 2: Quality Monitoring and API

### Task 7: Integrate QualityTracker for SS connections

**Files:**
- Modify: `src/server/proxy.rs`

- [ ] **Step 1: Update proxy_ss_connection to include quality tracking**

```rust
// Inside proxy_ss_connection, after connecting to target:

// Get or create quality tracker for this port
let quality_tracker = state.get_or_create_quality_tracker(ss_port).await;

// Wrap the bidirectional copy with quality measurements
// This will reuse existing quality tracking infrastructure from tunnel connections
```

- [ ] **Step 2: Implement and test quality tracking integration**

- [ ] **Step 3: Commit quality tracking integration**

---

### Task 8: Add Shadowsocks management API endpoints

**Files:**
- Modify: `src/server/api.rs`

- [ ] **Step 1: Add SS API routes and handlers**

- [ ] **Step 2: Implement GET /api/shadowsocks for status**

- [ ] **Step 3: Implement POST /api/shadowsocks for config updates**

- [ ] **Step 4: Implement GET /api/shadowsocks/stats for traffic statistics**

- [ ] **Step 5: Commit API endpoints**

---

### Task 9: Add database persistence for SS configuration and stats

**Files:**
- Modify: `src/server/db.rs`

- [ ] **Step 1: Create SS config table schema**

- [ ] **Step 2: Implement save/load SS config methods**

- [ ] **Step 3: Implement SS traffic stats persistence**

- [ ] **Step 4: Commit database integration**

---

### Task 10: Integration testing and bug fixes

**Files:**
- Create: `tests/shadowsocks_integration.rs`

- [ ] **Step 1: Write integration tests for SS proxy**

- [ ] **Step 2: Test SS and Tunnel running in parallel**

- [ ] **Step 3: Fix any discovered bugs**

- [ ] **Step 4: Run full test suite**

- [ ] **Step 5: Commit tests and fixes**

---

## Phase 3: Frontend Integration (Optional)

### Task 11: Add Shadowsocks management UI

**Files:**
- Create: `frontend/src/components/Shadowsocks.vue` or `.tsx`
- Modify: `frontend/src/router/index.ts`
- Modify: `frontend/src/api/index.ts`

- [ ] **Step 1: Add SS API client**

- [ ] **Step 2: Create SS management component**

- [ ] **Step 3: Add to Dashboard sidebar**

- [ ] **Step 4: Integrate SS traffic into Dashboard charts**

- [ ] **Step 5: Commit frontend changes**

---

## Post-Implementation Checklist

- [ ] All existing tunnel functionality works unchanged
- [ ] SS proxy works with standard SS clients
- [ ] Traffic stats correctly show both Tunnel and SS traffic
- [ ] Quality monitoring works for SS connections
- [ ] All unit tests pass
- [ ] All integration tests pass
- [ ] Performance overhead < 10% compared to standalone SS
- [ ] Documentation updated
