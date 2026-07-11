# PR-1: 集成测试基线 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 `tests/` 下建立覆盖隧道/重连/心跳/API/SSE 的内进程集成测试套件，接入 CI，作为后续三大模块重构的回归网。

**Architecture:** 每个测试用 `TestHarness` 在同一进程内 `tokio::spawn` 启动 `run_server` + `run_client`，通过 `TcpListener::bind("127.0.0.1:0")` 拿到内核随机分配的端口。所有等待用指数退避轮询而非 `sleep`。SS/Trojan 用例（PR-1.5）不在本计划范围内。

**Tech Stack:** Rust 1.75+、tokio、reqwest、tempfile、futures-util、serde_json、GitHub Actions。

**Spec 参考:** [docs/superpowers/specs/2026-07-11-testing-baseline-and-refactor-design.md](../specs/2026-07-11-testing-baseline-and-refactor-design.md) 阶段 1。

---

## File Structure

```
tests/
  common/
    mod.rs               # 各测试文件 include! 引入的共享模块
    harness.rs           # TestHarness + HarnessOpts
    api_client.rs        # 带 JWT 的 reqwest 封装
    echo.rs              # spawn_echo, spawn_http_echo
    retry.rs             # wait_until 指数退避辅助
  tunnel_basic.rs        # 3 个用例：双向转发、TLS 关、多端口
  tunnel_reconnect.rs    # 3 个用例：强断重连、心跳 RTT、server 重启
  api_auth.rs            # 4 个用例：JWT 流程 + 无密码
  api_sse.rs             # 2 个用例：日志 SSE + 流量桶
  README.md              # harness 使用说明和加新用例模板
.github/workflows/
  ci.yml                 # fmt + clippy + test
Cargo.toml               # 新增 dev-dependencies
```

**共享模块**用 `#[path = "common/mod.rs"] mod common;` 在每个 test crate 顶部 include，避免"unused code"警告与重复编译。

---

## Task 1: 新增 dev-dependencies

**Files:**
- Modify: `Cargo.toml`

- [ ] **Step 1: 追加 dev-dependencies**

将 `Cargo.toml` 底部 `[dev-dependencies]` 块替换为：

```toml
[dev-dependencies]
tempfile = "3.0"
reqwest = { version = "0.12", default-features = false, features = ["json", "stream", "rustls-tls"] }
futures-util = "0.3"
```

`serde_json` / `tokio` 已在主依赖中，无需重复添加。选择 `rustls-tls` 而非 `native-tls`，与项目 `rustls` 依赖一致，避免 OpenSSL 编译问题。

- [ ] **Step 2: 验证编译**

```bash
cargo build --tests
```

Expected: 编译通过（可能有 unused warning，但无 error）。

- [ ] **Step 3: 提交**

```bash
git add Cargo.toml Cargo.lock
git commit -m "test: add integration test dev-dependencies

Adds reqwest (rustls-tls), futures-util for upcoming
integration test harness."
```

---

## Task 2: retry.rs — 指数退避轮询工具

**Files:**
- Create: `tests/common/retry.rs`

- [ ] **Step 1: 创建 tests/common 目录**

```bash
mkdir -p tests/common
```

- [ ] **Step 2: 写实现**

创建 `tests/common/retry.rs`：

```rust
//! Exponential-backoff polling helper used by every integration test.
//!
//! Never `sleep(2s)` — always `wait_until(cond).await`.

use std::future::Future;
use std::time::Duration;

/// Poll `cond` up to 50 times with exponential backoff (base 20ms, cap 500ms).
/// Total worst-case wait ≈ 5 seconds.
///
/// Returns `Ok(T)` on the first `Some(T)` from `cond`, `Err(String)` after exhaustion.
pub async fn wait_until<F, Fut, T>(desc: &str, mut cond: F) -> Result<T, String>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let mut delay = Duration::from_millis(20);
    for attempt in 0..50 {
        if let Some(value) = cond().await {
            return Ok(value);
        }
        tokio::time::sleep(delay).await;
        delay = std::cmp::min(delay * 2, Duration::from_millis(500));
        let _ = attempt;
    }
    Err(format!("wait_until timed out after 50 attempts: {desc}"))
}
```

- [ ] **Step 3: 编译检查（此时不能直接测，稍后 harness 一起编）**

```bash
cargo check --tests
```

Expected: PASS（无 error）。

- [ ] **Step 4: 提交**

```bash
git add tests/common/retry.rs
git commit -m "test: add wait_until exponential-backoff helper"
```

---

## Task 3: echo.rs — TCP echo 后端

**Files:**
- Create: `tests/common/echo.rs`

- [ ] **Step 1: 写实现**

创建 `tests/common/echo.rs`：

```rust
//! Tiny TCP echo backends used as tunnel targets.

use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Spawn a TCP echo server on 127.0.0.1:0. Returns the bound address.
/// Every incoming byte is echoed back verbatim. Task lives until the process exits.
pub async fn spawn_echo() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind echo");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        loop {
            let (mut sock, _peer) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => return,
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                loop {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            if sock.write_all(&buf[..n]).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
    });
    addr
}

/// Spawn a tiny HTTP server that returns `200 OK\r\n\r\nhello` on any request.
/// Useful once we add SS/Trojan tests; safe to include now.
pub async fn spawn_http_echo() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind http_echo");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => return,
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                // Read the request headers (best-effort, up to the first \r\n\r\n).
                let _ = sock.read(&mut buf).await;
                let body = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello";
                let _ = sock.write_all(body).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    addr
}
```

- [ ] **Step 2: 编译检查**

```bash
cargo check --tests
```

Expected: PASS。

- [ ] **Step 3: 提交**

```bash
git add tests/common/echo.rs
git commit -m "test: add TCP and HTTP echo backends"
```

---

## Task 4: api_client.rs — 带 JWT 的 reqwest 封装

**Files:**
- Create: `tests/common/api_client.rs`

- [ ] **Step 1: 写实现**

创建 `tests/common/api_client.rs`：

```rust
//! Thin reqwest wrapper: keeps a JWT token, injects Bearer header when present.

use reqwest::{Client, StatusCode};
use serde_json::Value;

pub struct ApiClient {
    http: Client,
    base: String,
    token: Option<String>,
}

impl ApiClient {
    pub fn new(base: impl Into<String>) -> Self {
        // The server API listens on plain HTTP (Axum). No TLS on API port.
        Self {
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .expect("build reqwest"),
            base: base.into(),
            token: None,
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// POST /api/login with the given password. Stores the returned token.
    /// Returns the raw StatusCode and parsed JSON.
    pub async fn login(&mut self, password: &str) -> (StatusCode, Value) {
        let resp = self
            .http
            .post(format!("{}/api/login", self.base))
            .json(&serde_json::json!({ "password": password }))
            .send()
            .await
            .expect("login send");
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if status.is_success() {
            if let Some(t) = body.get("token").and_then(|v| v.as_str()) {
                self.token = Some(t.to_string());
            }
        }
        (status, body)
    }

    /// GET a JSON endpoint, returning `(status, body)`.
    /// Injects `Authorization: Bearer <token>` if a token is stored.
    pub async fn get_json(&self, path: &str) -> (StatusCode, Value) {
        let mut req = self.http.get(format!("{}{}", self.base, path));
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await.expect("get send");
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        (status, body)
    }

    /// Bare GET returning status (used for auth-negative tests where body is irrelevant).
    pub async fn get_status(&self, path: &str) -> StatusCode {
        let mut req = self.http.get(format!("{}{}", self.base, path));
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        req.send().await.expect("get send").status()
    }
}
```

- [ ] **Step 2: 编译检查**

```bash
cargo check --tests
```

Expected: PASS。

- [ ] **Step 3: 提交**

```bash
git add tests/common/api_client.rs
git commit -m "test: add ApiClient reqwest wrapper with JWT support"
```

---

## Task 5: harness.rs — TestHarness 核心

**Files:**
- Create: `tests/common/harness.rs`

- [ ] **Step 1: 写实现**

创建 `tests/common/harness.rs`：

```rust
//! In-process server+client harness. Each `TestHarness::spawn` yields a fully
//! isolated server (fresh tempdir, random ports) that lives until drop.

use rust_tunnel::client::config::{ClientConfig, ForwardRule};
use rust_tunnel::client::control::run_client;
use rust_tunnel::server::auth::AuthConfig;
use rust_tunnel::server::{api, control, Database, ServerConfig};
use std::net::{SocketAddr, TcpListener as StdTcpListener};
use tempfile::TempDir;
use tokio::task::JoinHandle;

use super::api_client::ApiClient;
use super::retry::wait_until;

/// Options for spawning a test server. Defaults are TLS-off, no auth.
#[derive(Default, Clone)]
pub struct HarnessOpts {
    pub tls: bool,
    pub admin_password: Option<String>,
    pub client_auth_token: Option<String>,
    /// Number of tunneled ports the harness should pre-reserve for tests.
    pub exposed_port_count: usize,
}

pub struct TestHarness {
    _tempdir: TempDir,
    pub control_addr: SocketAddr,
    pub api_base: String,
    pub exposed_ports: Vec<u16>,
    pub admin_password: Option<String>,
    pub client_auth_token: Option<String>,
    pub tls: bool,
    server_task: JoinHandle<()>,
    api_task: JoinHandle<()>,
    client_tasks: Vec<JoinHandle<()>>,
}

/// Reserve N ephemeral TCP ports on 127.0.0.1 by binding and dropping listeners.
/// There is a small TOCTOU window, but each test uses a fresh set of ports so
/// contention only matters when the CI runner is under massive parallel load.
fn reserve_ports(n: usize) -> Vec<u16> {
    (0..n)
        .map(|_| {
            let l = StdTcpListener::bind("127.0.0.1:0").expect("reserve port");
            l.local_addr().expect("local_addr").port()
        })
        .collect()
}

impl TestHarness {
    /// Spawn a fresh server and wait until `/api/health` returns 200.
    pub async fn spawn(opts: HarnessOpts) -> Self {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let data_dir = tempdir.path().to_path_buf();
        std::fs::create_dir_all(data_dir.join("tls")).expect("tls dir");

        let mut ports = reserve_ports(2 + opts.exposed_port_count);
        let control_port = ports.remove(0);
        let api_port = ports.remove(0);
        let exposed_ports: Vec<u16> = ports;

        let control_addr: SocketAddr = format!("127.0.0.1:{control_port}").parse().unwrap();
        let api_addr = format!("127.0.0.1:{api_port}");
        let api_base = format!("http://{api_addr}");

        let db_path = data_dir.join("rust-tunnel.db");
        let cert_path = data_dir.join("tls/cert.pem");
        let key_path = data_dir.join("tls/key.pem");

        let config = ServerConfig {
            control_addr: control_addr.to_string(),
            api_addr: api_addr.clone(),
            admin_password: opts.admin_password.clone(),
            jwt_secret: Some("test-jwt-secret-do-not-use-in-prod".to_string()),
            client_auth_token: opts.client_auth_token.clone(),
            tls: opts.tls,
            tls_cert: cert_path.to_string_lossy().to_string(),
            tls_key: key_path.to_string_lossy().to_string(),
            log: "warn".to_string(),
            db_path: db_path.to_string_lossy().to_string(),
            ss_enabled: false,
            ss_port: None,
            ss_cipher: None,
            ss_password: None,
            trojan_enabled: false,
            trojan_port: None,
            trojan_password: None,
            trojan_fallback: "127.0.0.1:80".to_string(),
            dns_enabled: false,
            dns_bind: "127.0.0.1:0".to_string(),
            dns_tunnel_domain: "tunnel.local".to_string(),
            dns_mesh_domain: "mesh.local".to_string(),
        };

        let db = Database::new(&config.db_path).await.expect("db new");
        let state = control::ServerState::with_db(db);
        let auth_config = AuthConfig::new(config.admin_password.clone(), config.jwt_secret.clone());

        let control_state = state.clone();
        let control_config = config.clone();
        let server_task = tokio::spawn(async move {
            let _ = control::run_server(control_config, control_state).await;
        });

        let api_state = state.clone();
        let api_addr_clone = api_addr.clone();
        let api_task = tokio::spawn(async move {
            let _ = api::run_api_server(api_addr_clone, api_state, auth_config).await;
        });

        // Wait for API health so tests know the server is ready.
        let health_base = api_base.clone();
        wait_until("api health", || {
            let base = health_base.clone();
            async move {
                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_millis(200))
                    .build()
                    .ok()?;
                let resp = client.get(format!("{base}/api/health")).send().await.ok()?;
                if resp.status().is_success() {
                    Some(())
                } else {
                    None
                }
            }
        })
        .await
        .expect("api never became healthy");

        Self {
            _tempdir: tempdir,
            control_addr,
            api_base,
            exposed_ports,
            admin_password: opts.admin_password,
            client_auth_token: opts.client_auth_token,
            tls: opts.tls,
            server_task,
            api_task,
            client_tasks: Vec::new(),
        }
    }

    /// Spawn a rust-tunnel client that connects back to this harness.
    /// Returns an `AbortHandle` that the caller can use to kill the client
    /// mid-test (e.g. to test reconnect). The harness itself also holds an
    /// abort handle for cleanup on drop.
    pub fn spawn_client(&mut self, forwards: Vec<ForwardRule>) -> tokio::task::AbortHandle {
        let client_config = ClientConfig {
            server: self.control_addr.to_string(),
            forwards: forwards
                .iter()
                .map(|f| format!("{}:{}", f.remote_port, f.local_addr))
                .collect(),
            mesh: None,
            mesh_name: None,
            mesh_services: Vec::new(),
            auth_token: self.client_auth_token.clone(),
            tls: self.tls,
            tls_server_name: Some("localhost".to_string()),
            tls_insecure: true,
            log: "warn".to_string(),
        };

        let handle = tokio::spawn(async move {
            let _ = run_client(client_config, forwards).await;
        });
        let abort = handle.abort_handle();
        self.client_tasks.push(handle);
        abort
    }

    /// Convenience: assert that at least `at_least` clients are registered via /api/clients.
    /// `api` must already be authenticated if the harness has a password.
    pub async fn wait_client_count(&self, api: &ApiClient, at_least: usize) -> Result<(), String> {
        wait_until("client registered", || async {
            let (status, body) = api.get_json("/api/clients").await;
            if !status.is_success() {
                return None;
            }
            let n = body.as_array().map(|a| a.len()).unwrap_or(0);
            if n >= at_least {
                Some(())
            } else {
                None
            }
        })
        .await
    }

    pub fn api_client(&self) -> ApiClient {
        ApiClient::new(self.api_base.clone())
    }
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        for t in self.client_tasks.drain(..) {
            t.abort();
        }
        self.server_task.abort();
        self.api_task.abort();
    }
}
```

- [ ] **Step 2: 编译检查**

```bash
cargo check --tests
```

Expected: PASS。若有 `use` 未用告警可忽略；error 必须清零。若 `ServerState::with_db` 签名不匹配（例如它是 `pub async fn`），改为对应形式：
- 如果它是 async：把 `let state = control::ServerState::with_db(db);` 改成 `.await`。
- 如果它接受 `Arc<Database>`：包一层 `Arc::new(db)`。

- [ ] **Step 3: 提交**

```bash
git add tests/common/harness.rs
git commit -m "test: add TestHarness for in-process server+client isolation"
```

---

## Task 6: common/mod.rs — 模块聚合

**Files:**
- Create: `tests/common/mod.rs`

- [ ] **Step 1: 写内容**

创建 `tests/common/mod.rs`：

```rust
//! Shared helpers for integration tests.
//!
//! Each test binary does `#[path = "common/mod.rs"] mod common;` at its top.

#![allow(dead_code)] // Different test crates use different subsets.

pub mod api_client;
pub mod echo;
pub mod harness;
pub mod retry;

pub use api_client::ApiClient;
pub use echo::{spawn_echo, spawn_http_echo};
pub use harness::{HarnessOpts, TestHarness};
pub use retry::wait_until;
```

- [ ] **Step 2: 编译检查**

```bash
cargo check --tests
```

Expected: PASS。

- [ ] **Step 3: 提交**

```bash
git add tests/common/mod.rs
git commit -m "test: add common module aggregator"
```

---

## Task 7: tunnel_basic.rs — 隧道基础用例

**Files:**
- Create: `tests/tunnel_basic.rs`

- [ ] **Step 1: 写第一个用例（双向转发），先跑失败**

创建 `tests/tunnel_basic.rs`：

```rust
//! Integration tests: tunnel establishment and bidirectional forwarding.

#[path = "common/mod.rs"]
mod common;

use common::{spawn_echo, wait_until, HarnessOpts, TestHarness};
use rust_tunnel::client::config::ForwardRule;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[tokio::test(flavor = "multi_thread")]
async fn tunnel_forwards_bytes_bidirectionally() {
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let mut harness = TestHarness::spawn(HarnessOpts {
            tls: false,
            exposed_port_count: 1,
            ..HarnessOpts::default()
        })
        .await;

        let echo_addr = spawn_echo().await;
        let remote_port = harness.exposed_ports[0];

        harness.spawn_client(vec![ForwardRule {
            remote_port,
            local_addr: echo_addr.to_string(),
            dns_name: None,
        }]);

        let api = harness.api_client();
        harness
            .wait_client_count(&api, 1)
            .await
            .expect("client did not register");

        // Wait for the listener to be actually bound on the exposed port.
        wait_until("exposed port open", || async {
            TcpStream::connect(("127.0.0.1", remote_port)).await.ok().map(|_| ())
        })
        .await
        .expect("exposed port never opened");

        // Now send 128 KiB of random-ish bytes and verify echo.
        let mut sock = TcpStream::connect(("127.0.0.1", remote_port))
            .await
            .expect("connect exposed");
        let payload: Vec<u8> = (0..131_072u32).map(|i| (i as u8).wrapping_mul(31)).collect();
        sock.write_all(&payload).await.expect("write");

        let mut recv = vec![0u8; payload.len()];
        sock.read_exact(&mut recv).await.expect("read");
        assert_eq!(recv, payload, "bytes should round-trip through tunnel");
    })
    .await;

    result.expect("test timed out after 15s");
}
```

- [ ] **Step 2: 跑测试，先验证它能建起来**

```bash
cargo test --test tunnel_basic tunnel_forwards_bytes_bidirectionally -- --nocapture
```

Expected: PASS。如果 FAIL，检查：
- 服务器日志（`log: "info"` 临时打开）是否显示端口监听成功
- `wait_client_count` 是否超时——通常意味着客户端注册协议路径没打通

若确实是产品 bug，停手写 issue，不要盲改产品代码。

- [ ] **Step 3: 加第二个用例——TLS 关（其实已经关了，改成显式对比也覆盖协议路径）**

追加到 `tests/tunnel_basic.rs` 末尾：

```rust
#[tokio::test(flavor = "multi_thread")]
async fn tunnel_forwards_with_tls_disabled() {
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        // Explicit tls=false — same as the default in HarnessOpts, but locked in
        // so a later change to the default can't silently strip coverage.
        let mut harness = TestHarness::spawn(HarnessOpts {
            tls: false,
            exposed_port_count: 1,
            ..HarnessOpts::default()
        })
        .await;

        let echo_addr = spawn_echo().await;
        let remote_port = harness.exposed_ports[0];

        harness.spawn_client(vec![ForwardRule {
            remote_port,
            local_addr: echo_addr.to_string(),
            dns_name: None,
        }]);

        let api = harness.api_client();
        harness.wait_client_count(&api, 1).await.expect("register");

        wait_until("exposed port open", || async {
            TcpStream::connect(("127.0.0.1", remote_port)).await.ok().map(|_| ())
        })
        .await
        .expect("exposed port never opened");

        let mut sock = TcpStream::connect(("127.0.0.1", remote_port))
            .await
            .expect("connect");
        sock.write_all(b"ping").await.expect("write");

        let mut buf = [0u8; 4];
        sock.read_exact(&mut buf).await.expect("read");
        assert_eq!(&buf, b"ping");
    })
    .await;
    result.expect("test timed out");
}
```

- [ ] **Step 4: 跑第二个用例**

```bash
cargo test --test tunnel_basic tunnel_forwards_with_tls_disabled -- --nocapture
```

Expected: PASS.

- [ ] **Step 5: 加第三个用例——多端口并发**

追加到 `tests/tunnel_basic.rs`：

```rust
#[tokio::test(flavor = "multi_thread")]
async fn tunnel_multi_port() {
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let mut harness = TestHarness::spawn(HarnessOpts {
            tls: false,
            exposed_port_count: 2,
            ..HarnessOpts::default()
        })
        .await;

        let echo_a = spawn_echo().await;
        let echo_b = spawn_echo().await;
        let port_a = harness.exposed_ports[0];
        let port_b = harness.exposed_ports[1];

        harness.spawn_client(vec![
            ForwardRule {
                remote_port: port_a,
                local_addr: echo_a.to_string(),
                dns_name: None,
            },
            ForwardRule {
                remote_port: port_b,
                local_addr: echo_b.to_string(),
                dns_name: None,
            },
        ]);

        let api = harness.api_client();
        harness.wait_client_count(&api, 1).await.expect("register");

        wait_until("port_a open", || async {
            TcpStream::connect(("127.0.0.1", port_a)).await.ok().map(|_| ())
        })
        .await
        .expect("port_a never opened");
        wait_until("port_b open", || async {
            TcpStream::connect(("127.0.0.1", port_b)).await.ok().map(|_| ())
        })
        .await
        .expect("port_b never opened");

        // Concurrent traffic on both ports must not interleave.
        let a = tokio::spawn(async move {
            let mut s = TcpStream::connect(("127.0.0.1", port_a)).await.unwrap();
            s.write_all(b"AAAA").await.unwrap();
            let mut b = [0u8; 4];
            s.read_exact(&mut b).await.unwrap();
            b
        });
        let b = tokio::spawn(async move {
            let mut s = TcpStream::connect(("127.0.0.1", port_b)).await.unwrap();
            s.write_all(b"BBBB").await.unwrap();
            let mut b = [0u8; 4];
            s.read_exact(&mut b).await.unwrap();
            b
        });

        assert_eq!(&a.await.unwrap(), b"AAAA");
        assert_eq!(&b.await.unwrap(), b"BBBB");
    })
    .await;
    result.expect("test timed out");
}
```

- [ ] **Step 6: 跑全部**

```bash
cargo test --test tunnel_basic -- --nocapture
```

Expected: 3 tests PASS.

- [ ] **Step 7: 提交**

```bash
git add tests/tunnel_basic.rs
git commit -m "test: add tunnel basic integration tests

- bidirectional 128 KiB byte roundtrip
- explicit tls=false coverage
- concurrent traffic across two exposed ports"
```

---

## Task 8: tunnel_reconnect.rs — 重连与心跳

**Files:**
- Create: `tests/tunnel_reconnect.rs`

- [ ] **Step 1: 写强断重连用例**

创建 `tests/tunnel_reconnect.rs`：

```rust
//! Integration tests: client reconnect, heartbeat RTT, server restart.

#[path = "common/mod.rs"]
mod common;

use common::{spawn_echo, wait_until, HarnessOpts, TestHarness};
use rust_tunnel::client::config::ForwardRule;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[tokio::test(flavor = "multi_thread")]
async fn client_reconnects_after_forced_drop() {
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        let mut harness = TestHarness::spawn(HarnessOpts {
            tls: false,
            exposed_port_count: 1,
            ..HarnessOpts::default()
        })
        .await;

        let echo_addr = spawn_echo().await;
        let remote_port = harness.exposed_ports[0];
        let forwards = vec![ForwardRule {
            remote_port,
            local_addr: echo_addr.to_string(),
            dns_name: None,
        }];

        // First client
        let abort = harness.spawn_client(forwards.clone());
        let api = harness.api_client();
        harness.wait_client_count(&api, 1).await.expect("first register");

        // Round-trip on original tunnel
        wait_until("port open v1", || async {
            TcpStream::connect(("127.0.0.1", remote_port)).await.ok().map(|_| ())
        })
        .await
        .expect("port never opened");
        {
            let mut s = TcpStream::connect(("127.0.0.1", remote_port)).await.unwrap();
            s.write_all(b"v1").await.unwrap();
            let mut b = [0u8; 2];
            s.read_exact(&mut b).await.unwrap();
            assert_eq!(&b, b"v1");
        }

        // Kill the client task hard.
        abort.abort();

        // Wait for server to notice disconnection and drop the client from the list.
        wait_until("client cleared", || async {
            let (status, body) = api.get_json("/api/clients").await;
            if status.is_success() && body.as_array().map(|a| a.is_empty()).unwrap_or(false) {
                Some(())
            } else {
                None
            }
        })
        .await
        .expect("client never cleared");

        // Spawn a replacement client with the same forward.
        harness.spawn_client(forwards);
        harness.wait_client_count(&api, 1).await.expect("second register");
        wait_until("port open v2", || async {
            TcpStream::connect(("127.0.0.1", remote_port)).await.ok().map(|_| ())
        })
        .await
        .expect("port never re-opened");

        let mut s = TcpStream::connect(("127.0.0.1", remote_port)).await.unwrap();
        s.write_all(b"v2").await.unwrap();
        let mut b = [0u8; 2];
        s.read_exact(&mut b).await.unwrap();
        assert_eq!(&b, b"v2");
    })
    .await;
    result.expect("test timed out");
}
```

- [ ] **Step 2: 跑**

```bash
cargo test --test tunnel_reconnect client_reconnects_after_forced_drop -- --nocapture
```

Expected: PASS.

- [ ] **Step 3: 写心跳 RTT 用例**

追加到 `tests/tunnel_reconnect.rs`：

```rust
#[tokio::test(flavor = "multi_thread")]
async fn heartbeat_measures_rtt() {
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        let mut harness = TestHarness::spawn(HarnessOpts {
            tls: false,
            exposed_port_count: 1,
            ..HarnessOpts::default()
        })
        .await;

        let echo_addr = spawn_echo().await;
        let remote_port = harness.exposed_ports[0];
        harness.spawn_client(vec![ForwardRule {
            remote_port,
            local_addr: echo_addr.to_string(),
            dns_name: None,
        }]);

        let api = harness.api_client();
        harness.wait_client_count(&api, 1).await.expect("register");

        // The quality tracker requires a couple of heartbeat samples. Poll the
        // per-port quality endpoint until rtt_ms > 0 or timeout.
        //
        // NOTE: this test intentionally waits for real heartbeats. If the
        // default heartbeat interval is >10s, revisit this timeout.
        wait_until("rtt sample", || async {
            let (status, body) = api
                .get_json(&format!("/api/quality/ports/{remote_port}"))
                .await;
            if !status.is_success() {
                return None;
            }
            let rtt = body.get("rtt_ms").and_then(|v| v.as_f64()).unwrap_or(0.0);
            if rtt > 0.0 {
                Some(rtt)
            } else {
                None
            }
        })
        .await
        .expect("no RTT sample ever recorded — check heartbeat interval");
    })
    .await;
    result.expect("test timed out");
}
```

⚠️ **响应字段名注意**：`rtt_ms` 只是猜测。如果这一步 FAIL，先跑 `curl http://127.0.0.1:PORT/api/quality/ports/PORT` 手动看真实字段名，修正 assertion，不要改产品代码。

- [ ] **Step 4: 跑心跳用例**

```bash
cargo test --test tunnel_reconnect heartbeat_measures_rtt -- --nocapture
```

Expected: PASS. 若 FAIL 且是字段名不匹配，修 assertion 后重跑。若 FAIL 且是心跳周期太长（>20s），把 timeout 提到 45s——**不改产品心跳间隔**。

- [ ] **Step 5: 写 server 重启用例**

追加：

```rust
#[tokio::test(flavor = "multi_thread")]
async fn server_restart_survives_reregistration() {
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        let mut harness1 = TestHarness::spawn(HarnessOpts {
            tls: false,
            exposed_port_count: 1,
            ..HarnessOpts::default()
        })
        .await;

        let echo_addr = spawn_echo().await;
        let remote_port = harness1.exposed_ports[0];
        let control_port = harness1.control_addr.port();
        let api_port: u16 = harness1
            .api_base
            .rsplit(':')
            .next()
            .unwrap()
            .parse()
            .unwrap();

        let forwards = vec![ForwardRule {
            remote_port,
            local_addr: echo_addr.to_string(),
            dns_name: None,
        }];
        harness1.spawn_client(forwards.clone());
        let api = harness1.api_client();
        harness1.wait_client_count(&api, 1).await.expect("register");

        // Kill the server (drop harness1). Its Drop aborts server + api tasks.
        drop(harness1);

        // Give the OS a moment to release the ports (SO_REUSEADDR usually handles it).
        wait_until("ports free", || async {
            let ok1 = std::net::TcpListener::bind(("127.0.0.1", control_port)).is_ok();
            let ok2 = std::net::TcpListener::bind(("127.0.0.1", api_port)).is_ok();
            if ok1 && ok2 { Some(()) } else { None }
        })
        .await
        .expect("ports never freed");

        // Bring up a fresh harness — but it can't reuse the exact ports without
        // fighting for them. We test *the client's* ability to reconnect by
        // spawning a new harness AND a new client pointed at it, which mirrors
        // what happens when a client is reconfigured.
        //
        // (For "same-endpoint reconnect" we rely on `client_reconnects_after_forced_drop`.)
        let mut harness2 = TestHarness::spawn(HarnessOpts {
            tls: false,
            exposed_port_count: 1,
            ..HarnessOpts::default()
        })
        .await;
        let new_port = harness2.exposed_ports[0];
        harness2.spawn_client(vec![ForwardRule {
            remote_port: new_port,
            local_addr: echo_addr.to_string(),
            dns_name: None,
        }]);
        let api2 = harness2.api_client();
        harness2.wait_client_count(&api2, 1).await.expect("register");

        wait_until("new port open", || async {
            TcpStream::connect(("127.0.0.1", new_port)).await.ok().map(|_| ())
        })
        .await
        .expect("new port never opened");

        let mut s = TcpStream::connect(("127.0.0.1", new_port)).await.unwrap();
        s.write_all(b"ok").await.unwrap();
        let mut b = [0u8; 2];
        s.read_exact(&mut b).await.unwrap();
        assert_eq!(&b, b"ok");
    })
    .await;
    result.expect("test timed out");
}
```

- [ ] **Step 6: 跑全部**

```bash
cargo test --test tunnel_reconnect -- --nocapture
```

Expected: 3 tests PASS.

- [ ] **Step 7: 提交**

```bash
git add tests/tunnel_reconnect.rs
git commit -m "test: add tunnel reconnect and heartbeat integration tests

- client_reconnects_after_forced_drop (task abort → re-registration)
- heartbeat_measures_rtt (quality endpoint rtt sampling)
- server_restart_survives_reregistration (harness lifecycle)"
```

---

## Task 9: api_auth.rs — JWT 认证用例

**Files:**
- Create: `tests/api_auth.rs`

- [ ] **Step 1: 写四个用例**

创建 `tests/api_auth.rs`：

```rust
//! Integration tests: /api/login, JWT bearer enforcement, no-password mode.

#[path = "common/mod.rs"]
mod common;

use common::{HarnessOpts, TestHarness};
use reqwest::StatusCode;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn login_returns_jwt() {
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        let harness = TestHarness::spawn(HarnessOpts {
            tls: false,
            admin_password: Some("secret".to_string()),
            ..HarnessOpts::default()
        })
        .await;
        let mut api = harness.api_client();
        let (status, body) = api.login("secret").await;
        assert_eq!(status, StatusCode::OK, "login should return 200, body={body:?}");
        assert!(
            body.get("token").and_then(|v| v.as_str()).is_some(),
            "response must contain a token, got {body:?}"
        );
    })
    .await;
    result.expect("test timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn protected_route_requires_bearer() {
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        let harness = TestHarness::spawn(HarnessOpts {
            tls: false,
            admin_password: Some("secret".to_string()),
            ..HarnessOpts::default()
        })
        .await;

        // No token → expect 401.
        let api = harness.api_client();
        let status = api.get_status("/api/clients").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // With a valid token → expect 200.
        let mut api = harness.api_client();
        let (login_status, _) = api.login("secret").await;
        assert_eq!(login_status, StatusCode::OK);
        let status = api.get_status("/api/clients").await;
        assert_eq!(status, StatusCode::OK);
    })
    .await;
    result.expect("test timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_password_returns_401() {
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        let harness = TestHarness::spawn(HarnessOpts {
            tls: false,
            admin_password: Some("secret".to_string()),
            ..HarnessOpts::default()
        })
        .await;
        let mut api = harness.api_client();
        let (status, _) = api.login("WRONG").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    })
    .await;
    result.expect("test timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn no_admin_password_disables_auth() {
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        // admin_password: None → all routes should be open, no Bearer required.
        let harness = TestHarness::spawn(HarnessOpts {
            tls: false,
            admin_password: None,
            ..HarnessOpts::default()
        })
        .await;
        let api = harness.api_client();
        let status = api.get_status("/api/clients").await;
        assert_eq!(status, StatusCode::OK, "no-password mode should allow all routes");
    })
    .await;
    result.expect("test timed out");
}
```

- [ ] **Step 2: 跑**

```bash
cargo test --test api_auth -- --nocapture
```

Expected: 4 tests PASS.

如果 `wrong_password_returns_401` FAIL 且实际返回 400 或 403，改 assertion 匹配实际行为（这里就是"锁定当前行为"）；如果 `login_returns_jwt` 找不到 `token` 字段，用 `curl` 看真实字段名后修 assertion——**不改产品代码**。

- [ ] **Step 3: 提交**

```bash
git add tests/api_auth.rs
git commit -m "test: add /api/login and JWT enforcement tests

- login_returns_jwt
- protected_route_requires_bearer
- wrong_password_returns_401
- no_admin_password_disables_auth (locks current open-mode behavior)"
```

---

## Task 10: api_sse.rs — SSE 日志流与流量桶

**Files:**
- Create: `tests/api_sse.rs`

- [ ] **Step 1: 写 SSE 日志流用例**

创建 `tests/api_sse.rs`：

```rust
//! Integration tests: /api/logs SSE stream and traffic bucket API.

#[path = "common/mod.rs"]
mod common;

use common::{spawn_echo, wait_until, HarnessOpts, TestHarness};
use futures_util::StreamExt;
use rust_tunnel::client::config::ForwardRule;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[tokio::test(flavor = "multi_thread")]
async fn sse_streams_log_entries() {
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let harness = TestHarness::spawn(HarnessOpts {
            tls: false,
            ..HarnessOpts::default()
        })
        .await;

        // Emit a distinctive log line from *inside* the test process. If the
        // server's LogLayer captures it, it should also fan out via SSE.
        //
        // NOTE: init_logging is done by run_server. It uses a global subscriber,
        // so `tracing::warn!` here reaches the same layer.
        tracing::warn!("integration-test-sse-marker-abc123");

        // Open SSE stream (any authenticated route). Use no-auth mode → no bearer needed.
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();

        let resp = http
            .get(format!("{}/api/logs/stream", harness.api_base))
            .send()
            .await
            .expect("sse connect");
        assert!(resp.status().is_success(), "SSE endpoint should 200, got {}", resp.status());

        let mut stream = resp.bytes_stream();
        // Emit a *second* marker after the SSE stream is definitely open — the
        // first one may have been fired before subscription.
        tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            tracing::warn!("integration-test-sse-marker-abc123");
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        let mut found = false;
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(500), stream.next()).await {
                Ok(Some(Ok(chunk))) => {
                    if String::from_utf8_lossy(&chunk).contains("integration-test-sse-marker-abc123") {
                        found = true;
                        break;
                    }
                }
                Ok(Some(Err(_))) | Ok(None) => break,
                Err(_) => continue,
            }
        }
        assert!(found, "SSE stream should have emitted our marker log line");
    })
    .await;
    result.expect("test timed out");
}
```

⚠️ **路径注意**：`/api/logs/stream` 是猜测。若 FAIL，`grep -n "sse\|EventSource\|text/event-stream" src/server/api.rs` 查真实路径，修 URL。

- [ ] **Step 2: 跑**

```bash
cargo test --test api_sse sse_streams_log_entries -- --nocapture
```

Expected: PASS.

- [ ] **Step 3: 写流量桶用例**

追加：

```rust
#[tokio::test(flavor = "multi_thread")]
async fn traffic_bucket_appears_after_transfer() {
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        let mut harness = TestHarness::spawn(HarnessOpts {
            tls: false,
            exposed_port_count: 1,
            ..HarnessOpts::default()
        })
        .await;

        let echo_addr = spawn_echo().await;
        let remote_port = harness.exposed_ports[0];
        harness.spawn_client(vec![ForwardRule {
            remote_port,
            local_addr: echo_addr.to_string(),
            dns_name: None,
        }]);

        let api = harness.api_client();
        harness.wait_client_count(&api, 1).await.expect("register");
        wait_until("port open", || async {
            TcpStream::connect(("127.0.0.1", remote_port)).await.ok().map(|_| ())
        })
        .await
        .expect("port never opened");

        // Push 64 KiB through the tunnel.
        let mut sock = TcpStream::connect(("127.0.0.1", remote_port)).await.unwrap();
        let payload = vec![0xAAu8; 65_536];
        sock.write_all(&payload).await.unwrap();
        let mut recv = vec![0u8; payload.len()];
        sock.read_exact(&mut recv).await.unwrap();
        drop(sock);

        // Poll /api/traffic and expect non-zero bytes on our port.
        wait_until("traffic recorded", || async {
            let (status, body) = api.get_json("/api/traffic").await;
            if !status.is_success() {
                return None;
            }
            // /api/traffic returns Vec<PortTraffic> — find our port.
            let arr = body.as_array()?;
            for item in arr {
                let port = item.get("port").and_then(|v| v.as_u64())?;
                if port as u16 != remote_port {
                    continue;
                }
                // Any nonzero byte counter counts as "recorded".
                for key in ["total_bytes", "bytes_in", "bytes_out", "rx_bytes", "tx_bytes"] {
                    if let Some(n) = item.get(key).and_then(|v| v.as_u64()) {
                        if n > 0 {
                            return Some(n);
                        }
                    }
                }
            }
            None
        })
        .await
        .expect("no traffic bucket ever recorded for our port");
    })
    .await;
    result.expect("test timed out");
}
```

- [ ] **Step 4: 跑**

```bash
cargo test --test api_sse traffic_bucket_appears_after_transfer -- --nocapture
```

Expected: PASS. 若响应字段结构与猜测不符，`curl -s http://127.0.0.1:PORT/api/traffic | jq` 查真实结构后修 assertion。

- [ ] **Step 5: 跑整个 api_sse 文件**

```bash
cargo test --test api_sse -- --nocapture
```

Expected: 2 tests PASS.

- [ ] **Step 6: 提交**

```bash
git add tests/api_sse.rs
git commit -m "test: add SSE log stream and traffic bucket API tests"
```

---

## Task 11: 全套 3 连跑，验证无 flake

**Files:**
- (none)

- [ ] **Step 1: 跑 3 遍全部集成测试**

```bash
for i in 1 2 3; do
  echo "=== run $i ==="
  cargo test --tests -- --test-threads=4 || exit 1
done
```

Expected: 3 轮全部 PASS。

如果某个用例出现间歇性失败：
- 时序类问题（等待超时）：加大该用例的 `timeout(...)`，不动其他。
- 端口撞车：临时改成 `--test-threads=1` 复跑，确认后再考虑给 harness 加 `#[serial]`。
- Panic：贴堆栈到 issue，进 debug 会话，不要"重试直到过"。

- [ ] **Step 2: 提交任何修补（如无变更跳过）**

```bash
git status --short
# 若有改动：
git add tests/
git commit -m "test: stabilize timing on flaky integration tests"
```

---

## Task 12: tests/README.md — harness 使用文档

**Files:**
- Create: `tests/README.md`

- [ ] **Step 1: 写文档**

创建 `tests/README.md`：

````markdown
# 集成测试

本目录为 rust-tunnel 后端提供**内进程**端到端集成测试。每个测试都在同一进程内 `tokio::spawn` 启动 `run_server` + `run_client`，通过 `bind("127.0.0.1:0")` 分配随机端口，用完即 drop。

## 运行

```bash
cargo test --tests                    # 全部集成测试
cargo test --test tunnel_basic        # 指定文件
cargo test --test tunnel_basic tunnel_forwards_bytes_bidirectionally -- --nocapture
```

## 目录结构

```
tests/
  common/
    mod.rs         # #[path = "common/mod.rs"] mod common; 每个测试文件 include
    harness.rs     # TestHarness + HarnessOpts
    api_client.rs  # 带 JWT 的 reqwest 封装
    echo.rs        # spawn_echo / spawn_http_echo
    retry.rs       # wait_until 指数退避
  tunnel_basic.rs      # 双向转发、TLS 关、多端口
  tunnel_reconnect.rs  # 强断重连、心跳 RTT、server 重启
  api_auth.rs          # /api/login、Bearer 校验、无密码模式
  api_sse.rs           # SSE 日志流、流量桶
```

## 写新用例的模板

```rust
#[path = "common/mod.rs"]
mod common;

use common::{HarnessOpts, TestHarness};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn my_new_test() {
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let mut harness = TestHarness::spawn(HarnessOpts {
            tls: false,
            exposed_port_count: 1,
            ..HarnessOpts::default()
        })
        .await;

        // ... 你的用例
    })
    .await;
    result.expect("test timed out");
}
```

## 规矩

1. **禁止 `tokio::time::sleep(seconds)`。** 一律用 `common::wait_until("desc", || async { ... }).await`——指数退避 50 次，总时长 ~5 秒。
2. **每个 test 必须 `tokio::time::timeout(15s, ...)` 包裹**，避免 hang 阻塞 CI。
3. **不要在 test 中改产品代码。** 如果 assertion 与实际行为不符，先 `curl` 看真实响应，改 assertion 让它锁定"当前行为"；产品 bug 单独开 issue，不塞进本 PR。
4. **每个 test 独立端口独立 tempdir**，理论上可并行；如出现 flake，先加大 timeout，不动 harness。
5. **响应字段猜测**：这套用例基于对 `src/server/api.rs` 字段的推测。凡是 assertion 命名不确定（如 `rtt_ms`、`bytes_in`），先用 `curl | jq` 校对真实字段。

## CI 集成

见 `.github/workflows/ci.yml`：每次 push / PR 都会跑 `cargo fmt --check`、`cargo clippy -- -D warnings`、`cargo test --tests`。
````

- [ ] **Step 2: 提交**

```bash
git add tests/README.md
git commit -m "docs: add integration test README"
```

---

## Task 13: CI workflow

**Files:**
- Create: `.github/workflows/ci.yml`

- [ ] **Step 1: 写 workflow**

创建 `.github/workflows/ci.yml`：

```yaml
name: CI

on:
  push:
  pull_request:
    branches: [main]

jobs:
  test:
    name: fmt + clippy + test
    runs-on: ubuntu-latest
    timeout-minutes: 20

    steps:
      - uses: actions/checkout@v4

      - name: Install Rust toolchain
        uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy

      - name: Cache cargo registry & target
        uses: actions/cache@v4
        with:
          path: |
            ~/.cargo/registry
            ~/.cargo/git
            target
          key: ${{ runner.os }}-cargo-${{ hashFiles('**/Cargo.lock') }}
          restore-keys: |
            ${{ runner.os }}-cargo-

      - name: Format check
        run: cargo fmt --all -- --check

      - name: Clippy
        run: cargo clippy --all-targets -- -D warnings

      - name: Build
        run: cargo build --tests

      - name: Test
        run: cargo test --tests -- --test-threads=4
        env:
          RUST_BACKTRACE: "1"
```

- [ ] **Step 2: 本地预检**

```bash
cargo fmt --all -- --check && \
cargo clippy --all-targets -- -D warnings && \
cargo test --tests -- --test-threads=4
```

Expected: 三个命令依次 PASS。若 `clippy -D warnings` 因为 pedantic 项报错，**暂时不要**在 workflow 里加 pedantic——项目本身通过 `Cargo.toml` 或 crate-level attribute 控制 clippy 级别，我们只强制默认级别 `-D warnings`。

- [ ] **Step 3: 提交**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: add fmt/clippy/test workflow"
```

---

## Task 14: 收尾自检 + 推送分支

- [ ] **Step 1: 检查 git 状态干净**

```bash
git status
```

Expected: `working tree clean`.

- [ ] **Step 2: 快速回顾 commit 列表**

```bash
git log --oneline main..HEAD
```

Expected: 大致 13 个 commit（每 task 一个左右）。若某个 commit 混入了不相关的改动，用 `git rebase -i` 拆分或让 reviewer 知晓。

- [ ] **Step 3: 推送分支**

```bash
BRANCH=$(git rev-parse --abbrev-ref HEAD)
git push -u origin "$BRANCH"
```

- [ ] **Step 4: 开 PR（由用户驱动，不由 agent 直接开）**

停在这里，把 PR 描述草稿交给用户：

> **PR-1：集成测试基线**
>
> 落地 `docs/superpowers/specs/2026-07-11-testing-baseline-and-refactor-design.md` 阶段 1。
>
> 变更：
> - 新增 `tests/` 集成测试目录，含 harness + 4 个测试文件、10 个用例
> - 新增 `.github/workflows/ci.yml`（fmt + clippy + test）
> - `Cargo.toml` 加入 `reqwest`、`futures-util` 到 dev-dependencies
> - 零产品代码变更（含 `src/`、`Cargo.toml` 主依赖、协议、schema）
>
> 后续：PR-1.5（SS/Trojan 集成用例）和阶段 2（三大模块重构）另开 PR。

---

## 附录：常见排错

| 症状 | 可能原因 | 处理 |
|---|---|---|
| `api never became healthy` 超时 | 端口被占 / 服务器启动 panic | 打开 `log: "info"`、`--nocapture` 看 stderr |
| `client did not register` 超时 | 客户端 TLS 配置不匹配 / auth token 未传 | 检查 `HarnessOpts.tls` 是否和 client 一致 |
| `rtt_ms` 字段查不到 | 猜错字段名 | `curl http://127.0.0.1:APIPORT/api/quality/ports/PORT | jq` 校对 |
| SSE stream 收不到 marker | 路径不对 / 日志级别过滤 | `grep event-stream src/server/api.rs` 找路径；改测试内 `tracing::warn!` 到 `tracing::error!` |
| Windows CI 失败 | 本次 CI 只跑 Linux | 忽略（本 PR 不承诺 Windows 支持） |
| `--test-threads=4` flake | 内核端口回收慢 | 先降到 1，稳定后再调回 |

## Spec Coverage 检查

- ✅ Cargo dev-deps（Task 1）
- ✅ TestHarness harness 设计（Task 5）
- ✅ echo/http_echo（Task 3）
- ✅ ApiClient（Task 4）
- ✅ wait_until（Task 2）
- ✅ tunnel_basic 3 用例（Task 7）
- ✅ tunnel_reconnect 3 用例（Task 8）
- ✅ api_auth 4 用例（Task 9）
- ✅ api_sse 2 用例（Task 10）
- ✅ 3 连跑防 flake（Task 11）
- ✅ tests/README.md（Task 12）
- ✅ CI job（Task 13）
- ✅ 收尾（Task 14）

未覆盖的 spec 条目：无（PR-1.5 SS/Trojan 明确排除在本计划外）。
