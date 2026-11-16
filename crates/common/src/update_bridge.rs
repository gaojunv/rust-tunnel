//! 客户端自更新与控制通道的桥接接口。
//!
//! 自更新只有 GUI 客户端才有（CLI 没有可交互的确认弹窗，也没有可替换的桌面程序），
//! 但控制通道的读循环在 `crates/client`、更新状态机在 `crates/client-gui`。为了让
//! `crates/client` 不依赖 GUI 框架，把两者之间的接口定义在这里——依赖方向为
//! `client → common ← client-gui`，与 [`crate::remote_bridge`] 完全同构。
//!
//! 生命周期：注册成功后 [`ClientUpdateBridge::attach`] 注入上行发送端（重连时热替换），
//! 读循环把 `ClientUpdate*` 变体交给 [`ClientUpdateBridge::handle_control_message`]，
//! 断开时 [`ClientUpdateBridge::on_disconnected`] 丢弃失效的发送端。
//!
//! **三个方法都在控制通道读循环里同步调用**，实现必须非阻塞：下载分片可能每秒到达
//! 几百条，任何阻塞都会拖住心跳与隧道消息。耗时工作（哈希校验、落盘）应在实现内部
//! 转交工作线程或标记为待处理，由 GUI 线程消费。

use crate::protocol::ControlMessage;

/// 客户端自更新与 [`crate::protocol::ControlMessage`] 控制通道之间的桥接。
pub trait ClientUpdateBridge: Send + Sync + 'static {
    /// 控制通道就绪时注入上行发送端（重连时热替换旧发送端）。
    fn attach(&self, sender: tokio::sync::mpsc::Sender<ControlMessage>);

    /// 处理一条来自服务端的控制消息。
    ///
    /// 返回 `true` 表示这条消息属于客户端自更新且已被消费；返回 `false` 表示与
    /// 本功能无关，调用方继续走原有的兜底分支。
    fn handle_control_message(&self, msg: ControlMessage) -> bool;

    /// 控制通道断开：丢弃已失效的上行发送端（在途下载由实现决定是否作废）。
    fn on_disconnected(&self);
}
