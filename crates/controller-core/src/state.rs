//! 面向 UI 的状态快照与事件映射。
//!
//! 引擎已经提供两份原始 `watch`：[`ClientStatus`]（控制通道）与
//! [`RemoteUiState`]（远程会话）。本模块把两者加上宿主自己的重连阶段聚合为一份
//! 轻量 [`ControllerSnapshot`]，并给出「当前应展示什么事件」的纯函数映射
//! [`ControllerSnapshot::event`]——UI 只需按 `session_id`/状态变化去重。

use rust_tunnel_client::ClientStatus;
use rust_tunnel_common::RemotePeerInfo;
use rust_tunnel_remote::{
    ActiveSessionInfo, CodecId, QualityPreset, RemoteUiState, SessionEnd, TransportPath,
};

/// 控制通道重连循环当前所处的阶段（宿主内部维护）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum LoopPhase {
    /// 正在连接/已连接：真正的连通性由 [`ClientStatus::connected`] 判定。
    #[default]
    Connecting,
    /// 刚断开，正在退避等待下一次重连。
    BackingOff {
        /// 断开原因（`None` 表示对端正常关闭）。
        reason: Option<String>,
    },
    /// 注册被拒绝（认证失败等），**不再重连**。
    Rejected {
        /// 服务端给出的原因。
        reason: String,
    },
    /// 已停止（用户调用 `stop` 或宿主被释放）。
    Stopped,
}

/// 控制通道的对外连接状态。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ConnectionState {
    /// 尚未启动。
    #[default]
    Idle,
    /// 正在建立控制通道。
    Connecting,
    /// 已注册，控制通道可用。
    Connected,
    /// 已断开，退避后会自动重连。
    Disconnected {
        /// 断开原因（`None` 表示对端正常关闭）。
        reason: Option<String>,
    },
    /// 注册被拒绝，不会自动重连（需要用户修配置）。
    Rejected {
        /// 服务端给出的原因。
        reason: String,
    },
    /// 宿主已停止。
    Stopped,
}

/// 远程会话生命周期（控制端视角）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionPhase {
    /// 没有会话。
    #[default]
    Idle,
    /// 已受理授权，等待被控端用户确认（对应引擎的 `Negotiating`）。
    AwaitingApproval,
    /// 已确认，正在建立 P2P/中继数据面。
    Connecting,
    /// 数据面就绪，画面/输入在流动。
    Active,
    /// 会话已结束（`session_end` 给出原因）。
    Ended,
}

/// 控制器宿主给 UI 的一份轻量状态快照。
///
/// 只含可廉价克隆的小对象（`peers` 除外，它跟随服务端下发的在线列表）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControllerSnapshot {
    /// 控制通道状态。
    pub connection: ConnectionState,
    /// 远程会话阶段。
    pub phase: SessionPhase,
    /// 当前会话（无则 `None`）。
    pub active: Option<ActiveSessionInfo>,
    /// 最近一次由会话循环观测到的结束信息。
    pub session_end: Option<(String, SessionEnd)>,
    /// 最近一次错误（控制通道或远程引擎）。
    pub last_error: Option<String>,
    /// 可被远程控制的在线客户端。
    pub peers: Vec<RemotePeerInfo>,
    /// 控制器选中的画质档位。
    pub quality: QualityPreset,
    /// 控制器选中的帧率上限。
    pub frame_rate: u8,
    /// 最近实测延迟（毫秒）。
    pub latency_ms: Option<u32>,
    /// 本端平台（恒为 `android`）。
    pub self_platform: String,
    /// 服务端分配的本端 RustDesk ID。
    pub rd_id: String,
    /// 服务端是否启用了远程控制。
    pub capability: bool,
    /// 当前会话的数据面路径。
    pub transport: Option<TransportPath>,
    /// 当前会话协商出的 codec。
    pub codec: Option<CodecId>,
}

impl ControllerSnapshot {
    /// 由三份原始状态聚合出快照（纯函数，便于单测）。
    #[must_use]
    pub fn derive(status: &ClientStatus, phase: &LoopPhase, remote: &RemoteUiState) -> Self {
        Self {
            connection: connection_state(status, phase),
            phase: session_phase(remote),
            active: remote.active.clone(),
            session_end: remote.session_end.clone(),
            last_error: status
                .last_error
                .clone()
                .or_else(|| remote.last_error.clone()),
            peers: remote.peers.clone(),
            quality: remote.quality,
            frame_rate: remote.frame_rate,
            latency_ms: remote.latency_ms,
            self_platform: remote.self_platform.clone(),
            rd_id: remote.rd_id.clone(),
            capability: remote.capability,
            transport: remote.transport,
            codec: remote.codec,
        }
    }

    /// 控制通道是否已可用。
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.connection == ConnectionState::Connected
    }

    /// 当前应展示的事件（由快照映射，UI 自行去重）。
    #[must_use]
    pub fn event(&self) -> ControllerEvent {
        if let ConnectionState::Rejected { reason } = &self.connection {
            return ControllerEvent::Rejected {
                reason: reason.clone(),
            };
        }
        match self.phase {
            SessionPhase::AwaitingApproval => {
                if let Some(active) = &self.active {
                    return ControllerEvent::SessionAwaitingApproval {
                        session_id: active.session_id.clone(),
                    };
                }
            }
            SessionPhase::Active => {
                if let Some(active) = &self.active {
                    return ControllerEvent::SessionActive {
                        session_id: active.session_id.clone(),
                    };
                }
            }
            SessionPhase::Ended => {
                if let Some((session_id, end)) = &self.session_end {
                    return ControllerEvent::SessionEnded {
                        session_id: session_id.clone(),
                        reason: end.reason.clone(),
                    };
                }
                if let Some(active) = &self.active {
                    return ControllerEvent::SessionEnded {
                        session_id: active.session_id.clone(),
                        reason: "会话已结束".to_owned(),
                    };
                }
            }
            SessionPhase::Idle | SessionPhase::Connecting => {}
        }
        match &self.connection {
            ConnectionState::Idle => ControllerEvent::Idle,
            ConnectionState::Connecting => ControllerEvent::Connecting,
            ConnectionState::Connected => ControllerEvent::Connected,
            ConnectionState::Disconnected { reason } => ControllerEvent::Disconnected {
                reason: reason.clone().or_else(|| self.last_error.clone()),
            },
            ConnectionState::Rejected { reason } => ControllerEvent::Rejected {
                reason: reason.clone(),
            },
            ConnectionState::Stopped => ControllerEvent::Stopped,
        }
    }
}

/// UI 可直接展示的事件（从 [`ControllerSnapshot`] 映射，不代表「刚刚发生变化」）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControllerEvent {
    /// 尚未启动。
    Idle,
    /// 正在建立控制通道。
    Connecting,
    /// 控制通道已注册。
    Connected,
    /// 控制通道断开（会自动重连）。
    Disconnected {
        /// 断开原因。
        reason: Option<String>,
    },
    /// 注册被拒绝（不会自动重连）。
    Rejected {
        /// 服务端给出的原因。
        reason: String,
    },
    /// 宿主已停止。
    Stopped,
    /// 会话已受理，等待被控端用户确认（对应「审批等待」）。
    SessionAwaitingApproval {
        /// 会话 ID。
        session_id: String,
    },
    /// 会话已建立，画面/输入在流动。
    SessionActive {
        /// 会话 ID。
        session_id: String,
    },
    /// 会话已结束。
    SessionEnded {
        /// 会话 ID。
        session_id: String,
        /// 结束原因。
        reason: String,
    },
}

/// 聚合控制通道状态。
fn connection_state(status: &ClientStatus, phase: &LoopPhase) -> ConnectionState {
    match phase {
        LoopPhase::Rejected { reason } => ConnectionState::Rejected {
            reason: reason.clone(),
        },
        _ if status.connected => ConnectionState::Connected,
        LoopPhase::Stopped => ConnectionState::Stopped,
        LoopPhase::BackingOff { reason } => ConnectionState::Disconnected {
            reason: reason.clone().or_else(|| status.last_error.clone()),
        },
        LoopPhase::Connecting => ConnectionState::Connecting,
    }
}

/// 聚合远程会话阶段。
fn session_phase(remote: &RemoteUiState) -> SessionPhase {
    let Some(active) = &remote.active else {
        return if remote.session_end.is_some() {
            SessionPhase::Ended
        } else {
            SessionPhase::Idle
        };
    };
    match active.state {
        rust_tunnel_remote::SessionState::Negotiating => SessionPhase::AwaitingApproval,
        rust_tunnel_remote::SessionState::Connecting => SessionPhase::Connecting,
        rust_tunnel_remote::SessionState::Active => SessionPhase::Active,
        rust_tunnel_remote::SessionState::Ended => SessionPhase::Ended,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_tunnel_remote::{SessionRole, SessionState};

    fn status(connected: bool, last_error: Option<&str>) -> ClientStatus {
        let mut s = ClientStatus::new("host:3000".into(), "pad".into(), "0.1.0".into());
        s.connected = connected;
        s.last_error = last_error.map(ToOwned::to_owned);
        s
    }

    fn active(state: SessionState) -> ActiveSessionInfo {
        ActiveSessionInfo {
            session_id: "sid-1".into(),
            role: SessionRole::Controller,
            peer_name: "home-pc".into(),
            peer_platform: "windows".into(),
            peer_rd_id: "123456789".into(),
            mode: rust_tunnel_common::RemoteMode::Control,
            state,
            started_at_ms: 1,
        }
    }

    #[test]
    fn connecting_when_attempt_in_flight() {
        let snap = ControllerSnapshot::derive(
            &status(false, None),
            &LoopPhase::Connecting,
            &RemoteUiState::default(),
        );
        assert_eq!(snap.connection, ConnectionState::Connecting);
        assert_eq!(snap.event(), ControllerEvent::Connecting);
    }

    #[test]
    fn connected_wins_over_loop_phase() {
        let snap = ControllerSnapshot::derive(
            &status(true, None),
            &LoopPhase::Connecting,
            &RemoteUiState::default(),
        );
        assert_eq!(snap.connection, ConnectionState::Connected);
        assert!(snap.is_connected());
        assert_eq!(snap.event(), ControllerEvent::Connected);
    }

    #[test]
    fn backing_off_maps_to_disconnected_with_reason() {
        let snap = ControllerSnapshot::derive(
            &status(false, None),
            &LoopPhase::BackingOff {
                reason: Some("reset".into()),
            },
            &RemoteUiState::default(),
        );
        assert_eq!(
            snap.connection,
            ConnectionState::Disconnected {
                reason: Some("reset".into())
            }
        );
        assert_eq!(
            snap.event(),
            ControllerEvent::Disconnected {
                reason: Some("reset".into())
            }
        );
    }

    #[test]
    fn rejected_takes_priority() {
        let snap = ControllerSnapshot::derive(
            &status(true, None),
            &LoopPhase::Rejected {
                reason: "认证失败".into(),
            },
            &RemoteUiState::default(),
        );
        assert_eq!(
            snap.connection,
            ConnectionState::Rejected {
                reason: "认证失败".into()
            }
        );
        assert_eq!(
            snap.event(),
            ControllerEvent::Rejected {
                reason: "认证失败".into()
            }
        );
    }

    #[test]
    fn stopped_maps_to_stopped() {
        let snap = ControllerSnapshot::derive(
            &status(false, None),
            &LoopPhase::Stopped,
            &RemoteUiState::default(),
        );
        assert_eq!(snap.connection, ConnectionState::Stopped);
        assert_eq!(snap.event(), ControllerEvent::Stopped);
    }

    #[test]
    fn negotiating_is_awaiting_approval() {
        let mut remote = RemoteUiState::default();
        remote.active = Some(active(SessionState::Negotiating));
        let snap = ControllerSnapshot::derive(&status(true, None), &LoopPhase::Connecting, &remote);
        assert_eq!(snap.phase, SessionPhase::AwaitingApproval);
        assert_eq!(
            snap.event(),
            ControllerEvent::SessionAwaitingApproval {
                session_id: "sid-1".into()
            }
        );
    }

    #[test]
    fn active_and_connecting_phases() {
        let mut remote = RemoteUiState::default();
        remote.active = Some(active(SessionState::Connecting));
        let snap = ControllerSnapshot::derive(&status(true, None), &LoopPhase::Connecting, &remote);
        assert_eq!(snap.phase, SessionPhase::Connecting);
        // Connecting 不是事件优先级最高的一档：控制通道事件照常给出。
        assert_eq!(snap.event(), ControllerEvent::Connected);

        remote.active = Some(active(SessionState::Active));
        let snap = ControllerSnapshot::derive(&status(true, None), &LoopPhase::Connecting, &remote);
        assert_eq!(snap.phase, SessionPhase::Active);
        assert_eq!(
            snap.event(),
            ControllerEvent::SessionActive {
                session_id: "sid-1".into()
            }
        );
    }

    #[test]
    fn ended_session_reports_reason() {
        let mut remote = RemoteUiState::default();
        remote.session_end = Some((
            "sid-1".into(),
            SessionEnd {
                reason: "对端关闭".into(),
                deliberate: false,
            },
        ));
        let snap = ControllerSnapshot::derive(&status(true, None), &LoopPhase::Connecting, &remote);
        assert_eq!(snap.phase, SessionPhase::Ended);
        assert_eq!(
            snap.event(),
            ControllerEvent::SessionEnded {
                session_id: "sid-1".into(),
                reason: "对端关闭".into(),
            }
        );
    }
}
