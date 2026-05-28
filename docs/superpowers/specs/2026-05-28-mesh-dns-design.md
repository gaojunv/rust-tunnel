# Mesh 组网 + 嵌入式 DNS 设计

## 概述

为 rust-tunnel 添加两个互补特性：
1. **Mesh 组网**：P2P 打洞 + 服务器中转回退，让多个客户端形成虚拟网络，任一客户端可访问其他客户端暴露的本地服务
2. **嵌入式 DNS**：为隧道端口和 Mesh 服务自动分配域名，替代 IP:端口访问方式

方案选择：**一体化协议扩展**（方案 A），在现有 `ControlMessage` 协议上扩展，服务器充当 STUN 协调中心 + DNS 权威。

## 一、Mesh 组网

### 1.1 核心概念

- **Mesh 网络**：一组客户端形成逻辑网络，通过 Mesh ID 标识
- **Mesh 路由表**：服务器维护全局路由表，记录 Mesh 成员和暴露的服务
- **P2P 打洞**：客户端通过 STUN 获取公网地址，尝试 UDP hole punching 直连
- **中转回退**：打洞失败时，流量通过服务器 MeshRelay 中继

### 1.2 协议扩展

在 `ControlMessage` 枚举中新增：

| 消息 | 方向 | 说明 |
|------|------|------|
| `MeshJoin { mesh_id, client_name }` | Client → Server | 加入 Mesh 网络 |
| `MeshLeave { mesh_id }` | Client → Server | 离开 Mesh 网络 |
| `MeshMemberList { members }` | Server → Client | Mesh 成员列表（含地址信息） |
| `MeshConnect { target_id, service_name }` | Client → Server | 请求连接 Mesh 内另一客户端的服务 |
| `P2PRequest { target_id, local_addr }` | Client → Server | 请求打洞，告知自己公网地址 |
| `P2PResponse { target_id, remote_addr }` | Server → Client | 对方公网地址 |
| `P2PResult { target_id, success }` | Client → Server | 打洞结果 |
| `MeshRelay { target_id, data }` | Client ↔ Server | 中转数据（打洞失败时） |

### 1.3 P2P 打洞流程

```
Client A          Server           Client B
   |                 |                 |
   |-- MeshJoin ---->|                 |
   |                 |<---- MeshJoin --|
   |<-- MemberList--|-- MemberList -->|
   |                 |                 |
   |-- P2PRequest --|                 |  (A 告知自己的地址)
   |                 |-- P2PResponse ->|  (服务器把 A 的地址转发给 B)
   |                 |<-- P2PRequest --|  (B 告知自己的地址)
   |<- P2PResponse -|                 |  (服务器把 B 的地址转发给 A)
   |                 |                 |
   |<========= UDP 打洞尝试 =========>|
   |                 |                 |
   |-- P2PResult -->|                 |  (报告成功/失败)
   |                 |                 |
   |   [成功] 直接通信                  |
   |   [失败] 通过 MeshRelay 中转      |
```

### 1.4 服务器角色

- **STUN**：帮助客户端发现公网 IP:Port
- **协调者**：交换客户端地址信息、仲裁打洞
- **中继**：打洞失败时转发 Mesh 流量
- **路由表维护**：跟踪 Mesh 成员、在线状态、暴露的服务

## 二、嵌入式 DNS

### 2.1 域名体系

| 域名区 | 示例 | 解析目标 |
|--------|------|----------|
| `*.tunnel.local` | `webapp.tunnel.local` → 服务器 IP | 隧道端口的域名映射 |
| `*.mesh.local` | `db.mesh.local` → Mesh 内客户端 B 的 IP | Mesh 服务发现 |

### 2.2 域名注册规则

**隧道端口域名**（`*.tunnel.local`）：
- 客户端注册转发规则时可指定 `dns_name`，如 `--forward 9000:localhost:80 --dns-name webapp`
- 自动生成 `webapp.tunnel.local` → 服务器 IP
- SRV 记录 `_http._tcp.webapp.tunnel.local` → 端口 9000
- 未指定 `dns_name` 时，使用 `port-9000.tunnel.local` 作为默认域名

**Mesh 服务发现**（`*.mesh.local`）：
- 客户端加入 Mesh 时声明暴露的服务：`--mesh mynet --service db:mysql:3306`
- 自动生成 `db.mynet.mesh.local` → Mesh 内可达地址
- 服务名在 Mesh 内必须唯一，冲突时后加入的覆盖并告警

### 2.3 DNS 服务器实现

- 使用 `hickory-dns` crate，轻量级权威 DNS 服务器
- 监听 UDP/TCP 53 端口（可配置），与 API 服务器共存
- 内存中维护区域文件，服务器重启时从客户端注册信息重建
- 支持标准查询类型：A、AAAA、SRV、TXT
- TXT 记录可选存储元信息（如 `version=1.0,client=client-a`）

### 2.4 配置扩展

服务器新增配置：
```toml
[dns]
enabled = true
bind = "0.0.0.0:53"
tunnel_domain = "tunnel.local"
mesh_domain = "mesh.local"
```

客户端新增配置：
```toml
[mesh]
id = "my-network"
name = "client-a"

[[mesh.services]]
name = "db"
protocol = "mysql"
local_addr = "localhost:3306"
```

### 2.5 DNS + Mesh 联动

- 客户端通过 `MeshJoin` 加入网络后，服务器自动在 `*.mesh.local` 区注册其服务记录
- Mesh 内客户端请求 `db.mynet.mesh.local` 时，DNS 返回目标客户端的 Mesh 地址
- 隧道客户端注册转发规则时，服务器自动在 `*.tunnel.local` 区注册 A + SRV 记录
- 客户端断开时自动删除对应 DNS 记录

## 三、模块划分

### 3.1 新增文件

```
src/
├── common/
│   ├── protocol.rs      # 扩展 ControlMessage
│   ├── stun.rs          # NEW: STUN 协议实现（RFC 5389 精简版）
│   └── mesh_types.rs    # NEW: Mesh 共享类型
├── server/
│   ├── mesh/
│   │   ├── mod.rs       # Mesh 管理器：路由表、成员管理、打洞协调
│   │   ├── router.rs    # Mesh 路由表
│   │   ├── relay.rs     # 中继转发
│   │   └── stun.rs      # STUN 服务器
│   ├── dns/
│   │   ├── mod.rs       # DNS 服务器入口
│   │   ├── zone.rs      # 区域管理
│   │   └── registry.rs  # DNS 注册表
│   └── config.rs        # 扩展配置
├── client/
│   ├── mesh/
│   │   ├── mod.rs       # Mesh 客户端
│   │   ├── p2p.rs       # P2P 打洞
│   │   └── relay.rs     # 中继客户端
│   ├── config.rs        # 扩展配置
│   └── control.rs       # 扩展控制消息处理
```

### 3.2 数据流

**场景 1：客户端 A 访问 Mesh 内客户端 B 的 MySQL**

```
1. A 查询 db.mynet.mesh.local → DNS 返回 B 的 Mesh 地址
2. A 发送 MeshConnect{target: B, service: "db"} → 服务器
3. 服务器发起 P2P 打洞协调
4a. [打洞成功] A ←──UDP──→ B 直连
4b. [打洞失败] A ←──MeshRelay──→ 服务器 ←──MeshRelay──→ B 中转
5. A 的本地端口映射到 B 的 MySQL
```

**场景 2：外部用户通过域名访问隧道端口**

```
1. 用户访问 webapp.tunnel.local → DNS 返回服务器 IP + SRV 端口
2. 用户连接服务器:9000 → 现有隧道转发逻辑（不变）
```

### 3.3 数据库扩展

```sql
CREATE TABLE mesh_networks (
    id          TEXT PRIMARY KEY,
    created_at  DATETIME NOT NULL,
    description TEXT
);

CREATE TABLE mesh_services (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    mesh_id      TEXT NOT NULL REFERENCES mesh_networks(id),
    client_name  TEXT NOT NULL,
    service_name TEXT NOT NULL,
    protocol     TEXT NOT NULL,
    local_addr   TEXT NOT NULL,
    dns_record   TEXT NOT NULL,
    UNIQUE(mesh_id, service_name)
);
```

DNS 记录是动态的（随客户端上线/下线变化），不持久化到数据库。

### 3.4 前端扩展

- **新增 Mesh 页面**：Mesh 网络列表、成员、服务、P2P 状态
- **Dashboard 集成**：客户端列表显示 Mesh 成员关系和 DNS 域名
- **DNS 管理面板**：查看所有 DNS 记录，手动添加/编辑隧道域名

### 3.5 API 端点扩展

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/mesh` | 列出所有 Mesh 网络 |
| GET | `/api/mesh/:id` | Mesh 详情 |
| GET | `/api/mesh/:id/services` | Mesh 内服务列表 |
| GET | `/api/dns/records` | 所有 DNS 记录 |
| POST | `/api/dns/records` | 手动添加 DNS 记录 |
| DELETE | `/api/dns/records/:name` | 删除 DNS 记录 |

## 四、依赖

- `hickory-dns`：DNS 服务器实现
- STUN 协议自行实现精简版（RFC 5389 核心部分），无需额外 crate

## 五、测试策略

- STUN 消息解析/构建：单元测试
- Mesh 路由表：单元测试（加入/离开/查找）
- DNS 区域管理：单元测试（注册/注销/查询）
- P2P 打洞：集成测试（需要两个客户端 + 服务器）
- 端到端：启动服务器 + 两个客户端，验证 Mesh 连通性和 DNS 解析
