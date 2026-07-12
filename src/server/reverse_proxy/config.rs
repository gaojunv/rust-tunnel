use serde::{Deserialize, Serialize};

/// Reverse proxy configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReverseProxyConfig {
    /// Enable reverse proxy service
    #[serde(default)]
    pub enabled: bool,

    /// Maximum concurrent connections
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,

    /// Connection timeout in seconds
    #[serde(default = "default_connection_timeout")]
    pub connection_timeout: u64,

    /// Buffer size for proxy operations
    #[serde(default = "default_buffer_size")]
    pub buffer_size: usize,
}

impl Default for ReverseProxyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_connections: default_max_connections(),
            connection_timeout: default_connection_timeout(),
            buffer_size: default_buffer_size(),
        }
    }
}

fn default_max_connections() -> u32 {
    10000
}

fn default_connection_timeout() -> u64 {
    30
}

fn default_buffer_size() -> usize {
    8192
}
