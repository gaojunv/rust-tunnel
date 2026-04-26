# Rust Tunnel

一个基于 Rust 的内网穿透工具，采用客户端-服务器架构，配有 React/TypeScript 前端管理界面。

## 功能特性

- 支持多个端口转发
- 基于 Tokio 的异步 IO
- 心跳检测保持连接
- 简洁的协议设计
- Web 管理界面（支持连接监控、流量统计、客户端管理）
- JWT 身份认证（可选）
- 实时流量图表
- 支持客户端断开后自动重连

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
./rust-tunnel-server --bind 0.0.0.0:8080
```

参数说明：
- `--bind <ADDR>` - 控制连接监听地址 (默认: 0.0.0.0:8080)
- `--api-bind <ADDR>` - API/前端管理界面监听地址 (默认: 0.0.0.0:3000)
- `--admin-password <PASSWORD>` - Web UI 管理员密码（可选，启用认证）
- `--jwt-secret <SECRET>` - JWT 签名密钥（可选，未指定时自动生成）
- `--log <LEVEL>` - 日志级别 (default: info)

### 客户端

在内网主机上运行：

```bash
./rust-tunnel-client --server <SERVER_ADDR>:8080 --forward 8080:localhost:80
```

参数说明：
- `--server <ADDR>` - 服务器地址 (例如: 123.123.123.123:8080)
- `--forward <REMOTE_PORT>:<LOCAL_HOST>:<LOCAL_PORT>` - 端口转发规则
  - 可以指定多个 `--forward` 参数转发多个端口
- `--log <LEVEL>` - 日志级别 (默认: info)

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

## 工作原理

1. 客户端主动与服务器建立控制连接
2. 服务器在指定的远程端口开始监听
3. 当有新连接到达服务器的远程端口时，服务器通过控制连接通知客户端
4. 客户端建立到本地目标地址的连接
5. 两端通过隧道转发数据

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
- 断开客户端连接

## 依赖

### 后端

- `tokio` - 异步运行时
- `clap` - 命令行参数解析
- `thiserror` - 错误处理
- `tracing` - 日志
- `serde` - 序列化
- `axum` - Web 框架
- `jsonwebtoken` - JWT 认证

### 前端

- `React` - UI 框架
- `TypeScript` - 类型安全
- `Vite` - 构建工具
- `React Query` - 数据获取
- `Recharts` - 图表库
- `Tailwind CSS` - 样式框架

## License

MIT
