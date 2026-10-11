//! RustDesk 控制消息的 protobuf 定义。
//!
//! 这里的结构体**手写** `prost` derive，而不是用 `build.rs` + `protoc` 生成：
//! 仓库因此不需要引入 `protoc` 构建依赖，CI 的 musl 静态构建面不变。互操作只依赖
//! **字段号与线上类型**，与结构体名、字段名、注释无关——所以本文件可以自由地按本
//! 项目风格组织，但每个 `tag` 都必须与上游 `rendezvous.proto` 严格一致，改动前
//! 请先想清楚线上兼容性。
//!
//! 未实现的字段一律保留声明并默认留空：proto3 对空值不产生字节，因此「声明了但
//! 不填」对老客户端与上游服务端都是安全的。

/// 重新导出 `prost::Message` trait：调用方 `use` 它之后即可对下面的消息调用
/// `encode_to_vec` / `decode`。
pub use prost::Message as ProstMessage;

/// 注册/心跳：客户端声明自己的 ID 与序号。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RegisterPeer {
    /// 客户端 ID（RustDesk 里是数字 ID 或自定义别名）。
    #[prost(string, tag = "1")]
    pub id: ::prost::alloc::string::String,
    /// 客户端本地维护的自增序号，服务端用于判断是否需要下发配置更新。
    #[prost(int32, tag = "2")]
    pub serial: i32,
}

/// 注册/心跳的应答。`request_pk` 为真时客户端应重新上报公钥。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RegisterPeerResponse {
    /// 注意：字段号是 2（历史遗留，1 已被占用/废弃），不是 1。
    #[prost(bool, tag = "2")]
    pub request_pk: bool,
}

/// 连接用途分类。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
#[repr(i32)]
pub enum ConnType {
    /// 默认的远程桌面控制连接。
    DefaultConn = 0,
    /// 文件传输。
    FileTransfer = 1,
    /// 端口转发。
    PortForward = 2,
    /// RDP 会话。
    Rdp = 3,
    /// 摄像头查看。
    ViewCamera = 4,
    /// 终端。
    Terminal = 5,
}

/// 客户端的 NAT 类型（由 NAT 类型探测端口判定）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
#[repr(i32)]
pub enum NatType {
    /// 未知/未判定。
    UnknownNat = 0,
    /// 非对称 NAT：打洞通常可行。
    Asymmetric = 1,
    /// 对称 NAT：打洞通常失败，需要中继。
    Symmetric = 2,
}

/// A 端发起的打洞请求。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PunchHoleRequest {
    /// 目标（被控端）ID。
    #[prost(string, tag = "1")]
    pub id: ::prost::alloc::string::String,
    /// 请求方的 NAT 类型。
    #[prost(enumeration = "NatType", tag = "2")]
    pub nat_type: i32,
    /// 服务端密钥；与配置的 key 明文比对（设备认证的全部）。
    #[prost(string, tag = "3")]
    pub licence_key: ::prost::alloc::string::String,
    /// 连接用途。
    #[prost(enumeration = "ConnType", tag = "4")]
    pub conn_type: i32,
    /// 可选的访问令牌。
    #[prost(string, tag = "5")]
    pub token: ::prost::alloc::string::String,
    /// 客户端版本号。
    #[prost(string, tag = "6")]
    pub version: ::prost::alloc::string::String,
    /// 客户端本地 UDP 端口，供对端打洞。
    #[prost(int32, tag = "7")]
    pub udp_port: i32,
    /// 强制走中继（跳过打洞）。
    #[prost(bool, tag = "8")]
    pub force_relay: bool,
    /// UPnP 映射端口。
    #[prost(int32, tag = "9")]
    pub upnp_port: i32,
    /// IPv6 套接字地址。
    #[prost(bytes = "vec", tag = "10")]
    pub socket_addr_v6: Vec<u8>,
    /// 交换机/多路复用码。
    #[prost(string, tag = "11")]
    pub switch_code: ::prost::alloc::string::String,
    /// WebRTC SDP offer；本实现不启 WebRTC 通道，仅透传声明。
    #[prost(string, tag = "12")]
    pub webrtc_sdp_offer: ::prost::alloc::string::String,
}

/// 控制权限位图（`permissions` 是各权限位或运算的结果）。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ControlPermissions {
    /// 权限位图。
    #[prost(uint64, tag = "1")]
    pub permissions: u64,
}

/// 控制权限的单个位定义。
pub mod control_permissions {
    /// 权限位序号（序号即 `ControlPermissions::permissions` 中的位下标）。
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
    #[repr(i32)]
    pub enum Permission {
        /// 键盘输入。
        Keyboard = 0,
        /// 远程打印。
        RemotePrinter = 1,
        /// 剪贴板。
        Clipboard = 2,
        /// 文件传输。
        File = 3,
        /// 音频。
        Audio = 4,
        /// 摄像头。
        Camera = 5,
        /// 终端。
        Terminal = 6,
        /// 隧道。
        Tunnel = 7,
        /// 重启。
        Restart = 8,
        /// 录屏。
        Recording = 9,
        /// 阻断输入。
        BlockInput = 10,
        /// 远程修改。
        RemoteModify = 11,
        /// 隐私模式。
        PrivacyMode = 12,
    }
}

/// 受控端上下文（审计关联）。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ControlledContext {
    /// 连接审计引用号。
    #[prost(string, tag = "1")]
    pub conn_audit_ref: ::prost::alloc::string::String,
}

/// 服务端下发给被控端（B 端）的打洞通知。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PunchHole {
    /// 请求方（A 端）的套接字地址，已编码。
    #[prost(bytes = "vec", tag = "1")]
    pub socket_addr: Vec<u8>,
    /// 建议使用的中继服务器地址。
    #[prost(string, tag = "2")]
    pub relay_server: ::prost::alloc::string::String,
    /// 请求方 NAT 类型。
    #[prost(enumeration = "NatType", tag = "3")]
    pub nat_type: i32,
    /// 请求方本地 UDP 端口。
    #[prost(int32, tag = "4")]
    pub udp_port: i32,
    /// 强制中继。
    #[prost(bool, tag = "5")]
    pub force_relay: bool,
    /// UPnP 端口。
    #[prost(int32, tag = "6")]
    pub upnp_port: i32,
    /// 请求方 IPv6 地址。
    #[prost(bytes = "vec", tag = "7")]
    pub socket_addr_v6: Vec<u8>,
    /// 控制权限。
    #[prost(message, optional, tag = "8")]
    pub control_permissions: ::core::option::Option<ControlPermissions>,
    /// 审计上下文。
    #[prost(message, optional, tag = "9")]
    pub controlled_context: ::core::option::Option<ControlledContext>,
    /// WebRTC SDP offer 透传。
    #[prost(string, tag = "10")]
    pub webrtc_sdp_offer: ::prost::alloc::string::String,
}

/// NAT 类型探测请求（连到主端口 - 1 的专用端口）。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct TestNatRequest {
    /// 请求序号，原样回显。
    #[prost(int32, tag = "1")]
    pub serial: i32,
}

/// NAT 类型探测应答：客户端比较请求端口与应答端口是否一致。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct TestNatResponse {
    /// 服务端观察到的客户端源端口。
    #[prost(int32, tag = "1")]
    pub port: i32,
    /// 配置更新（移动端使用）。
    #[prost(message, optional, tag = "2")]
    pub cu: ::core::option::Option<ConfigUpdate>,
}

/// 被控端（B 端）回给服务端的打洞结果，由服务端转交给请求方。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PunchHoleSent {
    /// 被控端的套接字地址，已编码。
    #[prost(bytes = "vec", tag = "1")]
    pub socket_addr: Vec<u8>,
    /// 被控端 ID。
    #[prost(string, tag = "2")]
    pub id: ::prost::alloc::string::String,
    /// 被控端建议的中继服务器。
    #[prost(string, tag = "3")]
    pub relay_server: ::prost::alloc::string::String,
    /// 被控端 NAT 类型。
    #[prost(enumeration = "NatType", tag = "4")]
    pub nat_type: i32,
    /// 被控端版本号。
    #[prost(string, tag = "5")]
    pub version: ::prost::alloc::string::String,
    /// UPnP 端口。
    #[prost(int32, tag = "6")]
    pub upnp_port: i32,
    /// IPv6 地址。
    #[prost(bytes = "vec", tag = "7")]
    pub socket_addr_v6: Vec<u8>,
    /// WebRTC SDP answer 透传。
    #[prost(string, tag = "8")]
    pub webrtc_sdp_answer: ::prost::alloc::string::String,
}

/// 上报设备公钥（首次连接或公钥变更时）。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RegisterPk {
    /// 客户端 ID。
    #[prost(string, tag = "1")]
    pub id: ::prost::alloc::string::String,
    /// 设备唯一标识，服务端据此拒绝同 ID 换设备。
    #[prost(bytes = "vec", tag = "2")]
    pub uuid: Vec<u8>,
    /// 设备公钥。
    #[prost(bytes = "vec", tag = "3")]
    pub pk: Vec<u8>,
    /// 重命名前的旧 ID。
    #[prost(string, tag = "4")]
    pub old_id: ::prost::alloc::string::String,
    /// 仅上报公钥、不注册为设备。
    #[prost(bool, tag = "5")]
    pub no_register_device: bool,
}

/// 公钥上报的应答。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RegisterPkResponse {
    /// 处理结果。
    #[prost(enumeration = "register_pk_response::Result", tag = "1")]
    pub result: i32,
    /// 建议的心跳间隔（秒）。
    #[prost(int32, tag = "2")]
    pub keep_alive: i32,
}

/// [`RegisterPkResponse`] 的结果枚举。
pub mod register_pk_response {
    /// 公钥上报的处理结果。
    ///
    /// 数值与上游一致且**不连续**（缺 1，从 2 起）：这些值直接上线，不可重排。
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
    #[repr(i32)]
    pub enum Result {
        /// 成功。
        Ok = 0,
        /// UUID 不匹配：该 ID 已绑定其它设备。
        UuidMismatch = 2,
        /// ID 已存在。
        IdExists = 3,
        /// 上报过于频繁。
        TooFrequent = 4,
        /// ID 格式非法。
        InvalidIdFormat = 5,
        /// 不支持。
        NotSupport = 6,
        /// 服务端内部错误。
        ServerError = 7,
        /// 尚未部署。
        NotDeployed = 8,
    }
}

/// 服务端对打洞请求的应答。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PunchHoleResponse {
    /// 目标端地址，已编码；打洞成功前为空。
    #[prost(bytes = "vec", tag = "1")]
    pub socket_addr: Vec<u8>,
    /// 目标端公钥（客户端据此校验对端身份）。
    #[prost(bytes = "vec", tag = "2")]
    pub pk: Vec<u8>,
    /// 失败原因。
    #[prost(enumeration = "punch_hole_response::Failure", tag = "3")]
    pub failure: i32,
    /// 建议的中继服务器。
    #[prost(string, tag = "4")]
    pub relay_server: ::prost::alloc::string::String,
    /// 目标端 NAT 类型，或「双方在同一内网」标记。
    #[prost(oneof = "punch_hole_response::Union", tags = "5, 6")]
    pub union: ::core::option::Option<punch_hole_response::Union>,
    /// 其它失败原因的自由文本。
    #[prost(string, tag = "7")]
    pub other_failure: ::prost::alloc::string::String,
    /// 打洞反馈。
    #[prost(int32, tag = "8")]
    pub feedback: i32,
    /// 目标端是否走 UDP。
    #[prost(bool, tag = "9")]
    pub is_udp: bool,
    /// UPnP 端口。
    #[prost(int32, tag = "10")]
    pub upnp_port: i32,
    /// IPv6 地址。
    #[prost(bytes = "vec", tag = "11")]
    pub socket_addr_v6: Vec<u8>,
    /// WebRTC SDP answer 透传。
    #[prost(string, tag = "12")]
    pub webrtc_sdp_answer: ::prost::alloc::string::String,
}

/// [`PunchHoleResponse`] 的 oneof 分支与失败枚举。
pub mod punch_hole_response {
    /// 应答携带的互斥信息：NAT 类型或同内网标记。
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Union {
        /// 目标端 NAT 类型。
        #[prost(enumeration = "super::NatType", tag = "5")]
        NatType(i32),
        /// 双方处于同一内网（应改用本地地址直连）。
        #[prost(bool, tag = "6")]
        IsLocal(bool),
    }

    /// 打洞失败原因。
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
    #[repr(i32)]
    pub enum Failure {
        /// 目标 ID 不存在。
        IdNotExist = 0,
        /// 目标离线。
        Offline = 2,
        /// 许可证不匹配（key 错误）。
        LicenseMismatch = 3,
        /// 许可证超额。
        LicenseOveruse = 4,
    }
}

/// 请求本地地址：同内网场景下打洞不可用，改由服务端回传双方内网地址。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct FetchLocalAddr {
    /// 请求方套接字地址。
    #[prost(bytes = "vec", tag = "1")]
    pub socket_addr: Vec<u8>,
    /// 中继服务器。
    #[prost(string, tag = "2")]
    pub relay_server: ::prost::alloc::string::String,
    /// IPv6 地址。
    #[prost(bytes = "vec", tag = "3")]
    pub socket_addr_v6: Vec<u8>,
    /// 控制权限。
    #[prost(message, optional, tag = "4")]
    pub control_permissions: ::core::option::Option<ControlPermissions>,
    /// 审计上下文。
    #[prost(message, optional, tag = "5")]
    pub controlled_context: ::core::option::Option<ControlledContext>,
}

/// 本地地址应答。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct LocalAddr {
    /// 请求方自己的公网地址。
    #[prost(bytes = "vec", tag = "1")]
    pub socket_addr: Vec<u8>,
    /// 请求方的内网地址。
    #[prost(bytes = "vec", tag = "2")]
    pub local_addr: Vec<u8>,
    /// 中继服务器。
    #[prost(string, tag = "3")]
    pub relay_server: ::prost::alloc::string::String,
    /// 对端 ID。
    #[prost(string, tag = "4")]
    pub id: ::prost::alloc::string::String,
    /// 对端版本。
    #[prost(string, tag = "5")]
    pub version: ::prost::alloc::string::String,
    /// IPv6 地址。
    #[prost(bytes = "vec", tag = "6")]
    pub socket_addr_v6: Vec<u8>,
}

/// 服务端下发的配置更新。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ConfigUpdate {
    /// 配置序号。
    #[prost(int32, tag = "1")]
    pub serial: i32,
    /// 汇合服务器列表。
    #[prost(string, repeated, tag = "2")]
    pub rendezvous_servers: ::prost::alloc::vec::Vec<::prost::alloc::string::String>,
}

/// 向中继服务器申请配对（中继数据面的唯一控制消息）。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RequestRelay {
    /// 本端 ID。
    #[prost(string, tag = "1")]
    pub id: ::prost::alloc::string::String,
    /// 配对令牌：两端以相同 `uuid` 相认。
    #[prost(string, tag = "2")]
    pub uuid: ::prost::alloc::string::String,
    /// 本端套接字地址。
    #[prost(bytes = "vec", tag = "3")]
    pub socket_addr: Vec<u8>,
    /// 中继服务器地址。
    #[prost(string, tag = "4")]
    pub relay_server: ::prost::alloc::string::String,
    /// 是否加密。
    #[prost(bool, tag = "5")]
    pub secure: bool,
    /// 服务端密钥，明文比对。
    #[prost(string, tag = "6")]
    pub licence_key: ::prost::alloc::string::String,
    /// 连接用途。
    #[prost(enumeration = "ConnType", tag = "7")]
    pub conn_type: i32,
    /// 访问令牌。
    #[prost(string, tag = "8")]
    pub token: ::prost::alloc::string::String,
    /// 控制权限。
    #[prost(message, optional, tag = "9")]
    pub control_permissions: ::core::option::Option<ControlPermissions>,
    /// 审计上下文。
    #[prost(message, optional, tag = "10")]
    pub controlled_context: ::core::option::Option<ControlledContext>,
    /// 交换机码。
    #[prost(string, tag = "11")]
    pub switch_code: ::prost::alloc::string::String,
}

/// 中继服务器的配对应答。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RelayResponse {
    /// 对端地址。
    #[prost(bytes = "vec", tag = "1")]
    pub socket_addr: Vec<u8>,
    /// 配对令牌。
    #[prost(string, tag = "2")]
    pub uuid: ::prost::alloc::string::String,
    /// 中继服务器。
    #[prost(string, tag = "3")]
    pub relay_server: ::prost::alloc::string::String,
    /// 对端 ID 或对端公钥。
    #[prost(oneof = "relay_response::Union", tags = "4, 5")]
    pub union: ::core::option::Option<relay_response::Union>,
    /// 拒绝原因。
    #[prost(string, tag = "6")]
    pub refuse_reason: ::prost::alloc::string::String,
    /// 对端版本。
    #[prost(string, tag = "7")]
    pub version: ::prost::alloc::string::String,
    /// 反馈值。
    #[prost(int32, tag = "9")]
    pub feedback: i32,
    /// IPv6 地址。
    #[prost(bytes = "vec", tag = "10")]
    pub socket_addr_v6: Vec<u8>,
    /// UPnP 端口。
    #[prost(int32, tag = "11")]
    pub upnp_port: i32,
    /// WebRTC SDP answer 透传。
    #[prost(string, tag = "12")]
    pub webrtc_sdp_answer: ::prost::alloc::string::String,
}

/// [`RelayResponse`] 的 oneof 分支。
pub mod relay_response {
    /// 配对成功时携带的对端标识。
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Union {
        /// 对端 ID。
        #[prost(string, tag = "4")]
        Id(::prost::alloc::string::String),
        /// 对端公钥。
        #[prost(bytes, tag = "5")]
        Pk(::prost::alloc::vec::Vec<u8>),
    }
}

/// 软件更新提示。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct SoftwareUpdate {
    /// 下载地址。
    #[prost(string, tag = "1")]
    pub url: ::prost::alloc::string::String,
}

/// 局域网设备发现广播。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PeerDiscovery {
    /// 子命令。
    #[prost(string, tag = "1")]
    pub cmd: ::prost::alloc::string::String,
    /// MAC 地址。
    #[prost(string, tag = "2")]
    pub mac: ::prost::alloc::string::String,
    /// 设备 ID。
    #[prost(string, tag = "3")]
    pub id: ::prost::alloc::string::String,
    /// 用户名。
    #[prost(string, tag = "4")]
    pub username: ::prost::alloc::string::String,
    /// 主机名。
    #[prost(string, tag = "5")]
    pub hostname: ::prost::alloc::string::String,
    /// 平台。
    #[prost(string, tag = "6")]
    pub platform: ::prost::alloc::string::String,
    /// 其它信息。
    #[prost(string, tag = "7")]
    pub misc: ::prost::alloc::string::String,
}

/// 批量查询在线状态。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct OnlineRequest {
    /// 查询方 ID。
    #[prost(string, tag = "1")]
    pub id: ::prost::alloc::string::String,
    /// 待查询的 ID 列表。
    #[prost(string, repeated, tag = "2")]
    pub peers: ::prost::alloc::vec::Vec<::prost::alloc::string::String>,
}

/// 在线状态应答：`states` 是每台设备 1 字节的紧凑位图，**顺序与
/// [`OnlineRequest::peers`] 一一对应**，每个字节 bit0 表示在线。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct OnlineResponse {
    /// 紧凑状态位图。
    #[prost(bytes = "vec", tag = "1")]
    pub states: Vec<u8>,
}

/// 密钥交换（本实现只做声明与最低限度应答，不启用量子/前向安全通道）。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct KeyExchange {
    /// 公钥列表。
    #[prost(bytes = "vec", repeated, tag = "1")]
    pub keys: ::prost::alloc::vec::Vec<::prost::alloc::vec::Vec<u8>>,
    /// 密钥交换版本；0 表示最原始方案。
    #[prost(uint32, tag = "2")]
    pub version: u32,
    /// 签名参数。
    #[prost(bytes = "vec", tag = "3")]
    pub signed_params: Vec<u8>,
}

/// 服务端对被签名参数的回显结构。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct KxParams {
    /// 公钥。
    #[prost(bytes = "vec", tag = "1")]
    pub pk: Vec<u8>,
    /// 密钥交换版本。
    #[prost(uint32, tag = "2")]
    pub version: u32,
}

/// 服务端健康检查。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct HealthCheck {
    /// 校验令牌。
    #[prost(string, tag = "1")]
    pub token: ::prost::alloc::string::String,
}

/// HTTP 代理请求头。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct HeaderEntry {
    /// 头名。
    #[prost(string, tag = "1")]
    pub name: ::prost::alloc::string::String,
    /// 头值。
    #[prost(string, tag = "2")]
    pub value: ::prost::alloc::string::String,
}

/// HTTP 代理请求。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct HttpProxyRequest {
    /// 方法。
    #[prost(string, tag = "1")]
    pub method: ::prost::alloc::string::String,
    /// 路径。
    #[prost(string, tag = "2")]
    pub path: ::prost::alloc::string::String,
    /// 请求头。
    #[prost(message, repeated, tag = "3")]
    pub headers: ::prost::alloc::vec::Vec<HeaderEntry>,
    /// 请求体。
    #[prost(bytes = "vec", tag = "4")]
    pub body: Vec<u8>,
}

/// HTTP 代理应答。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct HttpProxyResponse {
    /// 状态码。
    #[prost(int32, tag = "1")]
    pub status: i32,
    /// 响应头。
    #[prost(message, repeated, tag = "2")]
    pub headers: ::prost::alloc::vec::Vec<HeaderEntry>,
    /// 响应体。
    #[prost(bytes = "vec", tag = "3")]
    pub body: Vec<u8>,
    /// 错误信息。
    #[prost(string, tag = "4")]
    pub error: ::prost::alloc::string::String,
}

/// WebRTC ICE 候选（本实现不透传 WebRTC 通道）。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct IceCandidate {
    /// 会话 ID。
    #[prost(string, tag = "1")]
    pub id: ::prost::alloc::string::String,
    /// 套接字地址。
    #[prost(bytes = "vec", tag = "2")]
    pub socket_addr: Vec<u8>,
    /// 会话密钥。
    #[prost(string, tag = "3")]
    pub session_key: ::prost::alloc::string::String,
    /// 候选字符串。
    #[prost(string, tag = "4")]
    pub candidate: ::prost::alloc::string::String,
}

/// 服务端签名的 `ID ‖ 公钥` 绑定。
///
/// 服务端密钥（`--key`）的唯一用途就是对这个结构签名：客户端拿到签名后能确认
/// 「这个 ID 确实对应这把公钥」，从而防止中继/汇合服务端替换对端公钥。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct IdPk {
    /// 客户端 ID。
    #[prost(string, tag = "1")]
    pub id: ::prost::alloc::string::String,
    /// 客户端公钥。
    #[prost(bytes = "vec", tag = "2")]
    pub pk: Vec<u8>,
    /// WebRTC DTLS 指纹；非 WebRTC 握手时为空。
    #[prost(string, tag = "3")]
    pub dtls_fingerprint: ::prost::alloc::string::String,
    /// 签名方支持的最新密钥交换版本；0 表示最原始方案。
    #[prost(uint32, tag = "4")]
    pub kx_version: u32,
}

/// 汇合/中继服务端的统一控制消息信封。
///
/// 线上的每一个控制帧都是这个 oneof 的 protobuf 编码，外面套 [`crate::frame`]
/// 的长度前缀。**字段号 1-5 在此协议中从未使用**，所以标签从 6 起——这是历史
/// 遗留，不是笔误。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RendezvousMessage {
    /// 消息体。
    #[prost(oneof = "rendezvous_message::Union", tags = "6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29")]
    pub union: ::core::option::Option<rendezvous_message::Union>,
}

/// [`RendezvousMessage`] 的消息体枚举。
pub mod rendezvous_message {
    /// 所有可用的控制消息变体。
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Union {
        /// 注册/心跳。
        #[prost(message, tag = "6")]
        RegisterPeer(super::RegisterPeer),
        /// 注册/心跳应答。
        #[prost(message, tag = "7")]
        RegisterPeerResponse(super::RegisterPeerResponse),
        /// 打洞请求（A → 服务端）。
        #[prost(message, tag = "8")]
        PunchHoleRequest(super::PunchHoleRequest),
        /// 打洞通知（服务端 → B）。
        #[prost(message, tag = "9")]
        PunchHole(super::PunchHole),
        /// 打洞结果（B → 服务端）。
        #[prost(message, tag = "10")]
        PunchHoleSent(super::PunchHoleSent),
        /// 打洞应答（服务端 → A）。
        #[prost(message, tag = "11")]
        PunchHoleResponse(super::PunchHoleResponse),
        /// 请求本地地址。
        #[prost(message, tag = "12")]
        FetchLocalAddr(super::FetchLocalAddr),
        /// 本地地址应答。
        #[prost(message, tag = "13")]
        LocalAddr(super::LocalAddr),
        /// 配置更新。
        #[prost(message, tag = "14")]
        ConfigureUpdate(super::ConfigUpdate),
        /// 公钥上报。
        #[prost(message, tag = "15")]
        RegisterPk(super::RegisterPk),
        /// 公钥上报应答。
        #[prost(message, tag = "16")]
        RegisterPkResponse(super::RegisterPkResponse),
        /// 软件更新提示。
        #[prost(message, tag = "17")]
        SoftwareUpdate(super::SoftwareUpdate),
        /// 中继配对请求。
        #[prost(message, tag = "18")]
        RequestRelay(super::RequestRelay),
        /// 中继配对应答。
        #[prost(message, tag = "19")]
        RelayResponse(super::RelayResponse),
        /// NAT 类型探测请求。
        #[prost(message, tag = "20")]
        TestNatRequest(super::TestNatRequest),
        /// NAT 类型探测应答。
        #[prost(message, tag = "21")]
        TestNatResponse(super::TestNatResponse),
        /// 局域网发现。
        #[prost(message, tag = "22")]
        PeerDiscovery(super::PeerDiscovery),
        /// 在线状态查询。
        #[prost(message, tag = "23")]
        OnlineRequest(super::OnlineRequest),
        /// 在线状态应答。
        #[prost(message, tag = "24")]
        OnlineResponse(super::OnlineResponse),
        /// 密钥交换。
        #[prost(message, tag = "25")]
        KeyExchange(super::KeyExchange),
        /// 健康检查。
        #[prost(message, tag = "26")]
        Hc(super::HealthCheck),
        /// HTTP 代理请求。
        #[prost(message, tag = "27")]
        HttpProxyRequest(super::HttpProxyRequest),
        /// HTTP 代理应答。
        #[prost(message, tag = "28")]
        HttpProxyResponse(super::HttpProxyResponse),
        /// ICE 候选。
        #[prost(message, tag = "29")]
        IceCandidate(super::IceCandidate),
    }
}

/// 从变体构造 [`RendezvousMessage`] 的便捷构造器。
impl RendezvousMessage {
    /// 用单个消息体包装成信封。
    #[must_use]
    pub fn with(union: rendezvous_message::Union) -> Self {
        Self { union: Some(union) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 编码 → 解码往返，确认消息可用。
    fn roundtrip(msg: &RendezvousMessage) -> RendezvousMessage {
        let bytes = msg.encode_to_vec();
        RendezvousMessage::decode(&bytes[..]).expect("解码应成功")
    }

    #[test]
    fn register_peer_uses_expected_tags() {
        let msg = RendezvousMessage::with(rendezvous_message::Union::RegisterPeer(RegisterPeer {
            id: "123456789".into(),
            serial: 7,
        }));
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn rendezvous_oneof_tags_start_at_six() {
        // 信封只含一个变体：编码必然是「1 字节 tag + len」，tag = 6<<3|2 = 0x32。
        let msg = RendezvousMessage::with(rendezvous_message::Union::RegisterPeer(RegisterPeer {
            id: String::new(),
            serial: 0,
        }));
        let bytes = msg.encode_to_vec();
        assert_eq!(bytes[0], 0x32, "RegisterPeer 的 tag 应为 6");
    }

    #[test]
    fn register_peer_response_uses_tag_two() {
        // request_pk 字段号是 2 而非 1，这点容易写错且有互操作后果。
        let msg = RendezvousMessage::with(rendezvous_message::Union::RegisterPeerResponse(
            RegisterPeerResponse { request_pk: true },
        ));
        let inner = RegisterPeerResponse { request_pk: true }.encode_to_vec();
        assert_eq!(inner, vec![0x10, 0x01], "request_pk 应为 tag 2 varint 1");
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn punch_hole_response_union_is_exclusive() {
        let nat = PunchHoleResponse {
            union: Some(punch_hole_response::Union::NatType(NatType::Symmetric as i32)),
            ..Default::default()
        };
        let local = PunchHoleResponse {
            union: Some(punch_hole_response::Union::IsLocal(true)),
            ..Default::default()
        };
        assert_ne!(nat.encode_to_vec(), local.encode_to_vec());
        assert_eq!(
            PunchHoleResponse::decode(&nat.encode_to_vec()[..]).unwrap(),
            nat
        );
    }

    #[test]
    fn relay_response_carries_id_or_pk() {
        let with_id = RelayResponse {
            uuid: "u-1".into(),
            union: Some(relay_response::Union::Id("654321".into())),
            ..Default::default()
        };
        let decoded = RelayResponse::decode(&with_id.encode_to_vec()[..]).unwrap();
        assert_eq!(decoded, with_id);
    }

    #[test]
    fn request_relay_roundtrips_all_server_fields() {
        let msg = RendezvousMessage::with(rendezvous_message::Union::RequestRelay(RequestRelay {
            id: "abc".into(),
            uuid: "pair-1".into(),
            licence_key: "server-key".into(),
            conn_type: ConnType::DefaultConn as i32,
            ..Default::default()
        }));
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn id_pk_encoding_omits_empty_optionals() {
        // 签名前会被序列化：空的可选字段在 proto3 下不产生字节，
        // 因此 id/pk 的编码是稳定的，签名可复现。
        let id_pk = IdPk {
            id: "42".into(),
            pk: vec![1, 2, 3],
            ..Default::default()
        };
        let bytes = id_pk.encode_to_vec();
        // tag1(0x0A) len2 "42" | tag2(0x12) len3 01 02 03
        assert_eq!(
            bytes,
            vec![0x0A, 0x02, b'4', b'2', 0x12, 0x03, 0x01, 0x02, 0x03]
        );
    }

    #[test]
    fn register_pk_response_result_values_are_non_contiguous() {
        // 结果枚举缺 1、从 2 起；改错会让客户端把「UUID 不匹配」误读。
        assert_eq!(register_pk_response::Result::Ok as i32, 0);
        assert_eq!(register_pk_response::Result::UuidMismatch as i32, 2);
        assert_eq!(register_pk_response::Result::ServerError as i32, 7);
    }

    #[test]
    fn online_response_states_bitmap_roundtrips() {
        let msg = RendezvousMessage::with(rendezvous_message::Union::OnlineResponse(
            OnlineResponse {
                states: vec![1, 0, 1],
            },
        ));
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn frames_carry_rendezvous_messages() {
        // 信封 + 帧前缀的组合是真实线上的最小单元。
        let msg = RendezvousMessage::with(rendezvous_message::Union::TestNatRequest(
            TestNatRequest { serial: 3 },
        ));
        let wire = crate::frame::frame(&msg.encode_to_vec()).unwrap();
        let mut buf = bytes::BytesMut::from(&wire[..]);
        let payload = crate::frame::decode_header(&mut buf).unwrap().unwrap();
        assert_eq!(&buf[..payload], &msg.encode_to_vec()[..]);
    }
}
