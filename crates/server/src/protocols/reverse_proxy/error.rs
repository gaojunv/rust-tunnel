//! Error types for reverse proxy reconciliation.

use thiserror::Error;

/// Errors returned by the shared-listener reconciler.
#[derive(Debug, Error)]
pub enum ReconcileError {
    #[error("domain '{domain}' already claimed by rule '{other_rule_id}' on port {listen_addr}")]
    DomainConflict {
        listen_addr: String,
        domain: String,
        other_rule_id: String,
    },

    #[error("TLS setting mismatch on port {listen_addr}: existing rules have tls={existing_tls}, new rule has tls={new_tls}")]
    TlsMismatch {
        listen_addr: String,
        existing_tls: bool,
        new_tls: bool,
    },

    #[error("failed to bind {listen_addr}: {source}")]
    BindFailed {
        listen_addr: String,
        #[source]
        source: std::io::Error,
    },

    #[error("TLS enabled on port {listen_addr} but no certificate manager configured")]
    NoCertManager { listen_addr: String },
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
