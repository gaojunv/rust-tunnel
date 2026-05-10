# 连接质量监控功能实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为 rust-tunnel 内网穿透工具添加连接质量监控功能，包括 RTT 延迟、丢包率、吞吐量测量和质量评分（0-100），并在前端实时展示。

**Architecture:** 扩展现有的 Ping/Pong 心跳协议，服务器端集中计算质量指标。使用混合存储方案：内存保留最近1小时分钟级数据，SQLite 数据库持久化历史数据。API 层提供实时和历史数据端点，前端集成到现有列表、详情模态框和独立监控页面。

**Tech Stack:** Rust (Tokio, Axum, SQLx, Serde), TypeScript (React, Vite, Recharts), SQLite

---

## Task 1: 协议扩展 - ControlMessage Ping/Pong

**Files:**
- Modify: `src/common/protocol.rs`
- Test: `src/common/protocol.rs` (existing tests module)

### 背景信息
现有的 Ping 和 Pong 是无字段的枚举变体。需要将其改为带字段的结构化变体以携带序列号和时间戳。序列化使用 bincode。

### 实现步骤

- [ ] **Step 1: 修改 ControlMessage 枚举定义**

在 `src/common/protocol.rs:6-32`，将 `Ping` 和 `Pong` 从无字段改为带字段的结构：

```rust
#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum ControlMessage {
    /// 客户端请求注册以暴露远程端口
    Register {
        remote_port: u16,
        /// 客户端机器的主机名（向后兼容可选）
        hostname: Option<String>,
        /// 认证令牌（向后兼容可选，如果服务器启用认证则必需）
        auth_token: Option<String>,
    },
    /// 服务器对注册的响应
    RegisterResponse { success: bool, message: String },
    /// 服务器通知客户端有新的传入连接
    NewConnection { connection_id: u64, remote_port: u16 },
    /// 客户端通知服务器它已连接到本地目标并准备好
    ConnectionReady { connection_id: u64 },
    /// 特定连接的数据传输
    Data { connection_id: u64, data: Vec<u8> },
    /// 关闭特定连接
    Close { connection_id: u64 },
    /// 心跳 Ping（客户端 -> 服务器）
    Ping {
        /// 心跳序列号（客户端递增）
        seq: u32,
        /// 发送时间戳（微秒，客户端时间）
        timestamp_micros: u64,
    },
    /// 心跳 Pong（服务器 -> 客户端）
    Pong {
        /// 对应 Ping 的序列号
        seq: u32,
        /// Ping 发送时间戳（回显）
        ping_timestamp_micros: u64,
        /// Pong 发送时间戳（服务器时间）
        pong_timestamp_micros: u64,
    },
    /// 服务器请求客户端断开连接（Web 界面管理操作）
    Disconnect,
}
```

- [ ] **Step 2: 更新现有测试用例**

在 `src/common/protocol.rs:111-123`，更新消息变体序列化测试：

```rust
let messages = vec![
    ControlMessage::Register { remote_port: 8080, hostname: None, auth_token: None },
    ControlMessage::RegisterResponse { success: true, message: "ok".into() },
    ControlMessage::NewConnection { connection_id: 12345, remote_port: 9000 },
    ControlMessage::ConnectionReady { connection_id: 12345 },
    ControlMessage::Data { connection_id: 12345, data: vec![1, 2, 3, 4] },
    ControlMessage::Close { connection_id: 12345 },
    ControlMessage::Ping { seq: 1, timestamp_micros: 1234567890 },
    ControlMessage::Pong { seq: 1, ping_timestamp_micros: 1234567890, pong_timestamp_micros: 1234567891 },
    ControlMessage::Disconnect,
];
```

- [ ] **Step 3: 运行测试验证编译通过**

Run: `cargo test --package rust-tunnel --bin rust-tunnel-server -- protocol::tests 2>&1 | head -50`
Expected: All tests pass

- [ ] **Step 4: 提交**

```bash
git add src/common/protocol.rs
git commit -m "feat(protocol): extend Ping/Pong with sequence and timestamp for quality monitoring"
```

---

## Task 2: 质量数据结构定义

**Files:**
- Create: `src/server/quality.rs`
- Modify: `src/server/mod.rs` (add module export)

### 背景信息
需要定义连接质量相关的数据结构，包括实时质量数据、历史样本点、告警阈值和存储结构。

### 实现步骤

- [ ] **Step 1: 创建 src/server/quality.rs 并定义数据结构**

```rust
use chrono::{DateTime, Utc, Timelike};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::Mutex;

/// 连接质量告警阈值
#[derive(Debug, Clone)]
pub struct QualityThresholds {
    /// 延迟警告阈值（毫秒）
    pub warning_rtt_ms: f32,
    /// 延迟严重阈值（毫秒）
    pub critical_rtt_ms: f32,
    /// 丢包警告阈值（0.0-1.0）
    pub warning_loss_rate: f32,
    /// 丢包严重阈值（0.0-1.0）
    pub critical_loss_rate: f32,
}

impl Default for QualityThresholds {
    fn default() -> Self {
        Self {
            warning_rtt_ms: 200.0,
            critical_rtt_ms: 500.0,
            warning_loss_rate: 0.05,
            critical_loss_rate: 0.15,
        }
    }
}

/// 单个客户端的实时连接质量数据
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

impl Default for ConnectionQuality {
    fn default() -> Self {
        Self {
            last_rtt_ms: 0.0,
            avg_rtt_ms: 0.0,
            min_rtt_ms: f32::MAX,
            max_rtt_ms: 0.0,
            loss_rate: 0.0,
            consecutive_losses: 0,
            bytes_in_per_sec: 0.0,
            bytes_out_per_sec: 0.0,
            quality_score: 100,
            last_update: Utc::now(),
            is_warning: false,
            is_critical: false,
        }
    }
}

/// 历史质量样本点（每分钟一个点）
#[derive(Debug, Clone, Serialize)]
pub struct QualitySample {
    pub timestamp: DateTime<Utc>,
    pub avg_rtt_ms: f32,
    pub loss_rate: f32,
    pub bytes_in_per_sec: f64,
    pub bytes_out_per_sec: f64,
    pub quality_score: u8,
}

/// 用于 RTT 和丢包计算的中间状态（每个连接独立维护）
#[derive(Debug, Clone)]
pub struct QualityTracker {
    /// 下一个期望的序列号
    expected_seq: u32,
    /// 最近 RTT 样本（用于滑动平均）
    rtt_samples: VecDeque<f32>,
    /// 总心跳计数
    total_pings: u64,
    /// 丢失心跳计数
    lost_pings: u64,
    /// 最近 60 个心跳的丢包情况（用于短期丢包率）
    recent_losses: VecDeque<bool>,
}

impl Default for QualityTracker {
    fn default() -> Self {
        Self {
            expected_seq: 1,
            rtt_samples: VecDeque::with_capacity(20),
            total_pings: 0,
            lost_pings: 0,
            recent_losses: VecDeque::with_capacity(60),
        }
    }
}

impl QualityTracker {
    /// 记录收到的 Ping，计算丢包
    pub fn record_ping(&mut self, seq: u32) -> (u32, f32) {
        self.total_pings += 1;

        // 计算丢失的包数
        let lost = if seq > self.expected_seq {
            seq - self.expected_seq
        } else {
            0
        };

        if lost > 0 {
            self.lost_pings += lost as u64;
            for _ in 0..lost {
                self.recent_losses.push_back(true);
            }
        }

        // 记录当前包是否丢失（序列号回退视为乱序，不算丢失）
        self.recent_losses.push_back(false);

        // 保持最近 60 个样本
        while self.recent_losses.len() > 60 {
            self.recent_losses.pop_front();
        }

        // 更新下一个期望的序列号
        self.expected_seq = seq + 1;

        // 计算短期丢包率
        let recent_loss_count = self.recent_losses.iter().filter(|&&l| l).count();
        let loss_rate = if self.recent_losses.is_empty() {
            0.0
        } else {
            recent_loss_count as f32 / self.recent_losses.len() as f32
        };

        (lost, loss_rate)
    }

    /// 记录 RTT 样本
    pub fn record_rtt(&mut self, rtt_ms: f32) {
        self.rtt_samples.push_back(rtt_ms);
        while self.rtt_samples.len() > 20 {
            self.rtt_samples.pop_front();
        }
    }

    /// 获取平均 RTT
    pub fn get_avg_rtt(&self) -> f32 {
        if self.rtt_samples.is_empty() {
            return 0.0;
        }
        self.rtt_samples.iter().sum::<f32>() / self.rtt_samples.len() as f32
    }

    /// 获取最小 RTT
    pub fn get_min_rtt(&self) -> f32 {
        self.rtt_samples.iter().cloned().fold(f32::MAX, f32::min)
    }

    /// 获取最大 RTT
    pub fn get_max_rtt(&self) -> f32 {
        self.rtt_samples.iter().cloned().fold(0.0, f32::max)
    }
}

/// 计算质量评分 (0-100)
pub fn calculate_quality_score(avg_rtt_ms: f32, loss_rate: f32) -> u8 {
    let latency_penalty = (avg_rtt_ms / 500.0 * 30.0).min(30.0);
    let loss_penalty = (loss_rate * 70.0).min(70.0);
    let score = 100.0 - latency_penalty - loss_penalty;
    score.max(0.0).min(100.0).round() as u8
}

/// 检查是否触发告警
pub fn check_warnings(
    avg_rtt_ms: f32,
    loss_rate: f32,
    thresholds: &QualityThresholds,
) -> (bool, bool) {
    let is_critical = avg_rtt_ms >= thresholds.critical_rtt_ms || loss_rate >= thresholds.critical_loss_rate;
    let is_warning = !is_critical && (
        avg_rtt_ms >= thresholds.warning_rtt_ms || loss_rate >= thresholds.warning_loss_rate
    );
    (is_warning, is_critical)
}

/// 连接质量存储（内存）
#[derive(Clone)]
pub struct QualityStore {
    /// port -> 当前实时质量数据
    current: Arc<Mutex<HashMap<u16, ConnectionQuality>>>,
    /// port -> 最近 60 个样本点（1小时分钟级数据）
    samples: Arc<Mutex<HashMap<u16, VecDeque<QualitySample>>>>,
    /// 告警阈值配置
    thresholds: QualityThresholds,
}

impl Default for QualityStore {
    fn default() -> Self {
        Self::new()
    }
}

impl QualityStore {
    /// 创建新的质量存储
    pub fn new() -> Self {
        Self {
            current: Arc::new(Mutex::new(HashMap::new())),
            samples: Arc::new(Mutex::new(HashMap::new())),
            thresholds: QualityThresholds::default(),
        }
    }

    /// 更新实时质量数据
    pub async fn update_quality(&self, port: u16, quality: ConnectionQuality) {
        let mut current = self.current.lock().await;
        current.insert(port, quality);
    }

    /// 添加历史样本点
    pub async fn add_sample(&self, port: u16, sample: QualitySample) {
        let mut samples = self.samples.lock().await;
        let port_samples = samples.entry(port).or_insert_with(|| VecDeque::with_capacity(60));
        port_samples.push_back(sample);

        // 保留最近 60 个样本（1小时）
        while port_samples.len() > 60 {
            port_samples.pop_front();
        }
    }

    /// 获取实时质量数据
    pub async fn get_quality(&self, port: u16) -> Option<ConnectionQuality> {
        let current = self.current.lock().await;
        current.get(&port).cloned()
    }

    /// 获取所有端口的实时质量数据
    pub async fn get_all_quality(&self) -> Vec<(u16, ConnectionQuality)> {
        let current = self.current.lock().await;
        current.iter().map(|(k, v)| (*k, v.clone())).collect()
    }

    /// 获取历史样本
    pub async fn get_samples(&self, port: u16) -> Vec<QualitySample> {
        let samples = self.samples.lock().await;
        samples.get(&port).map(|s| s.iter().cloned().collect()).unwrap_or_default()
    }

    /// 移除端口数据（客户端断开时调用）
    pub async fn remove_port(&self, port: u16) {
        let mut current = self.current.lock().await;
        let mut samples = self.samples.lock().await;
        current.remove(&port);
        samples.remove(&port);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_quality_score_calculation() {
        // 完美连接
        assert_eq!(calculate_quality_score(0.0, 0.0), 100);

        // 延迟扣分
        assert_eq!(calculate_quality_score(250.0, 0.0), 85); // 250/500*30 = 15
        assert_eq!(calculate_quality_score(500.0, 0.0), 70); // 最多扣 30 分
        assert_eq!(calculate_quality_score(1000.0, 0.0), 70); // 超过也只扣 30 分

        // 丢包扣分
        assert_eq!(calculate_quality_score(0.0, 0.1), 93);  // 0.1 * 70 = 7
        assert_eq!(calculate_quality_score(0.0, 0.5), 65);  // 0.5 * 70 = 35
        assert_eq!(calculate_quality_score(0.0, 1.0), 30);  // 最多扣 70 分

        // 组合情况
        assert_eq!(calculate_quality_score(500.0, 1.0), 0); // 30 + 70 = 100 扣分
    }

    #[test]
    fn test_warning_checks() {
        let thresholds = QualityThresholds::default();

        // 正常
        assert_eq!(check_warnings(100.0, 0.01, &thresholds), (false, false));

        // 延迟警告
        assert_eq!(check_warnings(250.0, 0.01, &thresholds), (true, false));

        // 延迟严重
        assert_eq!(check_warnings(600.0, 0.01, &thresholds), (false, true));

        // 丢包警告
        assert_eq!(check_warnings(100.0, 0.1, &thresholds), (true, false));

        // 丢包严重
        assert_eq!(check_warnings(100.0, 0.2, &thresholds), (false, true));

        // 严重优先
        assert_eq!(check_warnings(600.0, 0.2, &thresholds), (false, true));
    }

    #[test]
    fn test_quality_tracker() {
        let mut tracker = QualityTracker::default();

        // 正常接收
        tracker.record_ping(1);
        tracker.record_ping(2);
        tracker.record_ping(3);

        // 丢包
        let (lost, _) = tracker.record_ping(6); // 丢失 4, 5
        assert_eq!(lost, 2);

        // RTT 记录
        tracker.record_rtt(50.0);
        tracker.record_rtt(60.0);
        tracker.record_rtt(70.0);
        assert_eq!(tracker.get_avg_rtt(), 60.0);
        assert_eq!(tracker.get_min_rtt(), 50.0);
        assert_eq!(tracker.get_max_rtt(), 70.0);
    }

    #[tokio::test]
    async fn test_quality_store() {
        let store = QualityStore::new();

        // 添加数据
        let quality = ConnectionQuality::default();
        store.update_quality(8080, quality).await;

        // 获取数据
        let result = store.get_quality(8080).await;
        assert!(result.is_some());

        // 获取所有
        let all = store.get_all_quality().await;
        assert_eq!(all.len(), 1);

        // 移除
        store.remove_port(8080).await;
        let result = store.get_quality(8080).await;
        assert!(result.is_none());
    }
}
```

- [ ] **Step 2: 在 src/server/mod.rs 中添加模块导出**

首先查看现有文件内容：
Run: `grep -n "pub mod" /root/projects/rust-tunnel/src/server/mod.rs`

然后添加：
```rust
pub mod quality;
```

- [ ] **Step 3: 运行测试验证**

Run: `cargo test --package rust-tunnel -- server::quality::tests 2>&1 | head -50`
Expected: All tests pass

- [ ] **Step 4: 提交**

```bash
git add src/server/quality.rs src/server/mod.rs
git commit -m "feat(server): add quality data structures and store"
```

---

## Task 3: 控制连接集成 - 实时质量计算

**Files:**
- Modify: `src/server/control.rs`
- Modify: `src/bin/client.rs` (update client to send extended Ping)

### 背景信息
需要在控制连接处理中集成质量监控逻辑。在 `handle_control_connection` 函数中处理 Ping 消息时计算 RTT 和丢包率，并更新 QualityStore。同时需要更新客户端以发送带序列号和时间戳的 Ping 消息。

### 实现步骤

- [ ] **Step 1: 更新 ServerState 以包含 QualityStore**

在 `src/server/control.rs:38-69`，修改 `ServerState` 结构体和构造函数：

```rust
/// 全局服务器状态，在所有任务之间共享
#[derive(Clone)]
pub struct ServerState {
    /// 从远程端口到客户端信息的映射
    clients: Arc<Mutex<HashMap<u16, ClientInfo>>>,
    /// 从连接 ID 到活动连接信息的映射
    active_connections: Arc<Mutex<HashMap<u64, ActiveConnection>>>,
    /// 流量统计存储
    pub traffic_store: TrafficStore,
    /// 数据库连接（可选）
    db: Option<Database>,
    /// 连接质量存储（新增）
    pub quality_store: QualityStore,
    /// 每个端口的质量跟踪器（新增）
    quality_trackers: Arc<Mutex<HashMap<u16, QualityTracker>>>,
}

impl ServerState {
    /// 创建新的服务器状态（不带数据库，用于向后兼容）
    pub fn new() -> Self {
        Self {
            clients: Arc::new(Mutex::new(HashMap::new())),
            active_connections: Arc::new(Mutex::new(HashMap::new())),
            traffic_store: TrafficStore::new(),
            db: None,
            quality_store: QualityStore::new(),
            quality_trackers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 创建新的带数据库的服务器状态
    pub fn with_db(db: Database) -> Self {
        Self {
            clients: Arc::new(Mutex::new(HashMap::new())),
            active_connections: Arc::new(Mutex::new(HashMap::new())),
            traffic_store: TrafficStore::with_db(db.clone()),
            db: Some(db),
            quality_store: QualityStore::new(),
            quality_trackers: Arc::new(Mutex::new(HashMap::new())),
        }
    }
```

- [ ] **Step 2: 在 ServerState 中添加质量跟踪方法**

在 `ServerState` impl 块末尾添加：

```rust
    /// 获取或创建端口的质量跟踪器
    pub async fn get_or_create_quality_tracker(&self, port: u16) -> QualityTracker {
        let mut trackers = self.quality_trackers.lock().await;
        trackers.entry(port).or_default().clone()
    }

    /// 更新端口的质量跟踪器
    pub async fn update_quality_tracker(&self, port: u16, tracker: QualityTracker) {
        let mut trackers = self.quality_trackers.lock().await;
        trackers.insert(port, tracker);
    }

    /// 移除端口的质量跟踪器（客户端断开时）
    pub async fn remove_quality_tracker(&self, port: u16) {
        let mut trackers = self.quality_trackers.lock().await;
        trackers.remove(&port);
    }
```

- [ ] **Step 3: 在 handle_control_connection 中处理 Ping 消息**

在 `handle_control_connection` 函数的主循环中，找到处理 `ControlMessage::Ping` 的位置并更新：

```rust
ControlMessage::Ping { seq, timestamp_micros } => {
    // 计算 RTT（使用服务器当前时间）
    let now = Utc::now();
    let now_micros = now.timestamp_micros() as u64;
    let rtt_ms = if now_micros > timestamp_micros {
        (now_micros - timestamp_micros) as f32 / 1000.0
    } else {
        0.0 // 时钟回退，暂时忽略
    };

    // 遍历此连接注册的所有端口，更新质量
    for &port in &registered_ports {
        // 获取或创建质量跟踪器
        let mut tracker = state.get_or_create_quality_tracker(port).await;

        // 记录 Ping，计算丢包
        let (lost, loss_rate) = tracker.record_ping(seq);

        // 记录 RTT
        tracker.record_rtt(rtt_ms);

        // 计算统计数据
        let avg_rtt = tracker.get_avg_rtt();
        let min_rtt = tracker.get_min_rtt();
        let max_rtt = tracker.get_max_rtt();

        // 计算质量评分
        let quality_score = calculate_quality_score(avg_rtt, loss_rate);

        // 检查告警
        let thresholds = QualityThresholds::default();
        let (is_warning, is_critical) = check_warnings(avg_rtt, loss_rate, &thresholds);

        // 获取吞吐量（从 TrafficStore 计算）
        let traffic_samples = state.traffic_store.get_port_traffic(port).await;
        let (bytes_in_per_sec, bytes_out_per_sec) = if let Some(traffic) = traffic_samples {
            // 使用最近两个桶计算速率（简化版）
            if traffic.buckets.len() >= 2 {
                let latest = &traffic.buckets[traffic.buckets.len() - 1];
                let prev = &traffic.buckets[traffic.buckets.len() - 2];
                let interval_sec = (latest.timestamp - prev.timestamp).num_seconds().max(1) as f64;
                (
                    latest.bytes_in as f64 / interval_sec,
                    latest.bytes_out as f64 / interval_sec,
                )
            } else {
                (0.0, 0.0)
            }
        } else {
            (0.0, 0.0)
        };

        // 更新实时质量数据
        let quality = ConnectionQuality {
            last_rtt_ms: rtt_ms,
            avg_rtt_ms: avg_rtt,
            min_rtt_ms: min_rtt,
            max_rtt_ms: max_rtt,
            loss_rate,
            consecutive_losses: lost,
            bytes_in_per_sec,
            bytes_out_per_sec,
            quality_score,
            last_update: now,
            is_warning,
            is_critical,
        };

        state.quality_store.update_quality(port, quality).await;
        state.update_quality_tracker(port, tracker).await;

        // 每分钟添加一次历史样本
        if now.second() < 10 {
            let sample = QualitySample {
                timestamp: now,
                avg_rtt_ms: avg_rtt,
                loss_rate,
                bytes_in_per_sec,
                bytes_out_per_sec,
                quality_score,
            };
            state.quality_store.add_sample(port, sample).await;
        }
    }

    // 发送 Pong 响应
    let _ = sender.send(ControlMessage::Pong {
        seq,
        ping_timestamp_micros: timestamp_micros,
        pong_timestamp_micros: Utc::now().timestamp_micros() as u64,
    }).await;
}
```

- [ ] **Step 4: 在客户端清理时也清理质量数据**

在 `handle_control_connection` 末尾的清理逻辑中，在 `state.remove_client(port).await` 后添加：

```rust
// 同时清理质量数据
state.quality_store.remove_port(port).await;
state.remove_quality_tracker(port).await;
```

- [ ] **Step 5: 添加必要的 use 导入**

在 `src/server/control.rs` 顶部添加：

```rust
use crate::server::quality::{
    QualityStore, QualityTracker, ConnectionQuality, QualitySample,
    calculate_quality_score, check_warnings, QualityThresholds,
};
```

- [ ] **Step 6: 更新客户端发送扩展 Ping**

查看 `src/bin/client.rs` 中发送 Ping 的代码：
Run: `grep -n -A5 -B5 "Ping" /root/projects/rust-tunnel/src/bin/client.rs`

更新客户端发送带序列号和时间戳的 Ping：

```rust
// 在客户端的心跳循环中
use std::time::SystemTime;

// 客户端需要维护自己的序列号
let mut ping_seq = 1u32;

// 然后在发送 Ping 时：
loop {
    let timestamp_micros = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64;

    if let Err(e) = ControlMessage::Ping {
        seq: ping_seq,
        timestamp_micros,
    }
    .write_to_stream(&mut writer)
    .await
    {
        error!("Failed to send ping: {}", e);
        break;
    }

    ping_seq += 1;

    // 等待下一次心跳
    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
}
```

- [ ] **Step 7: 处理客户端收到的 Pong 消息**

在客户端的消息处理循环中，更新 Pong 处理：

```rust
ControlMessage::Pong { seq, ping_timestamp_micros, pong_timestamp_micros } => {
    // 客户端可以选择记录 RTT，但目前服务器端已经计算
    debug!("Received Pong seq={}", seq);
}
```

- [ ] **Step 8: 编译验证**

Run: `cargo check 2>&1 | head -50`
Expected: Compiles successfully

- [ ] **Step 9: 提交**

```bash
git add src/server/control.rs src/bin/client.rs
git commit -m "feat(server): integrate quality calculation into control connection"
```

---

## Task 4: 数据库 - 质量历史持久化

**Files:**
- Modify: `src/server/db.rs`

### 背景信息
需要在 SQLite 数据库中添加质量历史表，并实现数据的持久化和查询操作。

### 实现步骤

- [ ] **Step 1: 查看现有的数据库代码结构**

Run: `grep -n -A20 "impl Database" /root/projects/rust-tunnel/src/server/db.rs | head -60`

- [ ] **Step 2: 添加数据库迁移和表创建**

在 `Database::new` 或 `init` 方法中，添加新表的创建：

```sql
CREATE TABLE IF NOT EXISTS connection_quality_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    port INTEGER NOT NULL,
    timestamp TEXT NOT NULL,
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

将这段 SQL 添加到数据库初始化代码中，与其他表的创建并列。

- [ ] **Step 3: 添加质量历史插入方法**

在 `Database` impl 块中添加：

```rust
    /// 插入质量历史记录
    pub async fn insert_quality_history(
        &self,
        port: u16,
        timestamp: &DateTime<Utc>,
        avg_rtt_ms: f32,
        min_rtt_ms: f32,
        max_rtt_ms: f32,
        loss_rate: f32,
        bytes_in_per_sec: f64,
        bytes_out_per_sec: f64,
        quality_score: u8,
        is_warning: bool,
        is_critical: bool,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        sqlx::query(
            r#"
            INSERT INTO connection_quality_history (
                port, timestamp, avg_rtt_ms, min_rtt_ms, max_rtt_ms,
                loss_rate, bytes_in_per_sec, bytes_out_per_sec,
                quality_score, is_warning, is_critical
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(port as i32)
        .bind(timestamp.to_rfc3339())
        .bind(avg_rtt_ms)
        .bind(min_rtt_ms)
        .bind(max_rtt_ms)
        .bind(loss_rate)
        .bind(bytes_in_per_sec)
        .bind(bytes_out_per_sec)
        .bind(quality_score as i32)
        .bind(is_warning as i32)
        .bind(is_critical as i32)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// 查询端口的质量历史
    pub async fn get_quality_history(
        &self,
        port: u16,
        start_time: &DateTime<Utc>,
        end_time: &DateTime<Utc>,
    ) -> Result<Vec<QualitySample>, Box<dyn std::error::Error + Send + Sync>> {
        let rows = sqlx::query_as::<_, QualityHistoryRow>(
            r#"
            SELECT timestamp, avg_rtt_ms, loss_rate,
                   bytes_in_per_sec, bytes_out_per_sec, quality_score
            FROM connection_quality_history
            WHERE port = ? AND timestamp >= ? AND timestamp <= ?
            ORDER BY timestamp ASC
            "#,
        )
        .bind(port as i32)
        .bind(start_time.to_rfc3339())
        .bind(end_time.to_rfc3339())
        .fetch_all(&self.pool)
        .await?;

        let samples = rows
            .into_iter()
            .filter_map(|row| {
                DateTime::parse_from_rfc3339(&row.timestamp)
                    .ok()
                    .map(|dt| QualitySample {
                        timestamp: dt.with_timezone(&Utc),
                        avg_rtt_ms: row.avg_rtt_ms,
                        loss_rate: row.loss_rate,
                        bytes_in_per_sec: row.bytes_in_per_sec,
                        bytes_out_per_sec: row.bytes_out_per_sec,
                        quality_score: row.quality_score as u8,
                    })
            })
            .collect();

        Ok(samples)
    }

    /// 清理过期的质量历史数据
    pub async fn cleanup_old_quality_history(
        &self,
        older_than: &DateTime<Utc>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        sqlx::query(
            r#"
            DELETE FROM connection_quality_history
            WHERE timestamp < ?
            "#,
        )
        .bind(older_than.to_rfc3339())
        .execute(&self.pool)
        .await?;

        Ok(())
    }
}

// 数据库行映射结构体
#[derive(sqlx::FromRow)]
struct QualityHistoryRow {
    timestamp: String,
    avg_rtt_ms: f32,
    loss_rate: f32,
    bytes_in_per_sec: f64,
    bytes_out_per_sec: f64,
    quality_score: i32,
}
```

- [ ] **Step 4: 添加必要的 use 导入**

在 `src/server/db.rs` 顶部添加：

```rust
use crate::server::quality::QualitySample;
```

- [ ] **Step 5: 编译验证**

Run: `cargo check 2>&1 | head -50`
Expected: Compiles successfully

- [ ] **Step 6: 提交**

```bash
git add src/server/db.rs
git commit -m "feat(db): add connection quality history persistence"
```

---

## Task 5: API 端点 - 质量数据接口

**Files:**
- Modify: `src/server/api.rs`

### 背景信息
需要添加质量相关的 API 端点，并扩展现有 ClientResponse 以包含质量数据。

### 实现步骤

- [ ] **Step 1: 添加 API 响应结构体**

在 `src/server/api.rs` 中现有结构体后添加：

```rust
/// 带质量数据的客户端响应
#[derive(Debug, Serialize)]
pub struct ClientWithQuality {
    pub port: u16,
    pub hostname: Option<String>,
    pub quality: ConnectionQuality,
}

/// 单个端口的完整质量数据响应
#[derive(Debug, Serialize)]
pub struct PortQualityResponse {
    pub current: ConnectionQuality,
    pub history: Vec<QualitySample>,
}

/// 告警信息
#[derive(Debug, Serialize)]
pub struct QualityWarning {
    pub port: u16,
    pub hostname: Option<String>,
    pub quality: ConnectionQuality,
    pub warning_type: String,
}
```

- [ ] **Step 2: 添加必要的 use 导入**

```rust
use crate::server::quality::{QualityStore, ConnectionQuality, QualitySample};
```

- [ ] **Step 3: 添加 API 处理函数**

在现有处理函数后添加：

```rust
// 获取所有客户端的实时质量数据
async fn get_all_quality(State(state): State<ApiState>) -> Json<Vec<ClientWithQuality>> {
    let clients = state.server_state.get_all_clients().await;
    let mut result = Vec::with_capacity(clients.len());

    for (port, info) in clients {
        if let Some(quality) = state.server_state.quality_store.get_quality(port).await {
            result.push(ClientWithQuality {
                port,
                hostname: info.hostname,
                quality,
            });
        }
    }

    Json(result)
}

// 获取单个端口的质量数据
async fn get_port_quality(
    State(state): State<ApiState>,
    Path(port): Path<u16>,
) -> impl IntoResponse {
    let current = state.server_state.quality_store.get_quality(port).await;
    let history = state.server_state.quality_store.get_samples(port).await;

    match current {
        Some(current) => Json(PortQualityResponse { current, history }).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// 获取历史质量数据（可选时间范围参数）
async fn get_quality_history(
    State(state): State<ApiState>,
    Path(port): Path<u16>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    // 首先从内存获取最近1小时数据
    let samples = state.server_state.quality_store.get_samples(port).await;

    // 如果有数据库且请求更长时间范围，从数据库查询
    // 简化版本：只返回内存中的最近1小时数据
    Json(samples)
}

// 获取当前有质量问题的连接
async fn get_quality_warnings(State(state): State<ApiState>) -> Json<Vec<QualityWarning>> {
    let clients = state.server_state.get_all_clients().await;
    let mut warnings = Vec::new();

    for (port, info) in clients {
        if let Some(quality) = state.server_state.quality_store.get_quality(port).await {
            let warning_type = if quality.is_critical {
                Some("critical".to_string())
            } else if quality.is_warning {
                Some("warning".to_string())
            } else {
                None
            };

            if let Some(warning_type) = warning_type {
                warnings.push(QualityWarning {
                    port,
                    hostname: info.hostname,
                    quality,
                    warning_type,
                });
            }
        }
    }

    Json(warnings)
}
```

- [ ] **Step 4: 在 ClientResponse 中添加 quality 字段**

更新 `ClientResponse` 结构体：

```rust
/// 客户端 API 响应
#[derive(Debug, Serialize)]
pub struct ClientResponse {
    pub port: u16,
    pub hostname: Option<String>,
    pub connection_count: usize,
    pub quality: Option<ConnectionQuality>,  // 新增
}
```

- [ ] **Step 5: 更新 list_clients 函数以包含质量数据**

在 `list_clients` 函数中：

```rust
// 列出所有客户端
async fn list_clients(State(state): State<ApiState>) -> Json<Vec<ClientResponse>> {
    let clients = state.server_state.get_all_clients().await;
    let mut response = Vec::with_capacity(clients.len());
    for (port, info) in clients {
        let connection_count = state.server_state.get_connection_count_for_port(port).await;
        let quality = state.server_state.quality_store.get_quality(port).await;
        response.push(ClientResponse {
            port,
            hostname: info.hostname,
            connection_count,
            quality,
        });
    }
    Json(response)
}
```

- [ ] **Step 6: 在 run_api_server 中添加路由**

在创建 protected_routes 的地方添加：

```rust
// 受保护路由（仅当设置密码时需要认证）
let mut protected_routes = Router::new()
    .route("/api/logout", post(logout))
    .route("/api/clients", get(list_clients))
    .route("/api/clients/:port", delete(disconnect_client))
    .route("/api/traffic", get(get_traffic))
    .route("/api/traffic/:port", get(get_port_traffic))
    .route("/api/metrics", get(get_metrics))
    // 新增质量监控端点
    .route("/api/quality/all", get(get_all_quality))
    .route("/api/quality/:port", get(get_port_quality))
    .route("/api/quality/:port/history", get(get_quality_history))
    .route("/api/quality/warnings", get(get_quality_warnings));
```

- [ ] **Step 7: 添加必要的导入**

确保导入了 `Query` 和 `IntoResponse`：

```rust
use axum::extract::Query;
```

- [ ] **Step 8: 编译验证**

Run: `cargo check 2>&1 | head -80`
Expected: Compiles successfully

- [ ] **Step 9: 提交**

```bash
git add src/server/api.rs
git commit -m "feat(api): add quality monitoring API endpoints"
```

---

## Task 6: 前端类型和 API 客户端

**Files:**
- Create/Modify: `frontend/src/types/index.ts`
- Create/Modify: `frontend/src/api/client.ts`

### 背景信息
需要在前端定义质量相关的 TypeScript 类型，并添加 API 调用方法。

### 实现步骤

- [ ] **Step 1: 查看现有类型文件**

Run: `cat /root/projects/rust-tunnel/frontend/src/types/index.ts 2>/dev/null || echo "File does not exist"`

- [ ] **Step 2: 添加质量类型定义**

```typescript
// 连接质量数据
export interface ConnectionQuality {
  last_rtt_ms: number;
  avg_rtt_ms: number;
  min_rtt_ms: number;
  max_rtt_ms: number;
  loss_rate: number;
  consecutive_losses: number;
  bytes_in_per_sec: number;
  bytes_out_per_sec: number;
  quality_score: number;
  last_update: string;
  is_warning: boolean;
  is_critical: boolean;
}

// 历史质量样本
export interface QualitySample {
  timestamp: string;
  avg_rtt_ms: number;
  loss_rate: number;
  bytes_in_per_sec: number;
  bytes_out_per_sec: number;
  quality_score: number;
}

// 带质量数据的客户端
export interface ClientWithQuality {
  port: number;
  hostname: string | null;
  quality: ConnectionQuality;
}

// 端口质量响应
export interface PortQualityResponse {
  current: ConnectionQuality;
  history: QualitySample[];
}

// 质量告警
export interface QualityWarning {
  port: number;
  hostname: string | null;
  quality: ConnectionQuality;
  warning_type: string;
}

// 扩展现有 ClientResponse
export interface ClientResponse {
  port: number;
  hostname: string | null;
  connection_count: number;
  quality: ConnectionQuality | null;
}
```

- [ ] **Step 3: 查看现有 API 客户端文件**

Run: `cat /root/projects/rust-tunnel/frontend/src/api/client.ts 2>/dev/null || echo "File does not exist"`

- [ ] **Step 4: 添加质量 API 调用方法**

```typescript
import axios from 'axios';

const API_BASE = import.meta.env.VITE_API_URL || '';

export const api = {
  // ... 现有方法 ...

  // 获取所有客户端质量数据
  async getAllQuality(): Promise<ClientWithQuality[]> {
    const response = await axios.get(`${API_BASE}/api/quality/all`);
    return response.data;
  },

  // 获取单个端口质量数据
  async getPortQuality(port: number): Promise<PortQualityResponse> {
    const response = await axios.get(`${API_BASE}/api/quality/${port}`);
    return response.data;
  },

  // 获取历史质量数据
  async getQualityHistory(port: number): Promise<QualitySample[]> {
    const response = await axios.get(`${API_BASE}/api/quality/${port}/history`);
    return response.data;
  },

  // 获取质量告警
  async getQualityWarnings(): Promise<QualityWarning[]> {
    const response = await axios.get(`${API_BASE}/api/quality/warnings`);
    return response.data;
  },
};
```

- [ ] **Step 5: 提交**

```bash
cd frontend && git add src/types/index.ts src/api/client.ts
git commit -m "feat(frontend): add quality types and API clients"
```

---

## Task 7: 前端 - ClientList 质量列展示

**Files:**
- Modify: `frontend/src/components/ClientList.tsx`

### 背景信息
需要在客户端列表中添加质量展示列：质量评分（彩色）、延迟、丢包率和告警指示器。

### 实现步骤

由于前端组件代码较长，这里提供核心修改要点：

1. 导入类型和工具函数
2. 添加质量状态颜色映射函数
3. 在表格中添加质量相关列
4. 添加质量评分展示组件（彩色圆点 + 数字）
5. 添加告警闪烁效果

### 质量评分颜色映射：
```typescript
// 根据质量评分获取颜色
export const getQualityColor = (score: number): string => {
  if (score >= 90) return '#22c55e'; // 绿色
  if (score >= 70) return '#eab308'; // 黄色
  if (score >= 50) return '#f97316'; // 橙色
  return '#ef4444'; // 红色
};

// 根据质量评分获取文本描述
export const getQualityText = (score: number): string => {
  if (score >= 90) return '优秀';
  if (score >= 70) return '良好';
  if (score >= 50) return '一般';
  return '较差';
};
```

- [ ] **Step 1: 实现 ClientList 质量列**
- [ ] **Step 2: 构建验证**
- [ ] **Step 3: 提交**

---

## Task 8: 前端 - ClientDetail 质量详情模态框

**Files:**
- Modify: `frontend/src/components/ClientDetail.tsx`

### 背景信息
需要在客户端详情模态框中添加质量详情标签页，包含实时仪表盘和历史图表。

### 实现要点：
1. 添加质量标签页切换
2. 实现圆形质量仪表盘（展示 0-100 分）
3. 使用 Recharts 添加 RTT 趋势折线图
4. 添加丢包率趋势图
5. 添加吞吐量趋势图
6. 实现告警历史列表

- [ ] **Step 1: 添加图表组件和仪表盘**
- [ ] **Step 2: 构建验证**
- [ ] **Step 3: 提交**

---

## Task 9: 前端 - 独立质量监控页面

**Files:**
- Create: `frontend/src/components/QualityPage.tsx`
- Modify: `frontend/src/App.tsx`

### 背景信息
需要创建独立的质量监控页面，展示全局概览、质量热力图、历史趋势和最差连接排行。

### 实现要点：
1. 全局概览卡片（平均质量、客户端数、告警数）
2. 按主机名分组的质量热力图
3. 24小时/7天/30天历史趋势图表
4. 最差连接排行榜（TOP 10）
5. 添加路由配置

- [ ] **Step 1: 创建 QualityPage 组件**
- [ ] **Step 2: 添加路由到 App.tsx**
- [ ] **Step 3: 构建验证**
- [ ] **Step 4: 提交**

---

## Task 10: 集成测试和完整验证

**Files:**
- Various

### 实现步骤

- [ ] **Step 1: 完整构建后端**
  Run: `cargo build --release 2>&1 | tail -30`

- [ ] **Step 2: 完整构建前端**
  Run: `cd frontend && npm run build 2>&1 | tail -30`

- [ ] **Step 3: 运行所有后端测试**
  Run: `cargo test 2>&1 | tail -50`

- [ ] **Step 4: 手动验证场景**
  - 启动服务器和客户端
  - 验证 API 端点返回正确数据
  - 验证前端展示正常
  - 验证客户端断开后数据正确清理

- [ ] **Step 5: 最终提交**

---

## 注意事项

1. **向后兼容性**: 旧版客户端（发送无字段 Ping）不会崩溃，但质量数据会显示为 "N/A"
2. **性能**: 质量计算在每个心跳时执行，约每5秒一次，开销很小
3. **数据库**: 质量历史数据持久化是异步的，不影响控制通道性能
4. **时钟同步**: RTT 计算可能受客户端和服务器时钟不同步影响，但滑动平均可以减轻影响
