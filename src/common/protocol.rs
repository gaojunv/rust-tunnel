use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::common::{TunnelError, TunnelResult};

/// A log entry from a connected client
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ClientLogEntry {
    /// Microsecond timestamp
    pub timestamp: i64,
    /// TRACE/DEBUG/INFO/WARN/ERROR
    pub level: String,
    /// tracing target (module path)
    pub target: String,
    /// Log message content
    pub message: String,
}

/// A member of a mesh network
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MeshMember {
    pub client_name: String,
    pub public_addr: Option<String>,
    pub online: bool,
}

/// A service exposed by a mesh client (used in protocol messages)
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MeshServiceDef {
    pub name: String,
    pub protocol: String,
    pub local_addr: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum ControlMessage {
    /// Client requests registration to expose a remote port
    Register {
        remote_port: u16,
        /// Hostname of the client machine (optional for backward compatibility)
        hostname: Option<String>,
        /// Authentication token (optional for backward compatibility, required if server enables auth)
        auth_token: Option<String>,
    },
    /// Server response to registration
    RegisterResponse { success: bool, message: String },
    /// Server notifies client of a new incoming connection
    NewConnection {
        connection_id: u64,
        remote_port: u16,
    },
    /// Client notifies server it's connected to local target and ready
    ConnectionReady { connection_id: u64 },
    /// Data transfer for a specific connection
    Data { connection_id: u64, data: Vec<u8> },
    /// Close a specific connection
    Close { connection_id: u64 },
    /// Heartbeat ping (client -> server)
    Ping {
        /// Heartbeat sequence number (client increments)
        seq: u32,
        /// Send timestamp (microseconds, client time)
        timestamp_micros: u64,
    },
    /// Heartbeat pong (server -> client)
    Pong {
        /// Corresponding Ping's sequence number
        seq: u32,
        /// Ping timestamp (echoed back)
        ping_timestamp_micros: u64,
        /// Pong send timestamp (server time)
        pong_timestamp_micros: u64,
    },
    /// Server requests client to disconnect (web interface admin action)
    Disconnect,
    /// Mesh network registration (client -> server)
    MeshJoin {
        mesh_id: String,
        client_name: String,
    },
    /// Leave a mesh network (client -> server)
    MeshLeave {
        mesh_id: String,
    },
    /// Server sends mesh member list to clients (server -> client)
    MeshMemberList {
        mesh_id: String,
        members: Vec<MeshMember>,
    },
    /// Request to connect to a service on another mesh client (client -> server)
    MeshConnect {
        target_client: String,
        service_name: String,
    },
    /// Request P2P hole punch with target (client -> server, contains own public address)
    P2PRequest {
        target_client: String,
        local_addr: String,
    },
    /// Forward P2P response with remote address info (server -> client)
    P2PResponse {
        target_client: String,
        remote_addr: String,
    },
    /// Report P2P hole punch result (client -> server)
    P2PResult {
        target_client: String,
        success: bool,
    },
    /// Relay data through server when P2P fails (client <-> server)
    MeshRelay {
        target_client: String,
        data: Vec<u8>,
    },
    /// Client registers mesh services (client -> server, sent after MeshJoin)
    MeshRegisterServices {
        mesh_id: String,
        services: Vec<MeshServiceDef>,
    },
    /// Client sends a batch of log entries
    LogBatch {
        entries: Vec<ClientLogEntry>,
    },
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
    pub async fn read_from_stream<R: AsyncReadExt + Unpin>(
        stream: &mut R,
    ) -> TunnelResult<Option<Self>> {
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
    pub async fn write_to_stream<W: AsyncWriteExt + Unpin>(
        &self,
        stream: &mut W,
    ) -> TunnelResult<()> {
        let bytes = self.serialize()?;
        stream.write_all(&bytes).await?;
        stream.flush().await?;
        Ok(())
    }

    /// Write message to a split stream half (alias for write_to_stream)
    pub async fn write_to_split<W: AsyncWriteExt + Unpin>(
        &self,
        stream: &mut W,
    ) -> TunnelResult<()> {
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
            hostname: Some("test-host".into()),
            auth_token: Some("secret-token".into()),
        };
        let bytes = msg.serialize().unwrap();
        assert!(bytes.len() > 4);
    }

    #[test]
    fn test_message_variants_serialization() {
        // Test all message variants can be serialized
        let messages = vec![
            ControlMessage::Register {
                remote_port: 8080,
                hostname: None,
                auth_token: None,
            },
            ControlMessage::RegisterResponse {
                success: true,
                message: "ok".into(),
            },
            ControlMessage::NewConnection {
                connection_id: 12345,
                remote_port: 9000,
            },
            ControlMessage::ConnectionReady {
                connection_id: 12345,
            },
            ControlMessage::Data {
                connection_id: 12345,
                data: vec![1, 2, 3, 4],
            },
            ControlMessage::Close {
                connection_id: 12345,
            },
            ControlMessage::Ping {
                seq: 1,
                timestamp_micros: 123456789,
            },
            ControlMessage::Pong {
                seq: 1,
                ping_timestamp_micros: 123456789,
                pong_timestamp_micros: 123456795,
            },
            ControlMessage::Disconnect,
            ControlMessage::MeshJoin {
                mesh_id: "test-mesh".into(),
                client_name: "client-a".into(),
            },
            ControlMessage::MeshLeave {
                mesh_id: "test-mesh".into(),
            },
            ControlMessage::MeshMemberList {
                mesh_id: "test-mesh".into(),
                members: vec![MeshMember {
                    client_name: "client-a".into(),
                    public_addr: Some("1.2.3.4:12345".into()),
                    online: true,
                }],
            },
            ControlMessage::MeshConnect {
                target_client: "client-b".into(),
                service_name: "db".into(),
            },
            ControlMessage::P2PRequest {
                target_client: "client-b".into(),
                local_addr: "1.2.3.4:12345".into(),
            },
            ControlMessage::P2PResponse {
                target_client: "client-b".into(),
                remote_addr: "5.6.7.8:54321".into(),
            },
            ControlMessage::P2PResult {
                target_client: "client-b".into(),
                success: true,
            },
            ControlMessage::MeshRelay {
                target_client: "client-b".into(),
                data: vec![1, 2, 3],
            },
            ControlMessage::MeshRegisterServices {
                mesh_id: "test-mesh".into(),
                services: vec![MeshServiceDef {
                    name: "db".into(),
                    protocol: "mysql".into(),
                    local_addr: "localhost:3306".into(),
                }],
            },
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
        let msg = ControlMessage::Data {
            connection_id: 1,
            data: large_data,
        };
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
            ControlMessage::Data {
                connection_id,
                data,
            } => {
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
        let msg = ControlMessage::Ping {
            seq: 1,
            timestamp_micros: 123456789,
        };
        // write_to_split is just an alias for write_to_stream
        msg.write_to_split(&mut buffer).await.unwrap();
        assert!(!buffer.is_empty());
    }

    #[tokio::test]
    async fn test_roundtrip_all_message_types() {
        let messages = vec![
            ControlMessage::Register {
                remote_port: 8080,
                hostname: Some("test-host".into()),
                auth_token: Some("token".into()),
            },
            ControlMessage::RegisterResponse {
                success: true,
                message: "ok".into(),
            },
            ControlMessage::RegisterResponse {
                success: false,
                message: "port in use".into(),
            },
            ControlMessage::NewConnection {
                connection_id: 12345,
                remote_port: 9000,
            },
            ControlMessage::ConnectionReady {
                connection_id: 12345,
            },
            ControlMessage::Data {
                connection_id: 12345,
                data: vec![1, 2, 3, 4],
            },
            ControlMessage::Data {
                connection_id: 0,
                data: vec![],
            },
            ControlMessage::Close {
                connection_id: 12345,
            },
            ControlMessage::Ping {
                seq: 42,
                timestamp_micros: 123456789,
            },
            ControlMessage::Pong {
                seq: 42,
                ping_timestamp_micros: 123456789,
                pong_timestamp_micros: 123456795,
            },
            ControlMessage::Disconnect,
            ControlMessage::MeshJoin {
                mesh_id: "test-mesh".into(),
                client_name: "client-a".into(),
            },
            ControlMessage::MeshLeave {
                mesh_id: "test-mesh".into(),
            },
            ControlMessage::MeshMemberList {
                mesh_id: "test-mesh".into(),
                members: vec![MeshMember {
                    client_name: "client-a".into(),
                    public_addr: Some("1.2.3.4:12345".into()),
                    online: true,
                }],
            },
            ControlMessage::MeshConnect {
                target_client: "client-b".into(),
                service_name: "db".into(),
            },
            ControlMessage::P2PRequest {
                target_client: "client-b".into(),
                local_addr: "1.2.3.4:12345".into(),
            },
            ControlMessage::P2PResponse {
                target_client: "client-b".into(),
                remote_addr: "5.6.7.8:54321".into(),
            },
            ControlMessage::P2PResult {
                target_client: "client-b".into(),
                success: true,
            },
            ControlMessage::MeshRelay {
                target_client: "client-b".into(),
                data: vec![1, 2, 3],
            },
            ControlMessage::MeshRegisterServices {
                mesh_id: "test-mesh".into(),
                services: vec![MeshServiceDef {
                    name: "db".into(),
                    protocol: "mysql".into(),
                    local_addr: "localhost:3306".into(),
                }],
            },
        ];

        for msg in messages {
            let mut buffer = Vec::new();
            msg.write_to_stream(&mut buffer).await.unwrap();

            let mut reader = &buffer[..];
            let read_msg = ControlMessage::read_from_stream(&mut reader).await.unwrap();
            assert!(read_msg.is_some(), "Failed to roundtrip {:?}", msg);
        }
    }

    #[test]
    fn test_log_batch_serialization() {
        let msg = ControlMessage::LogBatch {
            entries: vec![
                ClientLogEntry {
                    timestamp: 1234567890,
                    level: "INFO".into(),
                    target: "client::proxy".into(),
                    message: "Connection established".into(),
                },
                ClientLogEntry {
                    timestamp: 1234567891,
                    level: "ERROR".into(),
                    target: "client::control".into(),
                    message: "Heartbeat timeout".into(),
                },
            ],
        };
        let bytes = msg.serialize().unwrap();
        assert!(bytes.len() > 4);
    }

    #[tokio::test]
    async fn test_multiple_messages_on_stream() {
        let mut buffer = Vec::new();

        let msg1 = ControlMessage::Register {
            remote_port: 8080,
            hostname: None,
            auth_token: None,
        };
        let msg2 = ControlMessage::RegisterResponse {
            success: true,
            message: "ok".into(),
        };
        let msg3 = ControlMessage::Ping {
            seq: 1,
            timestamp_micros: 100,
        };

        msg1.write_to_stream(&mut buffer).await.unwrap();
        msg2.write_to_stream(&mut buffer).await.unwrap();
        msg3.write_to_stream(&mut buffer).await.unwrap();

        let mut reader = &buffer[..];
        let r1 = ControlMessage::read_from_stream(&mut reader)
            .await
            .unwrap()
            .unwrap();
        let r2 = ControlMessage::read_from_stream(&mut reader)
            .await
            .unwrap()
            .unwrap();
        let r3 = ControlMessage::read_from_stream(&mut reader)
            .await
            .unwrap()
            .unwrap();
        let r4 = ControlMessage::read_from_stream(&mut reader).await.unwrap();

        assert!(matches!(r1, ControlMessage::Register { .. }));
        assert!(matches!(r2, ControlMessage::RegisterResponse { .. }));
        assert!(matches!(r3, ControlMessage::Ping { .. }));
        assert!(r4.is_none()); // No more messages
    }

    #[tokio::test]
    async fn test_large_data_message() {
        let large_data = vec![0xAB; 100_000]; // 100KB
        let msg = ControlMessage::Data {
            connection_id: 1,
            data: large_data.clone(),
        };

        let mut buffer = Vec::new();
        msg.write_to_stream(&mut buffer).await.unwrap();

        let mut reader = &buffer[..];
        let read_msg = ControlMessage::read_from_stream(&mut reader)
            .await
            .unwrap()
            .unwrap();

        match read_msg {
            ControlMessage::Data {
                connection_id,
                data,
            } => {
                assert_eq!(connection_id, 1);
                assert_eq!(data, large_data);
            }
            _ => panic!("Unexpected message type"),
        }
    }

    #[test]
    fn test_serialize_length_prefix_correct() {
        let msg = ControlMessage::Disconnect;
        let bytes = msg.serialize().unwrap();

        // Length prefix should match payload length
        let len = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        assert_eq!(len, bytes.len() - 4);
    }

    #[test]
    fn test_serialize_register_with_all_fields() {
        let msg = ControlMessage::Register {
            remote_port: 65535,
            hostname: Some("a-very-long-hostname-with-special-chars-!@#$%".into()),
            auth_token: Some("bearer-token-12345".into()),
        };
        let bytes = msg.serialize().unwrap();
        assert!(bytes.len() > 4);

        // Verify deserialization works
        let len = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        let payload = &bytes[4..4 + len];
        let deserialized: ControlMessage = bincode::deserialize(payload).unwrap();
        assert!(matches!(deserialized, ControlMessage::Register { .. }));
    }

    #[tokio::test]
    async fn test_read_from_stream_corrupted_length() {
        // Create a message with length prefix pointing to more data than available
        let mut buffer = Vec::new();
        buffer.extend_from_slice(&100000u32.to_be_bytes()); // Claims 100KB payload
        buffer.extend_from_slice(&[1, 2, 3]); // Only 3 bytes

        let mut reader = &buffer[..];
        let result = ControlMessage::read_from_stream(&mut reader).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_read_from_stream_zero_length() {
        let mut buffer = Vec::new();
        buffer.extend_from_slice(&0u32.to_be_bytes()); // 0 length

        let mut reader = &buffer[..];
        let result = ControlMessage::read_from_stream(&mut reader).await;
        // Zero length means empty payload, bincode may fail to deserialize
        assert!(result.is_err());
    }

    #[test]
    fn test_message_clone() {
        let msg = ControlMessage::Data {
            connection_id: 42,
            data: vec![1, 2, 3],
        };
        let cloned = msg.clone();
        match cloned {
            ControlMessage::Data {
                connection_id,
                data,
            } => {
                assert_eq!(connection_id, 42);
                assert_eq!(data, vec![1, 2, 3]);
            }
            _ => panic!("Unexpected message type"),
        }
    }
}
