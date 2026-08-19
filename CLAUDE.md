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

### Cargo workspace：三库 crate + 根元包
- 根元包 `rust-tunnel` 仅托管 e2e 测试（`tests/`），无实现代码
- `crates/common`（`rust-tunnel-common`）— 协议/TLS/错误/日志/mesh + `DEFAULT_PTY_PORT`
- `crates/client`（`rust-tunnel-client`）— 客户端 lib + bin
- `crates/server`（`rust-tunnel-server`）— 服务端 lib + bins（server、checkdb）
- 依赖单向：`common ← client`、`common ← server`
- **`rag` 为非默认 feature**（门控 qdrant-edge 与 `/api/llm/kb*`）；完整服务需 `--features rag`，CI 构建用 `--features rag,embed-frontend`

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
- `llm/` — LLM 网关：OpenAI/Anthropic 双协议入口（`openai_handler.rs`/`anthropic_handler.rs`）、provider/model/api-key 管理、用量日志、compat 工具调用改写；`llm/rag/` — RAG 知识库：`extractor`（多格式文本提取：PDF/Word/Excel/PPT→Markdown）、`chunker`（Markdown 分块）、`embedder`（远端 embedding）、`store`（qdrant-edge 向量 shard）、`retriever`（检索+注入）、`ingest`（后台摄入任务）
- `pki/` — 证书与 ACME 自动续签
- `net/` — 网络基建（listener/dns/mesh）
- `agent/` — AI agent 工作台：`runner`（自研 agent 循环/回合，作为未配置 agent_type 的 workspace 的回退运行时路径；ACP 为主路径）、`tools`（工具 schema：shell/read_file/read_file_range/write_file/patch_file/list_dir/search/git_*/code_outline/read_symbol，工具经隧道在内网客户端执行）、`executor`（命令执行）、`approval`（审批矩阵：危险工具挂起等待用户批准，支持"本会话记住"）、`session`/`title`/`compact`（会话管理、自动标题、上下文压缩）、`sse`（WebSocket 事件流）、`spawner`（ACP 路径：经控制通道 negotiate AgentSpawnRequest/AgentLlmProxyStart，在客户端 spawn agent 进程）、`acp_bridge`（ACP 路径：管理 ACP session 生命周期、stdio pump、idle reaper、断线恢复）、`acp_events`（ACP `SessionUpdate` → 现有 WS 帧映射）、`llm_bridge`（AgentLlmProxyRequest → 服务端 LLM 网关转发，服务端注入认证）、`roles`（多角色子代理：`agent_roles` 表定义角色 persona——系统提示词/工具白名单黑名单/模型覆盖/mode=subagent|primary|all/scope 仿 agent_skills；task 工具 `agent` 参数调度子代理角色，主会话可经 session.role_id 或 `@role-name` 前缀切换 primary 角色；工具过滤叠加顺序 = 角色过滤 ∩ plan 模式裁剪，子 agent 强制剔除 task/todo_write；内置角色 general/explore 启动时 seed）。`AgentState` 挂在 `ServerState` 上，含 per-workspace 执行锁（git 状态安全）和 per-session 回合锁（多标签页/重连防并发写库）
- `config/` — 服务器配置（Clap + figment（TOML）+ 环境变量，三级优先级）

**`crates/client/src/`** — 客户端实现（零配置范式的端侧）
- `control.rs` — 建立控制连接、TLS、密码认证、`Register{protocol_version:2, name, password, version}`、`ClientState`（pending→active 连接管理）、分发 `OpenTunnel`/`Data`/`Close`/`Disconnect`
- `proxy.rs` — `handle_open_tunnel`：收到 OpenTunnel 后 `TcpStream::connect(target_addr)`，TunnelOpenResult 反馈，双向 shuttle 转发
- `config.rs` — 同上三级配置优先级，但只保留 `server/password/name/tls*` 和 `mesh*`，删除 `forwards`
- `logs.rs` — 客户端日志收集

### 前端架构
- 位于 `frontend/`，构建产物输出到 `frontend-dist/`（gitignored，由 `rust-embed` 嵌入）
- **react-router-dom v6** — `App.tsx` 中 `createBrowserRouter` + `ProtectedRoute` 守卫路由
- **React Query v5**（`@tanstack/react-query`）— 数据获取和缓存
- **无全局状态管理库** — 状态通过 React Query + 组件本地状态管理
- Vite 开发服务器将 `/api` 代理到 `localhost:3000`（服务器 API 端口）
- 共享组件在 `frontend/src/components/shared/`：`ChartContainer`、`StatCard`、`TimeRangeSelector`、`MobileBottomNav`
- 页面在 `frontend/src/pages/`；LLM 网关管理（`LLMPage`）与 RAG 知识库管理（`KbPage`，含 `components/llm/kb/` 下的 `KbList`/`KbDetail`/`KbDialog`）；AI agent 工作台（`AgentPage`，含 `components/agent/`：会话列表、消息流、审批弹层、@文件引用、workspace 管理——含 ACP 引擎配置：agent_type/agent_path/llm_model_id）
- TypeScript 类型定义集中 在 `frontend/src/types/index.ts`
- API 客户端在 `frontend/src/api/client.ts`：Axios + JWT 拦截器

### 数据库 (SQLite)
- 位置：`--db-path` 配置（默认 `./data/rust-tunnel.db`），WAL 模式
- 表：`port_traffic`（聚合流量）、`traffic_buckets`（分钟级，保留 24h）、`client_sessions`（连接历史）、`connection_quality_history`（质量数据）、`shadowsocks_config`、`trojan_config`、`log_entries`、`clients`（客户端名录）、`server_auth`（客户端接入 token）、`rag_knowledge_bases` / `rag_documents` / `rag_chunks`（RAG 知识库、文档与分块，向量本体存于 `<db_parent>/rag/<kb_id>/`，文档原文存于 `<db_parent>/rag_docs/<kb_id>/`；**向量本体仅随 `rag` feature 编译**）、`agent_workspaces` / `agent_sessions`（含 `role_id`） / `agent_messages` / `agent_roles`（多角色子代理定义）

### API 端点
- 公开：`POST /api/login`、`GET /api/health`、`GET /api/llm/kb/events`（SSE，`?token=` 认证）
- 受保护（设置密码时需 JWT）：`/api/clients`、`/api/server-auth`、`/api/traffic`、`/api/metrics`、`/api/quality/*`、`/api/shadowsocks/*`、`/api/trojan/*`、`/api/logs/*`、`POST /api/logout`
- LLM 网关（既有）：`/api/llm/gateway`、`/api/llm/providers`、`/api/llm/providers/:id`、`/api/llm/providers/:provider_id/models`、`/api/llm/models`、`/api/llm/models/:id`、`/api/llm/api-keys`、`/api/llm/api-keys/:id`、`/api/llm/usage/*`
- RAG 知识库（新）：`/api/llm/kb`（CRUD）、`/api/llm/kb/:id`、`/api/llm/kb/:id/docs`、`/api/llm/kb/:id/docs/:doc_id`（含 `/reindex`）、`/api/llm/kb/test-embedding`、`/api/llm/kb/:id/query`
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

## 测试

- **单元测试**：位于各 crate 的 `src/` 内（`crates/{common,client,server}/src/`），源文件末尾 `#[cfg(test)] mod tests`（协议专属测试如 shadowsocks/trojan/http2/aliyun 均已合并回各自实现文件）。
- **集成测试**：位于根元包 `tests/`，以黑盒 API/协议流程为主（`tunnel_basic.rs`、`api_auth.rs`、`stats_*.rs` 等）。

## CI/CD

GitHub Actions 工作流（`.github/workflows/release-server.yml`）：
1. `build-frontend`：Node.js 构建 → 上传 `frontend/dist/`
2. `build-server`：下载前端 → `x86_64-unknown-linux-musl` 静态编译 → strip → 上传二进制
3. `deploy`：SCP 二进制 + systemd 服务 + 配置到远程服务器 → SSH 重启服务

部署使用 systemd（`contrib/rust-tunnel-server.service`），配置模板 `contrib/config.toml.template`。
