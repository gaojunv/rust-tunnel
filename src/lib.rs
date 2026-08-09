//! rust-tunnel workspace 元包。
//!
//! 本 crate 仅作为 e2e 集成测试（`tests/`）的宿主；实际实现位于三个成员 crate：
//! - `rust-tunnel-common`：共享协议与基础设施
//! - `rust-tunnel-client`：客户端
//! - `rust-tunnel-server`：服务端

#![allow(dead_code)]
