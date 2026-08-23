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
        name: String,
        target_ip: String,
        port: u16,
    },
    /// SRV record for tunnel service
    TunnelSrv {
        name: String,
        target: String,
        port: u16,
    },
    /// A record for mesh service -> mesh client IP
    MeshA { name: String, target_ip: String },
    /// SRV record for mesh service
    MeshSrv {
        name: String,
        target: String,
        port: u16,
    },
    /// TXT metadata record
    Txt { name: String, text: String },
}

impl DnsRecord {
    #[must_use] 
    pub fn name(&self) -> &str {
        match self {
            DnsRecord::TunnelA { name, .. }
            | DnsRecord::TunnelSrv { name, .. }
            | DnsRecord::MeshA { name, .. }
            | DnsRecord::MeshSrv { name, .. }
            | DnsRecord::Txt { name, .. } => name,
        }
    }

    #[must_use] 
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
