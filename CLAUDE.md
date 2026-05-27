# CLAUDE.md

本文件为 Claude Code (claude.ai/code) 在本仓库中工作时提供指导。

## 项目概述

rust-tunnel 是一个基于 Rust 的客户端-服务器内网穿透工具，配有 React/TypeScript 前端管理界面。服务器运行在公网，暴露端口将流量通过加密控制通道转发到内网客户端。同时内置 Shadowsocks 和 Trojan 代理服务器和实时连接质量监控。

## 架构

### 后端 (Rust)
- **可执行文件**:
  - `src/bin/server.rs` - 服务器入口；运行控制平面和 API/前端服务
  - `src/bin/client.rs` - 客户端入口；连接服务器并管理本地转发
- **核心模块**:
  - `src/common/` - 共享协议和错误处理
    - `protocol.rs` - 定义 `ControlMessage` 和消息序列化（长度前缀 bincode）
      - `Register` 包含可选的 `hostname` 和 `auth_token` 字段
      - `Ping`/`Pong` 包含序列号和微秒时间戳，用于 RTT 测量
      - 最大消息大小：1MB
    - `error.rs` - `TunnelError`（Io, Serialization, Database, Protocol, ConnectionClosed, Timeout, Config, ControlChannel, Tls）和 `TunnelResult`
    - `tls.rs` - 控制通道 TLS 加密工具
      - 自签名证书生成（ECDSA P-256，有效期 1 年）
      - 加载或生成证书持久化（PEM 文件）
      - 服务器 TLS 配置（`rustls`）
      - 客户端 TLS 配置：不安全/TOFU 模式和安全模式（系统根证书）
      - `connect_tls_insecure` 和 `connect_tls_secure` 辅助函数
    - `logging.rs` - 日志初始化
  - `src/server/` - 服务器实现
    - `control.rs` - 管理控制连接和客户端注册；包含 `ServerState`
      - 支持单个控制连接上的多端口注册
      - 处理断开连接时的客户端清理
      - 追踪每个端口的活跃连接数
      - 通过心跳 RTT/丢包追踪进行质量监控
      - Shadowsocks 端口追踪
      - Trojan 端口追踪
    - `listener.rs` - 监听暴露端口，通知客户端新连接
    - `proxy.rs` - 处理每连接代理流量和流量统计
    - `api.rs` - Axum Web API 和 `TrafficStore` 指标
      - 通过 `rust-embed` 嵌入前端资源
      - 质量监控 API 端点
      - Shadowsocks 管理 API 端点
      - Trojan 管理 API 端点
    - `auth.rs` - Web 界面的 JWT 认证
    - `db.rs` - SQLite 数据库，用于持久化流量、质量和客户端会话数据
    - `config.rs` - 服务器配置，使用 Clap、figment（TOML 配置文件）和环境变量
    - `quality.rs` - 实时连接质量监控
      - RTT、丢包率、吞吐量追踪
      - 质量评分计算（0-100），基于延迟和丢包率
      - 警告/严重阈值告警
      - `QualityStore` 支持数据库持久化
      - `QualityTracker` 追踪每连接的序列号
      - 历史质量采样（内存 60 分钟，数据库 24 小时）
    - `shadowsocks.rs` - 内置 Shadowsocks 代理服务器
      - 使用 `shadowsocks-rust` crate 处理协议
      - 支持 AES-256-GCM 和 ChaCha20-Poly1305 加密
      - 使用 EVP_BytesToKey 进行密钥派生（基于 MD5）
      - 通过 `ProxyServerStream` 处理 SS 握手
      - 连接上下文追踪（`SSConnectionContext`）
    - `trojan.rs` - 内置 Trojan 代理服务器
      - TLS 必需协议：独立的 TLS 监听端口
      - SHA-224 密码哈希认证（56 字符十六进制）
      - Trojan 请求格式：`hex(SHA224(password)) + CRLF + CMD + ATYP + DST.ADDR + DST.PORT + CRLF + payload`
      - 支持 CONNECT（0x01）和 UDP ASSOCIATE（0x03）命令
      - 支持 IPv4（0x01）、域名（0x03）和 IPv6（0x04）地址类型
      - 认证失败时回退到配置的回退地址（防探测）
      - 增量解析处理部分 TLS 读取（`ParseResult`：Complete/Incomplete/Invalid）
      - 双向 TCP 转发（`copy_bidirectional`）
  - `src/client/` - 客户端实现
    - `control.rs` - 建立控制连接并管理本地转发
      - TLS 连接支持（自签名证书的不安全/TOFU 模式）
      - 注册时携带认证令牌
      - 增强心跳，支持 RTT 测量（解析 Pong 响应）
      - `ClientState` 连接管理（添加/删除/投递数据）
    - `proxy.rs` - 连接本地服务并代理流量
    - `config.rs` - 客户端配置，使用 Clap、figment（TOML 配置文件）和环境变量

### 前端 (React + TypeScript + Vite)
- 位于 `frontend/`
- 构建输出由 Rust 服务器从 `frontend-dist/` 提供（已 gitignore）
- 使用 Tailwind CSS、React Query 和 Recharts
- 主要组件：
  - `Dashboard.tsx` - 主仪表盘，展示指标
  - `ClientList.tsx` - 已连接客户端表格（按主机名分组）
  - `ClientDetail.tsx` - 客户端流量详情弹窗
  - `TrafficChart.tsx` - 实时流量图表
  - `Navbar.tsx` - 顶部导航栏
  - `QualityPage.tsx` - 连接质量监控页面
  - `ShadowsocksPage.tsx` - Shadowsocks 代理管理页面
  - `TrojanPage.tsx` - Trojan 代理管理页面
  - `Login.tsx` - 登录页面

### 数据库 (SQLite)
- **数据表**：
  - `port_traffic` - 每端口聚合流量统计
  - `traffic_buckets` - 分钟级粒度流量数据（保留最近 24 小时）
  - `client_sessions` - 客户端连接/断开历史，含主机名追踪
  - `connection_quality_history` - 历史质量数据（RTT、丢包、吞吐量、评分）
  - `shadowsocks_config` - Shadowsocks 配置持久化（端口、加密方式、密码、启用状态）
  - `trojan_config` - Trojan 配置持久化（端口、密码、回退地址、启用状态）
- **位置**：通过 `--db-path` 配置（默认：`./data/rust-tunnel.db`）
- **索引**：流量和质量表的端口、时间戳索引

### API 端点
- **公开**（无需认证）：
  - `POST /api/login` - 登录
  - `GET /api/health` - 健康检查
- **受保护**（设置密码时需要认证）：
  - `POST /api/logout` - 登出
  - `GET /api/clients` - 列出所有客户端（隧道 + Shadowsocks + Trojan）
  - `DELETE /api/clients/:port` - 断开客户端
  - `GET /api/traffic` - 获取所有流量数据
  - `GET /api/traffic/:port` - 获取指定端口流量
  - `GET /api/metrics` - 服务器指标
  - `GET /api/quality/all` - 所有客户端质量数据
  - `GET /api/quality/:port` - 指定端口质量数据
  - `GET /api/quality/:port/history` - 端口质量历史数据
  - `GET /api/quality/warnings` - 当前质量告警
  - `GET /api/shadowsocks` - Shadowsocks 配置
  - `POST /api/shadowsocks` - 更新 Shadowsocks 配置（尚未实现）
  - `GET /api/shadowsocks/stats` - Shadowsocks 流量统计
  - `GET /api/shadowsocks/quality` - Shadowsocks 质量数据
  - `GET /api/trojan` - Trojan 配置
  - `POST /api/trojan` - 更新 Trojan 配置（尚未实现）
  - `GET /api/trojan/stats` - Trojan 流量统计
  - `GET /api/trojan/quality` - Trojan 质量数据

## 常用开发命令

### 后端
```bash
cargo build                    # 调试构建
cargo build --release          # 发布构建
cargo check                    # 快速编译检查
cargo test                     # 运行测试
cargo test -- --nocapture      # 运行测试并显示输出
cargo run --bin rust-tunnel-server -- --bind 0.0.0.0:8080
cargo run --bin rust-tunnel-client -- --server localhost:8080 --forward 9000:localhost:80
```

### 前端
```bash
cd frontend
npm install                    # 安装依赖
npm run dev                    # 开发服务器（热重载）
npm run build                  # 构建到 dist/
npm run lint                   # ESLint 检查
```

### 部署前端
```bash
cd frontend
npm run build
rm -rf ../frontend-dist
cp -r dist ../frontend-dist
```

## 服务器配置

配置优先级（从高到低）：命令行参数 > 环境变量 > 配置文件 > 默认值

### 命令行参数 (Clap)
- `--config <PATH>` - TOML 配置文件路径
- `--bind <ADDR>` - 控制通道监听地址（默认 0.0.0.0:8080）
- `--api-bind <ADDR>` - API/前端监听地址（默认 0.0.0.0:3000）
- `--admin-password <PASSWORD>` - Web UI 管理员密码（可选；启用认证）
- `--jwt-secret <SECRET>` - JWT 签名密钥（未提供时自动生成）
- `--client-auth-token <TOKEN>` - 客户端认证令牌（可选；设置后客户端必须提供）
- `--tls <BOOL>` - 启用控制通道 TLS 加密（默认：true）
- `--tls-cert <PATH>` - TLS 证书文件路径（默认：`./data/tls/cert.pem`）
- `--tls-key <PATH>` - TLS 私钥文件路径（默认：`./data/tls/key.pem`）
- `--db-path <PATH>` - SQLite 数据库路径（默认：`./data/rust-tunnel.db`）
- `--ss-enabled <BOOL>` - 启用 Shadowsocks 代理（默认：false）
- `--ss-port <PORT>` - Shadowsocks 监听端口（启用 SS 时必填）
- `--ss-cipher <CIPHER>` - Shadowsocks 加密方式（aes-256-gcm, chacha20-ietf-poly1305；启用 SS 时必填）
- `--ss-password <PASSWORD>` - Shadowsocks 密码（启用 SS 时必填）
- `--trojan-enabled <BOOL>` - 启用 Trojan 代理（默认：false）
- `--trojan-port <PORT>` - Trojan 监听端口（启用 Trojan 时必填）
- `--trojan-password <PASSWORD>` - Trojan 认证密码（启用 Trojan 时必填）
- `--trojan-fallback <ADDR>` - Trojan 认证失败回退地址（可选；格式 host:port）
- `--log <LEVEL>` - 日志级别（trace/debug/info/warn/error）

### 环境变量
- `CONTROL_ADDR` - 控制通道地址
- `API_BIND` - API 地址
- `ADMIN_PASSWORD` - 管理员密码
- `JWT_SECRET` - JWT 密钥
- `CLIENT_AUTH_TOKEN` - 客户端认证令牌
- `TLS` - 启用 TLS（"true"/"1"）
- `TLS_CERT` - 证书文件路径
- `TLS_KEY` - 私钥文件路径
- `LOG_LEVEL` - 日志级别
- `DB_PATH` - 数据库路径
- `SS_ENABLED` - 启用 Shadowsocks（"true"/"1"）
- `SS_PORT` - Shadowsocks 端口
- `SS_CIPHER` - Shadowsocks 加密方式
- `SS_PASSWORD` - Shadowsocks 密码
- `TROJAN_ENABLED` - 启用 Trojan（"true"/"1"）
- `TROJAN_PORT` - Trojan 端口
- `TROJAN_PASSWORD` - Trojan 认证密码
- `TROJAN_FALLBACK` - Trojan 认证失败回退地址

## 客户端配置

配置优先级（从高到低）：命令行参数 > 环境变量 > 配置文件 > 默认值

### 命令行参数 (Clap)
- `--config <PATH>` - TOML 配置文件路径
- `--server <ADDR>` - 服务器控制地址（如 example.com:8080）
- `--forward <RULE>` - 转发规则（可重复）；格式 `REMOTE_PORT:LOCAL_HOST:LOCAL_PORT`（支持 IPv6）
- `--auth-token <TOKEN>` - 认证令牌（服务器启用客户端认证时必填）
- `--tls <BOOL>` - 启用控制通道 TLS 加密（默认：true）
- `--tls-server-name <NAME>` - TLS SNI 服务器名称（默认：从服务器地址提取主机名）
- `--tls-insecure <BOOL>` - 接受自签名证书 / TOFU 模式（默认：true）
- `--log <LEVEL>` - 日志级别

### 环境变量
- `SERVER_ADDR` - 服务器地址
- `FORWARDS` - 逗号分隔的转发规则
- `AUTH_TOKEN` - 认证令牌
- `TLS` - 启用 TLS
- `TLS_SERVER_NAME` - TLS SNI 名称
- `TLS_INSECURE` - 接受自签名证书
- `LOG_LEVEL` - 日志级别

## 代码模式

- 异步运行时：Tokio（完整功能）
- 错误处理：二进制文件使用 `anyhow`，库错误使用 `thiserror`
- 序列化：控制消息使用 `bincode`，API 使用 `serde_json`
- Web 框架：Axum + 嵌入式前端（`rust-embed`）
- 状态共享：`Arc<Mutex<T>>`、`tokio::sync::Mutex`
- 数据库：sqlx + SQLite
- TLS：rustls + tokio-rustls + rcgen（自签名证书生成）
- 配置：Clap（命令行）+ figment（TOML 配置文件）+ 环境变量
- 质量监控：基于心跳的 RTT 测量，通过序列号追踪丢包

## 近期功能与改进

### Trojan 代理（2026 年 5 月）
- 内置 Trojan 协议代理服务器
- TLS 必需：使用独立的 TLS 监听端口，复用控制通道证书
- SHA-224 密码哈希认证（56 字符十六进制），符合 Trojan 协议标准
- Trojan 请求解析：支持 CONNECT/UDP ASSOCIATE 命令，IPv4/域名/IPv6 地址类型
- 认证失败回退：将流量转发到配置的回退地址（防探测，伪装为 HTTPS 站点）
- 增量解析（`ParseResult`）处理部分 TLS 读取
- 连接追踪和流量统计
- Trojan 连接的质量监控
- 前端管理页面（`TrojanPage.tsx`）
- Trojan 配置的数据库持久化
- API 端点：`/api/trojan`、`/api/trojan/stats`、`/api/trojan/quality`
- 依赖：`sha2`（SHA-224 哈希）、`hex`（十六进制编码）

### TLS 加密（2026 年 5 月）
- 通过 rustls 支持控制通道的完整 TLS 1.3
- 自动生成自签名证书（ECDSA P-256，有效期 1 年）
- 证书持久化：加载现有 PEM 文件或生成新文件
- 客户端 TOFU（首次使用即信任）模式，用于自签名证书
- 安全客户端模式，使用系统根证书验证
- 服务器和客户端默认启用 TLS

### Shadowsocks 代理（2026 年 5 月）
- 使用 shadowsocks-rust crate 的内置 Shadowsocks 服务器
- 支持 AES-256-GCM 和 ChaCha20-Poly1305 加密
- EVP_BytesToKey 密钥派生（基于 MD5，符合 shadowsocks 标准）
- 通过 ProxyServerStream 处理 SS 握手，含加密/解密
- 连接追踪和流量统计
- Shadowsocks 连接的质量监控
- 前端管理页面（`ShadowsocksPage.tsx`）
- Shadowsocks 配置的数据库持久化
- API 端点：`/api/shadowsocks`、`/api/shadowsocks/stats`、`/api/shadowsocks/quality`

### 连接质量监控（2026 年 5 月）
- 实时追踪每端口的 RTT、丢包率和吞吐量
- 质量评分（0-100），基于延迟和丢包率计算
- 警告（RTT >= 200ms 或丢包 >= 5%）和严重（RTT >= 500ms 或丢包 >= 15%）阈值
- `QualityTracker` 通过心跳序列号检测丢包
- 自动检测客户端重启时的序列号重置
- 历史质量采样（内存 60 分钟，数据库最多 24 小时）
- 前端质量监控页面（`QualityPage.tsx`）
- API 端点：`/api/quality/all`、`/api/quality/:port`、`/api/quality/:port/history`、`/api/quality/warnings`

### 配置系统增强（2026 年 5 月）
- 通过 figment 支持 TOML 配置文件
- 服务器和客户端均支持 `--config` 参数
- 所有配置项均支持环境变量
- 三级优先级：命令行 > 环境变量 > 配置文件 > 默认值
- 客户端认证令牌（`--client-auth-token` / `AUTH_TOKEN`）

### 数据库持久化（2026 年 4 月）
- 新增 SQLite 数据库，用于持久化流量统计
- 数据表：`port_traffic`（聚合）、`traffic_buckets`（分钟级）、`client_sessions`（历史）
- 流量数据在服务器重启后保留
- 服务器启动时加载历史数据

### 配置重构（2026 年 4 月）
- 服务器配置移至 `src/server/config.rs`
- 客户端配置移至 `src/client/config.rs`
- 转发规则解析支持 IPv6
- 配置模块完整测试覆盖

### 主机名追踪（2026 年 4 月）
- 客户端向服务器报告主机名
- `ControlMessage::Register` 包含可选主机名字段（向后兼容）
- 前端按主机名分组显示客户端，支持折叠
- 数据库在会话历史中追踪客户端主机名

### 多端口注册（2026 年 4 月）
- 修复只处理第一个端口注册的问题
- 服务器现在处理单个控制连接上的多个 `Register` 消息
- 所有注册端口在断开时被追踪和清理

### 客户端重连
- 服务器在接受新注册前正确清理旧客户端注册
- 断开后重连不会出现"端口已注册"错误
- `ServerState` 在注册同一端口的新客户端前移除旧客户端

### 连接数追踪
- 修复前端所有客户端显示 0 连接的问题
- `ServerState` 现在追踪每个活跃连接所属端口
- API 返回准确的每客户端连接数

### 前端改进
- 新增 `ClientDetail` 弹窗，显示每端口详细流量
- 新增 `QualityPage` 实时质量监控页面
- 新增 `ShadowsocksPage` Shadowsocks 代理管理页面
- 新增 `TrojanPage` Trojan 代理管理页面
- 修复登出重定向从 `/login` 到 `/` 的问题
- 仪表盘现在显示实时指标和流量图表
