# AGENTS.md

本文件为 AI 编码代理提供 rust-tunnel 项目的完整指南。阅读本文件前不需要任何项目背景知识。

## 项目概述

rust-tunnel 是一个基于 Rust 的**内网穿透工具**，采用客户端-服务器架构：服务器部署在公网，客户端运行在内网，客户端主动与服务器建立（默认 TLS 加密的）控制连接，服务器通过控制通道按需让客户端打开到本地服务的隧道，并提供反向代理、Shadowsocks/Trojan 代理、Mesh 组网、嵌入式 DNS 等能力。项目配有 React/TypeScript 的 Web 管理界面（可嵌入服务器二进制中）。

主要能力：

- **控制通道**：v2 协议，客户端以 `name` + `password` 注册到 `ClientRegistry`，支持心跳、RTT 测量、断线自动重连。
- **反向代理**：HTTP/TCP/UDP 规则，支持 SNI 解析、TLS 终止、轮询/加权负载均衡，后端可以是直连地址或内网客户端（通过控制通道隧道）。
- **Shadowsocks 代理**：AES-256-GCM / ChaCha20-Poly1305。
- **Trojan 代理**：TLS 必需、SHA-224 认证、认证失败回退；支持独立监听或与反向代理共享端口（SNI 分流）。
- **统一统计系统（StatsCollector）**：`client` / `proxy` / `shadowsocks` / `trojan` 四类实体的流量、速率、连接数，分钟级快照持久化到 `stats_snapshots` 表，API 查询/汇总/SSE 实时推送。
- **Mesh 组网**：STUN（RFC 5389）打洞、P2P 直连与中继、服务注册。
- **嵌入式权威 DNS 服务器**：`*.tunnel.local` / `*.mesh.local` 域解析。
- **ACME 证书管理**：自动签发、续期，支持 HTTP-01 与 DNS-01 挑战（Cloudflare/Aliyun/Tencent/Custom）。
- **JWT 管理认证、客户端认证令牌、SQLite 持久化**。

> **重要变更（2026-07-19 之后）**：旧版的 `--forward` 端口转发已被移除，客户端注册后需要在 Web 管理界面的 **反向代理规则** 页创建 `backend = client` 的规则来暴露内网服务。客户端 CLI 改为 `--server`、`--password`、`--name`。

## 技术栈

**后端（Rust，单 crate，无 workspace）**

- 异步运行时 Tokio（full features）；错误处理：二进制用 `anyhow`，库用 `thiserror`（`TunnelError`）。
- 控制消息：`bincode` 序列化 + 4 字节大端长度前缀帧（最大 1MB）。
- API：`serde_json` + Axum 0.7 + `tower-http`（CORS、fs），SSE 日志/统计流。
- Web 框架：Axum 0.7；`axum-extra` 提供支持重复查询参数的 `Query` 提取器。
- TLS：rustls 0.22 + tokio-rustls + rcgen（自动生成 Ed25519 自签名证书）。
- 数据库：sqlx 0.7 + SQLite（WAL 模式）。
- 配置：clap 4（CLI）+ figment（TOML + 环境变量）。
- 反向代理：hyper 1 + hyper-util + hyper-rustls + reqwest。
- 其他：shadowsocks crate、hickory-proto（DNS）、instant-acme（ACME）、jsonwebtoken、chrono、uuid。

**前端（`frontend/`）**

- React 18 + TypeScript + Vite 5。
- TanStack Query **v5**（`@tanstack/react-query`）做数据获取与缓存。
- react-router-dom 6（`createBrowserRouter`，入口 `frontend/src/App.tsx`）。
- Tailwind CSS + Radix UI / shadcn 风格组件（`frontend/src/components/ui/`）。
- Recharts 图表、Axios（JWT 拦截器，见 `frontend/src/api/client.ts`）。
- 测试：Vitest + Testing Library（现有 `frontend/src/pages/DashboardPage.test.tsx` 等）。

## 构建与常用命令

### 后端

```bash
cargo build                    # 调试构建
cargo build --release          # 发布构建
cargo check                    # 快速编译检查
cargo test                     # 全部测试（单元 + 集成）
cargo test --tests             # 仅集成测试
cargo clippy --tests -- -D warnings   # 与 CI 一致的 Lint（另有若干 -A 豁免，见 .github/workflows/ci.yml）
cargo fmt --all                # 格式化（CI 检查 --check）

# 运行服务器（示例）
cargo run --bin rust-tunnel-server -- --bind 0.0.0.0:8080 --api-bind 0.0.0.0:3000

# 运行客户端（示例）
cargo run --bin rust-tunnel-client -- --server localhost:8080 --password <token> --name my-client

# SQLite 诊断
cargo run --bin checkdb
```

Cargo feature：`embed-frontend` — 通过 `rust-embed` 将 `frontend-dist/` 嵌入服务器二进制。不带该 feature 构建时静态页面路由不生效（见 `src/server/mgmt/api/mod.rs` 中的 `#[cfg(feature = "embed-frontend")]`）。

### 前端

node/npm 通过 nvm 安装。

```bash
cd frontend
npm install
npm run dev        # Vite 开发服务器，/api 代理到 localhost:3000
npm run build      # tsc 类型检查 + Vite 构建（输出 frontend/dist/）
npm run lint       # ESLint，--max-warnings 0（任何警告都会失败）
npm run test       # vitest run
```

构建产物需复制到仓库根的 `frontend-dist/`（gitignored，供 `embed-frontend` 嵌入）：

```bash
cd frontend && npm run build && rm -rf ../frontend-dist && cp -r dist ../frontend-dist
```

## 仓库结构与模块划分

### 二进制（`src/bin/`，Cargo 自动发现，共 3 个）

- `rust-tunnel-server`（`src/bin/server.rs`）— 服务器端入口。
- `rust-tunnel-client`（`src/bin/client.rs`）— 客户端入口，支持断线指数退避重连。
- `checkdb`（`src/bin/checkdb.rs`）— 独立的 SQLite `stats_snapshots` 诊断工具。

库入口 `src/lib.rs` 仅导出三个模块：`client`、`common`、`server`。集成测试直接调用库层的 `run_server`（`src/server/control_plane/server.rs`）和 `run_client`（`src/client/control.rs`）在同进程内起服务。

### `src/common/` — 共享协议与基础设施

- `protocol.rs` — `ControlMessage` 枚举，v2 协议，bincode + 长度前缀。
- `error.rs` — `TunnelError` / `TunnelResult`。
- `tls.rs` — 自签名证书生成、客户端 TOFU/安全模式配置、服务端 TLS 配置构造。
- `stun.rs` — STUN Binding 请求/响应编解码（RFC 5389）。
- `mesh_types.rs` — `MeshRoute`、`MeshService`、`DnsRecord` 等共享类型。
- `logging.rs` — tracing 初始化。

### `src/server/` — 服务器实现

顶层 `mod.rs` 重新导出：`api`（即 `mgmt::api`）、`control`（即 `control_plane`）、`db`（即 `persistence::db`）、`dns`（即 `net::dns`）、`listener`（即 `net::listener`）、`mesh`（即 `net::mesh`）、`acme`（即 `pki::acme`）、`reverse_proxy`（即 `protocols::reverse_proxy`）、`shadowsocks`（即 `protocols::shadowsocks`）、`trojan`（即 `protocols::trojan`）、`trojan_runtime`（即 `protocols::trojan_runtime`）。

- `control_plane/` — 控制平面
  - `server.rs` — `run_server` 入口，监听控制连接，TLS 处理，消息分发。
  - `state.rs` — `ServerState`：端口、连接数、统计、数据库、日志、Mesh、DNS、反向代理、ACME、动态配置等共享状态。
  - `client_registry.rs` — `ClientRegistry`：v2 协议的客户端注册、认证、踢出、`open_tunnel` 发起。
  - `tunnel_stream.rs` — `ClientTunnelStream`，反向代理通过客户端隧道收发数据。
  - `port_info.rs` — `PortInfo`（Shadowsocks/Trojan）、`TrojanRuntimeStatus`。
  - `acme_config.rs` — `AcmeFullConfig`：DB 作为 ACME 配置运行时的唯一来源。
- `net/` — 网络监听与 Mesh/DNS
  - `listener.rs` — Shadowsocks/Trojan 监听器的启动/停止/每连接处理。
  - `mesh/` — Mesh 组网（`router.rs` 路由表、`relay.rs` 中继、`stun.rs`）。
  - `dns/` — 嵌入式权威 DNS 服务器（`registry.rs` 记录注册表、`zone.rs` 区域）。
- `mgmt/` — 管理、API、统计、日志、认证、动态配置
  - `api/mod.rs` — Axum REST API + SSE 日志流 + 嵌入式前端路由构建。
  - `auth.rs` — JWT 认证中间件与 `AuthConfig`。
  - `stats.rs` — 统一统计采集器 `StatsCollector`。
  - `logs.rs` — 自定义 tracing Layer，日志写入内存 + SQLite，API 分页/过滤。
  - `dynamic_config.rs` — DB 支持的运行时动态配置（SS/Trojan/反代/DNS）。
- `persistence/` — 持久化
  - `db/mod.rs` — `Database` 封装与 `Pool<Sqlite>`。
  - `db/schema.rs` — 表结构初始化与迁移（WAL、列迁移、旧表清理）。
  - `db/{clients,logs,settings,shadowsocks,trojan,acme,mesh,proxy_rules,reverse_proxy,records,stats,server_auth,dns,llm}.rs` — 各领域 DB 操作。
- `llm/` — LLM 网关（设计见 `docs/superpowers/specs/2026-07-22-llm-gateway-design.md`）
  - `mod.rs` — 类型定义与 `LlmState`（DB 引用、Gateway 配置、字段加密器）。
  - `auth.rs` — 网关对外 API Key 生成（sk- 前缀）与 SHA-256 哈希校验。
  - `crypto.rs` — 提供商 API Key 的 AES-256-GCM 落库加密；主密钥存于 DB 同目录 `llm_master.key`（0600），密文带 `enc:v1:` 前缀，无前缀按历史明文兼容。
  - `provider.rs` — provider_type 校验与各类型默认 Base URL。
  - `router.rs` — 请求路由：model 名/别名解析 → 提供商（解密 API Key）。
  - `openai_handler.rs` — `/v1/chat/completions`、`/v1/models`。
  - `anthropic_handler.rs` — `/v1/messages`（请求/响应格式互转）。
  - `upstream.rs` — 上游 HTTP 调用与 SSE 透传、OpenAI 格式错误响应。
  - `format.rs` — OpenAI ↔ Anthropic 格式互转与流式 SSE 翻译器。
- `protocols/` — 协议实现
  - `shadowsocks.rs` — Shadowsocks 服务器握手与连接代理。
  - `trojan.rs` — Trojan 握手、认证、代理、回退。
  - `trojan_runtime.rs` — Trojan 统一启动/模式管理（ACME 证书解析、共享/独立模式判定、证书热更新）。
  - `reverse_proxy/` — 反向代理实现（`state.rs`、`rules.rs`、`handler/`、`upstream.rs`、`tcp_proxy.rs`、`sni_sniff.rs`、`sni_resolver.rs`、`shared_listener.rs`、`router.rs`、`connector.rs`、`config.rs`、`error.rs`）。
- `pki/` — 公钥基础设施与证书
  - `acme/` — ACME 证书管理（`manager.rs`、`client/`、`storage.rs`、`provider.rs`、`challenge.rs`、`dns/`）。
- `config/` — 服务端配置加载
  - `cli.rs` — `ServerCli`（clap）。
  - `file.rs` — `ServerConfigFile`（TOML）。
  - `merge.rs` — 配置合并与校验。
  - `mod.rs` — `ServerConfig` 结构体与默认值。

### `src/client/` — 客户端实现

- `control.rs` — 控制连接、TLS、认证注册、心跳 RTT 测量、消息分发；`run_client` 入口。
- `proxy.rs` — 处理 `OpenTunnel`，连接本地目标并转发数据。
- `config.rs` — 客户端配置加载（CLI/TOML/环境变量）。
- `logs.rs` — 客户端日志捕获与通过控制通道转发。

### 前端结构（`frontend/src/`）

- `pages/` — 页面组件（Dashboard、Clients、ClientDetail、Mesh、Dns、Shadowsocks、Trojan、ReverseProxy、Acme、Logs、Settings、Login）。
- `components/` — `ui/`（shadcn 基础组件）、`layout/`（AppLayout、Sidebar、MobileNav）、`shared/`（ChartContainer、StatCard、TimeRangeSelector、QualityBadge、ThemeToggle）及功能组件。
- `api/` — `client.ts`（Axios + JWT 拦截器）、`hooks.ts`（React Query hooks）。
- `types/index.ts` — 集中管理的 TypeScript 类型。
- 路径别名 `@` → `./src`（见 `vite.config.ts`）。

## 配置

### 配置示例与参考

- `config/server.example.toml` — 服务器完整参考配置（含环境变量映射表与命令行参数）。
- `config/client.example.toml` — 客户端参考配置。
- `contrib/config.toml.template` — 部署时由 GitHub Actions 替换敏感占位符的模板。
- `.env.example` — 环境变量示例。

配置优先级（高 → 低）：**命令行参数 > 环境变量 > TOML 配置文件 > 默认值**。

### 服务器关键配置项

- `control_addr` — 控制通道监听地址（默认 `0.0.0.0:8080`）。
- `api_addr` — API/Web 监听地址（默认 `0.0.0.0:3000`）。
- `admin_password` — Web 管理密码（可选，设置后启用 JWT 认证）。
- `jwt_secret` — JWT 签名密钥（未设置时自动生成，重启后旧 token 失效）。
- `client_auth_token` / `--password` — 客户端认证令牌；首次启动时若未设置会从 DB 读取或随机生成，并打印在日志中。
- `tls` / `tls_cert` / `tls_key` — 控制通道 TLS。
- `api_tls` / `api_domain` — API 服务器 TLS（需要 ACME 证书）。
- `db_path` — SQLite 数据库路径（默认 `./data/rust-tunnel.db`）。
- `ss_*` / `trojan_*` — Shadowsocks / Trojan 配置。
- `dns_*` — 嵌入式 DNS。
- `reverse_proxy_*` — 反向代理全局配置。
- `acme_*` — ACME 配置（DB 为运行时真相来源，见 `src/server/control_plane/acme_config.rs`）。

### 客户端关键配置项

- `server` — 服务器地址。
- `password` — 认证令牌（旧 `--auth-token` 已改为 `--password`）。
- `name` — 客户端名（默认系统主机名）。
- `tls` / `tls_server_name` / `tls_insecure` — TLS 设置。
- `mesh` / `mesh_name` / `mesh_services` — Mesh 组网。
- `log` — 日志级别。

## 测试

### 单元测试

位于 `src/` 各文件内（`#[cfg(test)] mod tests`）。例如 `src/common/protocol.rs`、`src/server/control_plane/state.rs`、`src/server/mgmt/stats.rs`、`src/server/persistence/db/mod.rs` 等。

### 集成测试（`tests/`，规范详见 `tests/README.md`）

内进程端到端测试：每个用例通过 `tokio::spawn` 启动 `run_server` + `run_client`，绑定 `127.0.0.1:0` 随机端口，独立 tempdir。

```bash
cargo test --tests                    # 全部集成测试
cargo test --test tunnel_basic        # 指定文件
```

测试文件：`tunnel_basic.rs`（双向转发、多端口）、`tunnel_reconnect.rs`（断开重连、心跳）、`api_auth.rs`（登录/Bearer 校验）、`api_sse.rs`（SSE 日志流、统计流）、`config_persist.rs`（SS/Trojan 配置修改后同库重启持久化）、`stats_client.rs`、`stats_proxy.rs`、`stats_ss_trojan.rs`、`trojan_domain.rs`、`llm_gateway.rs`（LLM 网关：管理 API CRUD、加密落库、OpenAI/Anthropic 端到端 mock 上游、同端口共存、配置恢复）。共享工具在 `tests/common/`（`TestHarness`、带 JWT 的 reqwest 封装、echo 服务、`wait_until` 指数退避）。

**集成测试硬性规矩**（来自 `tests/README.md`，写新用例必须遵守）：

1. **禁止 `tokio::time::sleep`**，一律用 `common::wait_until("desc", || async { ... })`。
2. 每个 test 必须用 `tokio::time::timeout(15s, ...)` 包裹，防止 hang 阻塞 CI。
3. 不要在测试中修改产品代码来迁就断言；行为不符时先 `curl` 核对真实响应。
4. 每个 test 使用独立端口和独立 tempdir，保证可并行。
5. 对 API 响应字段的断言，先用 `curl | jq` 校对真实字段名。

### 前端测试

`npm run test`（Vitest），现有 `frontend/src/pages/DashboardPage.test.tsx` 等。

## API 概览

完整路由定义见 `src/server/mgmt/api/mod.rs`（Router 构建在 `run_api_server` 中）。

- 公开（无认证）：`POST /api/login`、`GET /api/health`、`GET /api/stats/stream`（SSE）、`GET /api/logs/stream`（SSE，通过 `?token=` 认证）。
- 受保护（`admin_password` 设置时需 JWT Bearer）：
  - `/api/logout`
  - `/api/stats/query`、`/api/stats/summary`
  - `/api/clients`（列表）、`/api/clients/:name`（备注/删除）、`/api/clients/:name/kick`
  - `/api/server-auth`（获取/设置）、`/api/server-auth/rotate`
  - `/api/shadowsocks`（获取/更新配置）
  - `/api/trojan`（获取/更新配置）
  - `/api/mesh*`（Mesh 网络与服务）
  - `/api/dns/*`（DNS 记录与配置）
  - `/api/logs*`（日志分页、级别）
  - `/api/proxy/rules*`（反向代理规则与全局配置）
  - `/api/acme/*`（ACME 状态、配置、证书、DNS 提供商、挑战状态）
  - `/api/settings*`（通用设置、反代配置、DNS 配置）
  - `/api/llm/*`（LLM 网关：Gateway 配置、providers/models/api-keys CRUD）
- 静态前端：`/*path` → `serve_static`（仅在 `embed-frontend` feature 启用时）。

## 代码风格与约定

- 格式化：rustfmt 默认配置，CI 强制 `cargo fmt --all -- --check`。
- Lint：CI 运行 `cargo clippy --tests -- -D warnings`，并对 `src/` 中若干既有警告做了 `-A` 豁免（`collapsible_match`、`map_flatten`、`new_without_default`、`needless_borrows_for_generic_args`、`needless_borrow`、`io_other_error`、`bool_assert_comparison`、`zombie_processes`，见 `.github/workflows/ci.yml`）。**新代码不应新增任何 clippy 警告。**
- 状态共享：`Arc<Mutex<T>>` / `tokio::sync::Mutex` / `tokio::sync::RwLock`；控制消息分发用 `tokio::sync::mpsc`。
- 错误处理：库代码返回 `TunnelResult<T>`（thiserror），二进制入口用 `anyhow::Result`。
- 日志：统一用 `tracing`（不用 `println!`）；服务器端日志经自定义 `LogLayer` 持久化到 SQLite；客户端日志通过 `LogBatch` 控制消息转发到服务端。
- 注释与文档主要使用中文，保持与所在文件一致。
- 前端：ESLint `--max-warnings 0`；提交前跑 `npm run lint` 和 `npm run build`（含 tsc 类型检查）。

## 安全注意事项

- 控制通道 **TLS 默认开启**；服务器无证书时自动生成自签名证书（Ed25519，1 年有效期）存于 `./data/tls/`。客户端默认 TOFU（`tls_insecure = true`）自动接受自签名证书，生产环境应使用受信证书并关闭 `tls_insecure`。
- `admin_password`、`jwt_secret`、`password`、SS/Trojan 密码均为敏感配置。**不要把真实凭据提交进仓库**（`server.toml`、`client.toml`、`.env`、`data/` 已 gitignore；提交配置只改 `config/*.example.toml`）。
- 未设置 `jwt_secret` 时服务器会自动生成，重启后旧 token 全部失效——这是预期行为。
- 客户端认证令牌（`password`）保存在 `server_auth` 表，首次启动时若未提供则随机生成并在日志中打印；提供 CLI 值时仅在表为空才会写入。
- 密钥派生、认证等加密逻辑（SS 的 EVP_BytesToKey、Trojan 的 SHA-224）修改需格外谨慎，有独立测试覆盖。
- 注意区分**控制端口**（`--bind`）与**反向代理/远程暴露端口**（在 Web UI 或 `/api/proxy/rules` 中配置），两者必须不同。

## CI/CD 与部署

GitHub Actions（`.github/workflows/`）：

- `ci.yml` — 每次 push / PR：`cargo fmt --check` → `cargo clippy --tests -D warnings`（含豁免）→ `cargo build --tests` → `cargo test --tests -- --test-threads=4`。
- `release-server.yml` — 打 `v*` tag 或手动触发：Node 20 构建前端 → 下载产物到 `frontend-dist/` → `x86_64-unknown-linux-musl` 静态编译服务器（带 `embed-frontend`）→ strip → SCP 二进制 + systemd 单元 + 配置到远程服务器 → SSH 重启。
- `release-client.yml` — 打 tag 时交叉编译客户端：Linux musl、macOS x86_64/aarch64、Windows MSVC，上传为 Release 构件。

部署使用 systemd，单元文件 `contrib/rust-tunnel-server.service`（工作目录 `/opt/rust-tunnel`，配置 `/etc/rust-tunnel/config.toml`，模板见 `contrib/config.toml.template`）。

## 注意事项

- `frontend-dist/` 是构建产物目录，不要手动编辑其中的文件。
- `data/` 中的 SQLite 数据库、TLS 证书、WAL 文件均为运行时数据，已 gitignore。
- `CLAUDE.md` 与本文件并存。
- 设计文档位于 `docs/superpowers/specs/`，可按日期/主题查阅 ACME、反向代理、统计统一、客户端路由重构等设计背景。
