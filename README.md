# Rust Tunnel

一个基于 Rust 的内网穿透工具，采用客户端-服务器架构，配有 React/TypeScript 前端管理界面。支持 TLS 加密、Shadowsocks 代理和实时连接质量监控。

## 功能特性

- 支持多个端口转发
- 基于 Tokio 的异步 IO
- TLS 加密控制通道（默认开启，自动生成自签名证书）
- 内置 Shadowsocks 代理服务器（AES-256-GCM / ChaCha20-Poly1305）
- 实时连接质量监控（RTT、丢包率、质量评分）
- 心跳检测保持连接
- 简洁的协议设计
- Web 管理界面（支持连接监控、流量统计、客户端管理、质量监控、Shadowsocks 管理）
- JWT 身份认证（可选）
- 客户端认证令牌（可选）
- 实时流量图表
- 支持客户端断开后自动重连
- TOML 配置文件支持
- 环境变量配置支持
- SQLite 数据持久化（流量统计、质量历史、Shadowsocks 配置）

## 安装 Rust

如果你还没有安装 Rust，可以通过以下方式安装：

**macOS / Linux:**
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

安装完成后需要重新加载终端环境：
```bash
source $HOME/.cargo/env
```

**Windows:**
下载并运行 [rustup-init.exe](https://rustup.rs/)

## 编译项目

```bash
cargo build --release
```

编译完成后，可执行文件位于 `target/release/` 目录：
- `rust-tunnel-server` - 服务器端
- `rust-tunnel-client` - 客户端

## 使用方法

### 服务器端

在具有公网 IP 的服务器上运行：

```bash
# 基本用法（TLS 默认开启）
./rust-tunnel-server --bind 0.0.0.0:8080

# 禁用 TLS
./rust-tunnel-server --bind 0.0.0.0:8080 --tls false

# 启用 Shadowsocks 代理
./rust-tunnel-server --bind 0.0.0.0:8080 --ss-enabled true --ss-port 8388 --ss-cipher aes-256-gcm --ss-password mypassword

# 使用配置文件
./rust-tunnel-server --config /path/to/config.toml
```

参数说明：
- `--config <PATH>` - TOML 配置文件路径
- `--bind <ADDR>` - 控制连接监听地址 (默认: 0.0.0.0:8080)
- `--api-bind <ADDR>` - API/前端管理界面监听地址 (默认: 0.0.0.0:3000)
- `--admin-password <PASSWORD>` - Web UI 管理员密码（可选，启用认证）
- `--jwt-secret <SECRET>` - JWT 签名密钥（可选，未指定时自动生成）
- `--client-auth-token <TOKEN>` - 客户端认证令牌（可选，设置后客户端必须提供此令牌才能注册）
- `--tls <BOOL>` - 启用 TLS 加密控制通道 (默认: true)
- `--tls-cert <PATH>` - TLS 证书文件路径 (默认: `./data/tls/cert.pem`)
- `--tls-key <PATH>` - TLS 私钥文件路径 (默认: `./data/tls/key.pem`)
- `--db-path <PATH>` - SQLite 数据库路径 (默认: `./data/rust-tunnel.db`)
- `--ss-enabled <BOOL>` - 启用 Shadowsocks 代理 (默认: false)
- `--ss-port <PORT>` - Shadowsocks 监听端口（启用 SS 时必填）
- `--ss-cipher <CIPHER>` - Shadowsocks 加密方式 (aes-256-gcm, chacha20-ietf-poly1305；启用 SS 时必填)
- `--ss-password <PASSWORD>` - Shadowsocks 密码（启用 SS 时必填）
- `--log <LEVEL>` - 日志级别 (trace/debug/info/warn/error)

### 客户端

在内网主机上运行：

```bash
# 基本用法（TLS 默认开启，自动接受自签名证书）
./rust-tunnel-client --server <SERVER_ADDR>:8080 --forward 8080:localhost:80

# 禁用 TLS
./rust-tunnel-client --server <SERVER_ADDR>:8080 --forward 8080:localhost:80 --tls false

# 使用认证令牌
./rust-tunnel-client --server <SERVER_ADDR>:8080 --forward 8080:localhost:80 --auth-token my-token

# 使用配置文件
./rust-tunnel-client --config /path/to/config.toml
```

参数说明：
- `--config <PATH>` - TOML 配置文件路径
- `--server <ADDR>` - 服务器地址 (例如: 123.123.123.123:8080)
- `--forward <REMOTE_PORT>:<LOCAL_HOST>:<LOCAL_PORT>` - 端口转发规则
  - 可以指定多个 `--forward` 参数转发多个端口
  - 支持 IPv6 地址
- `--auth-token <TOKEN>` - 认证令牌（服务器启用客户端认证时必填）
- `--tls <BOOL>` - 启用 TLS 加密 (默认: true)
- `--tls-server-name <NAME>` - TLS SNI 服务器名称 (默认: 从服务器地址提取主机名)
- `--tls-insecure <BOOL>` - 接受自签名证书/TOFU 模式 (默认: true)
- `--log <LEVEL>` - 日志级别 (默认: info)

### 配置文件

服务器和客户端都支持 TOML 配置文件：

**服务器配置示例 (`server.toml`):**
```toml
control_addr = "0.0.0.0:8080"
api_addr = "0.0.0.0:3000"
admin_password = "admin123"
client_auth_token = "client-secret"
tls = true
tls_cert = "./data/tls/cert.pem"
tls_key = "./data/tls/key.pem"
db_path = "./data/rust-tunnel.db"
log = "info"
ss_enabled = true
ss_port = 8388
ss_cipher = "aes-256-gcm"
ss_password = "shadowsocks-password"
```

**客户端配置示例 (`client.toml`):**
```toml
server = "example.com:8080"
forwards = ["8080:localhost:80", "9000:127.0.0.1:3000"]
auth_token = "client-secret"
tls = true
tls_server_name = "tunnel.example.com"
tls_insecure = true
log = "info"
```

配置优先级（从高到低）：命令行参数 > 环境变量 > 配置文件 > 默认值

### 环境变量

所有配置项都可以通过环境变量设置：

**服务器环境变量：** `CONTROL_ADDR`, `API_BIND`, `ADMIN_PASSWORD`, `JWT_SECRET`, `CLIENT_AUTH_TOKEN`, `TLS`, `TLS_CERT`, `TLS_KEY`, `LOG_LEVEL`, `DB_PATH`, `SS_ENABLED`, `SS_PORT`, `SS_CIPHER`, `SS_PASSWORD`

**客户端环境变量：** `SERVER_ADDR`, `FORWARDS`（逗号分隔）, `AUTH_TOKEN`, `TLS`, `TLS_SERVER_NAME`, `TLS_INSECURE`, `LOG_LEVEL`

### 示例

将内网主机的 80 端口暴露到服务器的 8080 端口：

**服务器：**
```bash
./rust-tunnel-server --bind 0.0.0.0:8080
```

**客户端：**
```bash
./rust-tunnel-client --server 你的服务器IP:8080 --forward 8080:localhost:80
```

然后就可以通过 `http://你的服务器IP:8080` 访问到内网主机 80 端口上的服务了。

> **注意**：`--bind` 指定的是**控制端口**（用于客户端和服务器之间的通信），`--forward` 第一个参数是**远程端口**（用于公网用户访问）。这两个端口**必须不同**，因为它们需要分别绑定。例如，不要让控制端口和远程端口都用 8080。

### Shadowsocks 使用

启用 Shadowsocks 后，可以使用任何兼容的 Shadowsocks 客户端连接：

```bash
# 服务器端启用 Shadowsocks
./rust-tunnel-server --bind 0.0.0.0:8080 --ss-enabled true --ss-port 8388 --ss-cipher aes-256-gcm --ss-password mypassword
```

客户端配置：
- 服务器地址：你的服务器IP
- 端口：8388（或你设置的 `--ss-port`）
- 加密方式：aes-256-gcm 或 chacha20-ietf-poly1305
- 密码：你设置的 `--ss-password`

## 工作原理

1. 客户端主动与服务器建立控制连接（支持 TLS 加密）
2. 服务器在指定的远程端口开始监听
3. 当有新连接到达服务器的远程端口时，服务器通过控制连接通知客户端
4. 客户端建立到本地目标地址的连接
5. 两端通过隧道转发数据
6. 心跳机制保持连接并测量 RTT 和丢包率

## TLS 加密

- 默认启用 TLS 加密控制通道
- 服务器自动生成自签名证书（Ed25519，有效期 1 年），保存为 PEM 文件
- 客户端默认使用 TOFU（Trust On First Use）模式，自动接受服务器证书
- 支持自定义证书：通过 `--tls-cert` 和 `--tls-key` 指定

## 连接质量监控

- 实时追踪每个端口的 RTT（往返时间）、丢包率和吞吐量
- 质量评分（0-100），基于延迟和丢包率计算
- 告警阈值：警告（RTT >= 200ms 或丢包 >= 5%）、严重（RTT >= 500ms 或丢包 >= 15%）
- 历史数据保留：内存中 60 分钟，数据库中 24 小时
- 通过 Web 管理界面或 API 查看

## 开发

### 后端开发

```bash
# 检查代码错误
cargo check

# 运行测试
cargo test

# 调试模式运行服务器
cargo run --bin rust-tunnel-server -- --bind 0.0.0.0:8080

# 调试模式运行客户端
cargo run --bin rust-tunnel-client -- --server localhost:8080 --forward 8080:localhost:80
```

### 前端开发

```bash
cd frontend

# 安装依赖
npm install

# 开发模式运行（支持热重载）
npm run dev

# 构建前端
npm run build

# 构建并部署到 frontend-dist
npm run build && rm -rf ../frontend-dist && cp -r dist ../frontend-dist
```

### Web 管理界面

启动服务器后，可以通过浏览器访问 `http://<服务器IP>:3000` 打开管理界面，功能包括：

- 查看已连接的客户端
- 实时连接数统计
- 流量监控图表
- 客户端详情查看
- 连接质量监控（RTT、丢包率、质量评分）
- 质量告警
- Shadowsocks 代理管理
- 断开客户端连接

## API 接口

| 方法 | 路径 | 说明 |
|------|------|------|
| POST | `/api/login` | 登录 |
| GET | `/api/health` | 健康检查 |
| POST | `/api/logout` | 登出 |
| GET | `/api/clients` | 列出所有客户端 |
| DELETE | `/api/clients/:port` | 断开指定客户端 |
| GET | `/api/traffic` | 获取所有流量数据 |
| GET | `/api/traffic/:port` | 获取指定端口流量 |
| GET | `/api/metrics` | 服务器指标 |
| GET | `/api/quality/all` | 所有客户端质量数据 |
| GET | `/api/quality/:port` | 指定端口质量数据 |
| GET | `/api/quality/:port/history` | 质量历史数据 |
| GET | `/api/quality/warnings` | 质量告警 |
| GET | `/api/shadowsocks` | Shadowsocks 配置 |
| GET | `/api/shadowsocks/stats` | Shadowsocks 统计 |
| GET | `/api/shadowsocks/quality` | Shadowsocks 质量 |

## 依赖

### 后端

- `tokio` - 异步运行时
- `clap` - 命令行参数解析
- `figment` / `toml` - 配置文件支持
- `thiserror` / `anyhow` - 错误处理
- `tracing` - 日志
- `serde` / `bincode` - 序列化
- `axum` / `tower-http` - Web 框架
- `rust-embed` - 嵌入前端资源
- `jsonwebtoken` - JWT 认证
- `sqlx` - SQLite 数据库
- `rustls` / `tokio-rustls` / `rcgen` - TLS 加密
- `shadowsocks` - Shadowsocks 代理协议
- `chrono` - 时间处理
- `gethostname` - 获取主机名

### 前端

- `React` - UI 框架
- `TypeScript` - 类型安全
- `Vite` - 构建工具
- `React Query` - 数据获取
- `Recharts` - 图表库
- `Tailwind CSS` - 样式框架

## License

MIT
