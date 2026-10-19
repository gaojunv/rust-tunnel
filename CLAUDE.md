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
- `crates/remote`（`rust-tunnel-remote`）— GUI 客户端互相远程控制的**协议 + 引擎**（会话协议/端到端加密/信令/会话循环/VP9 4:4:4 视频编码/文件传输），**无平台或 UI 依赖**（但含 vendored 的便携 C 编解码器 `vendor/shiguredo_libvpx`）
- `crates/win-platform`（`rust-tunnel-win-platform`）— Windows 无人值守被控端的**平台层**：可移植模块（`error`/`pixels`/`report`/`session`/`desktop`/`monitor`/`inject`/`token`/`sysinfo`，纯 std、macOS 上全量单测）+ 仅在 Windows 编译的裸 FFI 模块（`capture`/`handle`/`service`/`probe`/`process`）。用 `windows-sys`（无 wrapper、无 libvpx），故本机可 `--target x86_64-pc-windows-gnu` 做**类型**检查
- `crates/win-service`（`rust-tunnel-client-service`）— Windows 无人值守被控端的 **SYSTEM 服务** bin（哑监督者：SCM 注册/安装卸载/启停、把无头实例拉进控制台会话、随会话变化重启）。`--run` 是本体（监督 `rust-tunnel-client-gui.exe --instance`）；`--install` 把两个 exe 复制到 `%ProgramFiles%\rust-tunnel\` 并加固 DACL（**服务以 SYSTEM 运行，exe 放在任何用户可写的目录里 = 替换 exe 即拿到 SYSTEM**）；`--probe`/`--probe-in-service`/`--probe-instance` 是阶段 0 的真机探针，保留
- 依赖单向：`common ← client`、`common ← server`、`common ← remote ← client-gui`；`rustdesk` **不依赖任何本仓 crate**（只依赖 prost/ed25519-dalek/bytes/tokio）；`win-platform ← win-service`，且 `win-platform` **不得**依赖 `remote`（否则 libvpx 会被拖进服务 exe，而服务 exe 走 MSVC、没有预编译库）
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
- `session.rs` — 会话帧（4 字节大端长度前缀 + bincode，上限 16 MiB——屏幕帧远大于 `common::protocol` 的 1 MiB）+ `Lane{Control,Screen,File}` 多路复用车道（`File` 预留给文件传输）。**`SESSION_PROTO_VERSION` 已到 2**（阶段 2 换成 VP9 数据面），两端靠 `Hello.proto_version` 硬相等实现「混合版本明文干净拒绝」——`Hello`(判别式 0)/`HelloAck`(1) 与内嵌的 `DisplayInfo` **逐字节冻结**（`discriminant_golden` + `hello_and_helloack_wire_bytes_are_frozen` 两条机械锁），新变体一律追加在末尾
- `crypto.rs` — X25519 临时密钥交换 + ChaCha20-Poly1305，双向独立密钥与 nonce 计数器。**一次性 token 进 KDF（别去掉）**：裸 ECDH 挡不住中继方——`Hello`/`HelloAck` 都是明文，它可替换两端临时公钥做完整 MITM；token 经加密控制通道下发且从不上数据面，攻击者不知道它就算不出密钥 → 两端密钥必然不一致 → 首帧 AEAD 认证失败。另拒绝低阶点（`was_contributory`）
- `auth.rs` — 握手 HMAC-SHA256，字段用 **8 字节长度前缀**（`nonce`/公钥是二进制，固定分隔符会有拼接歧义）；`Hello` 的 MAC 覆盖控制器临时公钥，`HelloAck` 的 `hmac_ack` 绑定被控端临时公钥，控制器**真正校验**它；挂起会话 TTL 60s、鉴权成功即消费（重放被拒）、MAC 失败保留以允许重试
- `signaling.rs` — RustDesk 会合/中继客户端（注册/心跳/打洞/中继请求）；`RustDeskTransport` 是真实数据面建立的唯一实现，`LazyTransport` 把它的构造推迟到 `RemoteParams` 到手（GUI 与 e2e 共用）
- `transport.rs` — 数据面传输抽象（`BoxDuplex`/`EstablishRequest`/`TransportEstablisher`），把「建立一条双向字节流」与「会话跑在这条流上」解耦；**建立契约**写在模块文档里（中继路径只发一帧 `RequestRelay` 后转裸字节、不等应答，并把 `early` 与之合并写出）。`TransportPath{Direct, Relay, Punch}` 的硬契约是「**必须如实上报实际走的路径**」——中继兜底绝不能报成 `Punch`（UI 据此告诉用户字节有没有过服务器）；`EstablishRequest.punch_cookie: [u8; 8]` 是打洞会话标识，**Rust 内部类型、不上线**
- `reliable.rs` — **用户态可靠层**（UDP 打洞路径专用）：最小 TCP 式 ARQ over datagrams。27 字节段头（`magic "RTP1"` / `conn_id` 固定在**偏移 4**——demux 只读前 12 字节即可路由，配机械锁防日后挪位 / `seq` 段序号 / 累计 `ack` + 16 位位图 / `flags` / `window` / `len`）、保序重组、延迟确认 50ms、Jacobson/Karels RTO（min 200ms / max 2s / 指数退避）、**3-dup-ack 快速重传必须早于 RTO**、AIMD cwnd、接收窗口流控、FIN/RST。载荷上限 **1200 B**（`27+1200 = 1227` ≤ IPv6 最小 MTU 预算，**任何情况下不触发 IP 分片**）。返回 `tokio::io::duplex()` 的一半 → 天然 `AsyncRead+AsyncWrite+Unpin+Send`，可直接塞进 `BoxDuplex`，且**背压是自动的**（应用不读 → 协议任务写不动 → 停止确认 → 对端窗口填满）。**为什么不用动 `session.rs`**：会话帧自带长度前缀、AEAD 又是**逐帧**的，帧边界由长度前缀恢复，所以可靠层只需提供**纯字节流**、不必保留 write 边界。**拥塞控制只做「保守地板」**——码率主控是 `qos.rs` 的应用层闭环，别让传输层跟它抢方向盘。测试用内存链路替身 + `start_paused` 虚拟时钟（dev-dep 需 `tokio/test-util`）
- `punch.rs` — 打洞的会话相关性与分发：`conn_id = HMAC-SHA256(token, b"rt-punch-v1")[..8]`（大端 u64；由一次性 `token` 派生 ⇒ hbbs 的**明文**会合流量里看不到它，第三方注入不进会话、也抢占不了打洞路径）；`peek_conn_id` **只读前 12 字节**给 demux 走快速路径；`PunchRegistry` 按 `conn_id` 路由，**未命中一律静默丢弃、绝不回复**（反反射）。`PUNCH_PROBE_INTERVAL = 250ms` / `PUNCH_BUDGET = 3s`。**`punch_back` 是必需的、不是优化**：多数家用路由是**端口受限 cone**（NAT 只放行「本机先发过包的目的地址」来的入站报文），若只有控制器单向打，被控端的 NAT 会丢掉控制器的 `SYN`（被控端从未向它发过任何东西）→ 死锁。故被控端收到 `PunchHole` 后**同时反向发探测**（裸 27 字节段、`conn_id = 0`、`flags = 0`，每 `PUNCH_PROBE_INTERVAL` 一发、共用同一 `PUNCH_BUDGET`）打开本端 NAT 的过滤窗口；控制器侧本来就在持续重发 `SYN`，两侧因此天然对称。`conn_id = 0` 是安全哨兵（demux → 登记表未命中 → protobuf 兜底 → 丢弃，**不回复**）；反向探测的目标端口取 `PunchHole.udp_port`（**服务端观测到的公网端口**，不是本地端口——报错它等于打到一个没有 NAT 映射的端口上），`udp_port == 0` 时不反向打
- `nat.rs` — 打洞前的 NAT 预判：`classify(a, b)` 是**纯函数**——从**同一个**套接字向**两个不同目的端口**（`21116/udp` 与 `21115/udp`）各探一次，比对观测到的源端口：相等 → cone（放行）、不等 → 对称（**跳过打洞、零额外等待**）、探不到 → `Unknown`（**fail-open，照打一次**）。**必须用两个目的端口、而不是两个本地套接字**：对称 NAT 对每个 `(源IP,源端口,目的)` 三元组独立映射，两个本地套接字本来就必然得到两个不同映射，**根本区分不出来**。**两个端口都必须应答**——`handle_datagram` 原本只认 `RegisterPeer`/`RegisterPk`（`TestNatRequest` 落进兜底被静默丢弃），所以主 UDP 端口也补了 `TestNatRequest` 应答，否则预判永远得到 `Unknown`。残余局限：两个目的端口同 IP，故「地址无关但端口依赖」的映射会被误判成 cone（现实中罕见，由 `NatCache` 兜住）。`NatCache` 按对端 rd_id 缓存失败 10 分钟，**只在进程内**——随传输重建自然失效，刻意不放偏好文件
- `controller.rs`/`controlled.rs` — 两侧会话循环（握手 → 画面/输入/心跳/文件）；控制器把 `Hello` 帧流水线进建立阶段，因此**会话循环不再发 `Hello`，只等 `HelloAck`**。UI 命令统一走 `ControllerCommand`（输入/切显示器/切画质/剪贴板/文件），单通道保证顺序与用户操作一致。**画面流水线**（被控端）：`采集(spawn_blocking) --有界(1)--> 编码任务(spawn_blocking) --有界--> 发送(独占 writer)`，`select!` 循环只留 cancel/心跳/输入/文件/控制。**丢弃策略两端方向相反**：被控端**编码之后绝不丢帧**（时域编码参考链一断就花屏且不自愈），背压出口只能在**编码之前**（队列满就跳过这一帧）；控制器可以丢，但必须按**序号断层**检测到并请求关键帧（`RequestKeyframe`），进入 `awaiting_keyframe` 只等 IDR
- `screen.rs` — `ScreenSource`/`InputSink`/`FrameSink`/`Clipboard` 平台 trait；`FrameSink::on_frame` 拿到的**一律是整帧** RGBA（瓦片路径已删除），`on_clipboard` 默认空实现，控制器侧不必实现 `Clipboard`（它读写的是**本机**剪贴板，由 UI 直接处理）。剪贴板是**单向按需**的：只有控制器会发 `ClipboardRequest`（被控端只应答），控制器侧收到它是异常，只记日志
- `video.rs` — VP9 4:4:4 编解码封装 + RGBA↔YUV444 转换（BT.709 **full range**，与编码器补丁 5 设的 `color_space`/`color_range` **严格一致**，两端共用同一份常数）。关键参数：`Profile1`（补丁 1，上游表达不了）、`lag_in_frames = Some(1)`（**必设**，`None` 会让 libvpx 缓冲前 25 帧）、`cpu_used = 8`、`rc_buffer_size_ms ≈ 300`（补丁 4）、`EncodingDeadline::Realtime` + `Cbr`、`tile_columns`（按宽度分档，见 `tile_columns_for`）+ `row_mt`（补丁 7）。**默认编码宽度 ≤ `MAX_ENCODE_WIDTH`（1920）**——4K 解码仍是最贵的一环，只能作显式选项。实测 libvpx 在 realtime+CBR 下**不**按场景切换自动插 IDR（`keyframes_only_at_the_configured_interval` 锁定），故无需 `kf_min_dist` 补丁。`Encoder`/`Decoder` 是 `Send` 非 `Sync`（编译期断言钉住），只能独占移进 `spawn_blocking`。`set_bitrate`/`set_fps` 走**在线重配置**（补丁 6 → `vpx_codec_enc_config_set`），**不重建编码器、不插 IDR**——这是 QoS 闭环的前提，见 `qos.rs`
- **解码并行化（补丁 7）**：`VideoDecoder::new()` 走 `DecoderConfig.threads = video_threads()` + `row_mt = true`，编码侧开 `tile_columns` + `row_mt`（两者都**不改线上格式**，见下）。本机 10 核 Apple M 系列实测（`video_bench_encode_decode`，release，合成内容）：解码 1080p 4.23→2.61 ms、4K 16.17→9.86 ms（**−38% / −39%**，同一份代码只翻这两个开关的 A/B）；瓦片顺带让**编码**快 17–20%（编码器也能跨瓦片并行），CBR 下码流字节基本不变（4K 反而 −1.7%）。**局限**：CBR 下字节数由码率控制决定，瓦片牺牲的熵效率会表现为同码率下画质略降而非字节变多——「字节没涨」不等于「压缩率没代价」，未测 PSNR。**绝对数字不可外推**：合成内容比真实桌面轻，且计划里 4K「37–58 ms」是另一台 M4 的单线程微基准，与上表不同机不同内容
- `qos.rs` — QoS 闭环（阶段 3）：控制器每秒回报的 `QosFeedback{rtt_ms, recv_kbps, decode_ms, backlog}` + 被控端本地出站队列积压 → AIMD 决策。**两条硬约束**：① 码率/帧率走**在线重配置**（补丁 6），**绝不重建编码器**——重建会强制 IDR，闭环每秒动一次就是每秒一个关键帧，正好把阶段 2 消掉的带宽尖峰引回来；只有**降编码分辨率**（几何变化）才重建 + 强制 IDR。② 降级阶梯是 码率 ×0.8 → 帧率 −1 → 分辨率 ×0.75，且**只有解码耗时 ≥ 37 ms**才走最后一级（37 这个数来自 M4 单线程微基准，**不是实测链路数据**；补丁 7 上多线程解码后它更难触发，是否仍是个有效阈值只能靠真机数据判断）；回升加性 ×1.1，不越过档位上限。`QualityPreset` 语义因此从「固定参数」变成**上限**（`EncodeOptions`），实际生效值由 `EncodeRates` 表示，两者在类型上不可混用。编码/解码侧读数经既有进程内 `SessionStats` 旁路送 UI（**不落线**）。机械锁：`rate_change_reconfigures_online_without_idr` / `resolution_change_rebuilds_and_forces_keyframe` + vendor 侧两条
- `file.rs` — 被控端共享目录的沙箱文件服务（虚拟 `/` 路径）。**两层校验缺一不可**：`validate_virtual_path` 管语法（拒 `..`/`.`/`\`/盘符/控制字符/空段），`FileService::resolve` 管语义（对**最深的已存在祖先**做 canonicalize 后 `Path::starts_with(canonical_root)`，故符号链接逃逸会被拒）。`list` 里指向根外的符号链接项直接跳过
- `transfer.rs` — 会话内文件传输状态机（两端共用）。只有**控制器**能发起（浏览/下载/上传），被控端只应答。同一时刻只允许一次传输；`CHUNK_SIZE=64 KiB` 且每块之间 `CHUNK_PAUSE=4ms` **主动让出调度**，会话循环的 `select!` 才有机会插入画面帧（中继只给一条字节流，画面与文件天然共享带宽）
- `approval.rs` — 被控端**接入确认**策略与待确认请求（`ApprovalPolicy{Auto,Confirm}`、`PendingApproval`、`APPROVAL_TIMEOUT=45s` / `APPROVAL_WAIT_TIMEOUT=60s`）。`Confirm` 是默认：被控端收到 `RemoteSessionIncoming` 后**不回响应、不登记 token、不派生任务**，只挂起等用户 `approve_pending`/`reject_pending`，到点自动拒绝。**token 绝不进 `PendingApproval`**——那是 UI 状态，凭证只在管理器内部持有。`APPROVAL_TIMEOUT` 必须明显小于服务端 `REMOTE_SESSION_TTL`（60s，见 `crates/server/src/control_plane/remote.rs`）：用户拍板的**整个过程**都要落在那个窗口内，否则响应会被服务端当未知会话丢弃，用户看到的是「卡住」而不是「被拒」
- `manager.rs` — `RemoteManager` 门面（`RemoteUiState` watch、连接请求、启停、平台注入、剪贴板/共享目录注入、接入确认门控、`RemoteBridge` 实现）
- 平台实现（xcap 抓屏 / enigo 注入 / arboard 剪贴板）**放在 `crates/client-gui`**，本 crate 保持编译面干净

#### 远程控制的控制面与数据面

**控制面走既有控制通道**（复用 `ControlMessage` + `ClientEntry.control_sender`），不新开 HTTP 鉴权面——GUI 客户端只有客户端 token，没有管理员 JWT。

**向后兼容靠版本门控**：服务端只在客户端版本 ≥ `MIN_REMOTE_CLIENT_VERSION`（`crates/agent/src/runner/version_gate.rs`）时才下发 `RemoteCapability`；客户端只在收到它之后才发新的上行变体。新客户端 + 老服务端 → 老服务端不下发 → 新客户端全程不发新变体；老客户端 + 新服务端 → 门控拦住。两边都不会因 bincode 未知变体而断连。**新增 S→C 推送时必须过 `client_supports_remote`**，否则老客户端反序列化失败会直接断连。

**数据面另有 `SESSION_PROTO_VERSION` 一道门控**（当前 2）：它**不是**控制通道的 `protocol_version`，而是数据面 `Hello` 里的字段，两端**硬相等**校验，不匹配就在握手阶段明文拒绝（`HelloAck{accepted:false}`），因此混合版本不会退化成 bincode 解码失败导致的硬断链——**前提是 `Hello`/`HelloAck`/`DisplayInfo` 的字节形状冻结**（两条机械锁守着，见上）。

**阶段 2 的发布顺序刻意分两步**（`MIN_REMOTE_CLIENT_VERSION` 上的注释写明了）：先把 0.11 客户端铺开（`SESSION_PROTO_VERSION = 2` 生效，0.11↔0.10 在握手阶段干净拒绝），**之后**再部署新服务端并把门控抬到 `(0,11,0)`。抬门控会让 0.10.x **彻底失去**远程控制（功能消失，非降级），所以必须等客户端铺开。**注意客户端上报的是 `CARGO_PKG_VERSION`**，而 `release-client.yml` 的 `VERSION` 只取 git tag、不写回 `Cargo.toml`——所以 `crates/client`/`crates/client-gui` 的版本号要手工抬。

**`crates/remote` 有 vendored 原生依赖**：VP9 4:4:4 需要 profile 1，上游 `shiguredo_libvpx` 表达不了，故 `vendor/shiguredo_libvpx/`（wrapper Apache-2.0 + 内联 libvpx BSD，与项目 MIT/Apache 相容，**无 GPL/AGPL**）打了 7 处补丁（1–3 是 4:4:4 的前提，4 是 CBR 交互级缓冲，5 是显式色彩空间/范围，**6 是运行期在线重配置码率/帧率——QoS 闭环的前提**，**7 是解码侧并行 threads + row-mt——解码原本恒为单线程**），每处带 `PATCH:` 注释写明动机，根 `Cargo.toml` 的 `[patch.crates-io]` 重定向，机械锁在 `crates/remote/tests/vendor_patches.rs`。预编译库覆盖 `aarch64-apple-darwin` / `x86_64-pc-windows-gnu` / `x86_64-unknown-linux-gnu`；**不覆盖** `x86_64-apple-darwin`（build.rs 直接 panic，已放弃该 target）与 MSVC（预编译是 mingw）。**Windows 那份预编译还是 msvcrt 味的 mingw-w64 构建**（它引用 `__imp__setjmp`，而 `_setjmp` 只在 msvcrt 的导入库里），所以那个 leg 的链接器也必须是 msvcrt 味的——runner 镜像自带的 `C:\mingw64` 是 **UCRT** 味，链不了；必须用 MSYS2 的 `mingw64` 环境。别把那条 step 简化回 `C:\mingw64`，理由写在 `release-client.yml` 里。`crates/server` 不依赖 `remote`，musl 静态编译不受影响。

**硬件编解码对这条数据面不适用，别重提**：码流是 VP9 profile 1（8-bit 4:4:4），而**四家厂商全无 VP9 4:4:4 硬件解码**（NVIDIA 支持矩阵的 VP9 列只有 4:2:0/8/10/12-bit，**NVENC 连 VP9 编码器都没有**）；Mac 侧连 H.264/HEVC 的 4:4:4 编码 profile 都不暴露（唯一能出的 4:4:4 是 ProRes 4444，Windows 解不了），NVDEC 也不解 H.264 4:4:4。唯一全硬件的 4:4:4 组合是 Windows+NVIDIA ↔ Windows+NVIDIA（HEVC RExt），不是本项目的部署形态。**2026-10-13 已用一手来源（厂商官方矩阵、SDK 头文件、Intel media-driver 源码、FFmpeg 源码）证否并改道软件解码并行化（补丁 7）**；`hwcodec` 也不可用（只在 RustDesk 的 AGPL 仓库里，crates.io 上 404）——将来若真要走硬件，每个后端都得自己写，**不得引入任何 AGPL/GPL 代码**。

**`rd_id` 由服务端分配并持久化**到 `clients.remote_id`（9 位数字，满足 `MIN_PEER_ID_LEN=6`）。同一 NAT 下多个客户端共用一个公网 IP，所以不能由客户端自选；用户全程看不到也不需要配置 ID。

**会话鉴权不依赖 hbbs**（hbbs 按设计不做身份认证，ID 可被抢占）：服务端签发 `session_id` + 一次性 `token`（TTL 60s），经加密控制通道同时下发给控制器与被控端；被控端在 P2P 握手时校验。**没有 token 的第三方即使抢注了 ID 也建不起会话。**

**接入确认复用既有 `RemoteSessionResponse` 通道，不新增控制消息变体**：服务端本来就无条件把被控端的响应转达给控制器（`handle_session_response` → `notify_controller_response`），所以两端只需各自「延迟应答 / 等待应答」。控制器**必须**等到 `accepted:true` 才 `establish`——若照旧在被授权时立刻建立，用户还在看弹窗时它的中继请求就已经发出去，会撞上 hbbr 的 30s 配对窗口，表现就是「卡在连接中」。`Confirm` 下被控端**不登记 token**，所以即使控制器硬闯也过不了握手鉴权：**拒绝是引擎强制的，弹窗只是表面**。开关语义因此是「允许被请求」而非「永久放行」。

被控端 UI 面（`crates/client-gui`）：确认弹窗由 `ui/remote.rs::approval_modal` 在 `GuiApp::update` 里每帧渲染、**常驻所有 Tab 之上**（与 `controlled_banner` 同理，安全提示不能只在「远程」Tab 可见），并按模式给出不同的风险说明（「完全控制 / 仅观看 / 仅文件传输」三者的后果不同）；策略选择器与「已信任的对端」列表在「远程」Tab。策略经 `GuiPrefs.remote_approval` 持久化（`#[serde(default)]`，缺省 `Confirm`，旧配置文件也落到「每次询问」）；**信任列表只存内存**，退出应用即清空（`RemoteUiState.trusted_peers`，按客户端名索引——`RemoteSessionIncoming` 不带 `from_rd_id`，改名即失效）。请求首次出现时会强制显示并聚焦窗口（用户可能已把窗口隐藏到托盘），托盘 tooltip 同步改为「有远程接入请求待确认」。

**数据面三条路径**：**P2P 打洞**（跨 NAT，经 hbbs 交换公网 UDP 映射后对打）→ **P2P 直连**（同内网，`FetchLocalAddr`/`LocalAddr`）→ **hbbr 中继兜底**（所有 NAT 场景可用，`always_use_relay` 可强制）。规则两侧**对称**，故两端对路径的判断一致；`always_use_relay` **两侧都要遵守**（S3 之前被控端分支忽略它，规则不对称、它也就失去了 kill switch 的意义）。

**打洞为什么现在成立**：早先 `forward_punch_result` 回给请求方的是目标端信令 TCP 连接的**源地址**（临时端口，目标端并未在那里监听），所以「拿到地址就直连」在跨 NAT 下必然失败；现在回的是**目标端已登记的 UDP 映射**（`peers.lookup(&phs.id).socket_addr`，由 `RegisterPeer`/`RegisterPk` 维护），而请求方自报的 UDP 端口经 `PunchHole.udp_port`（tag 4）转给目标端。**UDP 打洞不需要端口复用**——从已注册的套接字向对端发一个数据报就建立了 NAT 映射，这正是它比 TCP 同时打开简单的地方。**hbbs 的三处改动必须保持纯加法**：① `PunchHole.socket_addr` **一个字都不能动**——`forward_punch_result` 靠解码目标端回显的 `PunchHoleSent.socket_addr` 去 `sinks` 里找请求方那条 TCP 连接，而 `sinks` 按**信令 TCP 源地址**索引，改了它上游客户端的应答路由就断了（`push_to_connection` 找不到 sink 会静默丢弃）；② `udp_port == 0` 时逐字节退回旧行为；③ NAT 探测端口的 UDP 半边**只应答 `TestNatRequest`**，**不复用 `handle_datagram`**——否则会把「面向 NAT 端口的映射」写进 `peers`，污染打洞用的映射语义。

**打洞的两条安全不变量**（配机械锁）：只接受 `magic` 正确且 `conn_id` **命中已登记会话**的数据报，其余**静默丢弃、绝不回复**（反反射）；`Established` 时锁存对端源地址，之后只认该地址（等价于「NAT 重绑后 TCP 断掉」，与中继路径同构，不是新增弱点）。**被控端必须回观测源**而不是回 hbbs 报的地址——hbbs 报的是「对端面向 hbbs 的映射」，对称 NAT 下与「对端面向我的映射」不同。

**直连/打洞探测必须用短超时**（`signaling.rs` 的 `DIRECT_SIGNAL_TIMEOUT = 3s`，**不是** `RELAY_WAIT`）：hbbs 只对 `same_intranet` 的 `PunchHoleRequest` 回包，跨 NAT 时它只推一个 `PunchHole` 数据报、**永不回响应**。若沿用 25s，控制器会空等 25s 才去发中继信令，而被控端的 `wait_relay_push(RELAY_WAIT)` 几乎同时超时 → 会话必现「卡在连接中」。**打洞预算的硬不等式**：控制器最坏耗时 = `DIRECT_SIGNAL_TIMEOUT(3s) + PUNCH_BUDGET(3s) = 6s`，必须 **≪ 被控端 `RELAY_WAIT(25s)`**，否则会出现「控制器还在打洞、被控端已放弃转中继」的错位（配断言测试）。`signaling::tests::try_direct_gives_up_quickly_when_hbbs_never_answers` 用假静默 hbbs 锁定这一点。

**`udp_demux_task` 是 UDP 套接字唯一的 `recv_from` 读者**，所以它写错会连心跳一起弄坏、客户端被判离线。分派链固定为「可靠层数据报（`peek_conn_id` 命中）→ NAT 探测应答 → 既有 protobuf 路径」，且 **protobuf 路径必须逐字节不变、排在最后**（配 demux 分派测试）。

**数据面必须预登记**：hbbs 对**未登记**的对端是静默丢弃的（`RequestRelay` 只打 debug 就丢、不排队不重试；`PunchHoleRequest` 立刻回 `IdNotExist`，于是控制器一个 RTT 内就转去中继信令）。所以 `TransportEstablisher::prepare` 在收到 `RemoteCapability`、以及打开「允许被远程控制」开关时被调用，两端在会话发起前就已在线。**别把它当成可选优化**——被控端若等到接受 `RemoteSessionIncoming` 才登记，控制器早已发完请求，会话**必现**超时（`tests/remote_session.rs` 的 C 部分用例锁定这一点）。注意 `set_enabled` 由 GUI 的 UI 线程调用，**不在 Tokio 运行时内**，所以预登记走构造时捕获的 `Handle` 而不是 `tokio::spawn`。

**中继约束**：hbbr 按 uuid 配对，先发**恰好一个**带帧的 `RequestRelay` 然后转裸字节、**没有 ack**；`pair_window` 与 `idle_timeout` 都是 30s（任何字节都会重置 idle，每 3s 检查一次）。因此会话必须靠应用层心跳保活（`HEARTBEAT_INTERVAL` 低于 30s），**静止画面也不例外**。配对超时不会有任何错误通知，控制器必须自己按窗口超时提示，否则用户看到的是「卡在连接中」。

#### Windows 无人值守被控端（SYSTEM 服务 + 无头实例）

**要解决的两个问题**：控制 Windows 客户端时，**锁屏/登录界面**点不动，**任务管理器**这类提权窗口也点不动。前者根因三层且缺一不可（Winlogon 桌面 ACL 不放行 Administrators + UIPI + 会话隔离），**「以管理员身份运行」无效**；后者**只是 UIPI**——任务管理器在**默认桌面**上，只是完整性为 high，而 GUI 是 medium。唯一正路都是 SYSTEM 服务把进程拉进控制台会话。完整架构/部署/排障见根目录 `WINDOWS-SECURE-DESKTOP.md`。

**两个产物、两条工具链**：服务 exe 走 MSVC（不牵 libvpx），「实例」就是 GUI exe 的 `--instance` 无头模式（走 GNU leg）。`win-platform` **不得**依赖 `remote`，否则 libvpx 会被拖进服务 exe，而 MSVC 没有预编译库。

**身份归属是硬需求，且被动不能是静默的**：服务端按**客户端名**唯一，同名注册会互相踢（用户看到的是「远程控制每隔几秒掉一次」）。所以服务在跑时本机 GUI **不注册**自己的控制连接、转为被动：`service_watch.rs` 每 3s 查一次 SCM，`IdentityOwner` 走 `watch` 通道送到 UI。**`Pending`（启动中与停止中）也算服务在承载**——启动中它马上要注册，停止中它的实例可能还活着（`kill_instance` 最多等 10 秒）。`Unknown`（SCM 查询失败）**保持原归属**，是刻意的迟滞。被动时状态页横幅、托盘 tooltip、托盘首行都改说「服务接管」——托盘**不能**再说「已连接/离线」，那两句都是假的（本窗口压根没注册）。

**联锁是两层的，服务侧那道才是真闸门**：`--install` **有意不启动服务**，而实例读的是 `%ProgramData%\rust-tunnel\host.toml`（含服务端**全局 token**——谁拿到它就能以这台机器的身份注册，只能由服务端 Web 界面给，必须手工写）。若在 `host.toml` 不存在时启动服务，GUI 立刻转被动、而服务实例又读不到配置注册不了，**整机彻底失去客户端身份**。两层是：① **GUI 侧**（`service_ui::start_blocked_reason`）只在**确定**缺失（`HostConfigPresence::Absent`）时禁用启动按钮并写明原因，作用仅是替用户省一次白跑的 UAC——那个目录的 DACL 只放行 SYSTEM + Administrators，未提权的窗口连 `stat` 都进不去，**读不到 ≠ 不存在**，故 `Unknown` **放行**（曾用 `Path::is_file()`，它把 `ACCESS_DENIED` 折叠成 `false`，等于对**几乎每个正常 GUI** 永久禁用启动按钮、还配一句错的解释）；② **服务侧**（`win-service::supervise::refuse_reason`）在监督循环**每轮开头**（`wait_for_console_session` **之前**）查 `host.toml` 与实例 exe，缺任一个就 `error!` 记明原因并 `service::request_self_stop()`——**开机那条路径绕过了 GUI**（AUTO_START 的服务比 LogonUI 先起，那时用户还没打开过面板），少了这道闸门，机器会停在「服务 `Running`、却没有任何实例在注册」而面板谎称「服务正在承载客户端身份」。自停是**正常停止**（`SERVICE_STOPPED` + 退出码 0），故不触发安装时写下的 `SC_ACTION_RESTART` 失败动作，GUI 的 3s 轮询立刻拿回身份。`request_self_stop()` 只置标志、**不走 SCM**：`stop()` 在 `START_PENDING` 会被 1061 拒掉，而一个「发现自己根本无法提供服务」的服务必须能无条件干净地停下。`--start` 相应**按结果判定**（等 `Running` → 稳 1.5s 复看，自停就报错并指向 `service.log`），因为 `StartServiceW` **不等**服务进入运行态就返回。

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
- 产物归档下载：`GET /api/client-downloads`、`GET /api/wiki-downloads`（列表，需 JWT）；`GET /api/client-downloads/:version/:file`、`GET /api/wiki-downloads/:version/:file`（下载，公开路由自带 `?token=` 校验，因 `<a download>` 不能带 Header）。分别只读 `client_dist_dir` / `wiki_dist_dir`，由 `release-client.yml` / `release-wiki-client.yml` 落盘
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
  `windows-tests`（`windows-latest`，跑 `cargo test -p rust-tunnel-win-platform -p rust-tunnel-win-service`）
  与 `windows-gnu-typecheck`（`ubuntu-latest`，`cargo check`/`clippy -- -D warnings` 交叉到
  `x86_64-pc-windows-gnu`，覆盖 `client-gui`——**出厂 Windows GUI 正是这个 target**）。
  之所以需要它：`release-client.yml` 那条测试命令只在**打 tag 时**跑，回归要到发布那一刻才暴露；
  而 GNU leg 在打 tag 之前**没有任何东西**做过类型检查。用 `-p` 指定 crate，**绝不用 `--workspace`**
  （会拖进第三方 qdrant-edge）。**不覆盖**：`client-gui` 的宿主目标单测（Linux 上链接 eframe 需要
  一整套 X11/Wayland/GL 开发库，未经验证的红色 job 比如实记录缺口更糟），以及**全部运行时行为**
  ——check/clippy 不链接，Windows 安全桌面特性的验收必须在真机上做
- `release-server.yml`（手动触发）— 单 job：Node 构建前端 → `x86_64-unknown-linux-musl` 静态编译
  （`--features rag,embed-frontend`）→ 从 `contrib/config.toml.template` 渲染配置（占位符替换后校验）
  → SCP 二进制 + systemd 服务 + 配置 → SSH 重启服务
- `release-client.yml`（tag `v*` / 手动）— 四平台矩阵构建客户端二进制，各 runner 直接 SCP 到
  `${DEPLOY_PATH}/client/<tag>/`，`finalize-client` 在服务端生成 `SHA256SUMS` 并更新 `latest` 软链。
  Windows/MSVC leg 额外构建并上传 `rust-tunnel-client-service-windows-x86_64.exe`
  （无人值守服务；**不依赖 libvpx** 故走 MSVC 即可，不需要 GNU leg 那套 MSYS2 曲折；
  资产名必须以 `rust-tunnel-client-` 开头，`finalize-client` 的 glob 与下载页的
  `platform_from_filename` 才认得它），并在同一 leg 跑
  `cargo test -p rust-tunnel-win-platform -p rust-tunnel-win-service`
  ——那批单测是 `#[cfg(windows)]`（`probe`/`service`/`process`/`host`），
  **这个 runner 是仓库里唯一能真正跑它们的机器**，本机 macOS 只能做类型检查；
  但这条只在打 tag 时跑，推送阶段的同一批测试由 `ci.yml` 的 `windows-tests` 兜住
- `release-wiki-client.yml`（tag `wiki-v*` / 手动）— wiki 桌面端（Tauri 2）安装包：macOS
  aarch64/x86_64 `.dmg` + Windows `.msi`/NSIS `.exe`，产物重命名为
  `wiki-desktop-<os>-<arch>[-setup].<ext>` 后 SCP 到 `${DEPLOY_PATH}/wiki/<version>/`（版本目录名 =
  tag 去掉 `wiki-` 前缀），`finalize-wiki` 生成校验和与 `latest` 软链。触发用独立 tag 前缀与
  `release-client.yml` 解耦（Tauri 打包耗时远高于单文件二进制）

部署使用 systemd（`contrib/rust-tunnel-server.service`），配置模板 `contrib/config.toml.template`。
两个归档目录经 `client_dist_dir` / `wiki_dist_dir` 交给服务端只读，Web「下载」页分两个分区展示。

### Wiki Desktop 额外说明

- `crates/wiki-serve` 含 Codex agent 侧车（`agent/{mod,bridge,jsonrpc,process,config,resolve}`，`bridge` 为唯一 `#[cfg(feature=\"tauri\")]` 文件，`resolve` 解析链 sidecar→override→PATH + 熔断计数）。
- `wiki-desktop-ui/src/lib/agent/types/` 为 vendor 官方 TS 类型（pin `rust-v0.153.4`，脚本 `wiki-desktop-ui/scripts/vendor-codex-types.sh` 为单一事实来源，与 workflow `env.CODEX_VERSION` 同步）。
