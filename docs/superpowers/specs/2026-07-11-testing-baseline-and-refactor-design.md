# 集成测试基线与后端模块重构 设计文档

- 日期：2026-07-11
- 作者：brainstorming session (与用户共议)
- 状态：待实施
- 相关文件：`src/server/api.rs`、`src/server/control.rs`、`src/server/db.rs`

## 背景与动机

后端已经积累了一批"大文件 + 长函数"债务：

| 文件 | 行数 | 问题 |
|---|---|---|
| `src/server/api.rs` | 1675 | 30+ 结构体、40+ handler、路由/DTO/中间件混杂 |
| `src/server/control.rs` | 1625 | `handle_control_connection` 单函数 ≈ 800 行 |
| `src/server/db.rs` | 1515 | 5 张表的 CRUD、迁移、清理任务全部混在一起 |

同时项目**没有 `tests/` 集成测试目录**，只有分散在 `src/` 内的单元测试。隧道建立、重连、心跳、多端口、SS/Trojan、API+SSE 等关键行为均缺少端到端回归网。这意味着任何非平凡的重构都是"盲改"。

本次工作的核心命题是：**先建集成测试基线封盘，再按域拆分三大模块，全程零功能变更、零协议变更、零数据库 schema 变更。**

## 目标与非目标

### 目标
1. 在 `tests/` 下建立覆盖隧道/重连/心跳/SS/Trojan/API/SSE 的内进程集成测试套件，并接入 CI。
2. 将 `db.rs`、`control.rs`、`api.rs` 按域拆分为多文件模块，单文件 ≤ 400 行。
3. 保持所有对外契约不变：控制协议、REST API、数据库 schema、CLI 参数、TOML 配置。

### 非目标（明确不做）
- 性能优化（`copy_bidirectional`、DB 批写、零拷贝等）。
- 新功能（UDP、mesh 落地、多用户、hot reload）。
- 前端改动。
- 数据库 schema 变更或新迁移。
- 依赖大版本升级。

任何在实施过程中冒出的"顺手改"想法，一律另开 follow-up spec，不塞进本次 PR。

## 整体路线图

分两个阶段，5 个 PR：

| PR | 内容 | 大小估算 | 依赖 |
|---|---|---|---|
| PR-1 | 阶段 1：`tests/common/` harness + 隧道+API+SSE 用例 + CI job | ~1500 行（新增） | — |
| PR-1.5 | SS/Trojan 集成用例 | ~600 行（新增） | PR-1 |
| PR-2 | 阶段 2.1：`db.rs` 拆分 | ~1500 行搬迁 | PR-1 |
| PR-3 | 阶段 2.2：`control.rs` 拆分 | ~1600 行搬迁 | PR-2 |
| PR-4 | 阶段 2.3：`api.rs` 拆分 | ~1700 行搬迁 | PR-3 |

顺序 `db → control → api` 的理由：`db` 被依赖最深，先拆它后续每一步 diff 更干净；`api` 是最外层，最后拆时集成测试已经能守住 REST 契约。

---

## 阶段 1：集成测试基线

### 依赖变更
在 `Cargo.toml` `[dev-dependencies]` 中新增：
- `reqwest`（features: `json`, `stream`, `rustls-tls`）
- `tempfile`
- `futures-util`（读 SSE 流）
- `serde_json`

不新增任何 crate feature，不改主依赖。

### 入口暴露决议
集成测试需要在同进程内调用服务器与客户端入口。经确认，当前已有：
- `rust_tunnel::server::run_server(config, state) -> TunnelResult<()>` (src/server/control.rs:1568)
- `rust_tunnel::client::run_client(config, forwards) -> TunnelResult<()>` (src/client/control.rs:292)

两者均为 `pub`，本阶段**无需**改动任何函数可见性。若后续 harness 需要构造某个非 `pub` 的辅助结构（例如 `ServerState::new_for_test`），允许最小化增加 `pub(crate)` 或 `#[cfg(test)]` 辅助函数，但不扩大公开面。

### Harness 设计

**`tests/common/mod.rs` — `TestHarness`**

```
struct TestHarness {
    tempdir: TempDir,
    control_addr: SocketAddr,
    api_base: String,             // e.g. http://127.0.0.1:41321
    exposed_ports: Vec<u16>,      // 随机分配
    server_task: JoinHandle<()>,
    client_tasks: Vec<JoinHandle<()>>,
}

struct HarnessOpts {
    tls: bool,
    admin_password: Option<String>,
    client_auth_token: Option<String>,
    shadowsocks: Option<SsConfig>,
    trojan: Option<TrojanConfig>,
    exposed_port_count: usize,
}

impl TestHarness {
    async fn spawn(opts: HarnessOpts) -> Self { ... }
    async fn spawn_client(&mut self, forwards: Vec<Forward>) { ... }
    async fn wait_client_registered(&self, client_id: &str) -> Result<()> { ... }
    fn api(&self) -> ApiClient { ... }    // 封装带 JWT 的 reqwest
}

impl Drop for TestHarness {
    fn drop(&mut self) { /* abort tasks, tempdir 自动清理 */ }
}
```

**端口分配策略**：每次 `TcpListener::bind("127.0.0.1:0")` 拿到内核分配端口，立刻 `drop` 释放，读到的 `SocketAddr` 传给配置——避免测试之间端口撞车。

**健康检查**：`spawn` 结束前必须轮询 `GET /api/health` 直到 200 OK，指数退避 `Retry { max: 50, base: 20ms }`。

**echo 后端 — `tests/common/echo.rs`**
- `spawn_echo() -> SocketAddr`：最简 TCP echo，供隧道用。
- `spawn_http_echo() -> SocketAddr`：返回固定 200 + body 的 HTTP mini-server，供 SS/Trojan 用。

### 测试文件与用例清单

**PR-1 范围**（隧道 + API + SSE）：

`tests/tunnel_basic.rs`
- `tunnel_forwards_bytes_bidirectionally`：客户端注册 → 从公网端口发 128KB 随机数据 → echo → 断言字节一致。
- `tunnel_forwards_with_tls_disabled`：`tls=false` 分支。
- `tunnel_multi_port`：一个客户端注册 2 个暴露端口，并发流量互不干扰。

`tests/tunnel_reconnect.rs`
- `client_reconnects_after_forced_drop`：客户端 abort → 新客户端重连 → 再走流量成功。
- `heartbeat_measures_rtt`：等 3 个心跳周期，`GET /api/quality/ports/...` 返回 `rtt_ms > 0`。
- `server_restart_survives_reregistration`：重启 server → 客户端自动重连成功。

`tests/api_auth.rs`
- `login_returns_jwt`
- `protected_route_requires_bearer`
- `wrong_password_401`
- `no_admin_password_disables_auth`（未设密码时全开放，保留现有行为）

`tests/api_sse.rs`
- `sse_streams_log_entries`：订阅 SSE → 触发一条 warn 日志 → 从流里读到对应事件。
- `traffic_bucket_appears_after_transfer`：跑一波流量 → 轮询 `/api/traffic/ports/:port` 看到非零字节。

**PR-1.5 范围**（SS/Trojan）：

`tests/proxy_shadowsocks.rs`
- `ss_accepts_valid_client`：用 `shadowsocks` crate 客户端拨号，走通 http_echo。
- `ss_rejects_wrong_password`：错误密码握手失败但不 panic 服务器。

`tests/proxy_trojan.rs`
- `trojan_accepts_valid_hash`：手写最小 Trojan 帧（SHA-224(password) + CRLF + SOCKS-like 地址 + payload）走通 http_echo。
- `trojan_falls_back_on_bad_auth`：错误密码触发回退路径不 panic。

### 通用约束
- 每个 `#[tokio::test]` 用 `tokio::time::timeout(Duration::from_secs(15), ...)` 兜底。
- 禁止 `sleep(2s)` 式硬等——所有等待用 `wait_until` 指数退避。
- 断言"客户端已注册"必须走 `/api/clients`，不能靠 sleep。
- 每个测试独立 `TempDir`、独立端口，可并行执行；如 CI 撞车再降级为 `#[serial]`。

### CI 接入
现有 `.github/workflows/release-server.yml` 只有 build + deploy，无 test gate。本次新增 `.github/workflows/ci.yml`：
- 触发：`push` 到任意分支、`pull_request` 到 `main`。
- Job：`cargo fmt --check` → `cargo clippy -- -D warnings` → `cargo test --tests`（15 分钟超时）。
- 缓存：`actions/cache` 缓存 `~/.cargo` 与 `target/`。

### 验收
- `cargo test --tests` 全绿；
- 本地 3 连跑无 flake；
- CI 上跑通；
- 新建 `tests/README.md` 说明 harness 使用与加新用例的模板。

---

## 阶段 2：三大模块重构

每个 PR 合入前必须先跑通阶段 1 的完整集成测试。任何测试红→PR 必须回退或修正。

### 2.1 `db.rs` → `server/db/`

**目标结构**：
```
src/server/db/
  mod.rs                 // Pool 构建、迁移入口、pub use repositories::*
  migrations.rs          // 现有 CREATE TABLE / ALTER 语句集中
  repositories/
    mod.rs
    traffic.rs           // port_traffic + traffic_buckets
    quality.rs           // connection_quality_history
    sessions.rs          // client_sessions
    logs.rs              // log_entries
    proxy_config.rs      // shadowsocks_config + trojan_config
  retention.rs           // 定时清理任务
```

**做法**：
- `Database` struct 保留在 `mod.rs`，字段不变。方法按表拆到对应 repository：`db.record_traffic(...)` → `db.traffic().record(...)`。为兼容旧调用点，保留一层薄的 delegation 方法（`impl Database { pub fn record_traffic(&self, ...) { self.traffic().record(...) } }`）。
- 迁移 SQL 从各个 `init_*` 函数集中到 `migrations.rs::apply()`，一次性顺序执行；不引入 `sqlx-migrate`，仍是纯字符串。
- 数据结构（`PortTraffic`, `QualityHistoryEntry` 等）就近搬到对应 repository 文件；用 `pub use` 从 `db::mod.rs` 重新导出。
- **零 schema 变更**：任何 `ALTER TABLE` 或新 CREATE TABLE 一律不允许。
- 清理任务从 `db.rs` 抽到 `retention.rs`，`spawn_retention_task(db: Arc<Database>)`。

**验收**：
- `cargo test --tests` 全绿；
- `checkdb` 二进制无需修改仍可编译；
- 单文件不超过 400 行（除 `mod.rs`）。

### 2.2 `control.rs` → `server/control/`

**目标结构**：
```
src/server/control/
  mod.rs                 // pub use, run_server 入口保持不变
  state.rs               // ServerState, ClientInfo, PortInfo
  session.rs             // handle_control_connection 的 orchestration（<150 行）
  handshake.rs           // TLS accept + Auth 校验
  register.rs            // RegisterPorts / UnregisterPorts / 冲突检测
  frames.rs              // 主循环：读帧 → dispatch
  heartbeat.rs           // Ping/Pong、RTT 采样、丢包序号
  runtime.rs             // run_server: bind + accept loop
```

**做法**：
- `handle_control_connection` 现在 800 行是因为握手 + 认证 + 注册 + 主循环 + 心跳 + 清理串在一起。拆法：
  ```rust
  async fn session(stream, state) -> Result<()> {
      let mut conn = handshake::perform(stream, &state).await?;
      let _guard = state.register_client(&conn).await?;   // Drop 时反注册
      frames::pump(&mut conn, state).await
  }
  ```
- `frames::pump` 是一个 `select!`：读帧 / 心跳 tick / 关停信号。每种帧调用对应模块的 pure async 函数，签名不吃 `&mut self`，只吃 `&Arc<ServerState>` + 消息。
- `ServerState` 字段暂不再拆（`Arc<Mutex<HashMap<...>>>` 太多是另一个题目）；只把方法按行为分组：注册 / 流量统计 / 心跳。
- **协议零变更**：`ControlMessage` 枚举留在 `common/protocol.rs`，variant 不重命名。
- 移除若干 `#[allow(clippy::too_many_lines)]` / `#[allow(clippy::pedantic)]`——拆分后自然消解。

**风险与缓解**：
- `Drop`-guard 反注册易错：用 `RegistrationGuard` struct 确保 panic 也能清理。集成测试里的 `client_reconnects_after_forced_drop` 是守门员。
- `select!` 里的 borrow 交错难调：如编译器抱怨生命周期，允许把中间态放进一个 `SessionContext` struct 承载。

**验收**：
- 单文件 ≤ 400 行；
- 全套集成测试通过；
- `cargo clippy -- -D warnings` 在 pedantic 下无新增 warning。

### 2.3 `api.rs` → `server/api/`

**目标结构**：
```
src/server/api/
  mod.rs                 // ApiState, build_router(), run_api
  state.rs               // ApiState + TrafficStore
  auth.rs                // AuthLayer（JWT 提取器）+ login/logout handler
  static_assets.rs       // FrontendAssets embed + serve_static
  routes/
    mod.rs
    clients.rs
    traffic.rs
    metrics.rs
    quality.rs
    shadowsocks.rs
    trojan.rs
    mesh.rs
    dns.rs
    logs.rs              // /api/logs + SSE
  dto.rs                 // response/query 结构集中
```

**做法**：
- `build_router()` 在 `mod.rs`：
  ```rust
  Router::new()
      .merge(routes::clients::router())
      .merge(routes::traffic::router())
      .merge(routes::quality::router())
      // ...
      .route_layer(middleware::from_fn_with_state(state.clone(), auth::require_auth_if_password_set))
      .with_state(state)
  ```
  每个 `routes/xxx.rs` 只暴露一个 `pub fn router() -> Router<ApiState>`。
- Handler 保持薄：只做参数解析 + 调 `state.control.xxx()` / `state.db.xxx()` + 组装 response。**禁止**在 handler 里写 SQL 或直接改 `HashMap`。
- `dto.rs` 汇总共享的 30+ response/query 结构。仅被单个路由使用的 struct 允许留在路由文件里。
- 认证中间件从零散的 `if state.admin_password.is_some()` 判断收敛到一个 `AuthLayer`，路由用 `route_layer` 应用；`/api/login`、`/api/health` 不套。
- **API 契约零变更**：路径、方法、请求体、响应字段一律不动。阶段 1 的 `api_auth.rs` / `api_sse.rs` 是守门员。

**验收**：
- 单文件 ≤ 400 行；
- 阶段 1 集成测试增补 golden-file 断言，锁定 `/api/metrics`、`/api/clients`、`/api/traffic` 响应 JSON。

---

## 风险登记

| 风险 | 概率 | 影响 | 缓解 |
|---|---|---|---|
| 集成测试端口撞车导致 CI flake | 中 | 中 | 每 test 独立 `bind(:0)`；如仍 flake 加 `#[serial]` |
| `control.rs` 拆分破坏 `Drop`-guard 反注册顺序 | 中 | 高 | `client_reconnects_after_forced_drop` 用例作为守门员；`RegistrationGuard` 显式化 |
| `api.rs` 拆分意外改变响应 JSON 字段 | 中 | 高 | golden-file 断言；重构前后 `curl` diff |
| 迁移 SQL 集中执行顺序错误 | 低 | 高 | 沿用现有顺序；启动时用 `PRAGMA schema_version` 冒烟检查（可选） |
| Windows CI 上 `TcpListener::bind(:0)` 行为差异 | 低 | 低 | CI 目前只 build Linux；本次 CI 只跑 Linux |

## 开放决策（已拍板）

- ✅ 存量优化优先，不做新功能。
- ✅ 优先解决"大文件/长函数"+ "集成测试缺失"两项。
- ✅ 先测试封盘，后重构。
- ✅ 集成测试形态：内进程 tokio 驱动。
- ✅ 测试基线覆盖：隧道/重连/心跳/SS/Trojan/API/SSE。
- ✅ 拆分顺序：`db → control → api`。
- ✅ 每个模块拆分独立 PR。
- ✅ 零"顺手改"——所有额外优化想法进 follow-up spec。

## Follow-up 候选（不在本次 spec 内）

以下想法在 brainstorming 中被识别但明确排除，留待后续独立 spec：

- 可观测性：Prometheus `/metrics` + OpenTelemetry + JSON 结构化日志导出。
- 热路径性能：`copy_bidirectional`、DB 批写、`Bytes` 零拷贝改造。
- 数据库保留策略与 VACUUM。
- 认证/安全加固：登录限流、JWT 吊销名单、证书轮换。
- 构建/发布：多架构、Docker 镜像、release checksum。
- Mesh/DNS 模块正式落地（骨架已存在于 `src/server/mesh/`、`src/server/dns/`）。
- UDP 转发、HTTP 反代、Webhook 告警、多用户/多租户。
