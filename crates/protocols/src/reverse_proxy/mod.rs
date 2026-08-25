//! 反向代理核心：规则、路由、共享监听与协议转发。

/// 规则持久化与校验配置。
pub mod config;
/// 后端连接器抽象（直连/隧道）。
pub mod connector;
/// 共享监听器调和错误类型。
pub mod error;
/// HTTP 反代请求处理。
pub mod handler;
/// LLM Gateway 分流器 trait。
pub mod llm_dispatch;
/// 路由表：Host/Path → 后端选择。
pub mod router;
/// 代理规则与后端模型定义。
pub mod rules;
/// 共享监听器：多规则共用端口与热更新。
pub mod shared_listener;
/// SNI 解析器（基于 CertificateManager）。
pub mod sni_resolver;
/// ClientHello SNI 嗅探。
pub mod sni_sniff;
/// 反代状态容器与调和逻辑。
pub mod state;
/// TCP 反代转发。
pub mod tcp_proxy;
/// 上游 HTTP 客户端（四组合连接池）。
pub mod upstream;

pub use rules::*;
pub use state::*;
