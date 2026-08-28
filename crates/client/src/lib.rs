//! rust-tunnel 客户端库：控制通道、隧道代理与 agent 执行

// 测试代码豁免 panic 风险 lint（生产代码仍告警）
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

/// Agent 执行模块：沙箱路径解析与命令执行
pub mod agent;
/// 客户端配置解析（三级优先级：配置文件、环境变量、命令行）
pub mod config;
/// 控制通道：注册、心跳、消息分发与连接管理
pub mod control;
/// LLM 回环代理：本地 HTTP 转控制通道转发
pub mod llm_proxy;
/// 客户端日志采集与批量上报
pub mod logs;
/// 隧道代理：本地目标拨号与双向转发
pub mod proxy;
/// PTY 服务：回环端口上的交互式 shell
pub mod pty;
/// 长生命周期进程管理：spawn/stdin/stdout 桥接
pub mod spawn;

/// 本地环形日志缓冲（托盘/GUI 拉取最近日志）
pub mod log_buffer;
/// 重连策略（指数退避与可重连判定，供 CLI/GUI 复用）
pub mod reconnect;
/// 客户端运行状态快照（托盘/GUI 展示）
pub mod status;

pub use config::*;
pub use log_buffer::{LogBuffer, LogBuffer as ClientLogBuffer};
pub use reconnect::ReconnectPolicy;
pub use status::ClientStatus;
