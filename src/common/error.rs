use thiserror::Error;

#[derive(Error, Debug)]
pub enum TunnelError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] bincode::Error),

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
}

pub type TunnelResult<T> = Result<T, TunnelError>;
