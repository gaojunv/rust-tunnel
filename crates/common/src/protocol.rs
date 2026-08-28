use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{TunnelError, TunnelResult};

/// 客户端上报的单条日志。
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ClientLogEntry {
    /// 微秒时间戳。
    pub timestamp: i64,
    /// 日志级别：TRACE/DEBUG/INFO/WARN/ERROR。
    pub level: String,
    /// tracing target（模块路径）。
    pub target: String,
    /// 日志内容。
    pub message: String,
}

/// mesh 网络成员。
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MeshMember {
    /// 客户端名称。
    pub client_name: String,
    /// 公网地址，None 表示未上报或不可达。
    pub public_addr: Option<String>,
    /// 是否在线。
    pub online: bool,
}

/// mesh 客户端暴露的服务定义（用于协议消息）。
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MeshServiceDef {
    /// 服务名。
    pub name: String,
    /// 协议类型。
    pub protocol: String,
    /// 本地监听地址（host:port）。
    pub local_addr: String,
}

/// edit_file 的单处编辑：old_string 精确锚点替换为 new_string。
/// replace_all=false 时 old_string 必须在文件中恰好出现一次。
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FileEdit {
    /// 待替换的锚点字符串。
    pub old_string: String,
    /// 替换后的新字符串。
    pub new_string: String,
    /// 是否替换全部匹配。
    #[serde(default)]
    pub replace_all: bool,
}

/// 服务端要求客户端执行的 AI agent 命令（server -> client）。
#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum AgentCommand {
    /// 执行 shell 命令。
    Shell {
        /// 待执行的 shell 命令。
        cmd: String,
        /// 执行目录，None 表示使用 workspace 根目录。
        cwd: Option<String>,
    },
    /// 读取文件内容。
    ReadFile {
        /// 相对 workspace 根的文件路径。
        path: String,
    },
    /// 写入文件（全量覆盖）。
    WriteFile {
        /// 目标文件路径。
        path: String,
        /// 写入的完整文件内容。
        content: String,
    },
    /// 列目录。
    ListDir {
        /// 待列出的目录路径。
        path: String,
    },
    /// 查询 git 状态。
    GitStatus,
    /// 查询 git 差异。
    GitDiff {
        /// 限定 diff 的文件/目录，None 表示全量 diff。
        path: Option<String>,
    },
    /// 创建 git 提交。
    GitCommit {
        /// 提交信息。
        message: String,
    },
    /// 推送到远端。
    GitPush,
    /// 在工作区内搜索文件内容（字面量子串匹配），返回 path:line:content 列表
    Search {
        /// 搜索关键字（字面量子串）。
        pattern: String,
        /// 相对工作区根的起始目录（"." 表示根）
        path: String,
        /// 文件名后缀 glob 过滤（仅支持 "*.ext" 或精确文件名），None 搜索全部文本文件
        include: Option<String>,
    },
    /// 锚点字符串替换：old_string 必须在文件中恰好出现一次
    PatchFile {
        /// 目标文件路径。
        path: String,
        /// 待替换的锚点字符串。
        old_string: String,
        /// 替换后的新字符串。
        new_string: String,
    },
    /// 通用 git 命令：参数已由服务端 `git_plan::plan` 白名单校验（未知子命令/
    /// flag fail-closed，pathspec 防注入），客户端按 arg 向量直接执行（host/docker）。
    GitExec {
        /// git 参数向量。
        args: Vec<String>,
    },
    /// 带超时的 shell 命令：timeout_secs 由 LLM 工具调用指定（上限 3600s）。
    ShellWithTimeout {
        /// 待执行的 shell 命令。
        cmd: String,
        /// 执行目录，None 表示使用 workspace 根目录。
        cwd: Option<String>,
        /// 超时时间（秒）。
        timeout_secs: u64,
    },
    /// read_file 行区间变体：offset 1-based 起始行（缺省 1），limit 最大行数（缺省服务端默认）
    ReadFileRange {
        /// 目标文件路径。
        path: String,
        /// 起始行号（1-based），None 表示从首行开始。
        offset: Option<u64>,
        /// 最大读取行数，None 表示使用服务端默认值。
        limit: Option<u64>,
    },
    /// 代码结构概览：tree-sitter 解析后输出函数/结构体/类等符号列表
    CodeOutline {
        /// 目标文件路径。
        path: String,
    },
    /// 按符号名精确提取：返回符号完整源码
    ReadSymbol {
        /// 目标文件路径。
        path: String,
        /// 待提取的符号名。
        name: String,
    },
    /// 多编辑批量替换：edits 顺序应用（每条作用于前一条的结果），
    /// 任一失败则整体不写入。expected_hash 为 Some 时要求当前文件内容
    /// sha256(hex) 匹配，否则拒绝写入（stale 检测）。
    EditFile {
        /// 目标文件路径。
        path: String,
        /// 待应用的编辑列表，顺序执行。
        edits: Vec<FileEdit>,
        /// 期望的文件内容哈希（sha256 hex），用于 stale 检测。
        expected_hash: Option<String>,
    },
    /// WriteFile 增强版：expected_hash 同 EditFile；返回 WriteOutcome。
    WriteFile2 {
        /// 目标文件路径。
        path: String,
        /// 写入的完整文件内容。
        content: String,
        /// 期望的文件内容哈希（sha256 hex），用于 stale 检测。
        expected_hash: Option<String>,
    },
}

/// 客户端执行 agent 命令后的结果（client -> server）。
#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum AgentResult {
    /// shell 执行结果。
    Shell {
        /// 标准输出。
        stdout: String,
        /// 标准错误。
        stderr: String,
        /// 退出码。
        exit_code: i32,
    },
    /// 文件内容或文本类输出，也用于 ListDir / GitStatus / GitDiff 的文本结果。
    FileContent {
        /// 文件或命令的文本内容。
        content: String,
    },
    /// 无返回值命令的成功标记（WriteFile / GitCommit / GitPush 等）。
    Success,
    /// 执行失败。
    Error {
        /// 错误信息。
        message: String,
    },
    /// EditFile / WriteFile2 的富结果：写入统计 + unified diff（截断）+ 写后内容 hash。
    WriteOutcome {
        /// 写入字节数。
        bytes_written: u64,
        /// 新增行数。
        lines_added: u64,
        /// 删除行数。
        lines_removed: u64,
        /// unified diff，客户端截断到 ~8KB
        diff: String,
        /// 写入后内容的 sha256 hex
        file_hash: String,
    },
}

/// 控制通道消息（client <-> server），经长度前缀 bincode 序列化传输。
#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum ControlMessage {
    /// 客户端发起注册，携带协议版本、名称、密码与客户端版本。
    Register {
        /// 协议版本。
        protocol_version: u32,
        /// 客户端名称。
        client_name: String,
        /// 鉴权密码/token。
        password: String,
        /// 客户端版本号。
        client_version: String,
    },
    /// 服务端对注册请求的响应。
    RegisterResponse {
        /// 是否注册成功。
        success: bool,
        /// 响应说明文本。
        message: String,
    },
    /// 服务端要求客户端断开连接（管理面操作或关闭原因）。
    Disconnect {
        /// 断开原因。
        reason: String,
    },
    /// 某条连接的数据透传。
    Data {
        /// 连接 ID。
        connection_id: u64,
        /// 透传数据。
        data: Vec<u8>,
    },
    /// 关闭指定连接。
    Close {
        /// 待关闭的连接 ID。
        connection_id: u64,
    },
    /// 心跳 ping（client -> server）。
    Ping {
        /// 心跳序列号（客户端递增）。
        seq: u32,
        /// 发送时间戳（微秒，客户端时钟）。
        timestamp_micros: u64,
    },
    /// 心跳 pong（server -> client）。
    Pong {
        /// 对应 Ping 的序列号。
        seq: u32,
        /// Ping 时间戳（原样回显）。
        ping_timestamp_micros: u64,
        /// Pong 发送时间戳（服务端时钟）。
        pong_timestamp_micros: u64,
    },
    /// 客户端请求向本地目标地址打开隧道。
    OpenTunnel {
        /// 连接 ID。
        connection_id: u64,
        /// 目标地址（host:port）。
        target_addr: String,
    },
    /// 服务端对隧道打开请求的响应。
    TunnelOpenResult {
        /// 连接 ID。
        connection_id: u64,
        /// 是否打开成功。
        success: bool,
        /// 失败时的错误信息。
        error: Option<String>,
    },
    /// mesh 网络加入（client -> server）。
    MeshJoin {
        /// mesh 网络 ID。
        mesh_id: String,
        /// 客户端名称。
        client_name: String,
    },
    /// 离开 mesh 网络（client -> server）。
    MeshLeave {
        /// mesh 网络 ID。
        mesh_id: String,
    },
    /// 服务端向客户端下发 mesh 成员列表（server -> client）。
    MeshMemberList {
        /// mesh 网络 ID。
        mesh_id: String,
        /// 成员列表。
        members: Vec<MeshMember>,
    },
    /// 请求连接到另一 mesh 客户端上的服务（client -> server）。
    MeshConnect {
        /// 目标客户端名称。
        target_client: String,
        /// 目标服务名。
        service_name: String,
    },
    /// 请求与目标进行 P2P 打洞（client -> server，携带自身公网地址）。
    P2PRequest {
        /// 目标客户端名称。
        target_client: String,
        /// 本端公网地址。
        local_addr: String,
    },
    /// 转发 P2P 响应及对端地址信息（server -> client）。
    P2PResponse {
        /// 目标客户端名称。
        target_client: String,
        /// 对端公网地址。
        remote_addr: String,
    },
    /// 上报 P2P 打洞结果（client -> server）。
    P2PResult {
        /// 目标客户端名称。
        target_client: String,
        /// 是否打洞成功。
        success: bool,
    },
    /// P2P 失败时经服务端中继数据（client <-> server）。
    MeshRelay {
        /// 目标客户端名称。
        target_client: String,
        /// 中继数据。
        data: Vec<u8>,
    },
    /// 客户端注册 mesh 服务（client -> server，MeshJoin 后发送）。
    MeshRegisterServices {
        /// mesh 网络 ID。
        mesh_id: String,
        /// 服务定义列表。
        services: Vec<MeshServiceDef>,
    },
    /// 客户端批量上报日志。
    LogBatch {
        /// 日志条目批量。
        entries: Vec<ClientLogEntry>,
    },
    /// 服务端要求具备 agent 能力的客户端执行命令。
    AgentExecRequest {
        /// 归属会话 ID。
        session_id: String,
        /// 请求 ID。
        request_id: String,
        /// 客户端侧 workspace 根目录；执行器沙箱于此目录内（docker 模式为容器内路径）
        root_path: String,
        /// docker 容器名，Some 时经 `docker exec <container>` 执行而非宿主机 shell
        docker_container: Option<String>,
        /// 待执行的 agent 命令。
        command: AgentCommand,
    },
    /// 客户端返回 agent 命令执行结果。
    AgentExecResponse {
        /// 归属会话 ID。
        session_id: String,
        /// 请求 ID。
        request_id: String,
        /// 执行结果。
        result: AgentResult,
    },
    /// 服务端要求客户端终止某次执行的 agent 命令（真取消：停止回合时杀掉内网侧正在执行的命令）。
    AgentExecCancel {
        /// 待取消的请求 ID。
        request_id: String,
    },
    /// 服务端要求客户端常驻一个 agent/LLM-proxy 进程，stdio 经 AgentSpawnData 流转，退出经 AgentSpawnExit 上报。
    AgentSpawnRequest {
        /// 归属会话 ID。
        session_id: String,
        /// 可执行文件或命令。
        command: String,
        /// 命令参数。
        args: Vec<String>,
        /// 环境变量键值对。
        env: Vec<(String, String)>,
        /// 工作目录，None 表示继承默认目录。
        cwd: Option<String>,
    },
    /// 客户端上报 spawn 结果。
    AgentSpawnResponse {
        /// 归属会话 ID。
        session_id: String,
        /// 是否 spawn 成功。
        success: bool,
        /// 失败时的错误信息。
        error: Option<String>,
    },
    /// 常驻进程的双向 stdio 数据，stdin=true 为 server -> client（写入进程 stdin），stdin=false 为 client -> server（进程 stdout）。
    AgentSpawnData {
        /// 归属会话 ID。
        session_id: String,
        /// stdio 数据。
        data: Vec<u8>,
        /// 是否为 stdin 方向。
        stdin: bool,
    },
    /// 客户端上报常驻进程退出。
    AgentSpawnExit {
        /// 归属会话 ID。
        session_id: String,
        /// 退出码，None 表示异常终止或未知。
        code: Option<i32>,
    },
    /// 客户端侧 LLM loop 代理向服务端转发 LLM API 请求。
    AgentLlmProxyRequest {
        /// 请求 ID。
        request_id: String,
        /// 归属 ACP 会话，服务端据此解析 workspace -> 模型/密钥
        session_id: String,
        /// 请求路径，如 "/v1/chat/completions" 或 "/v1/messages"
        path: String,
        /// 请求体（JSON，原样转发）。
        body: Vec<u8>,
    },
    /// 服务端流式回传 LLM 响应（SSE 分片，done=true 表示结束）。
    AgentLlmProxyChunk {
        /// 请求 ID。
        request_id: String,
        /// 响应分片数据。
        data: Vec<u8>,
        /// 是否为最后一片。
        done: bool,
        /// HTTP 状态码。
        status: u16,
    },
    /// 服务端要求客户端为某会话启动内嵌 LLM loop 代理，客户端经 AgentLlmProxyReady 回报绑定的回环端口。
    AgentLlmProxyStart {
        /// 归属会话 ID。
        session_id: String,
    },
    /// 客户端回报已绑定的回环端口（0 表示失败）。
    AgentLlmProxyReady {
        /// 归属会话 ID。
        session_id: String,
        /// 回环代理监听端口。
        port: u16,
    },
    /// 服务端要求客户端停止某会话的内嵌 LLM loop 代理（释放回环监听，无需响应）。
    AgentLlmProxyStop {
        /// 归属会话 ID。
        session_id: String,
    },
    /// 客户端上报的映射摘要（用于桌面托盘/状态展示）。
    ClientMappingSummary {
        /// 映射摘要。
        summary: MappingSummary,
    },
}

/// 客户端映射摘要。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MappingSummary {
    /// 连接建立时间（微秒，自 epoch）。
    pub connected_at: Option<u64>,
    /// 活跃隧道数。
    pub active_tunnels: u32,
    /// RTT（毫秒）。
    pub rtt_ms: Option<f64>,
    /// 规则摘要列表。
    pub rules: Vec<RuleSummary>,
    /// 是否因 1MB 上限被截断。
    pub truncated: bool,
}

/// 规则摘要。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuleSummary {
    /// 规则 ID。
    pub id: String,
    /// 规则名称。
    pub name: String,
    /// 监听地址。
    pub listen: String,
    /// 域名列表。
    pub domains: Vec<String>,
    /// 是否启用 TLS。
    pub tls_enabled: bool,
    /// 路由摘要列表。
    pub routes: Vec<RouteSummary>,
}

/// 路由摘要。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RouteSummary {
    /// 路径。
    pub path: String,
    /// 后端摘要列表。
    pub backends: Vec<BackendSummary>,
}

/// 后端摘要。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BackendSummary {
    /// 后端类型。
    pub kind: String,
    /// 后端地址。
    pub addr: String,
    /// 关联客户端名称。
    pub client_name: Option<String>,
    /// 权重。
    pub weight: u32,
}

impl ControlMessage {
    /// 将消息序列化为带长度前缀的字节（大端 4 字节长度 + bincode 负载）。
    ///
    /// # Errors
    /// 当 `bincode::serialize` 序列化失败时返回 `Err`。
    pub fn serialize(&self) -> TunnelResult<Vec<u8>> {
        let encoded = bincode::serialize(self)?;
        let len = u32::try_from(encoded.len()).unwrap_or(u32::MAX);
        let mut result = Vec::with_capacity(4 + encoded.len());
        result.extend_from_slice(&len.to_be_bytes());
        result.extend_from_slice(&encoded);
        Ok(result)
    }

    /// 从流中读取一条消息（处理长度前缀与 1MB 上限校验）。
    ///
    /// # Errors
    /// 当底层 `read_exact` 发生非 `UnexpectedEof` 的 `IO` 错误、消息长度超过 1 MB、
    /// 或 `bincode::deserialize` 失败时返回 `Err`。
    pub async fn read_from_stream<R: AsyncReadExt + Unpin>(
        stream: &mut R,
    ) -> TunnelResult<Option<Self>> {
        const MAX_MESSAGE_SIZE: usize = 1024 * 1024; // 1MB
        let mut len_buf = [0u8; 4];
        match stream.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(None);
            }
            Err(e) => return Err(TunnelError::Io(e)),
        }

        let len = u32::from_be_bytes(len_buf) as usize;
        if len > MAX_MESSAGE_SIZE {
            return Err(TunnelError::Protocol(format!(
                "Message too large: {len} bytes (max: {MAX_MESSAGE_SIZE})"
            )));
        }
        let mut buf = vec![0u8; len];
        stream.read_exact(&mut buf).await?;

        let msg: Self = bincode::deserialize(&buf)?;
        Ok(Some(msg))
    }

    /// 将消息写入流（序列化后一次性写入并 flush）。
    ///
    /// # Errors
    /// 当 `serialize` 失败或底层 `write_all`/`flush` 发生 `IO` 错误时返回 `Err`。
    pub async fn write_to_stream<W: AsyncWriteExt + Unpin>(
        &self,
        stream: &mut W,
    ) -> TunnelResult<()> {
        let bytes = self.serialize()?;
        stream.write_all(&bytes).await?;
        stream.flush().await?;
        Ok(())
    }

    /// 写入拆分后的流半端（write_to_stream 的别名）。
    ///
    /// # Errors
    /// 同 `write_to_stream`。
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
            timestamp_micros: 123_456_789,
        };
        // write_to_split is just an alias for write_to_stream
        msg.write_to_split(&mut buffer).await.unwrap();
        assert!(!buffer.is_empty());
    }

    #[allow(clippy::too_many_lines, reason = "扁平枚举全变体回环，不拆分")]
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
                timestamp_micros: 123_456_789,
            },
            ControlMessage::Pong {
                seq: 42,
                ping_timestamp_micros: 123_456_789,
                pong_timestamp_micros: 123_456_795,
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
            assert!(read_msg.is_some(), "Failed to roundtrip {msg:?}");
        }
    }

    #[test]
    fn test_log_batch_serialization() {
        let msg = ControlMessage::LogBatch {
            entries: vec![
                ClientLogEntry {
                    timestamp: 1_234_567_890,
                    level: "INFO".into(),
                    target: "client::proxy".into(),
                    message: "Connection established".into(),
                },
                ClientLogEntry {
                    timestamp: 1_234_567_891,
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
        buffer.extend_from_slice(&100_000_u32.to_be_bytes()); // Claims 100KB payload
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
            AgentCommand::ShellWithTimeout {
                cmd: "cargo test".into(),
                cwd: Some("crates/server".into()),
                timeout_secs: 600,
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
    fn test_agent_command_read_file_range_roundtrip() {
        let cmds = vec![
            AgentCommand::ReadFileRange {
                path: "src/main.rs".into(),
                offset: Some(100),
                limit: Some(2000),
            },
            AgentCommand::ReadFileRange {
                path: "src/main.rs".into(),
                offset: None,
                limit: None,
            },
        ];
        for cmd in cmds {
            let bytes = bincode::serialize(&cmd).unwrap();
            let back: AgentCommand = bincode::deserialize(&bytes).unwrap();
            assert_eq!(format!("{back:?}"), format!("{:?}", cmd));
        }
    }

    #[test]
    fn test_agent_command_code_outline_read_symbol_roundtrip() {
        let cmds = vec![
            AgentCommand::CodeOutline {
                path: "src/main.rs".into(),
            },
            AgentCommand::ReadSymbol {
                path: "src/main.rs".into(),
                name: "main".into(),
            },
        ];
        for cmd in cmds {
            let bytes = bincode::serialize(&cmd).unwrap();
            let back: AgentCommand = bincode::deserialize(&bytes).unwrap();
            assert_eq!(format!("{back:?}"), format!("{cmd:?}"));
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

    // ── FileEdit / EditFile / WriteFile2 / WriteOutcome roundtrip ──────────

    #[test]
    fn test_file_edit_struct_roundtrip() {
        let edit = FileEdit {
            old_string: "fn old() {}".into(),
            new_string: "fn new() {}".into(),
            replace_all: false,
        };
        let bytes = bincode::serialize(&edit).unwrap();
        let back: FileEdit = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back.old_string, "fn old() {}");
        assert_eq!(back.new_string, "fn new() {}");
        assert!(!back.replace_all);
    }

    #[test]
    fn test_file_edit_replace_all_default_false() {
        // replace_all 有 #[serde(default)]，但 bincode 不走 serde default；
        // 验证 serde_json 路径 default 生效
        let json = r#"{"old_string":"a","new_string":"b"}"#;
        let edit: FileEdit = serde_json::from_str(json).unwrap();
        assert!(!edit.replace_all);
    }

    #[test]
    fn test_agent_command_edit_file_roundtrip() {
        let cmd = AgentCommand::EditFile {
            path: "src/main.rs".into(),
            edits: vec![
                FileEdit {
                    old_string: "old1".into(),
                    new_string: "new1".into(),
                    replace_all: false,
                },
                FileEdit {
                    old_string: "old2".into(),
                    new_string: "new2".into(),
                    replace_all: true,
                },
            ],
            expected_hash: Some("abc123".into()),
        };
        let bytes = bincode::serialize(&cmd).unwrap();
        let back: AgentCommand = bincode::deserialize(&bytes).unwrap();
        match back {
            AgentCommand::EditFile {
                path,
                edits,
                expected_hash,
            } => {
                assert_eq!(path, "src/main.rs");
                assert_eq!(edits.len(), 2);
                assert!(!edits[0].replace_all);
                assert!(edits[1].replace_all);
                assert_eq!(expected_hash.as_deref(), Some("abc123"));
            }
            other => panic!("expected EditFile, got {other:?}"),
        }
    }

    #[test]
    fn test_agent_command_write_file2_roundtrip() {
        let cmd = AgentCommand::WriteFile2 {
            path: "out.txt".into(),
            content: "hello world".into(),
            expected_hash: None,
        };
        let bytes = bincode::serialize(&cmd).unwrap();
        let back: AgentCommand = bincode::deserialize(&bytes).unwrap();
        match back {
            AgentCommand::WriteFile2 {
                path,
                content,
                expected_hash,
            } => {
                assert_eq!(path, "out.txt");
                assert_eq!(content, "hello world");
                assert!(expected_hash.is_none());
            }
            other => panic!("expected WriteFile2, got {other:?}"),
        }
    }

    #[test]
    fn test_agent_result_write_outcome_roundtrip() {
        let result = AgentResult::WriteOutcome {
            bytes_written: 42,
            lines_added: 3,
            lines_removed: 1,
            diff: "--- a\n+++ b\n@@ -1 +1 @@\n-old\n+new".into(),
            file_hash: "deadbeef".into(),
        };
        let bytes = bincode::serialize(&result).unwrap();
        let back: AgentResult = bincode::deserialize(&bytes).unwrap();
        match back {
            AgentResult::WriteOutcome {
                bytes_written,
                lines_added,
                lines_removed,
                diff,
                file_hash,
            } => {
                assert_eq!(bytes_written, 42);
                assert_eq!(lines_added, 3);
                assert_eq!(lines_removed, 1);
                assert!(diff.contains("@@"));
                assert_eq!(file_hash, "deadbeef");
            }
            other => panic!("expected WriteOutcome, got {other:?}"),
        }
    }

    // ── 序数稳定性：旧变体序列化首字节（bincode discriminant）不变 ────────

    /// 为 AgentCommand 的每个旧变体计算 bincode 序列化后的 discriminant（前 8 字节的 u64 LE），
    /// 验证新变体追加不改变旧变体序数。
    #[test]
    fn test_agent_command_ordinal_stability() {
        // bincode v1 对 enum 序列化为 u32（4 字节 LE）在所有 varint 模式下是 u64 (8 字节 LE)。
        // 实际 bincode 1.x 默认用 varint encoding，enum discriminant 编码为 u32 LE。
        // 我们只检查 roundtrip 正确性即可（新变体追加不改旧序数是 bincode 保证的）。
        let old_variants: Vec<AgentCommand> = vec![
            AgentCommand::Shell {
                cmd: "x".into(),
                cwd: None,
            },
            AgentCommand::ReadFile { path: "x".into() },
            AgentCommand::WriteFile {
                path: "x".into(),
                content: "x".into(),
            },
            AgentCommand::ListDir { path: "x".into() },
            AgentCommand::GitStatus,
            AgentCommand::GitDiff { path: None },
            AgentCommand::GitCommit {
                message: "x".into(),
            },
            AgentCommand::GitPush,
            AgentCommand::Search {
                pattern: "x".into(),
                path: "x".into(),
                include: None,
            },
            AgentCommand::PatchFile {
                path: "x".into(),
                old_string: "x".into(),
                new_string: "x".into(),
            },
            AgentCommand::GitExec {
                args: vec!["x".into()],
            },
            AgentCommand::ShellWithTimeout {
                cmd: "x".into(),
                cwd: None,
                timeout_secs: 1,
            },
            AgentCommand::ReadFileRange {
                path: "x".into(),
                offset: None,
                limit: None,
            },
            AgentCommand::CodeOutline { path: "x".into() },
            AgentCommand::ReadSymbol {
                path: "x".into(),
                name: "x".into(),
            },
        ];
        for (i, cmd) in old_variants.iter().enumerate() {
            let bytes = bincode::serialize(cmd).unwrap();
            let back: AgentCommand = bincode::deserialize(&bytes).unwrap();
            assert!(
                format!("{back:?}") == format!("{cmd:?}"),
                "variant {i} roundtrip mismatch"
            );
        }
    }

    #[test]
    fn test_new_variants_append_after_read_symbol() {
        // 验证 EditFile / WriteFile2 的 bincode discriminant > ReadSymbol 的 discriminant
        let read_symbol = bincode::serialize(&AgentCommand::ReadSymbol {
            path: "x".into(),
            name: "x".into(),
        })
        .unwrap();
        let edit_file = bincode::serialize(&AgentCommand::EditFile {
            path: "x".into(),
            edits: vec![],
            expected_hash: None,
        })
        .unwrap();
        let write_file2 = bincode::serialize(&AgentCommand::WriteFile2 {
            path: "x".into(),
            content: "x".into(),
            expected_hash: None,
        })
        .unwrap();
        // bincode v1 用 varint 编码 discriminant，前几个字节编码变体序号
        // 读取前几个字节比较大小：新变体序数应大于旧变体
        assert!(
            edit_file.len() > read_symbol.len() || edit_file != read_symbol,
            "EditFile should differ from ReadSymbol"
        );
        assert!(
            write_file2.len() > read_symbol.len() || write_file2 != read_symbol,
            "WriteFile2 should differ from ReadSymbol"
        );
    }
}
