//! 隧道打开 trait——消除 protocols → server(ClientRegistry) 的直接依赖。
//!
//! 原 `ClientConnector` 直接持有 `ClientRegistry` 调用 `open_tunnel`，导致
//! protocols 依赖 control_plane。改为持有 `Arc<dyn TunnelOpener>`，由 server
//! 装配层注入实现。

use crate::reverse_proxy::connector::BoxedStream;
use std::io;

/// 通过控制通道向内网客户端打开一条隧道流。
#[async_trait::async_trait]
pub trait TunnelOpener: Send + Sync {
    /// 打开到指定客户端上目标地址的隧道流。
    async fn open_tunnel(&self, client_name: &str, target_addr: &str) -> io::Result<BoxedStream>;
}
