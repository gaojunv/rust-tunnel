//! RustDesk 配置校验：配置合并（TOML/环境变量/CLI）与管理 API 两条路径共用的唯一校验器。
//!
//! # 为什么抽出来
//!
//! 同一套规则原本在 `config/merge.rs` 与 `mgmt/api/rustdesk.rs` 各写了一遍，且已经分叉：
//! 配置文件路径漏了「中继端口 == 会合端口 - 1（NAT 探测端口）」这条（于是能写出中继与
//! 会合自抢端口、必然 bind 失败的组合），`relay_server` 一处要求 host 非空一处只要求端口
//! 非零，`enabled=false` 时一处仍校验端口一处整段跳过。把规则收敛到本模块，两条路径都调它，
//! 从结构上杜绝再次分叉。

use std::str::FromStr;

use rust_tunnel_rustdesk::rendezvous::Mask;

/// 一次调用的 RustDesk 配置快照（借用，不持有所有权）。
///
/// 刻意用平铺字段 + 借用字符串，而不是直接吃 `ServerConfig` / `RustdeskDynamicConfig`：
/// 两条调用路径的载体结构不同，校验器不该绑定其中任一类型。
#[derive(Debug)]
pub struct RustdeskValidationInput<'a> {
    /// 是否启用会合/中继服务；为 true 时两个端口必填。
    pub enabled: bool,
    /// 会合(hbbs)端口。
    pub rendezvous_port: Option<u16>,
    /// 中继(hbbr)端口。
    pub relay_port: Option<u16>,
    /// 下发给客户端的中继地址；空串 = 不下发。
    pub relay_server: &'a str,
    /// 内网判定掩码（IPv4 CIDR，如 `192.168.0.0/16`）；空串 = 不做内网判定。
    pub mask: &'a str,
    /// 其他服务已占用的端口及来源标签（如 `(8388, "ss_port")`）。
    pub other_ports: &'a [(u16, &'a str)],
}

/// RustDesk 配置校验失败原因。
///
/// `Display` 文案被既有测试按**子串**断言（`required when rustdesk_enabled`、
/// `must differ`、`must be >= 2`、`collide with <label>`、`Invalid rustdesk_mask`、
/// `Invalid rustdesk_relay_server`），改动时务必保持这些子串原样。
#[derive(Debug, thiserror::Error)]
pub enum RustdeskConfigError {
    /// 启用时缺少会合/中继端口。
    #[error(
        "rustdesk_rendezvous_port and rustdesk_relay_port are required when rustdesk_enabled is true"
    )]
    MissingPorts,
    /// 会合端口小于 2（此时 NAT 探测端口 = 会合端口 - 1 会落到 0，无法绑定）。
    #[error("rustdesk_rendezvous_port must be >= 2 (the NAT-test port is rendezvous_port - 1)")]
    RendezvousPortTooSmall,
    /// 会合端口与中继端口相同。
    #[error("rustdesk_rendezvous_port and rustdesk_relay_port must differ")]
    PortsEqual,
    /// 中继端口恰好等于会合端口派生出的 NAT 探测端口（`rendezvous_port - 1`），二者会互抢。
    #[error("rustdesk_relay_port must not equal the NAT-test port (rendezvous_port - 1)")]
    RelayPortIsNatTestPort,
    /// RustDesk 端口与其他服务占用的端口冲突。
    #[error("rustdesk ports collide with {label} (port {port})")]
    PortCollision {
        /// 冲突端口号。
        port: u16,
        /// 冲突来源标签（配置侧 `ss_port`/`trojan_port`，API 侧 `Shadowsocks`/`Trojan`）。
        label: String,
    },
    /// `mask` 不是合法 IPv4 CIDR。
    #[error("Invalid rustdesk_mask '{0}': expected IPv4 CIDR like 192.168.0.0/16")]
    InvalidMask(String),
    /// `relay_server` 不是合法 `host:port`。
    #[error("Invalid rustdesk_relay_server '{0}': expected host:port")]
    InvalidRelayServer(String),
}

/// 校验一份 RustDesk 配置；返回第一个命中的错误。
///
/// # 语义（合并三处历史分叉的取舍）
///
/// - **中继端口不得等于 `rendezvous_port - 1`**（NAT 探测端口）。这条原本只在 API 侧有，
///   现补进配置侧——否则配置文件能写出中继与 NAT 探测抢同一 TCP 端口、必然 bind 失败的组合。
/// - **`relay_server` 要求 host 非空且端口 > 0**。配置侧原只要求 host 非空（允许端口 0），
///   API 侧原只要求端口 > 0（允许空 host），两边各严一半，这里取并集：两边都严。空串仍合法，
///   表示不下发中继地址。
/// - **端口取值合法性无论 `enabled` 与否都校验**（停用时也不该把坏值写进 DB，留到下次启动才炸）；
///   只有「端口必填」这一条才受 `enabled` 约束。
///
/// # Errors
///
/// 见 [`RustdeskConfigError`] 各变体。
pub fn validate_rustdesk_config(
    input: &RustdeskValidationInput<'_>,
) -> Result<(), RustdeskConfigError> {
    let (rendezvous_port, relay_port) = (input.rendezvous_port, input.relay_port);

    // 必填只由 enabled 决定：未启用时端口可以缺席（默认配置、测试 harness 均如此）。
    if input.enabled && (rendezvous_port.is_none() || relay_port.is_none()) {
        return Err(RustdeskConfigError::MissingPorts);
    }

    // 已给出的端口值始终校验，与 enabled 无关。
    if let Some(rendezvous_port) = rendezvous_port {
        if rendezvous_port < 2 {
            return Err(RustdeskConfigError::RendezvousPortTooSmall);
        }
    }
    if let (Some(rendezvous_port), Some(relay_port)) = (rendezvous_port, relay_port) {
        if rendezvous_port == relay_port {
            return Err(RustdeskConfigError::PortsEqual);
        }
        if relay_port == rendezvous_port - 1 {
            return Err(RustdeskConfigError::RelayPortIsNatTestPort);
        }
    }

    // 与其他服务端口比对：收集已给出的 RustDesk 占用端口（会合、NAT 探测、中继）。
    let rustdesk_ports: Vec<u16> = rendezvous_port
        .into_iter()
        .flat_map(|p| [p, p - 1])
        .chain(relay_port)
        .collect();
    for &(port, label) in input.other_ports {
        if rustdesk_ports.contains(&port) {
            return Err(RustdeskConfigError::PortCollision {
                port,
                label: label.to_string(),
            });
        }
    }

    validate_mask(input.mask)?;
    validate_relay_server(input.relay_server)
}

/// 校验 `mask`：空串跳过；否则用 rustdesk 库的 [`Mask`] 作唯一解析器（避免本仓再实现一份 CIDR 解析）。
fn validate_mask(mask: &str) -> Result<(), RustdeskConfigError> {
    let mask = mask.trim();
    if mask.is_empty() {
        return Ok(());
    }
    Mask::from_str(mask).map_err(|_| RustdeskConfigError::InvalidMask(mask.to_string()))?;
    Ok(())
}

/// 校验 `relay_server`：空串跳过（= 不下发中继地址）；否则要求 host 非空且端口 > 0。
fn validate_relay_server(relay_server: &str) -> Result<(), RustdeskConfigError> {
    let relay_server = relay_server.trim();
    if relay_server.is_empty() {
        return Ok(());
    }
    let valid = relay_server.rsplit_once(':').is_some_and(|(host, port)| {
        !host.is_empty() && port.parse::<u16>().is_ok_and(|p| p > 0)
    });
    if valid {
        Ok(())
    } else {
        Err(RustdeskConfigError::InvalidRelayServer(
            relay_server.to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一份合法基线输入，供各用例做单点改动。
    fn base<'a>(other_ports: &'a [(u16, &'a str)]) -> RustdeskValidationInput<'a> {
        RustdeskValidationInput {
            enabled: true,
            rendezvous_port: Some(21116),
            relay_port: Some(21117),
            relay_server: "relay.example.com:21117",
            mask: "192.168.0.0/16",
            other_ports,
        }
    }

    #[test]
    fn valid_config_passes() {
        assert!(validate_rustdesk_config(&base(&[])).is_ok());
        // 空 relay_server / 空 mask 同样合法（表示不下发中继地址 / 不做内网判定）。
        let mut input = base(&[]);
        input.relay_server = "";
        input.mask = "";
        assert!(validate_rustdesk_config(&input).is_ok());
    }

    #[test]
    fn relay_port_on_nat_test_port_is_rejected() {
        // 中继端口 = 会合端口 - 1，与会合服务自建的 NAT 探测端口互抢。
        let mut input = base(&[]);
        input.relay_port = Some(21115);
        let err = validate_rustdesk_config(&input).unwrap_err().to_string();
        assert!(err.contains("NAT-test port"), "实得: {err}");
    }

    #[test]
    fn relay_server_without_host_is_rejected() {
        // 端口合法但 host 为空：API 旧行为放行，统一后应拒。
        let mut input = base(&[]);
        input.relay_server = ":21117";
        let err = validate_rustdesk_config(&input).unwrap_err().to_string();
        assert!(err.contains("Invalid rustdesk_relay_server"), "实得: {err}");
    }

    #[test]
    fn relay_server_with_zero_port_is_rejected() {
        // host 非空但端口为 0：配置侧旧行为放行，统一后应拒。
        let mut input = base(&[]);
        input.relay_server = "host:0";
        let err = validate_rustdesk_config(&input).unwrap_err().to_string();
        assert!(err.contains("Invalid rustdesk_relay_server"), "实得: {err}");
    }

    #[test]
    fn port_collision_with_other_service_is_rejected() {
        // NAT 探测端口（8388 = 8389 - 1）撞上 ss_port。
        let mut input = base(&[(8388, "ss_port")]);
        input.rendezvous_port = Some(8389);
        input.relay_port = Some(8390);
        let err = validate_rustdesk_config(&input).unwrap_err().to_string();
        assert!(err.contains("collide with ss_port"), "实得: {err}");
    }

    #[test]
    fn disabled_without_ports_is_silent() {
        // 默认配置 / e2e harness 的形态：未启用且无端口，必须静默通过。
        let input = RustdeskValidationInput {
            enabled: false,
            rendezvous_port: None,
            relay_port: None,
            relay_server: "",
            mask: "",
            other_ports: &[],
        };
        assert!(validate_rustdesk_config(&input).is_ok());
    }

    #[test]
    fn disabled_still_validates_port_values() {
        // 停用时也不该把越界端口写进 DB：会合端口 < 2 仍应拒。
        let mut input = base(&[]);
        input.enabled = false;
        input.rendezvous_port = Some(1);
        assert!(matches!(
            validate_rustdesk_config(&input),
            Err(RustdeskConfigError::RendezvousPortTooSmall)
        ));
    }
}
