# 日志查看器功能设计

## 概述

在服务端前端添加日志 Tab，实时查看后端服务日志和已连接客户端的日志。采用自定义 Tracing Layer 捕获日志，SSE 推送实时日志，REST 拉取历史日志，SQLite 持久化存储。

## 架构

### 数据流

```
服务端 tracing → 自定义 Layer → LogStore → 内存缓冲区(1000条)
                                           → SQLite(7天保留)
                                           → broadcast channel → SSE /api/logs/stream

客户端 tracing → 自定义 Layer → 批量缓冲(50条/2秒)
                               → ControlMessage::LogBatch
                               → 服务端 control.rs → LogStore

前端 LogsPage → EventSource(/api/logs/stream) → 实时追加
              → GET /api/logs                  → 历史加载
              → PUT /api/logs/level            → 动态调整级别
```

## 后端设计

### 核心组件：LogStore

共享状态，通过 `Arc<Mutex<..>>` 在 Layer 和 API 之间共享：

- `buffer: VecDeque<LogEntry>` — 内存环形缓冲区，容量 1000 条
- `tx: tokio::sync::broadcast::Sender<LogEntry>` — SSE 订阅通道
- `db: SqlitePool` — SQLite 连接池
- `level: Arc<AtomicU8>` — 当前日志级别（可动态调整）
- `buffer_write_tx/mpsc` — 非阻塞写入通道，后台任务处理 DB 写入

### LogEntry 结构

```rust
struct LogEntry {
    id: i64,           // 自增 ID（DB 分配，内存中为 0）
    timestamp: i64,    // 微秒时间戳
    level: String,     // TRACE/DEBUG/INFO/WARN/ERROR
    source: String,    // "server" | "client:{hostname}:{port}"
    target: String,    // tracing target（模块路径）
    message: String,   // 日志内容
}
```

### 自定义 Tracing Layer

实现 `tracing_subscriber::Layer` trait：

- `enabled()`: 检查 `LogStore` 中的动态级别，低于该级别的日志直接丢弃
- `on_event()`: 格式化日志事件为 `LogEntry`，通过 mpsc 通道发送到后台写入任务
- 后台任务：写入内存缓冲区 + broadcast 通知 SSE + 批量写入 SQLite（每 500ms 或 50 条）
- source 固定为 `"server"`

### 客户端日志上报

新增协议消息：

```rust
// ControlMessage 新增变体
LogBatch {
    entries: Vec<ClientLogEntry>,
}

struct ClientLogEntry {
    timestamp: i64,    // 微秒时间戳
    level: String,     // TRACE/DEBUG/INFO/WARN/ERROR
    target: String,    // tracing target
    message: String,   // 日志内容
}
```

客户端侧实现：

- 自定义 Layer 捕获日志，缓冲到 `Vec<ClientLogEntry>`
- 当缓冲满 50 条或每 2 秒，发送 `ControlMessage::LogBatch`
- 服务端 `control.rs` 收到后写入 `LogStore`，source 标记为 `client:{hostname}:{port}`

### API 端点

| 端点 | 方法 | 说明 |
|------|------|------|
| `/api/logs` | GET | 拉取历史日志 |
| `/api/logs/stream` | GET | SSE 流推送新日志 |
| `/api/logs/level` | GET | 获取当前日志级别 |
| `/api/logs/level` | PUT | 动态设置日志级别 |

**GET /api/logs 查询参数**：
- `level`: 过滤级别（如 "warn" 表示只返回 WARN 及以上）
- `source`: 过滤来源（"server" 或 "client"）
- `search`: 关键词搜索（匹配 message 字段）
- `limit`: 返回条数（默认 200，最大 1000）
- `before_id`: 分页游标，返回 id < before_id 的记录

**GET /api/logs/stream (SSE)**：
- `event: log` + `data: {LogEntry JSON}` — 新日志
- `event: ping` — 心跳（每 30 秒）
- 支持查询参数 `level` 和 `source` 过滤
- 连接断开时自动清理订阅者

**PUT /api/logs/level**：
- Body: `{"level": "debug"}`
- 更新 `LogStore` 的动态级别，即时生效

### SQLite 表结构

```sql
CREATE TABLE server_logs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp INTEGER NOT NULL,
    level TEXT NOT NULL,
    source TEXT NOT NULL,
    target TEXT NOT NULL,
    message TEXT NOT NULL
);
CREATE INDEX idx_logs_timestamp ON server_logs(timestamp);
CREATE INDEX idx_logs_level ON server_logs(level);
```

**数据保留**：后台定时任务每 1 小时清理 7 天前的记录。

### logging.rs 修改

- `init_logging()` 改为接受 `LogStore` 参数
- 注册自定义 Layer 与现有的 fmt layer 和 EnvFilter 并行
- fmt layer 保持不变（stderr 输出不受影响）

## 前端设计

### 新增文件

- `frontend/src/components/LogsPage.tsx` — 日志页面主组件

### 修改文件

- `Dashboard.tsx` — 新增 `'logs'` tab 类型，条件渲染 `<LogsPage />`
- `Navbar.tsx` — 新增 "日志" 按钮

### LogsPage 布局

```
┌─────────────────────────────────────────────────┐
│ [级别过滤 ▼] [🔍 搜索...] [暂停/继续] [设置级别 ▼] │
├─────────────────────────────────────────────────┤
│ 12:00:01.234 INFO  server  tunnel::control     │
│                    Client registered on :9000   │
│ 12:00:02.567 WARN  client:web:9000 tunnel::proxy│
│                    Connection timeout to local   │
│ 12:00:03.890 ERROR server  tunnel::proxy       │
│                    Failed to forward traffic     │
│ ...                                              │
├─────────────────────────────────────────────────┤
│ [自动滚动 ✓] [加载更多]                          │
└─────────────────────────────────────────────────┘
```

### 功能细节

**日志级别过滤**：下拉选择 ALL/TRACE/DEBUG/INFO/WARN/ERROR，同时作为 SSE 连接参数和服务端过滤参数

**关键词搜索**：输入框实时过滤当前已加载的日志（前端内存过滤）

**暂停/继续**：
- 暂停时关闭 EventSource，继续时重新连接
- 暂停期间的日志通过重新连接后的历史加载补回

**动态级别设置**：下拉选择级别，调用 `PUT /api/logs/level`

**颜色标记**：ERROR 红色、WARN 黄色、INFO 蓝色、DEBUG 灰色、TRACE 暗灰

**自动滚动**：新日志到达时自动滚动到底部，用户手动上滚时暂停自动滚动

**历史加载**：页面首次加载通过 `GET /api/logs` 拉取最近 200 条，点击"加载更多"拉取更早的记录

### SSE 连接管理

```tsx
useEffect(() => {
  const es = new EventSource('/api/logs/stream?level=info');
  es.addEventListener('log', (e) => {
    const entry = JSON.parse(e.data);
    appendLog(entry);
  });
  return () => es.close();
}, [levelFilter, isPaused]);
```

- 级别过滤变化时重新建立连接（带新参数）
- 暂停时关闭连接，继续时重新连接
- 认证：SSE 连接通过 URL 查询参数传递 token（`?token=xxx`），服务端 SSE 端点需支持此认证方式

## 错误处理

- SSE 连接断开：自动重连（指数退避，最大 30 秒）
- SQLite 写入失败：日志到 warn 级别，不影响主流程
- 客户端日志上报失败：客户端侧丢弃旧日志，保持缓冲区不溢出
- broadcast 通道满（慢消费者）：丢弃消息，SSE 端点检测到 lag 时发送同步事件

## 测试策略

- 后端：LogStore 单元测试（缓冲区、级别过滤、搜索）、API 集成测试（SSE 流、历史查询、级别设置）
- 前端：LogsPage 组件测试（过滤、搜索、暂停/继续）
