use thiserror::Error;

#[derive(Error, Debug)]
pub enum TunnelError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] bincode::Error),

    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("Protocol error: {0}")]
    Protocol(String),

    #[error("Connection closed by peer")]
    ConnectionClosed,

    #[error("Timeout")]
    Timeout,

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Control channel error: {0}")]
    ControlChannel(String),

    #[error("TLS error: {0}")]
    Tls(String),

    #[error("Trojan authentication failed")]
    TrojanAuthFailed(Vec<u8>),
}

pub type TunnelResult<T> = Result<T, TunnelError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let err = TunnelError::Protocol("test error".into());
        assert_eq!(format!("{}", err), "Protocol error: test error");

        let err = TunnelError::Config("invalid config".into());
        assert_eq!(format!("{}", err), "Configuration error: invalid config");

        let err = TunnelError::ControlChannel("channel failed".into());
        assert_eq!(format!("{}", err), "Control channel error: channel failed");

        let err = TunnelError::ConnectionClosed;
        assert_eq!(format!("{}", err), "Connection closed by peer");

        let err = TunnelError::Timeout;
        assert_eq!(format!("{}", err), "Timeout");
    }

    #[test]
    fn test_io_error_conversion() {
        let io_err = std::io::Error::new(std::io::ErrorKind::Other, "io error");
        let tunnel_err: TunnelError = io_err.into();
        assert!(matches!(tunnel_err, TunnelError::Io(_)));
    }

    #[test]
    fn test_tunnel_result() {
        let ok_result: TunnelResult<()> = Ok(());
        assert!(ok_result.is_ok());

        let err_result: TunnelResult<()> = Err(TunnelError::Timeout);
        assert!(err_result.is_err());
    }

    #[test]
    fn test_database_error_conversion() {
        // Create a sqlx error via a failed connection
        let db_err = sqlx::Error::Io(std::io::Error::new(std::io::ErrorKind::Other, "db error"));
        let tunnel_err: TunnelError = db_err.into();
        assert!(matches!(tunnel_err, TunnelError::Database(_)));
    }

    #[test]
    fn test_protocol_error_display() {
        let err = TunnelError::Protocol("invalid message".into());
        assert_eq!(format!("{}", err), "Protocol error: invalid message");
    }

    #[test]
    fn test_tls_error_display() {
        let err = TunnelError::Tls("handshake failed".into());
        assert_eq!(format!("{}", err), "TLS error: handshake failed");
    }

    #[test]
    fn test_config_error_display() {
        let err = TunnelError::Config("missing field".into());
        assert_eq!(format!("{}", err), "Configuration error: missing field");
    }

    #[test]
    fn test_control_channel_error_display() {
        let err = TunnelError::ControlChannel("broken pipe".into());
        assert_eq!(format!("{}", err), "Control channel error: broken pipe");
    }

    #[test]
    fn test_serialization_error_conversion() {
        // bincode serialization error from serializing non-serializable data
        // We can create one by using a custom error scenario
        let data: Vec<u8> = vec![0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        let result: Result<i32, bincode::Error> = bincode::deserialize(&data);
        if let Err(e) = result {
            let tunnel_err: TunnelError = e.into();
            assert!(matches!(tunnel_err, TunnelError::Serialization(_)));
        }
    }

    #[test]
    fn test_error_debug() {
        let err = TunnelError::Protocol("test".into());
        let debug_str = format!("{:?}", err);
        assert!(debug_str.contains("Protocol"));
    }

    #[test]
    fn test_all_error_variants_display() {
        // Verify all variants produce non-empty display output
        let variants: Vec<TunnelError> = vec![
            TunnelError::Io(std::io::Error::new(std::io::ErrorKind::Other, "io")),
            TunnelError::Database(sqlx::Error::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "db",
            ))),
            TunnelError::Protocol("proto".into()),
            TunnelError::ConnectionClosed,
            TunnelError::Timeout,
            TunnelError::Config("config".into()),
            TunnelError::ControlChannel("channel".into()),
            TunnelError::Tls("tls".into()),
            TunnelError::TrojanAuthFailed(vec![1, 2, 3]),
        ];

        for err in variants {
            let display = format!("{}", err);
            assert!(!display.is_empty(), "Empty display for {:?}", err);
        }
    }

    #[test]
    fn test_trojan_auth_failed_display() {
        let err = TunnelError::TrojanAuthFailed(vec![1, 2, 3]);
        let display = format!("{}", err);
        assert!(display.contains("Trojan authentication failed"));
    }

    #[test]
    fn test_trojan_auth_failed_empty_data() {
        let err = TunnelError::TrojanAuthFailed(vec![]);
        assert!(matches!(err, TunnelError::TrojanAuthFailed(_)));
        let display = format!("{}", err);
        assert!(!display.is_empty());
    }

    #[test]
    fn test_trojan_auth_failed_with_data() {
        let data = vec![0u8; 100];
        let err = TunnelError::TrojanAuthFailed(data);
        if let TunnelError::TrojanAuthFailed(recovered) = err {
            assert_eq!(recovered.len(), 100);
        } else {
            panic!("Expected TrojanAuthFailed variant");
        }
    }
}
