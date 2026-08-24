//! ACP bridge 测试集（按域拆分，共享辅助在 [`helpers`]）。

pub(super) mod helpers;

mod cancel_queue;
mod capabilities;
mod config_injection;
mod config_overrides;
mod events;
mod fs_tunnel;
mod llm_proxy;
mod mcp_tunnel;
mod persistence;
mod prompt_guard;
mod resume;
mod session_lifecycle;
mod stdio_pump;
mod ws_conn;
