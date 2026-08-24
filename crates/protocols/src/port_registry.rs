//! 端口注册与连接计数的依赖倒置 trait。
//!
//! 原 `ServerState` 中的端口管理方法收敛为 [`PortRegistry`]，protocols crate
//! 通过该 trait 操作端口，不再直接依赖 `ServerState`。

use std::fmt::Debug;

/// 端口类型标识。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortType {
    /// Shadowsocks 代理端口。
    Shadowsocks,
    /// Trojan 代理端口。
    Trojan,
}

/// 单个代理端口的完整信息（注册时写入，查询时返回）。
#[derive(Debug, Clone)]
pub enum PortInfo {
    /// Shadowsocks 端口配置。
    Shadowsocks {
        /// 监听端口。
        port: u16,
        /// 加密方法（如 `aes-256-gcm`）。
        cipher: String,
        /// 连接密码。
        password: String,
        /// 是否启用。
        enabled: bool,
        /// 创建时间戳（Unix epoch 秒）。
        created_at: i64,
    },
    /// Trojan 端口配置。
    Trojan {
        /// 监听端口。
        port: u16,
        /// 连接密码。
        password: String,
        /// 回退地址（认证失败时转发到此）。
        fallback: String,
        /// 是否启用。
        enabled: bool,
        /// 创建时间戳（Unix epoch 秒）。
        created_at: i64,
    },
}

impl PortInfo {
    /// 返回端口类型。
    #[must_use]
    pub fn port_type(&self) -> PortType {
        match self {
            Self::Shadowsocks { .. } => PortType::Shadowsocks,
            Self::Trojan { .. } => PortType::Trojan,
        }
    }

    /// 返回监听端口号。
    #[must_use]
    pub fn port(&self) -> u16 {
        match self {
            Self::Shadowsocks { port, .. } | Self::Trojan { port, .. } => *port,
        }
    }
}

/// 端口注册与连接计数 trait。
///
/// server 侧 `ServerState` 实现此 trait；protocols crate 的监听器/代理逻辑
/// 通过 `Arc<dyn PortRegistry>` 操作端口状态。
#[async_trait::async_trait]
pub trait PortRegistry: Send + Sync + Debug {
    /// 注册 Shadowsocks 端口。已存在时返回 `false`。
    async fn register_shadowsocks(&self, port: u16, cipher: String, password: String) -> bool;
    /// 注册 Trojan 端口。已存在时返回 `false`。
    async fn register_trojan(&self, port: u16, password: String, fallback: String) -> bool;
    /// 查询端口信息。
    async fn get_port(&self, port: u16) -> Option<PortInfo>;
    /// 注销端口。不存在时返回 `false`。
    async fn unregister_port(&self, port: u16) -> bool;
    /// 查询指定端口的活跃连接数（SS + Trojan 合计）。
    async fn get_connection_count_for_port(&self, remote_port: u16) -> usize;
    /// 增加 Shadowsocks 连接计数。
    async fn increment_ss_connections(&self, port: u16);
    /// 减少 Shadowsocks 连接计数。
    async fn decrement_ss_connections(&self, port: u16);
    /// 增加 Trojan 连接计数。
    async fn increment_trojan_connections(&self, port: u16);
    /// 减少 Trojan 连接计数。
    async fn decrement_trojan_connections(&self, port: u16);
}

/// 内存版 PortRegistry，用于 protocols crate 内测试与 TrojanSniEntry 占位。
#[derive(Debug, Default)]
pub struct MockPortRegistry {
    ports: std::sync::Mutex<std::collections::HashMap<u16, PortInfo>>,
    ss_conns: std::sync::Mutex<std::collections::HashMap<u16, usize>>,
    trojan_conns: std::sync::Mutex<std::collections::HashMap<u16, usize>>,
}

impl MockPortRegistry {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl PortRegistry for MockPortRegistry {
    async fn register_shadowsocks(&self, port: u16, cipher: String, password: String) -> bool {
        let mut ports = self.ports.lock().unwrap();
        if ports.contains_key(&port) {
            return false;
        }
        ports.insert(
            port,
            PortInfo::Shadowsocks {
                port,
                cipher,
                password,
                enabled: true,
                created_at: chrono::Utc::now().timestamp(),
            },
        );
        true
    }
    async fn register_trojan(&self, port: u16, password: String, fallback: String) -> bool {
        let mut ports = self.ports.lock().unwrap();
        if ports.contains_key(&port) {
            return false;
        }
        ports.insert(
            port,
            PortInfo::Trojan {
                port,
                password,
                fallback,
                enabled: true,
                created_at: chrono::Utc::now().timestamp(),
            },
        );
        true
    }
    async fn get_port(&self, port: u16) -> Option<PortInfo> {
        self.ports.lock().unwrap().get(&port).cloned()
    }
    async fn unregister_port(&self, port: u16) -> bool {
        self.ports.lock().unwrap().remove(&port).is_some()
    }
    async fn get_connection_count_for_port(&self, remote_port: u16) -> usize {
        let ss = self
            .ss_conns
            .lock()
            .unwrap()
            .get(&remote_port)
            .copied()
            .unwrap_or(0);
        let tj = self
            .trojan_conns
            .lock()
            .unwrap()
            .get(&remote_port)
            .copied()
            .unwrap_or(0);
        ss + tj
    }
    async fn increment_ss_connections(&self, port: u16) {
        *self.ss_conns.lock().unwrap().entry(port).or_insert(0) += 1;
    }
    async fn decrement_ss_connections(&self, port: u16) {
        if let Some(c) = self.ss_conns.lock().unwrap().get_mut(&port) {
            if *c > 0 {
                *c -= 1;
            }
        }
    }
    async fn increment_trojan_connections(&self, port: u16) {
        *self.trojan_conns.lock().unwrap().entry(port).or_insert(0) += 1;
    }
    async fn decrement_trojan_connections(&self, port: u16) {
        if let Some(c) = self.trojan_conns.lock().unwrap().get_mut(&port) {
            if *c > 0 {
                *c -= 1;
            }
        }
    }
}
