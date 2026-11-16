//! 会话数据面协议：帧编解码 + 握手消息 + 保活不变量。
//!
//! # 帧格式
//!
//! ```text
//! ┌──────────────┬────────┬───────────────────────────────┐
//! │ u32 大端长度  │ u8 lane│ 载荷（bincode / AEAD 密文）      │
//! └──────────────┴────────┴───────────────────────────────┘
//! ```
//!
//! 长度字段 = `1 (lane) + 载荷长度`。载荷在未加密时为 [`SessionMessage`] 的 bincode 编码，
//! 装上 [`crate::crypto::Cipher`] 后为「密文 ‖ tag」。`lane` 保持明文，为阶段 2 的多路复用
//! 留出「不解密即可路由」的能力。
//!
//! 上限 [`MAX_SESSION_FRAME`]（16 MiB）：**不复用**控制通道的 1 MiB 上限。VP9 的关键帧远小于
//! 此值，但文件块（`File` lane）与将来可能的更大分辨率仍需要它。
//!
//! # 保活与延迟探测
//!
//! [`HEARTBEAT_INTERVAL`] = 1s，硬性小于 hbbr 中继的 `idle_timeout`（30s，见
//! `crates/rustdesk/src/relay.rs`）。**静止画面也必须发心跳**，否则中继按空闲断开。
//!
//! 心跳同时充当**延迟探测**：1s 一次往返，RTT 读数最快也就 1s 一个样本，足够 QoS 闭环按秒
//! 决策。可信上限是 [`TRUSTED_RTT_MAX`]（10s）——超过它的读数只可能是迟到 ack 与新的心跳
//! 配错了对，或对端卡了整整几个周期，一律丢弃（见 [`trusted_rtt`]）。

use crate::auth::handshake_mac;
use crate::crypto::{random_nonce, Cipher, EphemeralKeyPair};
use crate::error::{RemoteError, RemoteResult};
use crate::screen::{DisplayInfo, FileEntry, InputEvent, QualityPreset};
use bytes::{Buf, BufMut, BytesMut};
use serde::{Deserialize, Serialize};
use std::io;
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::codec::{Decoder, Encoder};

/// 会话协议版本；不匹配的 `Hello` 一律拒绝。
///
/// # 为什么是 8
///
/// 版本 8 做了四件事，**都不向后兼容**：
///
/// - [`SessionMessage::Heartbeat`] / [`SessionMessage::HeartbeatAck`] 带上单调时钟时间戳
///   （`t1_us` / `t2_us` + `t3_us`），用于 NTP 式四时间戳估计两端时钟偏差，进而算端到端延迟；
/// - [`SessionMessage::VideoPacket`] / [`SessionMessage::RegionUpdate`] 各追加一个
///   `capture_ts_us`（被控端采集完成的时刻）；
/// - 末尾追加 [`SessionMessage::HostStats`]（判别式 28，被控端每秒上报的整机遥测）；
/// - 心跳/视频帧的字段形状变化本身就会让任何旧端 bincode 解码失败。
///
/// 因此**两端必须同为 v8**：旧端（v7 及更早）会在握手阶段被 `Hello.proto_version` 的硬相等
/// 校验干净拒绝（回 `HelloAck{accepted:false}`），不会退化成硬断链。`Hello` / `HelloAck` /
/// `DisplayInfo` 的字节形状**一个字都没动**（两条机械锁守着），所以这条拒绝路径仍然成立。
///
/// # 发布顺序（**重要**）
///
/// **先把携带 v8 的客户端铺开，再部署依赖它的服务端门控**。v8 ↔ v7 互不可控（握手阶段被
/// 硬相等校验拒绝），所以同一次升级必须让所有 GUI 客户端都到 v8；服务端
/// `MIN_REMOTE_CLIENT_VERSION`（`crates/agent/src/runner/version_gate.rs`）按需再抬一档。
/// 客户端上报的是 `CARGO_PKG_VERSION`，而发布 workflow 只从 git tag 取版本、**不写回**
/// `Cargo.toml`，所以 `crates/client` / `crates/client-gui` 的版本号仍要手工抬。
///
/// # 版本 7
///
/// 只在末尾追加了 [`SessionMessage::LockScreen`]（控制器 → 被控端，请求锁定屏幕）。
/// 它没有任何新旧共存语义：老端在 bincode 解码到判别式 27 时会失败，故版本号必须抬，
/// 且**两端必须同为 v7**——旧端（v6）会在握手阶段被 `Hello.proto_version` 的硬相等校验
/// 干净拒绝（回 `HelloAck{accepted:false}`），不会退化成硬断链。
///
/// # 版本 6
///
/// 版本 6 在我们这条分支上把两件事合到了一起：
///
/// - **脏矩形增量补丁**：追加 [`SessionMessage::RegionUpdate`]（被控端 → 控制端）。它只追加在
///   [`SessionMessage`] 末尾，但老端会在 bincode 解码时因未知判别式失败，故必须抬版本。
/// - **输入语义变更**（main 的 v5）：可打印键不再走 `KeyEvent{code: TEXT, text}`（被控端按
///   Unicode 码点注入，绕过被控端的键盘布局与输入法），改为发送完整键位码，由被控端映射成
///   VK / CGKeyCode / keysym 注入，字符最终由**被控端**的键盘布局与输入决定。键码表与旧语义
///   不兼容（同一 `code` 值含义不同），所以那次也必须抬版本。
///
/// 两笔改动各自都把版本抬到了 5，但含义不同；合并后统一为 **6**，任一旧端（v4 或 v5）都会
/// 被硬相等校验干净拒绝。数据面没有会话内向后兼容分支，两端靠 `Hello.proto_version` 的
/// **硬相等** 校验实现「混合版本干净拒绝」，不会解码对方的画面帧。
///
/// 版本 4 是控制端可选的 [`SessionMessage::SetFrameRate`]（帧率上限）；版本 3 是阶段 5 的
/// 硬件编码协商（[`SessionMessage::CodecOffer`] / [`SessionMessage::CodecSelect`] 与
/// [`CodecId::H264`]）。
///
/// **配套的第二道门控在服务端**（`crates/agent/src/runner/version_gate.rs` 的
/// `MIN_REMOTE_CLIENT_VERSION`）。故此处刻意只动本常量。
pub const SESSION_PROTO_VERSION: u16 = 8;

/// 单帧载荷上限（16 MiB）。
pub const MAX_SESSION_FRAME: usize = 16 * 1024 * 1024;

/// 心跳间隔。**必须小于 hbbr 的 30s idle_timeout**（由单测断言）。
///
/// 心跳同时是延迟探测：1s 一次的零载荷往返让 QoS 闭环每秒都能拿到一个 RTT 样本。**复用既有
/// 心搏变体、不新增数据面变体**——`SessionMessage` 的硬相等版本校验会让混合版本的一端 bincode
/// 解码失败而直接断链。高 RTT 链路上，迟到的 ack 可能与更新的心跳配错对，故两端都用
/// 「发出时刻队列」而不是单个 `Option<Instant>`（见 `controller.rs` / `controlled.rs`）。
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);

/// 存活超时：连续这么久收不到对端 `HeartbeatAck` 就判定链路已断。
///
/// = 4 × [`HEARTBEAT_INTERVAL`]。**必须 ≥ 2 个心跳周期**，否则一次调度抖动或一个迟到的 ack
/// 就会误杀健康会话；又必须**远小于 hbbr 中继的 30s `idle_timeout`**——链路半开（中继停转发
/// 但 TCP 不断、接收方零窗口）时中继本身不会报任何错，只能靠这个上界把「会话永久冻结、UI 仍
/// 显示在线」变成一次明确的断线结束（`deliberate: false`，从而触发自动重连）。
///
/// 判据只看**队首**（最老的未应答心跳）的年龄：队首超时说明最早那一拍都没回来，后面的读数
/// 自然也不可信。**没有发出过心跳时（队列空）不判**——会话刚建立还没有样本。
pub const LIVENESS_TIMEOUT: Duration = Duration::from_secs(4);

/// RTT 样本的可信上限。
///
/// 心跳是**零载荷**的固定往返，正常 RTT 远小于它；一旦读数达到 10s，只可能是把一个迟到的
/// `HeartbeatAck` 与另一次（或已作废的）`Heartbeat` 配了对，或对端/链路卡了整整几个周期。
/// 两者都不可信，报给 UI 只会误导。**它只是可信上限，不是探测间隔**——探测间隔是
/// [`HEARTBEAT_INTERVAL`]（1s）。
pub const TRUSTED_RTT_MAX: Duration = Duration::from_secs(10);

/// 由「发出时刻」与「收到应答时刻」算一次可信的 RTT 样本。
///
/// 只丢弃 `>= TRUSTED_RTT_MAX` 的样本（见该常量的说明）；1–10s 的读数仍然采信——跨洲中继/
/// 卫星链路完全可能在这个区间，早先按「一个心跳间隔」丢弃会把它们全部误判成错配。
/// 抽成纯函数以便单测。
#[must_use]
pub fn trusted_rtt(sent: Instant, now: Instant) -> Option<Duration> {
    let rtt = now.saturating_duration_since(sent);
    (rtt < TRUSTED_RTT_MAX).then_some(rtt)
}

/// 一次会话的结束信息：会话循环的返回值，也是管理器判定的唯一依据。
///
/// 放在本模块（而非 `manager`）是因为 `controller` / `controlled` 两个循环都要**返回**它；
/// 若定义在 `manager` 就成了「循环依赖管理器、管理器依赖循环」的互相引用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEnd {
    /// 面向用户的中文原因描述。
    pub reason: String,
    /// 是否**主动**结束（本端用户中断，或收到对端 `Bye`）。
    /// `false` 表示连接是断的/失败的——这是断线自动重连的唯一判据。
    pub deliberate: bool,
}

/// 长度前缀字节数。
const LENGTH_PREFIX: usize = 4;

/// 多路复用通道。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum Lane {
    /// 控制（握手、心跳、显示列表、结束）。
    Control = 0,
    /// 画面。
    Screen = 1,
    /// 文件（阶段 2）。
    File = 2,
}

impl Lane {
    /// 取线上字节值。
    #[must_use]
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// 从线上字节值解析；未知值返回 `None`。
    #[must_use]
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Control),
            1 => Some(Self::Screen),
            2 => Some(Self::File),
            _ => None,
        }
    }
}

/// 视频编解码器标识。
///
/// 它是 [`SessionMessage::VideoPacket`] 与 [`SessionMessage::ScreenConfig`] 的**字段**而不是
/// `SessionMessage` 的新变体，所以给它追加变体不影响 `SessionMessage` 的判别式布局。
/// 但这**不等于**可以不动版本号：老对端收到不认识的 `CodecId` 判别式一样会在 bincode 解码时失败，
/// 故阶段 5（硬件编码）抬了 `SESSION_PROTO_VERSION`，并在握手后交换 `CodecOffer` / `CodecSelect`
/// 只发对端声明支持、且双方都实际具备的 codec。
///
/// **判别式顺序是线上契约**：`Vp9 = 0`、`H264 = 1`，新值一律追加在末尾（由
/// `codec_id_discriminants_are_stable` 机械锁住）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CodecId {
    /// VP9 profile 1（8-bit 4:4:4），软件编码。
    Vp9,
    /// H.264（固定 4:2:0），通常由平台硬件编码器提供（Windows NVENC）。
    ///
    /// 色度格式**不做协商**：本项目的 H.264 路径固定 4:2:0；需要 4:4:4 的调用方继续用 VP9。
    H264,
}

/// [`SessionMessage::VideoPacket::flags`]：本包携带编码器配置变更（编码尺寸/色度格式）。
///
/// 收到它意味着后续帧的几何/格式可能与之前不同，解码侧应重建解码器并等待关键帧。
pub const VIDEO_FLAG_CONFIG: u8 = 0b0000_0001;

/// [`SessionMessage::VideoPacket::flags`]：本包含 IDR（解码器可由此重建参考帧）。
pub const VIDEO_FLAG_KEYFRAME: u8 = 0b0000_0010;

/// [`RegionRect::flags`]：`data` 是用 zstd（level 1）压缩过的紧包 NV12。
///
/// 未置位时 `data` 就是原始 NV12（压缩没有收益时用，避免为极小块付 zstd 帧头开销）。
pub const REGION_FLAG_ZSTD: u8 = 0b0000_0001;

/// 一条 [`SessionMessage::RegionUpdate`] 里允许的最大矩形数。
///
/// 上限的作用是**限制控制端的解码/合成工作量**：矩形越多，解压与逐块覆写的固定开销越大。
/// 被控端在矩形超过这个数时会退回全帧视频（见 `crates/remote/src/controlled.rs`）。
pub const MAX_REGION_RECTS: usize = 32;

/// 脏矩形增量补丁里的一个矩形。
///
/// 坐标与尺寸都在**目标帧缓冲（编码尺寸）坐标系**里，且**全部为偶数**：补丁像素是 NV12
/// （UV 半分辨率），奇数边界会让色度块错位。`data` 的布局是紧包 NV12：
/// `Y(width*height) ‖ UV(width*height/2)`，无行填充。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegionRect {
    /// 左上角 X（偶数）。
    pub x: u32,
    /// 左上角 Y（偶数）。
    pub y: u32,
    /// 宽度（偶数）。
    pub width: u32,
    /// 高度（偶数）。
    pub height: u32,
    /// 压缩标志；目前只有 [`REGION_FLAG_ZSTD`]。
    pub flags: u8,
    /// 压缩（或原始）的紧包 NV12。
    pub data: Vec<u8>,
}

/// 会话消息。
///
/// # 判别式布局（**硬约束，见 `discriminant_golden` 机械锁**）
///
/// `Hello` 恒为 0、`HelloAck` 恒为 1，新变体一律**追加在末尾**。两端靠「先解出 `Hello`、
/// 读到 `proto_version`、再明文干净拒绝」实现混合版本不断链，因此这两个变体的字节形状与
/// 判别式**一个字都不能动**。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SessionMessage {
    /// 控制器发起握手。
    Hello {
        /// 会话 ID（服务端签发）。
        session_id: String,
        /// 协议版本。
        proto_version: u16,
        /// 控制器 RustDesk ID。
        controller_rd_id: String,
        /// 随机 nonce（防重放）。
        nonce: Vec<u8>,
        /// `HMAC-SHA256(token, session_id ‖ controller_rd_id ‖ nonce)`。
        hmac: Vec<u8>,
        /// 控制器的 X25519 临时公钥（32 字节）。
        x25519_pub: Vec<u8>,
    },
    /// 被控端回应握手。
    HelloAck {
        /// 是否接受。
        accepted: bool,
        /// 拒绝原因；接受时为 `None`。
        reason: Option<String>,
        /// 被控端显示器列表。
        displays: Vec<DisplayInfo>,
        /// 被控端的 X25519 临时公钥（32 字节）。
        x25519_pub: Vec<u8>,
        /// `HMAC-SHA256(token, session_id ‖ responder_pub ‖ initiator_pub)`，证明被控端持有 token
        /// 并绑定自己的临时公钥（防中继方替换 `x25519_pub`）。
        hmac_ack: Vec<u8>,
    },
    /// 一条输入事件。
    Input(InputEvent),
    /// 控制器切换观看的显示器（被控端据此换采集目标）。
    SelectDisplay {
        /// 目标显示器编号（取自 `HelloAck.displays`）。
        display: u32,
    },
    /// 控制器下发画质档位（被控端据此换编码参数并强制重发关键帧）。
    SetQuality {
        /// 目标档位。
        preset: QualityPreset,
    },
    /// 剪贴板文本（双向）。
    Clipboard {
        /// 文本内容。
        text: String,
    },
    /// 请求对端把剪贴板内容发过来（双向）。
    ClipboardRequest,
    /// 显示器列表（会话中变更时下发）。
    DisplayList(Vec<DisplayInfo>),
    /// 心跳（同时是延迟/时钟探测；双向）。
    ///
    /// `t1_us` 是**发送方**的单调时钟（[`crate::clock::now_us`]）。对端据此算出四时间戳里的
    /// `t1`，配合自己的 `t2`（收到时刻）与 `t3`（应答发出时刻），发送方再用 `t4`（收到 ack）
    /// 估计两端时钟偏差（[`crate::clock::clock_offset_us`]）。时间戳**只**用于观测：
    /// 它不进任何控制决策，也绝不参与鉴权/密钥派生。
    Heartbeat {
        /// 发送时刻（发送方进程内单调时钟，微秒）。
        t1_us: u64,
    },
    /// 心跳应答（双向）。
    ///
    /// `t2_us` / `t3_us` 都是**应答方**的单调时钟：`t2` 是收到 `Heartbeat` 的时刻、`t3` 是
    /// 发出本应答的时刻。两者只在四时间戳公式里使用，不承担同步语义。
    HeartbeatAck {
        /// 收到对端 `Heartbeat` 的时刻（应答方本地时钟，微秒）。
        t2_us: u64,
        /// 发出本应答的时刻（应答方本地时钟，微秒）。
        t3_us: u64,
    },
    /// 会话结束。
    Bye {
        /// 结束原因。
        reason: String,
    },
    // ── 文件传输（走 `File` lane；`id` 由发起方分配，两端各自独立编号空间）──
    /// 请求列目录（`path` 为共享根下的虚拟路径，`""` = 根）。
    FileListRequest {
        /// 请求 ID。
        id: u64,
        /// 虚拟路径。
        path: String,
    },
    /// 列目录结果。
    FileList {
        /// 请求 ID。
        id: u64,
        /// 回显虚拟路径。
        path: String,
        /// 目录项。
        entries: Vec<FileEntry>,
    },
    /// 请求下载（被控端 → 控制器方向的文件）。
    FileDownloadRequest {
        /// 传输 ID。
        id: u64,
        /// 虚拟路径。
        path: String,
    },
    /// 请求上传（控制器 → 被控端方向的文件）。
    FileUploadRequest {
        /// 传输 ID。
        id: u64,
        /// 虚拟路径（含目标文件名）。
        path: String,
        /// 声明字节数（被控端据此预检限额）。
        size: u64,
    },
    /// 传输受理/拒绝。
    FileAccept {
        /// 传输 ID。
        id: u64,
        /// 是否受理。
        accepted: bool,
        /// 拒绝原因；受理时为 `None`。
        reason: Option<String>,
        /// 受理方回填的实际字节数（下载时由被控端给出；上传时为 0）。
        size: u64,
    },
    /// 数据块（按序追加；单条 TCP 流不会乱序，故无需 offset）。
    FileChunk {
        /// 传输 ID。
        id: u64,
        /// 数据。
        data: Vec<u8>,
    },
    /// 传输完成。
    FileDone {
        /// 传输 ID。
        id: u64,
    },
    /// 传输失败/取消（任一端均可发）。
    FileError {
        /// 传输 ID。
        id: u64,
        /// 原因。
        reason: String,
    },
    // ── 视频数据面（阶段 2；走 `Screen` lane，判别式追加在末尾）──
    /// 一帧视频（时域编码，codec 由 [`SessionMessage::CodecSelect`] 决定）。
    /// 关键帧与增量帧都走这里。
    ///
    /// `data` 是**一个或多个**长度前缀（u32 大端）拼接的帧；解码侧按同一前缀切分后逐帧
    /// `decode`。正常路径下 VP9 的 `lag_in_frames = 1` 时每次 `encode` 恰好产出一帧，前缀是为
    /// 健壮性保留。`codec` 必须与 `CodecSelect` 一致。
    VideoPacket {
        /// 来源显示器编号。
        display: u32,
        /// 帧序号（被控端递增，跨关键帧连续）。
        seq: u64,
        /// 编解码器。
        codec: CodecId,
        /// 标志位（[`VIDEO_FLAG_CONFIG`] | [`VIDEO_FLAG_KEYFRAME`]）。
        flags: u8,
        /// **编码**尺寸的宽（不等于显示尺寸）。
        width: u32,
        /// **编码**尺寸的高。
        height: u32,
        /// 帧数据（长度前缀拼接）。
        data: Vec<u8>,
        /// 该帧**采集完成**的时刻（被控端单调时钟，微秒；[`crate::clock::now_us`]）。
        ///
        /// 控制器用 `本地时刻 + 时钟偏差 - capture_ts_us` 算端到端延迟（采集 → 解码完成）。
        /// 它只是观测值：0 表示未知（旧逻辑/异常路径），控制器此时不报端到端。
        capture_ts_us: u64,
    },
    /// 控制器请求关键帧（解码失败、丢弃了参考帧、或长时间无帧）。
    RequestKeyframe {
        /// 目标显示器编号。
        display: u32,
    },
    /// 被控端下行屏幕配置：编码尺寸 + codec（首个视频包之前发一次，变更时重发）。
    ///
    /// **显示尺寸不在这里**——它在 `HelloAck.displays` 的 [`DisplayInfo`] 里，而
    /// `HelloAck` 的字节形状一个字都不能动（见 [`SessionMessage`] 的判别式布局说明）。
    /// `width`/`height` 是**编码**尺寸，即后续 [`SessionMessage::VideoPacket`] 会声明的尺寸；
    /// `codec` 必须与 [`SessionMessage::CodecSelect`] 一致。
    ScreenConfig {
        /// 目标显示器编号。
        display: u32,
        /// 编解码器。
        codec: CodecId,
        /// 编码尺寸的宽。
        width: u32,
        /// 编码尺寸的高。
        height: u32,
    },
    /// 控制器回报 QoS 观测量（阶段 3 的 AIMD 闭环用；阶段 2 只定义不消费）。
    QosFeedback {
        /// 心跳往返（毫秒）。
        rtt_ms: u32,
        /// 接收码率（kbps）。
        recv_kbps: u32,
        /// 解码耗时（毫秒，最近一帧）。
        decode_ms: u32,
        /// 待解码积压（帧数）。
        backlog: u32,
    },
    // ── 编解码器协商（阶段 5；判别式追加在末尾，握手加密后立刻交换）──
    /// 控制器声明自己**能解码**的 codec（控制器 → 被控端，握手完成后立刻发）。
    ///
    /// 这只是能力声明，不是保证：被控端只有在**自己也注入了对应编码器**时才会选它。
    CodecOffer {
        /// 本端是否具备 H.264 解码能力。
        h264: bool,
        /// 本端是否具备 VP9 解码能力（内置实现始终为 `true`）。
        vp9: bool,
    },
    /// 被控端通告最终选定的 codec（被控端 → 控制器，**开始出帧之前**发）。
    ///
    /// 控制器据此创建解码器；后续 [`SessionMessage::ScreenConfig`] 与
    /// [`SessionMessage::VideoPacket`] 的 `codec` 字段必须与之一致。
    CodecSelect {
        /// 选定 codec。
        codec: CodecId,
    },
    /// 控制器下发帧率**上限**（控制端 → 被控端）。
    ///
    /// 帧率是上限而非固定值：QoS 闭环仍可在其上降级（地板见 [`crate::qos::QosParams::min_fps`]）。
    /// 被控端只改采集节拍与编码器的 `fps`（在线重配置，**不重建编码器、不插 IDR**）。
    SetFrameRate {
        /// 目标帧率上限（被控端钳到 `[min_fps, 120]`）。
        fps: u8,
    },
    /// 脏矩形增量补丁（被控端 → 控制端；仅 Windows H.264 路径）。
    ///
    /// # 语义
    ///
    /// 应用在上一次 [`SessionMessage::VideoPacket`]（或补丁）之上：控制端维护一块**持久**
    /// NV12 帧缓冲，全帧到达时整块替换，补丁到达时只覆写给出的矩形。`rects` 为空表示
    /// **保活**（画面没变，不要重绘），用于让控制端的「长时间无帧」看门狗保持安静。
    ///
    /// # 顺序与可靠性
    ///
    /// 补丁与全帧走**同一条**解码队列，因此严格同序；补丁**不允许被丢弃**（丢一个会让画面
    /// 永久缺一块）。控制端队列满时会请求关键帧，被控端据此退出补丁模式并发一个全帧 IDR。
    ///
    /// # 与视频序号的关系
    ///
    /// 补丁**不占用** [`SessionMessage::VideoPacket::seq`]：补丁模式下编码器不出帧，视频
    /// 序号保持不变；退出补丁模式后的第一个全帧序号仍与解码端的 `expected_seq` 连续，
    /// 不会误触发「序号断层」。
    RegionUpdate {
        /// 目标显示器编号。
        display: u32,
        /// 目标帧缓冲宽度（编码尺寸，偶数）。
        width: u32,
        /// 目标帧缓冲高度（编码尺寸，偶数）。
        height: u32,
        /// 本批矩形；空 = 保活。
        rects: Vec<RegionRect>,
        /// 该批补丁**采集完成**的时刻（被控端单调时钟，微秒；语义同
        /// [`SessionMessage::VideoPacket::capture_ts_us`]）。
        capture_ts_us: u64,
    },
    /// 请求被控端**锁定本机屏幕**（控制器 → 被控端）。
    ///
    /// 纯 UI 动作：被控端只在**完全控制**模式下执行（仅观看/仅文件传输不该改变远端机器
    /// 状态），其余模式忽略并记日志。会话本身保持连接；锁屏后 Windows 进入安全桌面，
    /// GUI 模式被控端通常无法继续采集画面（见根目录 `CLAUDE.md` 的锁屏说明）。
    LockScreen,
    /// 被控端**整机遥测**（被控端 → 控制器，每秒一条）。
    ///
    /// 控制器的会话角标据此显示对端身份（平台、编码器/采集后端与适配器）与实时遥测
    /// （采集/编码耗时、实测 fps、码率、关键帧拆解、出站队列积压、编码器重建次数……）。
    ///
    /// **只读观测面**：控制器收到后不据此做任何控制决策（QoS 闭环仍只看 `QosFeedback`），
    /// 也绝不回发。字段是 v8 的线上契约，新增一律追加在结构体末尾。
    HostStats(HostStats),
}

/// 由**变化面积**推出的内容活跃等级（被控端本地 `qos` 的判断，随 [`HostStats`] 上报展示）。
///
/// 与 [`crate::qos`] 内部的同名概念一一对应，但它必须能序列化（要上线），所以单独定义在
/// 协议模块里。判别式顺序（Ui = 0 / Mixed = 1 / Active = 2）是线上契约，**只追加、不插入**。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostActivity {
    /// UI 操作（打字/菜单/光标）：变化面积很小。
    Ui,
    /// 介于两者之间。
    #[default]
    Mixed,
    /// 运动/视频：变化面积大，不设内容驱动的码率上限。
    Active,
}

impl HostActivity {
    /// 面向 UI 的中文标签。
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Ui => "界面",
            Self::Mixed => "混合",
            Self::Active => "运动",
        }
    }
}

/// 被控端每秒上报的整机遥测（**线上类型**，v8 新增）。
///
/// # 为什么要单独一条消息
///
/// 这些读数只在**被控端**存在（编码器后端、采集适配器、出站队列积压……），而控制器的统计
/// 角标要显示它们。进程内的 [`crate::stats::SessionStats`] 只在本端有效，必须跨端传一份。
///
/// # 语义
///
/// - 身份字段（`platform` / `*_backend` / `*_adapter`）每次上报都填当前值，控制器直接覆盖；
/// - 每秒遥测字段是**本秒窗口**的读数（`capture_us` 是平均单帧、`fps` 是实测出帧率）；
/// - C2/C3/C5 字段（`out_queue_*` / `encoder_rebuilds` / `forced_idrs` / `capture_*` /
///   `no_new_frame`）是**每秒增量**，不是累计值。
///
/// 控制器收到后只写进 [`crate::manager::RemoteUiState`]（甚至不参与 QoS 决策）。
#[expect(
    clippy::struct_excessive_bools,
    reason = "线上遥测天然由若干互不相关的独立开关（GPU 采集/零拷贝/禁用/原生分辨率/补丁模式）\
              组成，合并成枚举只会让序列化形状与对端契约更难核对"
)]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostStats {
    /// 平台标识（`windows` / `macos` / `linux`）。
    pub platform: String,
    /// 编码器后端（`"NVENC"` / `"Intel QSV"` / `"AMD VCE"` / `"MFT"` / `"libvpx"`）。
    pub encoder_backend: String,
    /// 编码器名称（例如 `"NVIDIA NVENC H.264"`）。
    pub encoder_name: String,
    /// 编码器所在适配器名（显卡名；软件编码时为空）。
    pub encoder_adapter: String,
    /// 采集后端（`"WGC"` / `"DXGI"` / `"xcap"` / `"BitBlt"`）。
    pub capture_backend: String,
    /// 采集适配器名（GPU 采集时为显卡名；CPU 采集为空）。
    pub capture_adapter: String,
    /// 采集是否走 GPU（零拷贝 / 纹理直通）。
    pub gpu_capture: bool,
    /// 编码器后端是否使用 **GPU 纹理输入**（NVENC 直通 / 复用采集设备的 MFT）。
    ///
    /// 这是**编码器的输入形态**，不是「这一帧一定没有回读」：NVENC 在跨设备或需要缩放时
    /// 仍会回读（见 `win_video.rs` 的直通条件）。角标据此区分编码器是 GPU 还是 CPU 路径。
    pub gpu_encode: bool,
    /// GPU 路径是否已被永久禁用（任一环失败）。
    pub gpu_disabled: bool,
    /// GPU 被禁用的原因（未禁用时为空串）。
    pub gpu_disabled_reason: String,
    /// 是否**原生分辨率**会话（4K 编码，仅硬件 H.264 的显式选项）。
    pub native_resolution: bool,
    /// 当前编码宽度（像素）。
    pub encode_width: u32,
    /// 当前编码高度（像素）。
    pub encode_height: u32,
    /// 当前显示器的物理宽度（像素）。
    pub display_width: u32,
    /// 当前显示器的物理高度（像素）。
    pub display_height: u32,
    /// 最近一秒平均单帧**采集**耗时（微秒；GPU 采集常在 1 ms 内，毫秒取整会恒为 0）。
    pub capture_us: u32,
    /// 最近一秒平均单帧**编码**耗时（毫秒）。
    pub encode_ms: u32,
    /// 最近一秒**实测**编码帧率（帧/秒，不是目标值）。
    pub fps: u32,
    /// 当前**生效**的目标码率（kbps，闭环在上限之下浮动后的值）。
    pub bitrate_kbps: u32,
    /// 最近一秒**实际出码**速率（kbps）。
    pub sent_kbps: u32,
    /// 最近一秒因「无变化」跳过的帧数。
    pub skipped: u32,
    /// 最近一秒的关键帧数。
    pub keyframes: u32,
    /// 最近一秒关键帧出码码率（kbps）。
    pub idr_kbps: u32,
    /// 最近一秒关键帧出码字节数（与 `idr_kbps` 同一窗口，可算「关键帧平均多大」）。
    pub idr_bytes: u64,
    /// 最近一秒普通帧出码码率（kbps）。
    pub p_kbps: u32,
    /// 最近一秒编码帧的平均变化面积（千分比，0..=1000）。
    pub changed_permille: u32,
    /// 当前生效的缩放（千分比；1000 = 原始尺寸）。
    pub scale_permille: u16,
    /// 内容活跃等级。
    pub activity: HostActivity,
    /// 是否处于脏矩形补丁模式。
    pub patch_mode: bool,
    /// 出站画面队列当前深度（帧）。
    pub out_queue_depth: u32,
    /// 最近一秒因出站队列满而在**编码之前**跳过的帧数（背压）。
    pub out_queue_skips: u32,
    /// 最近一秒编码器重建次数（几何变化才重建）。
    pub encoder_rebuilds: u32,
    /// 最近一秒因 `force` 而真正产出的关键帧数。
    pub forced_idrs: u32,
    /// 最近一秒采集失败次数（采集调用返回错误）。
    pub capture_failures: u32,
    /// 最近一秒硬件采集失败回退到 CPU 采集的次数（`WGC/DXGI → xcap`）。
    pub capture_fallbacks: u32,
    /// 最近一秒打开采集源时 `WGC → DXGI` 回退的次数。
    pub capture_open_fallbacks: u32,
    /// 最近一秒采集后端「没有新帧」（超时/无新合成帧）的次数。
    pub no_new_frame: u32,
}

/// 一个带 `lane` 的会话帧。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionFrame {
    /// 多路复用通道。
    pub lane: Lane,
    /// 消息体。
    pub message: SessionMessage,
}

impl SessionFrame {
    /// 构造一个 `Control` 通道帧。
    #[must_use]
    pub fn control(message: SessionMessage) -> Self {
        Self {
            lane: Lane::Control,
            message,
        }
    }

    /// 构造一个 `Screen` 通道帧。
    #[must_use]
    pub fn screen(message: SessionMessage) -> Self {
        Self {
            lane: Lane::Screen,
            message,
        }
    }

    /// 构造一个 `File` 通道帧。
    #[must_use]
    pub fn file(message: SessionMessage) -> Self {
        Self {
            lane: Lane::File,
            message,
        }
    }
}

/// 会话帧编解码器。
///
/// 未装 [`Cipher`] 时收发明文载荷；[`SessionCodec::set_cipher`] 之后收发 AEAD 密文载荷。
/// 握手帧必须明文收发，因此加密在 `HelloAck` 之后才装上。
#[derive(Debug)]
pub struct SessionCodec {
    /// 单帧长度上限。
    max_frame: usize,
    /// 载荷加密器；`None` 为明文。
    cipher: Option<Cipher>,
}

impl SessionCodec {
    /// 构造一个明文编解码器。
    #[must_use]
    pub fn new() -> Self {
        Self {
            max_frame: MAX_SESSION_FRAME,
            cipher: None,
        }
    }

    /// 装上加密器（握手完成后调用）。
    pub fn set_cipher(&mut self, cipher: Cipher) {
        self.cipher = Some(cipher);
    }

    /// 当前是否已加密。
    #[must_use]
    pub fn is_encrypted(&self) -> bool {
        self.cipher.is_some()
    }

    /// 当前单帧上限。
    #[must_use]
    pub fn max_frame(&self) -> usize {
        self.max_frame
    }

    /// 收紧单帧上限（对端不可信时限制内存）。
    pub fn set_max_frame(&mut self, max_frame: usize) {
        self.max_frame = max_frame;
    }

    /// 把一个 [`SessionFrame`] 编码成完整线上字节（含长度前缀），便于流水线写出。
    ///
    /// # Errors
    /// bincode 编码失败或载荷超过上限时返回 [`RemoteError::Codec`]。
    pub fn encode_frame_bytes(&mut self, frame: &SessionFrame) -> RemoteResult<Vec<u8>> {
        let mut buf = BytesMut::new();
        self.encode(frame.clone(), &mut buf)
            .map_err(|err| RemoteError::Codec(err.to_string()))?;
        Ok(buf.to_vec())
    }
}

impl Default for SessionCodec {
    fn default() -> Self {
        Self::new()
    }
}

/// 构造一个 IO 层的非法数据错误。
fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

impl Decoder for SessionCodec {
    type Item = SessionFrame;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<SessionFrame>, io::Error> {
        if src.len() < LENGTH_PREFIX {
            return Ok(None);
        }
        let len_u32 = u32::from_be_bytes(
            src[..LENGTH_PREFIX]
                .try_into()
                .map_err(|_| invalid("长度前缀读取失败"))?,
        );
        let len = usize::try_from(len_u32).map_err(|_| invalid("长度前缀溢出"))?;
        if len < 1 {
            return Err(invalid("会话帧长度非法（缺少 lane 字节）"));
        }
        if len > self.max_frame {
            return Err(invalid(format!(
                "会话帧长度 {len} 超过上限 {}",
                self.max_frame
            )));
        }
        if src.len() < LENGTH_PREFIX + len {
            // 半包：等待更多数据。
            src.reserve(LENGTH_PREFIX + len - src.len());
            return Ok(None);
        }
        src.advance(LENGTH_PREFIX);
        let mut body = src.split_to(len);
        let lane = Lane::from_u8(body[0]).ok_or_else(|| invalid("未知 lane 值"))?;
        body.advance(1);

        let plaintext = match self.cipher.as_mut() {
            Some(cipher) => cipher
                .open(&body)
                .map_err(|err| invalid(err.to_string()))?,
            None => body.to_vec(),
        };
        let message: SessionMessage = bincode::deserialize(&plaintext)
            .map_err(|err| invalid(format!("bincode 解码失败: {err}")))?;
        Ok(Some(SessionFrame { lane, message }))
    }
}

impl Encoder<SessionFrame> for SessionCodec {
    type Error = io::Error;

    fn encode(&mut self, item: SessionFrame, dst: &mut BytesMut) -> Result<(), io::Error> {
        let payload = bincode::serialize(&item.message)
            .map_err(|err| invalid(format!("bincode 编码失败: {err}")))?;
        let payload = match self.cipher.as_mut() {
            Some(cipher) => cipher.seal(&payload).map_err(|err| invalid(err.to_string()))?,
            None => payload,
        };
        let len = payload
            .len()
            .checked_add(1)
            .ok_or_else(|| invalid("载荷长度溢出"))?;
        if len > self.max_frame {
            return Err(invalid(format!("会话帧长度 {len} 超过上限 {}", self.max_frame)));
        }
        let len_u32 = u32::try_from(len).map_err(|_| invalid("会话帧长度超出 u32"))?;
        dst.reserve(LENGTH_PREFIX + len);
        dst.put_u32(len_u32);
        dst.put_u8(item.lane.as_u8());
        dst.put_slice(&payload);
        Ok(())
    }
}

/// 控制器侧握手所需的预计算材料。
///
/// 在建立传输**之前**构造：`hello_frame` 会与中继的 `RequestRelay` 帧写在同一次 write
/// （流水线），配对一建立对端立刻拿到 `Hello`，省一个 RTT。会话循环随后只等 `HelloAck`。
pub struct ControllerHandshake {
    /// 控制器临时密钥对（收到 `HelloAck` 后用于 ECDH）。
    pub keypair: EphemeralKeyPair,
    /// 本次握手的随机 nonce。
    pub nonce: Vec<u8>,
    /// 服务端下发的一次性 token：既用于校验 `HelloAck` 的 MAC，也作为密钥派生的认证输入。
    pub token: String,
    /// 预编码的 `Hello` 帧线上字节。
    pub hello_frame: Vec<u8>,
}

impl std::fmt::Debug for ControllerHandshake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControllerHandshake")
            .field("nonce_len", &self.nonce.len())
            .field("hello_frame_len", &self.hello_frame.len())
            .finish_non_exhaustive()
    }
}

/// 构造控制器握手材料（生成临时密钥、nonce、MAC 与 `Hello` 帧）。
///
/// # Errors
/// `Hello` 帧编码失败时返回 [`RemoteError::Codec`]。
pub fn plan_controller_handshake(
    session_id: &str,
    token: &str,
    controller_rd_id: &str,
) -> RemoteResult<ControllerHandshake> {
    let keypair = EphemeralKeyPair::generate();
    let nonce = random_nonce().to_vec();
    let x25519_pub = keypair.public_bytes().to_vec();
    // MAC 覆盖临时公钥：否则中继方可替换它而不被发现。
    let hmac = handshake_mac(token, session_id, controller_rd_id, &nonce, &x25519_pub);
    let message = SessionMessage::Hello {
        session_id: session_id.to_owned(),
        proto_version: SESSION_PROTO_VERSION,
        controller_rd_id: controller_rd_id.to_owned(),
        nonce: nonce.clone(),
        hmac,
        x25519_pub,
    };
    let mut codec = SessionCodec::new();
    let hello_frame = codec.encode_frame_bytes(&SessionFrame::control(message))?;
    Ok(ControllerHandshake {
        keypair,
        nonce,
        token: token.to_owned(),
        hello_frame,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::{FileEntry, KeyEvent, MouseButton, QualityPreset};

    /// `Hello` / `HelloAck` 的固定线上字节（机械锁；见 `hello_and_helloack_wire_bytes_are_frozen`）。
    const FROZEN_HELLO_BYTES: &[u8] = &[
        // 判别式 Hello = 0
        0, 0, 0, 0,
        // session_id "sess-1"
        6, 0, 0, 0, 0, 0, 0, 0, b's', b'e', b's', b's', b'-', b'1',
        // proto_version = 3
        3, 0,
        // controller_rd_id "111111111"
        9, 0, 0, 0, 0, 0, 0, 0, b'1', b'1', b'1', b'1', b'1', b'1', b'1', b'1', b'1',
        // nonce [1, 2, 3, 4]
        4, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4,
        // hmac [7; 32]
        32, 0, 0, 0, 0, 0, 0, 0,
        7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
        7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
        // x25519_pub [9; 32]
        32, 0, 0, 0, 0, 0, 0, 0,
        9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9,
        9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9,
    ];
    const FROZEN_HELLO_ACK_BYTES: &[u8] = &[
        // 判别式 HelloAck = 1
        1, 0, 0, 0,
        // accepted = true
        1,
        // reason = None
        0,
        // displays = [DisplayInfo { id: 0, width: 1920, height: 1080, name: "main", primary: true }]
        1, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0,
        128, 7, 0, 0,
        56, 4, 0, 0,
        4, 0, 0, 0, 0, 0, 0, 0, b'm', b'a', b'i', b'n',
        1,
        // x25519_pub [8; 32]
        32, 0, 0, 0, 0, 0, 0, 0,
        8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8,
        8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8,
        // hmac_ack [6; 32]
        32, 0, 0, 0, 0, 0, 0, 0,
        6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6,
        6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6,
    ];


    fn codec_roundtrip(message: SessionMessage, lane: Lane) -> SessionFrame {
        let mut enc = SessionCodec::new();
        let mut wire = BytesMut::new();
        enc.encode(SessionFrame { lane, message }, &mut wire).unwrap();

        let mut dec = SessionCodec::new();
        dec.decode(&mut wire).unwrap().expect("应解出一帧")
    }

    #[test]
    fn heartbeat_interval_is_below_relay_idle_timeout() {
        // hbbr 的 idle_timeout 是 30s（relay.rs::RelayConfig::default）。
        assert!(
            HEARTBEAT_INTERVAL < Duration::from_secs(30),
            "心跳间隔必须严格小于中继空闲超时，否则静止会话会被断开"
        );
    }

    #[test]
    fn trusted_rtt_keeps_fast_samples_and_drops_slow_ones() {
        let base = Instant::now();
        // 正常往返：远小于上限，应采信。
        assert_eq!(
            trusted_rtt(base, base + Duration::from_millis(5)),
            Some(Duration::from_millis(5))
        );
        // 1.5s 仍必须采信：高 RTT 链路（卫星/跨洲中继）完全可能在这个量级，它不是错配信号。
        assert_eq!(
            trusted_rtt(base, base + Duration::from_millis(1500)),
            Some(Duration::from_millis(1500))
        );
        // 恰好等于上限、以及超过上限：样本不可信，丢弃（只可能是迟到 ack 配错对）。
        assert_eq!(trusted_rtt(base, base + TRUSTED_RTT_MAX), None);
        assert_eq!(
            trusted_rtt(base, base + TRUSTED_RTT_MAX + Duration::from_millis(1)),
            None
        );
        // 时钟回拨/负差不应 panic，按 0 处理（仍是可信样本）。
        assert_eq!(trusted_rtt(base, base), Some(Duration::ZERO));
    }

    #[test]
    fn heartbeat_probes_every_second_and_trusts_up_to_ten() {
        // 探测间隔与可信上限是两件事：1s 探测、10s 上限。把它们绑在一起会让所有高 RTT 链路
        // 的读数被永久丢弃。
        assert_eq!(HEARTBEAT_INTERVAL, Duration::from_secs(1));
        assert_eq!(TRUSTED_RTT_MAX, Duration::from_secs(10));
        assert!(HEARTBEAT_INTERVAL < TRUSTED_RTT_MAX);
    }

    #[test]
    fn messages_roundtrip() {
        let cases = vec![
            (
                SessionMessage::Hello {
                    session_id: "s1".into(),
                    proto_version: SESSION_PROTO_VERSION,
                    controller_rd_id: "123456789".into(),
                    nonce: vec![1, 2, 3],
                    hmac: vec![9; 32],
                    x25519_pub: vec![7; 32],
                },
                Lane::Control,
            ),
            (
                SessionMessage::HelloAck {
                    accepted: true,
                    reason: None,
                    displays: vec![DisplayInfo {
                        id: 0,
                        width: 1920,
                        height: 1080,
                        name: "main".into(),
                        primary: true,
                    }],
                    x25519_pub: vec![8; 32],
                    hmac_ack: vec![9; 32],
                },
                Lane::Control,
            ),
            (
                SessionMessage::VideoPacket {
                    display: 0,
                    seq: 42,
                    codec: CodecId::Vp9,
                    flags: VIDEO_FLAG_KEYFRAME,
                    width: 640,
                    height: 480,
                    data: vec![0x10, 0x20, 0x30],
                    capture_ts_us: 1_234_567,
                },
                Lane::Screen,
            ),
            (
                SessionMessage::Input(InputEvent::MouseButton {
                    button: MouseButton::Left,
                    pressed: true,
                }),
                Lane::Control,
            ),
            (
                SessionMessage::Heartbeat { t1_us: 42 },
                Lane::Control,
            ),
            (
                SessionMessage::HeartbeatAck {
                    t2_us: 43,
                    t3_us: 44,
                },
                Lane::Control,
            ),
            (
                SessionMessage::Bye {
                    reason: "done".into(),
                },
                Lane::Control,
            ),
        ];
        for (message, lane) in cases {
            let got = codec_roundtrip(message.clone(), lane);
            assert_eq!(got.lane, lane, "lane 应保持");
            assert_eq!(got.message, message, "消息应往返一致");
        }
    }

    /// 协商消息的编解码往返（`CodecOffer` / `CodecSelect` 走 `Control` lane）。
    #[test]
    fn codec_offer_and_select_roundtrip() {
        for message in [
            SessionMessage::CodecOffer {
                h264: true,
                vp9: true,
            },
            SessionMessage::CodecOffer {
                h264: false,
                vp9: true,
            },
        ] {
            let got = codec_roundtrip(message.clone(), Lane::Control);
            assert_eq!(got.message, message);
            assert_eq!(got.lane, Lane::Control);
        }
        for codec in [CodecId::Vp9, CodecId::H264] {
            let message = SessionMessage::CodecSelect { codec };
            let got = codec_roundtrip(message.clone(), Lane::Control);
            assert_eq!(got.message, message);
            assert_eq!(got.lane, Lane::Control);
        }
    }

    /// 造一个显示器的元信息。
    fn display(id: u32) -> DisplayInfo {
        DisplayInfo {
            id,
            width: 1280,
            height: 720,
            name: format!("d{id}"),
            primary: id == 0,
        }
    }

    /// 造一个目录项。
    fn entry(name: &str) -> FileEntry {
        FileEntry {
            name: name.to_owned(),
            path: format!("/{name}"),
            is_dir: false,
            size: 4096,
            modified_ms: 1_700_000_000_000,
        }
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "枚举全部变体平铺成一张表，拆开反而看不出覆盖面"
    )]
    fn all_session_variants_roundtrip() {
        // 锁的不变量：**每一个** `SessionMessage` 变体都能 bincode 编解码往返，且 lane 保持。
        //
        // 这是数据面的线上契约：`messages_roundtrip` 只覆盖了约 6 个变体，全部 `File*`、
        // `VideoPacket`、`SetQuality`、`SelectDisplay`、`Clipboard`、`ClipboardRequest`、
        // `DisplayList`、`HeartbeatAck` 从未被序列化过。某个变体的字段一旦漂移（改类型、
        // 调字段序），只有真机联调才会炸——这条用例把契约钉死在单测里。
        // 每个变体都带上非空与边界内容（空 entries、空 data、空字符串路径）。
        let cases: Vec<(SessionMessage, Lane)> = vec![
            (
                SessionMessage::Hello {
                    session_id: "s1".into(),
                    proto_version: SESSION_PROTO_VERSION,
                    controller_rd_id: "123456789".into(),
                    nonce: vec![1, 2, 3],
                    hmac: vec![9; 32],
                    x25519_pub: vec![7; 32],
                },
                Lane::Control,
            ),
            (
                SessionMessage::HelloAck {
                    accepted: false,
                    reason: Some("拒绝".into()),
                    displays: vec![],
                    x25519_pub: vec![],
                    hmac_ack: vec![],
                },
                Lane::Control,
            ),
            (
                SessionMessage::VideoPacket {
                    display: 0,
                    seq: 42,
                    codec: CodecId::Vp9,
                    flags: VIDEO_FLAG_KEYFRAME | VIDEO_FLAG_CONFIG,
                    width: 640,
                    height: 480,
                    data: vec![0x10, 0x20, 0x30],
                    capture_ts_us: 9_999,
                },
                Lane::Screen,
            ),
            // 边界：空 VP9 数据（编码器本轮无输出）+ 未知采集时刻。
            (
                SessionMessage::VideoPacket {
                    display: 1,
                    seq: 0,
                    codec: CodecId::Vp9,
                    flags: 0,
                    width: 0,
                    height: 0,
                    data: vec![],
                    capture_ts_us: 0,
                },
                Lane::Screen,
            ),
            (SessionMessage::RequestKeyframe { display: 2 }, Lane::Screen),
            (
                SessionMessage::ScreenConfig {
                    display: 0,
                    codec: CodecId::Vp9,
                    width: 1920,
                    height: 1080,
                },
                Lane::Screen,
            ),
            // 边界：全零 QoS 观测。
            (
                SessionMessage::QosFeedback {
                    rtt_ms: 0,
                    recv_kbps: 0,
                    decode_ms: 0,
                    backlog: 0,
                },
                Lane::Screen,
            ),
            (
                SessionMessage::QosFeedback {
                    rtt_ms: 250,
                    recv_kbps: 4000,
                    decode_ms: 17,
                    backlog: 3,
                },
                Lane::Screen,
            ),
            (
                SessionMessage::Input(InputEvent::MouseMove {
                    x: -5,
                    y: 100,
                    display: 2,
                }),
                Lane::Control,
            ),
            (
                SessionMessage::Input(InputEvent::MouseButton {
                    button: MouseButton::Middle,
                    pressed: false,
                }),
                Lane::Control,
            ),
            (
                SessionMessage::Input(InputEvent::Wheel { dx: -3, dy: 9 }),
                Lane::Control,
            ),
            (
                SessionMessage::Input(InputEvent::Key(KeyEvent {
                    code: 27,
                    pressed: true,
                    text: None,
                    modifiers: 0b1010,
                })),
                Lane::Control,
            ),
            (SessionMessage::SelectDisplay { display: 1 }, Lane::Control),
            (SessionMessage::SetQuality { preset: QualityPreset::Hd }, Lane::Control),
            (SessionMessage::Clipboard { text: "剪贴板文本".into() }, Lane::Control),
            // 边界：空剪贴板文本。
            (SessionMessage::Clipboard { text: String::new() }, Lane::Control),
            (SessionMessage::ClipboardRequest, Lane::Control),
            (SessionMessage::DisplayList(vec![display(0), display(1)]), Lane::Control),
            // 边界：显示器列表为空（无显示器）。
            (SessionMessage::DisplayList(vec![]), Lane::Control),
            (SessionMessage::Heartbeat { t1_us: 0 }, Lane::Control),
            (
                SessionMessage::HeartbeatAck {
                    t2_us: u64::MAX,
                    t3_us: 1,
                },
                Lane::Control,
            ),
            // 边界：空结束原因。
            (SessionMessage::Bye { reason: String::new() }, Lane::Control),
            // 边界：根路径（`""` = 共享根）。
            (SessionMessage::FileListRequest { id: 1, path: String::new() }, Lane::File),
            (
                SessionMessage::FileList {
                    id: 1,
                    path: "/docs".into(),
                    entries: vec![entry("a.txt"), entry("b.bin")],
                },
                Lane::File,
            ),
            // 边界：空目录。
            (SessionMessage::FileList { id: 2, path: "/empty".into(), entries: vec![] }, Lane::File),
            (SessionMessage::FileDownloadRequest { id: 3, path: "/a.bin".into() }, Lane::File),
            // 边界：声明字节数为 u64 上限。
            (
                SessionMessage::FileUploadRequest {
                    id: 4,
                    path: "/big.bin".into(),
                    size: u64::MAX,
                },
                Lane::File,
            ),
            (
                SessionMessage::FileAccept {
                    id: 5,
                    accepted: true,
                    reason: None,
                    size: 200 * 1024,
                },
                Lane::File,
            ),
            (
                SessionMessage::FileAccept {
                    id: 6,
                    accepted: false,
                    reason: Some("被控端未开启文件共享".into()),
                    size: 0,
                },
                Lane::File,
            ),
            (SessionMessage::FileChunk { id: 7, data: vec![0xCD; 4096] }, Lane::File),
            // 边界：空数据块。
            (SessionMessage::FileChunk { id: 8, data: vec![] }, Lane::File),
            (SessionMessage::FileDone { id: 9 }, Lane::File),
            (SessionMessage::FileError { id: 10, reason: "用户取消".into() }, Lane::File),
            // 编解码协商：两种能力组合都覆盖（全支持 / 只有 VP9）。
            (SessionMessage::CodecOffer { h264: true, vp9: true }, Lane::Control),
            (SessionMessage::CodecOffer { h264: false, vp9: true }, Lane::Control),
            (SessionMessage::CodecSelect { codec: CodecId::Vp9 }, Lane::Control),
            (SessionMessage::CodecSelect { codec: CodecId::H264 }, Lane::Control),
            (SessionMessage::SetFrameRate { fps: 30 }, Lane::Control),
            (SessionMessage::SetFrameRate { fps: 60 }, Lane::Control),
            // 边界：被控端会把它钳到 [min_fps, 120]。
            (SessionMessage::SetFrameRate { fps: 0 }, Lane::Control),
            // 补丁：带一个 zstd 矩形与一个原始矩形（两种 flags 都覆盖）。
            (
                SessionMessage::RegionUpdate {
                    display: 0,
                    width: 1920,
                    height: 1080,
                    rects: vec![
                        RegionRect {
                            x: 2,
                            y: 4,
                            width: 64,
                            height: 32,
                            flags: REGION_FLAG_ZSTD,
                            data: vec![0x11; 96],
                        },
                        RegionRect {
                            x: 100,
                            y: 200,
                            width: 16,
                            height: 16,
                            flags: 0,
                            data: vec![0x22; 384],
                        },
                    ],
                    capture_ts_us: 4_242,
                },
                Lane::Screen,
            ),
            // 边界：空补丁 = 保活。
            (
                SessionMessage::RegionUpdate {
                    display: 1,
                    width: 2,
                    height: 2,
                    rects: vec![],
                    capture_ts_us: 0,
                },
                Lane::Screen,
            ),
            (SessionMessage::LockScreen, Lane::Control),
            // 被控端遥测：满字段与全默认（空）各一条。
            (
                SessionMessage::HostStats(HostStats {
                    platform: "windows".into(),
                    encoder_backend: "NVENC".into(),
                    encoder_name: "NVIDIA NVENC H.264".into(),
                    encoder_adapter: "NVIDIA GeForce RTX 5070 Ti".into(),
                    capture_backend: "WGC".into(),
                    capture_adapter: "NVIDIA GeForce RTX 5070 Ti".into(),
                    gpu_capture: true,
                    gpu_encode: true,
                    gpu_disabled: false,
                    gpu_disabled_reason: String::new(),
                    native_resolution: true,
                    encode_width: 3840,
                    encode_height: 2160,
                    display_width: 3840,
                    display_height: 2160,
                    capture_us: 31_000,
                    encode_ms: 3,
                    fps: 60,
                    bitrate_kbps: 12_000,
                    sent_kbps: 8_500,
                    skipped: 12,
                    keyframes: 1,
                    idr_kbps: 900,
                    idr_bytes: 112_500,
                    p_kbps: 7_600,
                    changed_permille: 421,
                    scale_permille: 1000,
                    activity: HostActivity::Active,
                    patch_mode: false,
                    out_queue_depth: 2,
                    out_queue_skips: 0,
                    encoder_rebuilds: 0,
                    forced_idrs: 1,
                    capture_failures: 0,
                    capture_fallbacks: 1,
                    capture_open_fallbacks: 0,
                    no_new_frame: 3,
                }),
                Lane::Control,
            ),
            (
                SessionMessage::HostStats(HostStats::default()),
                Lane::Control,
            ),
        ];
        for (message, lane) in cases {
            let got = codec_roundtrip(message.clone(), lane);
            assert_eq!(got.lane, lane, "lane 应保持：{message:?}");
            assert_eq!(got.message, message, "消息应往返一致");
        }
    }

    #[test]
    fn unknown_session_variant_is_rejected() {
        // 锁的不变量：数据面收到**未知的 `SessionMessage` 判别式**（lane 合法、长度合法、
        // 但枚举序号越界）必须报错，既不能静默接受也不能 panic。
        //
        // 这是数据面的「版本门控等价物」：控制通道有 `client_supports_remote` 挡住老客户端，
        // 数据面没有——版本漂移最可能的表现就是发来一个本端不认识的新变体，必须干净地拒绝。
        // bincode 1.3 默认 fixint 编码：枚举判别式是 u32 小端（见 `bincode::serialize` 的
        // `DefaultOptions::new().with_fixint_encoding()`），故越界值用 u32 构造。
        let mut dec = SessionCodec::new();
        let mut buf = BytesMut::new();
        let payload = u32::MAX.to_le_bytes();
        // 长度 = 1 字节 lane + 载荷长度。
        buf.put_u32(u32::try_from(1 + payload.len()).unwrap());
        buf.put_u8(Lane::Control.as_u8());
        buf.put_slice(&payload);
        assert!(dec.decode(&mut buf).is_err(), "越界的枚举判别式必须被拒绝");
    }

    /// 固定的 `Hello`（判别式/字节恒等机械锁用；字段值覆盖二进制与字符串两类）。
    ///
    /// `proto_version` 刻意写成**字面量 3**、而不是 [`SESSION_PROTO_VERSION`]：这条测试锁的是
    /// `Hello` 的**字节形状**（字段顺序、长度前缀位置），不是当前协议版本。每次抬版本都去改
    /// `FROZEN_HELLO_BYTES` 会让「形状没变」这一断言失去意义。
    fn frozen_hello() -> SessionMessage {
        SessionMessage::Hello {
            session_id: "sess-1".into(),
            proto_version: 3,
            controller_rd_id: "111111111".into(),
            nonce: vec![1, 2, 3, 4],
            hmac: vec![7u8; 32],
            x25519_pub: vec![9u8; 32],
        }
    }

    /// 固定的 `HelloAck`（同上）。
    fn frozen_hello_ack() -> SessionMessage {
        SessionMessage::HelloAck {
            accepted: true,
            reason: None,
            displays: vec![DisplayInfo {
                id: 0,
                width: 1920,
                height: 1080,
                name: "main".into(),
                primary: true,
            }],
            x25519_pub: vec![8u8; 32],
            hmac_ack: vec![6u8; 32],
        }
    }

    /// **判别式黄金测试**：逐个断言 `SessionMessage` 每个变体的序号。
    ///
    /// 锁的不变量（见 [`SessionMessage`] 的判别式布局说明与计划「硬约束 2」）：
    /// `Hello` = 0、`HelloAck` = 1，新变体一律**追加在末尾**。两端靠「先解出 `Hello`、读到
    /// `proto_version`、再明文干净拒绝」实现混合版本不断链；任何**插入式**改动（把新变体插到
    /// 中间）都会让判别式错位，使 `Hello`/`HelloAck` 不再是 0/1，整个方案立刻退化成 bincode
    /// 解码失败导致的硬断链。bincode 1.3 默认 fixint：判别式是 u32 小端，故直接读前 4 字节。
    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "逐个变体平铺成一张判别式表，拆开反而看不出「只追加、不插入」的契约"
    )]
    fn discriminant_golden() {
        let cases: Vec<(&str, SessionMessage, u32)> = vec![
            ("Hello", frozen_hello(), 0),
            ("HelloAck", frozen_hello_ack(), 1),
            (
                "Input",
                SessionMessage::Input(InputEvent::MouseButton {
                    button: MouseButton::Left,
                    pressed: true,
                }),
                2,
            ),
            ("SelectDisplay", SessionMessage::SelectDisplay { display: 0 }, 3),
            ("SetQuality", SessionMessage::SetQuality { preset: QualityPreset::Hd }, 4),
            ("Clipboard", SessionMessage::Clipboard { text: "x".into() }, 5),
            ("ClipboardRequest", SessionMessage::ClipboardRequest, 6),
            ("DisplayList", SessionMessage::DisplayList(vec![]), 7),
            ("Heartbeat", SessionMessage::Heartbeat { t1_us: 0 }, 8),
            (
                "HeartbeatAck",
                SessionMessage::HeartbeatAck {
                    t2_us: 0,
                    t3_us: 0,
                },
                9,
            ),
            ("Bye", SessionMessage::Bye { reason: String::new() }, 10),
            (
                "FileListRequest",
                SessionMessage::FileListRequest { id: 1, path: String::new() },
                11,
            ),
            (
                "FileList",
                SessionMessage::FileList { id: 1, path: "/".into(), entries: vec![] },
                12,
            ),
            (
                "FileDownloadRequest",
                SessionMessage::FileDownloadRequest { id: 1, path: "/a".into() },
                13,
            ),
            (
                "FileUploadRequest",
                SessionMessage::FileUploadRequest { id: 1, path: "/a".into(), size: 0 },
                14,
            ),
            (
                "FileAccept",
                SessionMessage::FileAccept { id: 1, accepted: true, reason: None, size: 0 },
                15,
            ),
            ("FileChunk", SessionMessage::FileChunk { id: 1, data: vec![] }, 16),
            ("FileDone", SessionMessage::FileDone { id: 1 }, 17),
            ("FileError", SessionMessage::FileError { id: 1, reason: String::new() }, 18),
            (
                "VideoPacket",
                SessionMessage::VideoPacket {
                    display: 0,
                    seq: 0,
                    codec: CodecId::Vp9,
                    flags: 0,
                    width: 0,
                    height: 0,
                    data: vec![],
                    capture_ts_us: 0,
                },
                19,
            ),
            ("RequestKeyframe", SessionMessage::RequestKeyframe { display: 0 }, 20),
            (
                "ScreenConfig",
                SessionMessage::ScreenConfig {
                    display: 0,
                    codec: CodecId::Vp9,
                    width: 0,
                    height: 0,
                },
                21,
            ),
            (
                "QosFeedback",
                SessionMessage::QosFeedback {
                    rtt_ms: 0,
                    recv_kbps: 0,
                    decode_ms: 0,
                    backlog: 0,
                },
                22,
            ),
            (
                "CodecOffer",
                SessionMessage::CodecOffer { h264: true, vp9: true },
                23,
            ),
            (
                "CodecSelect",
                SessionMessage::CodecSelect { codec: CodecId::H264 },
                24,
            ),
            (
                "SetFrameRate",
                SessionMessage::SetFrameRate { fps: 60 },
                25,
            ),
            (
                "RegionUpdate",
                SessionMessage::RegionUpdate {
                    display: 0,
                    width: 0,
                    height: 0,
                    rects: vec![],
                    capture_ts_us: 0,
                },
                26,
            ),
            ("LockScreen", SessionMessage::LockScreen, 27),
            (
                "HostStats",
                SessionMessage::HostStats(HostStats::default()),
                28,
            ),
        ];
        for (name, msg, want) in &cases {
            let bytes = bincode::serialize(msg).unwrap();
            let got = u32::from_le_bytes(bytes[..4].try_into().unwrap());
            assert_eq!(got, *want, "变体 `{name}` 的判别式错位（应为 {want}）");
        }
        // 序号必须连续无洞：任何「删一个变体」的改动都会让这张表与实现脱节。
        for (i, (_, _, want)) in cases.iter().enumerate() {
            assert_eq!(usize::try_from(*want).unwrap(), i, "判别式必须从 0 起连续");
        }
    }

    /// **`HostStats` bincode 往返**：满字段与全默认各走一遍编解码。
    ///
    /// 它是 v8 新增的**唯一**大结构（36 个字段），字段顺序/类型漂移只会到真机联调才暴露；
    /// 这里把「编解码后逐字段相等」钉死在单测里。`HostActivity` 的判别式也一并锁住。
    #[test]
    fn host_stats_roundtrip() {
        let full = HostStats {
            platform: "windows".into(),
            encoder_backend: "Intel QSV".into(),
            encoder_name: "H.264 MFT".into(),
            encoder_adapter: "Intel UHD Graphics 770".into(),
            capture_backend: "DXGI".into(),
            capture_adapter: "Intel UHD Graphics 770".into(),
            gpu_capture: true,
            gpu_encode: true,
            gpu_disabled: true,
            gpu_disabled_reason: "设备丢失".into(),
            native_resolution: false,
            encode_width: 1920,
            encode_height: 1080,
            display_width: 2560,
            display_height: 1440,
            capture_us: 8_250,
            encode_ms: 13,
            fps: 30,
            bitrate_kbps: 6_000,
            sent_kbps: 4_321,
            skipped: 7,
            keyframes: 2,
            idr_kbps: 512,
            idr_bytes: 64_000,
            p_kbps: 3_809,
            changed_permille: 37,
            scale_permille: 750,
            activity: HostActivity::Ui,
            patch_mode: true,
            out_queue_depth: 1,
            out_queue_skips: 4,
            encoder_rebuilds: 1,
            forced_idrs: 2,
            capture_failures: 3,
            capture_fallbacks: 5,
            capture_open_fallbacks: 1,
            no_new_frame: 6,
        };
        for stats in [full, HostStats::default()] {
            let bytes = bincode::serialize(&stats).unwrap();
            let back: HostStats = bincode::deserialize(&bytes).unwrap();
            assert_eq!(back, stats);
        }
        // `HostActivity` 的判别式是 u32 小端、按声明顺序：Ui = 0 / Mixed = 1 / Active = 2。
        for (activity, want) in [
            (HostActivity::Ui, 0u32),
            (HostActivity::Mixed, 1),
            (HostActivity::Active, 2),
        ] {
            let bytes = bincode::serialize(&activity).unwrap();
            assert_eq!(u32::from_le_bytes(bytes[..4].try_into().unwrap()), want);
            assert!(!activity.label().is_empty());
        }
    }

    /// **`CodecId` 判别式黄金测试**：`Vp9 = 0`、`H264 = 1`，新值只能追加在末尾。
    ///
    /// `CodecId` 是 `VideoPacket` / `ScreenConfig` / `CodecSelect` 的字段，同一个 bincode 枚举
    /// 编码规则（u32 小端判别式）。把 `H264` 插到 `Vp9` 前面会让老端的 `Vp9` 解码成 `H264`，
    /// 画面直接花屏且没有任何版本错误可查——故单独锁一条。
    #[test]
    fn codec_id_discriminants_are_stable() {
        for (codec, want) in [(CodecId::Vp9, 0u32), (CodecId::H264, 1)] {
            let bytes = bincode::serialize(&codec).unwrap();
            let got = u32::from_le_bytes(bytes[..4].try_into().unwrap());
            assert_eq!(got, want, "CodecId::{codec:?} 的判别式错位");
        }
    }

    /// **`Hello` / `HelloAck` 字节恒等测试**：固定输入编码后比对精确字节向量。
    ///
    /// 锁的不变量（计划「硬约束 1」）：这两个结构**一个字都不能加、不能改**，`DisplayInfo`
    /// （内嵌在 `HelloAck.displays`）也一样。它们承载「混合版本先解出 `Hello`、读到
    /// `proto_version`、再明文干净拒绝」这条路径；改坏它，混合版本就会退化成 bincode 解码失败
    /// 导致的硬断链。
    #[test]
    fn hello_and_helloack_wire_bytes_are_frozen() {
        assert_eq!(
            bincode::serialize(&frozen_hello()).unwrap(),
            FROZEN_HELLO_BYTES,
            "Hello 的线上字节变了——混合版本的干净拒绝方案会塌掉"
        );
        assert_eq!(
            bincode::serialize(&frozen_hello_ack()).unwrap(),
            FROZEN_HELLO_ACK_BYTES,
            "HelloAck 的线上字节变了（含内嵌的 DisplayInfo）——同上"
        );
    }

    #[test]
    fn short_length_prefix_awaits_more_data() {
        // 锁的不变量：长度前缀不足 4 字节时必须返回 `Ok(None)`（半包，等待更多数据），
        // 而不是报错或 panic——读循环据此保留缓冲、下次读齐了再解。
        for n in 0..LENGTH_PREFIX {
            let mut dec = SessionCodec::new();
            let mut buf = BytesMut::from(&vec![0u8; n][..]);
            assert!(
                dec.decode(&mut buf).unwrap().is_none(),
                "长度前缀只有 {n} 字节时应为半包（Ok(None)）"
            );
        }
    }

    #[test]
    fn key_event_roundtrips_through_input() {
        let ev = InputEvent::Key(KeyEvent {
            code: 65,
            pressed: false,
            text: Some("A".into()),
            modifiers: 0b001,
        });
        let got = codec_roundtrip(SessionMessage::Input(ev.clone()), Lane::Control);
        assert_eq!(got.message, SessionMessage::Input(ev));
    }

    #[test]
    fn oversize_frame_is_rejected_on_encode_and_decode() {
        // 编码侧：载荷超过上限。
        let mut enc = SessionCodec::new();
        enc.set_max_frame(8);
        let mut wire = BytesMut::new();
        let big = SessionMessage::VideoPacket {
            display: 0,
            seq: 0,
            codec: CodecId::Vp9,
            flags: 0,
            width: 1,
            height: 1,
            data: vec![0u8; 64],
            capture_ts_us: 0,
        };
        assert!(enc.encode(SessionFrame::screen(big), &mut wire).is_err());

        // 解码侧：伪造一个声明超大长度的头。
        let mut dec = SessionCodec::new();
        let mut buf = BytesMut::new();
        buf.put_u32(u32::try_from(MAX_SESSION_FRAME + 1).unwrap());
        buf.put_u8(0);
        assert!(dec.decode(&mut buf).is_err(), "超限长度应被拒绝");
    }

    #[test]
    fn zero_length_frame_is_rejected() {
        let mut dec = SessionCodec::new();
        let mut buf = BytesMut::new();
        buf.put_u32(0);
        assert!(dec.decode(&mut buf).is_err(), "长度为 0 缺少 lane，应拒绝");
    }

    #[test]
    fn invalid_lane_is_rejected() {
        let mut dec = SessionCodec::new();
        let mut buf = BytesMut::new();
        // 长度 2：1 字节 lane + 1 字节垃圾载荷。
        buf.put_u32(2);
        buf.put_u8(0xFF);
        buf.put_u8(0x00);
        assert!(dec.decode(&mut buf).is_err(), "未知 lane 应拒绝");
    }

    #[test]
    fn malformed_payload_is_rejected() {
        let mut dec = SessionCodec::new();
        let mut buf = BytesMut::new();
        // 合法 lane、非法 bincode 载荷。
        buf.put_u32(4);
        buf.put_u8(0);
        buf.put_slice(&[0xFF, 0xFF, 0xFF]);
        assert!(dec.decode(&mut buf).is_err(), "非法 bincode 应拒绝");
    }

    #[test]
    fn split_delivery_reassembles() {
        let mut enc = SessionCodec::new();
        let mut wire = BytesMut::new();
        enc.encode(
            SessionFrame::control(SessionMessage::Heartbeat { t1_us: 7 }),
            &mut wire,
        )
        .unwrap();
        let tail = wire.split_off(2);

        let mut dec = SessionCodec::new();
        assert!(dec.decode(&mut wire).unwrap().is_none(), "半包应返回 None");
        wire.extend_from_slice(&tail);
        assert_eq!(
            dec.decode(&mut wire).unwrap().unwrap().message,
            SessionMessage::Heartbeat { t1_us: 7 }
        );
    }

    #[test]
    fn encrypted_frames_roundtrip_and_differ_from_plaintext() {
        let key = [5u8; 32];
        let mut enc = SessionCodec::new();
        enc.set_cipher(Cipher::new(key).unwrap());
        assert!(enc.is_encrypted());
        let mut wire = BytesMut::new();
        enc.encode(
            SessionFrame::control(SessionMessage::Bye { reason: "x".into() }),
            &mut wire,
        )
        .unwrap();

        // 明文编解码器读密文应失败（证明真的加密了）。
        let mut plain = SessionCodec::new();
        let mut copy = wire.clone();
        assert!(plain.decode(&mut copy).is_err());

        // 正确密钥可解。
        let mut dec = SessionCodec::new();
        dec.set_cipher(Cipher::new(key).unwrap());
        let got = dec.decode(&mut wire).unwrap().unwrap();
        assert_eq!(got.message, SessionMessage::Bye { reason: "x".into() });
    }

    #[test]
    fn plan_controller_handshake_encodes_a_hello_frame() {
        let plan = plan_controller_handshake("sess-1", "tok", "123456789").unwrap();
        let mut dec = SessionCodec::new();
        let mut buf = BytesMut::from(&plan.hello_frame[..]);
        let frame = dec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(frame.lane, Lane::Control);
        match frame.message {
            SessionMessage::Hello {
                session_id,
                proto_version,
                controller_rd_id,
                x25519_pub,
                ..
            } => {
                assert_eq!(session_id, "sess-1");
                assert_eq!(proto_version, SESSION_PROTO_VERSION);
                assert_eq!(controller_rd_id, "123456789");
                assert_eq!(x25519_pub.len(), 32);
            }
            other => panic!("应为 Hello，实得 {other:?}"),
        }
    }
}
