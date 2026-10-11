//! 远程控制功能的控制通道桥接接口。
//!
//! GUI 客户端的远程控制引擎（`rust-tunnel-remote`）需要同时做两件事：接收服务端
//! 经控制通道下发的远程控制消息，以及把上行消息写回同一条通道。但 `crates/client`
//! 不该依赖 `crates/remote`（CLI 客户端完全用不到远程控制），因此把接口定义在这里
//! ——依赖方向为 `client → common ← remote`，两侧都只认这个 trait。
//!
//! 生命周期与 [`crate::protocol::ControlMessage`] 的控制通道一致：注册成功后
//! [`RemoteBridge::attach`] 注入上行发送端，读循环把远程变体交给
//! [`RemoteBridge::handle_control_message`]，断开时 [`RemoteBridge::on_disconnected`]
//! 清理在途会话。重连会重新 `attach`（热替换），因此实现必须容忍多次注入。

use crate::protocol::ControlMessage;

/// 远程控制引擎与控制通道之间的桥接。
///
/// 实现者通常是一个常驻的远程引擎（`RemoteManager`），内部持有 UI 事件通道与
/// 当前会话状态；本 trait 的三个方法都必须是非阻塞的——它们在控制通道的读循环里
/// 被同步调用，任何阻塞都会拖住心跳与隧道消息。
pub trait RemoteBridge: Send + Sync + 'static {
    /// 控制通道就绪时注入上行发送端（重连时热替换旧发送端）。
    fn attach(&self, sender: tokio::sync::mpsc::Sender<ControlMessage>);

    /// 处理一条来自服务端的控制消息。
    ///
    /// 返回 `true` 表示这条消息属于远程控制且已被消费；返回 `false` 表示与本功能
    /// 无关，调用方继续走原有的兜底分支（通常是 `warn!` 日志）。
    fn handle_control_message(&self, msg: ControlMessage) -> bool;

    /// 控制通道断开：清理在途会话并丢弃已失效的上行发送端。
    fn on_disconnected(&self);
}
