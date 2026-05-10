# 连接质量监控功能设计文档

**日期**: 2026-05-10  
**版本**: v1.0  
**状态**: 设计确认

## 1. 概述

rust-tunnel 是一个内网穿透工具，目前已实现基础的心跳机制（Ping/Pong）和流量统计。本功能为其添加连接质量监控，实时监测客户端与服务器之间的连接状态。

### 1.1 功能目标

- **延迟监控**: 测量客户端到服务端的 RTT（往返时间）
- **丢包率监控**: 检测心跳包的丢失情况，计算丢包率
- **吞吐量监控**: 测量数据传输的实时速率
- **质量评分**: 综合延迟和丢包率给出 0-100 的连接质量评分
- **告警机制**: 当质量低于阈值时显示警告
- **历史趋势**: 保存质量数据用于趋势分析

### 1.2 非功能目标

- **性能开销小**: 复用现有心跳机制，最小化额外开销
- **向后兼容**: 旧版本客户端可正常连接（不显示质量数据）
- **数据持久化**: 内存保留最近1小时 + 数据库保留历史数据

---

## 2. 实现方案

采用**心跳扩展方案 + 服务器集中统计**的混合方案：

1. 扩展现有的 Ping/Pong 消息，携带序列号和时间戳
2. 服务器端计算 RTT 和丢包率（避免客户端时钟不一致）
3. 基于 TrafficStore 计算吞吐量
4. 混合存储：内存（最近1小时）+ SQLite 数据库（历史数据）

---

## 3. 协议设计

### 3.1 ControlMessage 扩展

```rust
pub enum ControlMessage {
    // --- 现有消息类型 ---

    /// 心跳 Ping（扩展后）
    Ping {
        seq: u32,                 // 心跳序列号（客户端递增）
        timestamp_micros: u64,    // 发送时间（微秒时间戳）
    },

    /// 心跳 Pong（扩展后）
    Pong {
        seq: u32,                     // 对应 Ping 的序列号
        ping_timestamp_micros: u64,   // Ping 发送时间（回显）
        pong_timestamp_micros: u64,   // Pong 发送时间（服务器填充）
    },

    // --- 保持向后兼容 ---
    // 旧版无字段的 Ping/Pong 仍可被识别，质量数据显示为 "N/A"
}
```

### 3.2 心跳流程

```
客户端                              服务器
  |                                   |
  |-- Ping { seq=N, timestamp=T1 } -->|
  |                                   |  记录接收时间 T2
  |                                   |  计算 RTT = T2 - T1
  |                                   |
  |<-- Pong { seq=N, ... } -----------|
  |                                   |
```

---

## 4. 服务端数据结构

### 4.1 ConnectionQuality（实时质量数据）

```rust
#[derive(Debug, Clone, Serialize)]
pub struct ConnectionQuality {
    // RTT 数据（毫秒）
    pub last_rtt_ms: f32,
    pub avg_rtt_ms: f32,
    pub min_rtt_ms: f32,
    pub max_rtt_ms: f32,

    // 丢包数据
    pub loss_rate: f32,          // 0.0 - 1.0
    pub consecutive_losses: u32, // 连续丢包计数

    // 吞吐量（字节/秒）
    pub bytes_in_per_sec: f64,
    pub bytes_out_per_sec: f64,

    // 质量评分
    pub quality_score: u8,       // 0-100

    // 状态
    pub last_update: DateTime<Utc>,
    pub is_warning: bool,
    pub is_critical: bool,
}
```

### 4.2 QualitySample（历史样本点）

```rust
#[derive(Debug, Clone, Serialize)]
pub struct QualitySample {
    pub timestamp: DateTime<Utc>,
    pub avg_rtt_ms: f32,
    pub loss_rate: f32,
    pub bytes_in_per_sec: f64,
    pub bytes_out_per_sec: f64,
    pub quality_score: u8,
}
```

### 4.3 QualityThresholds（告警阈值）

```rust
pub struct QualityThresholds {
    pub warning_rtt_ms: f32,      // 默认: 200ms
    pub critical_rtt_ms: f32,     // 默认: 500ms
    pub warning_loss_rate: f32,   // 默认: 0.05 (5%)
    pub critical_loss_rate: f32,  // 默认: 0.15 (15%)
}
```

---

## 5. 质量评分算法

```
质量评分 = 100 - (延迟扣分 + 丢包扣分)

延迟扣分 = min(avg_rtt_ms / 500ms * 30, 30)  // 最多扣 30 分
丢包扣分 = min(loss_rate * 70, 70)            // 最多扣 70 分

颜色映射:
  - 90-100: 绿色 (优秀)
  - 70-89:  黄色 (一般)
  - 50-69:  橙色 (较差)
  - 0-49:   红色 (严重)
```

---

## 6. 存储设计

### 6.1 内存存储

```rust
#[derive(Clone)]
pub struct QualityStore {
    /// port -> 最近 60 个样本点（1小时分钟级数据）
    samples: Arc<Mutex<HashMap<u16, VecDeque<QualitySample>>>>,
    /// port -> 当前实时质量数据
    current: Arc<Mutex<HashMap<u16, ConnectionQuality>>>,
}
```

**特性**:
- 每个端口最多保留 60 个样本点（1分钟 × 60 = 1小时）
- 客户端断开时清理内存数据

### 6.2 数据库存储

**新增表 `connection_quality_history`**:

```sql
CREATE TABLE IF NOT EXISTS connection_quality_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    port INTEGER NOT NULL,
    timestamp TEXT NOT NULL,          -- ISO8601 格式
    avg_rtt_ms REAL NOT NULL,
    min_rtt_ms REAL NOT NULL,
    max_rtt_ms REAL NOT NULL,
    loss_rate REAL NOT NULL,
    bytes_in_per_sec REAL NOT NULL,
    bytes_out_per_sec REAL NOT NULL,
    quality_score INTEGER NOT NULL,
    is_warning INTEGER NOT NULL,
    is_critical INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_quality_port_time
    ON connection_quality_history(port, timestamp);
```

**数据保留策略**:
- 分钟级数据：保留 24 小时
- 小时级聚合：保留 7 天
- 天级聚合：保留 30 天

---

## 7. API 设计

### 7.1 新增端点

```rust
// 获取所有客户端的实时质量
GET /api/quality/all
Response: { port: u16, quality: ConnectionQuality }[]

// 获取单个客户端的实时质量 + 最近1小时历史
GET /api/quality/:port
Response: {
    current: ConnectionQuality,
    history: QualitySample[]
}

// 获取历史质量数据（支持时间范围和粒度）
GET /api/quality/:port/history?start=ISO8601&end=ISO8601&granularity=minute|hour|day
Response: QualitySample[]

// 获取当前有质量问题的连接
GET /api/quality/warnings
Response: {
    port: u16,
    hostname: Option<String>,
    quality: ConnectionQuality,
    warning_type: string  // "high_latency" | "high_loss" | "critical"
}[]
```

### 7.2 扩展现有端点

在 `ClientResponse` 中添加质量字段：

```rust
pub struct ClientResponse {
    pub port: u16,
    pub hostname: Option<String>,
    pub connection_count: usize,
    pub quality: Option<ConnectionQuality>,  // 新增
}
```

---

## 8. 前端展示设计

### 8.1 ClientList 表格扩展

在客户端列表中新增列：
- **连接质量**: 彩色圆点 + 评分数字（绿色 90+，黄色 70+，橙色 50+，红色 <50）
- **延迟**: 显示当前平均 RTT（如 "45ms"）
- **丢包率**: 显示丢包百分比（如 "1.2%"）
- **状态指示器**: 警告/严重图标（黄色/红色圆点闪烁）

### 8.2 客户端详情模态框

在点击客户端弹出的详情中，新增质量标签页：
- **质量仪表盘**: 圆形进度条显示 0-100 分，颜色对应质量等级
- **RTT 折线图**: 最近 60 分钟的延迟趋势
- **丢包率折线图**: 最近 60 分钟的丢包率趋势
- **吞吐量图表**: 最近 60 分钟的带宽趋势（入站/出站）
- **告警历史**: 最近的警告/严重事件列表（时间 + 类型）

### 8.3 独立质量监控页面

新增路由 `/quality`，包含：
- **全局概览卡片**:
  - 平均连接质量评分
  - 在线客户端总数
  - 当前告警数量（警告/严重）
- **质量热力图**: 按主机名分组的客户端质量分布（颜色编码）
- **历史趋势图表**: 支持 24 小时 / 7 天 / 30 天视图切换
- **最差连接排行榜**: 质量最差的前 10 个连接（支持一键断开）

---

## 9. 实现模块划分

### 9.1 Rust 后端

| 文件 | 修改内容 |
|------|----------|
| `src/common/protocol.rs` | 扩展 Ping/Pong 消息结构 |
| `src/server/quality.rs` | 新增：质量计算、存储、评分算法 |
| `src/server/control.rs` | 集成质量监控到控制连接处理 |
| `src/server/api.rs` | 添加质量相关 API 端点 |
| `src/server/db.rs` | 添加质量历史数据表和操作 |

### 9.2 TypeScript 前端

| 文件 | 修改内容 |
|------|----------|
| `frontend/src/types/index.ts` | 添加质量相关类型定义 |
| `frontend/src/api/client.ts` | 添加质量 API 调用方法 |
| `frontend/src/components/ClientList.tsx` | 添加质量列显示 |
| `frontend/src/components/ClientDetail.tsx` | 添加质量详情和图表 |
| `frontend/src/components/QualityPage.tsx` | 新增：独立质量监控页面 |
| `frontend/src/App.tsx` | 添加质量页面路由 |

---

## 10. 测试计划

### 10.1 单元测试

- 质量评分算法正确性测试
- RTT 计算和滑动平均测试
- 丢包率计算测试
- 数据保留策略测试

### 10.2 集成测试

- 客户端连接/断开时质量数据的生命周期
- API 端点响应格式正确性
- 数据库持久化和加载正确性
- 向后兼容性测试（旧版客户端连接）

---

## 11. 性能影响评估

| 指标 | 预估影响 |
|------|----------|
| CPU | 极低（每个心跳仅做简单浮点计算） |
| 内存 | ~5KB 每个客户端（60个历史样本 + 当前状态） |
| 网络 | 极低（每个心跳仅增加 ~16 字节） |
| 数据库 | 每分钟每个客户端 1 次写入 |

