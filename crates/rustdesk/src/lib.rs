// 测试代码豁免 panic 风险 lint（生产代码仍告警）
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! 内置 RustDesk 兼容服务：汇合/ID 服务器（hbbs）+ 中继服务器（hbbr）。
//!
//! # 为什么自研而不是托管官方二进制
//!
//! 上游 `rustdesk-server` 是 **AGPL-3.0**，而本项目是 `MIT OR Apache-2.0`。
//! 把它的代码抄进来（或以 lib 形式链接）会把整个项目拖成 AGPL，因此这里
//! **独立实现线上协议**，只对齐字段号、端口与帧格式这些互操作必需的事实，
//! 代码组织、命名与注释均为本项目自有。运行官方二进制作为独立进程是另一条
//! 合规路线，但会带来额外进程与分发负担。
//!
//! # 模块划分
//!
//! - [`frame`]：长度前缀帧编解码（含中继数据面的裸字节模式）
//! - [`proto`]：控制消息的 protobuf 定义（手写 `prost` derive，无需 `protoc`）
//! - [`key`]：服务端密钥（Ed25519 签名 + `licence_key` 明文比对）
//! - [`addr`]：控制消息里传递套接字地址的 `AddrMangle` 编解码
//! - [`peer`]：在线对端注册表（ID → 地址/公钥/心跳新鲜度）
//! - [`relay`]：中继服务器 hbbr
//! - [`rendezvous`]：汇合/ID 服务器 hbbs（注册、心跳、打洞协调、中继调度）
//!
//! # 端口约定（与上游一致）
//!
//! | 端口 | 组件 | 传输 | 用途 |
//! |---|---|---|---|
//! | 21115 | hbbs | TCP | NAT 类型探测 |
//! | 21116 | hbbs | TCP + UDP | 注册、心跳、打洞协调 |
//! | 21117 | hbbr | TCP | 中继数据面 |
//! | 21118 | hbbs | TCP/WS | Web 客户端接入 |
//! | 21119 | hbbr | TCP/WS | Web 客户端中继 |
//!
//! 端口不是硬编码：hbbs 以「主端口」为基准取 `port - 1` 与 `port + 2`，
//! hbbr 取 `port` 与 `port + 2`，因此改主端口即可整体平移。
//!
//! # 未实现的部分（有意为之）
//!
//! - **WebRTC 通道**：`webrtc_sdp_*`、`IceCandidate` 只做声明与透传，不协商。
//! - **密钥交换升级**（[`proto::KeyExchange`] / `kx_version`）：这两个消息类型**只为与上游
//!   客户端互通而在 [`proto`] 里声明**（收到时不会因未知变体报错）；服务端**不实现任何应答
//!   或协商**——TCP 信令路径收到即直接关闭连接（不回任何消息），UDP 路径静默丢弃，中继侧按
//!   非法帧拒绝。客户端因此永远拿不到前向安全通道，只能走经典方案。
//! - **Web 客户端端口**（21118/21119）：需要 WebSocket 承载，而当前依赖里没有可用的
//!   WebSocket 实现；原生桌面客户端走裸 TCP，不受影响。这两条端口在有需要时再补。

pub mod addr;
pub mod frame;
pub mod key;
pub mod peer;
pub mod proto;
pub mod relay;
pub mod rendezvous;
