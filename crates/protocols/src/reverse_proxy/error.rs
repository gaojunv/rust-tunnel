//! Error types for reverse proxy reconciliation.

use thiserror::Error;

/// Errors returned by the shared-listener reconciler.
#[derive(Debug, Error)]
pub enum ReconcileError {
    /// 域名冲突：同一端口下已有规则占用该域名。
    #[error("domain '{domain}' already claimed by rule '{other_rule_id}' on port {listen_addr}")]
    DomainConflict {
        /// 监听地址。
        listen_addr: String,
        /// 冲突域名。
        domain: String,
        /// 已占用该域名的规则 ID。
        other_rule_id: String,
    },

    /// TLS 配置不一致：同一端口下规则的 TLS 开关必须一致。
    #[error("TLS setting mismatch on port {listen_addr}: existing rules have tls={existing_tls}, new rule has tls={new_tls}")]
    TlsMismatch {
        /// 监听地址。
        listen_addr: String,
        /// 已有规则的 TLS 开关。
        existing_tls: bool,
        /// 新规则的 TLS 开关。
        new_tls: bool,
    },

    /// 端口绑定失败。
    #[error("failed to bind {listen_addr}: {source}")]
    BindFailed {
        /// 监听地址。
        listen_addr: String,
        /// 底层 IO 错误。
        #[source]
        source: std::io::Error,
    },

    /// 端口启用了 TLS 但未配置证书管理器。
    #[error("TLS enabled on port {listen_addr} but no certificate manager configured")]
    NoCertManager {
        /// 监听地址。
        listen_addr: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_conflict_display() {
        let e = ReconcileError::DomainConflict {
            listen_addr: "0.0.0.0:443".into(),
            domain: "a.example.com".into(),
            other_rule_id: "r-1".into(),
        };
        let s = format!("{e}");
        assert!(s.contains("a.example.com"));
        assert!(s.contains("r-1"));
        assert!(s.contains("0.0.0.0:443"));
    }

    #[test]
    fn tls_mismatch_display() {
        let e = ReconcileError::TlsMismatch {
            listen_addr: "0.0.0.0:443".into(),
            existing_tls: true,
            new_tls: false,
        };
        assert!(format!("{e}").contains("mismatch"));
    }
}
