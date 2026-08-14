use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{TunnelError, TunnelResult};

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

/// A command the AI agent asks a client to execute (server -> client)
#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum AgentCommand {
    Shell {
        cmd: String,
        cwd: Option<String>,
    },
    ReadFile {
        path: String,
    },
    WriteFile {
        path: String,
        content: String,
    },
    ListDir {
        path: String,
    },
    GitStatus,
    GitDiff {
        path: Option<String>,
    },
    GitCommit {
        message: String,
    },
    GitPush,
    /// 在工作区内搜索文件内容（字面量子串匹配），返回 path:line:content 列表
    Search {
        pattern: String,
        /// 相对工作区根的起始目录（"." 表示根）
        path: String,
        /// 文件名后缀 glob 过滤（仅支持 "*.ext" 或精确文件名），None 搜索全部文本文件
        include: Option<String>,
    },
    /// 锚点字符串替换：old_string 必须在文件中恰好出现一次
    PatchFile {
        path: String,
        old_string: String,
        new_string: String,
    },
    /// 通用 git 命令：参数已由服务端 `git_plan::plan` 白名单校验（未知子命令/
    /// flag fail-closed，pathspec 防注入），客户端按 arg 向量直接执行（host/docker）。
    GitExec {
        args: Vec<String>,
    },
}

/// Result of an agent command executed on the client (client -> server)
#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum AgentResult {
    Shell {
        stdout: String,
        stderr: String,
        exit_code: i32,
    },
    /// Also used for ListDir / GitStatus / GitDiff textual output
    FileContent {
        content: String,
    },
    /// WriteFile / GitCommit / GitPush 等无返回值的命令
    Success,
    Error {
        message: String,
    },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum ControlMessage {
    /// Client requests registration with protocol version, name, password, and client version
    Register {
        protocol_version: u32,
        client_name: String,
        password: String,
        client_version: String,
    },
    /// Server response to registration
    RegisterResponse { success: bool, message: String },
    /// Server requests client to disconnect (web interface admin action / close reason)
    Disconnect { reason: String },
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
    /// Client requests to open a tunnel to a local target
    OpenTunnel {
        connection_id: u64,
        target_addr: String,
    },
    /// Server response to a tunnel open request
    TunnelOpenResult {
        connection_id: u64,
        success: bool,
        error: Option<String>,
    },
    /// Mesh network registration (client -> server)
    MeshJoin {
        mesh_id: String,
        client_name: String,
    },
    /// Leave a mesh network (client -> server)
    MeshLeave { mesh_id: String },
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
    LogBatch { entries: Vec<ClientLogEntry> },
    /// Server asks an agent-capable client to execute a command
    AgentExecRequest {
        session_id: String,
        request_id: String,
        /// Workspace root directory on the client; the executor sandboxes into it
        /// (in docker mode this is the container-side path)
        root_path: String,
        /// When set, commands run via `docker exec <container>` instead of host shell
        docker_container: Option<String>,
        command: AgentCommand,
    },
    /// Client returns the result of an agent command
    AgentExecResponse {
        session_id: String,
        request_id: String,
        result: AgentResult,
    },
    /// Server asks an agent-capable client to kill the running exec for a
    /// request (真取消：停止回合时杀掉内网侧正在执行的命令)。
    AgentExecCancel { request_id: String },
    /// Server asks client to spawn a long-lived agent/LLM-proxy process.
    /// stdio flows via AgentSpawnData; process exit reported via AgentSpawnExit.
    AgentSpawnRequest {
        session_id: String,
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
        cwd: Option<String>,
    },
    /// Client reports spawn result
    AgentSpawnResponse {
        session_id: String,
        success: bool,
        error: Option<String>,
    },
    /// Bidirectional stdio for a spawned process.
    /// stdin=true: server -> client (write to process stdin);
    /// stdin=false: client -> server (process stdout).
    AgentSpawnData {
        session_id: String,
        data: Vec<u8>,
        stdin: bool,
    },
    /// Client reports spawned process exit
    AgentSpawnExit {
        session_id: String,
        code: Option<i32>,
    },
    /// Client-side LLM loop proxy forwards an LLM API request to the server
    AgentLlmProxyRequest {
        request_id: String,
        /// Owning ACP session; server uses it to resolve workspace -> model/key
        session_id: String,
        /// e.g. "/v1/chat/completions" or "/v1/messages"
        path: String,
        body: Vec<u8>,
    },
    /// Server streams LLM response back (SSE chunks; done=true ends)
    AgentLlmProxyChunk {
        request_id: String,
        data: Vec<u8>,
        done: bool,
        status: u16,
    },
    /// Server asks client to start the embedded LLM loop proxy for a session.
    /// Client reports the bound loopback port via AgentLlmProxyReady.
    AgentLlmProxyStart { session_id: String },
    /// Client reports the bound loopback port (0 = failure)
    AgentLlmProxyReady { session_id: String, port: u16 },
    /// Server asks client to stop the embedded LLM loop proxy for a session
    /// (frees the loopback listener; no response expected).
    AgentLlmProxyStop { session_id: String },
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

        let msg: Self = bincode::deserialize(&buf)?;
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
    fn test_register_v2_roundtrip() {
        let msg = ControlMessage::Register {
            protocol_version: 2,
            client_name: "home-nas".into(),
            password: "secret".into(),
            client_version: "0.4.0".into(),
        };
        let bytes = msg.serialize().unwrap();
        let decoded: ControlMessage = bincode::deserialize(&bytes[4..]).unwrap();
        match decoded {
            ControlMessage::Register {
                protocol_version,
                client_name,
                password,
                client_version,
            } => {
                assert_eq!(protocol_version, 2);
                assert_eq!(client_name, "home-nas");
                assert_eq!(password, "secret");
                assert_eq!(client_version, "0.4.0");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_open_tunnel_roundtrip() {
        let msg = ControlMessage::OpenTunnel {
            connection_id: 42,
            target_addr: "127.0.0.1:80".into(),
        };
        let bytes = msg.serialize().unwrap();
        let decoded: ControlMessage = bincode::deserialize(&bytes[4..]).unwrap();
        match decoded {
            ControlMessage::OpenTunnel {
                connection_id,
                target_addr,
            } => {
                assert_eq!(connection_id, 42);
                assert_eq!(target_addr, "127.0.0.1:80");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_tunnel_open_result_roundtrip() {
        let msg = ControlMessage::TunnelOpenResult {
            connection_id: 42,
            success: false,
            error: Some("connection refused".into()),
        };
        let bytes = msg.serialize().unwrap();
        let decoded: ControlMessage = bincode::deserialize(&bytes[4..]).unwrap();
        match decoded {
            ControlMessage::TunnelOpenResult {
                connection_id,
                success,
                error,
            } => {
                assert_eq!(connection_id, 42);
                assert!(!success);
                assert_eq!(error.as_deref(), Some("connection refused"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_disconnect_with_reason_roundtrip() {
        let msg = ControlMessage::Disconnect {
            reason: "replaced".into(),
        };
        let bytes = msg.serialize().unwrap();
        let decoded: ControlMessage = bincode::deserialize(&bytes[4..]).unwrap();
        match decoded {
            ControlMessage::Disconnect { reason } => assert_eq!(reason, "replaced"),
            _ => panic!("wrong variant"),
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
                protocol_version: 2,
                client_name: "test-client".into(),
                password: "token".into(),
                client_version: "0.4.0".into(),
            },
            ControlMessage::RegisterResponse {
                success: true,
                message: "ok".into(),
            },
            ControlMessage::RegisterResponse {
                success: false,
                message: "port in use".into(),
            },
            ControlMessage::Disconnect {
                reason: "shutdown".into(),
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
            ControlMessage::OpenTunnel {
                connection_id: 12345,
                target_addr: "127.0.0.1:80".into(),
            },
            ControlMessage::TunnelOpenResult {
                connection_id: 12345,
                success: true,
                error: None,
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
            ControlMessage::AgentLlmProxyStop {
                session_id: "sess-1".into(),
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
            protocol_version: 2,
            client_name: "test-client".into(),
            password: "secret".into(),
            client_version: "0.4.0".into(),
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
        let msg = ControlMessage::Ping {
            seq: 1,
            timestamp_micros: 100,
        };
        let bytes = msg.serialize().unwrap();

        // Length prefix should match payload length
        let len = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        assert_eq!(len, bytes.len() - 4);
    }

    #[test]
    fn test_serialize_register_with_all_fields() {
        let msg = ControlMessage::Register {
            protocol_version: 2,
            client_name: "a-very-long-hostname-with-special-chars-!@#$%".into(),
            password: "bearer-password-12345".into(),
            client_version: "0.4.0".into(),
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

    #[test]
    fn test_agent_exec_request_roundtrip() {
        let msg = ControlMessage::AgentExecRequest {
            session_id: "sess-1".into(),
            request_id: "req-1".into(),
            root_path: "/workspace".into(),
            docker_container: Some("dev-ctr".into()),
            command: AgentCommand::Shell {
                cmd: "ls -la".into(),
                cwd: Some("/workspace".into()),
            },
        };
        let bytes = msg.serialize().unwrap();
        let decoded: ControlMessage = bincode::deserialize(&bytes[4..]).unwrap();
        match decoded {
            ControlMessage::AgentExecRequest {
                session_id,
                request_id,
                root_path,
                docker_container,
                command,
            } => {
                assert_eq!(session_id, "sess-1");
                assert_eq!(request_id, "req-1");
                assert_eq!(root_path, "/workspace");
                assert_eq!(docker_container.as_deref(), Some("dev-ctr"));
                match command {
                    AgentCommand::Shell { cmd, cwd } => {
                        assert_eq!(cmd, "ls -la");
                        assert_eq!(cwd.as_deref(), Some("/workspace"));
                    }
                    _ => panic!("wrong command variant"),
                }
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_agent_exec_cancel_roundtrip() {
        let msg = ControlMessage::AgentExecCancel {
            request_id: "req-cancel-1".into(),
        };
        let bytes = msg.serialize().unwrap();
        let decoded: ControlMessage = bincode::deserialize(&bytes[4..]).unwrap();
        assert!(matches!(
            decoded,
            ControlMessage::AgentExecCancel { request_id } if request_id == "req-cancel-1"
        ));
    }

    #[test]
    fn test_agent_exec_response_all_results_roundtrip() {
        let results = vec![
            AgentResult::Shell {
                stdout: "out".into(),
                stderr: "err".into(),
                exit_code: 1,
            },
            AgentResult::FileContent {
                content: "file body".into(),
            },
            AgentResult::Success,
            AgentResult::Error {
                message: "boom".into(),
            },
        ];
        for result in results {
            let msg = ControlMessage::AgentExecResponse {
                session_id: "s".into(),
                request_id: "r".into(),
                result,
            };
            let bytes = msg.serialize().unwrap();
            let decoded: ControlMessage = bincode::deserialize(&bytes[4..]).unwrap();
            assert!(matches!(decoded, ControlMessage::AgentExecResponse { .. }));
        }
    }

    #[test]
    fn test_agent_command_all_variants_roundtrip() {
        let commands = vec![
            AgentCommand::ReadFile {
                path: "a.rs".into(),
            },
            AgentCommand::WriteFile {
                path: "b.rs".into(),
                content: "fn main() {}".into(),
            },
            AgentCommand::ListDir { path: ".".into() },
            AgentCommand::GitStatus,
            AgentCommand::GitDiff { path: None },
            AgentCommand::GitDiff {
                path: Some("src/main.rs".into()),
            },
            AgentCommand::GitCommit {
                message: "fix bug".into(),
            },
            AgentCommand::GitPush,
            AgentCommand::GitExec {
                args: vec!["log".into(), "-n".into(), "5".into()],
            },
        ];
        for command in commands {
            let msg = ControlMessage::AgentExecRequest {
                session_id: "s".into(),
                request_id: "r".into(),
                root_path: "/workspace".into(),
                docker_container: None,
                command,
            };
            let bytes = msg.serialize().unwrap();
            let decoded: ControlMessage = bincode::deserialize(&bytes[4..]).unwrap();
            assert!(matches!(decoded, ControlMessage::AgentExecRequest { .. }));
        }
    }

    #[test]
    fn test_agent_command_git_exec_roundtrip() {
        // GitExec 是新变体：验证 bincode round-trip（arg 向量原样保留）。
        let args = vec![
            "push".to_string(),
            "--force-with-lease".to_string(),
            "origin".to_string(),
            "main".to_string(),
        ];
        let bytes = bincode::serialize(&AgentCommand::GitExec { args: args.clone() }).unwrap();
        let back: AgentCommand = bincode::deserialize(&bytes).unwrap();
        match back {
            AgentCommand::GitExec { args: got } => assert_eq!(got, args),
            other => panic!("expected GitExec, got {other:?}"),
        }
    }

    #[test]
    fn test_agent_command_search_patch_roundtrip() {
        let cmds = vec![
            AgentCommand::Search {
                pattern: "fn main".into(),
                path: "src".into(),
                include: Some("*.rs".into()),
            },
            AgentCommand::Search {
                pattern: "TODO".into(),
                path: ".".into(),
                include: None,
            },
            AgentCommand::PatchFile {
                path: "src/a.rs".into(),
                old_string: "old".into(),
                new_string: "new".into(),
            },
        ];
        for cmd in cmds {
            let bytes = bincode::serialize(&cmd).unwrap();
            let back: AgentCommand = bincode::deserialize(&bytes).unwrap();
            assert_eq!(format!("{back:?}"), format!("{:?}", cmd));
        }
    }

    #[test]
    fn test_agent_spawn_messages_roundtrip() {
        let req = ControlMessage::AgentSpawnRequest {
            session_id: "sess-1".into(),
            command: "gemini".into(),
            args: vec!["--experimental-acp".into()],
            env: vec![("OPENAI_BASE_URL".into(), "http://127.0.0.1:8080".into())],
            cwd: Some("/home/user/project".into()),
        };
        let encoded = bincode::serialize(&req).unwrap();
        let decoded: ControlMessage = bincode::deserialize(&encoded).unwrap();
        match decoded {
            ControlMessage::AgentSpawnRequest {
                session_id,
                command,
                args,
                env,
                cwd,
            } => {
                assert_eq!(session_id, "sess-1");
                assert_eq!(command, "gemini");
                assert_eq!(args, vec!["--experimental-acp"]);
                assert_eq!(env.len(), 1);
                assert_eq!(cwd.as_deref(), Some("/home/user/project"));
            }
            other => panic!("expected AgentSpawnRequest, got {other:?}"),
        }

        let data = ControlMessage::AgentSpawnData {
            session_id: "sess-1".into(),
            data: b"{\"jsonrpc\":\"2.0\"}".to_vec(),
            stdin: false,
        };
        let encoded = bincode::serialize(&data).unwrap();
        let decoded: ControlMessage = bincode::deserialize(&encoded).unwrap();
        assert!(matches!(
            decoded,
            ControlMessage::AgentSpawnData { stdin: false, .. }
        ));

        let exit = ControlMessage::AgentSpawnExit {
            session_id: "sess-1".into(),
            code: Some(0),
        };
        let encoded = bincode::serialize(&exit).unwrap();
        let decoded: ControlMessage = bincode::deserialize(&encoded).unwrap();
        assert!(matches!(
            decoded,
            ControlMessage::AgentSpawnExit { code: Some(0), .. }
        ));
    }

    #[test]
    fn test_agent_llm_proxy_messages_roundtrip() {
        let req = ControlMessage::AgentLlmProxyRequest {
            request_id: "req-1".into(),
            session_id: "sess-1".into(),
            path: "/v1/chat/completions".into(),
            body: b"{\"model\":\"gpt-4\"}".to_vec(),
        };
        let encoded = bincode::serialize(&req).unwrap();
        let decoded: ControlMessage = bincode::deserialize(&encoded).unwrap();
        match decoded {
            ControlMessage::AgentLlmProxyRequest {
                request_id, path, ..
            } => {
                assert_eq!(request_id, "req-1");
                assert_eq!(path, "/v1/chat/completions");
            }
            other => panic!("expected AgentLlmProxyRequest, got {other:?}"),
        }

        let chunk = ControlMessage::AgentLlmProxyChunk {
            request_id: "req-1".into(),
            data: b"data: {}".to_vec(),
            done: false,
            status: 200,
        };
        let encoded = bincode::serialize(&chunk).unwrap();
        let decoded: ControlMessage = bincode::deserialize(&encoded).unwrap();
        assert!(matches!(
            decoded,
            ControlMessage::AgentLlmProxyChunk {
                done: false,
                status: 200,
                ..
            }
        ));
    }

    #[test]
    fn test_agent_llm_proxy_start_roundtrip() {
        let start = ControlMessage::AgentLlmProxyStart {
            session_id: "s1".into(),
        };
        let encoded = bincode::serialize(&start).unwrap();
        let decoded: ControlMessage = bincode::deserialize(&encoded).unwrap();
        assert!(matches!(decoded, ControlMessage::AgentLlmProxyStart { .. }));

        let ready = ControlMessage::AgentLlmProxyReady {
            session_id: "s1".into(),
            port: 45678,
        };
        let encoded = bincode::serialize(&ready).unwrap();
        let decoded: ControlMessage = bincode::deserialize(&encoded).unwrap();
        match decoded {
            ControlMessage::AgentLlmProxyReady { port, .. } => assert_eq!(port, 45678),
            other => panic!("expected AgentLlmProxyReady, got {other:?}"),
        }

        let stop = ControlMessage::AgentLlmProxyStop {
            session_id: "s1".into(),
        };
        let encoded = bincode::serialize(&stop).unwrap();
        let decoded: ControlMessage = bincode::deserialize(&encoded).unwrap();
        match decoded {
            ControlMessage::AgentLlmProxyStop { session_id } => assert_eq!(session_id, "s1"),
            other => panic!("expected AgentLlmProxyStop, got {other:?}"),
        }
    }
}
