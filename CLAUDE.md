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
- `crates/remote`（`rust-tunnel-remote`）— GUI 客户端互相远程控制的**协议 + 引擎**（会话协议/端到端加密/信令/会话循环/瓦片差分/文件传输），**不含任何平台或 UI 依赖**
- 依赖单向：`common ← client`、`common ← server`、`common ← remote ← client-gui`；`rustdesk` **不依赖任何本仓 crate**（只依赖 prost/ed25519-dalek/bytes/tokio）
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
| 21115 | hbbs | TCP | NAT 类型探测（= `rendezvous_port - 1`） |
| 21116 | hbbs | **TCP + UDP** | 注册/心跳/打洞协调（UDP 半边是打洞成功的关键） |
| 21117 | hbbr | TCP | 中继数据面（裸字节转发） |

- 有意未实现：WebSocket 端口 21118/21119（Web 客户端不可用，原生桌面端不受影响）、WebRTC 通道、`KeyExchange`/`kx_version` 前向安全升级
- 部署/防火墙/密钥/真机验证清单见根目录 `RUSTDESK.md`（`docs/` 被 gitignore，不放交付文档）

**`crates/remote/src/`** — GUI 客户端互相远程控制的协议 + 引擎（**零平台/UI 依赖**）
- `session.rs` — 会话帧（4 字节大端长度前缀 + bincode，上限 16 MiB——屏幕帧远大于 `common::protocol` 的 1 MiB）+ `Lane{Control,Screen,File}` 多路复用车道（`File` 预留给文件传输）
- `crypto.rs` — X25519 临时密钥交换 + ChaCha20-Poly1305，双向独立密钥与 nonce 计数器。**一次性 token 进 KDF（别去掉）**：裸 ECDH 挡不住中继方——`Hello`/`HelloAck` 都是明文，它可替换两端临时公钥做完整 MITM；token 经加密控制通道下发且从不上数据面，攻击者不知道它就算不出密钥 → 两端密钥必然不一致 → 首帧 AEAD 认证失败。另拒绝低阶点（`was_contributory`）
- `auth.rs` — 握手 HMAC-SHA256，字段用 **8 字节长度前缀**（`nonce`/公钥是二进制，固定分隔符会有拼接歧义）；`Hello` 的 MAC 覆盖控制器临时公钥，`HelloAck` 的 `hmac_ack` 绑定被控端临时公钥，控制器**真正校验**它；挂起会话 TTL 60s、鉴权成功即消费（重放被拒）、MAC 失败保留以允许重试
- `signaling.rs` — RustDesk 会合/中继客户端（注册/心跳/打洞/中继请求）；`RustDeskTransport` 是真实数据面建立的唯一实现，`LazyTransport` 把它的构造推迟到 `RemoteParams` 到手（GUI 与 e2e 共用）
- `transport.rs` — 数据面传输抽象（`BoxDuplex`/`EstablishRequest`/`TransportEstablisher`），把「建立一条双向字节流」与「会话跑在这条流上」解耦；**建立契约**写在模块文档里（中继路径只发一帧 `RequestRelay` 后转裸字节、不等应答，并把 `early` 与之合并写出）
- `controller.rs`/`controlled.rs` — 两侧会话循环（握手 → 画面/输入/心跳/文件）；控制器把 `Hello` 帧流水线进建立阶段，因此**会话循环不再发 `Hello`，只等 `HelloAck`**。UI 命令统一走 `ControllerCommand`（输入/切显示器/切画质/剪贴板/文件），单通道保证顺序与用户操作一致
- `screen.rs` — `ScreenSource`/`InputSink`/`FrameSink`/`Clipboard` 平台 trait + JPEG 编解码 + 帧哈希去重（静止画面零输出）；`FrameSink` 的 `on_tiles`/`on_clipboard` 默认空实现，控制器侧不必实现 `Clipboard`（它读写的是**本机**剪贴板，由 UI 直接处理）。剪贴板是**单向按需**的：只有控制器会发 `ClipboardRequest`（被控端只应答），控制器侧收到它是异常，只记日志
- `tiles.rs` — 瓦片差分（64×64 网格）：只编码变化的瓦片，静止桌面流量趋近于零。**关键不变量：瓦片永远是原生分辨率，缩放帧只能整帧发**——所以 `scale != 1.0` 一律强制关键帧，否则缩放关键帧与原生瓦片混在一起会错位。`KEYFRAME_INTERVAL_FRAMES=120` / `KEYFRAME_CHANGE_RATIO=0.6` 是重发整帧的两个阈值
- `file.rs` — 被控端共享目录的沙箱文件服务（虚拟 `/` 路径）。**两层校验缺一不可**：`validate_virtual_path` 管语法（拒 `..`/`.`/`\`/盘符/控制字符/空段），`FileService::resolve` 管语义（对**最深的已存在祖先**做 canonicalize 后 `Path::starts_with(canonical_root)`，故符号链接逃逸会被拒）。`list` 里指向根外的符号链接项直接跳过
- `transfer.rs` — 会话内文件传输状态机（两端共用）。只有**控制器**能发起（浏览/下载/上传），被控端只应答。同一时刻只允许一次传输；`CHUNK_SIZE=64 KiB` 且每块之间 `CHUNK_PAUSE=4ms` **主动让出调度**，会话循环的 `select!` 才有机会插入画面帧（中继只给一条字节流，画面与文件天然共享带宽）
- `approval.rs` — 被控端**接入确认**策略与待确认请求（`ApprovalPolicy{Auto,Confirm}`、`PendingApproval`、`APPROVAL_TIMEOUT=45s` / `APPROVAL_WAIT_TIMEOUT=60s`）。`Confirm` 是默认：被控端收到 `RemoteSessionIncoming` 后**不回响应、不登记 token、不派生任务**，只挂起等用户 `approve_pending`/`reject_pending`，到点自动拒绝。**token 绝不进 `PendingApproval`**——那是 UI 状态，凭证只在管理器内部持有。`APPROVAL_TIMEOUT` 必须明显小于服务端 `REMOTE_SESSION_TTL`（60s，见 `crates/server/src/control_plane/remote.rs`）：用户拍板的**整个过程**都要落在那个窗口内，否则响应会被服务端当未知会话丢弃，用户看到的是「卡住」而不是「被拒」
- `manager.rs` — `RemoteManager` 门面（`RemoteUiState` watch、连接请求、启停、平台注入、剪贴板/共享目录注入、接入确认门控、`RemoteBridge` 实现）
- 平台实现（xcap 抓屏 / enigo 注入 / arboard 剪贴板）**放在 `crates/client-gui`**，本 crate 保持编译面干净

#### 远程控制的控制面与数据面

**控制面走既有控制通道**（复用 `ControlMessage` + `ClientEntry.control_sender`），不新开 HTTP 鉴权面——GUI 客户端只有客户端 token，没有管理员 JWT。

**向后兼容靠版本门控，不动 `protocol_version`**：服务端只在客户端版本 ≥ `MIN_REMOTE_CLIENT_VERSION`（`crates/agent/src/runner/version_gate.rs`，当前 `(0,10,0)`）时才下发 `RemoteCapability`；客户端只在收到它之后才发新的上行变体。新客户端 + 老服务端 → 老服务端不下发 → 新客户端全程不发新变体；老客户端 + 新服务端 → 门控拦住。两边都不会因 bincode 未知变体而断连。**新增 S→C 推送时必须过 `client_supports_remote`**，否则老客户端反序列化失败会直接断连。

**`rd_id` 由服务端分配并持久化**到 `clients.remote_id`（9 位数字，满足 `MIN_PEER_ID_LEN=6`）。同一 NAT 下多个客户端共用一个公网 IP，所以不能由客户端自选；用户全程看不到也不需要配置 ID。

**会话鉴权不依赖 hbbs**（hbbs 按设计不做身份认证，ID 可被抢占）：服务端签发 `session_id` + 一次性 `token`（TTL 60s），经加密控制通道同时下发给控制器与被控端；被控端在 P2P 握手时校验。**没有 token 的第三方即使抢注了 ID 也建不起会话。**

**接入确认复用既有 `RemoteSessionResponse` 通道，不新增控制消息变体**：服务端本来就无条件把被控端的响应转达给控制器（`handle_session_response` → `notify_controller_response`），所以两端只需各自「延迟应答 / 等待应答」。控制器**必须**等到 `accepted:true` 才 `establish`——若照旧在被授权时立刻建立，用户还在看弹窗时它的中继请求就已经发出去，会撞上 hbbr 的 30s 配对窗口，表现就是「卡在连接中」。`Confirm` 下被控端**不登记 token**，所以即使控制器硬闯也过不了握手鉴权：**拒绝是引擎强制的，弹窗只是表面**。开关语义因此是「允许被请求」而非「永久放行」。

被控端 UI 面（`crates/client-gui`）：确认弹窗由 `ui/remote.rs::approval_modal` 在 `GuiApp::update` 里每帧渲染、**常驻所有 Tab 之上**（与 `controlled_banner` 同理，安全提示不能只在「远程」Tab 可见），并按模式给出不同的风险说明（「完全控制 / 仅观看 / 仅文件传输」三者的后果不同）；策略选择器与「已信任的对端」列表在「远程」Tab。策略经 `GuiPrefs.remote_approval` 持久化（`#[serde(default)]`，缺省 `Confirm`，旧配置文件也落到「每次询问」）；**信任列表只存内存**，退出应用即清空（`RemoteUiState.trusted_peers`，按客户端名索引——`RemoteSessionIncoming` 不带 `from_rd_id`，改名即失效）。请求首次出现时会强制显示并聚焦窗口（用户可能已把窗口隐藏到托盘），托盘 tooltip 同步改为「有远程接入请求待确认」。

**数据面分两层**：主路径是 hbbr 中继（所有 NAT 场景可用）；同内网直连（`FetchLocalAddr`/`LocalAddr`）是优化。**跨 NAT TCP 同时打开打洞在当前 hbbs 语义下不成立**——`forward_punch_result` 回给请求方的是目标端信令连接的**源地址**（临时端口），目标端并未在该端口监听，所以「拿到地址就直连」在跨 NAT 场景走不通。打洞是阶段 3 的可选项，只能真机验证。

**直连探测必须用短超时**（`signaling.rs` 的 `DIRECT_SIGNAL_TIMEOUT = 2s`，**不是** `RELAY_WAIT`）：hbbs 只对 `same_intranet` 的 `PunchHoleRequest` 回包，跨 NAT 时它只推一个 `PunchHole` 数据报、**永不回响应**。若沿用 25s，控制器会空等 25s 才去发中继信令，而被控端的 `wait_relay_push(RELAY_WAIT)` 几乎同时超时 → 会话必现「卡在连接中」。`signaling::tests::try_direct_gives_up_quickly_when_hbbs_never_answers` 用假静默 hbbs 锁定这一点。

**数据面必须预登记**：hbbs 对**未登记**的对端是静默丢弃的（`RequestRelay` 只打 debug 就丢、不排队不重试；`PunchHoleRequest` 立刻回 `IdNotExist`，于是控制器一个 RTT 内就转去中继信令）。所以 `TransportEstablisher::prepare` 在收到 `RemoteCapability`、以及打开「允许被远程控制」开关时被调用，两端在会话发起前就已在线。**别把它当成可选优化**——被控端若等到接受 `RemoteSessionIncoming` 才登记，控制器早已发完请求，会话**必现**超时（`tests/remote_session.rs` 的 C 部分用例锁定这一点）。注意 `set_enabled` 由 GUI 的 UI 线程调用，**不在 Tokio 运行时内**，所以预登记走构造时捕获的 `Handle` 而不是 `tokio::spawn`。

**中继约束**：hbbr 按 uuid 配对，先发**恰好一个**带帧的 `RequestRelay` 然后转裸字节、**没有 ack**；`pair_window` 与 `idle_timeout` 都是 30s（任何字节都会重置 idle，每 3s 检查一次）。因此会话必须靠应用层心跳保活（`HEARTBEAT_INTERVAL` 低于 30s），**静止画面也不例外**。配对超时不会有任何错误通知，控制器必须自己按窗口超时提示，否则用户看到的是「卡在连接中」。

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

- `release-server.yml`（手动触发）— 单 job：Node 构建前端 → `x86_64-unknown-linux-musl` 静态编译
  （`--features rag,embed-frontend`）→ 从 `contrib/config.toml.template` 渲染配置（占位符替换后校验）
  → SCP 二进制 + systemd 服务 + 配置 → SSH 重启服务
- `release-client.yml`（tag `v*` / 手动）— 四平台矩阵构建客户端二进制，各 runner 直接 SCP 到
  `${DEPLOY_PATH}/client/<tag>/`，`finalize-client` 在服务端生成 `SHA256SUMS` 并更新 `latest` 软链
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
