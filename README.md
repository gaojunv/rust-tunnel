# Rust Tunnel

**简体中文** | [English](README_EN.md)

基于 Rust 的客户端-服务器内网穿透与边缘代理平台，配有 React/TypeScript 管理界面。服务器运行在公网，通过加密控制通道将流量转发到内网客户端；同时内置 Shadowsocks / Trojan 代理、反向代理（含直连与隧道两类后端）、嵌入式 DNS / Mesh 服务发现、ACME 自动证书、SQLite 持久化、实时可观测性，以及 LLM 网关（含 RAG 知识库）与 AI Agent 工作台（ACP 为主路径，隧道内工具执行）。

## 功能特性

- **内网穿透**：加密控制通道（TLS 默认开启，自签名 Ed25519）、客户端注册与隧道复用（`ClientConnector → ClientRegistry.open_tunnel → ClientTunnelStream`）
- **反向代理**：规则化路由，`Direct` 直连 / `Client` 隧道两类后端，统一 `Connector` 抽象，支持 TCP/HTTP 与 SNI 分发
- **代理协议**：内置 Shadowsocks（`shadowsocks-rust`，AES-256-GCM / ChaCha20-Poly1305）与 Trojan（TLS 必需，SHA-224 认证，回退伪装）
- **网络基建**：嵌入式 DNS 权威（`*.tunnel.local` / `*.mesh.local`）、Mesh 服务发现、PKI/ACME 自动续签、API TLS
- **可观测性**：心跳 RTT/丢包/吞吐、质量评分 0–100 与告警阈值；流量分钟级分桶（保留 24h）+ 聚合统计；结构化日志（tracing Layer → 内存 + SQLite，分页/过滤/SSE）
- **LLM 网关**：OpenAI / Anthropic / Responses 三协议入口（`POST /v1/chat/completions` 与 `POST /v1/responses`），provider / model / api-key / 用量管理，compat 工具调用改写，上游 `responses` 协议按模型 `extra_config` 声明
- **统一知识容器**：多格式提取（PDF/Word/Excel/PPT→Markdown）→ Markdown 分块 → 远端 embedding → qdrant-edge 向量 + 页面图谱双索引；后台摄入、重建、检索预览与跨源搜索
- **AI Agent 工作台**：WebSocket 回合流，ACP 主路径（`AgentSpawn`/`AgentLlmProxy` 经控制通道在客户端 spawn 进程、stdio pump、idle reaper）+ runner 回退路径；隧道内工具执行（shell/read/write/patch/list/search/git/code_outline/read_symbol/task 等）、审批矩阵、多角色子代理、会话压缩与标题生成
- **管理面**：Axum + `rust-embed` 嵌入前端，JWT 认证，产物归档下载（client / wiki-desktop）
- **桌面端**：`crates/client-gui` 托盘客户端（winit + tray-icon + eframe/egui，四 Tab：连接/日志/设置/关于）
- **Wiki 桌面**：Tauri 2 打包（`wiki-desktop-ui` + `wiki-core`/`wiki-serve`），Markdown + 全文检索 + 图谱

## 架构

### Cargo Workspace

根元包 `rust-tunnel` 仅托管 `tests/` e2e 测试，无实现代码；实现分布在 13 个 crate：

| crate | 说明 |
|---|---|
| `crates/common` | 协议/TLS/错误/日志/mesh + `DEFAULT_PTY_PORT` |
| `crates/client` | 客户端 lib + bin（控制通道、隧道 shuttle、配置） |
| `crates/client-gui` | 桌面托盘客户端（winit/tray-icon/eframe） |
| `crates/server` | 服务端装配（control_plane/protocols/persistence/mgmt/llm/pki/net/agent/config） |
| `crates/agent` | Agent 领域（runner/tools/executor/approval/session/title/compact/sse/spawner/acp_bridge/acp_events/llm_bridge/roles） |
| `crates/llm` | LLM 网关（openai/anthropic/responses 适配、provider/model/key、用量、model_groups） |
| `crates/rag` | RAG 知识库（extractor/chunker/embedder/store/retriever/ingest） |
| `crates/persistence` | SQLite 访问层 |
| `crates/pki` | 证书与 ACME |
| `crates/protocols` | SS/Trojan/反向代理协议实现 |
| `crates/stats` | 统计收集与持久化 |
| `crates/wiki-core` / `wiki-serve` | Wiki 核心与服务 |

依赖单向：`common ← client`、`common ← server`，`rag` 为非默认 feature（门控 qdrant-edge 与向量索引侧）。

### 核心数据流

1. 客户端经 TLS 控制通道向服务端 `Register{protocol_version:2, name, password, version}` 注册
2. 管道路由由 Web 管理的反向代理规则定义（`kind=Direct` 直连外部，`kind=Client` 经隧道到内网）
3. `ClientConnector → ClientRegistry.open_tunnel → ClientTunnelStream` 在控制通道上搭建 `AsyncRead/Write` 流
4. 反向代理的 TCP/HTTP handler 经 `Connector` trait 统一调用，无需关心后端类型

## 安装 Rust

**macOS / Linux:**
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source $HOME/.cargo/env
```
**Windows:** 下载并运行 [rustup-init.exe](https://rustup.rs/)

前端需 Node.js（建议经 nvm 安装）。

## 编译

```bash
cargo build                      # 调试构建（默认不含 qdrant-edge）
cargo build -p rust-tunnel-server --features rag              # 含 RAG 的完整构建
cargo build -p rust-tunnel-server --features rag,embed-frontend  # 含前端嵌入（发布形态）
cargo check
cargo clippy -p rust-tunnel-server
```

前端嵌入产物由 `frontend-dist/`（gitignored）经 `rust-embed` 打入二进制：

```bash
cd frontend && npm install && npm run build
rm -rf ../frontend-dist && cp -r dist ../frontend-dist
```

## 使用方法

### 服务端

```bash
# 基本（TLS 默认开启，自签名证书自动生成）
cargo run -p rust-tunnel-server --features rag -- --bind 0.0.0.0:8080

# 指定配置与端口
./rust-tunnel-server --config /path/to/config.toml --bind 0.0.0.0:8080 --api-bind 0.0.0.0:3000

# 启用 Shadowsocks / Trojan / DNS / 反向代理 / ACME（亦可在 Web 界面或 TOML 中配置）
./rust-tunnel-server --ss-enabled true --ss-port 8388 --ss-cipher aes-256-gcm --ss-password <pwd> \
  --trojan-enabled true --trojan-port 443 --trojan-password <pwd> --trojan-fallback 127.0.0.1:80 \
  --dns-enabled true --dns-bind 0.0.0.0:53
```

常用参数（完整见 `--help`）：

- `--config <PATH>` — TOML 配置文件
- `--bind <ADDR>` — 控制通道监听（默认 `0.0.0.0:8080`）
- `--api-bind <ADDR>` — API/Web 监听（默认 `0.0.0.0:3000`）
- `--admin-password <PWD>` / `--jwt-secret <SECRET>` / `--client-auth-token <TOKEN>`
- `--tls / --tls-cert / --tls-key` — 控制通道 TLS（默认开启，缺省自动生成 `data/tls/*`）
- `--db-path <PATH>` — SQLite 路径（默认 `./data/rust-tunnel.db`，WAL 模式）
- `--client-dist-dir / --wiki-dist-dir` — 归档只读目录（CI 落盘，Web 下载页消费）
- `--ss-enabled / --ss-port / --ss-cipher / --ss-password`
- `--trojan-enabled / --trojan-port / --trojan-password / --trojan-fallback`
- `--dns-enabled / --dns-bind / --dns-tunnel-domain / --dns-mesh-domain`
- `--reverse-proxy-enabled / --reverse-proxy-max-connections / --reverse-proxy-connection-timeout / --reverse-proxy-buffer-size`
- `--api-tls / --api-domain`
- `--acme-enabled / --acme-server-url / --acme-cert-dir / --acme-auto-renew / --acme-renewal-check-interval / --acme-renewal-days-before-expiry / --acme-email / --acme-tos-agreed`
- `--log <LEVEL>` — trace/debug/info/warn/error

> 产物形态以 `contrib/config.toml.template` 为部署模板（占位符在 CI 中渲染后校验，见 [CI/CD](#cicd)）。

### 客户端

客户端已收敛为零配置范式，不再使用 `--forward`；转发由服务端的反向代理规则定义，客户端仅需注册与隧道承载。

```bash
# 基本
cargo run -p rust-tunnel-client -- --server example.com:8080 --password <client_token> --name home-nas

# TLS 控制（默认开启，TOFU 接受自签名）
./rust-tunnel-client --server example.com:8080 --password <token> --tls true --tls-insecure true

# Mesh 与 Agent 执行器
./rust-tunnel-client --server example.com:8080 --password <token> --mesh home --mesh-name nas \
  --mesh-service db:mysql:localhost:3306 --enable-agent --agent-pty-port 45631

# 配置文件
./rust-tunnel-client --config /path/to/client.toml
```

参数：`--server <host:port>`（必填）、`--password <token>`（必填）、`--name <name>`（默认 hostname）、`--tls`/`--tls-server-name`/`--tls-insecure`、`--mesh`/`--mesh-name`/`--mesh-service`（可重复）、`--enable-agent`/`--agent-pty-port`、`--log`、`--config`。

`client.toml` 示例见 [`config/client.example.toml`](config/client.example.toml)。

### 桌面托盘客户端

```bash
cargo run -p rust-tunnel-client-gui
```

原生托盘 + eframe 四 Tab（连接/日志/设置/关于），配置落地于平台标准目录（macOS `~/Library/Application Support` / Windows `%APPDATA%` / Linux `~/.config`），支持 keyring 与自启动。

### Wiki 桌面端

位于 `wiki-desktop-ui`（Tauri 2），产物由 [`release-wiki-client.yml`](.github/workflows/release-wiki-client.yml) 按 `wiki-v*` tag 构建为 macOS `.dmg` 与 Windows `.msi`/`.exe`。

## 配置

三级优先级：**CLI > 环境变量 > TOML 配置文件 > 默认值**。

- 服务端 TOML 参考：[`config/server.example.toml`](config/server.example.toml)（含生产/开发/no-tls 示例段）
- 客户端 TOML 参考：[`config/client.example.toml`](config/client.example.toml)
- 环境变量示例：[`.env.example`](.env.example)
- 部署模板：[`contrib/config.toml.template`](contrib/config.toml.template)（`release-server.yml` 渲染 `${ADMIN_PASSWORD}` / `${CLIENT_AUTH_TOKEN}` / `${CLIENT_DIST_DIR}` / `${WIKI_DIST_DIR}` / `${SS_PASSWORD}` / `${TROJAN_PASSWORD}` 等占位符）

服务端环境变量映射（节选）：`CONTROL_ADDR`/`API_BIND`/`ADMIN_PASSWORD`/`JWT_SECRET`/`CLIENT_AUTH_TOKEN`/`TLS`/`TLS_CERT`/`TLS_KEY`/`LOG_LEVEL`/`DB_PATH`/`CLIENT_DIST_DIR`/`WIKI_DIST_DIR`/`DNS_ENABLED`/`DNS_BIND`/`DNS_TUNNEL_DOMAIN`/`DNS_MESH_DOMAIN`/`TROJAN_ENABLED`/`TROJAN_PORT`/`TROJAN_PASSWORD`/`TROJAN_FALLBACK` 等，完整见 `config/server.example.toml` 头部注释与 `--help`。

客户端环境变量：`SERVER_ADDR`/`PASSWORD`/`NAME`/`TLS`/`TLS_SERVER_NAME`/`TLS_INSECURE`/`MESH_ID`/`MESH_NAME`/`MESH_SERVICES`（逗号分隔）/`LOG_LEVEL`。

数据库（SQLite，WAL）：`--db-path`（默认 `./data/rust-tunnel.db`）。表：`port_traffic`/`traffic_buckets`/`client_sessions`/`connection_quality_history`/`shadowsocks_config`/`trojan_config`/`log_entries`/`clients`/`server_auth`/`knowledge_sources`/`knowledge_docs`/`knowledge_doc_index`/`knowledge_chunks`/`knowledge_pages`/`knowledge_page_edges`/`agent_workspaces`/`agent_sessions`/`agent_messages`/`agent_roles` 等；向量本体位于 `<db_parent>/rag/<source_id>/`，文档原文位于 `<db_parent>/knowledge_docs/<source_id>/`；向量索引仅随 `rag` feature 编译。

## Web 管理界面

启动后访问 `http://<server>:3000`（或 `--api-bind` 指定地址）：

- Dashboard / Mesh / DNS / Clients（含详情与踢下线）/ Shadowsocks / Trojan / 反向代理 / ACME / 日志 / 设置
- LLM 网关（`LLMPage`）与统一知识容器（`KnowledgePage`，向量与页面双索引）
- AI Agent 工作台（`AgentPage`，会话列表/消息流/审批弹层/@文件引用/workspace 管理与 ACP 引擎配置）
- 下载页（客户端二进制与 Wiki 桌面安装包，分区展示，只读归档目录）

技术栈：`react-router-dom v6`（`createBrowserRouter` + `ProtectedRoute`，路由级 lazy）、`@tanstack/react-query v5`、Vite（`/api` 代理到 `localhost:3000`）、`Tailwind CSS`/`Radix UI`/`Recharts`/`CodeMirror 6`/`xterm.js`/`streamdown` 等；共享组件在 `frontend/src/components/shared/`，页面在 `frontend/src/pages/`，类型在 `frontend/src/types/index.ts`，API 客户端在 `frontend/src/api/client.ts`（Axios + JWT 拦截）。

## TLS 与安全

- 控制通道 TLS 默认开启，缺省自动生成 Ed25519 自签名证书（PEM，1 年有效期）；支持 `--tls-cert`/`--tls-key` 指定自定义证书
- 客户端默认 TOFU（`--tls-insecure true`）接受自签名；可经 `--tls-server-name` 指定 SNI
- API TLS 与 ACME（`instant-acme` + `hickory-proto`）支持自动续签与 80 端口重定向
- Web JWT 认证（`--admin-password` 启用时）、客户端接入 Token（`--client-auth-token` / `server_auth` 表）、下载端点的 `?token=` 查询参数校验（因 `<a download>` 无法带 Header）

## 可观测性

- 质量监控：心跳 RTT/丢包/吞吐，评分 0–100，阈值告警（警告 RTT≥200ms/丢包≥5%，严重 RTT≥500ms/丢包≥15%），历史保留内存 60 分钟、DB 24 小时
- 流量：聚合与分钟级分桶，`stats` crate 负责收集与落库，SSE 推送到前端图表
- 日志：自定义 tracing Layer 同时写入内存环与 SQLite，API 支持分页/过滤/SSE

## LLM 网关 / 知识库 / Agent

- **LLM 网关**：`POST /v1/chat/completions` 与 `POST /v1/responses`（后者经 `responses.rs` 双向转换），provider/model/api-key/用量/model-groups（多模型 failover 与熔断），`compat` 工具调用改写
- **知识库**：`extractor`（PDF/Word/Excel/PPT→Markdown，`lopdf`/`zip`/`quick-xml`）→ `chunker` → `embedder`（远端）→ `store`（qdrant-edge shard）→ `retriever`（检索注入）→ `ingest`（后台任务）；SSE 事件流 `GET /api/knowledge/events?token=`
- **Agent**：WebSocket `GET /api/agent/ws`（含 `notifications/ws`、`terminal/ws`），workspace/session/message 持久化，per-workspace 执行锁与 per-session 回合锁，上下文压缩与自动标题，多角色子代理（`agent_roles`，`mode=subagent|primary|all`，工具白/黑名单、模型覆盖、`@role-name` 切换）

## 开发

### 后端

```bash
cargo check
cargo test -p rust-tunnel-common --lib
cargo test -p rust-tunnel-client --lib
cargo test -p rust-tunnel-server --lib            # 无 RAG，快速
cargo test -p rust-tunnel-server --lib --features rag  # 含 RAG
cargo test                                        # 根 e2e（含 rag dev-dep）
cargo test -j 2                                   # 内存受限时（e2e 含 qdrant-edge 编译较重）
cargo clippy -p rust-tunnel-server
cargo run -p rust-tunnel-server --bin checkdb     # SQLite 诊断
```

### 前端

```bash
cd frontend
npm install
npm run dev          # Vite HMR，/api → localhost:3000
npm run build        # tsc + Vite
npm run lint         # ESLint --max-warnings 0
npm test             # Vitest (jsdom)
```

### 构建缓存

```bash
du -sh target
cargo clean -p rust-tunnel-server
cargo clean
```

Lint 基线：`clippy::pedantic = deny`（`doc_markdown = allow`），`unwrap_used`/`expect_used`/`panic`/`unwrap_in_result` = deny，`missing_docs = deny`。

## API 概览

完整路由见 [`crates/server/src/mgmt/api/mod.rs`](crates/server/src/mgmt/api/mod.rs)。

- 公开：`POST /api/login`、`GET /api/health`、`GET /api/knowledge/events`（SSE，`?token=`）、`GET /api/stats/stream`、`GET /api/logs/stream`、`GET /api/agent/ws` 等；下载：`GET /api/client-downloads/:version/:file`、`GET /api/wiki-downloads/:version/:file`（公开，`?token=` 校验）
- 受保护（设置密码时需 JWT）：`POST /api/logout`、`/api/clients`、`/api/server-auth`、`/api/stats/query`、`/api/stats/summary`、`/api/shadowsocks/*`、`/api/trojan/*`、`/api/mesh/*`、`/api/dns/*`、`/api/logs/*`、`/api/proxy/rules`、`/api/acme/*`、`/api/llm/*`、`/api/knowledge/*`、`/api/agent/*`、`/api/preferences`、`/api/settings` 等
- LLM：`/api/llm/gateway`、`/api/llm/providers`、`/api/llm/providers/:provider_id/models`、`/api/llm/models`、`/api/llm/api-keys`、`/api/llm/usage/*`、`/api/llm/model-groups/*`，以及 `POST /v1/responses` 与 `POST /v1/chat/completions`
- 知识容器：`/api/knowledge` CRUD、`/api/knowledge/:id/docs`、`/api/knowledge/:id/query`（检索预览）、`/api/knowledge/:id/pages|graph|search`、`/api/knowledge/search`
- Agent：`/api/agent/workspaces`、`/api/agent/workspaces/:id/files|fs/*|git/*|github/*|sessions`、`/api/agent/sessions/:id`（含 `/model`、`/role`、`/archive`、`/messages`、`/export`）、`/api/agent/roles`、`/api/agent/default-model`

## CI/CD

GitHub Actions（见 [`.github/workflows/`](.github/workflows/)）：

- `release-server.yml`（手动）：前端构建 → `x86_64-unknown-linux-musl` 静态编译（`--features rag,embed-frontend`）→ 从 `contrib/config.toml.template` 渲染配置并校验 → SCP 二进制 + systemd 服务 + 配置 → SSH 重启
- `release-client.yml`（tag `v*` / 手动）：四平台矩阵构建客户端二进制，SCP 到 `${DEPLOY_PATH}/client/<tag>/`，`finalize-client` 生成 `SHA256SUMS` 并更新 `latest` 软链
- `release-wiki-client.yml`（tag `wiki-v*` / 手动）：Tauri 2 安装包（macOS aarch64/x86_64 `.dmg` + Windows `.msi`/NSIS `.exe`），重命名为 `wiki-desktop-<os>-<arch>[-setup].<ext>` 后 SCP 到 `${DEPLOY_PATH}/wiki/<version>/`（目录名 = tag 去 `wiki-` 前缀），`finalize-wiki` 生成校验和与 `latest` 软链

部署为 systemd（[`contrib/rust-tunnel-server.service`](contrib/rust-tunnel-server.service)），两类归档经 `client_dist_dir` / `wiki_dist_dir` 以绝对路径只读暴露给 Web 下载页。

## 依赖

后端（节选，完整见 [`Cargo.toml`](Cargo.toml)）：`tokio`、`axum`/`tower-http`/`hyper`、`sqlx`/`chrono`/`uuid`、`rustls`/`tokio-rustls`/`rcgen`/`webpki-roots`、`shadowsocks`、`qdrant-edge`、`tantivy`/`comrak`/`petgraph`、`portable-pty`、`agent-client-protocol`（`unstable_elicitation`）、`hickory-proto`/`trust-dns-resolver`/`instant-acme` 等；前端见 [`frontend/package.json`](frontend/package.json)。

## License

[MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE) 双许可，任选其一。
