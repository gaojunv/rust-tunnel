use serde::{Deserialize, Serialize};

/// 表示 mesh 路由表中的一条路由。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshRoute {
    /// 目标客户端名称。
    pub client_name: String,
    /// 已发现的公网地址（ip:port），通过 STUN 探测得到，未发现时为 None。
    pub public_addr: Option<String>,
    /// 是否可建立 P2P 直连。
    pub p2p_available: bool,
    /// 该客户端上注册的服务列表。
    pub services: Vec<MeshService>,
}

/// mesh 客户端注册的单个服务。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshService {
    /// 服务名称。
    pub name: String,
    /// 服务协议（如 tcp/http）。
    pub protocol: String,
    /// 本地监听地址（如 127.0.0.1:3306）。
    pub local_addr: String,
}

/// 注册表中的 DNS 记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DnsRecord {
    /// 隧道端口对应的 A 记录（指向服务器 IP）。
    TunnelA {
        /// 记录名。
        name: String,
        /// 目标 IP。
        target_ip: String,
        /// 隧道端口。
        port: u16,
    },
    /// 隧道服务的 SRV 记录。
    TunnelSrv {
        /// 记录名。
        name: String,
        /// 目标主机名。
        target: String,
        /// 目标端口。
        port: u16,
    },
    /// mesh 服务对应的 A 记录（指向 mesh 客户端 IP）。
    MeshA {
        /// 记录名。
        name: String,
        /// 目标 IP。
        target_ip: String,
    },
    /// mesh 服务的 SRV 记录。
    MeshSrv {
        /// 记录名。
        name: String,
        /// 目标主机名。
        target: String,
        /// 目标端口。
        port: u16,
    },
    /// TXT 元数据记录。
    Txt {
        /// 记录名。
        name: String,
        /// 文本内容。
        text: String,
    },
}

impl DnsRecord {
    /// 返回记录名。
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

    /// 返回记录类型（A / SRV / TXT）。
    #[must_use]
    pub fn record_type(&self) -> &'static str {
        match self {
            DnsRecord::TunnelA { .. } | DnsRecord::MeshA { .. } => "A",
            DnsRecord::TunnelSrv { .. } | DnsRecord::MeshSrv { .. } => "SRV",
            DnsRecord::Txt { .. } => "TXT",
        }
    }
}

/// 隧道转发规则的 DNS 配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelDnsConfig {
    /// 自定义 DNS 名称（可选，如 "webapp" -> webapp.tunnel.local）。
    pub dns_name: Option<String>,
    /// SRV 记录所用协议（如 "http"、"mysql"）。
    pub protocol: Option<String>,
}
