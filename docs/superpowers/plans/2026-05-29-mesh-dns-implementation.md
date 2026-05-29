# Mesh 组网 + 嵌入式 DNS 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为 rust-tunnel 添加 P2P Mesh 组网（打洞+中转回退）和嵌入式权威 DNS 服务器，让多个客户端形成虚拟网络并通过域名相互访问。

**Architecture:** 在现有 `ControlMessage` 协议上扩展，服务器充当 STUN 协调中心+DNS 权威+中继回退。客户端通过新的 control message 消息类型加入/离开 Mesh、发起 P2P 打洞、以及在中转模式下通信。DNS 服务器使用 `hickory-dns` crate 实现轻量级权威服务器，监听 UDP 53 端口，为 `*.tunnel.local`（隧道端口）和 `*.mesh.local`（Mesh 服务发现）提供解析。

**Tech Stack:** Rust/Tokio, hickory-dns (DNS), bincode (序列化), Axum (API), React/TypeScript (前端)

---

### Task 1: 扩展 ControlMessage 协议

**Files:**
- Modify: `src/common/protocol.rs`
- Test: `src/common/protocol.rs` (tests module)

- [ ] **Step 1: 将 Mesh 消息变体添加到 ControlMessage**

在 `src/common/protocol.rs` 的 `ControlMessage` 枚举中添加以下变体：

```rust
/// Mesh network registration (client -> server)
MeshJoin {
    mesh_id: String,
    client_name: String,
},
/// Leave a mesh network (client -> server)
MeshLeave {
    mesh_id: String,
},
/// Server sends mesh member list to clients (server -> client)
MeshMemberList {
    mesh_id: String,
    members: Vec<MeshMember>,
},
/// Request to connect to a service on another mesh client (client -> server)
MeshConnect {
    target_client: String,
    service_name: String,
},
/// Request P2P hole punch with target (client -> server, contains own public address)
P2PRequest {
    target_client: String,
    local_addr: String,   // "ip:port"
},
/// Forward P2P response with remote address info (server -> client)
P2PResponse {
    target_client: String,
    remote_addr: String,  // "ip:port"
},
/// Report P2P hole punch result (client -> server)
P2PResult {
    target_client: String,
    success: bool,
},
/// Relay data through server when P2P fails (client <-> server)
MeshRelay {
    target_client: String,
    data: Vec<u8>,
},
/// Client registers mesh services (client -> server, sent after MeshJoin)
MeshRegisterServices {
    mesh_id: String,
    services: Vec<MeshServiceDef>,
},
```

- [ ] **Step 2: 添加 MeshMember 和 MeshServiceDef 结构体**

在 `ControlMessage` 上方添加：

```rust
/// A member of a mesh network
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MeshMember {
    pub client_name: String,
    pub public_addr: Option<String>, // known public address after STUN
    pub online: bool,
}

/// A service exposed by a mesh client
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MeshServiceDef {
    pub name: String,         // e.g. "db"
    pub protocol: String,     // e.g. "mysql"
    pub local_addr: String,   // e.g. "localhost:3306"
}
```

- [ ] **Step 3: 运行测试验证序列化兼容性**

```bash
cargo test -p rust-tunnel -- common::protocol::tests
```

Expected: FAIL — 现有测试中 roundtrip_all_message_types 和 test_message_variants_serialization 没有包含新变体，需要更新

- [ ] **Step 4: 更新现有测试以包含新消息变体**

在 `test_message_variants_serialization` 中添加：

```rust
ControlMessage::MeshJoin {
    mesh_id: "test-mesh".into(),
    client_name: "client-a".into(),
},
ControlMessage::MeshLeave {
    mesh_id: "test-mesh".into(),
},
ControlMessage::MeshMemberList {
    mesh_id: "test-mesh".into(),
    members: vec![MeshMember {
        client_name: "client-a".into(),
        public_addr: Some("1.2.3.4:12345".into()),
        online: true,
    }],
},
ControlMessage::P2PRequest {
    target_client: "client-b".into(),
    local_addr: "1.2.3.4:12345".into(),
},
ControlMessage::P2PResponse {
    target_client: "client-b".into(),
    remote_addr: "5.6.7.8:54321".into(),
},
ControlMessage::MeshRelay {
    target_client: "client-b".into(),
    data: vec![1, 2, 3],
},
```

在 `test_roundtrip_all_message_types` 中添加同样的变体。

- [ ] **Step 5: 运行测试**

```bash
cargo test -p rust-tunnel -- common::protocol::tests
```

Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add src/common/protocol.rs
git commit -m "feat: add Mesh, P2P, and Relay message variants to ControlMessage"
```

---

### Task 2: 创建 Mesh 共享类型

**Files:**
- Create: `src/common/mesh_types.rs`
- Modify: `src/common/mod.rs`

- [ ] **Step 1: 创建 `src/common/mesh_types.rs`**

```rust
use serde::{Deserialize, Serialize};

/// Represents a route entry in the mesh routing table
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshRoute {
    /// Target client name
    pub client_name: String,
    /// Known public address (ip:port) if discovered via STUN
    pub public_addr: Option<String>,
    /// Whether P2P direct connection is available
    pub p2p_available: bool,
    /// Registered services on this client
    pub services: Vec<MeshService>,
}

/// A service registered by a mesh client
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshService {
    pub name: String,
    pub protocol: String,
    pub local_addr: String,
}

/// Represents a DNS record in the registry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DnsRecord {
    /// A record for tunnel port -> server IP
    TunnelA {
        name: String,         // e.g. "webapp.tunnel.local"
        target_ip: String,    // server public IP
        port: u16,            // exposed port
    },
    /// SRV record for tunnel service
    TunnelSrv {
        name: String,         // e.g. "_http._tcp.webapp.tunnel.local"
        target: String,       // server hostname
        port: u16,
    },
    /// A record for mesh service -> mesh client IP
    MeshA {
        name: String,         // e.g. "db.mynet.mesh.local"
        target_ip: String,
    },
    /// SRV record for mesh service
    MeshSrv {
        name: String,         // e.g. "_mysql._tcp.db.mynet.mesh.local"
        target: String,
        port: u16,
    },
    /// TXT metadata record
    Txt {
        name: String,
        text: String,
    },
}

impl DnsRecord {
    pub fn name(&self) -> &str {
        match self {
            DnsRecord::TunnelA { name, .. }
            | DnsRecord::TunnelSrv { name, .. }
            | DnsRecord::MeshA { name, .. }
            | DnsRecord::MeshSrv { name, .. }
            | DnsRecord::Txt { name, .. } => name,
        }
    }

    pub fn record_type(&self) -> &'static str {
        match self {
            DnsRecord::TunnelA { .. } | DnsRecord::MeshA { .. } => "A",
            DnsRecord::TunnelSrv { .. } | DnsRecord::MeshSrv { .. } => "SRV",
            DnsRecord::Txt { .. } => "TXT",
        }
    }
}

/// DNS configuration for a tunnel forward rule
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelDnsConfig {
    /// Custom DNS name (optional, e.g. "webapp" -> webapp.tunnel.local)
    pub dns_name: Option<String>,
    /// Protocol for SRV record (e.g. "http", "mysql")
    pub protocol: Option<String>,
}
```

- [ ] **Step 2: 更新 `src/common/mod.rs`**

```rust
pub mod error;
pub mod logging;
pub mod mesh_types;
pub mod protocol;
pub mod tls;

pub use error::*;
pub use logging::*;
pub use mesh_types::*;
pub use protocol::*;
pub use tls::*;
```

- [ ] **Step 3: 运行编译检查**

```bash
cargo check
```

Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add src/common/mesh_types.rs src/common/mod.rs
git commit -m "feat: add mesh shared types (MeshRoute, MeshService, DnsRecord)"
```

---

### Task 3: 实现 STUN 协议（精简版 RFC 5389）

**Files:**
- Create: `src/common/stun.rs`
- Modify: `src/common/mod.rs`

- [ ] **Step 1: 创建 `src/common/stun.rs`**

```rust
use rand::Rng;
use serde::{Deserialize, Serialize};

/// STUN message types (RFC 5389)
pub const STUN_BINDING_REQUEST: u16 = 0x0001;
pub const STUN_BINDING_RESPONSE: u16 = 0x0101;
pub const STUN_MAGIC_COOKIE: u32 = 0x2112A442;

/// MAPPED-ADDRESS attribute (RFC 5389 Section 15.1)
pub const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
/// XOR-MAPPED-ADDRESS attribute (RFC 5389 Section 15.2)
pub const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;

/// Parsed STUN message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StunMessage {
    pub msg_type: u16,
    pub transaction_id: [u8; 12],
    /// Parsed mapped address as "ip:port"
    pub mapped_address: Option<String>,
}

/// Build a STUN Binding Request packet
pub fn build_binding_request() -> ([u8; 20], [u8; 12]) {
    let mut rng = rand::thread_rng();
    let mut transaction_id = [0u8; 12];
    rng.fill(&mut transaction_id);

    let mut packet = [0u8; 20];
    // Message type: Binding Request (0x0001)
    packet[0] = 0x00;
    packet[1] = 0x01;
    // Message length: 0 (no attributes)
    packet[2] = 0x00;
    packet[3] = 0x00;
    // Magic cookie
    packet[4..8].copy_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
    // Transaction ID
    packet[8..20].copy_from_slice(&transaction_id);

    (packet, transaction_id)
}

/// Parse a STUN Binding Response and extract the XOR-MAPPED-ADDRESS
pub fn parse_binding_response(data: &[u8], expected_tid: &[u8; 12]) -> Option<StunMessage> {
    if data.len() < 20 {
        return None;
    }

    let msg_type = u16::from_be_bytes([data[0], data[1]]);
    if msg_type != STUN_BINDING_RESPONSE {
        return None;
    }

    let msg_len = u16::from_be_bytes([data[2], data[3]]) as usize;
    if data.len() < 20 + msg_len {
        return None;
    }

    // Verify magic cookie
    let magic = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    if magic != STUN_MAGIC_COOKIE {
        return None;
    }

    // Verify transaction ID
    let mut tid = [0u8; 12];
    tid.copy_from_slice(&data[8..20]);
    if &tid != expected_tid {
        return None;
    }

    // Parse attributes
    let mut mapped_address = None;
    let mut pos = 20;
    while pos + 4 <= data.len() && pos + 4 <= 20 + msg_len {
        let attr_type = u16::from_be_bytes([data[pos], data[pos + 1]]);
        let attr_len = u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
        pos += 4;

        if pos + attr_len > data.len() {
            break;
        }

        match attr_type {
            ATTR_XOR_MAPPED_ADDRESS => {
                if attr_len >= 8 {
                    if let Some(addr) = parse_xor_mapped_address(&data[pos..pos + attr_len]) {
                        mapped_address = Some(addr);
                    }
                }
            }
            ATTR_MAPPED_ADDRESS => {
                if attr_len >= 8 {
                    if let Some(addr) = parse_mapped_address(&data[pos..pos + attr_len]) {
                        mapped_address = Some(addr);
                    }
                }
            }
            _ => {}
        }

        // Align to 4-byte boundary
        pos += (attr_len + 3) & !3;
    }

    Some(StunMessage {
        msg_type,
        transaction_id: tid,
        mapped_address,
    })
}

fn parse_xor_mapped_address(data: &[u8]) -> Option<String> {
    if data.len() < 8 {
        return None;
    }
    // Skip first byte (reserved), second byte is family (0x01 = IPv4)
    let family = data[1];
    if family != 0x01 {
        return None; // Only IPv4 supported
    }
    let port_xor = u16::from_be_bytes([data[2], data[3]]);
    let port = port_xor ^ (STUN_MAGIC_COOKIE as u16);
    let ip_xor = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let ip = ip_xor ^ STUN_MAGIC_COOKIE;
    let ip_str = format!("{}.{}.{}.{}", (ip >> 24) & 0xFF, (ip >> 16) & 0xFF, (ip >> 8) & 0xFF, ip & 0xFF);
    Some(format!("{}:{}", ip_str, port))
}

fn parse_mapped_address(data: &[u8]) -> Option<String> {
    if data.len() < 8 {
        return None;
    }
    let family = data[1];
    if family != 0x01 {
        return None;
    }
    let port = u16::from_be_bytes([data[2], data[3]]);
    let ip = format!("{}.{}.{}.{}", data[4], data[5], data[6], data[7]);
    Some(format!("{}:{}", ip, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_binding_request() {
        let (packet, tid) = build_binding_request();
        assert_eq!(packet.len(), 20);
        assert_eq!(tid.len(), 12);
        // Verify message type header
        assert_eq!(packet[0], 0x00);
        assert_eq!(packet[1], 0x01);
        // Verify magic cookie
        let magic = u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]);
        assert_eq!(magic, STUN_MAGIC_COOKIE);
    }

    #[test]
    fn test_parse_binding_response_empty() {
        let result = parse_binding_response(&[], &[0u8; 12]);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_binding_response_short() {
        let result = parse_binding_response(&[0u8; 10], &[0u8; 12]);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_binding_response_wrong_type() {
        let mut packet = [0u8; 20];
        packet[0] = 0x00;
        packet[1] = 0x01; // Binding Request, not Response
        packet[4..8].copy_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
        let result = parse_binding_response(&packet, &[0u8; 12]);
        assert!(result.is_none());
    }

    #[test]
    fn test_build_binding_request_unique_tids() {
        let (_, tid1) = build_binding_request();
        let (_, tid2) = build_binding_request();
        assert_ne!(tid1, tid2);
    }
}
```

- [ ] **Step 2: 更新 `src/common/mod.rs`**

```rust
pub mod error;
pub mod logging;
pub mod mesh_types;
pub mod protocol;
pub mod stun;
pub mod tls;

pub use error::*;
pub use logging::*;
pub use mesh_types::*;
pub use protocol::*;
pub use stun::*;
pub use tls::*;
```

- [ ] **Step 3: 运行 STUN 测试**

```bash
cargo test -p rust-tunnel -- common::stun::tests
```

Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add src/common/stun.rs src/common/mod.rs
git commit -m "feat: add lightweight STUN protocol implementation (RFC 5389)"
```

---

### Task 4: 创建 Mesh 路由表

**Files:**
- Create: `src/server/mesh/mod.rs`
- Create: `src/server/mesh/router.rs`

- [ ] **Step 1: 创建 `src/server/mesh/router.rs`**

```rust
use std::collections::HashMap;
use crate::common::MeshRoute;

/// Mesh routing table tracking all mesh networks and their members.
pub struct MeshRouter {
    /// mesh_id -> (client_name -> MeshRoute)
    networks: HashMap<String, HashMap<String, MeshRoute>>,
}

impl MeshRouter {
    pub fn new() -> Self {
        Self {
            networks: HashMap::new(),
        }
    }

    /// Join a client to a mesh network
    pub fn join(&mut self, mesh_id: &str, client_name: &str) {
        self.networks
            .entry(mesh_id.to_string())
            .or_default()
            .entry(client_name.to_string())
            .or_insert_with(|| MeshRoute {
                client_name: client_name.to_string(),
                public_addr: None,
                p2p_available: false,
                services: Vec::new(),
            });
    }

    /// Remove a client from a mesh network
    pub fn leave(&mut self, mesh_id: &str, client_name: &str) -> bool {
        if let Some(members) = self.networks.get_mut(mesh_id) {
            let removed = members.remove(client_name).is_some();
            if members.is_empty() {
                self.networks.remove(mesh_id);
            }
            return removed;
        }
        false
    }

    /// Update a client's public address
    pub fn update_address(&mut self, mesh_id: &str, client_name: &str, addr: String) -> bool {
        if let Some(members) = self.networks.get_mut(mesh_id) {
            if let Some(route) = members.get_mut(client_name) {
                route.public_addr = Some(addr);
                return true;
            }
        }
        false
    }

    /// Set P2P availability for a client
    pub fn set_p2p_available(&mut self, mesh_id: &str, client_name: &str, available: bool) {
        if let Some(members) = self.networks.get_mut(mesh_id) {
            if let Some(route) = members.get_mut(client_name) {
                route.p2p_available = available;
            }
        }
    }

    /// Register services for a client
    pub fn register_services(
        &mut self,
        mesh_id: &str,
        client_name: &str,
        services: Vec<crate::common::MeshService>,
    ) {
        if let Some(members) = self.networks.get_mut(mesh_id) {
            if let Some(route) = members.get_mut(client_name) {
                route.services = services;
            }
        }
    }

    /// Get all members of a mesh
    pub fn get_members(&self, mesh_id: &str) -> Vec<&MeshRoute> {
        self.networks
            .get(mesh_id)
            .map(|m| m.values().collect())
            .unwrap_or_default()
    }

    /// Find which meshes a client belongs to
    pub fn get_client_meshes(&self, client_name: &str) -> Vec<String> {
        self.networks
            .iter()
            .filter(|(_, members)| members.contains_key(client_name))
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Look up a specific client in a mesh
    pub fn get_member(&self, mesh_id: &str, client_name: &str) -> Option<&MeshRoute> {
        self.networks.get(mesh_id)?.get(client_name)
    }

    /// Remove a client from all meshes
    pub fn remove_client(&mut self, client_name: &str) -> Vec<String> {
        let affected: Vec<String> = self.get_client_meshes(client_name);
        for mesh_id in &affected.clone() {
            self.leave(mesh_id, client_name);
        }
        affected
    }

    /// List all mesh networks
    pub fn list_networks(&self) -> Vec<String> {
        self.networks.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_join_and_leave() {
        let mut router = MeshRouter::new();
        router.join("mesh1", "client-a");
        assert_eq!(router.get_members("mesh1").len(), 1);

        router.leave("mesh1", "client-a");
        assert_eq!(router.get_members("mesh1").len(), 0);
    }

    #[test]
    fn test_leave_nonexistent() {
        let mut router = MeshRouter::new();
        assert!(!router.leave("mesh1", "nobody"));
    }

    #[test]
    fn test_update_address() {
        let mut router = MeshRouter::new();
        router.join("mesh1", "client-a");
        assert!(router.update_address("mesh1", "client-a", "1.2.3.4:12345".into()));
        let member = router.get_member("mesh1", "client-a").unwrap();
        assert_eq!(member.public_addr, Some("1.2.3.4:12345".into()));
    }

    #[test]
    fn test_remove_client_from_all() {
        let mut router = MeshRouter::new();
        router.join("mesh1", "client-a");
        router.join("mesh2", "client-a");
        router.join("mesh2", "client-b");

        let affected = router.remove_client("client-a");
        assert_eq!(affected.len(), 2);
        assert_eq!(router.get_members("mesh1").len(), 0);
        assert_eq!(router.get_members("mesh2").len(), 1); // client-b remains
    }

    #[test]
    fn test_list_networks() {
        let mut router = MeshRouter::new();
        router.join("mesh1", "client-a");
        router.join("mesh2", "client-b");
        let nets = router.list_networks();
        assert_eq!(nets.len(), 2);
    }

    #[test]
    fn test_get_client_meshes() {
        let mut router = MeshRouter::new();
        router.join("mesh1", "client-a");
        router.join("mesh2", "client-a");
        let meshes = router.get_client_meshes("client-a");
        assert_eq!(meshes.len(), 2);
    }

    #[test]
    fn test_register_services() {
        let mut router = MeshRouter::new();
        router.join("mesh1", "client-a");
        router.register_services("mesh1", "client-a", vec![
            crate::common::MeshService {
                name: "db".into(),
                protocol: "mysql".into(),
                local_addr: "localhost:3306".into(),
            },
        ]);
        let member = router.get_member("mesh1", "client-a").unwrap();
        assert_eq!(member.services.len(), 1);
        assert_eq!(member.services[0].name, "db");
    }
}
```

- [ ] **Step 2: 运行测试**

```bash
cargo test -p rust-tunnel -- server::mesh::router::tests
```

Expected: PASS

- [ ] **Step 3: Commit**

```bash
git add src/server/mesh/ src/server/mod.rs
git commit -m "feat: add MeshRouter with join/leave/route/address management"
```

---

### Task 5: 创建 Mesh 中继转发

**Files:**
- Create: `src/server/mesh/relay.rs`

- [ ] **Step 1: 创建 `src/server/mesh/relay.rs`**

```rust
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use crate::common::{ControlMessage, MeshRelayError};

/// Relay tunnel between two mesh clients via the server.
/// Bi-directional: data from A is forwarded to B and vice versa.
pub struct MeshRelay {
    /// Maps client_name -> mpsc Sender for delivering MeshRelay messages
    tunnels: Arc<Mutex<HashMap<String, mpsc::Sender<ControlMessage>>>>,
}

impl MeshRelay {
    pub fn new() -> Self {
        Self {
            tunnels: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Register a client's control channel for relay delivery
    pub async fn register(&self, client_name: &str, tx: mpsc::Sender<ControlMessage>) {
        let mut tunnels = self.tunnels.lock().await;
        tunnels.insert(client_name.to_string(), tx);
    }

    /// Unregister a client
    pub async fn unregister(&self, client_name: &str) {
        let mut tunnels = self.tunnels.lock().await;
        tunnels.remove(client_name);
    }

    /// Relay data from source to target
    pub async fn relay_data(
        &self,
        source: &str,
        target: &str,
        data: Vec<u8>,
    ) -> Result<(), MeshRelayError> {
        let tunnels = self.tunnels.lock().await;
        let tx = tunnels
            .get(target)
            .ok_or_else(|| MeshRelayError::TargetNotFound(target.to_string()))?;

        let msg = ControlMessage::MeshRelay {
            target_client: source.to_string(), // so receiver knows who sent it
            data,
        };

        tx.send(msg).await.map_err(|_| MeshRelayError::SendFailed)?;
        Ok(())
    }
}

/// Errors for mesh relay operations
#[derive(Debug, thiserror::Error)]
pub enum MeshRelayError {
    #[error("Target client not found: {0}")]
    TargetNotFound(String),
    #[error("Failed to send relay message")]
    SendFailed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_register_and_unregister() {
        let relay = MeshRelay::new();
        let (tx, _rx) = mpsc::channel::<ControlMessage>(16);

        relay.register("client-a", tx).await;
        {
            let tunnels = relay.tunnels.lock().await;
            assert!(tunnels.contains_key("client-a"));
        }

        relay.unregister("client-a").await;
        {
            let tunnels = relay.tunnels.lock().await;
            assert!(!tunnels.contains_key("client-a"));
        }
    }

    #[tokio::test]
    async fn test_relay_data_target_not_found() {
        let relay = MeshRelay::new();
        let result = relay.relay_data("client-a", "client-b", vec![1, 2, 3]).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_relay_data_success() {
        let relay = MeshRelay::new();
        let (tx, mut rx) = mpsc::channel::<ControlMessage>(16);

        relay.register("client-b", tx).await;
        relay
            .relay_data("client-a", "client-b", vec![1, 2, 3])
            .await
            .unwrap();

        let msg = rx.recv().await.unwrap();
        match msg {
            ControlMessage::MeshRelay { target_client, data } => {
                assert_eq!(target_client, "client-a");
                assert_eq!(data, vec![1, 2, 3]);
            }
            _ => panic!("Unexpected message"),
        }
    }
}
```

- [ ] **Step 2: 运行测试**

```bash
cargo test -p rust-tunnel -- server::mesh::relay::tests
```

Expected: 编译错误 — `MeshRelayError` 需要在 `common::error.rs` 中定义

- [ ] **Step 3: 在 `src/common/error.rs` 中添加 MeshRelayError**

在 `TunnelError` 枚举中添加变体：

```rust
#[error("Mesh relay error: {0}")]
MeshRelay(String),
```

然后删除 `relay.rs` 中的 `MeshRelayError` enum，改用 `crate::common::TunnelError`:

```rust
use crate::common::{ControlMessage, TunnelError};

pub async fn relay_data(
    &self,
    source: &str,
    target: &str,
    data: Vec<u8>,
) -> Result<(), TunnelError> {
    let tunnels = self.tunnels.lock().await;
    let tx = tunnels
        .get(target)
        .ok_or_else(|| TunnelError::MeshRelay(format!("Target not found: {}", target)))?;
    // ...
}
```

- [ ] **Step 4: 运行测试**

```bash
cargo test -p rust-tunnel -- server::mesh::relay::tests
```

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/server/mesh/relay.rs src/common/error.rs
git commit -m "feat: add MeshRelay for server-side relay forwarding between mesh clients"
```

---

### Task 6: 创建 STUN 服务器模块

**Files:**
- Create: `src/server/mesh/stun.rs`

- [ ] **Step 1: 创建 `src/server/mesh/stun.rs`**

```rust
use std::net::SocketAddr;
use tokio::net::UdpSocket;
use crate::common::{
    build_binding_request, parse_binding_response, STUN_BINDING_RESPONSE,
    STUN_MAGIC_COOKIE, ATTR_XOR_MAPPED_ADDRESS,
};
use anyhow::Result;

/// Lightweight STUN server that responds to Binding Requests
/// with the client's observed public address.
pub struct StunServer {
    socket: UdpSocket,
}

impl StunServer {
    pub async fn bind(addr: &str) -> Result<Self> {
        let socket = UdpSocket::bind(addr).await?;
        Ok(Self { socket })
    }

    /// Run the STUN server loop
    pub async fn run(self) -> Result<()> {
        let mut buf = [0u8; 1500];
        loop {
            let (len, src_addr) = self.socket.recv_from(&mut buf).await?;
            let response = Self::handle_binding_request(&buf[..len], src_addr);
            if let Some(resp) = response {
                self.socket.send_to(&resp, src_addr).await?;
            }
        }
    }

    fn handle_binding_request(data: &[u8], src_addr: SocketAddr) -> Option<Vec<u8>> {
        if data.len() < 20 {
            return None;
        }

        let msg_type = u16::from_be_bytes([data[0], data[1]]);
        // Only handle Binding Requests
        if msg_type != 0x0001 {
            return None;
        }

        let transaction_id = &data[8..20];

        // Build XOR-MAPPED-ADDRESS attribute
        let mut attr = Vec::new();
        match src_addr {
            SocketAddr::V4(v4) => {
                let family: u16 = 0x01; // IPv4
                let ip = u32::from_be_bytes(v4.ip().octets());
                let port = v4.port();

                // XOR with magic cookie
                let xor_port = port ^ (STUN_MAGIC_COOKIE as u16);
                let xor_ip = ip ^ STUN_MAGIC_COOKIE;

                attr.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes()); // type
                attr.extend_from_slice(&8u16.to_be_bytes()); // length
                attr.push(0x00); // reserved
                attr.push(family as u8);
                attr.extend_from_slice(&xor_port.to_be_bytes());
                attr.extend_from_slice(&xor_ip.to_be_bytes());
            }
            SocketAddr::V6(_) => {
                // IPv6 not supported in lightweight implementation
                return None;
            }
        }

        let mut response = Vec::with_capacity(20 + attr.len());
        // Message type: Binding Success Response (0x0101)
        response.extend_from_slice(&0x0101u16.to_be_bytes());
        // Message length
        response.extend_from_slice(&(attr.len() as u16).to_be_bytes());
        // Magic cookie
        response.extend_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
        // Transaction ID (copied from request)
        response.extend_from_slice(transaction_id);
        // Attributes
        response.extend_from_slice(&attr);

        Some(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4};

    #[test]
    fn test_handle_binding_request_valid() {
        let (request, tid) = build_binding_request();
        let src = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(1, 2, 3, 4), 12345));

        let response = StunServer::handle_binding_request(&request, src).unwrap();
        assert!(response.len() > 20);

        // Verify it's a Binding Response
        let msg_type = u16::from_be_bytes([response[0], response[1]]);
        assert_eq!(msg_type, STUN_BINDING_RESPONSE);

        // Verify transaction ID is echoed
        let resp_tid = &response[8..20];
        assert_eq!(resp_tid, &tid as &[u8]);
    }

    #[test]
    fn test_handle_binding_request_empty() {
        let src = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(1, 2, 3, 4), 12345));
        let response = StunServer::handle_binding_request(&[], src);
        assert!(response.is_none());
    }
}
```

- [ ] **Step 2: 运行测试**

```bash
cargo test -p rust-tunnel -- server::mesh::stun::tests
```

Expected: PASS

- [ ] **Step 3: Commit**

```bash
git add src/server/mesh/stun.rs
git commit -m "feat: add lightweight STUN server for NAT address discovery"
```

---

### Task 7: 创建 Mesh 管理器并集成到 ServerState

**Files:**
- Create: `src/server/mesh/mod.rs`
- Modify: `src/server/mod.rs`
- Modify: `src/server/control.rs`

- [ ] **Step 1: 创建 `src/server/mesh/mod.rs`**

```rust
pub mod relay;
pub mod router;
pub mod stun;

use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::mpsc;

use crate::common::{ControlMessage, MeshMember, MeshRoute, MeshService};

use self::relay::MeshRelay;
use self::router::MeshRouter;

/// Central mesh manager: combines routing table + relay + per-client control channels
pub struct MeshManager {
    pub router: Arc<Mutex<MeshRouter>>,
    pub relay: MeshRelay,
    /// client_name -> mpsc Sender for ControlMessage delivery
    clients: Arc<Mutex<std::collections::HashMap<String, mpsc::Sender<ControlMessage>>>>,
}

impl MeshManager {
    pub fn new() -> Self {
        Self {
            router: Arc::new(Mutex::new(MeshRouter::new())),
            relay: MeshRelay::new(),
            clients: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Register a client's control channel
    pub async fn register_client(
        &self,
        client_name: &str,
        tx: mpsc::Sender<ControlMessage>,
    ) {
        self.relay.register(client_name, tx.clone()).await;
        self.clients.lock().await.insert(client_name.to_string(), tx);
    }

    /// Unregister a client from all meshes and relay
    pub async fn unregister_client(&self, client_name: &str) {
        self.relay.unregister(client_name).await;
        self.clients.lock().await.remove(client_name);
        self.router.lock().await.remove_client(client_name);
    }

    /// Join a mesh
    pub async fn join_mesh(&self, mesh_id: &str, client_name: &str) -> Vec<MeshMember> {
        self.router.lock().await.join(mesh_id, client_name);
        self.get_members_for(mesh_id, client_name).await
    }

    /// Leave a mesh
    pub async fn leave_mesh(&self, mesh_id: &str, client_name: &str) -> Vec<MeshMember> {
        self.router.lock().await.leave(mesh_id, client_name);
        self.get_members_for(mesh_id, client_name).await
    }

    /// Register services for a client in a mesh
    pub async fn register_services(
        &self,
        mesh_id: &str,
        client_name: &str,
        services: Vec<MeshService>,
    ) {
        self.router.lock().await.register_services(mesh_id, client_name, services);
    }

    /// Build member list for broadcast (excludes the requesting client)
    async fn get_members_for(&self, mesh_id: &str, exclude: &str) -> Vec<MeshMember> {
        let router = self.router.lock().await;
        router
            .get_members(mesh_id)
            .into_iter()
            .map(|r| MeshMember {
                client_name: r.client_name.clone(),
                public_addr: r.public_addr.clone(),
                online: r.client_name != exclude, // exclude is the requester
            })
            .collect()
    }

    /// Send message to a specific client
    pub async fn send_to_client(&self, client_name: &str, msg: ControlMessage) -> bool {
        if let Some(tx) = self.clients.lock().await.get(client_name) {
            tx.send(msg).await.is_ok()
        } else {
            false
        }
    }

    /// Broadcast to all clients in a mesh
    pub async fn broadcast_to_mesh(
        &self,
        mesh_id: &str,
        msg: ControlMessage,
    ) {
        let router = self.router.lock().await;
        let clients = self.clients.lock().await;
        for member in router.get_members(mesh_id) {
            if let Some(tx) = clients.get(&member.client_name) {
                let _ = tx.send(msg.clone()).await;
            }
        }
    }

    /// Get all mesh networks and their members
    pub async fn list_networks(&self) -> Vec<(String, Vec<MeshRoute>)> {
        let router = self.router.lock().await;
        router
            .list_networks()
            .into_iter()
            .map(|id| {
                let members = router.get_members(&id).into_iter().cloned().collect();
                (id, members)
            })
            .collect()
    }

    /// Get a specific mesh's details
    pub async fn get_mesh(&self, mesh_id: &str) -> Option<Vec<MeshRoute>> {
        let router = self.router.lock().await;
        if router.list_networks().contains(&mesh_id.to_string()) {
            Some(router.get_members(mesh_id).into_iter().cloned().collect())
        } else {
            None
        }
    }

    /// Look up a service in a mesh
    pub async fn lookup_service(
        &self,
        mesh_id: &str,
        service_name: &str,
    ) -> Option<(MeshRoute, MeshService)> {
        let router = self.router.lock().await;
        for member in router.get_members(mesh_id) {
            for svc in &member.services {
                if svc.name == service_name {
                    return Some((member.clone(), svc.clone()));
                }
            }
        }
        None
    }
}
```

- [ ] **Step 2: 更新 `src/server/mod.rs` 添加 mesh 子模块**

```rust
pub mod api;
pub mod auth;
pub mod config;
pub mod control;
pub mod db;
pub mod listener;
pub mod logs;
pub mod mesh;
pub mod proxy;
pub mod quality;
pub mod shadowsocks;
pub mod trojan;

#[cfg(test)]
mod shadowsocks_test;

#[cfg(test)]
mod trojan_test;

pub use api::TrafficStore;
pub use config::*;
pub use db::Database;
```

- [ ] **Step 3: 在 ServerState 中添加 MeshManager**

在 `src/server/control.rs` 的 `ServerState` 结构体中添加字段：

```rust
/// Mesh network manager
pub mesh_manager: MeshManager,
```

在文件顶部添加引用：
```rust
use crate::server::mesh::MeshManager;
```

更新 ServerState 构造函数以包含 `mesh_manager: MeshManager::new()`。

- [ ] **Step 4: 运行编译检查**

```bash
cargo check
```

Expected: 需要根据编译错误调整 ServerState 构造处

- [ ] **Step 5: 找到 ServerState 构造位置并添加 mesh_manager**

```bash
grep -n "ServerState {" src/server/control.rs
```

确认构造函数并添加 `mesh_manager: MeshManager::new()`。

- [ ] **Step 6: Commit**

```bash
git add src/server/mesh/mod.rs src/server/mod.rs src/server/control.rs
git commit -m "feat: add MeshManager with routing, relay, and client management"
```

---

### Task 8: 处理服务器端 Mesh 控制消息

**Files:**
- Modify: `src/server/control.rs`

- [ ] **Step 1: 在控制消息处理循环中添加 Mesh 消息处理**

在 `src/server/control.rs` 的消息处理 match 语句中添加对新 Mesh 消息的处理。找到 `process_control_message` 或等效的消息处理函数，添加以下分支：

```rust
ControlMessage::MeshJoin { mesh_id, client_name } => {
    let members = state.mesh_manager.join_mesh(&mesh_id, &client_name).await;
    // Register client for relay
    state.mesh_manager.register_client(&client_name, tx.clone()).await;
    // Send member list back to requester
    let _ = tx
        .send(ControlMessage::MeshMemberList {
            mesh_id: mesh_id.clone(),
            members: members.iter().map(|m| MeshMember {
                client_name: m.client_name.clone(),
                public_addr: m.public_addr.clone(),
                online: m.online,
            }).collect(),
        })
        .await;
    // Notify other members of new joiner
    let notify_msg = ControlMessage::MeshMemberList {
        mesh_id: mesh_id.clone(),
        members: members.into_iter().map(|m| MeshMember {
            client_name: m.client_name.clone(),
            public_addr: m.public_addr.clone(),
            online: true,
        }).collect(),
    };
    state.mesh_manager.broadcast_to_mesh(&mesh_id, notify_msg).await;
}

ControlMessage::MeshLeave { mesh_id } => {
    let members = state.mesh_manager.leave_mesh(&mesh_id, &client_name).await;
    // Notify remaining members
    let notify_msg = ControlMessage::MeshMemberList {
        mesh_id: mesh_id.clone(),
        members: members.into_iter().map(|m| MeshMember {
            client_name: m.client_name.clone(),
            public_addr: m.public_addr.clone(),
            online: true,
        }).collect(),
    };
    state.mesh_manager.broadcast_to_mesh(&mesh_id, notify_msg).await;
}

ControlMessage::P2PRequest { target_client, local_addr } => {
    // Update requestor's address in router
    state.mesh_manager.router.lock().await.update_address(
        "", // mesh_id needs context — we need client_name-to-mesh_id mapping
        &client_name,
        local_addr.clone(),
    );
    // Forward to target
    state.mesh_manager.send_to_client(&target_client, ControlMessage::P2PResponse {
        target_client: client_name.clone(),
        remote_addr: local_addr,
    }).await;
}

ControlMessage::P2PResult { target_client, success } => {
    // Record P2P status
    // Need mesh_id context for the router update
    if success {
        info!("P2P connection established between {} and {}", client_name, target_client);
    } else {
        info!("P2P hole punch failed between {} and {}", client_name, target_client);
    }
}

ControlMessage::MeshRelay { target_client, data } => {
    // Relay data to target
    if let Err(e) = state.mesh_manager.relay.relay_data(
        &client_name,
        &target_client,
        data,
    ).await {
        warn!("Mesh relay failed from {} to {}: {}", client_name, target_client, e);
    }
}

ControlMessage::MeshRegisterServices { mesh_id, services } => {
    let mesh_services: Vec<MeshService> = services.into_iter().map(|s| MeshService {
        name: s.name,
        protocol: s.protocol,
        local_addr: s.local_addr,
    }).collect();
    state.mesh_manager.register_services(&mesh_id, &client_name, mesh_services).await;
}
```

注意：这里需要确认 `client_name` 变量如何在消息处理循环中可用。现有的 `Register` 消息可能只有 `hostname`，需要确保 server 能追踪每个连接的 client_name。

- [ ] **Step 2: 在 client 连接信息中存储 client_name**

修改 `ClientInfo` 结构体添加 `client_name` 字段，并在 Register/mesh 消息处理中关联。

- [ ] **Step 3: 运行编译检查并修复错误**

```bash
cargo check 2>&1 | head -50
```

- [ ] **Step 4: Commit**

```bash
git add src/server/control.rs
git commit -m "feat: handle Mesh control messages in server (join, leave, P2P, relay)"
```

---

### Task 9: DNS 区域管理

**Files:**
- Create: `src/server/dns/mod.rs`
- Create: `src/server/dns/zone.rs`

- [ ] **Step 1: 添加 hickory-dns 依赖到 Cargo.toml**

在 `Cargo.toml` 的 `[dependencies]` 中添加：

```toml
# DNS server
hickory-server = { version = "0.24", default-features = false, features = ["resolver"] }
hickory-proto = "0.24"
```

- [ ] **Step 2: 创建 `src/server/dns/zone.rs`**

```rust
use std::collections::HashMap;
use chrono::Utc;
use crate::common::DnsRecord;

/// In-memory DNS zone for tunnel.local and mesh.local
pub struct DnsZone {
    /// All records keyed by domain name
    records: HashMap<String, Vec<DnsRecord>>,
}

impl DnsZone {
    pub fn new() -> Self {
        Self {
            records: HashMap::new(),
        }
    }

    /// Add a DNS record
    pub fn add_record(&mut self, record: DnsRecord) {
        let name = record.name().to_lowercase();
        self.records.entry(name).or_default().push(record);
    }

    /// Remove all records for a given name
    pub fn remove_records(&mut self, name: &str) -> usize {
        self.records.remove(&name.to_lowercase()).map(|v| v.len()).unwrap_or(0)
    }

    /// Remove records matching a predicate
    pub fn remove_by_predicate<F>(&mut self, predicate: F) -> usize
    where
        F: Fn(&DnsRecord) -> bool,
    {
        let mut count = 0;
        self.records.retain(|_, records| {
            let before = records.len();
            records.retain(|r| !predicate(r));
            count += before - records.len();
            !records.is_empty()
        });
        count
    }

    /// Get all records for a name
    pub fn get_records(&self, name: &str) -> Vec<&DnsRecord> {
        self.records
            .get(&name.to_lowercase())
            .map(|v| v.iter().collect())
            .unwrap_or_default()
    }

    /// Get A records for a name
    pub fn get_a_records(&self, name: &str) -> Vec<String> {
        self.get_records(name)
            .iter()
            .filter_map(|r| match r {
                DnsRecord::TunnelA { target_ip, .. } => Some(target_ip.clone()),
                DnsRecord::MeshA { target_ip, .. } => Some(target_ip.clone()),
                _ => None,
            })
            .collect()
    }

    /// Get SRV records for a name
    pub fn get_srv_records(&self, name: &str) -> Vec<(String, u16)> {
        self.get_records(name)
            .iter()
            .filter_map(|r| match r {
                DnsRecord::TunnelSrv { target, port, .. } => Some((target.clone(), *port)),
                DnsRecord::MeshSrv { target, port, .. } => Some((target.clone(), *port)),
                _ => None,
            })
            .collect()
    }

    /// List all unique record names
    pub fn list_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.records.keys().cloned().collect();
        names.sort();
        names
    }

    /// List all records (returns copies)
    pub fn list_all(&self) -> Vec<DnsRecord> {
        self.records.values().flat_map(|v| v.iter().cloned()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tunnel_a(name: &str, ip: &str, port: u16) -> DnsRecord {
        DnsRecord::TunnelA {
            name: name.to_string(),
            target_ip: ip.to_string(),
            port,
        }
    }

    #[test]
    fn test_add_and_get_record() {
        let mut zone = DnsZone::new();
        zone.add_record(make_tunnel_a("webapp.tunnel.local", "10.0.0.1", 9000));

        let records = zone.get_records("webapp.tunnel.local");
        assert_eq!(records.len(), 1);

        let a_records = zone.get_a_records("webapp.tunnel.local");
        assert_eq!(a_records, vec!["10.0.0.1"]);
    }

    #[test]
    fn test_remove_records() {
        let mut zone = DnsZone::new();
        zone.add_record(make_tunnel_a("webapp.tunnel.local", "10.0.0.1", 9000));
        let removed = zone.remove_records("webapp.tunnel.local");
        assert_eq!(removed, 1);
        assert!(zone.get_records("webapp.tunnel.local").is_empty());
    }

    #[test]
    fn test_case_insensitive() {
        let mut zone = DnsZone::new();
        zone.add_record(make_tunnel_a("Webapp.Tunnel.Local", "10.0.0.1", 9000));
        assert_eq!(zone.get_a_records("webapp.tunnel.local").len(), 1);
        assert_eq!(zone.get_a_records("WEBAPP.TUNNEL.LOCAL").len(), 1);
    }

    #[test]
    fn test_remove_by_predicate() {
        let mut zone = DnsZone::new();
        zone.add_record(make_tunnel_a("a.tunnel.local", "10.0.0.1", 9000));
        zone.add_record(make_tunnel_a("b.tunnel.local", "10.0.0.2", 9001));

        let removed = zone.remove_by_predicate(|r| {
            matches!(r, DnsRecord::TunnelA { port, .. } if *port == 9000)
        });
        assert_eq!(removed, 1);
        assert_eq!(zone.list_names().len(), 1);
    }

    #[test]
    fn test_list_all() {
        let mut zone = DnsZone::new();
        zone.add_record(make_tunnel_a("a.tunnel.local", "10.0.0.1", 9000));
        zone.add_record(make_tunnel_a("b.tunnel.local", "10.0.0.2", 9001));
        assert_eq!(zone.list_all().len(), 2);
    }
}
```

- [ ] **Step 3: 运行测试**

```bash
cargo test -p rust-tunnel -- server::dns::zone::tests
```

Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add src/server/dns/zone.rs Cargo.toml
git commit -m "feat: add DNS zone management (add/remove/query records)"
```

---

### Task 10: DNS 注册表（registry）

**Files:**
- Create: `src/server/dns/registry.rs`

- [ ] **Step 1: 创建 `src/server/dns/registry.rs`**

```rust
use std::sync::Arc;
use tokio::sync::Mutex;
use crate::common::DnsRecord;
use crate::server::dns::zone::DnsZone;

/// Central DNS registry that ties tunnel ports and mesh services to DNS records.
/// Thread-safe wrapper around DnsZone.
#[derive(Clone)]
pub struct DnsRegistry {
    zone: Arc<Mutex<DnsZone>>,
    /// Server's public IP for tunnel A records
    server_ip: Arc<Mutex<String>>,
    tunnel_domain: String,
    mesh_domain: String,
}

impl DnsRegistry {
    pub fn new(server_ip: String, tunnel_domain: String, mesh_domain: String) -> Self {
        Self {
            zone: Arc::new(Mutex::new(DnsZone::new())),
            server_ip: Arc::new(Mutex::new(server_ip)),
            tunnel_domain,
            mesh_domain,
        }
    }

    /// Register a tunnel port DNS record
    pub async fn register_tunnel(
        &self,
        dns_name: &str,
        port: u16,
        protocol: Option<&str>,
    ) {
        let a_name = format!("{}.{}", dns_name, self.tunnel_domain);
        let ip = self.server_ip.lock().await.clone();
        let mut zone = self.zone.lock().await;

        zone.add_record(DnsRecord::TunnelA {
            name: a_name.clone(),
            target_ip: ip.clone(),
            port,
        });

        if let Some(proto) = protocol {
            let srv_name = format!("_{}._tcp.{}.{}", proto, dns_name, self.tunnel_domain);
            zone.add_record(DnsRecord::TunnelSrv {
                name: srv_name,
                target: a_name,
                port,
            });
        }
    }

    /// Auto-register tunnel port with default name
    pub async fn register_tunnel_default(&self, port: u16, protocol: Option<&str>) -> String {
        let dns_name = format!("port-{}", port);
        self.register_tunnel(&dns_name, port, protocol).await;
        format!("{}.{}", dns_name, self.tunnel_domain)
    }

    /// Unregister all records for a tunnel by its DNS name
    pub async fn unregister_tunnel(&self, dns_name: &str, port: u16) {
        let mut zone = self.zone.lock().await;
        let a_name = format!("{}.{}", dns_name, self.tunnel_domain);
        zone.remove_records(&a_name);
        // Also clean up default name for this port
        let default_name = format!("port-{}.{}", port, self.tunnel_domain);
        if default_name != a_name {
            zone.remove_records(&default_name);
        }
    }

    /// Register a mesh service as DNS record
    pub async fn register_mesh_service(
        &self,
        mesh_id: &str,
        client_name: &str,
        service_name: &str,
        protocol: &str,
        target_ip: &str,
        port: u16,
    ) {
        let name = format!("{}.{}.{}", service_name, mesh_id, self.mesh_domain);
        let mut zone = self.zone.lock().await;

        zone.add_record(DnsRecord::MeshA {
            name: name.clone(),
            target_ip: target_ip.to_string(),
        });

        let srv_name = format!("_{}._tcp.{}.{}.{}", protocol, service_name, mesh_id, self.mesh_domain);
        zone.add_record(DnsRecord::MeshSrv {
            name: srv_name,
            target: name,
            port,
        });
    }

    /// Unregister all mesh services for a client
    pub async fn unregister_mesh_client(
        &self,
        mesh_id: &str,
        client_name: &str,
    ) {
        let mut zone = self.zone.lock().await;
        zone.remove_by_predicate(|r| match r {
            DnsRecord::MeshA { name, .. } | DnsRecord::MeshSrv { name, .. } => {
                name.contains(mesh_id) && name.contains(client_name)
            }
            _ => false,
        });
    }

    /// Query A records
    pub async fn query_a(&self, name: &str) -> Vec<String> {
        self.zone.lock().await.get_a_records(name)
    }

    /// Query SRV records
    pub async fn query_srv(&self, name: &str) -> Vec<(String, u16)> {
        self.zone.lock().await.get_srv_records(name)
    }

    /// List all DNS records
    pub async fn list_records(&self) -> Vec<DnsRecord> {
        self.zone.lock().await.list_all()
    }

    /// Add a manual DNS record
    pub async fn add_manual_record(&self, record: DnsRecord) {
        self.zone.lock().await.add_record(record);
    }

    /// Remove a DNS record by name
    pub async fn remove_record(&self, name: &str) -> usize {
        self.zone.lock().await.remove_records(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_register_tunnel() {
        let registry = DnsRegistry::new(
            "10.0.0.1".into(),
            "tunnel.local".into(),
            "mesh.local".into(),
        );
        registry.register_tunnel("webapp", 9000, Some("http")).await;

        let a = registry.query_a("webapp.tunnel.local").await;
        assert_eq!(a, vec!["10.0.0.1"]);

        let srv = registry.query_srv("_http._tcp.webapp.tunnel.local").await;
        assert_eq!(srv.len(), 1);
        assert_eq!(srv[0].1, 9000);
    }

    #[tokio::test]
    async fn test_register_tunnel_default() {
        let registry = DnsRegistry::new(
            "10.0.0.1".into(),
            "tunnel.local".into(),
            "mesh.local".into(),
        );
        let name = registry.register_tunnel_default(8080, None).await;
        assert_eq!(name, "port-8080.tunnel.local");

        let a = registry.query_a("port-8080.tunnel.local").await;
        assert_eq!(a, vec!["10.0.0.1"]);
    }

    #[tokio::test]
    async fn test_unregister_tunnel() {
        let registry = DnsRegistry::new(
            "10.0.0.1".into(),
            "tunnel.local".into(),
            "mesh.local".into(),
        );
        registry.register_tunnel("webapp", 9000, None).await;
        assert_eq!(registry.query_a("webapp.tunnel.local").await.len(), 1);

        registry.unregister_tunnel("webapp", 9000).await;
        assert!(registry.query_a("webapp.tunnel.local").await.is_empty());
    }

    #[tokio::test]
    async fn test_register_mesh_service() {
        let registry = DnsRegistry::new(
            "10.0.0.1".into(),
            "tunnel.local".into(),
            "mesh.local".into(),
        );
        registry.register_mesh_service(
            "mynet", "client-a", "db", "mysql", "192.168.1.100", 3306,
        ).await;

        let a = registry.query_a("db.mynet.mesh.local").await;
        assert_eq!(a, vec!["192.168.1.100"]);
    }

    #[tokio::test]
    async fn test_unregister_mesh_client() {
        let registry = DnsRegistry::new(
            "10.0.0.1".into(),
            "tunnel.local".into(),
            "mesh.local".into(),
        );
        registry.register_mesh_service(
            "mynet", "client-a", "db", "mysql", "192.168.1.100", 3306,
        ).await;
        registry.register_mesh_service(
            "mynet", "client-a", "api", "http", "192.168.1.100", 8080,
        ).await;

        registry.unregister_mesh_client("mynet", "client-a").await;
        assert!(registry.query_a("db.mynet.mesh.local").await.is_empty());
        assert!(registry.query_a("api.mynet.mesh.local").await.is_empty());
    }
}
```

- [ ] **Step 2: 运行测试**

```bash
cargo test -p rust-tunnel -- server::dns::registry::tests
```

Expected: PASS

- [ ] **Step 3: Commit**

```bash
git add src/server/dns/registry.rs
git commit -m "feat: add DNS registry for tunnel and mesh service record management"
```

---

### Task 11: DNS 服务器入口（hickory-dns 集成）

**Files:**
- Create: `src/server/dns/mod.rs`
- Modify: `src/server/mod.rs`
- Modify: `src/server/config.rs`
- Modify: `src/bin/server.rs`

- [ ] **Step 1: 创建 `src/server/dns/mod.rs`**

```rust
pub mod registry;
pub mod zone;

use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use hickory_proto::op::{Message, MessageType, OpCode, ResponseCode};
use hickory_proto::rr::{Name, RData, Record, RecordType};
use hickory_proto::serialize::binary::{BinEncodable, BinEncoder};
use tracing::{debug, error, info};

use self::registry::DnsRegistry;
use crate::common::DnsRecord;

/// Lightweight authoritative DNS server for tunnel.local and mesh.local zones
pub struct DnsServer {
    registry: DnsRegistry,
    bind_addr: SocketAddr,
}

impl DnsServer {
    pub fn new(registry: DnsRegistry, bind_addr: &str) -> Result<Self, String> {
        let addr: SocketAddr = bind_addr.parse().map_err(|e| format!("Invalid DNS bind address: {}", e))?;
        Ok(Self {
            registry,
            bind_addr: addr,
        })
    }

    /// Start the DNS server, listening on UDP
    pub async fn run(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let socket = UdpSocket::bind(self.bind_addr).await?;
        info!("DNS server listening on {}", self.bind_addr);

        let mut buf = [0u8; 512];
        loop {
            let (len, src_addr) = match socket.recv_from(&mut buf).await {
                Ok(v) => v,
                Err(e) => {
                    error!("DNS recv error: {}", e);
                    continue;
                }
            };

            let request_data = buf[..len].to_vec();
            let registry = self.registry.clone();

            tokio::spawn(async move {
                let response = handle_dns_query(&registry, &request_data).await;
                if let Err(e) = socket.send_to(&response, src_addr).await {
                    error!("DNS send error: {}", e);
                }
            });
        }
    }
}

async fn handle_dns_query(registry: &DnsRegistry, data: &[u8]) -> Vec<u8> {
    let request = match Message::from_vec(data) {
        Ok(req) => req,
        Err(e) => {
            debug!("Failed to parse DNS query: {}", e);
            return build_error_response(data, ResponseCode::FormErr);
        }
    };

    // Only handle standard queries
    if request.message_type() != MessageType::Query
        || request.op_code() != OpCode::Query
    {
        return build_error_response(data, ResponseCode::NotImp);
    }

    let mut response = Message::new();
    response.set_id(request.id());
    response.set_message_type(MessageType::Response);
    response.set_op_code(OpCode::Query);
    response.set_authoritative(true);
    response.set_recursion_available(false);

    // If there are no questions, return FormErr
    if request.query_count() == 0 {
        response.set_response_code(ResponseCode::FormErr);
        let mut encoder = BinEncoder::new(&mut Vec::new());
        let _ = response.emit(&mut encoder);
        return encoder.into_bytes();
    }

    let mut response_code = ResponseCode::NoError;

    for question in request.queries() {
        let qname = question.name().to_string();
        let qname = qname.trim_end_matches('.').to_lowercase();

        debug!("DNS query: {} type={:?}", qname, question.query_type());

        match question.query_type() {
            RecordType::A => {
                let ips = registry.query_a(&qname).await;
                if ips.is_empty() {
                    response_code = ResponseCode::NXDomain;
                } else {
                    let mut name = question.name().clone();
                    name.set_fqdn(false);
                    for ip in &ips {
                        if let Ok(addr) = ip.parse::<std::net::Ipv4Addr>() {
                            let mut record = Record::new();
                            record.set_name(name.clone());
                            record.set_record_type(RecordType::A);
                            record.set_ttl(300);
                            record.set_rdata(RData::A(addr));
                            response.add_answer(record);
                        }
                    }
                }
            }
            RecordType::SRV => {
                let srvs = registry.query_srv(&qname).await;
                if srvs.is_empty() {
                    response_code = ResponseCode::NXDomain;
                } else {
                    for (target, port) in &srvs {
                        if let Ok(target_name) = Name::from_ascii(&format!("{}.", target)) {
                            let mut record = Record::new();
                            record.set_name(question.name().clone());
                            record.set_record_type(RecordType::SRV);
                            record.set_ttl(300);
                            record.set_rdata(RData::SRV(hickory_proto::rr::rdata::SRV::new(
                                0, 0, *port, target_name,
                            )));
                            response.add_answer(record);
                        }
                    }
                }
            }
            RecordType::TXT => {
                response_code = ResponseCode::NXDomain; // Simple TXT support, add later
            }
            _ => {
                response_code = ResponseCode::NotImp;
            }
        }
    }

    response.set_response_code(response_code);

    let mut encoder = BinEncoder::new(&mut Vec::new());
    if response.emit(&mut encoder).is_err() {
        return build_error_response(data, ResponseCode::ServFail);
    }

    let mut bytes = encoder.into_bytes();
    // Truncate to 512 bytes max for UDP DNS
    if bytes.len() > 512 {
        response.set_truncated(true);
        let mut encoder = BinEncoder::new(&mut Vec::new());
        let _ = response.emit(&mut encoder);
        bytes = encoder.into_bytes();
        bytes.truncate(512);
    }

    bytes
}

fn build_error_response(_request_data: &[u8], code: ResponseCode) -> Vec<u8> {
    let mut response = Message::new();
    response.set_message_type(MessageType::Response);
    response.set_response_code(code);

    let mut encoder = BinEncoder::new(&mut Vec::new());
    let _ = response.emit(&mut encoder);
    encoder.into_bytes()
}
```

- [ ] **Step 2: 更新 `src/server/mod.rs` 添加 dns 子模块**

```rust
pub mod api;
pub mod auth;
pub mod config;
pub mod control;
pub mod db;
pub mod dns;
pub mod listener;
pub mod logs;
pub mod mesh;
pub mod proxy;
pub mod quality;
pub mod shadowsocks;
pub mod trojan;

#[cfg(test)]
mod shadowsocks_test;

#[cfg(test)]
mod trojan_test;

pub use api::TrafficStore;
pub use config::*;
pub use db::Database;
pub use dns::DnsServer;
pub use dns::registry::DnsRegistry;
```

- [ ] **Step 3: 扩展 ServerConfig 添加 DNS 配置**

在 `src/server/config.rs` 的 `ServerCli` 中添加：

```rust
/// Enable embedded DNS server
#[clap(long = "dns-enabled")]
pub dns_enabled: Option<bool>,

/// DNS server bind address
#[clap(long = "dns-bind")]
pub dns_bind: Option<String>,

/// Tunnel domain suffix
#[clap(long = "dns-tunnel-domain")]
pub dns_tunnel_domain: Option<String>,

/// Mesh domain suffix
#[clap(long = "dns-mesh-domain")]
pub dns_mesh_domain: Option<String>,
```

在 `ServerConfigFile` 和 `ServerConfig` 中添加对应字段，并提供默认值：
```rust
pub dns_enabled: bool,           // default: true
pub dns_bind: String,            // default: "0.0.0.0:53"
pub dns_tunnel_domain: String,   // default: "tunnel.local"
pub dns_mesh_domain: String,     // default: "mesh.local"
```

- [ ] **Step 4: 更新 Default for ServerConfig**

在 `Default` 实现中添加：
```rust
dns_enabled: true,
dns_bind: "0.0.0.0:53".to_string(),
dns_tunnel_domain: "tunnel.local".to_string(),
dns_mesh_domain: "mesh.local".to_string(),
```

- [ ] **Step 5: 在 server.rs 中启动 DNS 服务器**

在 `src/bin/server.rs` 的启动逻辑中添加 DNS 服务器启动代码：

```rust
if config.dns_enabled {
    let dns_registry = DnsRegistry::new(
        "0.0.0.0".into(), // TODO: detect actual public IP
        config.dns_tunnel_domain.clone(),
        config.dns_mesh_domain.clone(),
    );

    let dns_server = DnsServer::new(dns_registry, &config.dns_bind)
        .expect("Failed to create DNS server");

    // Share registry with ServerState
    server_state.set_dns_registry(dns_registry);

    tokio::spawn(async move {
        if let Err(e) = dns_server.run().await {
            tracing::error!("DNS server error: {}", e);
        }
    });
}
```

- [ ] **Step 6: 运行编译检查并修复 hickory-dns API 问题**

```bash
cargo check 2>&1 | head -80
```

- [ ] **Step 7: Commit**

```bash
git add src/server/dns/mod.rs src/server/mod.rs src/server/config.rs src/bin/server.rs
git commit -m "feat: add embedded DNS server using hickory-dns for tunnel/mesh domains"
```

---

### Task 12: 扩展客户端配置（Mesh 相关）

**Files:**
- Modify: `src/client/config.rs`

- [ ] **Step 1: 添加客户端 Mesh 配置**

在 `src/client/config.rs` 的 `ClientCli` 中添加：

```rust
/// Mesh network ID to join
#[clap(long = "mesh")]
pub mesh: Option<String>,

/// Mesh client name (display name in mesh network)
#[clap(long = "mesh-name")]
pub mesh_name: Option<String>,

/// Mesh service definitions: NAME:PROTOCOL:LOCAL_ADDR
#[clap(long = "mesh-service", action = ArgAction::Append)]
pub mesh_services: Vec<String>,
```

在 `ClientConfigFile` 和 `ClientConfig` 中添加：

```rust
pub mesh: Option<String>,
pub mesh_name: Option<String>,
pub mesh_services: Vec<String>,
```

- [ ] **Step 2: 添加 ForwardRule 的 DNS 名称字段**

在 `ForwardRule` 结构体中添加：

```rust
pub dns_name: Option<String>,
```

- [ ] **Step 3: 更新 parse_forwards 支持 --dns-name**

扩展 CLI 参数支持 `--forward 9000:localhost:80 --dns-name webapp` 或类似格式。

- [ ] **Step 4: Commit**

```bash
git add src/client/config.rs
git commit -m "feat: add mesh and DNS config options to client"
```

---

### Task 13: 扩展服务器配置验证

**Files:**
- Modify: `src/server/config.rs`

- [ ] **Step 1: 添加 DNS 配置验证**

在 `ServerConfig::from_cli` 的验证阶段添加：

```rust
// Validate DNS configuration
if config.dns_enabled {
    // Ensure DNS bind is valid
    config.dns_bind.parse::<std::net::SocketAddr>()
        .map_err(|e| format!("Invalid dns_bind: {}", e))?;
}
```

- [ ] **Step 2: 运行测试**

```bash
cargo test -p rust-tunnel -- server::config::tests
```

Expected: PASS

- [ ] **Step 3: Commit**

```bash
git add src/server/config.rs
git commit -m "feat: add DNS config validation for server"
```

---

### Task 14: 数据库扩展（Mesh 表）

**Files:**
- Modify: `src/server/db.rs`

- [ ] **Step 1: 在 `initialize_schema` 中添加 Mesh 相关表**

在 `src/server/db.rs` 的 `initialize_schema` 函数中添加：

```rust
// Mesh networks table
sqlx::query(
    r#"
    CREATE TABLE IF NOT EXISTS mesh_networks (
        id TEXT PRIMARY KEY,
        created_at DATETIME NOT NULL,
        description TEXT
    )
    "#,
)
.execute(pool)
.await?;

// Mesh services table
sqlx::query(
    r#"
    CREATE TABLE IF NOT EXISTS mesh_services (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        mesh_id TEXT NOT NULL REFERENCES mesh_networks(id),
        client_name TEXT NOT NULL,
        service_name TEXT NOT NULL,
        protocol TEXT NOT NULL,
        local_addr TEXT NOT NULL,
        dns_record TEXT NOT NULL,
        UNIQUE(mesh_id, service_name)
    )
    "#,
)
.execute(pool)
.await?;

sqlx::query("CREATE INDEX IF NOT EXISTS idx_mesh_services_mesh ON mesh_services(mesh_id)")
    .execute(pool)
    .await?;
```

- [ ] **Step 2: 添加 Mesh 相关数据库操作方法**

```rust
/// Save a mesh network
pub async fn save_mesh_network(&self, id: &str, description: Option<&str>) -> Result<(), sqlx::Error> {
    let now = Utc::now();
    sqlx::query(
        r#"
        INSERT INTO mesh_networks (id, created_at, description)
        VALUES (?, ?, ?)
        ON CONFLICT(id) DO UPDATE SET description = excluded.description
        "#,
    )
    .bind(id)
    .bind(now)
    .bind(description)
    .execute(&self.pool)
    .await?;
    Ok(())
}

/// Load all mesh networks
pub async fn load_mesh_networks(&self) -> Result<Vec<MeshNetworkRecord>, sqlx::Error> {
    sqlx::query_as::<_, MeshNetworkRecord>(
        "SELECT id, created_at, description FROM mesh_networks ORDER BY id"
    )
    .fetch_all(&self.pool)
    .await
}

/// Save a mesh service
pub async fn save_mesh_service(
    &self,
    mesh_id: &str,
    client_name: &str,
    service_name: &str,
    protocol: &str,
    local_addr: &str,
    dns_record: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO mesh_services (mesh_id, client_name, service_name, protocol, local_addr, dns_record)
        VALUES (?, ?, ?, ?, ?, ?)
        ON CONFLICT(mesh_id, service_name) DO UPDATE SET
            client_name = excluded.client_name,
            protocol = excluded.protocol,
            local_addr = excluded.local_addr,
            dns_record = excluded.dns_record
        "#,
    )
    .bind(mesh_id)
    .bind(client_name)
    .bind(service_name)
    .bind(protocol)
    .bind(local_addr)
    .bind(dns_record)
    .execute(&self.pool)
    .await?;
    Ok(())
}

/// Load services for a mesh
pub async fn load_mesh_services(&self, mesh_id: &str) -> Result<Vec<MeshServiceRecord>, sqlx::Error> {
    sqlx::query_as::<_, MeshServiceRecord>(
        "SELECT id, mesh_id, client_name, service_name, protocol, local_addr, dns_record FROM mesh_services WHERE mesh_id = ? ORDER BY service_name"
    )
    .bind(mesh_id)
    .fetch_all(&self.pool)
    .await
}

/// Delete a mesh service
pub async fn delete_mesh_service(&self, mesh_id: &str, service_name: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM mesh_services WHERE mesh_id = ? AND service_name = ?")
        .bind(mesh_id)
        .bind(service_name)
        .execute(&self.pool)
        .await?;
    Ok(())
}
```

- [ ] **Step 3: 添加记录类型**

```rust
#[derive(FromRow, Debug)]
pub struct MeshNetworkRecord {
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub description: Option<String>,
}

#[derive(FromRow, Debug)]
pub struct MeshServiceRecord {
    pub id: i32,
    pub mesh_id: String,
    pub client_name: String,
    pub service_name: String,
    pub protocol: String,
    pub local_addr: String,
    pub dns_record: String,
}
```

- [ ] **Step 4: 运行测试**

```bash
cargo test -p rust-tunnel -- server::db::tests
```

Expected: PASS（新表不影响现有测试）

- [ ] **Step 5: Commit**

```bash
git add src/server/db.rs
git commit -m "feat: add mesh_networks and mesh_services database tables"
```

---

### Task 15: API 端点扩展（Mesh + DNS）

**Files:**
- Modify: `src/server/api.rs`

- [ ] **Step 1: 添加 Mesh 相关请求/响应类型**

在 `src/server/api.rs` 中添加：

```rust
/// Mesh network info response
#[derive(Debug, Serialize)]
pub struct MeshNetworkResponse {
    pub id: String,
    pub members: Vec<MeshMemberResponse>,
    pub services: Vec<MeshServiceResponse>,
}

#[derive(Debug, Serialize)]
pub struct MeshMemberResponse {
    pub client_name: String,
    pub public_addr: Option<String>,
    pub p2p_available: bool,
    pub online: bool,
}

#[derive(Debug, Serialize)]
pub struct MeshServiceResponse {
    pub service_name: String,
    pub protocol: String,
    pub local_addr: String,
    pub client_name: String,
}

/// DNS record response
#[derive(Debug, Serialize)]
pub struct DnsRecordResponse {
    pub name: String,
    pub record_type: String,
    pub value: String,
}

/// Request to add a manual DNS record
#[derive(Debug, Deserialize)]
pub struct AddDnsRecordRequest {
    pub name: String,
    pub record_type: String, // "A" or "SRV"
    pub value: String,
    pub port: Option<u16>,
}
```

- [ ] **Step 2: 添加 Mesh API handler**

```rust
// GET /api/mesh — list all meshes
async fn list_meshes(State(state): State<ApiState>) -> Json<Vec<MeshNetworkResponse>> {
    let networks = state.server_state.mesh_manager.list_networks().await;
    let response: Vec<MeshNetworkResponse> = networks
        .into_iter()
        .map(|(id, members)| {
            let services: Vec<MeshServiceResponse> = members
                .iter()
                .flat_map(|m| {
                    m.services.iter().map(|s| MeshServiceResponse {
                        service_name: s.name.clone(),
                        protocol: s.protocol.clone(),
                        local_addr: s.local_addr.clone(),
                        client_name: m.client_name.clone(),
                    })
                })
                .collect();

            MeshNetworkResponse {
                id,
                members: members.iter().map(|m| MeshMemberResponse {
                    client_name: m.client_name.clone(),
                    public_addr: m.public_addr.clone(),
                    p2p_available: m.p2p_available,
                    online: true,
                }).collect(),
                services,
            }
        })
        .collect();

    Json(response)
}

// GET /api/mesh/:id — mesh detail
async fn get_mesh(
    State(state): State<ApiState>,
    Path(mesh_id): Path<String>,
) -> impl IntoResponse {
    match state.server_state.mesh_manager.get_mesh(&mesh_id).await {
        Some(members) => {
            let services: Vec<MeshServiceResponse> = members
                .iter()
                .flat_map(|m| {
                    m.services.iter().map(|s| MeshServiceResponse {
                        service_name: s.name.clone(),
                        protocol: s.protocol.clone(),
                        local_addr: s.local_addr.clone(),
                        client_name: m.client_name.clone(),
                    })
                })
                .collect();

            Json(MeshNetworkResponse {
                id: mesh_id,
                members: members.iter().map(|m| MeshMemberResponse {
                    client_name: m.client_name.clone(),
                    public_addr: m.public_addr.clone(),
                    p2p_available: m.p2p_available,
                    online: true,
                }).collect(),
                services,
            })
            .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// GET /api/mesh/:id/services — mesh services
async fn get_mesh_services(
    State(state): State<ApiState>,
    Path(mesh_id): Path<String>,
) -> impl IntoResponse {
    match state.server_state.mesh_manager.get_mesh(&mesh_id).await {
        Some(members) => {
            let services: Vec<MeshServiceResponse> = members
                .iter()
                .flat_map(|m| {
                    m.services.iter().map(|s| MeshServiceResponse {
                        service_name: s.name.clone(),
                        protocol: s.protocol.clone(),
                        local_addr: s.local_addr.clone(),
                        client_name: m.client_name.clone(),
                    })
                })
                .collect();
            Json(services).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
```

- [ ] **Step 3: 添加 DNS API handler**

```rust
// GET /api/dns/records — list all DNS records
async fn get_dns_records(State(state): State<ApiState>) -> impl IntoResponse {
    let dns_registry = match &state.server_state.dns_registry {
        Some(r) => r,
        None => return (StatusCode::SERVICE_UNAVAILABLE, "DNS not enabled").into_response(),
    };

    let records = dns_registry.list_records().await;
    let response: Vec<DnsRecordResponse> = records
        .iter()
        .map(|r| DnsRecordResponse {
            name: r.name().to_string(),
            record_type: r.record_type().to_string(),
            value: match r {
                DnsRecord::TunnelA { target_ip, port, .. } =>
                    format!("{} (port {})", target_ip, port),
                DnsRecord::MeshA { target_ip, .. } => target_ip.clone(),
                DnsRecord::TunnelSrv { target, port, .. } =>
                    format!("{}:{}", target, port),
                DnsRecord::MeshSrv { target, port, .. } =>
                    format!("{}:{}", target, port),
                DnsRecord::Txt { text, .. } => text.clone(),
            },
        })
        .collect();

    Json(response).into_response()
}

// POST /api/dns/records — add manual DNS record
async fn add_dns_record(
    State(state): State<ApiState>,
    Json(body): Json<AddDnsRecordRequest>,
) -> impl IntoResponse {
    let dns_registry = match &state.server_state.dns_registry {
        Some(r) => r,
        None => return (StatusCode::SERVICE_UNAVAILABLE, "DNS not enabled").into_response(),
    };

    let record = match body.record_type.as_str() {
        "A" => DnsRecord::TunnelA {
            name: body.name.clone(),
            target_ip: body.value.clone(),
            port: body.port.unwrap_or(80),
        },
        _ => return (StatusCode::BAD_REQUEST, "Unsupported record type").into_response(),
    };

    dns_registry.add_manual_record(record).await;
    StatusCode::CREATED.into_response()
}

// DELETE /api/dns/records/:name — delete DNS record
async fn delete_dns_record(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let dns_registry = match &state.server_state.dns_registry {
        Some(r) => r,
        None => return (StatusCode::SERVICE_UNAVAILABLE, "DNS not enabled").into_response(),
    };

    dns_registry.remove_record(&name).await;
    StatusCode::OK.into_response()
}
```

- [ ] **Step 4: 注册路由**

在 `run_api_server` 中添加路由：

```rust
// Mesh endpoints
.route("/api/mesh", get(list_meshes))
.route("/api/mesh/:id", get(get_mesh))
.route("/api/mesh/:id/services", get(get_mesh_services))
// DNS endpoints
.route("/api/dns/records", get(get_dns_records).post(add_dns_record))
.route("/api/dns/records/:name", delete(delete_dns_record))
```

- [ ] **Step 5: 更新 ApiState**

```rust
pub struct ApiState {
    pub server_state: ServerState,
    pub auth_config: Arc<AuthConfig>,
    pub log_store: Option<crate::server::logs::LogStore>,
}
```

在 ServerState 中添加 `dns_registry: Option<DnsRegistry>` 字段。

- [ ] **Step 6: Commit**

```bash
git add src/server/api.rs src/server/control.rs
git commit -m "feat: add Mesh and DNS management API endpoints"
```

---

### Task 16: 前端类型扩展和 API 客户端更新

**Files:**
- Modify: `frontend/src/types/index.ts`
- Modify: `frontend/src/api/client.ts`

- [ ] **Step 1: 在 `frontend/src/types/index.ts` 中添加 Mesh 和 DNS 类型**

```typescript
// Mesh network types
export interface MeshMemberResponse {
  client_name: string;
  public_addr?: string;
  p2p_available: boolean;
  online: boolean;
}

export interface MeshServiceResponse {
  service_name: string;
  protocol: string;
  local_addr: string;
  client_name: string;
}

export interface MeshNetworkResponse {
  id: string;
  members: MeshMemberResponse[];
  services: MeshServiceResponse[];
}

// DNS types
export interface DnsRecordResponse {
  name: string;
  record_type: string;
  value: string;
}

export interface AddDnsRecordRequest {
  name: string;
  record_type: string;
  value: string;
  port?: number;
}
```

- [ ] **Step 2: 在 `frontend/src/api/client.ts` 中添加 API 调用**

```typescript
// Mesh API
export const getMeshes = async (): Promise<MeshNetworkResponse[]> => {
  const response = await api.get<MeshNetworkResponse[]>('/mesh');
  return response.data;
};

export const getMesh = async (id: string): Promise<MeshNetworkResponse> => {
  const response = await api.get<MeshNetworkResponse>(`/mesh/${id}`);
  return response.data;
};

export const getMeshServices = async (id: string): Promise<MeshServiceResponse[]> => {
  const response = await api.get<MeshServiceResponse[]>(`/mesh/${id}/services`);
  return response.data;
};

// DNS API
export const getDnsRecords = async (): Promise<DnsRecordResponse[]> => {
  const response = await api.get<DnsRecordResponse[]>('/dns/records');
  return response.data;
};

export const addDnsRecord = async (record: AddDnsRecordRequest): Promise<void> => {
  await api.post('/dns/records', record);
};

export const deleteDnsRecord = async (name: string): Promise<void> => {
  await api.delete(`/dns/records/${encodeURIComponent(name)}`);
};
```

- [ ] **Step 3: Commit**

```bash
git add frontend/src/types/index.ts frontend/src/api/client.ts
git commit -m "feat: add Mesh and DNS TypeScript types and API client methods"
```

---

### Task 17: 前端 Mesh 页面

**Files:**
- Create: `frontend/src/components/MeshPage.tsx`

- [ ] **Step 1: 创建 `frontend/src/components/MeshPage.tsx`**

```tsx
import { useState } from 'react';
import { useQuery } from 'react-query';
import { getMeshes, getMeshServices } from '../api/client';
import type { MeshNetworkResponse, MeshServiceResponse } from '../types';
import { StatCard } from './shared/StatCard';

export const MeshPage: React.FC = () => {
  const [selectedMesh, setSelectedMesh] = useState<string | null>(null);

  const { data: meshes, isLoading } = useQuery('meshes', getMeshes, {
    refetchInterval: 10000,
  });

  const { data: services } = useQuery(
    ['mesh-services', selectedMesh],
    () => selectedMesh ? getMeshServices(selectedMesh) : Promise.resolve([]),
    { enabled: !!selectedMesh }
  );

  if (isLoading) {
    return (
      <div className="flex items-center justify-center h-64">
        <div className="animate-spin rounded-full h-8 w-8 border-b-2 border-blue-600"></div>
      </div>
    );
  }

  if (!meshes || meshes.length === 0) {
    return (
      <div className="space-y-6">
        <h2 className="text-2xl font-bold text-gray-800">Mesh 网络</h2>
        <div className="bg-white rounded-lg shadow p-12 text-center text-gray-500">
          <p className="text-lg mb-2">暂无 Mesh 网络</p>
          <p className="text-sm">
            使用 --mesh 和 --mesh-service 参数启动客户端来创建 Mesh 网络
          </p>
        </div>
      </div>
    );
  }

  return (
    <div className="space-y-6">
      <h2 className="text-2xl font-bold text-gray-800">Mesh 网络</h2>

      {/* Mesh network cards */}
      <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-4">
        {meshes.map((mesh) => (
          <button
            key={mesh.id}
            onClick={() => setSelectedMesh(mesh.id)}
            className={`bg-white rounded-lg shadow p-6 text-left hover:shadow-md transition-shadow ${
              selectedMesh === mesh.id ? 'ring-2 ring-blue-500' : ''
            }`}
          >
            <h3 className="text-lg font-semibold text-gray-800 mb-2">
              {mesh.id}
            </h3>
            <div className="flex space-x-4 text-sm text-gray-600">
              <span>{mesh.members.length} 成员</span>
              <span>{mesh.services.length} 服务</span>
            </div>
          </button>
        ))}
      </div>

      {/* Selected mesh detail */}
      {selectedMesh && (
        <MeshDetail
          mesh={meshes.find((m) => m.id === selectedMesh)!}
          services={services || []}
        />
      )}
    </div>
  );
};

const MeshDetail: React.FC<{
  mesh: MeshNetworkResponse;
  services: MeshServiceResponse[];
}> = ({ mesh, services }) => {
  return (
    <div className="space-y-4">
      {/* Members */}
      <div className="bg-white rounded-lg shadow">
        <div className="px-6 py-4 border-b border-gray-200">
          <h3 className="text-lg font-semibold">成员 ({mesh.members.length})</h3>
        </div>
        <div className="p-6">
          <table className="min-w-full">
            <thead>
              <tr className="text-left text-sm text-gray-500">
                <th className="pb-3">客户端名称</th>
                <th className="pb-3">公网地址</th>
                <th className="pb-3">P2P</th>
              </tr>
            </thead>
            <tbody>
              {mesh.members.map((member) => (
                <tr key={member.client_name} className="border-t border-gray-100">
                  <td className="py-3">
                    <span className="font-medium">{member.client_name}</span>
                    {member.online && (
                      <span className="ml-2 inline-block w-2 h-2 bg-green-500 rounded-full"></span>
                    )}
                  </td>
                  <td className="py-3 text-gray-600">
                    {member.public_addr || '-'}
                  </td>
                  <td className="py-3">
                    {member.p2p_available ? (
                      <span className="text-green-600 text-sm">直连</span>
                    ) : (
                      <span className="text-yellow-600 text-sm">中转</span>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </div>

      {/* Services */}
      <div className="bg-white rounded-lg shadow">
        <div className="px-6 py-4 border-b border-gray-200">
          <h3 className="text-lg font-semibold">服务 ({services.length})</h3>
        </div>
        <div className="p-6">
          {services.length === 0 ? (
            <p className="text-gray-500 text-center py-4">暂无注册的服务</p>
          ) : (
            <table className="min-w-full">
              <thead>
                <tr className="text-left text-sm text-gray-500">
                  <th className="pb-3">服务名</th>
                  <th className="pb-3">协议</th>
                  <th className="pb-3">本地地址</th>
                  <th className="pb-3">所属客户端</th>
                </tr>
              </thead>
              <tbody>
                {services.map((svc) => (
                  <tr key={svc.service_name} className="border-t border-gray-100">
                    <td className="py-3 font-medium">{svc.service_name}</td>
                    <td className="py-3 text-gray-600">{svc.protocol}</td>
                    <td className="py-3 text-gray-600">{svc.local_addr}</td>
                    <td className="py-3 text-gray-600">{svc.client_name}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>
      </div>
    </div>
  );
};
```

- [ ] **Step 2: Commit**

```bash
git add frontend/src/components/MeshPage.tsx
git commit -m "feat: add Mesh network management page to frontend"
```

---

### Task 18: 前端 DNS 管理页面

**Files:**
- Create: `frontend/src/components/DnsPage.tsx`

- [ ] **Step 1: 创建 `frontend/src/components/DnsPage.tsx`**

```tsx
import { useState } from 'react';
import { useQuery, useMutation, useQueryClient } from 'react-query';
import { getDnsRecords, addDnsRecord, deleteDnsRecord } from '../api/client';
import type { DnsRecordResponse } from '../types';

export const DnsPage: React.FC = () => {
  const [showAddForm, setShowAddForm] = useState(false);
  const [newName, setNewName] = useState('');
  const [newValue, setNewValue] = useState('');
  const [newPort, setNewPort] = useState(80);
  const queryClient = useQueryClient();

  const { data: records, isLoading } = useQuery('dns-records', getDnsRecords, {
    refetchInterval: 15000,
  });

  const addMutation = useMutation(
    (data: { name: string; record_type: string; value: string; port?: number }) =>
      addDnsRecord(data),
    {
      onSuccess: () => {
        queryClient.invalidateQueries('dns-records');
        setShowAddForm(false);
        setNewName('');
        setNewValue('');
        setNewPort(80);
      },
    }
  );

  const deleteMutation = useMutation(
    (name: string) => deleteDnsRecord(name),
    {
      onSuccess: () => queryClient.invalidateQueries('dns-records'),
    }
  );

  if (isLoading) {
    return (
      <div className="flex items-center justify-center h-64">
        <div className="animate-spin rounded-full h-8 w-8 border-b-2 border-blue-600"></div>
      </div>
    );
  }

  return (
    <div className="space-y-6">
      <div className="flex justify-between items-center">
        <h2 className="text-2xl font-bold text-gray-800">DNS 记录</h2>
        <button
          onClick={() => setShowAddForm(!showAddForm)}
          className="px-4 py-2 bg-blue-600 text-white rounded-lg hover:bg-blue-700 transition-colors"
        >
          {showAddForm ? '取消' : '添加记录'}
        </button>
      </div>

      {/* Add Record Form */}
      {showAddForm && (
        <div className="bg-white rounded-lg shadow p-6">
          <h3 className="text-lg font-semibold mb-4">添加 DNS 记录</h3>
          <div className="space-y-4">
            <div>
              <label className="block text-sm text-gray-600 mb-1">域名</label>
              <input
                type="text"
                value={newName}
                onChange={(e) => setNewName(e.target.value)}
                placeholder="e.g. myapp.tunnel.local"
                className="w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-blue-500 focus:border-transparent"
              />
            </div>
            <div>
              <label className="block text-sm text-gray-600 mb-1">IP 地址</label>
              <input
                type="text"
                value={newValue}
                onChange={(e) => setNewValue(e.target.value)}
                placeholder="e.g. 10.0.0.1"
                className="w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-blue-500 focus:border-transparent"
              />
            </div>
            <div>
              <label className="block text-sm text-gray-600 mb-1">端口</label>
              <input
                type="number"
                value={newPort}
                onChange={(e) => setNewPort(Number(e.target.value))}
                className="w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-blue-500 focus:border-transparent"
              />
            </div>
            <button
              onClick={() =>
                addMutation.mutate({
                  name: newName,
                  record_type: 'A',
                  value: newValue,
                  port: newPort,
                })
              }
              disabled={!newName || !newValue || addMutation.isLoading}
              className="px-4 py-2 bg-green-600 text-white rounded-lg hover:bg-green-700 disabled:opacity-50 disabled:cursor-not-allowed"
            >
              {addMutation.isLoading ? '添加中...' : '添加'}
            </button>
          </div>
        </div>
      )}

      {/* Records Table */}
      <div className="bg-white rounded-lg shadow overflow-hidden">
        <table className="min-w-full">
          <thead className="bg-gray-50">
            <tr>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">
                域名
              </th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">
                类型
              </th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">
                值
              </th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">
                操作
              </th>
            </tr>
          </thead>
          <tbody className="divide-y divide-gray-200">
            {(!records || records.length === 0) ? (
              <tr>
                <td colSpan={4} className="px-6 py-12 text-center text-gray-500">
                  暂无 DNS 记录
                </td>
              </tr>
            ) : (
              records.map((record, idx) => (
                <tr key={idx}>
                  <td className="px-6 py-4 text-sm font-medium text-gray-800">
                    {record.name}
                  </td>
                  <td className="px-6 py-4 text-sm">
                    <span className="px-2 py-1 bg-gray-100 rounded text-gray-600">
                      {record.record_type}
                    </span>
                  </td>
                  <td className="px-6 py-4 text-sm text-gray-600">
                    {record.value}
                  </td>
                  <td className="px-6 py-4 text-sm">
                    <button
                      onClick={() => {
                        if (confirm(`确定要删除 ${record.name} 吗？`)) {
                          deleteMutation.mutate(record.name);
                        }
                      }}
                      className="text-red-600 hover:text-red-800"
                    >
                      删除
                    </button>
                  </td>
                </tr>
              ))
            )}
          </tbody>
        </table>
      </div>
    </div>
  );
};
```

- [ ] **Step 2: Commit**

```bash
git add frontend/src/components/DnsPage.tsx
git commit -m "feat: add DNS records management page to frontend"
```

---

### Task 19: 前端集成（App.tsx + Dashboard + Navbar）

**Files:**
- Modify: `frontend/src/components/Navbar.tsx`
- Modify: `frontend/src/components/Dashboard.tsx`
- Modify: `frontend/src/App.tsx`

- [ ] **Step 1: 更新 Navbar 添加 Mesh 和 DNS 导航**

在 `Navbar.tsx` 的导航链接中添加：

```tsx
{/* Mesh link */}
<button
  onClick={() => onNavigate('mesh')}
  className={`px-3 py-2 rounded-md text-sm font-medium ${
    currentPage === 'mesh'
      ? 'bg-blue-100 text-blue-700'
      : 'text-gray-600 hover:text-gray-900'
  }`}
>
  Mesh
</button>

{/* DNS link */}
<button
  onClick={() => onNavigate('dns')}
  className={`px-3 py-2 rounded-md text-sm font-medium ${
    currentPage === 'dns'
      ? 'bg-blue-100 text-blue-700'
      : 'text-gray-600 hover:text-gray-900'
  }`}
>
  DNS
</button>
```

- [ ] **Step 2: 更新 Dashboard 集成 MeshPage 和 DnsPage**

在 `Dashboard.tsx` 中添加页面状态和路由：

```tsx
import { MeshPage } from './MeshPage';
import { DnsPage } from './DnsPage';

// Add state
const [currentPage, setCurrentPage] = useState<'dashboard' | 'clients' | 'traffic' | 'quality' | 'shadowsocks' | 'trojan' | 'logs' | 'mesh' | 'dns'>('dashboard');

// Add render cases
case 'mesh':
  return <MeshPage />;
case 'dns':
  return <DnsPage />;
```

- [ ] **Step 3: 更新 Navbar 的 onNavigate prop 类型**

确保 `currentPage` 和 `onNavigate` 的类型包含 `'mesh'` 和 `'dns'`。

- [ ] **Step 4: Commit**

```bash
git add frontend/src/components/Navbar.tsx frontend/src/components/Dashboard.tsx frontend/src/App.tsx
git commit -m "feat: integrate Mesh and DNS pages into frontend navigation"
```

---

### Task 20: 前端构建验证

- [ ] **Step 1: 执行 TypeScript 类型检查**

```bash
cd frontend && npx tsc --noEmit
```

Expected: PASS（无类型错误）

- [ ] **Step 2: 执行前端构建**

```bash
cd frontend && npm run build
```

Expected: PASS（构建成功）

- [ ] **Step 3: 运行完整的后端编译检查**

```bash
cargo check
```

Expected: PASS

- [ ] **Step 4: 运行所有现有测试**

```bash
cargo test
```

Expected: PASS（所有现有测试继续通过）

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "chore: verify full build and test suite passes with mesh+dns changes"
```

---

## Summary

Total: 20 tasks covering:
- **Protocol layer** (Tasks 1-3): ControlMessage extensions, shared types, STUN
- **Server mesh** (Tasks 4-8): Router, relay, STUN server, manager, message handling
- **DNS** (Tasks 9-11): Zone management, registry, hickory-dns server
- **Config** (Tasks 12-13): Client and server config extensions
- **Database** (Task 14): Mesh networks and services tables
- **API** (Task 15): Mesh and DNS REST endpoints
- **Frontend** (Tasks 16-19): Types, API client, Mesh page, DNS page, integration
- **Verification** (Task 20): Full build and test verification
