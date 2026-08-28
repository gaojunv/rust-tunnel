//! 客户端状态快照，供托盘/GUI 展示。

use std::time::SystemTime;

use rust_tunnel_common::protocol::MappingSummary;

/// 客户端运行状态快照（GUI/托盘消费）。
#[derive(Debug, Clone)]
pub struct ClientStatus {
    /// 是否已连接到服务端。
    pub connected: bool,
    /// 服务端地址。
    pub server: String,
    /// 客户端名称。
    pub client_name: String,
    /// 客户端版本。
    pub version: String,
    /// 连接建立时间。
    pub connected_at: Option<SystemTime>,
    /// 活跃隧道数。
    pub active_tunnels: usize,
    /// 等待拨号的隧道数。
    pub pending_tunnels: usize,
    /// 最近隧道记录：(connection_id, target_addr, time)，cap 20。
    pub recent_tunnels: Vec<(u64, String, SystemTime)>,
    /// 最近一次错误信息。
    pub last_error: Option<String>,
    /// 映射摘要（服务端下发的规则/路由/后端概览）。
    pub mapping_summary: Option<MappingSummary>,
    /// 最近一次 RTT（毫秒）。
    pub rtt_ms: Option<f64>,
}

impl ClientStatus {
    /// 创建一个新的 `ClientStatus`。
    #[must_use]
    pub fn new(server: String, client_name: String, version: String) -> Self {
        Self {
            connected: false,
            server,
            client_name,
            version,
            connected_at: None,
            active_tunnels: 0,
            pending_tunnels: 0,
            recent_tunnels: Vec::new(),
            last_error: None,
            mapping_summary: None,
            rtt_ms: None,
        }
    }
}

impl Default for ClientStatus {
    fn default() -> Self {
        Self::new(String::new(), String::new(), String::new())
    }
}
