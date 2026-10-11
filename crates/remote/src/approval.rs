//! 被控端接入确认：策略、待确认请求与相关时限。
//!
//! 本模块只放**公开数据类型与常量**，编排逻辑在 [`crate::manager::RemoteManager`]。
//! 关键点：待确认请求里的**一次性 token 绝不进 UI 状态**——[`PendingApproval`] 只带
//! 展示所需字段，真正用于握手的 `token` 留在管理器内部。

use rust_tunnel_common::protocol::RemoteMode;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// 被控端等待用户确认的时限；超时自动拒绝。
///
/// # 硬约束：必须明显小于服务端的 `REMOTE_SESSION_TTL`（60s）
///
/// 服务端在 `RemoteSessionTable::issue` 里会把「超过 `REMOTE_SESSION_TTL` 仍未被应答」的
/// 挂起会话判为过期并回收（`crates/server/src/control_plane/remote.rs`，其文档写作
/// 「被控端须在此期间内应答，否则视为过期」）。用户拍板的**整个**过程——弹窗、点击、
/// 经控制通道回 `RemoteSessionResponse`——都必须落在那个 60s 窗口内，否则服务端会把响应
/// 当成未知会话丢弃，控制器只能一路等到 [`APPROVAL_WAIT_TIMEOUT`] 才报超时（用户看到的
/// 是「卡住」而非「被拒」）。45s 留出 15s 余量，**不要**把它抬到 60s 或更高。
///
/// 注意被控端一旦接受，数据面建立（最长约 30s 的 hbbr 配对窗口）就与服务端会话表无关了：
/// token 已由两端各自持有，服务端不再回查，故建立耗时不吃这段 TTL。
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(45);

/// 控制器等待被控端确认的时限（须大于 [`APPROVAL_TIMEOUT`]）。
///
/// 被控端先在 [`APPROVAL_TIMEOUT`] 到点自动拒绝并回 `accepted:false`，控制器据此走拒绝
/// 路径；本时限只是**兜底**（响应丢失等），因此必须留出富余。
pub const APPROVAL_WAIT_TIMEOUT: Duration = Duration::from_secs(60);

/// 被控端的接入确认策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ApprovalPolicy {
    /// 直接接受，不询问（无人值守场景；风险最高）。
    Auto,
    /// 每次会话都需本机确认（默认）。
    #[default]
    Confirm,
}

/// 待用户确认的接入请求（UI 弹窗数据）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApproval {
    /// 会话 ID。
    pub session_id: String,
    /// 发起方客户端名。
    pub from_name: String,
    /// 会话模式。
    pub mode: RemoteMode,
    /// 等待截止时刻（毫秒，自 epoch）；到点自动拒绝。
    pub deadline_ms: u64,
}
