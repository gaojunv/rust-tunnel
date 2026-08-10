use crate::control::ControlMessageSender;

/// Sender for control messages - can be shared across tasks
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortType {
    Shadowsocks,
    Trojan,
}

/// Information about a connected client
#[derive(Debug, Clone)]
pub struct ClientInfo {
    /// Remote port that client wants to expose
    pub remote_port: u16,
    /// Hostname of the client machine (optional)
    pub hostname: Option<String>,
    /// Sender for sending messages to client via control channel
    pub control_sender: ControlMessageSender,
}

#[derive(Debug, Clone)]
pub enum PortInfo {
    Shadowsocks {
        port: u16,
        cipher: String,
        password: String,
        enabled: bool,
        created_at: i64,
    },
    Trojan {
        port: u16,
        password: String,
        fallback: String,
        enabled: bool,
        created_at: i64,
    },
}

impl PortInfo {
    pub fn port_type(&self) -> PortType {
        match self {
            PortInfo::Shadowsocks { .. } => PortType::Shadowsocks,
            PortInfo::Trojan { .. } => PortType::Trojan,
        }
    }

    pub fn port(&self) -> u16 {
        match self {
            PortInfo::Shadowsocks { port, .. } => *port,
            PortInfo::Trojan { port, .. } => *port,
        }
    }
}

/// Trojan 运行时状态：证书来源与监听模式。
/// 由 `trojan_runtime::apply_trojan_config` / API 更新路径写入，`GET /api/trojan` 读取。
#[derive(Debug, Clone, Default)]
pub struct TrojanRuntimeStatus {
    /// "acme_exact" | "acme_wildcard" | "self_signed"；未运行时为 None
    pub cert_source: Option<String>,
    /// true = 与反代共享端口（SNI 分流）；false = 独立监听
    pub shared: bool,
}
