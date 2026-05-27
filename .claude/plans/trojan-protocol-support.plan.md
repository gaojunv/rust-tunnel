# Plan: 添加 Trojan 协议代理支持

**Complexity**: Large

## Summary

为 rust-tunnel 服务器添加 Trojan 协议代理支持，镜像现有 Shadowsocks 集成模式。Trojan 与 Shadowsocks 的核心区别是：Trojan 不实现自己的加密层，完全依赖外层 TLS 隧道，并通过 SHA-224 密码哈希认证。这意味着 Trojan 监听器需要先完成 TLS 握手，再解析 Trojan 请求头，然后双向转发原始 TCP 数据。同时实现 fallback 机制（将非法流量转发到后端 Web 服务器）以抵抗主动探测。

## Trojan 协议要点

- **认证**: 客户端发送 `hex(SHA-224(password))` (56 字节) + CRLF
- **请求格式**: `[56-byte hash][CRLF][CMD:1B][ATYP:1B][DST.ADDR:var][DST.PORT:2B][CRLF][payload]`
- **CMD**: 0x01=CONNECT(TCP), 0x03=UDP ASSOCIATE
- **ATYP**: 0x01=IPv4(4B), 0x03=Domain(1B len + domain), 0x04=IPv6(16B)
- **无响应头**: 认证成功后直接双向转发原始数据
- **Fallback**: 认证失败时转发到预设后端（如 127.0.0.1:80），模拟真实 Web 服务器
- **安全依赖 TLS**: 协议本身不加密，必须运行在 TLS 之上

## Patterns to Mirror

| Category | Source | Pattern |
|---|---|---|
| Naming | `src/server/shadowsocks.rs` | `SSConnectionContext`, `handle_ss_handshake()`, `parse_cipher_kind()` |
| Naming | `src/server/config.rs:60-73` | `ss_enabled`, `ss_port`, `ss_password` CLI/TOML/Config 字段 |
| Naming | `src/server/control.rs:41-50` | `PortInfo::Shadowsocks { port, cipher, password, enabled, created_at }` |
| Errors | `src/server/shadowsocks.rs:43` | `TunnelError::Protocol(format!("..."))` 用于协议错误 |
| Logging | `src/server/listener.rs:38-54` | `info!` 启动, `debug!` 连接, `warn!` 失败 |
| Data access | `src/server/db.rs:140-155` | 独立 `shadowsocks_config` 表 + CRUD 方法 |
| Proxy | `src/server/proxy.rs:158-209` | `proxy_ss_connection()`: 增减连接计数 → 连接目标 → 双向转发 → 更新质量 |
| Tests | `src/server/shadowsocks_test.rs` | 单元测试在模块内，集成测试在 `shadowsocks_test.rs` |

## Files to Change

| File | Action | Why |
|---|---|---|
| `Cargo.toml` | UPDATE | 添加 `sha2` 依赖用于 SHA-224 哈希 |
| `src/server/trojan.rs` | CREATE | Trojan 协议核心：请求解析、握手处理、连接上下文 |
| `src/server/mod.rs` | UPDATE | 注册 `trojan` 模块 |
| `src/server/config.rs` | UPDATE | 添加 `trojan_enabled/port/password/fallback` 配置字段 |
| `src/server/control.rs` | UPDATE | 添加 `PortInfo::Trojan` 变体、`trojan_active_connections`、注册/查询方法 |
| `src/server/listener.rs` | UPDATE | 添加 `start_trojan_listener()`、`handle_inbound_connection` 增加 Trojan 分支 |
| `src/server/proxy.rs` | UPDATE | 添加 `proxy_trojan_connection()`、`update_trojan_quality()` |
| `src/server/api.rs` | UPDATE | 添加 Trojan API 类型、端点处理器、路由 |
| `src/server/db.rs` | UPDATE | 添加 `trojan_config` 表和 CRUD 方法 |
| `src/bin/server.rs` | UPDATE | 启动 Trojan 监听器 |
| `src/common/error.rs` | UPDATE | 确认 `TunnelError` 变体足够（已有的 `Protocol`/`Tls`/`Auth` 应够用） |
| `frontend/src/types/index.ts` | UPDATE | 添加 `TrojanConfig`/`TrojanStats`/`TrojanQuality` 接口 |
| `frontend/src/api/client.ts` | UPDATE | 添加 Trojan API 调用函数 |
| `frontend/src/components/TrojanPage.tsx` | CREATE | Trojan 管理页面组件 |
| `frontend/src/components/Dashboard.tsx` | UPDATE | 添加 Trojan tab |
| `frontend/src/components/Navbar.tsx` | UPDATE | 添加 Trojan 导航按钮 |
| `config/server.example.toml` | UPDATE | 添加 Trojan 配置示例 |

## Tasks

### Task 1: 添加依赖和协议解析模块 (`src/server/trojan.rs`)
- **Action**: 在 `Cargo.toml` 添加 `sha2 = "0.10"` 依赖；创建 `src/server/trojan.rs` 实现：
  - `TrojanRequest` 结构体（hash, cmd, address, payload）
  - `TrojanAddress` 枚举（IPv4/IPv6/Domain）
  - `TrojanCommand` 枚举（Connect/UdpAssociate）
  - `TrojanConnectionContext` 结构体
  - `parse_trojan_request(buf: &[u8]) -> Result<Option<(TrojanRequest, usize)>, TunnelError>` — 增量解析，返回 `None` 表示数据不足
  - `sha224_hex(password: &str) -> String` — SHA-224 哈希并转 56 字符 hex
  - `verify_password(received_hash: &str, expected_password: &str) -> bool`
  - `handle_trojan_handshake()` — TLS 接受后读取请求头、认证、提取目标地址、返回 (context, TLS stream + 剩余 payload)
- **Mirror**: `shadowsocks.rs` 的 `SSConnectionContext`、`handle_ss_handshake()` 模式
- **Validate**: `cargo check`

### Task 2: 配置系统扩展 (`src/server/config.rs`)
- **Action**: 在 `ServerCli`、`ServerConfigFile`、`ServerConfig` 中添加：
  - `trojan_enabled: Option<bool>` / `bool`
  - `trojan_port: Option<u16>`
  - `trojan_password: Option<String>`
  - `trojan_fallback: Option<String>` — fallback 地址，默认 `127.0.0.1:80`
  - 环境变量: `TROJAN_ENABLED`、`TROJAN_PORT`、`TROJAN_PASSWORD`、`TROJAN_FALLBACK`
  - CLI: `--trojan-enabled`、`--trojan-port`、`--trojan-password`、`--trojan-fallback`
  - 验证逻辑：`trojan_enabled=true` 时 `trojan_port` 和 `trojan_password` 必填
  - Trojan 需要 TLS 证书，验证时确保 TLS 配置可用
- **Mirror**: `ss_enabled/ss_port/ss_cipher/ss_password` 的三级优先级加载模式
- **Validate**: `cargo test -p rust-tunnel -- config`

### Task 3: ServerState 扩展 (`src/server/control.rs`)
- **Action**:
  - `PortType` 枚举添加 `Trojan` 变体
  - `PortInfo` 枚举添加 `Trojan { port, password, fallback, enabled, created_at }` 变体
  - `ServerState` 添加 `trojan_active_connections: Arc<Mutex<HashMap<u16, usize>>>`
  - 添加方法: `register_trojan()`、`get_trojan_ports()`、`is_trojan_port()`、`increment_trojan_connections()`、`decrement_trojan_connections()`
  - 更新 `get_connection_count_for_port()` 以包含 Trojan 连接数
- **Mirror**: `PortInfo::Shadowsocks` 和 `ss_active_connections` 模式
- **Validate**: `cargo test`

### Task 4: 监听器和代理 (`src/server/listener.rs` + `src/server/proxy.rs`)
- **Action**:
  - `listener.rs`:
    - 添加 `start_trojan_listener(state, port, password, fallback, tls_acceptor)` — 注册端口后创建 TLS 监听器
    - `handle_inbound_connection` 添加 `PortInfo::Trojan` 分支：先做 TLS accept，再调用 `handle_trojan_handshake`
  - `proxy.rs`:
    - `proxy_trojan_connection()` — 连接目标、双向转发原始 TCP 数据（不需要加解密层）、更新质量
    - `copy_bidirectional_with_trojan()` — 直接使用 `tokio::io::copy_bidirectional`（因为 Trojan 在 TLS 内，数据是明文转发）
    - `handle_trojan_fallback()` — 将非法连接转发到 fallback 后端
    - `update_trojan_quality()` — 复用与 SS 相同的质量更新模式
- **Mirror**: `start_shadowsocks_listener()`、`proxy_ss_connection()` 模式
- **Validate**: `cargo check`

### Task 5: 数据库持久化 (`src/server/db.rs`)
- **Action**:
  - 添加 `trojan_config` 表: `id, port, password, fallback, enabled, created_at, updated_at`
  - CRUD 方法: `save_trojan_config()`、`load_trojan_configs()`、`load_enabled_trojan_configs()`、`get_trojan_config()`、`delete_trojan_config()`
  - `TrojanConfigRecord` 结构体
- **Mirror**: `shadowsocks_config` 表和 CRUD 模式
- **Validate**: `cargo test`

### Task 6: API 端点 (`src/server/api.rs`)
- **Action**:
  - 类型: `TrojanConfig`、`TrojanStats`、`TrojanQuality`
  - 端点:
    - `GET /api/trojan` — 获取配置
    - `POST /api/trojan` — 更新配置（暂返回 501）
    - `GET /api/trojan/stats` — 流量统计
    - `GET /api/trojan/quality` — 质量数据
  - 注册路由到 `run_api_server()`
  - 更新 `list_clients` 包含 Trojan 客户端（hostname `[Trojan]`）
- **Mirror**: Shadowsocks API 类型和路由模式
- **Validate**: `cargo check`

### Task 7: 服务器启动集成 (`src/bin/server.rs`)
- **Action**: 在 Shadowsocks 启动块后添加类似的 Trojan 启动块：
  - 加载 TLS 证书（复用现有 `load_or_generate_cert`）
  - 创建 `TlsAcceptor`
  - 调用 `start_trojan_listener(state, port, password, fallback, tls_acceptor)`
- **Mirror**: `if config.ss_enabled { ... }` 启动模式
- **Validate**: `cargo build`

### Task 8: 前端页面
- **Action**:
  - `types/index.ts`: 添加 `TrojanConfig`、`TrojanStats`、`TrojanQuality` 接口
  - `api/client.ts`: 添加 `getTrojanConfig`、`getTrojanStats`、`getTrojanQuality`
  - 创建 `TrojanPage.tsx`: 显示配置状态（启用/端口/密码/fallback）、流量统计、质量数据
  - `Dashboard.tsx`: 添加 `trojan` tab 和条件渲染
  - `Navbar.tsx`: 添加 "Trojan" 按钮
- **Mirror**: `ShadowsocksPage.tsx`、`Dashboard.tsx` tab 集成模式
- **Validate**: `cd frontend && npm run build`

### Task 9: 测试和文档
- **Action**:
  - `src/server/trojan.rs` 内联单元测试: SHA-224 哈希、请求解析、密码验证
  - `src/server/config.rs` 测试: Trojan 配置验证
  - 更新 `CLAUDE.md`: Trojan 协议文档
  - 更新 `config/server.example.toml`
- **Validate**: `cargo test && cd frontend && npm run build`

## Validation
```bash
cargo check
cargo test
cargo build --release
cd frontend && npm run build
```

## Risks

| Risk | Likelihood | Mitigation |
|---|---|---|
| TLS 监听器与控制通道 TLS 冲突 | Low | Trojan 使用独立的 `TlsAcceptor` 和独立端口，与控制通道完全隔离 |
| Trojan 请求解析的增量读取（数据不完整） | Medium | 实现 `parse_trojan_request` 返回 `None` 表示需要更多数据，循环读取直到完整 |
| Fallback 机制增加复杂度 | Medium | 初期实现基础 fallback（转发到 HTTP 后端），后续可增强为连接池 |
| UDP ASSOCIATE 支持 | Low | 第一版仅实现 TCP CONNECT，UDP 作为后续迭代 |
| `sha2` crate 与现有 `sha1`/`hmac` 版本兼容 | Low | `sha2` 0.10 与现有 `sha1` 0.10 同属 RustCrypto 生态，版本兼容 |
| Trojan 与现有 TLS 证书共享 | Medium | 复用 `load_or_generate_cert` 函数，但建议生产环境使用真实 CA 证书 |

## Acceptance
- [ ] Trojan 监听器可通过 TLS 接受连接并验证密码
- [ ] 认证成功后正确连接目标地址并双向转发数据
- [ ] 认证失败时转发到 fallback 后端
- [ ] Trojan 端口在 API 和前端中正确显示配置、统计和质量数据
- [ ] 配置系统支持 CLI/TOML/环境变量三级优先级
- [ ] Trojan 配置持久化到数据库
- [ ] 所有测试通过
- [ ] 前端构建成功
