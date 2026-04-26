use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::common::{TunnelError, TunnelResult};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum ControlMessage {
    /// Client requests registration to expose a remote port
    Register { remote_port: u16 },
    /// Server response to registration
    RegisterResponse { success: bool, message: String },
    /// Server notifies client of a new incoming connection
    NewConnection { connection_id: u64, remote_port: u16 },
    /// Client notifies server it's connected to local target and ready
    ConnectionReady { connection_id: u64 },
    /// Data transfer for a specific connection
    Data { connection_id: u64, data: Vec<u8> },
    /// Close a specific connection
    Close { connection_id: u64 },
    /// Heartbeat ping (client -> server)
    Ping,
    /// Heartbeat pong (server -> client)
    Pong,
    /// Server requests client to disconnect (web interface admin action)
    Disconnect,
}

impl ControlMessage {
    /// Serialize message to bytes with length prefix
    pub fn serialize(&self) -> TunnelResult<Vec<u8>> {
        let encoded = bincode::serialize(self)?;
        let len = encoded.len() as u32;
        let mut result = Vec::with_capacity(4 + encoded.len());
        result.extend_from_slice(&len.to_be_bytes());
        result.extend_from_slice(&encoded);
        Ok(result)
    }

    /// Deserialize message from stream
    pub async fn read_from_stream<R: AsyncReadExt + Unpin>(stream: &mut R) -> TunnelResult<Option<Self>> {
        let mut len_buf = [0u8; 4];
        match stream.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(None);
            }
            Err(e) => return Err(TunnelError::Io(e)),
        }

        let len = u32::from_be_bytes(len_buf) as usize;
        // Maximum message size is 1MB to prevent OOM attacks or corrupted data
        const MAX_MESSAGE_SIZE: usize = 1024 * 1024; // 1MB
        if len > MAX_MESSAGE_SIZE {
            return Err(TunnelError::Protocol(format!(
                "Message too large: {} bytes (max: {})",
                len, MAX_MESSAGE_SIZE
            )));
        }
        let mut buf = vec![0u8; len];
        stream.read_exact(&mut buf).await?;

        let msg = bincode::deserialize(&buf)?;
        Ok(Some(msg))
    }

    /// Write message to stream
    pub async fn write_to_stream<W: AsyncWriteExt + Unpin>(&self, stream: &mut W) -> TunnelResult<()> {
        let bytes = self.serialize()?;
        stream.write_all(&bytes).await?;
        stream.flush().await?;
        Ok(())
    }

    /// Write message to a split stream half (alias for write_to_stream)
    pub async fn write_to_split<W: AsyncWriteExt + Unpin>(&self, stream: &mut W) -> TunnelResult<()> {
        self.write_to_stream(stream).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serialize_deserialize() {
        let msg = ControlMessage::Register { remote_port: 8080 };
        let bytes = msg.serialize().unwrap();
        assert!(bytes.len() > 4);
    }
}
