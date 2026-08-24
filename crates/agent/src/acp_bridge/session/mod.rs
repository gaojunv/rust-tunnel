//! ACP session 生命周期与连接处理：spawn/attach/detach/resume/kill、回合
//! prompt/cancel、ACP handshake（含 fs 请求转发与审批）、客户端控制消息路由。
//!
//! `impl AcpBridge` 的会话方法按域拆至子模块：[`lifecycle`]（ensure/wait_ready）、
//! [`prompt`]（回合与队列）、[`kill`]（清理回收）、[`config`]（配置项）、
//! [`client_msg`]（控制消息路由）；自由函数见 [`ws`]（事件通道）、
//! [`approval`]（审批桥接）、[`capabilities`]、[`fs`]（fs 隧道转发）。
//! stdio pump 见 [`super::pump`]，事件/消息落库见 [`super::store`]，idle reaper
//! 见 [`super::reaper`]。

mod approval;
mod capabilities;
mod client_msg;
mod config;
mod fs;
mod handshake;
mod kill;
mod lifecycle;
mod mcp_tunnel;
mod prompt;
mod ws;

pub(crate) use approval::{
    approve_or_disconnect, elicit_or_disconnect, permission_option_to_approval, pick_option,
};
pub(crate) use capabilities::{client_capabilities, to_workspace_relative};
pub(crate) use fs::{acp_raw_to_string, exec_fs_read, exec_fs_write};
pub(crate) use ws::{broadcast_ws_frame, current_ws_channel, current_ws_tx};
