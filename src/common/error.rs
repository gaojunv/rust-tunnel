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
}
