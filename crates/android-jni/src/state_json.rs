//! 状态/显示器列表的 JSON 序列化（契约 §3、§5）。
//!
//! 纯函数：只依赖 `rust-tunnel-controller-core` 与 `rust-tunnel-remote` 的类型，
//! 不触碰 JNI，故可在主机上完整单测。
//!
//! # 字段口径
//!
//! 除契约 §5 的最小字段集外，本模块还追加了 `peers`（在线客户端列表，含 `name`/`platform`/
//! `rd_id`）、`quality`、`latency_ms`、`codec`、`session_id`、`self_rd_id`、`capability`、
//! `connected`。Kotlin 侧按契约「忽略未知字段」处理即可。
//!
//! # 与原型的差异（有意为之）
//!
//! - `session` 取 `none|awaiting_approval|connecting|active|ended`。控制器内部的
//!   `Connecting`（已批准、正在建数据面）单独用 `connecting` 表示——若折算成
//!   `awaiting_approval`，用户在批准之后仍会看到「等待被控端确认」，与事实不符。
//! - `connection` 的 `Idle` / `Stopped` 都折算为 `disconnected`（契约只有四个值）。
//! - `kbps` 恒为数字：尚无样本时为 `0`（而非 `null`），方便 Kotlin 直接显示。

use rust_tunnel_controller_core::{ConnectionState, ControllerSnapshot, SessionPhase};
use rust_tunnel_remote::{CodecId, DisplayInfo, QualityPreset, RemoteUiState, TransportPath};
use serde_json::{json, Value};

/// 最近一帧远端画面的几何信息（由 [`FrameSink`](rust_tunnel_remote::FrameSink) 更新）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct FrameGeometry {
    /// 远端画面宽度（像素）。
    pub(crate) width: u32,
    /// 远端画面高度（像素）。
    pub(crate) height: u32,
    /// 当前画面来源显示器编号。
    pub(crate) display: u32,
}

/// 构造 `nativeSnapshot` / `onState` 的状态 JSON。
///
/// 参数：
/// - `snap`：宿主聚合快照（连接/会话/在线列表）；
/// - `remote`：引擎原始状态（补 `selected_display`、`recv_kbps`、`audio_available` 等快照外的读数）；
/// - `frame`：最近一帧远端画面的尺寸（`onFrameInfo` 的同一份数据）；
/// - `target`：最近一次 `nativeConnect` 的目标客户端名（无活动会话时用作 `target`）。
#[must_use]
pub(crate) fn state_json(
    snap: &ControllerSnapshot,
    remote: &RemoteUiState,
    frame: FrameGeometry,
    target: &str,
) -> String {
    let (width, height) = if frame.width == 0 || frame.height == 0 {
        // 还没出帧时回落到协商出的编码尺寸（也是逻辑「远端画面尺寸」）。
        remote.encode_dims.unwrap_or((0, 0))
    } else {
        (frame.width, frame.height)
    };

    let active_target = snap
        .active
        .as_ref()
        .map(|active| active.peer_name.clone())
        .filter(|name| !name.is_empty());
    let session_id = snap.active.as_ref().map(|active| active.session_id.clone());

    let peers: Vec<Value> = snap
        .peers
        .iter()
        .map(|peer| {
            let name = if peer.display_name.trim().is_empty() {
                peer.client_name.clone()
            } else {
                peer.display_name.clone()
            };
            json!({
                "name": name,
                "display_name": peer.display_name,
                "client_name": peer.client_name,
                "platform": peer.platform,
                "rd_id": peer.rd_id,
                "displays": peer.displays,
            })
        })
        .collect();

    let value = json!({
        "connection": connection_str(&snap.connection),
        "session": session_str(snap.phase),
        "target": active_target.unwrap_or_else(|| target.to_owned()),
        "reason": snap.session_end.as_ref().map(|(_, end)| end.reason.clone()),
        "remote_width": width,
        "remote_height": height,
        "display": remote.selected_display,
        "fps": snap.frame_rate,
        "kbps": remote.recv_kbps.unwrap_or(0),
        "path": snap.transport.map(path_str),
        "audio_available": remote.audio_available,
        "error": snap.last_error,
        "peers": peers,
        "quality": quality_str(snap.quality),
        "latency_ms": snap.latency_ms,
        "codec": snap.codec.map(codec_str),
        "session_id": session_id,
        "self_rd_id": snap.rd_id,
        "capability": snap.capability,
        "connected": snap.is_connected(),
    });
    value.to_string()
}

/// 构造 `onDisplays` 的显示器列表 JSON（契约 §3）。
#[must_use]
pub(crate) fn displays_json(displays: &[DisplayInfo]) -> String {
    let list: Vec<Value> = displays
        .iter()
        .map(|display| {
            json!({
                "id": display.id,
                "width": display.width,
                "height": display.height,
                "primary": display.primary,
                "name": display.name,
            })
        })
        .collect();
    Value::Array(list).to_string()
}

/// 连接状态 → 契约取值。
fn connection_str(state: &ConnectionState) -> &'static str {
    match state {
        ConnectionState::Connected => "connected",
        ConnectionState::Connecting => "connecting",
        ConnectionState::Rejected { .. } => "rejected",
        // `Disconnected` 是退避重连中；`Idle`/`Stopped` 对 UI 而言同样是「没有连接」。
        ConnectionState::Disconnected { .. } | ConnectionState::Idle | ConnectionState::Stopped => {
            "disconnected"
        }
    }
}

/// 会话阶段 → 契约取值。
fn session_str(phase: SessionPhase) -> &'static str {
    match phase {
        SessionPhase::Idle => "none",
        SessionPhase::AwaitingApproval => "awaiting_approval",
        SessionPhase::Connecting => "connecting",
        SessionPhase::Active => "active",
        SessionPhase::Ended => "ended",
    }
}

/// 画质档位 → 字符串。
fn quality_str(preset: QualityPreset) -> &'static str {
    match preset {
        QualityPreset::Smooth => "smooth",
        QualityPreset::Balanced => "balanced",
        QualityPreset::Hd => "hd",
    }
}

/// 数据面路径 → 字符串。
fn path_str(path: TransportPath) -> &'static str {
    match path {
        TransportPath::Direct => "direct",
        TransportPath::Relay => "relay",
        TransportPath::Punch => "punch",
    }
}

/// codec → 字符串。
fn codec_str(codec: CodecId) -> &'static str {
    match codec {
        CodecId::Vp9 => "vp9",
        CodecId::H264 => "h264",
        CodecId::H265 => "h265",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_tunnel_common::{RemoteMode, RemotePeerInfo};
    use rust_tunnel_controller_core::LoopPhase;
    use rust_tunnel_remote::{
        ActiveSessionInfo, CodecId, SessionEnd, SessionRole, SessionState, TransportPath,
    };

    fn snapshot_default() -> ControllerSnapshot {
        ControllerSnapshot {
            connection: ConnectionState::Connected,
            phase: SessionPhase::Idle,
            active: None,
            session_end: None,
            last_error: None,
            peers: Vec::new(),
            quality: QualityPreset::Balanced,
            frame_rate: 30,
            latency_ms: None,
            self_platform: "android".into(),
            rd_id: "123456789".into(),
            capability: true,
            transport: None,
            codec: None,
        }
    }

    fn parse(json: &str) -> Value {
        serde_json::from_str(json).expect("JSON 应可解析")
    }

    #[test]
    fn minimal_fields_present_and_typed() {
        let snap = snapshot_default();
        let remote = RemoteUiState::default();
        let json = state_json(
            &snap,
            &remote,
            FrameGeometry {
                width: 0,
                height: 0,
                display: 0,
            },
            "home-pc",
        );
        let v = parse(&json);

        assert_eq!(v["connection"], "connected");
        assert_eq!(v["session"], "none");
        assert_eq!(v["target"], "home-pc");
        assert!(v["reason"].is_null());
        assert_eq!(v["remote_width"], 0);
        assert_eq!(v["remote_height"], 0);
        assert_eq!(v["display"], 0);
        assert_eq!(v["fps"], 30);
        assert_eq!(v["kbps"], 0);
        assert!(v["path"].is_null());
        assert_eq!(v["audio_available"], false);
        assert!(v["error"].is_null());
        assert_eq!(v["peers"].as_array().expect("peers 数组").len(), 0);
        // 追加字段
        assert_eq!(v["quality"], "balanced");
        assert_eq!(v["self_rd_id"], "123456789");
        assert_eq!(v["capability"], true);
        assert_eq!(v["connected"], true);
    }

    #[test]
    fn frame_geometry_wins_over_encode_dims() {
        let mut snap = snapshot_default();
        snap.active = Some(ActiveSessionInfo {
            session_id: "sid-1".into(),
            role: SessionRole::Controller,
            peer_name: "office-pc".into(),
            peer_platform: "windows".into(),
            peer_rd_id: "987654321".into(),
            mode: RemoteMode::Control,
            state: SessionState::Active,
            started_at_ms: 1,
        });
        snap.phase = SessionPhase::Active;
        snap.transport = Some(TransportPath::Punch);
        snap.codec = Some(CodecId::H264);
        snap.latency_ms = Some(23);

        let mut remote = RemoteUiState::default();
        remote.encode_dims = Some((1920, 1080));
        remote.selected_display = 2;
        remote.recv_kbps = Some(2400);
        remote.audio_available = true;

        let v = parse(&state_json(
            &snap,
            &remote,
            FrameGeometry {
                width: 3840,
                height: 2160,
                display: 2,
            },
            "ignored-because-active",
        ));
        // 有活跃会话时 target 取会话对端名，而不是最后一次连接请求。
        assert_eq!(v["target"], "office-pc");
        assert_eq!(v["session"], "active");
        assert_eq!(v["remote_width"], 3840);
        assert_eq!(v["remote_height"], 2160);
        assert_eq!(v["display"], 2);
        assert_eq!(v["kbps"], 2400);
        assert_eq!(v["path"], "punch");
        assert_eq!(v["codec"], "h264");
        assert_eq!(v["latency_ms"], 23);
        assert_eq!(v["audio_available"], true);
        assert_eq!(v["session_id"], "sid-1");
    }

    #[test]
    fn encode_dims_used_before_first_frame() {
        let snap = snapshot_default();
        let mut remote = RemoteUiState::default();
        remote.encode_dims = Some((1280, 720));
        let v = parse(&state_json(&snap, &remote, FrameGeometry::default(), "pc"));
        assert_eq!(v["remote_width"], 1280);
        assert_eq!(v["remote_height"], 720);
    }

    #[test]
    fn ended_session_reports_reason() {
        let mut snap = snapshot_default();
        snap.phase = SessionPhase::Ended;
        snap.session_end = Some((
            "sid-9".into(),
            SessionEnd {
                reason: "对端关闭".into(),
                deliberate: false,
            },
        ));
        let v = parse(&state_json(
            &snap,
            &RemoteUiState::default(),
            FrameGeometry::default(),
            "pc",
        ));
        assert_eq!(v["session"], "ended");
        assert_eq!(v["reason"], "对端关闭");
    }

    #[test]
    fn connecting_session_is_distinct_from_awaiting_approval() {
        let mut snap = snapshot_default();
        snap.phase = SessionPhase::Connecting;
        let v = parse(&state_json(
            &snap,
            &RemoteUiState::default(),
            FrameGeometry::default(),
            "pc",
        ));
        assert_eq!(v["session"], "connecting");

        snap.phase = SessionPhase::AwaitingApproval;
        let v = parse(&state_json(
            &snap,
            &RemoteUiState::default(),
            FrameGeometry::default(),
            "pc",
        ));
        assert_eq!(v["session"], "awaiting_approval");
    }

    #[test]
    fn connection_state_strings() {
        assert_eq!(connection_str(&ConnectionState::Connecting), "connecting");
        assert_eq!(connection_str(&ConnectionState::Connected), "connected");
        assert_eq!(
            connection_str(&ConnectionState::Disconnected { reason: None }),
            "disconnected"
        );
        assert_eq!(
            connection_str(&ConnectionState::Rejected {
                reason: "认证失败".into()
            }),
            "rejected"
        );
        assert_eq!(connection_str(&ConnectionState::Stopped), "disconnected");
        assert_eq!(connection_str(&ConnectionState::Idle), "disconnected");
        // 四个契约取值之外的枚举确实不影响取值集合。
        let snapshot = snapshot_default();
        let v = parse(&state_json(
            &snapshot,
            &RemoteUiState::default(),
            FrameGeometry::default(),
            "pc",
        ));
        assert_eq!(v["connection"], "connected");
    }

    #[test]
    fn error_and_backoff_surfaces() {
        let mut snap = snapshot_default();
        snap.connection = ConnectionState::Disconnected {
            reason: Some("reset".into()),
        };
        snap.last_error = Some("reset".into());
        let v = parse(&state_json(
            &snap,
            &RemoteUiState::default(),
            FrameGeometry::default(),
            "pc",
        ));
        assert_eq!(v["connection"], "disconnected");
        assert_eq!(v["error"], "reset");
    }

    #[test]
    fn peers_include_name_platform_and_rd_id() {
        let mut snap = snapshot_default();
        snap.peers = vec![
            RemotePeerInfo {
                client_name: "home-nas".into(),
                display_name: "Home NAS".into(),
                platform: "linux".into(),
                displays: 1,
                rd_id: "111222333".into(),
            },
            RemotePeerInfo {
                client_name: "bare".into(),
                display_name: String::new(),
                platform: "windows".into(),
                displays: 2,
                rd_id: "444555666".into(),
            },
        ];
        let v = parse(&state_json(
            &snap,
            &RemoteUiState::default(),
            FrameGeometry::default(),
            "pc",
        ));
        let peers = v["peers"].as_array().expect("peers 数组");
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0]["name"], "Home NAS");
        assert_eq!(peers[0]["platform"], "linux");
        assert_eq!(peers[0]["rd_id"], "111222333");
        // 展示名为空时回落到客户端名。
        assert_eq!(peers[1]["name"], "bare");
        assert_eq!(peers[1]["displays"], 2);
    }

    #[test]
    fn displays_json_shape() {
        let displays = vec![
            DisplayInfo {
                id: 0,
                width: 1920,
                height: 1080,
                name: "主显示器".into(),
                primary: true,
            },
            DisplayInfo {
                id: 1,
                width: 2560,
                height: 1440,
                name: "副屏".into(),
                primary: false,
            },
        ];
        let v = parse(&displays_json(&displays));
        let list = v.as_array().expect("数组");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0]["id"], 0);
        assert_eq!(list[0]["width"], 1920);
        assert_eq!(list[0]["primary"], true);
        assert_eq!(list[1]["height"], 1440);
        assert_eq!(list[1]["primary"], false);
        assert_eq!(displays_json(&[]), "[]");
    }

    #[test]
    fn phase_strings_are_from_contract_set() {
        let mut snap = snapshot_default();
        for (phase, expected) in [
            (SessionPhase::Idle, "none"),
            (SessionPhase::AwaitingApproval, "awaiting_approval"),
            (SessionPhase::Connecting, "connecting"),
            (SessionPhase::Active, "active"),
            (SessionPhase::Ended, "ended"),
        ] {
            snap.phase = phase;
            let v = parse(&state_json(
                &snap,
                &RemoteUiState::default(),
                FrameGeometry::default(),
                "pc",
            ));
            assert_eq!(v["session"], expected);
        }
        // LoopPhase 与快照无关，但确保导入的类型仍可用（保持测试与宿主 API 同步）。
        let _ = LoopPhase::Connecting;
    }
}
