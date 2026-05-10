# Shadowsocks 服务集成设计文档

> 文档版本: v1.0  
> 创建日期: 2026-05-10  
> 作者: Claude Code  
> 状态: 待审核

## 一、项目背景

rust-tunnel 是一个 TCP 内网穿透工具，现需要集成 Shadowsocks 代理服务功能，让 rust-tunnel 服务器同时可以作为 Shadowsocks 代理服务端使用。

### 1.1 目标

- 在 rust-tunnel server 中集成 Shadowsocks TCP 代理功能
- SS 流量深度集成到现有的流量统计和质量监控系统
- 保持现有内网穿透功能不受影响
- 支持动态配置和管理 Shadowsocks 服务

### 1.2 非目标（MVP 阶段）

- 不支持 UDP 转发
- 不支持多用户/多端口
- 不支持插件（v2ray-plugin 等）
- 不支持 ACL 访问控制
- 只支持两种加密方式: aes-256-gcm, chacha20-ietf-poly1305

---

## 二、架构设计

### 2.1 设计方案选择

采用 **方案二改良版**: 集成到现有 Listener 架构

将 Shadowsocks 作为一种特殊的 "端口类型"，深度集成到现有的 listener/proxy 架构中。只使用 shadowsocks-rust 的 crypto 低层 API，流量转发和统计逻辑复用现有代码。

**为什么选择方案二：**
- 长期灵活性高，支持个性化开发
- 流量粒度控制精确，可以在每个数据包层面插入逻辑
- 天然集成到 `ServerState`、`TrafficStore`、`QualityTracker`
- 未来扩展能力强（动态路由、多用户、ACL 等）

### 2.2 端口类型扩展

```
端口类型：
├── TUNNEL（原有类型）- 普通的内网穿透端口，流量转发到对应 client
└── SHADOWSOCKS（新增类型）- SS 代理端口，流量解密后直接转发到目标地址
```

### 2.3 模块划分

| 文件 | 类型 | 职责 |
|------|------|------|
| `src/server/shadowsocks.rs` | ✨新增 | Shadowsocks 协议处理：加解密、握手解析、目标地址提取 |
| `src/server/listener.rs` | 修改 | 扩展 `run_listener`，根据端口类型分流到不同的处理函数 |
| `src/server/control.rs` | 修改 | 在 `ServerState` 中增加 SS 端口的注册、查询、管理方法 |
| `src/server/config.rs` | 修改 | 增加 SS 相关配置项 |
| `src/server/api.rs` | 修改 | 增加 SS 管理的 API 端点 |
| `src/server/proxy.rs` | 修改 | 增加 SS 专用的 proxy 函数（解密后转发） |

### 2.4 数据流向对比

**原有 TUNNEL 端口流量：**
```
User → Listener → ControlChannel → Client → LocalService
```

**新增 SHADOWSOCKS 端口流量：**
```
User → Listener → SS Decrypt → Proxy → TargetServer
                                   ↓
                            TrafficStore / QualityTracker
```

---

## 三、核心数据结构设计

### 3.1 端口类型枚举

在 `src/server/control.rs` 中新增：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortType {
    Tunnel,      // 原有的内网穿透端口
    Shadowsocks, // 新增的 Shadowsocks 代理端口
}
```

### 3.2 PortInfo 统一结构

将原来只用于 Tunnel 的 `ClientInfo` 重构为通用的 `PortInfo`：

```rust
#[derive(Debug, Clone)]
pub enum PortInfo {
    Tunnel(ClientInfo),  // Tunnel 端口保留原有信息
    Shadowsocks {        // SS 端口的配置信息
        port: u16,
        cipher: String,  // 加密方式: "aes-256-gcm", "chacha20-ietf-poly1305"
        password: String,
        enabled: bool,
        created_at: i64,
    },
}

// ServerState 中的 clients HashMap 改名为 ports
// 从: HashMap<u16, ClientInfo>
// 改为: HashMap<u16, PortInfo>
```

### 3.3 Shadowsocks 连接上下文

在 `src/server/shadowsocks.rs` 中定义：

```rust
pub struct SSConnectionContext {
    cipher_type: String,
    key: Vec<u8>,
    target_addr: String,  // 解析出的目标地址: "example.com:443" 或 "1.2.3.4:80"
    connection_id: u64,
    port: u16,
}
```

---

## 四、Shadowsocks 协议处理设计

### 4.1 依赖配置

```toml
# Cargo.toml
shadowsocks = { version = "1.24", default-features = false, features = ["aes-gcm", "chacha20-poly1305"] }
```

### 4.2 核心协议处理函数

```rust
// src/server/shadowsocks.rs

/// 处理 SS 握手，解析目标地址并创建解密/加密上下文
pub async fn handle_ss_handshake(
    stream: &mut TcpStream,
    cipher: &str,
    password: &str,
) -> TunnelResult<(SSConnectionContext, Box<dyn SSCipher>)>;

/// 解密从客户端发来的数据
pub async fn ss_decrypt(
    cipher: &mut dyn SSCipher,
    data: &[u8],
) -> TunnelResult<Vec<u8>>;

/// 加密要发回给客户端的数据
pub async fn ss_encrypt(
    cipher: &mut dyn SSCipher,
    data: &[u8],
) -> TunnelResult<Vec<u8>>;
```

### 4.3 SS Proxy 主流程

在 `src/server/proxy.rs` 中新增：

```rust
pub async fn proxy_ss_connection(
    connection_id: u64,
    ss_port: u16,
    mut user_stream: TcpStream,
    ss_ctx: SSConnectionContext,
    mut cipher: Box<dyn SSCipher>,
    state: ServerState,
) {
    // 1. 连接到目标服务器 (从 ss_ctx.target_addr)
    // 2. 创建 QualityTracker 用于质量监控
    // 3. 双向流量拷贝 + 加解密 + 统计
}
```

---

## 五、配置设计

### 5.1 配置文件支持

```toml
# config.toml
[shadowsocks]
enabled = true
port = 8388
cipher = "aes-256-gcm"
password = "your-password-here"
```

### 5.2 命令行参数

```bash
cargo run --bin rust-tunnel-server -- \
  --ss-enabled \
  --ss-port 8388 \
  --ss-cipher aes-256-gcm \
  --ss-password your-password
```

### 5.3 环境变量支持

```bash
SS_ENABLED=true
SS_PORT=8388
SS_CIPHER=aes-256-gcm
SS_PASSWORD=your-password
```

---

## 六、API 设计

### 6.1 新增 API 端点

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/shadowsocks` | 获取 SS 服务状态和配置 |
| POST | `/api/shadowsocks` | 更新 SS 配置（动态启停） |
| GET | `/api/shadowsocks/stats` | 获取 SS 流量统计 |
| GET | `/api/shadowsocks/quality` | 获取 SS 连接质量数据 |

### 6.2 响应示例

```json
// GET /api/shadowsocks
{
  "enabled": true,
  "port": 8388,
  "cipher": "aes-256-gcm",
  "active_connections": 12,
  "total_bytes_tx": 123456789,
  "total_bytes_rx": 987654321
}
```

---

## 七、流量统计和质量监控集成

### 7.1 TrafficStore 集成

- SS 流量直接写入现有的 `TrafficStore`
- 使用 SS 端口号作为标记区分 Tunnel 和 SS 流量
- Dashboard 总流量自动包含 SS 流量

### 7.2 QualityTracker 集成

- 复用现有的 `QualityTracker` 机制
- 延迟、丢包、抖动计算逻辑完全复用
- 质量评分和告警逻辑完全复用
- 前端可以看到 SS 端口的质量图表

---

## 八、实施阶段划分

### Phase 1: 核心功能（MVP）

1. 扩展配置系统，添加 SS 配置项
2. 重构 `ClientInfo` 为 `PortInfo` enum
3. 实现 `shadowsocks.rs` 协议处理模块
4. 实现 `proxy_ss_connection` 函数
5. 扩展 `run_listener` 支持端口类型分流
6. 集成 TrafficStore 流量统计

### Phase 2: 质量监控和 API

1. 集成 QualityTracker 质量监控
2. 实现 SS 管理 API 端点
3. 添加数据库持久化支持
4. 前端 Shadowsocks 管理页面

### Phase 3: 高级功能（可选）

1. UDP 转发支持
2. 多端口/多用户支持
3. 插件支持
4. ACL 访问控制
5. 流量限制和配额

---

## 九、测试计划

### 9.1 单元测试

- SS 握手解析测试
- 加解密正确性测试
- 配置解析测试
- 端口类型注册测试

### 9.2 集成测试

- SS 代理连通性测试
- 流量统计准确性测试
- 与现有 Tunnel 功能并行测试
- 并发连接稳定性测试

### 9.3 兼容性测试

- 使用标准 Shadowsocks 客户端（Windows/Mac/Linux/iOS/Android）测试连接
- 测试不同加密方式

---

## 十、风险和注意事项

### 10.1 技术风险

| 风险 | 影响 | 缓解措施 |
|------|------|----------|
| SS 协议处理 bug | 高 | 充分的单元测试和集成测试 |
| 加密性能开销 | 中 | 使用 shadowsocks-rust 优化过的 crypto 实现 |
| 重构影响现有功能 | 高 | 分步重构，保留向后兼容 |

### 10.2 安全注意事项

- SS 密码在配置文件中存储要注意权限
- API 端点需要管理员认证
- 防止重放攻击（依赖 shadowsocks-rust 的实现）
- 内存安全：加密上下文使用后及时清理

---

## 十一、验收标准

- [ ] 可以通过配置文件/命令行启动 SS 服务
- [ ] 标准 SS 客户端可以连接并正常代理
- [ ] SS 流量在 Dashboard 中可见
- [ ] SS 连接质量数据正确显示
- [ ] 现有 Tunnel 功能完全正常
- [ ] SS 和 Tunnel 可以同时运行互不干扰
- [ ] 所有单元测试通过
- [ ] 性能开销在可接受范围内（< 10% 性能损失）
