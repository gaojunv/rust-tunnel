//! 统一出站 HTTP 客户端工厂。
//!
//! 所有 reqwest 构造点共享一套默认值：UA / connect timeout / TCP keepalive /
//! 连接池上限，慢端点在 [`crate::http_client::builder()`] 起点上覆盖整体超时。panic 语义单点收敛
//! 在 [`crate::http_client::build()`]：reqwest 构建失败仅当 TLS 后端初始化失败（构造期 fatal，
//! 无法恢复），各调用点不再各自 `expect`/`#[expect]`。

use std::time::Duration;

/// 统一连接超时（TCP + TLS 握手）。
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// 默认档整体请求超时；慢端点（embedding 等）在 builder 上覆盖。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// TCP keepalive 间隔。
pub const TCP_KEEPALIVE: Duration = Duration::from_mins(1);
/// 每 host 连接池空闲连接上限。
pub const POOL_MAX_IDLE_PER_HOST: usize = 32;
/// 统一 User-Agent（workspace 版本号）。
pub const USER_AGENT: &str = concat!("rust-tunnel/", env!("CARGO_PKG_VERSION"));

/// 带统一默认的 builder 起点：UA + connect timeout + keepalive + 连接池。
/// 调用方按需继续覆盖（整体超时 / http1_only / read_timeout 等）。
pub fn builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(CONNECT_TIMEOUT)
        .tcp_keepalive(TCP_KEEPALIVE)
        .pool_max_idle_per_host(POOL_MAX_IDLE_PER_HOST)
}

/// 默认档客户端（[`DEFAULT_TIMEOUT`] 整体超时）。
#[must_use]
pub fn default_client() -> reqwest::Client {
    build(builder().timeout(DEFAULT_TIMEOUT))
}

/// 构建客户端。TLS 后端初始化失败属构造期 fatal（出站 HTTP 全不可用），
/// panic 语义统一收敛在此，调用点不再各自豁免。
///
/// # Panics
/// reqwest builder 构建失败（TLS 后端不可用）时 panic。
#[must_use]
pub fn build(client_builder: reqwest::ClientBuilder) -> reqwest::Client {
    #[expect(clippy::expect_used)]
    client_builder
        .build()
        .expect("failed to build HTTP client (TLS backend init)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_client_builds() {
        let _ = default_client();
    }

    #[test]
    fn builder_accepts_overrides() {
        let _ = build(builder().timeout(Duration::from_secs(5)).http1_only());
    }
}
