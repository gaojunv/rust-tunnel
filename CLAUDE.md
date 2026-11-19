# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 项目概述

rust-tunnel 是一个基于 Rust 的客户端-服务器内网穿透工具，配有 React/TypeScript 前端管理界面。服务器运行在公网，暴露端口将流量通过加密控制通道转发到内网客户端。同时内置 Shadowsocks 和 Trojan 代理服务器、实时连接质量监控、LLM 网关（含 RAG 知识库）和 AI agent 工作台（服务端 agent 循环 + 隧道内工具执行）。

## 常用开发命令

### 后端（workspace：crates/common、crates/client、crates/server）
```bash
cargo build                      # 调试构建（默认 feature，不编译 qdrant-edge）
cargo build -p rust-tunnel-server --features rag   # 含 RAG 的完整构建
cargo check                      # 快速编译检查
cargo test -p rust-tunnel-common --lib
cargo test -p rust-tunnel-client --lib
cargo test -p rust-tunnel-server --lib        # 服务器单测（无 RAG，快速）
cargo test -p rust-tunnel-server --lib --features rag   # 含 RAG 单测
cargo test -p rust-tunnel-remote --lib        # 远程控制引擎单测（纯协议，秒级）
cargo test                       # 根目录：e2e 集成测试（dev-dep 带 rag feature）
cargo test -j 2                  # 内存限制：-j 是 cargo 构建并行度（e2e 编译含 qdrant-edge 时尤其需要）
cargo clippy -p rust-tunnel-server
cargo run -p rust-tunnel-server --features rag -- --bind 0.0.0.0:8080
cargo run -p rust-tunnel-client -- --server localhost:8080 --password <token> --name home-nas
cargo run -p rust-tunnel-server --bin checkdb  # SQLite 数据库诊断工具
```

### 前端
#### 注意 node 和 npm 通过nvm安装
```bash
cd frontend
npm install                    # 安装依赖
npm run dev                    # 开发服务器（Vite HMR，/api 代理到 localhost:3000）
npm run build                  # tsc 类型检查 + Vite 构建
npm run lint                   # ESLint 检查（--max-warnings 0）
npm test                       # Vitest 单元/组件测试（jsdom 环境）
```

### 部署前端到嵌入式资源
```bash
cd frontend && npm run build && rm -rf ../frontend-dist && cp -r dist ../frontend-dist
```

### 构建缓存治理
```bash
du -sh target                       # 查看缓存体积（分解 debug/incremental）
cargo clean -p rust-tunnel-server   # 定点清理单个 crate 的产物/增量缓存
cargo clean                         # 全量清空（磁盘告警时；重建约需数分钟）
# 可选后续：sccache 对象级缓存（跨目标/CI 复用，清仓重建也快）
```

### AI开发模型
进行开发时，使用worktree进行代码隔离，使用subagent模型进行代码开发

## 架构

### Cargo workspace：crate + 根元包
- 根元包 `rust-tunnel` 仅托管 e2e 测试（`tests/`），无实现代码
- `crates/common`（`rust-tunnel-common`）— 协议/TLS/错误/日志/mesh + `DEFAULT_PTY_PORT`
- `crates/client`（`rust-tunnel-client`）— 客户端 lib + bin
- `crates/client-gui`（`rust-tunnel-client-gui`）— 桌面托盘客户端（eframe/egui + winit + tray-icon），「状态/映射/设置/日志/远程」五个 Tab
- `crates/server`（`rust-tunnel-server`）— 服务端 lib + bins（server、checkdb）
- `crates/rustdesk`（`rust-tunnel-rustdesk`）— 内置 RustDesk 兼容服务（hbbs 会合 + hbbr 中继）的**独立协议库**（`frame`/`proto`/`addr`/`key`/`peer`/`relay`/`rendezvous`）
- `crates/remote`（`rust-tunnel-remote`）— GUI 客户端互相远程控制的**协议 + 引擎**（会话协议/端到端加密/信令/会话循环/VP9 4:4:4 软件编码/H.264·H.265 4:2:0 硬件编解码抽象与协商/文件传输），**无平台或 UI 依赖**（但含 vendored 的便携 C 编解码器 `vendor/shiguredo_libvpx`）
- `crates/win-platform`（`rust-tunnel-win-platform`）— Windows 无人值守被控端的**平台层**：可移植模块（`error`/`pixels`/`report`/`session`/`desktop`/`monitor`/`inject`/`token`/`sysinfo`，纯 std、macOS 上全量单测）+ 仅在 Windows 编译的裸 FFI 模块（`capture`/`handle`/`service`/`probe`/`process`）。用 `windows-sys`（无 wrapper、无 libvpx），故本机可 `--target x86_64-pc-windows-gnu` 做**类型**检查
- `crates/win-service`（`rust-tunnel-client-service`）— Windows 无人值守被控端的 **SYSTEM 服务** bin（哑监督者：SCM 注册/安装卸载/启停、把无头实例拉进控制台会话、随会话变化重启）。`--run` 是本体（监督 `rust-tunnel-client-gui.exe --instance`）；`--install` 把两个 exe 复制到 `%ProgramFiles%\rust-tunnel\` 并加固 DACL（**服务以 SYSTEM 运行，exe 放在任何用户可写的目录里 = 替换 exe 即拿到 SYSTEM**），可选 `--host-config <准备好的 host.toml>`（安装时落盘）与 `--start`（装完即启动）；`--probe`/`--probe-in-service`/`--probe-instance` 是阶段 0 的真机探针，保留
- `crates/win-video`（`rust-tunnel-win-video`）— Windows 远程控制**硬件数据面**：WGC（优先）/ DXGI Desktop Duplication（回退）采集（自顶向下 BGRA）、NVENC **H.264/H.265 4:2:0** 硬件编码（运行期 `nvEncodeAPI64.dll`，无链接期驱动依赖）、Media Foundation 硬件编码 MFT（H.264/H.265）、`MSH264DecoderMFT`/`MSH265DecoderMFT` 解码（NV12→RGBA，BT.709 limited）；`src/codec.rs` 的 `CodecKind{H264,H265}` 是编码标准的统一维度。**不依赖 `remote`、不依赖 libvpx**，因此可在 MSVC 上独立编译/单测；真机探针在 `src/bin/`（`win-video-probe`/`capture-probe`/`decode-probe`/`e2e-probe`）。**实例模式在 `Default` 桌面也复用它**（混合采集，见下文），安全桌面仍回退 BitBlt。`win-video-capture-probe` 会打印 PID/用户/会话/当前线程桌面并落盘报告（默认 `%ProgramData%\rust-tunnel\probe\`）；跑之前自己声明 per-monitor DPI 感知（否则 4K@150% 会被报成 2560×1440，而 WGC 实际采到 3840×2160）
- `crates/win-audio`（`rust-tunnel-win-audio`）— Windows 远程控制**音频数据面**：默认回放设备的 WASAPI loopback 采集、默认输出设备的播放（独立线程 + 抖动缓冲 60–120 ms + MMCSS）、Opus 48 kHz 立体声 20 ms 帧 96 kbps（vendored libopus 静态库）。**不依赖 `remote`**（引擎只认 `AudioSource`/`AudioSink` trait），`client-gui` 在 Windows 上把两端注入管理器。单测覆盖抖动缓冲/Opus 往返/度量；真机探针 `win-audio-probe devices|capture|rt|tone|playtest`
- 依赖单向：`common ← client`、`common ← server`、`common ← remote ← client-gui`；`rustdesk` **不依赖任何本仓 crate**（只依赖 prost/ed25519-dalek/bytes/tokio）；`win-platform ← win-service`、`win-video ← client-gui`、`win-audio ← client-gui`。`win-platform` **不得**依赖 `remote`（否则 libvpx 会被拖进服务 exe），`win-video`/`win-audio` **也不得**依赖 `remote`（否则会话协议与 libvpx 会拖进硬件/音频层，MSVC 下无法独立测试）
- **Windows 锁屏/登录界面为什么必须走服务**：`WinSta0\Winlogon` 桌面的 ACL 只放行 LocalSystem（Administrators 不在名单里）+ UIPI（LogonUI 是 System 完整性）+ 会话隔离（服务在会话 0）。**管理员身份运行无效**。详见根目录 `WINDOWS-SECURE-DESKTOP.md`（含架构、部署、真机验收清单与探针闸门）
- **`rag` 为非默认 feature**（门控 qdrant-edge 与知识容器的向量索引侧）；完整服务需 `--features rag`，CI 构建用 `--features rag,embed-frontend`

### 核心数据流
1. 客户端通过控制通道（加密 TLS）向服务端注册，提供名称和密码
2. 管道路由由服务端 Web 管理的"反向代理规则"定义（Backend `kind=Direct` 直连外部，`kind=Client` 通过客户端内网穿透）
3. `ClientConnector` → `ClientRegistry.open_tunnel` → `ClientTunnelStream` 在控制通道上搭建 `AsyncRead/Write` 流
4. 反向代理的 TCP/HTTP handler 通过 `Connector` trait 统一调用，无需关心 backend 是直连还是 tunnel

### 模块职责

**`crates/common/src/`** — 共享协议和基础设施工具
- `protocol.rs` — `ControlMessage` 枚举，长度前缀 bincode 序列化（最大 1MB）
- `error.rs` — `TunnelError`（10 个变体）和 `TunnelResult`
- `tls.rs` — 自签名证书生成（Ed25519）、TOFU/安全模式客户端配置
- `logging.rs` — 日志初始化

**`crates/server/src/`** — 服务器实现
- `control_plane/` — 控制通道、`ServerState`、`ClientRegistry` 管理、消息分发、心跳质量监控、SS/Trojan 端口追踪
- `protocols/` — 代理协议实现：
  - `shadowsocks.rs` — 内置 SS 代理：`shadowsocks-rust` crate，AES-256-GCM / ChaCha20-Poly1305，EVP_BytesToKey 密钥派生
  - `trojan.rs` — 内置 Trojan 代理：TLS 必需、SHA-224 认证、增量解析（`ParseResult`）、认证失败回退
  - `trojan_runtime.rs` — SS/Trojan 运行时管理与连接生命周期
  - `reverse_proxy/` — 反向代理子系统，含规则、路由、HTTP/TCP handler、Connector trait、直连/隧道连接、SNI 分发
- `persistence/` — 数据持久化（SQLite WAL 模式）：流量、质量、会话、clients、SS/Trojan 配置、日志、RAG（kb/docs/chunks）
- `mgmt/` — 管理面：
  - `api/` — Axum API 路由 + `rust-embed` 嵌入前端
  - `auth.rs` — JWT 认证
  - `logs.rs` — 自定义 tracing Layer，捕获日志到内存 + SQLite，API 支持分页/过滤
  - `stats.rs` — 实时质量监控、RTT/丢包/吞吐量追踪、评分（0-100）、阈值告警、历史采样
  - `dynamic_config.rs` — 动态配置管理
- `llm/` — LLM 网关：OpenAI/Anthropic/Responses 三协议入口（`openai_handler.rs`/`anthropic_handler.rs`/`responses_handler.rs`，后者经 `responses.rs` 做 Responses API 与内部 chat completions 格式双向转换）、provider/model/api-key 管理、用量日志、compat 工具调用改写；`llm/rag/` — RAG 知识库：`extractor`（多格式文本提取：PDF/Word/Excel/PPT→Markdown）、`chunker`（Markdown 分块）、`embedder`（远端 embedding）、`store`（qdrant-edge 向量 shard）、`retriever`（检索+注入）、`ingest`（后台摄入任务）
- `pki/` — 证书与 ACME 自动续签
- `net/` — 网络基建（listener/dns/mesh）
- `agent/` — AI agent 工作台：`runner`（自研 agent 循环/回合，作为未配置 agent_type 的 workspace 的回退运行时路径；ACP 为主路径）、`tools`（工具 schema：shell/read_file/read_file_range/write_file/patch_file/list_dir/search/git_*/code_outline/read_symbol，工具经隧道在内网客户端执行）、`executor`（命令执行）、`approval`（审批矩阵：危险工具挂起等待用户批准，支持"本会话记住"）、`session`/`title`/`compact`（会话管理、自动标题、上下文压缩）、`sse`（WebSocket 事件流）、`spawner`（ACP 路径：经控制通道 negotiate AgentSpawnRequest/AgentLlmProxyStart，在客户端 spawn agent 进程）、`acp_bridge`（ACP 路径：管理 ACP session 生命周期、stdio pump、idle reaper、断线恢复）、`acp_events`（ACP `SessionUpdate` → 现有 WS 帧映射）、`llm_bridge`（AgentLlmProxyRequest → 服务端 LLM 网关转发，服务端注入认证）、`roles`（多角色子代理：`agent_roles` 表定义角色 persona——系统提示词/工具白名单黑名单/模型覆盖/mode=subagent|primary|all/scope 仿 agent_skills；task 工具 `agent` 参数调度子代理角色，主会话可经 session.role_id 或 `@role-name` 前缀切换 primary 角色；工具过滤叠加顺序 = 角色过滤 ∩ plan 模式裁剪，子 agent 强制剔除 task/todo_write；内置角色 general/explore 启动时 seed）。`AgentState` 挂在 `ServerState` 上，含 per-workspace 执行锁（git 状态安全）和 per-session 回合锁（多标签页/重连防并发写库）
- `config/` — 服务器配置（Clap + figment（TOML）+ 环境变量，三级优先级）
- `rustdesk_runtime.rs` — RustDesk 会合/中继运行时：`apply_rustdesk_config`（停旧→绑 4 个监听→起 4 个任务）、`nat_test_port()`；端口由本模块绑定（协议库只做协议处理）

**`crates/client/src/`** — 客户端实现（零配置范式的端侧）
- `control.rs` — 建立控制连接、TLS、密码认证、`Register{protocol_version:2, name, password, version}`、`ClientState`（pending→active 连接管理）、分发 `OpenTunnel`/`Data`/`Close`/`Disconnect`
- `proxy.rs` — `handle_open_tunnel`：收到 OpenTunnel 后 `TcpStream::connect(target_addr)`，TunnelOpenResult 反馈，双向 shuttle 转发
- `config.rs` — 同上三级配置优先级，但只保留 `server/password/name/tls*` 和 `mesh*`，删除 `forwards`
- `logs.rs` — 客户端日志收集

**`crates/rustdesk/src/`** — 内置 RustDesk 兼容服务（hbbs 会合 + hbbr 中继）的**独立协议库**
- 独立实现线上协议（上游 `rustdesk-server` 是 AGPL，本项目 MIT/Apache；只对齐字段号/端口/帧格式，代码自有）；不引 libsodium C 依赖，Ed25519 走 `ed25519-dalek`（与 NaCl 私钥布局逐字节兼容）
- `frame.rs` — 长度前缀帧编解码（可变长头部；配对成功后切裸字节模式）；`proto.rs` — 手写 `prost` derive 的消息定义（无需 protoc）；`addr.rs` — 地址 `AddrMangle` 编解码；`key.rs` — `ServerKey`（Ed25519 签 `IdPk` 绑定 + `licence_key` 明文比对）；`peer.rs` — 在线对端登记表（含 per-ID / per-IP 限流）；`relay.rs` — hbbr 中继；`rendezvous.rs` — hbbs 会合（注册/心跳/打洞协调/中继调度）
- **不依赖 `stats`、不参与 `PortRegistry`/SNI 路由**：端口是固定的 21115/21116/21117，既不随反代规则分配、也不进 SS/Trojan 的端口表，故不登记端口；运行时胶水（绑定监听、统计轮询）在 `crates/server/src/rustdesk_runtime.rs`，管理面在 `crates/server/src/mgmt/api/rustdesk.rs`
- 端口表（改主端口即整体平移；NAT 探测 = 会合端口 - 1）：

| 端口 | 组件 | 传输 | 用途 |
|---|---|---|---|
| 21115 | hbbs | **TCP + UDP** | NAT 类型探测（= `rendezvous_port - 1`；UDP 半边供客户端自判 NAT 映射类型） |
| 21116 | hbbs | **TCP + UDP** | 注册/心跳/打洞协调/NAT 探测（UDP 半边是打洞成功的关键） |
| 21117 | hbbr | TCP | 中继数据面（裸字节转发） |

- 有意未实现：WebSocket 端口 21118/21119（Web 客户端不可用，原生桌面端不受影响）、WebRTC 通道、`KeyExchange`/`kx_version` 前向安全升级
- 部署/防火墙/密钥/真机验证清单见根目录 `RUSTDESK.md`（`docs/` 被 gitignore，不放交付文档）

**`crates/remote/src/`** — GUI 客户端互相远程控制的协议 + 引擎（**无平台/UI 依赖**；但含内联的便携 C 编解码器 `vendor/shiguredo_libvpx`，见下）
- `session.rs` — 会话帧（4 字节大端长度前缀 + bincode，上限 16 MiB——屏幕帧远大于 `common::protocol` 的 1 MiB）+ `Lane{Control,Screen,File,Audio}` 多路复用车道（`File` 文件传输，`Audio` 音频）。**`SESSION_PROTO_VERSION` 已到 11**（v11 追加音频转发：`Lane::Audio` 与 `AudioStatus`/`AudioPacket`/`SetAudioEnabled`；v10 追加 `CodecId::H265` 与 `CodecOffer.h265`，协商优先 H.265/HEVC，建编码器失败时运行时回退 H.264→VP9；v8 追加 `HostStats` 遥测、心跳时间戳与采集时间戳；v9 追加 `CursorShape` 远端光标形状；阶段 5 引入 `CodecOffer`/`CodecSelect` 协商与 `CodecId::H264`；v4 追加控制端可选的 `SetFrameRate { fps }` 帧率上限；v5 把键盘输入改为**键位码注入**——可打印键不再发送 Unicode 文本，改由被控端的键盘布局与输入法解成字符），两端靠 `Hello.proto_version` 硬相等实现「混合版本明文干净拒绝」——`Hello`(判别式 0)/`HelloAck`(1) 与内嵌的 `DisplayInfo` **逐字节冻结**（`discriminant_golden` + `hello_and_helloack_wire_bytes_are_frozen` 两条机械锁），新变体一律追加在末尾
- `crypto.rs` — X25519 临时密钥交换 + ChaCha20-Poly1305，双向独立密钥与 nonce 计数器。**一次性 token 进 KDF（别去掉）**：裸 ECDH 挡不住中继方——`Hello`/`HelloAck` 都是明文，它可替换两端临时公钥做完整 MITM；token 经加密控制通道下发且从不上数据面，攻击者不知道它就算不出密钥 → 两端密钥必然不一致 → 首帧 AEAD 认证失败。另拒绝低阶点（`was_contributory`）
- `auth.rs` — 握手 HMAC-SHA256，字段用 **8 字节长度前缀**（`nonce`/公钥是二进制，固定分隔符会有拼接歧义）；`Hello` 的 MAC 覆盖控制器临时公钥，`HelloAck` 的 `hmac_ack` 绑定被控端临时公钥，控制器**真正校验**它；挂起会话 TTL 60s、鉴权成功即消费（重放被拒）、MAC 失败保留以允许重试
- `signaling.rs` — RustDesk 会合/中继客户端（注册/心跳/打洞/中继请求）；`RustDeskTransport` 是真实数据面建立的唯一实现，`LazyTransport` 把它的构造推迟到 `RemoteParams` 到手（GUI 与 e2e 共用）
- `transport.rs` — 数据面传输抽象（`BoxDuplex`/`EstablishRequest`/`TransportEstablisher`），把「建立一条双向字节流」与「会话跑在这条流上」解耦；**建立契约**写在模块文档里（中继路径只发一帧 `RequestRelay` 后转裸字节、不等应答，并把 `early` 与之合并写出）。`TransportPath{Direct, Relay, Punch}` 的硬契约是「**必须如实上报实际走的路径**」——中继兜底绝不能报成 `Punch`（UI 据此告诉用户字节有没有过服务器）；`EstablishRequest.punch_cookie: [u8; 8]` 是打洞会话标识，**Rust 内部类型、不上线**
- `reliable.rs` — **用户态可靠层**（UDP 打洞路径专用）：最小 TCP 式 ARQ over datagrams。27 字节段头（`magic "RTP1"` / `conn_id` 固定在**偏移 4**——demux 只读前 12 字节即可路由，配机械锁防日后挪位 / `seq` 段序号 / 累计 `ack` + 16 位位图 / `flags` / `window` / `len`）、保序重组、延迟确认 50ms、Jacobson/Karels RTO（min 200ms / max 2s / 指数退避）、**3-dup-ack 快速重传必须早于 RTO**、AIMD cwnd、接收窗口流控、FIN/RST。载荷上限 **1200 B**（`27+1200 = 1227` ≤ IPv6 最小 MTU 预算，**任何情况下不触发 IP 分片**）。返回 `tokio::io::duplex()` 的一半 → 天然 `AsyncRead+AsyncWrite+Unpin+Send`，可直接塞进 `BoxDuplex`，且**背压是自动的**（应用不读 → 协议任务写不动 → 停止确认 → 对端窗口填满）。**为什么不用动 `session.rs`**：会话帧自带长度前缀、AEAD 又是**逐帧**的，帧边界由长度前缀恢复，所以可靠层只需提供**纯字节流**、不必保留 write 边界。**拥塞控制只做「保守地板」**——码率主控是 `qos.rs` 的应用层闭环，别让传输层跟它抢方向盘。测试用内存链路替身 + `start_paused` 虚拟时钟（dev-dep 需 `tokio/test-util`）
- `punch.rs` — 打洞的会话相关性与分发：`conn_id = HMAC-SHA256(token, b"rt-punch-v1")[..8]`（大端 u64；由一次性 `token` 派生 ⇒ hbbs 的**明文**会合流量里看不到它，第三方注入不进会话、也抢占不了打洞路径）；`peek_conn_id` **只读前 12 字节**给 demux 走快速路径；`PunchRegistry` 按 `conn_id` 路由，**未命中一律静默丢弃、绝不回复**（反反射）。`PUNCH_PROBE_INTERVAL = 250ms` / `PUNCH_BUDGET = 3s`。**`punch_back` 是必需的、不是优化**：多数家用路由是**端口受限 cone**（NAT 只放行「本机先发过包的目的地址」来的入站报文），若只有控制器单向打，被控端的 NAT 会丢掉控制器的 `SYN`（被控端从未向它发过任何东西）→ 死锁。故被控端收到 `PunchHole` 后**同时反向发探测**（裸 27 字节段、`conn_id = 0`、`flags = 0`，每 `PUNCH_PROBE_INTERVAL` 一发、共用同一 `PUNCH_BUDGET`）打开本端 NAT 的过滤窗口；控制器侧本来就在持续重发 `SYN`，两侧因此天然对称。`conn_id = 0` 是安全哨兵（demux → 登记表未命中 → protobuf 兜底 → 丢弃，**不回复**）；反向探测的目标端口取 `PunchHole.udp_port`（**服务端观测到的公网端口**，不是本地端口——报错它等于打到一个没有 NAT 映射的端口上），`udp_port == 0` 时不反向打
- `nat.rs` — 打洞前的 NAT 预判：`classify(a, b)` 是**纯函数**——从**同一个**套接字向**两个不同目的端口**（`21116/udp` 与 `21115/udp`）各探一次，比对观测到的源端口：相等 → cone（放行）、不等 → 对称（**跳过打洞、零额外等待**）、探不到 → `Unknown`（**fail-open，照打一次**）。**必须用两个目的端口、而不是两个本地套接字**：对称 NAT 对每个 `(源IP,源端口,目的)` 三元组独立映射，两个本地套接字本来就必然得到两个不同映射，**根本区分不出来**。**两个端口都必须应答**——`handle_datagram` 原本只认 `RegisterPeer`/`RegisterPk`（`TestNatRequest` 落进兜底被静默丢弃），所以主 UDP 端口也补了 `TestNatRequest` 应答，否则预判永远得到 `Unknown`。残余局限：两个目的端口同 IP，故「地址无关但端口依赖」的映射会被误判成 cone（现实中罕见，由 `NatCache` 兜住）。`NatCache` 按对端 rd_id 缓存失败 10 分钟，**只在进程内**——随传输重建自然失效，刻意不放偏好文件
- `controller.rs`/`controlled.rs` — 两侧会话循环（握手 → 画面/输入/心跳/文件）；控制器把 `Hello` 帧流水线进建立阶段，因此**会话循环不再发 `Hello`，只等 `HelloAck`**。UI 命令统一走 `ControllerCommand`（输入/切显示器/切画质/剪贴板/文件），单通道保证顺序与用户操作一致。**画面流水线**（被控端）：`采集(spawn_blocking) --有界(1)--> 编码任务(spawn_blocking) --有界--> 发送(独占 writer)`，`select!` 循环只留 cancel/心跳/输入/文件/控制。**丢弃策略两端方向相反**：被控端**编码之后绝不丢帧**（时域编码参考链一断就花屏且不自愈），背压出口只能在**编码之前**（队列满就跳过这一帧）；控制器可以丢，但必须按**序号断层**检测到并请求关键帧（`RequestKeyframe`），进入 `awaiting_keyframe` 只等 IDR
- `screen.rs` — `ScreenSource`/`InputSink`/`FrameSink`/`Clipboard` 平台 trait；`FrameSink::on_frame` 拿到的**一律是整帧** RGBA（瓦片路径已删除），`on_clipboard` / `on_cursor` 默认空实现，控制器侧不必实现 `Clipboard`（它读写的是**本机**剪贴板，由 UI 直接处理）。`CursorShape`（协议 v9）也定义在这里：`ScreenSource::cursor_shape()` 默认 `None`（平台不支持/读不到），被控端每拍采集后采样、变化时经 `SessionMessage::CursorShape` 下发；判别式顺序是线上契约（`cursor_shape_discriminants_are_stable` 机械锁）。剪贴板是**单向按需**的：只有控制器会发 `ClipboardRequest`（被控端只应答），控制器侧收到它是异常，只记日志
- `video.rs` — VP9 4:4:4 编解码封装 + RGBA↔YUV444 转换（BT.709 **full range**，与编码器补丁 5 设的 `color_space`/`color_range` **严格一致**，两端共用同一份常数）。关键参数：`Profile1`（补丁 1，上游表达不了）、`lag_in_frames = Some(1)`（**必设**，`None` 会让 libvpx 缓冲前 25 帧）、`cpu_used = 8`、`rc_buffer_size_ms ≈ 300`（补丁 4）、`EncodingDeadline::Realtime` + `Cbr`、`tile_columns`（按宽度分档，见 `tile_columns_for`）+ `row_mt`（补丁 7）。**默认编码宽度 ≤ `MAX_ENCODE_WIDTH`（1920）**——4K 解码仍是最贵的一环，只能作显式选项。实测 libvpx 在 realtime+CBR 下**不**按场景切换自动插 IDR（`keyframes_only_at_the_configured_interval` 锁定），故无需 `kf_min_dist` 补丁。`Encoder`/`Decoder` 是 `Send` 非 `Sync`（编译期断言钉住），只能独占移进 `spawn_blocking`。`set_bitrate`/`set_fps` 走**在线重配置**（补丁 6 → `vpx_codec_enc_config_set`），**不重建编码器、不插 IDR**——这是 QoS 闭环的前提，见 `qos.rs`
- **解码并行化（补丁 7）**：`VideoDecoder::new()` 走 `DecoderConfig.threads = video_threads()` + `row_mt = true`，编码侧开 `tile_columns` + `row_mt`（两者都**不改线上格式**，见下）。本机 10 核 Apple M 系列实测（`video_bench_encode_decode`，release，合成内容）：解码 1080p 4.23→2.61 ms、4K 16.17→9.86 ms（**−38% / −39%**，同一份代码只翻这两个开关的 A/B）；瓦片顺带让**编码**快 17–20%（编码器也能跨瓦片并行），CBR 下码流字节基本不变（4K 反而 −1.7%）。**局限**：CBR 下字节数由码率控制决定，瓦片牺牲的熵效率会表现为同码率下画质略降而非字节变多——「字节没涨」不等于「压缩率没代价」，未测 PSNR。**绝对数字不可外推**：合成内容比真实桌面轻，且计划里 4K「37–58 ms」是另一台 M4 的单线程微基准，与上表不同机不同内容
- `qos.rs` — QoS 闭环（阶段 3）：控制器每秒回报的 `QosFeedback{rtt_ms, recv_kbps, decode_ms, backlog}` + 被控端本地出站队列积压 → AIMD 决策。**两条硬约束**：① 码率/帧率走**在线重配置**（补丁 6），**绝不重建编码器**——重建会强制 IDR，闭环每秒动一次就是每秒一个关键帧，正好把阶段 2 消掉的带宽尖峰引回来；只有**降编码分辨率**（几何变化）才重建 + 强制 IDR。RTT 判据是**相对会话基线**的：`baseline = 会话内最小 RTT`，只有 `rtt > max(baseline + 120 ms, 150 ms)` 或持续积压才判拥塞；`rtt_ms = 0` 是「尚无心跳样本」哨兵，不参与基线；帧率地板 15。当前档位上限：流畅 20 fps / 2 Mbps、均衡 30 fps / 6 Mbps、高清 60 fps / 10 Mbps。② 降级阶梯是 码率 ×0.8 → 帧率 −1 → 分辨率 ×0.75，且**只有解码耗时 ≥ 37 ms**才走最后一级（37 这个数来自 M4 单线程微基准，**不是实测链路数据**；补丁 7 上多线程解码后它更难触发，是否仍是个有效阈值只能靠真机数据判断）；回升加性 ×1.1，不越过档位上限。`QualityPreset` 语义因此从「固定参数」变成**上限**（`EncodeOptions`），实际生效值由 `EncodeRates` 表示，两者在类型上不可混用。编码/解码侧读数经既有进程内 `SessionStats` 旁路送 UI（**不落线**）。机械锁：`rate_change_reconfigures_online_without_idr` / `resolution_change_rebuilds_and_forces_keyframe` + vendor 侧两条
- `file.rs` — 被控端共享目录的沙箱文件服务（虚拟 `/` 路径）。**两层校验缺一不可**：`validate_virtual_path` 管语法（拒 `..`/`.`/`\`/盘符/控制字符/空段），`FileService::resolve` 管语义（对**最深的已存在祖先**做 canonicalize 后 `Path::starts_with(canonical_root)`，故符号链接逃逸会被拒）。`list` 里指向根外的符号链接项直接跳过
- `transfer.rs` — 会话内文件传输状态机（两端共用）。只有**控制器**能发起（浏览/下载/上传），被控端只应答。同一时刻只允许一次传输；`CHUNK_SIZE=64 KiB` 且每块之间 `CHUNK_PAUSE=4ms` **主动让出调度**，会话循环的 `select!` 才有机会插入画面帧（中继只给一条字节流，画面与文件天然共享带宽）
- `approval.rs` — 被控端**接入确认**策略与待确认请求（`ApprovalPolicy{Auto,Confirm}`、`PendingApproval`、`APPROVAL_TIMEOUT=45s` / `APPROVAL_WAIT_TIMEOUT=60s`）。`Confirm` 是默认：被控端收到 `RemoteSessionIncoming` 后**不回响应、不登记 token、不派生任务**，只挂起等用户 `approve_pending`/`reject_pending`，到点自动拒绝。**token 绝不进 `PendingApproval`**——那是 UI 状态，凭证只在管理器内部持有。`APPROVAL_TIMEOUT` 必须明显小于服务端 `REMOTE_SESSION_TTL`（60s，见 `crates/server/src/control_plane/remote.rs`）：用户拍板的**整个过程**都要落在那个窗口内，否则响应会被服务端当未知会话丢弃，用户看到的是「卡住」而不是「被拒」
- `manager.rs` — `RemoteManager` 门面（`RemoteUiState` watch、连接请求、启停、平台注入、剪贴板/共享目录注入、接入确认门控、`RemoteBridge` 实现）
- 平台实现（xcap 抓屏 / enigo 注入 / arboard 剪贴板；Windows GUI 采集用 `crates/win-video` 的 WGC/DXGI）**放在 `crates/client-gui`**，本 crate 保持编译面干净
- `codec.rs` — `VideoEncoderApi`/`VideoDecoderApi`/工厂：会话循环只依赖 trait，具体实现由 `RemoteManagerConfig` 注入；默认仍是 VP9。`VideoDecoderApi::configure(w,h)` 是给 H.264 MFT 的尺寸钩子（`create()` 时还不知道编码尺寸），解码任务在首个包/尺寸变化时调用

**`crates/win-video/src/`** — Windows 硬件数据面（仅 Windows 编译；**不依赖 `remote`/libvpx**）
- `capture/` — WGC（`GraphicsCaptureItem` + 自由线程 frame pool + `TryGetNextFrame`）优先，DXGI Desktop Duplication 回退；`enumerate_monitors`/`open_best`/`CaptureSource`，输出**自顶向下 BGRA**。无新帧时最多等 10ms 后复用上一帧；HDR 桌面下 DXGI 可能拿到 `R16G16B16A16_FLOAT`，此时明确失败并由上层回退 xcap/GDI。采集帧**不含鼠标光标**（WGC 光标捕获已显式关闭），控制器只显示本机光标以避免双光标；远端光标**形状**自协议 v9 起由被控端单独采样（Windows `GetCursorInfo`）并经 `SessionMessage::CursorShape` 下发，控制端用 `set_cursor_icon` 跟随——光标位置仍由本机系统光标渲染，所以移动零额外延迟，也不会出现画面里的双光标。`capture_texture_with_change` 是 GPU 路径的「静止不编码」闸门：WGC 在静止桌面仍按合成器节奏产帧（实测 119/120），所以拿到新帧后还要过 `change.rs` 的**像素级检测**（VideoProcessor → **320×180** NV12 → staging 回读 → 逐字节比，4K 实测 ~0.36 ms/帧）。**网格不能取更稀**：VideoProcessor 大比例缩小时是**点采样**（`D3D11_VIDEO_PROCESSOR_FILTER_*` 没有线性项），128×72 在 4K 上只抽 1/900 的像素，16×16 的变化只有约一半概率被看到；320×180 把密度提到 4 倍而检测耗时不变（960×540 才明显变贵）。残余盲区由引擎侧 250 ms 保活补编兜底（见下）
- `pixels.rs` — `bgra_to_rgba`、`scale_rgba_box`、`scale_bgra_to_rgba`（一步 BGRA→目标尺寸 RGBA；4K→1080p 命中 2×2 快速路径）
- `nvenc/` — 运行期 dlopen `nvEncodeAPI64.dll`；原生绑定 vendored 自 `moq-nvenc` 0.1.2（MIT，bindgen 自 NVIDIA `nv-codec-headers`）；`NvencEncoder` 用 D3D11 纹理 + `NV_ENC_BUFFER_FORMAT_ARGB/ABGR`，**峰值受限 VBR**（均值 = 峰值 × 70%）/超低延迟/无限 GOP/显式 IDR；`set_bitrate`/`set_fps` 走 `nvEncReconfigureEncoder` 不插 IDR；**`inputWidth/Height` 只裁剪、不缩放**
- `mft_encoder/` — Media Foundation **硬件 H.264 编码**（Intel QSV 优先，也覆盖 AMD VCE 等硬件 MFT）：`MFTEnumEx(MFT_ENUM_FLAG_HARDWARE)` + 异步 MFT 事件模型 + D3D11 设备管理器；RGBA→NV12（BT.709 limited）+ Annex B；码率控制优先 **PeakConstrainedVBR**（QSV 的 CBR 会给静止画面插填充，实测 21666 B/帧），不支持时回退 CBR；`set_bitrate` 走 `ICodecAPI` 不插 IDR；**Intel MFT 不支持运行期改帧率**（适配器保持内部帧率，只让采集节奏随 QoS）；1080p 实测 ~10 ms/帧（GPU 纹理输入，墙钟中位）
- `decode/` — Media Foundation `MSH264DecoderMFT`，**DXVA/D3D11 硬件解码优先**（`MFT_MESSAGE_SET_D3D_MANAGER` + `IMFDXGIBuffer` staging 回读），失败自动回退系统内存软件；NV12→RGBA（BT.709 limited 定点），按 `MF_MT_MINIMUM_DISPLAY_APERTURE` 裁掉 16 像素宏块填充；`CoInitializeEx` 线程局部、每次 `decode` 确认
- `d3d11.rs` — 显式选 NVIDIA 适配器建设备（混显下默认适配器可能是核显）；`src/bin/*-probe.rs` — NVENC/采集/解码/全链路真机探针

#### 远程控制的控制面与数据面

**控制面走既有控制通道**（复用 `ControlMessage` + `ClientEntry.control_sender`），不新开 HTTP 鉴权面——GUI 客户端只有客户端 token，没有管理员 JWT。

**向后兼容靠版本门控**：服务端只在客户端版本 ≥ `MIN_REMOTE_CLIENT_VERSION`（`crates/agent/src/runner/version_gate.rs`）时才下发 `RemoteCapability`；客户端只在收到它之后才发新的上行变体。新客户端 + 老服务端 → 老服务端不下发 → 新客户端全程不发新变体；老客户端 + 新服务端 → 门控拦住。两边都不会因 bincode 未知变体而断连。**新增 S→C 推送时必须过 `client_supports_remote`**，否则老客户端反序列化失败会直接断连。

**数据面另有 `SESSION_PROTO_VERSION` 一道门控**（当前 11）：它**不是**控制通道的 `protocol_version`，而是数据面 `Hello` 里的字段，两端**硬相等**校验，不匹配就在握手阶段明文拒绝（`HelloAck{accepted:false}`），因此混合版本不会退化成 bincode 解码失败导致的硬断链——**前提是 `Hello`/`HelloAck`/`DisplayInfo` 的字节形状冻结**（两条机械锁守着，见上）。

**v5（输入键位码）的发布顺序：两端必须同为 v5**（v4 的 `SetFrameRate` 也要求两端同为 v4+）。`SetFrameRate` 只用于「控制端在会话窗口里选 30/60」，被控端据此在线重配置采集节拍与编码器 fps（**不重建、不插 IDR**），QoS 仍可在上限之下降到 `min_fps`（15）；v5 则改了键盘输入的编码语义（可打印键从 Unicode 文本改成完整键位码，见下面「远程控制的输入语义」）。v4 ↔ v5 在握手阶段被 `Hello.proto_version` 硬相等校验干净拒绝（`HelloAck{accepted:false}`），不会退化成 bincode 硬断链；但这意味着**新旧 GUI 客户端之间不能互相远程控制**，所以必须等全部客户端升到 v5 再部署依赖它的版本。`Hello`/`HelloAck`/`DisplayInfo` 的字节形状仍冻结（`FROZEN_HELLO_BYTES` 里的 `proto_version = 3` 是**形状锁**、不是当前版本，别去改它）。阶段 5 的服务端门控 `MIN_REMOTE_CLIENT_VERSION` 抬升仍按原计划走两步：先铺客户端、再部署新服务端并把门控抬到 `(0,12,0)`。**注意客户端上报的是 `CARGO_PKG_VERSION`**，而客户端发布 workflow（`release-client.yml` / `release-client-gui.yml`）的 `VERSION` 只取 git tag、不写回 `Cargo.toml`——所以 `crates/client`/`crates/client-gui` 的版本号要手工抬（本次已抬到 0.12.0）。

**远程控制的输入语义（v5）**：控制端**不再转发 `egui::Event::Text`**——可打印键（字母/数字/标点）改为发送完整键位码（按下+抬起配对），由被控端映射成 Windows VK / macOS CGKeyCode / Linux keysym 注入，最终字符由**被控端**的键盘布局与输入法决定（这正是远端切中文输入法能出中文、且不再被控制端布局带偏的原因）。`egui_key_to_code` 用逻辑键（`Event::Key.key` 的 logical-or-physical fallback）而不是 `physical_key`；上档标点（`:`/`+`/`!` 等）映射到同一物理键、Shift 由修饰键事件单独转发。`Ctrl/Cmd+C/X/V` 被 egui-winit 吞成 `Event::Copy/Cut/Paste`，`InputGate::shortcut_click` 负责把它们合成点按（修饰键已转达就只发键本身，否则临时补一对修饰键按下/抬起）。`egui_modifiers` 的 META **只来自 `m.mac_cmd`**、绝不来自 `m.command`（Windows 上 `command == ctrl`，按 `command` 置位会让每次 Ctrl 都额外按下 Win 键）。键码表分区：0..=15 特殊键、20..=31 F1..=F12、40..=43 修饰键、0x100 起字母/数字/标点、0x140 起 F13..=F24（旧表按「F1 + (n-1)」排会与修饰键撞号）。**已知限制**：winit/egui 不产生 `Win`（Windows 键）/`CapsLock`/`PrintScreen` 事件，故这些键无法透传。

**`crates/remote` 有 vendored 原生依赖**：VP9 4:4:4 需要 profile 1，上游 `shiguredo_libvpx` 表达不了，故 `vendor/shiguredo_libvpx/`（wrapper Apache-2.0 + 内联 libvpx BSD，与项目 MIT/Apache 相容，**无 GPL/AGPL**）打了 7 处补丁（1–3 是 4:4:4 的前提，4 是 CBR 交互级缓冲，5 是显式色彩空间/范围，**6 是运行期在线重配置码率/帧率——QoS 闭环的前提**，**7 是解码侧并行 threads + row-mt——解码原本恒为单线程**），每处带 `PATCH:` 注释写明动机，根 `Cargo.toml` 的 `[patch.crates-io]` 重定向，机械锁在 `crates/remote/tests/vendor_patches.rs`。预编译库覆盖 `aarch64-apple-darwin` / `x86_64-pc-windows-gnu` / `x86_64-unknown-linux-gnu`；**不覆盖** `x86_64-apple-darwin`（build.rs 直接 panic，已放弃该 target）与 MSVC（预编译是 mingw）。**Windows 那份预编译还是 msvcrt 味的 mingw-w64 构建**（它引用 `__imp__setjmp`，而 `_setjmp` 只在 msvcrt 的导入库里），所以那个 leg 的链接器也必须是 msvcrt 味的——runner 镜像自带的 `C:\mingw64` 是 **UCRT** 味，链不了；必须用 MSYS2 的 `mingw64` 环境。别把那条 step 简化回 `C:\mingw64`，理由写在 `release-client-gui.yml` 里。`crates/server` 不依赖 `remote`，musl 静态编译不受影响。

**硬件编解码的落地范围（阶段 5）**：数据面不再只有 VP9。**Windows GUI 被控端**在 NVENC 或 Intel/AMD 硬件编码 MFT 可用时用 **H.264/H.265 4:2:0 硬件编码**（NVENC 优先；无 NVIDIA 时用 Media Foundation 硬件编码 MFT，Intel QSV 优先），控制器用 **Media Foundation `MSH264DecoderMFT`/`MSH265DecoderMFT`** 解码（DXVA 优先）；采集走 **WGC（优先）→ DXGI Desktop Duplication（回退）→ xcap/GDI（最后兜底）**。**有意固定 4:2:0**：消费级 GPU 的 H.264 硬件编码器普遍只支持 4:2:0（本机 RTX 5070 Ti 虽报 `SUPPORT_YUV444_ENCODE=1`，但 H.264 4:4:4 的**解码**侧没有可用硬件/系统解码器，混合部署会断），且 4:2:0 + 高码率对桌面文本已足够；需要 4:4:4 时继续走 VP9 profile 1 软件路径。**这不是给 VP9 加硬件**：四家厂商仍无 VP9 4:4:4 硬件解码，硬件路径是换 codec 到 H.264 4:2:0；VP9 保持软件 4:4:4 作为质量优先的回退。**VP9 软件路径仍是默认回退**：无 NVENC、无硬件编码 MFT、非 Windows、MFT 缺失或对端未声明硬件 codec 时，`CodecOffer`/`CodecSelect` 协商按 `H.265 → H.264 → VP9` 自动降级。**NVENC 的 `inputWidth/Height` 只做左上角裁剪、不做缩放**（实测码流逐字节相同），故 4K→1920 的下采样由 `crates/win-video` 的 `pixels::scale_bgra_to_rgba` 在采集层一步完成（BGRA→RGBA + 盒式缩放，实测 4K→1080p ~10 ms），避免 33 MB 中间缓冲。**H.264/H.265 的显示尺寸 ≠ 编码尺寸**：MFT 会按宏块/CTU 对齐输出（H.264 16 像素、HEVC 64 像素；640×360 解出 640×368、1080p 解出 1920×1088），必须按 `MF_MT_MINIMUM_DISPLAY_APERTURE` 裁掉填充，否则坐标/纹理尺寸整体错位。**实例模式是混合采集**：`crates/client-gui/src/remote/win_hybrid.rs` 按 worker 实际挂载的桌面分流——`Default` 走 WGC/DXGI + GPU 零拷贝（与 GUI 同一套 `win-video`），`Winlogon` / `Screen-saver` 以及任何失败场景回退 `win-platform` 的 GDI `BitBlt`。会话级锁存（`instance_gpu.rs`，纯逻辑、全平台单测）保证「第一帧没拿到 GPU 纹理的会话不再交 GPU 帧」——编码器在首次编码时按当时的采集设备定型，中途把 GPU 纹理交给 CPU-only 的 MFT 适配器会命中 `!self.gpu` 分支并**永久禁用**进程级 GPU。GPU 采集走 `capture_gpu_strict`（失败不写全局禁用、不回退 xcap），回退目标是 BitBlt；`capture_info` 按最近一帧的真实来源上报，BitBlt 帧之后不会继续显示 WGC/DXGI。另有一条进程内 hint（`GpuContext::encode_gpu_ok`）：MFT 工厂只能建 CPU 输入适配器时（采集设备不是 Intel / `new_with_device` 失败）会把它置 false，采集侧下一帧起改交 CPU 帧，而不是让 `MftAdapter::encode_frame` 的 `!self.gpu` 分支永久禁用 GPU（那会连 WGC 的 CPU 回读一起关掉）；每次会话开始由 `supports_gpu_capture()` 重置为 true 重新尝试。H.264/H.265 编码器对 BitBlt 帧同样可用（RGBA 输入）。**编码器的在线重配置**：NVENC 走 `nvEncReconfigureEncoder`（`resetEncoder=0`/`forceIDR=0`）；Intel/AMD MFT 走 `ICodecAPI`（码率支持、帧率 Intel MFT 不支持，适配器保持内部帧率而不重建）。两者都遵守「码率变化不重建、不插 IDR」，只有几何变化才重建 + 强制 IDR。真机实测：RTX 5070 Ti（4K WGC，release）采集 ~31 ms、转换+缩放 ~10 ms、NVENC ~3 ms；Intel UHD 770 **QSV 1080p 编码 ~13 ms**（现场 HD 630 的 VP9 软件编码是 **113 ms**）；控制器 **DXVA 解码 1080p CPU 时间 13.4→9.1 ms/帧**（输出与软件逐像素一致，QSV→DXVA 端到端 MAD ~1.0/255）。像素转换（RGBA↔NV12、NV12→RGBA、4K 盒式缩放）按行并行（`crates/win-video/src/lib.rs::worker_count`，最多 4 个 worker；1080p 转换 release 本机实测 4.5→1.1 ms，去掉复用缓冲的多余 memset 后 ~0.5/0.7 ms）；解码把 `MF_MT_MINIMUM_DISPLAY_APERTURE` 裁剪融进颜色转换，省掉 1080p 整帧 8.3 MB 的二次拷贝；GUI 启动时 `timeBeginPeriod(1)`，避免 MFT/WGC 的 1–2 ms 轮询被 Windows 默认 15.6 ms 定时器粒度放大；采集源几何每 2 s 复查一次，不再每帧枚举显示器。NVENC 绑定 vendored 自 `moq-nvenc` 0.1.2（MIT，bindgen 自 NVIDIA `nv-codec-headers`），运行期 dlopen，**不引入任何 AGPL/GPL 代码**。

**GPU 零拷贝与「静止不编码」（阶段 6–7，2026-11-14）**：被控端 GPU 路径是 **WGC/DXGI 采集纹理 → D3D11 VideoProcessor 转 NV12 + 缩放 → `MFCreateDXGISurfaceBuffer` → QSV MFT**，全程不落 CPU；采集与编码复用同一 D3D11 设备，任何一环失败都会永久禁用 GPU 并回退 CPU（`RUST_TUNNEL_DISABLE_GPU_CAPTURE=1` 强制关闭）。控制器把 H.264 解码出的紧包 NV12 直接交给 wgpu 着色器上屏（Y/UV 两平面 + BT.709 limited→RGB，Windows 默认 wgpu，`RUST_TUNNEL_RENDERER=glow` 回退 CPU 转换）。三条**实测得出、反直觉**的约束：① **Intel QSV 的 CBR 会给静止画面插填充**——`mft-gpu-probe` 用完全相同的输入帧跑 1080p@30fps、目标 8 Mbps，每帧仍输出 **21666 字节（≈5.2 Mbps）**；码率控制因此改为**峰值受限 VBR**（均值 = 目标 × 70%，峰值 = 目标；编码器不支持时回退 CBR），同一测试降到 **~3027 B/帧**，NVENC 同步改 `NV_ENC_PARAMS_RC_VBR`。② **WGC 不会因为画面静止就不产帧**——30 fps 轮询下 120 次采集 **119 次**在 0.2 ms 内拿到新帧，所以「有新帧」≠「有变化」；真正省带宽的是 `crates/win-video/src/capture/change.rs` 的**像素级变化检测**（VideoProcessor 把采集纹理缩到 **320×180 NV12** → staging `Map` 回读 → 与上一帧逐字节比，任一像素 |Δ|>3 即算变化；4K 实测 **0.36 ms/帧**，静止画面在相同输入下为精确 0 差异），`CaptureSource::capture_texture_with_change` 返回 `ChangeReport { changed, changed_permille }`，引擎在 `RawFrame.changed == false` 且非 `force` 时**完全跳过编码/发送**。**网格密度直接决定检出率**：VideoProcessor 大比例缩小时是**点采样**、不是区域平均，旧的 128×72 在 4K 上只有 1/900 的采样密度，16×16 的变化只有约一半概率被看到；320×180 把密度提到 4 倍而**检测耗时不变**，960×540 才明显变贵。检测盲区与「编码前背压丢帧 / 采集看门狗作废结果（`changed` 水位已推进）」都会让一次已发生的变化一帧都发不出去，故静止闸门另加 **250 ms 保活补编**（`controlled.rs` 的 `UNCHANGED_RESEND_INTERVAL`：普通 P 帧、**不请求 IDR**，QSV/NVENC 实测静止 P 帧 ~3 KB，4 fps ≈ 12 KB/s ≈ 100 kbps），把延迟上界压到 250 ms。③ **控制端的「丢包→请求 IDR→IDR 又被丢」会自锁**：解码队列（`DECODE_QUEUE = 16`）满时丢包会触发序号断层与关键帧请求；请求若每帧一发就是 IDR 风暴（带宽尖峰 + 可见帧率崩到个位数）。修复是三件套：队列溢出推进 `drop_epoch` 让陈旧 P 帧整体作废；`awaiting_keyframe` 期间主循环**入队前**就丢 P 帧（保证 IDR 不被挤掉）；所有关键帧请求走 400 ms 限频 + 定时重试。观测面新增：被控端 `SessionStats::Encode { capture_us, skipped, .. }`（采集耗时**用微秒**——GPU 采集 <1 ms 时毫秒取整恒显示 0；`skipped` 是无变化跳过的帧数）、控制器 `SessionStats::Decode { packets, dropped, .. }`，GUI 分别显示为「采集 x.x ms · 跳过 n/s」与「收包 n/s · 丢包 n/s」。**未做**：NVENC 直接吃采集纹理（`nvEncRegisterResource`，目前 GPU 帧要回读）、4K 原生编码、D3D11 立即上下文的多线程互斥（采集线程与编码线程目前共用同一个 `ID3D11DeviceContext`，正确做法是加锁或各自用延迟上下文）。

**出码拆解与变化面积自适应码率（阶段 8）**：带宽偏高的根因是**整帧编码 + VBR 码率控制器会把档位预算花完**，且此前看不到字节花在关键帧还是普通帧上。两件事都不改线上协议（`SESSION_PROTO_VERSION` 不为此抬版本、`QosFeedback` 字段不变）：① **观测面**——`EncodeStatsWindow` 把每个统计窗口拆成关键帧数 / 关键帧字节 / 普通帧字节 / 面积累加，`SessionStats::Encode { keyframes, idr_kbps, p_kbps, changed_permille, .. }` 每秒上报，GUI 被控端行显示「关键帧 n/s · 关键帧 x kbps · 普通帧 y kbps · 变化 z%」（0/未知项不显示）。② **面积驱动上限**——`win-video` 的变化检测从 `bool` 升级为 `ChangeReport { changed, changed_permille }`（超阈值字节占整个 NV12 网格的千分比；首帧/检测失败给满面积 1000，保守不压制码率），`RawFrame.changed_permille` 只在被控端进程内传递；`RateController::observe_activity` 分三档给**生效值**加钳制：UI（<5%）1.2 Mbps/15 fps、Mixed（5–20%）2.5 Mbps/30 fps、Active（>20%）不设上限（完全交给 AIMD）。**2026-11-17 起默认关闭**（`QosParams::content_caps_enabled = false`）：上限压低码率会连带塌掉编码器的 VBV 单帧预算，H.264 下表现为「画面一变就糊成一块」；只有 VP9 CBR 会话在 `controlled.rs` 里显式打开（CBR 静止时也会灌满目标码率）。面积仍照常上报统计角标。**升档立刻生效、降档需连续 `cap_hold_ticks`（2 秒）**——视频开始就要放量，视频停了两秒才收紧，避免单个静止帧把上限压死。**只钳制 `current()` 的显示/生效值，不改 AIMD 状态**（`bitrate_kbps`/`fps` 的历史保留，面积一变立刻回到 AIMD 水平），也**绝不置 `force_keyframe`、不改 `scale`**：上限变化只走既有的在线重配置（`set_bitrate`/`set_fps`），不重建编码器、不插 IDR；分辨率降级仍只由「解码耗时 ≥ 37 ms」触发。**上限按编码面积等比放大**（`RateController::set_content_area_permille`，1080p = 1000‰、4K = 4000‰）：1.2 Mbps 是在 1080p 上标定的，同一个变化**比例**在 4K 上是四倍像素，按绝对码率收紧会让每像素比特掉到 1/4、文字被量化糊。被控端在投递编码任务时用**实际编码尺寸**（`controlled::content_area_permille`，与视频路径同一套 `encode_dimensions`）设置它。

**文字清晰度的四层（2026-11-17）**：① **编码前缩放**——默认编码宽度 1920，只有 `native_resolution`（**被控端本机偏好**，默认开做了一次性迁移）**且**协商到硬件 H.264 才放宽到 3840；原生码率上限按像素数放大（`NATIVE_BITRATE_FACTOR = 4`，封顶 `NATIVE_MAX_BITRATE_KBPS = 60 Mbps`，Hd 档即 40 Mbps）。无人值守实例现在也默认开原生（`host.toml` 的 `native_resolution`，缺省 true），安全桌面的 BitBlt 帧由 `win_hybrid::clamp_bitblt_frame` 先盒式缩到 1920（纯 CPU 路径不编 4K）。② **关键帧**用 AIMD 码率、**跳过内容上限**（`RateController::keyframe_bitrate_kbps`），编码器**创建时**也用该值（MFT 的峰值/VBV 只在创建时设定）；NVENC/MFT 的 VBV 取 `max(6 帧码率, min(每像素 2 位, 1 秒码率))`（VBV 同时是单帧比特上限；只按「6 帧 × 当前码率」算时，4K/60fps/40 Mbps 也只有 0.5 bit/px，整页重绘（切标签）会被量化成一团；2 bit/px 给出 1080p ≈520 KB、4K ≈2 MB 的单帧预算，「1 秒码率」封住低带宽场景）。③ **编码器画质**：NVENC 用 `LOW_LATENCY` 而非 `ULTRA_LOW_LATENCY`、开四分之一分辨率两遍编码与空间 AQ（强度 8）；MFT 尽力设 `AVEncCommonQuality = 70`。**QP 上限（画质地板）**是「文字不发虚」的主旋钮：`win-video` 的 `MAX_QP = 20`（`RUST_TUNNEL_MAX_QP` 可覆盖，范围 1..=51；20 是 H.264「视觉近无损」区间，22 已略软、26 在 4K 文本上明显发虚）——只给码率时编码器会把预算摊到整幅画面、文字被平均掉，设 QP 上限后它必须为文字掏更多比特（仍受 VBV 单帧上限约束），这是 RDP RemoteFX/Parsec 一类实现的共同做法。NVENC 走 `rcParams.enableMaxQP/maxQP`（I 帧再低 2 档），MFT 走 `CODECAPI_AVEncVideoMaxQP`（`IsSupported` 不通过时静默跳过）。**渐进式刷新（intra refresh / GDR）**：NVENC 开 `h264.enableIntraRefresh`，周期 `INTRA_REFRESH_SECONDS = 1` 秒（`intraRefreshPeriod = intraRefreshCnt = 2×fps`）——每帧把一小条宏块编成 intra，参考帧持续自愈且**没有关键帧尖峰**（Moonlight/Sunshine 与 RDP 的做法）；显式 FORCEIDR 语义不变，引擎每 10 s 的周期 IDR 仍作为跨后端（QSV 无 GDR）的安全网。④ **显示侧**（控制器）——`view_scale` 的一切比例都以**物理像素**为基准（传给 `display_size`/`initial_window_size` 的 `pixels_per_point`），`P100` = 1 远端像素对 1 物理像素，「适应窗口」仍等比**铺满**窗口（可放大可缩小，只是比例不再被 DPI 二次放大），绘制矩形对齐到整物理像素（`snap_to_physical`），NV12 着色器按导数估算采样足迹、缩小时做 4 tap 亮度预滤波。**排查入口**：会话统计角标的「显示缩放」（`view_scale_permille`，1000‰ 才是真 1:1）+「编码尺寸 / 显示尺寸 / 原生分辨率 / 编码器」四行，能一眼区分「编码糊」和「显示缩放糊」。

**脏矩形增量补丁（阶段 9，协议 v6）**：`SESSION_PROTO_VERSION = 6`（main 的 v5 是键盘输入改键位码的语义变更，本分支把两笔不兼容改动合到一起后统一抬到 6），`SessionMessage` 末尾追加 `RegionUpdate { display, width, height, rects }` + `RegionRect { x, y, width, height, flags, data }`（判别式仍只追加；`Hello`/`HelloAck`/`DisplayInfo` 字节形状不变，v5/v4 ↔ v6 在握手阶段被硬相等校验干净拒绝）。**只在 Windows H.264/H.265 路径启用**（VP9 解码输出是 RGBA，控制端需要另一套持久帧缓冲）。被控端在 `Video ↔ Patches` 之间切换：连续 3 拍变化面积 ≤3% 才进入，>8% / 尺寸或矩形预算超限立即退出。**进模式用 IDR、退出用 P 帧**：补丁只改控制端合成缓冲、不碰 H.264 参考链（补丁期间编码器根本不编码），退出后的第一帧只需普通 P 帧；但**基准帧必须是 IDR**——补丁只覆盖变化矩形，静态区域会长期沿用这一帧，若它继承了一张糊的参考（典型：切浏览器标签导致整页重绘、那一帧被单帧预算压过），整页就再也不会自愈（补丁模式里编码器不跑、intra refresh 也停了）。`EncodeJob` 因此把「必须编码一次」`force_encode` 与「必须是 IDR」`force_keyframe` 拆开；`RequestKeyframe`、切显示器/切画质、分辨率变化、QoS 降分辨率、编码器重建同样 IDR。**顺序不变量**：进入前必须让基准帧**真正进入出站写队列**（`encode_loop` 的 `base_frame_sent`，按 `keyframe || force_encode` 置位），补丁随后走同一条 FIFO 队列；控制端把补丁与全帧放在**同一条解码队列**，因此 `FrameSink::on_frame`/`on_patch` 严格同序，补丁不占 `VideoPacket.seq`（退出后的第一个全帧序号仍连续）。补丁**绝不允许丢弃**：控制端队列满/校验失败/解压失败一律请求关键帧，被控端据此退出补丁模式。采集层（`win-video/src/capture/patches.rs`）把 320×180 脏格 → 膨胀 → 8 连通域 → 合并 → 目标坐标矩形（上限 16 块、总面积 ≤20%），一次 `CopySubresourceRegion` 进纵向 staging 图集 + 一次 `Map`，再用**绝对坐标**盒式缩放（与整帧缩放逐像素一致，避免矩形边缘接缝）转 BT.709 limited NV12。补丁用 zstd level 1 压缩（压缩后不更小则发原始 NV12），单条 `RegionUpdate` 上限 64 KiB，且有一个 1s 窗口的**补丁码率兜底**（超过 `min(有效码率×2, 2 Mbps)` 就回全帧——补丁模式下 QoS 暂停，这是唯一的带宽保护）；空矩形=保活（250 ms），让控制端 `NO_FRAME_TIMEOUT` 保持安静；补丁采集节拍固定 `PATCH_MAX_INTERVAL`（**30 fps**，不跟随视频侧已被压到 15 fps 的内容上限，否则进入/退出判据的检测延迟翻倍）。补丁模式下**暂停 QoS 闭环**（控制器未解码视频，`decode_ms`/`recv_kbps` 是陈旧值），退出后自动恢复；变化面积**逐帧**喂给 `RateController::promote_activity`（**只升档**，安静两秒后一开始拖窗就放量，不必等下一次 1 Hz 反馈），降档仍按 1 Hz 反馈的 `cap_hold_ticks` 迟滞。观测面：`SessionStats::Decode { patches, .. }`（GUI 显示「补丁 n/s」）、`SessionStats::KeyframeRequest { reason }`（`SeqGap`/`QueueOverflow`/`DecodeError`/`NoDecoder`/`ConfigureFailed`/`StalledNoFrame`/`Manual`，GUI 显示每秒关键帧请求数与原因分桶）、`SessionStats::Encode::idr_bytes`（算「关键帧平均 x KB」）。**网络/QoS 侧同批改动**：心跳探测 10s→1s（`TRUSTED_RTT_MAX = 10s` 只作可信上限；两端用 `VecDeque<Instant>` FIFO 配对迟到 ack）；`on_feedback` 把**解码饱和**（`decode_ms × fps ≥ 80%` 且积压）与**网络拥塞**分开，前者先乘性降帧率（×3/4）→ 分辨率 → 最后才砍码率；**网络路径绝不改分辨率**（阶段 9 最初的 `bpp < 0.04 → 降分辨率` 在真机上把 1080p 会话几秒内降到 608p/342p、文字完全不可读，已删除：分辨率只该由对端**解码能力**决定，需要低分辨率省带宽请显式选「流畅」档）；拥塞判据改成三条防误判——RTT 用 EWMA 取平滑值且要连续 2 tick（单条心跳样本的抖动曾被当成拥塞）、队列深度要达到 `backlog_frames`(4 帧)/`local_backlog_frames`(2 帧) 才算积压（`> 0` 只是流水线常态，会把闭环**永远**压在低码率）、健康不要求队列精确为空；恢复在不足上限 1/4 时乘性 ×1.5（纯加性 +10% 从地板爬回 10 Mbps 要 41 秒）；另有 `recv_kbps / sent_kbps < 0.7` 连续 3 tick 作为辅助拥塞判据，`MAX_RECV_WINDOW 64→256`（≈307 KB 在途，解除 100 ms RTT 下 ~6 Mbps 的吞吐上限）。**发布顺序**：两端必须同为 v6；先把客户端铺到 0.13.0（`crates/client`/`client-gui`/`win-platform`/`win-service` 已同步抬版），再部署服务端并按需把 `MIN_REMOTE_CLIENT_VERSION` 抬到 `(0,13,0)`。

**被控端统计角标（协议 v8）**：`SESSION_PROTO_VERSION = 8`。会话窗口右上角的统计角标现在显示**被控端**的真实数据面口径：编码器后端/名称/适配器（NVENC / Intel QSV / AMD VCE / libvpx）、采集方式与适配器（WGC / DXGI / xcap / BitBlt）、GPU 采集/编码的输入形态（是否真的走 GPU 纹理）、采集/编码耗时、实测编码 fps、目标/实际码率、关键帧拆解、变化面积、活跃档、补丁模式、出站队列深度/背压跳过、编码器重建/强制 IDR、采集失败/回退/无新帧；控制器本端另有 codec、编码/显示尺寸、解码后端（DXVA/软件）、渲染后端（wgpu/glow）、帧到达抖动、可靠层重传/乱序、解码积压与**端到端延迟**。**线上一共三笔改动**：① `Heartbeat` / `HeartbeatAck` 各带单调时钟时间戳（`t1_us` / `t2_us` + `t3_us`），控制器用 NTP 四时间戳估计两端时钟偏差（最小 RTT 样本优先 + 1/8 EMA），`VideoPacket` / `RegionUpdate` 追加 `capture_ts_us`，两者相减即「采集→解码完成」；UI 再用同一偏差算「采集→上屏」；② 末尾追加 `HostStats(HostStats)`（判别式 28，被控端每秒上报的整机遥测，含 B1–B14 / C2 / C3 / C5，`outbound_class` 归 Control）；③ 心跳/视频帧的字段形状本身已不向后兼容，故两端必须同为 v8，旧端在握手阶段被 `Hello.proto_version` 硬相等校验干净拒绝（`Hello`/`HelloAck`/`DisplayInfo` 字节形状仍未动）。被控端每拍心跳顺带上报 `HostStats` 并同时喂本机 UI；控制器收到后只写 `RemoteUiState`（**不参与 QoS 决策**）。可靠层指标（A7）经 `ReliableMetrics` 原子句柄由 `EstablishedStream.reliable` 带出，只有 UDP 打洞路径有，直连/中继显示 `—`。GUI 角标用 `egui::Area` + `.interactable(false)` 实现**鼠标穿透**（`Memory::layer_id_at` 会跳过非交互层，点击照常注入远桌面），半透明且无投影；「简洁 / 详细」由 `GuiPrefs::remote_stats_detail` 持久化、在会话设置里切换。平台侧：`win-video` 的 WGC/DXGI 源累计「无新帧」、`WinVideoScreen` 累计 xcap 回退/WGC→DXGI 回退并区分「GPU 能力可用」与「本帧真的走了 GPU 纹理」（VP9 会话走 CPU 回读时不得谎报 GPU），NVENC/MFT 适配器上报编码器名称与适配器名。**发布顺序**：两端必须同为 v8，先把客户端铺到 **0.16.0**（`crates/client`/`client-gui`/`win-platform`/`win-service` 已同步抬版；0.15.0 是 main 上的控制通道自更新版本，协议仍是 v7，不要与 v8 混发），再部署服务端并按需抬 `MIN_REMOTE_CLIENT_VERSION`。

**远端光标形状跟随（协议 v9）**：`SESSION_PROTO_VERSION = 9`，`SessionMessage` 末尾追加 `CursorShape { shape }`（判别式 29，被控端 → 控制器；`Hello`/`HelloAck`/`DisplayInfo` 字节形状不动，v8 ↔ v9 在握手阶段被硬相等校验干净拒绝）。**只传形状、不传位置或位图**：完全控制模式下控制端本机指针就是远端指针，系统光标继续由本机渲染（移动零额外延迟），被控端只需在形状变化时发一条控制帧。引擎侧：`screen.rs` 的 `CursorShape` 判别式是线上契约（机械锁 `cursor_shape_discriminants_are_stable`）；`ScreenSource::cursor_shape()` 默认 `None`；被控端在 `run_capture` 每拍采集后采样并放进 `CaptureOutcome.cursor`，主循环只在变化时 `enqueue_ctrl`（`None` 既不发也不清 `last_cursor`）；采集失败也采样，静止画面也有采集节拍，所以形状变化不需额外定时器。Windows 实现 `client-gui/src/remote/win_cursor.rs`：`GetCursorInfo` + `LoadCursorW(null, IDC_*)` 句柄比对，`IDC_IBEAM`/`IDC_HAND`/`IDC_SIZENS` 等逐个映射；应用自定义光标落到 `Unknown`（控制端显示默认箭头），隐藏光标是 `Hidden`（**用户拍板：也显示默认箭头，不隐藏本机指针**）。实例模式只在 `DesktopSlot::Default` 采样，安全桌面返回 `None`。控制端：`FrameSink::on_cursor` → `GuiFrameSink` 写 `FrameState.cursor_shape` 并 `request_repaint_of(会话 viewport)`；`RemoteView::capture_input` 在指针位于画面内时（仅**完全控制**模式）`ctx.set_cursor_icon(...)`，映射 `None`/`Arrow`/`Unknown`/`UpArrow`/`Hidden` → `Default`、`IBeam` → `Text`、`Hand` → `PointingHand`、`Size*` → 对应 resize 图标等；`reset_session` 清空形状，避免上一场 I-beam 带到新会话。**发布顺序**：两端必须同为 v9 / **0.17.0**（`crates/client`/`client-gui`/`win-platform`/`win-service` 同步抬版），先铺客户端，再按需抬服务端 `MIN_REMOTE_CLIENT_VERSION`。macOS/Linux 暂未实现采样（保持默认箭头）；这是已知边界，不是回归。

**H.265/HEVC 硬件编码（协议 v10）**：`SESSION_PROTO_VERSION = 10`，`CodecId` 末尾追加 `H265`（判别式 2），`CodecOffer` 末尾追加 `h265: bool`（字段顺序 `h264, vp9, h265`；`Hello`/`HelloAck`/`DisplayInfo` 字节形状不动，v9 ↔ v10 在握手阶段被硬相等校验干净拒绝，两端必须同为 v10）。**固定 HEVC Main 8-bit 4:2:0（NV12）、BT.709 limited**：与 H.264 共用采集、NV12 合成缓冲、补丁模式、wgpu 着色器与 QoS，渲染侧零改动。协商优先级 `H.265 > H.264 > VP9`（`choose_codec`，还受本端工厂与 `prefer_h265` 约束；超时/关闭回退 VP9）；控制器 `CodecOffer.h265 = prefer_h265 && h265_decoder_factory.is_some()`。**运行时回退只在编码器创建失败时触发**：被控端 `H265 → H264 → Vp9` 每级一次，`CodecSelect` 重发必须走 **Screen 车道**（`send_video`）——放 Control 车道会被 writer 的 ctrl 优先级插到已排队的旧 codec 视频帧之前；同时 `ScreenConfig` 的发送条件从「仅几何变化」扩成「尺寸或 codec 变化」，主循环观察 `codec_state: AtomicU8` 并在降到 VP9 时关 GPU 采集/退出补丁/宽度回 1920/重开内容上限。**能力探测全部是创建式的、失败即不声明**：NVENC 用 `CodecKind` 查询/编码器创建探测 HEVC，MFT 用 `enumerate_encoders(codec, hardware_only)` + 实例化探测；控制器 HEVC 解码依赖 `MSH265DecoderMFT`（Microsoft Store 的 **HEVC Video Extensions**），未安装/不可用则 `h265=false` 自动回 H.264。**开关**：`GuiPrefs.remote_prefer_h265`（默认开）、实例 `host.toml` 的 `prefer_h265`（默认 true）、环境变量 `RUST_TUNNEL_DISABLE_H265`（整机 kill switch，编码与解码两侧工厂都不注入，优先于设置页开关）。HEVC 码流解析（`mft_encoder/bitstream.rs`）：NAL 类型 `(b0>>1)&0x3F`、关键帧 = IRAP `16..=23`、参数集 VPS=32/SPS=33/PPS=34；NVENC 侧 `repeatSPSPPS=1` 保证 IDR 内联参数集，MFT 编码器尽力按 `MF_MT_MPEG2_PROFILE = eAVEncH265VProfile_Main_420_8` 与 BT.709 limited 颜色属性配置。真机实测（本机 RTX 5070 Ti + Intel iGPU + HEVC Video Extension 2.5.48）：NVENC/QSV HEVC 均能创建、IDR 带 VPS/SPS/PPS、在线 `set_bitrate` 不插 IDR；`MSH265DecoderMFT` 走 DXVA 解码正常（合成内容 MAD ≈1.4–1.9），QSV HEVC MFT 与 H.264 MFT 一样**不支持运行期改帧率**（适配器保持内部帧率）。**发布顺序**：两端必须同为 v10 / **0.22.0**（`crates/client`/`client-gui`/`win-platform`/`win-service` 同步抬版），先铺客户端，再按需抬服务端 `MIN_REMOTE_CLIENT_VERSION`；0.21 与 0.22 不能互相远程控制（握手干净拒绝）。

**音频转发（协议 v11）**：`SESSION_PROTO_VERSION = 11`，新 `Lane::Audio = 3`，`SessionMessage` 末尾追加 `AudioStatus`(30)/`AudioPacket`(31)/`SetAudioEnabled`(32)。被控端在 `CodecSelect` 之后上报一次 `AudioStatus`（能力/格式/原因），控制端**只在 `available && audio_enabled && 非 FileTransfer`** 时回 `SetAudioEnabled{true}`；被控端收到后才打开默认回放设备的 WASAPI loopback，按 20 ms 一帧 Opus 96 kbps 发 `AudioPacket`（`seq` 从 1 递增，`capture_ts_us` 为进程单调时钟）。**出站第五档队列**：`close > ctrl > audio > screen > file`，音频只 `try_send`、满则丢最新包（播放侧抖动缓冲 PLC 抹平），绝不阻塞主循环。控制端播放线程：抖动缓冲（默认 80 ms，欠载自适应到 120 ms，8 秒无欠载回落，地板 60 ms）→ Opus 解码 → WASAPI 默认输出；音量是本地增益、不上线。**隐私默认**：会话批准即转发；控制端工具条扬声器同时本地静音与停止对端采集；被控端横幅可「停止转发声音」而不中断会话；设置页有整机开关，实例用 `host.toml` 的 `audio`（缺省 true）。**平台**：首期 Windows（`crates/win-audio`，WASAPI + vendored libopus 静态库）；macOS/Linux 的 `audio_endpoints()` 返回空，引擎向对端上报「不支持音频」。**发布顺序**：两端必须同为 v11 / **0.23.0**（`crates/client`/`client-gui`/`win-platform`/`win-service` 同步抬版），先铺客户端，再按需抬服务端 `MIN_REMOTE_CLIENT_VERSION`。

**锁屏（协议 v7）**：`SESSION_PROTO_VERSION = 7`，`SessionMessage` 末尾追加单元变体 `LockScreen`（判别式 27，其余字节形状不变；v6 ↔ v7 在握手阶段被硬相等校验干净拒绝，两端必须同为 v7）。控制器在会话窗口工具条点「锁屏」→ `RemoteManager::lock_screen()` → `ControllerCommand::LockScreen` → 数据面 `LockScreen`；被控端**只在完全控制模式**执行（`inject == true`），仅观看/仅文件传输忽略并记 debug。`InputSink::lock_screen` 的平台实现：Windows `LockWorkStation`（作用于进程所在的交互式会话，实例模式无需挂载桌面）、macOS `CGSession -suspend`、Linux 先 `loginctl lock-session` 再 `xdg-screensaver lock`。`GuiPrefs::remote_auto_lock_on_disconnect` **默认关闭**：开启后被控端在**握手鉴权成功且会话共享画面**的会话收尾（取消/对端关闭/IO 错误，含硬断链）由 RAII 守卫自动锁屏，偏好实时读取（会话中打开即生效）；纯文件传输会话不创建守卫、不触发。无头实例用 `host.toml` 的 `auto_lock = true`（`--install` 生成的 `host.toml` 亦从 GUI 当前偏好写入）。**已知限制**：GUI 模式被控端无法采集 Windows 锁屏后的安全桌面，锁屏即等于主动切断远端画面；要锁屏后仍能远程重新登录必须用无人值守服务实例。

**Windows 10 兼容性（已踩过的坑）**：① `crates/win-video` 的 Media Foundation 入口（`MFStartup`/`MFCreateMediaType`/`MFCreateMemoryBuffer`/`MFCreateSample`）必须**运行期 `dlopen("mfplat.dll")`**——Windows 10 N/LTSC/精简镜像可能缺这个 DLL，直接静态导入会让 GUI 在 `main` 之前加载失败（点开即闪退）；缺 DLL 时工厂探测失败，自动回退 VP9。② GUI 主线程必须在 winit 之前 `OleInitialize(STA)`：winit 创建窗口时也调 `OleInitialize`，若主线程已被初始化成 MTA 会直接 panic（`RPC_E_CHANGED_MODE`）。③ GUI 的启动/panic 日志在 `%LOCALAPPDATA%\rust-tunnel\gui-startup.log`；现场闪退先看它，没有该文件则多半是 loader 失败（看事件查看器缺哪个 DLL）。④ 部分 Windows 10 机器的 OpenGL 驱动不提供 Glow 要求的配置（`Found no glutin configs matching the template`），GUI 会自动以 `RUST_TUNNEL_RENDERER=wgpu` 重启自身并走 wgpu/DX12；启动日志会保留两次尝试（Glow 失败原因 + 最终 renderer）。⑤ MFT 编码/解码的 COM 初始化必须**线程局部**（`CoInitializeEx(MTA)`，`RPC_E_CHANGED_MODE` 视为成功），不要改成进程启动时一次——编码/解码任务可能落在不同的线程池线程上。

**`rd_id` 由服务端分配并持久化**到 `clients.remote_id`（9 位数字，满足 `MIN_PEER_ID_LEN=6`）。同一 NAT 下多个客户端共用一个公网 IP，所以不能由客户端自选；用户全程看不到也不需要配置 ID。

**会话鉴权不依赖 hbbs**（hbbs 按设计不做身份认证，ID 可被抢占）：服务端签发 `session_id` + 一次性 `token`（TTL 60s），经加密控制通道同时下发给控制器与被控端；被控端在 P2P 握手时校验。**没有 token 的第三方即使抢注了 ID 也建不起会话。**

**接入确认复用既有 `RemoteSessionResponse` 通道，不新增控制消息变体**：服务端本来就无条件把被控端的响应转达给控制器（`handle_session_response` → `notify_controller_response`），所以两端只需各自「延迟应答 / 等待应答」。控制器**必须**等到 `accepted:true` 才 `establish`——若照旧在被授权时立刻建立，用户还在看弹窗时它的中继请求就已经发出去，会撞上 hbbr 的 30s 配对窗口，表现就是「卡在连接中」。`Confirm` 下被控端**不登记 token**，所以即使控制器硬闯也过不了握手鉴权：**拒绝是引擎强制的，弹窗只是表面**。开关语义因此是「允许被请求」而非「永久放行」。

被控端 UI 面（`crates/client-gui`）：确认弹窗由 `ui/remote.rs::approval_modal` 在 `GuiApp::update` 里每帧渲染、**常驻所有 Tab 之上**（与 `controlled_banner` 同理，安全提示不能只在「远程」Tab 可见），并按模式给出不同的风险说明（「完全控制 / 仅观看 / 仅文件传输」三者的后果不同）；策略选择器与「已信任的对端」列表在「远程」Tab。策略经 `GuiPrefs.remote_approval` 持久化（`#[serde(default)]`，缺省 `Confirm`，旧配置文件也落到「每次询问」）；**信任列表只存内存**，退出应用即清空（`RemoteUiState.trusted_peers`，按客户端名索引——`RemoteSessionIncoming` 不带 `from_rd_id`，改名即失效）。请求首次出现时会强制显示并聚焦窗口（用户可能已把窗口隐藏到托盘），托盘 tooltip 同步改为「有远程接入请求待确认」。

**帧 → UI 通道（`crates/client-gui/src/remote/mod.rs::GuiFrameSink`）**：帧通道必须 `request_repaint_of(<会话 viewport>)`（会话窗口是 deferred viewport，与根窗口独立调度；根 ctx 的 `request_repaint()` 只重绘根窗口，曾导致画面被压在 ~2.5 fps）；`embed_viewports()`（后端不能开原生多窗口）时内容真实嵌在根窗口，才退回 `ViewportId::ROOT`。

**数据面三条路径**：**P2P 打洞**（跨 NAT，经 hbbs 交换公网 UDP 映射后对打）→ **P2P 直连**（同内网，`FetchLocalAddr`/`LocalAddr`）→ **hbbr 中继兜底**（所有 NAT 场景可用，`always_use_relay` 可强制）。规则两侧**对称**，故两端对路径的判断一致；`always_use_relay` **两侧都要遵守**（S3 之前被控端分支忽略它，规则不对称、它也就失去了 kill switch 的意义）。

**打洞为什么现在成立**：早先 `forward_punch_result` 回给请求方的是目标端信令 TCP 连接的**源地址**（临时端口，目标端并未在那里监听），所以「拿到地址就直连」在跨 NAT 下必然失败；现在回的是**目标端已登记的 UDP 映射**（`peers.lookup(&phs.id).socket_addr`，由 `RegisterPeer`/`RegisterPk` 维护），而请求方自报的 UDP 端口经 `PunchHole.udp_port`（tag 4）转给目标端。**UDP 打洞不需要端口复用**——从已注册的套接字向对端发一个数据报就建立了 NAT 映射，这正是它比 TCP 同时打开简单的地方。**hbbs 的三处改动必须保持纯加法**：① `PunchHole.socket_addr` **一个字都不能动**——`forward_punch_result` 靠解码目标端回显的 `PunchHoleSent.socket_addr` 去 `sinks` 里找请求方那条 TCP 连接，而 `sinks` 按**信令 TCP 源地址**索引，改了它上游客户端的应答路由就断了（`push_to_connection` 找不到 sink 会静默丢弃）；② `udp_port == 0` 时逐字节退回旧行为；③ NAT 探测端口的 UDP 半边**只应答 `TestNatRequest`**，**不复用 `handle_datagram`**——否则会把「面向 NAT 端口的映射」写进 `peers`，污染打洞用的映射语义。

**打洞的两条安全不变量**（配机械锁）：只接受 `magic` 正确且 `conn_id` **命中已登记会话**的数据报，其余**静默丢弃、绝不回复**（反反射）；`Established` 时锁存对端源地址，之后只认该地址（等价于「NAT 重绑后 TCP 断掉」，与中继路径同构，不是新增弱点）。**被控端必须回观测源**而不是回 hbbs 报的地址——hbbs 报的是「对端面向 hbbs 的映射」，对称 NAT 下与「对端面向我的映射」不同。

**直连/打洞探测必须用短超时**（`signaling.rs` 的 `DIRECT_SIGNAL_TIMEOUT = 3s`，**不是** `RELAY_WAIT`）：hbbs 只对 `same_intranet` 的 `PunchHoleRequest` 回包，跨 NAT 时它只推一个 `PunchHole` 数据报、**永不回响应**。若沿用 25s，控制器会空等 25s 才去发中继信令，而被控端的 `wait_relay_push(RELAY_WAIT)` 几乎同时超时 → 会话必现「卡在连接中」。**打洞预算的硬不等式**：控制器最坏耗时 = `DIRECT_SIGNAL_TIMEOUT(3s) + PUNCH_BUDGET(3s) = 6s`，必须 **≪ 被控端 `RELAY_WAIT(25s)`**，否则会出现「控制器还在打洞、被控端已放弃转中继」的错位（配断言测试）。`signaling::tests::try_direct_gives_up_quickly_when_hbbs_never_answers` 用假静默 hbbs 锁定这一点。

**`udp_demux_task` 是 UDP 套接字唯一的 `recv_from` 读者**，所以它写错会连心跳一起弄坏、客户端被判离线。分派链固定为「可靠层数据报（`peek_conn_id` 命中）→ NAT 探测应答 → 既有 protobuf 路径」，且 **protobuf 路径必须逐字节不变、排在最后**（配 demux 分派测试）。

**数据面必须预登记**：hbbs 对**未登记**的对端是静默丢弃的（`RequestRelay` 只打 debug 就丢、不排队不重试；`PunchHoleRequest` 立刻回 `IdNotExist`，于是控制器一个 RTT 内就转去中继信令）。所以 `TransportEstablisher::prepare` 在收到 `RemoteCapability`、以及打开「允许被远程控制」开关时被调用，两端在会话发起前就已在线。**别把它当成可选优化**——被控端若等到接受 `RemoteSessionIncoming` 才登记，控制器早已发完请求，会话**必现**超时（`tests/remote_session.rs` 的 C 部分用例锁定这一点）。注意 `set_enabled` 由 GUI 的 UI 线程调用，**不在 Tokio 运行时内**，所以预登记走构造时捕获的 `Handle` 而不是 `tokio::spawn`。

**中继约束**：hbbr 按 uuid 配对，先发**恰好一个**带帧的 `RequestRelay` 然后转裸字节、**没有 ack**；`pair_window` 与 `idle_timeout` 都是 30s（任何字节都会重置 idle，每 3s 检查一次）。因此会话必须靠应用层心跳保活（`HEARTBEAT_INTERVAL` 低于 30s），**静止画面也不例外**。配对超时不会有任何错误通知，控制器必须自己按窗口超时提示，否则用户看到的是「卡在连接中」。

#### 客户端 GUI 自更新（控制通道内传输）

**复用控制通道，不新开 HTTP 鉴权面**：GUI 客户端只有客户端 token、没有管理员 JWT，与远程控制控制面的取舍一致。服务端在客户端注册后（以及客户端主动 `ClientUpdateCheck` 时）扫描 `client_dist_dir`，把版本通告与**文件分片**都经同一条加密控制通道下发；`ClientUpdate*` 变体全部**追加在 `ControlMessage` 末尾**（bincode 序数只增不改，机械锁 `test_update_variants_append_after_remote_session_end`）。

**双向版本门控**（`MIN_UPDATE_CLIENT_VERSION = (0,15,0)`，见 `crates/agent/src/runner/version_gate.rs`）：服务端只在客户端版本 ≥ 此值时才下发任何 S→C 更新变体；客户端也只在**收到过服务端更新消息**后才上行 `ClientUpdateCheck` / `ClientUpdateRequest`（否则新客户端配老服务端会因未知 bincode 变体断连）。因此服务端即使是「已是最新」也**必须**回一条 `ClientUpdateUpToDate`——它同时是「对端理解更新协议」的能力凭据。

**更新源与产物选择**：`control_plane/update.rs::select_update` 从归档中取**语义化版本最高、且含可自动替换 GUI 产物**的目录（不依赖 `latest` 软链——它只由 CLI 发布流程维护）。可自动替换 = 文件名以 `rust-tunnel-client-gui-` 开头 + 扩展名 `.exe` 或无扩展名 + `SHA256SUMS` 有 64 位摘要；CLI 二进制、Windows 单包 zip、macOS dmg 一律排除（本机自更新不做解包）。macOS 发布 workflow 因此额外上传裸二进制 `rust-tunnel-client-gui-macos-aarch64`（dmg 仍保留给首次安装）。

**服务 exe 随客户端一起下发**（`MIN_SERVICE_UPDATE_CLIENT_VERSION = (0,20,0)`）：同一版本目录里的 `rust-tunnel-client-service-*.exe` 只在客户端版本 ≥ 0.20.0 时才追加进 `ClientUpdateAvailable.files`（GUI 在前、服务在后）。**老客户端绝不能收到它**——0.15–0.19 的 `on_available` 会取「第一个平台匹配的文件」，会把服务 exe 当成自己的 GUI 覆盖掉。线上协议**零改动**（仍是一个 `Vec<ClientUpdateFile>`），因此新服务端可以先于客户端部署：老客户端照旧只拿 GUI。`handle_update_request` 也只对 ≥ 0.20.0 的客户端放行服务 exe 的下载请求。

**传输**：`ClientUpdateChunk` 按 **256 KiB** 分片（控制通道单条上限 1 MiB），带 `transfer_id` + `offset`，最后一片 `data` 为空、`last = true`；同一客户端同一时刻只允许一个传输（`ClientEntry::update_transfer_active` 原子位），失败/结束必定释放。分片在独立任务里推送，不阻塞控制通道读循环。客户端本轮要下载两个文件时**顺序**复用同一个传输位（GUI 完成后再请求服务 exe），每一片仍带独立的 `transfer_id`。

**客户端**（`crates/client-gui/src/update.rs`）：`UpdateBridge` 在控制通道读循环里同步收包（`ClientUpdateBridge`，接口在 `common/update_bridge.rs`，与 `RemoteBridge` 同构），内存拼装 → **SHA-256 必校验** → 写临时目录 → GUI 线程取走并替换。哈希不匹配/长度不符**绝不**进入可执行状态。UI 弹窗（`ui/update.rs::modal`）显示版本号并常驻所有 Tab 之上，首次出现强制显示并聚焦窗口；设置页提供「自动检查更新」（6 小时轮询，默认开）、手动检查、「稍后」与「跳过此版本」（后者落盘 `GuiPrefs.update_skipped_version`，新版本到来时自动重新弹窗）。

**替换与重启（GUI）**：替换在后台线程执行（提权会阻塞到子进程结束，放 UI 线程就是「界面无响应」）。Windows 上若本机装了无人值守服务（`%ProgramFiles%\rust-tunnel\rust-tunnel-client-service.exe` 存在），**先提权**执行新服务 exe 的 `--install --gui-exe <新 GUI>`（停服务 → 复制两个 exe → 重新注册；`--host-config` 不传，保留现有 `host.toml`；按更新前的运行状态决定是否带 `--start`），**成功后才**用下载到的新 GUI exe 拉起 `--apply-update <target>`。**任何一步失败（含用户取消 UAC）两端都不更新**：GUI 与服务实例的数据面 `SESSION_PROTO_VERSION` 是硬相等校验，半套更新会让远程控制直接握手失败。已下载的包留在临时目录供「重试」直接重装。Unix 走同目录临时文件 + 原子 `rename` 再拉起新进程。

**替换与重启（无人值守实例）**：服务在跑时 GUI 被动、根本不注册，走不了上面的路。无头实例（`--instance`）自己接同一条更新通道（`UpdateBridge::for_instance`）：收到通告就自动下载 GUI + 服务 exe，两个都到齐后**以 SYSTEM 身份**就地换掉 `%ProgramFiles%` 下的两个副本（先写 `.<name>.new`，再把运行中的旧 exe 改名为 `.<name>.old`，最后换入；服务先换、实例后换，实例失败会回滚服务），成功后置 `exit` 让实例退出，服务立刻拉起新实例——**不需要 UAC**。服务 exe 的映像在下次服务启动（通常重启）时才切换，但真正承载业务的是实例 exe，新版本当拍生效。**实例更新会重启进程，正在进行的远程会话会中断**（控制器重连即可）；这是无人值守自动更新的代价，换版本不重启就得长期停在旧版本。下载中断/写入失败都继续跑旧版本（5 分钟冷却后由下次通告重试），绝不半套更新。实例没有 GUI 的每帧 `maybe_check`，故 `instance.rs` 另起一个 30 分钟的定期 `ClientUpdateCheck` 定时器（服务端只在注册时主动推一次）。

**「服务是否已安装」必须查 SCM，不能 `is_file()`**：`--install` 把 `%ProgramFiles%\rust-tunnel\` 的 DACL 收敛到「仅 SYSTEM + Administrators」，**未提权的 GUI 连 stat 都不进去**——`Path::is_file()` 会把 `ACCESS_DENIED` 折成 `false`，于是更新会永远跳过服务。`update::service_installed()` 用 `service::query_state()`（查询服务状态对普通用户开放）。查询本身失败按「没装」处理：宁可漏一次服务更新，也不能把普通 GUI 更新变成「顺手 `--install` 注册一个 AUTO_START 服务」。

**迁移与发布顺序**：0.20.0 之前的客户端不认识服务更新（GUI 只换自己；无人值守实例根本没有更新桥），所以**已经在跑的旧服务/旧实例需要手工装一次 0.20.0**，之后两条路径才会自动。0.19 → 0.20 的 GUI 自更新仍只换 GUI（服务端按 `MIN_SERVICE_UPDATE_CLIENT_VERSION` 不给老客户端下发服务 exe），服务要等到下一次更新（0.21）才随 GUI 补齐；这期间数据面协议未变（v9），新旧混跑不影响远程控制。服务端可以先部署（门控只影响下发内容），客户端铺到 0.20.0 后服务更新才对 ≥0.20.0 的客户端生效；若要让新版服务端对旧客户端也立即生效，需把门控降下来，但那会让 0.15–0.19 把服务 exe 当成 GUI——别这么做。

**抬版清单**：服务端看到的是 `Register.client_version`，它在 **`crates/client`** 里用 `env!("CARGO_PKG_VERSION")` 拼出来，**不是** `client-gui` 的版本。所以 0.20.0 必须同时抬 `crates/client`、`crates/client-gui`、`crates/win-service`、`crates/win-platform` 四处（`Cargo.lock` 随之更新），并让 `v0.20.0` 与 `gui-v0.20.0` 两个 tag 指向同一版本一起发；只抬 GUI 会让门控按 0.17.0 判定，服务 exe 永远不下发。`crates/win-service` 的 `--install` 新增了 `--gui-sha256` / `--self-sha256`（复制前复验），GUI 更新路径会带上它们；手工安装不传，行为与从前一致。

#### Windows 无人值守被控端（SYSTEM 服务 + 无头实例）

**要解决的两个问题**：控制 Windows 客户端时，**锁屏/登录界面**点不动，**任务管理器**这类提权窗口也点不动。前者根因三层且缺一不可（Winlogon 桌面 ACL 不放行 Administrators + UIPI + 会话隔离），**「以管理员身份运行」无效**；后者**只是 UIPI**——任务管理器在**默认桌面**上，只是完整性为 high，而 GUI 是 medium。唯一正路都是 SYSTEM 服务把进程拉进控制台会话。完整架构/部署/排障见根目录 `WINDOWS-SECURE-DESKTOP.md`。

**两个产物、两条工具链**：服务 exe 走 MSVC（不牵 libvpx），「实例」就是 GUI exe 的 `--instance` 无头模式（走 GNU leg）。`win-platform` **不得**依赖 `remote`，否则 libvpx 会被拖进服务 exe，而 MSVC 没有预编译库。

**采集路径（混合，2026-11-16）**：实例的 `Default` 桌面用 WGC（优先）/DXGI + GPU 纹理直通（4K 采集由 VideoProcessor 一步缩到 1920，编码器吃 GPU 纹理），`Winlogon` / `Screen-saver` 回退 GDI `BitBlt`（安全桌面只有它可靠）。分流在 `win_hybrid.rs`，桌面状态与显示器几何由桌面跟随 worker 经 `Shared` 发布；GPU 失败/不适用的每一帧都回退 BitBlt，**不会永久关闭**进程级 GPU。实例的 `supports_gpu_capture()` 每会话只在 `Default` 桌面返回 true；第一帧实际结果把会话定格成 `Enabled`/`Disabled`（见上）。遥测（协议 v8 的 `HostStats`）如实上报最近一帧的后端：会话角标应显示「采集方式 WGC/DXGI、采集耗时毫秒级、GPU 零拷贝（采集）GPU」；若仍显示 BitBlt，说明进程/SYSTEM 环境下 WGC 不可用或第一帧锁存成了 CPU。真机验证用 `win-video-capture-probe`（见 `WINDOWS-SECURE-DESKTOP.md` 第 4 趟）或直接看会话角标；现场强制回 BitBlt 用 `RUST_TUNNEL_DISABLE_GPU_CAPTURE=1`（同时关掉 GPU 纹理与 WGC 的 CPU 回读）。

**身份归属是硬需求，且被动不能是静默的**：服务端按**客户端名**唯一，同名注册会互相踢（用户看到的是「远程控制每隔几秒掉一次」）。所以服务在跑时本机 GUI **不注册**自己的控制连接、转为被动：`service_watch.rs` 每 3s 查一次 SCM，`IdentityOwner` 走 `watch` 通道送到 UI。**`Pending`（启动中与停止中）也算服务在承载**——启动中它马上要注册，停止中它的实例可能还活着（`kill_instance` 最多等 10 秒）。`Unknown`（SCM 查询失败）**保持原归属**，是刻意的迟滞。被动时状态页横幅、托盘 tooltip、托盘首行都改说「服务接管」——托盘**不能**再说「已连接/离线」，那两句都是假的（本窗口压根没注册）。

**联锁是两层的，服务侧那道才是真闸门**：`--install` **默认不启动服务**（用 `--host-config` + `--start` 时是「复制 → 加固 → 注册 → 写配置 → 启动」在**同一个提权进程**里一次做完，GUI 的「安装并启用」走的就是这条），而实例读的是 `%ProgramData%\rust-tunnel\host.toml`（含服务端**全局 token**——谁拿到它就能以这台机器的身份注册；GUI 一键流程从**当前客户端配置**里取，纯命令行安装仍要手工写）。音频开关是同一文件的 `audio`（缺省 `true`），GUI「安装并启用」把当前偏好原样写入；`false` 时实例的音频任务直接上报「本机已停止转发声音」，不会打开 loopback 设备。若在 `host.toml` 不存在时启动服务，GUI 立刻转被动、而服务实例又读不到配置注册不了，**整机彻底失去客户端身份**。两层是：① **GUI 侧**（`service_ui::start_blocked_reason`）只在**确定**缺失（`HostConfigPresence::Absent`）时禁用启动按钮并写明原因，作用仅是替用户省一次白跑的 UAC——那个目录的 DACL 只放行 SYSTEM + Administrators，未提权的窗口连 `stat` 都进不去，**读不到 ≠ 不存在**，故 `Unknown` **放行**（曾用 `Path::is_file()`，它把 `ACCESS_DENIED` 折叠成 `false`，等于对**几乎每个正常 GUI** 永久禁用启动按钮、还配一句错的解释）；② **服务侧**（`win-service::supervise::refuse_reason`）在监督循环**每轮开头**（`wait_for_console_session` **之前**）查 `host.toml` 与实例 exe，缺任一个就 `error!` 记明原因并 `service::request_self_stop()`——**开机那条路径绕过了 GUI**（AUTO_START 的服务比 LogonUI 先起，那时用户还没打开过面板），少了这道闸门，机器会停在「服务 `Running`、却没有任何实例在注册」而面板谎称「服务正在承载客户端身份」。自停是**正常停止**（`SERVICE_STOPPED` + 退出码 0），故不触发安装时写下的 `SC_ACTION_RESTART` 失败动作，GUI 的 3s 轮询立刻拿回身份。`request_self_stop()` 只置标志、**不走 SCM**：`stop()` 在 `START_PENDING` 会被 1061 拒掉，而一个「发现自己根本无法提供服务」的服务必须能无条件干净地停下。`--start` 相应**按结果判定**（等 `Running` → 稳 1.5s 复看，自停就报错并指向 `service.log`），因为 `StartServiceW` **不等**服务进入运行态就返回。

**GUI 的四个操作走同一条提权路径**：安装/卸载/启停都由 GUI 用 `ShellExecuteExW` + `runas` 动词重新拉起服务 exe（`elevate::run_elevated`），**GUI 自己不需要是管理员**。用 `ShellExecuteExW` 而非 `ShellExecuteW` 才分得清三件事：用户在 UAC 上点「否」（`ERROR_CANCELLED` 1223 → 静默处理，**不是**错误）、子进程**自己的**退出码、以及子进程跑完没有。

**探针的闸门至今没跑过，含义已经变了**：`--probe` / `--probe-in-service` / 装完重启三趟与全部判据都还在，但仓库里**没有任何实测记录**（本机是 macOS，只有用户能在真 Windows 机器上跑）。阶段 1 的编排是**用户拍板并行**推进的，理由写在下面。所以闸门不再挡「写阶段 1」，而是挡**对外声称登录页那一半可用**——若结论为否，备选是 `SendSAS`，或退回「文档化限制 + 让机器不锁屏」。**问题 2（任务管理器）不依赖闸门**：它是默认桌面上的 UIPI 问题，实例以 SYSTEM 运行即 ≥ high。

**探针的三条跑法**：`--probe`（对照组，已登录未锁屏）/ `--probe-in-service`（**闸门趟**，敲完命令立刻 Win+L）/ 装完重启（开机停在登录界面，靠服务 `SERVICE_AUTO_START`）。后两趟**不能**「先锁屏再敲命令」——那时没有控制台，所以反过来让测试体自己等：子实例等安全桌面（`CHILD_SECURE_WAIT` 60s），服务侧等控制台会话出现（`CONSOLE_SESSION_WAIT` 60s，开机时服务比 LogonUI 先起）。两个等待之和必须明显小于 `win-service` 的 `SERVICE_PROBE_TIMEOUT`（有单测钉住）。报告与截图落 `%ProgramData%\rust-tunnel\probe\`（阶段 0 曾放 `C:\Users\Public\`——全局可写、可被 junction 重定向，而报告含**锁屏截图**）。

**几处容易踩的坑（都带注释）**：① 服务在会话 0，那里的窗口站是 `Service-0x0-3e7$`，**够不着**控制台会话的 `WinSta0`，所以测量必须由 `CreateProcessAsUserW` 拉起的子实例做，且 `lpDesktop` 明确写 `winsta0\default`；② `SetThreadDesktop` 只能在**一次性专用线程**上调用（不能复用、不能用 `spawn_blocking` 的池线程、挂载前线程不得拥有窗口）；③ **父进程预先算出子实例的报告路径**再把时间戳用 `--probe-instance <戳>` 交给它——两边各算各的「当前时刻」会跨秒对不上，一次**成功**的测量会被报成「子实例没写出报告」（戳必须过 `sysinfo::is_valid_stamp`，它会拼进文件名）；④ 采集用 BitBlt（`biHeight` 取负 = 自顶向下），BMP 落盘的是**原始 BGRA**（只把第 4 个字节的 alpha 统一写成 255，否则「honor alpha」的看图工具会把这张**证据图**显示成全透明、被误读成采集失败；B/G/R 逐字节原样）、ASCII 缩略图来自转换后的 RGBA——刻意不让两者共用一条通道，通道顺序错了才看得出来。⑤ **判据必须 fail-closed**：`OpenInputDesktop` 成功不等于线程切过去了，所以挂载后要重新读 `GetThreadDesktop` 比对（报告里的 `挂载后是否等于输入桌面`），不等就**跳过注入**；光标判据带 ±1 像素容差（归一化坐标的取整约定差一点就是 1 像素，逐像素相等会把一次成功判成「存疑」），且注入前光标已在目标点时要标 `⚠ 本次鼠标判据无效`。服务侧**抓子实例报告下结论时必须先看 `是否安全桌面: true`**——否则默认桌面的对照组读数会被当成闸门通过。

### 前端架构
- 位于 `frontend/`，构建产物输出到 `frontend-dist/`（gitignored，由 `rust-embed` 嵌入）
- **react-router-dom v6** — `App.tsx` 中 `createBrowserRouter` + `ProtectedRoute` 守卫路由
- **React Query v5**（`@tanstack/react-query`）— 数据获取和缓存
- **无全局状态管理库** — 状态通过 React Query + 组件本地状态管理
- Vite 开发服务器将 `/api` 代理到 `localhost:3000`（服务器 API 端口）
- 共享组件在 `frontend/src/components/shared/`：`ChartContainer`、`StatCard`、`TimeRangeSelector`、`MobileBottomNav`
- 页面在 `frontend/src/pages/`；LLM 网关管理（`LLMPage`）与统一知识容器（`KnowledgePage`，`components/knowledge/source/` 下的 `SourceSection`/`SourceList`/`SourceDetail`/`SourceDialog`/`SourceDocsTab`，页面层仍用 `components/knowledge/wiki/` 的 `WikiPagesTab`/`WikiGraph`/`WikiPageDialog`）；AI agent 工作台（`AgentPage`，含 `components/agent/`：会话列表、消息流、审批弹层、@文件引用、workspace 管理——含 ACP 引擎配置：agent_type/agent_path/llm_model_id）
- TypeScript 类型定义集中 在 `frontend/src/types/index.ts`
- API 客户端在 `frontend/src/api/client.ts`：Axios + JWT 拦截器

### 数据库 (SQLite)
- 位置：`--db-path` 配置（默认 `./data/rust-tunnel.db`），WAL 模式
- 表：`port_traffic`（聚合流量）、`traffic_buckets`（分钟级，保留 24h）、`client_sessions`（连接历史）、`connection_quality_history`（质量数据）、`shadowsocks_config`、`trojan_config`、`rustdesk_config`（单行：enabled/端口/key/relay_server/always_use_relay/local_ip/mask）、`log_entries`、`clients`（客户端名录）、`server_auth`（客户端接入 token）、`knowledge_sources`（统一知识容器，`index_vector`/`index_pages` 双索引开关，取代旧 `rag_knowledge_bases`/`agent_wikis`）/ `knowledge_docs`（文档）/ `knowledge_doc_index`（按 `(doc_id, kind)` 记录每侧索引进度）/ `knowledge_chunks`（向量分块）/ `knowledge_pages`（页面）/ `knowledge_page_edges`（页面链接图谱；向量本体存于 `<db_parent>/rag/<source_id>/`，文档原文存于 `<db_parent>/knowledge_docs/<source_id>/`；**向量索引仅随 `rag` feature 编译**）、`agent_workspaces` / `agent_sessions`（含 `role_id`） / `agent_messages` / `agent_roles`（多角色子代理定义）；模型 `extra_config` JSON 支持 `upstream_protocol: "responses"` 声明上游使用 Responses API（与 `agent_context_limit` 并列，缺省 `chat_completions`）

### API 端点
- 公开：`POST /api/login`、`GET /api/health`、`GET /api/knowledge/events`（SSE，`?token=` 认证）
- 受保护（设置密码时需 JWT）：`/api/clients`、`/api/server-auth`、`/api/traffic`、`/api/metrics`、`/api/quality/*`、`/api/shadowsocks/*`、`/api/trojan/*`、`/api/logs/*`、`POST /api/logout`
- RustDesk（新）：`GET /api/rustdesk`（配置 + 运行状态；密钥只回 `key_set` 布尔，明文永不回传）、`POST /api/rustdesk`（更新并应用；`generate_key: true` 时一次性回传新公钥）
- 产物归档下载：`GET /api/client-downloads`、`GET /api/wiki-downloads`（列表，需 JWT）；`GET /api/client-downloads/:version/:file`、`GET /api/wiki-downloads/:version/:file`（下载，公开路由自带 `?token=` 校验，因 `<a download>` 不能带 Header）。分别只读 `client_dist_dir` / `wiki_dist_dir`，由 `release-client.yml` + `release-client-gui.yml` / `release-wiki-client.yml` 落盘
- LLM 网关（既有）：`/api/llm/gateway`、`/api/llm/providers`、`/api/llm/providers/:id`、`/api/llm/providers/:provider_id/models`、`/api/llm/models`、`/api/llm/models/:id`、`/api/llm/api-keys`、`/api/llm/api-keys/:id`、`/api/llm/usage/*`、`POST /v1/responses`（Responses API 入口，与 `/v1/chat/completions` 同域）
- 统一知识容器（新）：`/api/knowledge`（CRUD）、`/api/knowledge/:id`、`/api/knowledge/:id/docs`、`/api/knowledge/:id/docs/:doc_id`（含 `/reindex`）、`/api/knowledge/test-embedding`、`/api/knowledge/:id/query`（向量检索预览）、`/api/knowledge/:id/pages`、`/api/knowledge/:id/graph`、`/api/knowledge/:id/search`、`/api/knowledge/search`
- AI agent（新）：`GET /api/agent/ws`（WebSocket 回合/事件流）、`/api/agent/workspaces`（CRUD，含 `/files`、`/sessions`）、`/api/agent/sessions/:id`（含 `/model`、`/archive`、`/messages`、`/role`）、`/api/agent/default-model`、`/api/agent/roles`（多角色子代理 CRUD，含 `:id/toggle`）
- 完整列表见 `crates/server/src/mgmt/api/mod.rs`

## 代码模式

- 异步运行时：Tokio（full features）
- 错误处理：二进制文件用 `anyhow`，库错误用 `thiserror`
- 序列化：控制消息用 `bincode`，API 用 `serde_json`
- Web 框架：Axum + `rust-embed` 嵌入前端
- 状态共享：`Arc<Mutex<T>>`、`tokio::sync::Mutex`
- 数据库：sqlx + SQLite（WAL 模式）
- TLS：rustls + tokio-rustls + rcgen（自签名证书）
- 配置：Clap（CLI）+ figment（TOML + 环境变量），三级优先级：CLI > 环境变量 > 配置文件 > 默认值（`crates/server/src/config/`）
- 质量监控：基于心跳的 RTT 测量，通过序列号追踪丢包
- Lint：项目配置为 `clippy::pedantic`

## 配置参考

所有 CLI 参数和环境变量可通过 `--help` 查看。完整的 TOML 配置示例见：
- 服务器：`config/server.example.toml`
- 客户端：`config/client.example.toml`
- 环境变量示例：`.env.example`

客户端 CLI 已收敛为仅三个必填/常用参数：
- `--server <host:port>` — 服务器地址
- `--password <token>` — 客户端 token（即服务端 Web 管理的 client_token）
- `--name <name>` — 客户端名称（默认系统 hostname）
- `--tls` / `--tls-insecure` — TLS 连接控制

服务端 RustDesk 配置项（默认关闭；TOML `rustdesk_*` / 环境变量 `RUSTDESK_*` / CLI `--rustdesk-*`，同一条三级优先级）：
- `rustdesk_enabled` — 总开关（hbbs 会合 + hbbr 中继共用一个开关）
- `rustdesk_rendezvous_port` / `rustdesk_relay_port` — 会合端口（NAT 探测端口 = 此值 - 1）/ 中继端口
- `rustdesk_key` — 服务端密钥；这是**密钥对**，推荐由 Web UI 的「生成」按钮产生（服务端持久化私钥、一次性展示公钥）
- `rustdesk_relay_server` — 下发给客户端的中继地址（空 = 由客户端自定）
- `rustdesk_always_use_relay` — 强制客户端走中继
- `rustdesk_local_ip` / `rustdesk_mask` — 内网中继地址 / 内网判定 CIDR（如 `192.168.0.0/16`）

优先级 default < 配置文件 < 环境变量 < CLI；但**一旦落库（打开过开关）就以 DB 为唯一事实来源**，配置文件只在首次 seed。完整部署/防火墙/密钥/真机验证见根目录 `RUSTDESK.md`。

## 测试

- **单元测试**：位于各 crate 的 `src/` 内（`crates/{common,client,server}/src/`），源文件末尾 `#[cfg(test)] mod tests`（协议专属测试如 shadowsocks/trojan/http2/aliyun 均已合并回各自实现文件）。
- **集成测试**：位于根元包 `tests/`，以黑盒 API/协议流程为主（`tunnel_basic.rs`、`api_auth.rs`、`stats_*.rs`、`rustdesk_server.rs` 等）。

## CI/CD

GitHub Actions 工作流：

- `ci.yml`（push/PR 到 `main` / 手动）— **推送阶段的 Windows 门禁**，两个 job：
  `windows-tests`（`windows-latest`，跑 `cargo test -p rust-tunnel-win-platform -p rust-tunnel-win-service -p rust-tunnel-win-video -p rust-tunnel-win-audio`）
  与 `windows-gnu-typecheck`（`ubuntu-latest`，`cargo check`/`clippy -- -D warnings` 交叉到
  `x86_64-pc-windows-gnu`，覆盖 `client-gui`——**出厂 Windows GUI 正是这个 target**）。
  之所以需要它：`release-client-gui.yml` 的 `build-service` 那条测试命令只在**打 tag 时**跑，回归要到发布那一刻才暴露；
  而 GNU leg 在打 tag 之前**没有任何东西**做过类型检查。用 `-p` 指定 crate，**绝不用 `--workspace`**
  （会拖进第三方 qdrant-edge）。**不覆盖**：`client-gui` 的宿主目标单测（Linux 上链接 eframe 需要
  一整套 X11/Wayland/GL 开发库，未经验证的红色 job 比如实记录缺口更糟），以及**全部运行时行为**
  ——check/clippy 不链接，Windows 安全桌面特性的验收必须在真机上做
- `release-server.yml`（手动触发）— 单 job：Node 构建前端 → `x86_64-unknown-linux-musl` 静态编译
  （`--features rag,embed-frontend`）→ 从 `contrib/config.toml.template` 渲染配置（占位符替换后校验）
  → SCP 二进制 + systemd 服务 + 配置 → SSH 重启服务
- `release-client.yml`（tag `v*` / 手动）— **只负责 CLI 客户端**：四平台矩阵（Linux musl、
  macOS x86_64/aarch64、Windows MSVC），各 runner 直接 SCP 到 `${DEPLOY_PATH}/client/<version>/`，
  `finalize-client` 在服务端**合并** `SHA256SUMS`（只负责 CLI 那几个文件名，保留 GUI 流程写下的
  其余行）并更新 `latest` 软链。`latest` 由本流程独占维护，代表「包含完整 CLI 产物的最新版本」
- `release-client-gui.yml`（tag `gui-v*` / 手动）— **只负责 GUI 客户端**：Windows（GNU）GUI exe +
  macOS arm64 `.dmg` + Windows（MSVC）无人值守服务 exe + Windows 单包 zip。产物落到与 CLI
  **同一个** `${DEPLOY_PATH}/client/<version>/`（版本 = tag 去掉 `gui-` 前缀，使下载页按版本聚合
  两类产物；保留前缀会让 `semver_key` 解析失败、版本排到列表末尾）。
  **不再构建/发布 Linux GUI**，Linux 只提供 CLI 客户端（musl）。
  `build-service` 额外构建并上传 `rust-tunnel-client-service-windows-x86_64.exe`
  （无人值守服务；**不依赖 libvpx** 故走 MSVC 即可，不需要 `build-gui` 那套 MSYS2 曲折；
  资产名必须以 `rust-tunnel-client-` 开头，`finalize-gui` 的 glob 与下载页的
  `platform_from_filename` 才认得它），并在同一 job 跑
  `cargo test -p rust-tunnel-win-platform -p rust-tunnel-win-service -p rust-tunnel-win-video -p rust-tunnel-win-audio`
  ——那批单测是 `#[cfg(windows)]`（`probe`/`service`/`process`/`host`，以及 `win-video` 的像素/颜色/解码纯逻辑），
  **这个 runner 是仓库里唯一能真正跑它们的机器**，本机 macOS 只能做类型检查；
  但这条只在打 tag 时跑，推送阶段的同一批测试由 `ci.yml` 的 `windows-tests` 兜住。
  `package-windows` 在 GUI 与服务上传完成后把两个 exe 从服务器拉回、
  打成 `rust-tunnel-client-gui-windows-x86_64.zip`（内含两个 exe + README），用户只下载一次；
  GUI 的「安装并启用」会自动找同目录的服务 exe，并把当前客户端配置写成 `host.toml`。
  `finalize-gui` 用同一套合并规则写 `SHA256SUMS`（只负责 gui/service/dmg 那几个文件名），
  且**不碰 `latest`**——GUI-only 版本仍是独立版本目录，会出现在下载页的版本列表里，但不会成为 `latest`。
  协议不兼容的版本必须让 `v*` 与 `gui-v*` 两个 tag 指向同一版本一起发；
  `crates/client`/`crates/client-gui` 的 `CARGO_PKG_VERSION` 仍要手工抬高（workflow 不写回 `Cargo.toml`），
  但**tag 与清单版本不一致会直接让发布失败**（见下面的版本解析小节），不会再悄悄发出一个版本自述错误的产物
- `release-wiki-client.yml`（tag `wiki-v*` / 手动）— wiki 桌面端（Tauri 2）安装包：macOS
  aarch64/x86_64 `.dmg` + Windows `.msi`/NSIS `.exe`，产物重命名为
  `wiki-desktop-<os>-<arch>[-setup].<ext>` 后 SCP 到 `${DEPLOY_PATH}/wiki/<version>/`（版本目录名 =
  tag 去掉 `wiki-` 前缀），`finalize-wiki` 生成校验和与 `latest` 软链。触发用独立 tag 前缀与
  客户端发布（`release-client.yml` / `release-client-gui.yml`）解耦（Tauri 打包耗时远高于单文件二进制）

**版本目录名由清单自动解析（`scripts/resolve-release-version.sh`，三个客户端 workflow 共用）**：手动触发**不再需要手填版本**，默认取对应 crate 的 `Cargo.toml` 里 `[package] version`（CLI → `crates/client`、GUI → `crates/client-gui`、wiki → `crates/wiki-serve`）拼成 `v<版本>`；tag 触发仍以 tag 为准（`v*` / `gui-v*` / `wiki-v*`，去掉 `gui-`/`wiki-` 前缀），但会校验它**必须等于** `v<清单版本>`，不一致就 `::error::` 硬失败——客户端上报给服务端的版本号就是 `CARGO_PKG_VERSION`，tag 与之不符会让版本门控（`MIN_REMOTE_CLIENT_VERSION` / `MIN_UPDATE_CLIENT_VERSION` / 数据面 `SESSION_PROTO_VERSION`）判断错对象。`version` 输入保留为显式覆盖（发 `dev-<sha7>` 之类的试验目录），覆盖时只告警。wiki 额外用 `--cross-check` 校验 `tauri.conf.json` 的 `version`（Tauri 打包器以它为准）与 `Cargo.toml` 一致。目录名仍按服务端 `is_safe_segment` 的字符集校验。

部署使用 systemd（`contrib/rust-tunnel-server.service`），配置模板 `contrib/config.toml.template`。
两个归档目录经 `client_dist_dir` / `wiki_dist_dir` 交给服务端只读，Web「下载」页分两个分区展示。

### Wiki Desktop 额外说明

- `crates/wiki-serve` 含 Codex agent 侧车（`agent/{mod,bridge,jsonrpc,process,config,resolve}`，`bridge` 为唯一 `#[cfg(feature=\"tauri\")]` 文件，`resolve` 解析链 sidecar→override→PATH + 熔断计数）。
- `wiki-desktop-ui/src/lib/agent/types/` 为 vendor 官方 TS 类型（pin `rust-v0.153.4`，脚本 `wiki-desktop-ui/scripts/vendor-codex-types.sh` 为单一事实来源，与 workflow `env.CODEX_VERSION` 同步）。
