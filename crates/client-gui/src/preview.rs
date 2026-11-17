//! 截图评审用的演示数据（`RUST_TUNNEL_GUI_DEMO=1` 时启用）。
//!
//! **只在隔离的配置目录里使用**：本模块不发起任何网络连接，也不写配置；它只是把一份
//! 合成的 `ClientStatus` 与几条日志塞进 `AppState`，让状态页/映射页/日志页在没有真实
//! 服务端的情况下也能展示完整布局（空状态看不出卡片、趋势线与表格的层次）。
//!
//! 正常启动不设置该变量，这段代码不会被触达。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rust_tunnel_client::ClientStatus;
use rust_tunnel_common::protocol::{
    BackendSummary, ClientLogEntry, MappingSummary, RouteSummary, RuleSummary,
};

use crate::app::AppState;

/// 是否启用演示数据。
#[must_use]
pub fn enabled() -> bool {
    std::env::var("RUST_TUNNEL_GUI_DEMO").as_deref() == Ok("1")
}

/// 注入演示状态与日志。
pub fn seed(app_state: &AppState) {
    let mut status = ClientStatus::new(
        "tunnel.example.com:3000".to_owned(),
        "home-nas".to_owned(),
        env!("CARGO_PKG_VERSION").to_owned(),
    );
    status.connected = true;
    status.connected_at =
        Some(SystemTime::now() - (Duration::from_hours(2) + Duration::from_mins(14)));
    status.active_tunnels = 3;
    status.pending_tunnels = 0;
    status.rtt_ms = Some(12.0);
    status.recent_tunnels = (1u64..=6)
        .map(|i| {
            (
                i,
                format!("192.168.1.{}:{}", 10 + i, 8000 + i),
                SystemTime::now() - Duration::from_secs((7 - i) * 9),
            )
        })
        .collect();
    status.mapping_summary = Some(demo_mappings());
    let _ = app_state.status_tx.send(status);

    for entry in demo_logs() {
        app_state.log_buffer.push(entry);
    }
}

fn demo_mappings() -> MappingSummary {
    MappingSummary {
        connected_at: None,
        active_tunnels: 3,
        rtt_ms: Some(12.0),
        truncated: false,
        rules: vec![
            RuleSummary {
                id: "rule_7f3a".to_owned(),
                name: "web-app".to_owned(),
                listen: "0.0.0.0:443".to_owned(),
                domains: vec!["app.example.com".to_owned(), "www.example.com".to_owned()],
                tls_enabled: true,
                routes: vec![
                    RouteSummary {
                        path: "/".to_owned(),
                        backends: vec![
                            backend("Client", "127.0.0.1:8080", Some("home-nas"), 1),
                            backend("Client", "127.0.0.1:8080", Some("office-pc"), 2),
                        ],
                    },
                    RouteSummary {
                        path: "/api".to_owned(),
                        backends: vec![backend("Direct", "10.0.0.5:9000", None, 1)],
                    },
                ],
            },
            RuleSummary {
                id: "rule_2b91".to_owned(),
                name: "ssh-gateway".to_owned(),
                listen: "0.0.0.0:2222".to_owned(),
                domains: Vec::new(),
                tls_enabled: false,
                routes: vec![RouteSummary {
                    path: "/".to_owned(),
                    backends: vec![backend("Client", "127.0.0.1:22", Some("home-nas"), 1)],
                }],
            },
            RuleSummary {
                id: "rule_c04d".to_owned(),
                name: "media".to_owned(),
                listen: "0.0.0.0:8096".to_owned(),
                domains: vec!["media.example.com".to_owned()],
                tls_enabled: true,
                routes: vec![RouteSummary {
                    path: "/".to_owned(),
                    backends: vec![backend("Client", "192.168.1.20:8096", Some("media-box"), 1)],
                }],
            },
        ],
    }
}

fn backend(kind: &str, addr: &str, client: Option<&str>, weight: u32) -> BackendSummary {
    BackendSummary {
        kind: kind.to_owned(),
        addr: addr.to_owned(),
        client_name: client.map(str::to_owned),
        weight,
    }
}

fn demo_logs() -> Vec<ClientLogEntry> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX));
    let rows = [
        (
            "INFO",
            "rust_tunnel_client::control",
            "控制通道已连接，协议版本 9",
        ),
        (
            "INFO",
            "rust_tunnel_client::control",
            "注册成功：home-nas（rd_id 418923077）",
        ),
        (
            "INFO",
            "rust_tunnel_client::proxy",
            "打开隧道 #1 → 127.0.0.1:8080",
        ),
        (
            "DEBUG",
            "rust_tunnel_client::control",
            "心跳 RTT 12 ms，质量评分 98",
        ),
        (
            "INFO",
            "rust_tunnel_client::proxy",
            "打开隧道 #2 → 127.0.0.1:22",
        ),
        (
            "WARN",
            "rust_tunnel_client::control",
            "服务端推送的映射摘要被 1MB 上限截断",
        ),
        (
            "ERROR",
            "rust_tunnel_client::proxy",
            "连接 192.168.1.42:9000 超时（目标未监听）",
        ),
        ("INFO", "rust_tunnel_remote::manager", "允许被远程控制：开"),
        (
            "DEBUG",
            "rust_tunnel_remote::controller",
            "关键帧请求 1/s，补丁 24/s，解码 2.1 ms",
        ),
        (
            "INFO",
            "rust_tunnel_client::proxy",
            "隧道 #1 已关闭（对端 EOF）",
        ),
        (
            "INFO",
            "rust_tunnel_client::control",
            "心跳正常，控制通道保持连接",
        ),
    ];
    rows.iter()
        .enumerate()
        .map(|(i, (level, target, message))| ClientLogEntry {
            timestamp: now - i64::try_from(i).unwrap_or(0) * 2_500_000,
            level: (*level).to_owned(),
            target: (*target).to_owned(),
            message: (*message).to_owned(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::demo_mappings;

    #[test]
    fn demo_mappings_have_routes_and_backends() {
        let summary = demo_mappings();
        assert_eq!(summary.rules.len(), 3);
        assert!(summary
            .rules
            .iter()
            .any(|rule| rule.routes.iter().any(|route| !route.backends.is_empty())));
    }
}
