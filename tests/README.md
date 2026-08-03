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
  tunnel_reconnect.rs  # 管理员断开重连、心跳 RTT、server 重启
  api_auth.rs          # /api/login、Bearer 校验、无密码模式
  api_sse.rs           # SSE 日志流、流量桶
  config_persist.rs    # SS/Trojan 改配置后用同一 DB 重启，端口不回退
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

1. **禁止 `tokio::time::sleep(seconds)`。** 一律用 `common::wait_until("desc", || async { ... }).await`——指数退避 30 次，总时长 ~13 秒。
2. **每个 test 必须 `tokio::time::timeout(15s, ...)` 包裹**，避免 hang 阻塞 CI。
3. **不要在 test 中改产品代码。** 如果 assertion 与实际行为不符，先 `curl` 看真实响应，改 assertion 让它锁定"当前行为"；产品 bug 单独开 issue，不塞进本 PR。
4. **每个 test 独立端口独立 tempdir**，理论上可并行；如出现 flake，先加大 timeout，不动 harness。
5. **响应字段猜测**：这套用例基于对 `src/server/api.rs` 字段的推测。凡是 assertion 命名不确定（如 `rtt_ms`、`bytes_in`），先用 `curl | jq` 校对真实字段。

## CI 集成

见 `.github/workflows/ci.yml`：每次 push / PR 都会跑 `cargo fmt --all -- --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test --tests`。

## 构建性能（`.cargo/config.toml` + `Cargo.toml` profile）

本仓库编译/测试的内存与磁盘开销主要来自两处：

1. **`qdrant-edge` 编译峰值 ~3GB**。本机/CI 若可用内存不足 4GB，cargo 默认 `jobs=nproc` 并行编译会直接 OOM（`signal: 9, SIGKILL`）。`.cargo/config.toml` 把 `jobs` 限到 **2**，牺牲一点编译吞吐换稳定性；内存充裕的机器可酌情调高或删除该行。
2. **测试二进制体积**。debug 默认 `debuginfo=2`，单个测试二进制 ~1.2GB，12 个集成测试 + lib 测试合计 80GB+。`Cargo.toml` 的 `[profile.test] debug=1` 保留行号、丢弃局部变量调试信息，把单二进制压到 ~640MB，`target/debug` 总量降到 ~26GB。`incremental=false` 避免测试一次性编译还写增量缓存。

链接器用 **mold**（`rustflags = ["-C", "link-arg=-fuse-ld=mold"]`），比默认 GNU ld 快数倍。若系统没有 mold，先 `apt install mold` / `brew install mold`，或临时注释掉 `.cargo/config.toml` 里对应 target 的 rustflags。

日常命令：

```bash
cargo test --tests            # 受限并行编译 + mold 链接，内存安全
cargo test --test tunnel_basic tunnel_forwards_bytes_bidirectionally -- --nocapture
```


## 已知限制

- `run_client` 在库层内部 `tokio::spawn` 了 writer、heartbeat、log-forwarder 三个 detached task。`AbortHandle::abort()` 只能中止外层 future，不能中止这些子 task。因此"客户端崩溃后自动重连"的场景无法在 harness 中通过 task abort 测试，只能通过 `DELETE /api/clients/:port`（admin disconnect）测试"服务器踢下线后重新注册"。
- server-side `LogLayer` 只在 `src/bin/server.rs` 中安装，不在 `run_server` 中。SSE 测试通过 spawn client（它安装 `ClientLogLayer`）让日志事件经由控制通道转发到 server log store。
