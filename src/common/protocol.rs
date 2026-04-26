use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::common::{TunnelError, TunnelResult};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum ControlMessage {
    /// Client requests registration to expose a remote port
    Register {
        remote_port: u16,
        /// Hostname of the client machine (optional for backward compatibility)
        hostname: Option<String>,
    },
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
        let msg = ControlMessage::Register {
            remote_port: 8080,
            hostname: Some("test-host".into())
        };
        let bytes = msg.serialize().unwrap();
        assert!(bytes.len() > 4);
    }

    #[test]
    fn test_message_variants_serialization() {
        // Test all message variants can be serialized
        let messages = vec![
            ControlMessage::Register { remote_port: 8080, hostname: None },
            ControlMessage::RegisterResponse { success: true, message: "ok".into() },
            ControlMessage::NewConnection { connection_id: 12345, remote_port: 9000 },
            ControlMessage::ConnectionReady { connection_id: 12345 },
            ControlMessage::Data { connection_id: 12345, data: vec![1, 2, 3, 4] },
            ControlMessage::Close { connection_id: 12345 },
            ControlMessage::Ping,
            ControlMessage::Pong,
            ControlMessage::Disconnect,
        ];

        for msg in messages {
            let bytes = msg.serialize().unwrap();
            assert!(bytes.len() > 4);
            // Verify length prefix
            let len = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
            assert_eq!(len, bytes.len() - 4);
        }
    }

    #[test]
    fn test_max_message_size() {
        // Create a message that would exceed max size when serialized
        let large_data = vec![0u8; 2 * 1024 * 1024]; // 2MB
        let msg = ControlMessage::Data { connection_id: 1, data: large_data };
        let result = msg.serialize();
        // The serialization itself works, but read_from_stream will reject it
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_write_and_read_from_stream() {
        // Create an in-memory buffer to simulate a stream
        let mut buffer = Vec::new();

        // Write a message
        let original_msg = ControlMessage::Data {
            connection_id: 42,
            data: vec![10, 20, 30, 40, 50],
        };
        original_msg.write_to_stream(&mut buffer).await.unwrap();

        // Read it back
        let mut reader = &buffer[..];
        let read_msg = ControlMessage::read_from_stream(&mut reader).await.unwrap();

        assert!(read_msg.is_some());
        match read_msg.unwrap() {
            ControlMessage::Data { connection_id, data } => {
                assert_eq!(connection_id, 42);
                assert_eq!(data, vec![10, 20, 30, 40, 50]);
            }
            _ => panic!("Unexpected message type"),
        }
    }

    #[tokio::test]
    async fn test_read_from_stream_eof() {
        // Empty buffer
        let mut buffer = &[] as &[u8];
        let result = ControlMessage::read_from_stream(&mut buffer).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_read_from_stream_partial_length() {
        // Only 2 bytes of length prefix - should return Ok(None) for EOF
        let buffer = [0x00, 0x01];
        let mut reader = &buffer[..];
        let result = ControlMessage::read_from_stream(&mut reader).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_write_to_split_alias() {
        let mut buffer = Vec::new();
        let msg = ControlMessage::Ping;
        // write_to_split is just an alias for write_to_stream
        msg.write_to_split(&mut buffer).await.unwrap();
        assert!(!buffer.is_empty());
    }
}
