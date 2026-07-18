# AGENTS.md

本文件为 AI 编码代理提供 rust-tunnel 项目的完整指南。阅读本文件前不需要任何项目背景知识。

## 项目概述

rust-tunnel 是一个基于 Rust 的**内网穿透工具**，采用客户端-服务器架构：服务器部署在公网，客户端运行在内网，客户端主动与服务器建立（默认 TLS 加密的）控制连接，服务器在暴露端口接收公网流量并通过控制通道转发给内网客户端，再由客户端转发到本地服务。

项目配有 React/TypeScript 的 Web 管理界面（嵌入服务器二进制中），并提供：

- 多端口转发、心跳保活、断线自动重连
- 实时连接质量监控（RTT、丢包率、质量评分 0-100、阈值告警）
- 内置 Shadowsocks 代理（AES-256-GCM / ChaCha20-Poly1305）
- 内置 Trojan 代理（TLS 必需、SHA-224 认证、认证失败回退）
- Mesh 组网：STUN（RFC 5389）打洞、P2P 直连与中继、服务注册
- 嵌入式权威 DNS 服务器（`tunnel.local` / `mesh.local` 域）
- 反向代理（HTTP/TCP/UDP 规则、SNI 解析、轮询/加权负载均衡）
- ACME 证书自动签发与管理（含 DNS-01 挑战）
- JWT 管理认证、客户端认证令牌、SQLite 持久化

## 技术栈

**后端（Rust，单 crate，无 workspace）：**

- 异步运行时 Tokio（full features）；错误处理：二进制用 `anyhow`，库用 `thiserror`（`TunnelError`）
- 控制消息：`bincode` 序列化 + 长度前缀帧（最大 1MB）；API：`serde_json`
- Web 框架 Axum 0.7 + tower-http（CORS、fs）；SSE 日志流
- TLS：rustls 0.22 + tokio-rustls + rcgen（自动生成 ECDSA P-256 自签名证书）
- 数据库：sqlx 0.7 + SQLite（WAL 模式）
- 配置：clap 4（CLI）+ figment（TOML + 环境变量）
- 其他：shadowsocks crate、hickory-proto（DNS）、instant-acme（ACME）、jsonwebtoken

**前端（`frontend/`）：**

- React 18 + TypeScript + Vite 5
- TanStack Query **v5**（`@tanstack/react-query`）做数据获取与缓存
- react-router-dom 6（`createBrowserRouter`，入口 `frontend/src/App.tsx`）
- Tailwind CSS + Radix UI / shadcn 风格组件（`frontend/src/components/ui/`）
- Recharts 图表、Axios（JWT 拦截器，见 `frontend/src/api/client.ts`）
- 测试：Vitest + Testing Library（目前仅有 theme 相关测试）

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
cargo run --bin rust-tunnel-server -- --bind 0.0.0.0:8080
cargo run --bin rust-tunnel-client -- --server localhost:8080 --forward 9000:localhost:80
cargo run --bin checkdb        # SQLite 质量历史诊断工具
```

Cargo feature：`embed-frontend` — 通过 `rust-embed` 将 `frontend-dist/` 嵌入服务器二进制。不带该 feature 构建时静态页面路由不生效（见 `src/server/api.rs` 中的 `#[cfg(feature = "embed-frontend")]`）。

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

- `rust-tunnel-server`（`src/bin/server.rs`）— 服务器端
- `rust-tunnel-client`（`src/bin/client.rs`）— 客户端
- `checkdb`（`src/bin/checkdb.rs`）— 独立的 SQLite 诊断工具

库入口 `src/lib.rs` 仅导出三个模块：`client`、`common`、`server`。集成测试直接调用库层的 `run_server`（`src/server/control.rs`）和 `run_client`（`src/client/control.rs`）在同进程内起服务。

### `src/common/` — 共享协议与基础设施

- `protocol.rs` — `ControlMessage` 枚举，bincode + 长度前缀
- `error.rs` — `TunnelError` / `TunnelResult`
- `tls.rs` — 自签名证书生成、客户端 TOFU/安全模式配置
- `stun.rs` — STUN Binding 请求/响应编解码（RFC 5389）
- `mesh_types.rs` — `MeshRoute`、`MeshService`、`DnsRecord` 等共享类型
- `logging.rs` — tracing 初始化

### `src/server/` — 服务器实现

- `control.rs` — `ServerState`：客户端注册、多端口管理、心跳质量监控；`run_server` 入口
- `listener.rs` / `proxy.rs` — 暴露端口监听、每连接双向 `tokio::io::copy` 转发
- `api.rs` — Axum REST API + SSE 日志流 + `TrafficStore` + 嵌入式前端
- `auth.rs` — JWT 认证中间件
- `db.rs` — SQLite（WAL）：16 张表，含 `port_traffic`、`traffic_buckets`、`client_sessions`、`connection_quality_history`、`shadowsocks_config`、`trojan_config`、`server_logs`、`mesh_networks`、`mesh_services`、`proxy_rules`、`proxy_traffic`、`acme_certificates`、`acme_challenges`、`reverse_proxy_config`、`dns_config`、`server_settings`
- `config.rs` — clap + figment 三级配置
- `dynamic_config.rs` — DB 支持的运行时动态配置（SS/Trojan/反代/DNS）
- `quality.rs` — RTT/丢包/吞吐量追踪、评分、告警（内存保留 60 分钟，DB 保留 24 小时）
- `logs.rs` — 自定义 tracing Layer，日志写入内存 + SQLite，API 分页/过滤
- `shadowsocks.rs` / `trojan.rs` — 内置代理服务器（配套测试在 `shadowsocks_test.rs`、`trojan_test.rs`）
- `mesh/` — Mesh 组网（`router.rs` 路由表、`relay.rs` 中继、`stun.rs`）
- `dns/` — 嵌入式权威 DNS 服务器（UDP，`registry.rs` 记录注册表、`zone.rs` 区域）
- `reverse_proxy/` — 反向代理（`handler.rs`、`router.rs`、`sni_resolver.rs`、`tcp_proxy.rs`、`upstream.rs`、`shared_listener.rs`）
- `acme/` — ACME 证书管理（`manager.rs`、`client.rs`、`challenge.rs`、`provider.rs`、`storage.rs`、`dns/`）

### `src/client/` — 客户端实现

- `control.rs` — 控制连接、TLS、认证令牌、心跳 RTT 测量；`run_client` 入口
- `proxy.rs` — 连接本地服务并转发流量
- `config.rs` / `logs.rs` — 配置与日志收集（日志经控制通道转发到服务器）

### 前端结构（`frontend/src/`）

- `pages/` — 页面组件（Dashboard、Clients、Quality、Shadowsocks、Trojan、Mesh、Dns、ReverseProxy、Acme、Logs、Settings、Login）
- `components/` — `ui/`（shadcn 基础组件）、`layout/`（AppLayout、Sidebar、MobileNav）、`shared/`（ChartContainer、StatCard、TimeRangeSelector、QualityBadge、ThemeToggle）及功能组件
- `api/` — `client.ts`（Axios + JWT 拦截器）、`hooks.ts`（React Query hooks）
- `types/index.ts` — 集中管理的 TypeScript 类型
- 路径别名 `@` → `./src`（见 `vite.config.ts`）

### 配置示例与参考

- `config/server.example.toml` / `config/client.example.toml` — 带完整注释的参考配置（含环境变量映射表）
- `.env.example` — 环境变量示例
- 配置优先级（高 → 低）：**命令行参数 > 环境变量 > TOML 配置文件 > 默认值**
- 默认路径：TLS 证书 `./data/tls/`、数据库 `./data/rust-tunnel.db`

## 测试

### 单元测试

位于 `src/` 各文件内（`#[cfg(test)] mod tests`）。Shadowsocks/Trojan 协议测试在独立文件 `src/server/shadowsocks_test.rs`、`src/server/trojan_test.rs`，反代 HTTP/2 测试在 `src/server/reverse_proxy/http2_test.rs`。

### 集成测试（`tests/`，规范详见 `tests/README.md`）

内进程端到端测试：每个用例通过 `tokio::spawn` 启动 `run_server` + `run_client`，绑定 `127.0.0.1:0` 随机端口，独立 tempdir。

```bash
cargo test --tests                    # 全部集成测试
cargo test --test tunnel_basic        # 指定文件
```

测试文件：`tunnel_basic.rs`（双向转发、多端口）、`tunnel_reconnect.rs`（断开重连、心跳）、`api_auth.rs`（登录/Bearer 校验）、`api_sse.rs`（SSE 日志流、流量桶）、`config_persist.rs`（SS/Trojan 配置修改后同库重启持久化）。共享工具在 `tests/common/`（`TestHarness`、带 JWT 的 reqwest 封装、echo 服务、`wait_until` 指数退避）。

**集成测试硬性规矩**（来自 `tests/README.md`，写新用例必须遵守）：

1. **禁止 `tokio::time::sleep`**，一律用 `common::wait_until("desc", || async { ... })`。
2. 每个 test 必须用 `tokio::time::timeout(15s, ...)` 包裹，防止 hang 阻塞 CI。
3. 不要在测试中修改产品代码来迁就断言；行为不符时先 `curl` 核对真实响应。
4. 每个 test 使用独立端口和独立 tempdir，保证可并行。
5. 对 API 响应字段的断言，先用 `curl | jq` 校对真实字段名。

### 前端测试

`npm run test`（Vitest），现有用例仅覆盖 `frontend/src/theme/`。

## 代码风格与约定

- 格式化：rustfmt 默认配置，CI 强制 `cargo fmt --all -- --check`。
- Lint：CI 运行 `cargo clippy --tests -- -D warnings`，并对 `src/` 中 8 类既有警告做了 `-A` 豁免（`collapsible_match`、`map_flatten`、`new_without_default`、`needless_borrows_for_generic_args`、`needless_borrow`、`io_other_error`、`bool_assert_comparison`、`zombie_processes`，见 `ci.yml`）。**新代码不应新增任何 clippy 警告。**
- 状态共享：`Arc<Mutex<T>>` / `tokio::sync::Mutex`；控制消息分发用 `tokio::sync::mpsc`。
- 错误处理：库代码返回 `TunnelResult<T>`（thiserror），二进制入口用 `anyhow::Result`。
- 日志：统一用 `tracing`（不用 `println!`）；服务器端日志经自定义 Layer 持久化到 SQLite。
- 注释与文档主要使用中文，保持与所在文件一致。
- 前端：ESLint `--max-warnings 0`；提交前跑 `npm run lint` 和 `npm run build`（含 tsc 类型检查）。

## API 概览

完整路由定义见 `src/server/api.rs`（约 2900 行处的 Router 构建）。

- 公开：`POST /api/login`、`GET /api/health`、`GET /api/logs/stream`（SSE）
- 受保护（设置了 `admin_password` 时需 JWT Bearer）：`/api/clients`、`/api/traffic`、`/api/metrics`、`/api/quality/*`、`/api/shadowsocks/*`、`/api/trojan/*`、`/api/mesh*`、`/api/dns/*`、`/api/logs*`、`/api/proxy/*`（反向代理）、`/api/acme/*`、`/api/settings*`、`POST /api/logout`
- 静态前端：`/*path` → `serve_static`

## 安全注意事项

- 控制通道 **TLS 默认开启**；服务器无证书时自动生成自签名证书（ECDSA P-256，1 年有效期）存于 `./data/tls/`。客户端默认 TOFU（`tls_insecure = true`）自动接受自签名证书，生产环境应使用受信证书并关闭 `tls_insecure`。
- `admin_password`、`jwt_secret`、`client_auth_token`、SS/Trojan 密码均为敏感配置。**不要把真实凭据提交进仓库**（`server.toml`、`client.toml`、`.env`、`data/` 已 gitignore；提交配置只改 `config/*.example.toml`）。
- 未设置 `jwt_secret` 时服务器会自动生成，重启后旧 token 全部失效——这是预期行为。
- 密钥派生、认证等加密逻辑（SS 的 EVP_BytesToKey、Trojan 的 SHA-224）修改需格外谨慎，有独立测试文件覆盖。

## CI/CD 与部署

GitHub Actions（`.github/workflows/`）：

- `ci.yml` — 每次 push / PR：`cargo fmt --check` → `cargo clippy --tests -D warnings`（含豁免）→ `cargo build --tests` → `cargo test --tests -- --test-threads=4`。
- `release-server.yml` — 打 `v*` tag 或手动触发：Node 20 构建前端 → 下载产物到 `frontend-dist/` → `x86_64-unknown-linux-musl` 静态编译服务器 → strip → SCP 二进制 + systemd 单元 + 配置到远程服务器 → SSH 重启。
- `release-client.yml` — 打 tag 时交叉编译客户端：Linux musl、macOS x86_64/aarch64、Windows MSVC，上传为 Release 构件。

部署使用 systemd，单元文件 `contrib/rust-tunnel-server.service`（工作目录 `/opt/rust-tunnel`，配置 `/etc/rust-tunnel/config.toml`，模板见 `contrib/config.toml.template`）。

## 注意事项

- `CLAUDE.md` 与本文件并存，但 `CLAUDE.md` 部分内容已过时（如"无路由库"、"React Query v3"、"无 tests/ 集成测试目录"均不再成立）；以本文件和实际代码为准。
- 注意区分**控制端口**（`--bind`，客户端-服务器通信）与**远程暴露端口**（`--forward` 第一个参数，公网访问），两者必须不同。
- `frontend-dist/` 是构建产物目录，不要手动编辑其中的文件。
