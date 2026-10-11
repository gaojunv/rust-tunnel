use crate::control_plane::ControlMessageSender;

/// 代理端口类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortType {
    /// Shadowsocks 代理端口。
    Shadowsocks,
    /// Trojan 代理端口。
    Trojan,
}

/// 已连接客户端信息（控制通道侧视图）。
#[derive(Debug, Clone)]
pub struct ClientInfo {
    /// 客户端希望暴露的远端端口。
    pub remote_port: u16,
    /// 客户端机器 hostname（可选）。
    pub hostname: Option<String>,
    /// 经控制通道向该客户端发送消息的发送端。
    pub control_sender: ControlMessageSender,
}

/// 服务端端口注册表项（Shadowsocks 或 Trojan）。
#[derive(Debug, Clone)]
pub enum PortInfo {
    /// Shadowsocks 端口配置。
    Shadowsocks {
        /// 监听端口。
        port: u16,
        /// 加密算法。
        cipher: String,
        /// 密码。
        password: String,
        /// 是否启用。
        enabled: bool,
        /// 创建时间戳（秒）。
        created_at: i64,
    },
    /// Trojan 端口配置。
    Trojan {
        /// 监听端口。
        port: u16,
        /// 认证密码。
        password: String,
        /// 认证失败回退地址。
        fallback: String,
        /// 是否启用。
        enabled: bool,
        /// 创建时间戳（秒）。
        created_at: i64,
    },
}

impl PortInfo {
    /// 返回端口类型。
    #[must_use]
    pub fn port_type(&self) -> PortType {
        match self {
            PortInfo::Shadowsocks { .. } => PortType::Shadowsocks,
            PortInfo::Trojan { .. } => PortType::Trojan,
        }
    }

    /// 返回监听端口号。
    #[must_use]
    pub fn port(&self) -> u16 {
        match self {
            PortInfo::Shadowsocks { port, .. } | PortInfo::Trojan { port, .. } => *port,
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

/// RustDesk 运行时状态：会合/中继监听情况。
/// 由 `rustdesk_runtime` 的启动/停止路径写入，`GET /api/rustdesk` 读取。
#[derive(Debug, Clone, Default)]
pub struct RustdeskRuntimeStatus {
    /// 会合(hbbs)与中继(hbbr)监听是否都已就绪。
    pub listening: bool,
    /// 实际生效的会合端口；未运行时为 None。
    pub rendezvous_port: Option<u16>,
    /// 实际生效的中继端口；未运行时为 None。
    pub relay_port: Option<u16>,
    /// 最近一次启动/绑定失败的原因；管理 API 用它向用户暴露 bind 失败。
    pub last_error: Option<String>,
    /// 在线对端数（`REG_TIMEOUT` 内有心跳的 ID 数），由统计轮询任务刷新。
    pub online_peers: usize,
    /// 登记表里的 ID 总数（含已离线），由统计轮询任务刷新。
    pub known_peers: usize,
    /// 当前活跃中继配对数（`RelayStats::active_pairs`），由统计轮询任务刷新。
    pub relay_pairs: usize,
    /// 累计已建立的中继对数（`RelayStats` 单调递增计数），由统计轮询任务刷新。
    pub relay_total_pairs: u64,
    /// 累计拒绝数（会合 + 中继），由统计轮询任务刷新。
    pub rejected_total: u64,
}
