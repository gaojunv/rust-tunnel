//! 管理面模块 — 鉴权、动态配置、日志与 API 路由装配。

/// 管理面 HTTP API 路由与状态。
pub mod api;
/// JWT 鉴权与中间件。
pub mod auth;
/// 动态配置（数据库驱动的运行时配置）。
pub mod dynamic_config;
/// 日志收集与查询。
pub mod logs;
/// 统计与流量聚合（re-export 自独立 crate）。
pub use rust_tunnel_stats as stats;
