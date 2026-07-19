# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 项目概述

rust-tunnel 是一个基于 Rust 的客户端-服务器内网穿透工具，配有 React/TypeScript 前端管理界面。服务器运行在公网，暴露端口将流量通过加密控制通道转发到内网客户端。同时内置 Shadowsocks 和 Trojan 代理服务器和实时连接质量监控。

## 常用开发命令

### 后端
```bash
cargo build                    # 调试构建
cargo build --release          # 发布构建
cargo check                    # 快速编译检查
cargo test                     # 运行所有测试
cargo test test_name           # 运行单个测试
cargo test -- --nocapture      # 运行测试并显示输出
cargo test -p rust-tunnel --test '*' module::test_name  # 按模块筛选
cargo clippy                   # Lint（项目配置为 clippy::pedantic）
cargo run --bin rust-tunnel-server -- --bind 0.0.0.0:8080
cargo run --bin rust-tunnel-client -- --server localhost:8080 --password <token> --name home-nas
cargo run --bin checkdb        # SQLite 数据库诊断工具
```

### 前端
#### 注意 node 和 npm 通过nvm安装
```bash
cd frontend
npm install                    # 安装依赖
npm run dev                    # 开发服务器（Vite HMR，/api 代理到 localhost:3000）
npm run build                  # tsc 类型检查 + Vite 构建
npm run lint                   # ESLint 检查
```

### 部署前端到嵌入式资源
```bash
cd frontend && npm run build && rm -rf ../frontend-dist && cp -r dist ../frontend-dist
```

## 架构

### 单 Crate 三二进制
项目是单个 Cargo crate（无 workspace），三个二进制目标：
- `rust-tunnel-server`（`src/bin/server.rs`）
- `rust-tunnel-client`（`src/bin/client.rs`）
- `checkdb`（`src/bin/checkdb.rs`）— 独立的 SQLite 质量历史诊断工具

库入口 `src/lib.rs` 导出三个模块：`client`、`common`、`server`。

### 核心数据流
1. 客户端通过控制通道（加密 TLS）向服务端注册，提供名称和密码
2. 管道路由由服务端 Web 管理的"反向代理规则"定义（Backend `kind=Direct` 直连外部，`kind=Client` 通过客户端内网穿透）
3. `ClientConnector` → `ClientRegistry.open_tunnel` → `ClientTunnelStream` 在控制通道上搭建 `AsyncRead/Write` 流
4. 反向代理的 TCP/HTTP handler 通过 `Connector` trait 统一调用，无需关心 backend 是直连还是 tunnel

### 模块职责

**`src/common/`** — 共享协议和基础设施工具
- `protocol.rs` — `ControlMessage` 枚举，长度前缀 bincode 序列化（最大 1MB）
- `error.rs` — `TunnelError`（10 个变体）和 `TunnelResult`
- `tls.rs` — 自签名证书生成（ECDSA P-256）、TOFU/安全模式客户端配置
- `logging.rs` — 日志初始化

**`src/server/`** — 服务器实现
- `control.rs` — `ServerState`：控制连接接收、`ClientRegistry` 管理、消息分发（OpenTunnel/Data/Close）、心跳质量监控、SS/Trojan 端口追踪
- `client_registry.rs` — 客户端名录（在线/离线）、注册认证、踢人、`open_tunnel` 拨号入口
- `tunnel_stream.rs` — `ClientTunnelStream`：基于控制通道的 `AsyncRead+AsyncWrite`，`write→Data{bytes}` / `read←Data{bytes}` / `drop→Close`
- `api/` — Axum API 路由（`mod.rs` + `clients.rs` + `server_auth.rs`）+ `rust-embed` 嵌入前端 + `TrafficStore` 指标
- `auth.rs` — JWT 认证
- `db.rs` — SQLite（WAL 模式）：流量、质量、会话、clients、server_auth、SS/Trojan 配置、日志持久化
- `config.rs` — Clap + figment（TOML）+ 环境变量，三级优先级
- `quality.rs` — 实时质量监控：RTT/丢包/吞吐量追踪、评分（0-100）、阈值告警、历史采样（内存 60 分钟，数据库 24 小时）
- `reverse_proxy/` — 反向代理子系统，含 `mod.rs`（规则、校验、状态）、`connector.rs`（`Connector` trait + `DirectConnector` + `ClientConnector`）、`handler.rs`（HTTP 请求处理）、`tcp_proxy.rs`（TCP 透明代理）、`upstream.rs`（直连 HTTP 客户端池）、`router.rs`（host/路径路由表）、`shared_listener.rs`（多规则共享端口）、`sni_resolver.rs`（TLS SNI 分发）、`error.rs`（ReconcileError）
- `shadowsocks.rs` — 内置 SS 代理：`shadowsocks-rust` crate，AES-256-GCM / ChaCha20-Poly1305，EVP_BytesToKey 密钥派生
- `trojan.rs` — 内置 Trojan 代理：TLS 必需、SHA-224 认证、增量解析（`ParseResult`）、认证失败回退
- `logs.rs` — 自定义 tracing Layer，捕获日志到内存 + SQLite，API 支持分页/过滤

**`src/client/`** — 客户端实现（零配置范式的端侧）
- `control.rs` — 建立控制连接、TLS、密码认证、`Register{protocol_version:2, name, password, version}`、`ClientState`（pending→active 连接管理）、分发 `OpenTunnel`/`Data`/`Close`/`Disconnect`
- `proxy.rs` — `handle_open_tunnel`：收到 OpenTunnel 后 `TcpStream::connect(target_addr)`，TunnelOpenResult 反馈，双向 shuttle 转发
- `config.rs` — 同上三级配置优先级，但只保留 `server/password/name/tls*` 和 `mesh*`，删除 `forwards`
- `logs.rs` — 客户端日志收集

### 前端架构
- 位于 `frontend/`，构建产物输出到 `frontend-dist/`（gitignored，由 `rust-embed` 嵌入）
- **无路由库** — 通过 `App.tsx` 中的状态条件渲染切换页面
- **React Query v3**（非 TanStack Query v5）— 数据获取和缓存
- **无全局状态管理库** — 状态通过 React Query + 组件本地状态管理
- Vite 开发服务器将 `/api` 代理到 `localhost:3000`（服务器 API 端口）
- 共享组件在 `frontend/src/components/shared/`：`ChartContainer`、`StatCard`、`TimeRangeSelector`、`MobileBottomNav`
- TypeScript 类型定义集中 在 `frontend/src/types/index.ts`
- API 客户端在 `frontend/src/api/client.ts`：Axios + JWT 拦截器

### 数据库 (SQLite)
- 位置：`--db-path` 配置（默认 `./data/rust-tunnel.db`），WAL 模式
- 表：`port_traffic`（聚合流量）、`traffic_buckets`（分钟级，保留 24h）、`client_sessions`（连接历史）、`connection_quality_history`（质量数据）、`shadowsocks_config`、`trojan_config`、`log_entries`、`clients`（客户端名录）、`server_auth`（客户端接入 token）

### API 端点
- 公开：`POST /api/login`、`GET /api/health`
- 受保护（设置密码时需 JWT）：`/api/clients`、`/api/server-auth`、`/api/traffic`、`/api/metrics`、`/api/quality/*`、`/api/shadowsocks/*`、`/api/trojan/*`、`/api/logs/*`、`POST /api/logout`
- 完整列表见 `src/server/api/mod.rs`

## 代码模式

- 异步运行时：Tokio（full features）
- 错误处理：二进制文件用 `anyhow`，库错误用 `thiserror`
- 序列化：控制消息用 `bincode`，API 用 `serde_json`
- Web 框架：Axum + `rust-embed` 嵌入前端
- 状态共享：`Arc<Mutex<T>>`、`tokio::sync::Mutex`
- 数据库：sqlx + SQLite（WAL 模式）
- TLS：rustls + tokio-rustls + rcgen（自签名证书）
- 配置：Clap（CLI）+ figment（TOML + 环境变量），三级优先级：CLI > 环境变量 > 配置文件 > 默认值
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

## 测试

所有测试为单元测试，位于 `src/` 内（`#[cfg(test)] mod tests`）。无 `tests/` 集成测试目录。
协议专属测试在独立文件：`src/server/shadowsocks_test.rs`、`src/server/trojan_test.rs`。

## CI/CD

GitHub Actions 工作流（`.github/workflows/release-server.yml`）：
1. `build-frontend`：Node.js 构建 → 上传 `frontend/dist/`
2. `build-server`：下载前端 → `x86_64-unknown-linux-musl` 静态编译 → strip → 上传二进制
3. `deploy`：SCP 二进制 + systemd 服务 + 配置到远程服务器 → SSH 重启服务

部署使用 systemd（`contrib/rust-tunnel-server.service`），配置模板 `contrib/config.toml.template`。
