# Rust Tunnel

一个基于 Rust 的内网穿透工具，采用客户端-服务器架构。

## 功能特性

- 支持多个端口转发
- 基于 Tokio 的异步 IO
- 心跳检测保持连接
- 简洁的协议设计

## 项目结构

```
.
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── bin/
    │   ├── client.rs    # 客户端入口
    │   └── server.rs    # 服务器入口
    ├── client/          # 客户端实现
    │   ├── config.rs    # 配置解析
    │   ├── control.rs   # 控制连接处理
    │   ├── mod.rs
    │   └── proxy.rs     # 代理连接处理
    ├── server/          # 服务器实现
    │   ├── config.rs    # 配置解析
    │   ├── control.rs   # 控制连接处理
    │   ├── listener.rs  # 本地端口监听
    │   ├── mod.rs
    │   └── proxy.rs     # 代理连接处理
    └── common/          # 公共模块
        ├── error.rs     # 错误定义
        ├── logging.rs   # 日志配置
        ├── mod.rs
        └── protocol.rs  # 通信协议
```

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

## 依赖

- `tokio` - 异步运行时
- `clap` - 命令行参数解析
- `thiserror` - 错误处理
- `tracing` - 日志
- `serde` - 序列化

## License

MIT
