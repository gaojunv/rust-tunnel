# 远程控制数据面延迟审查（重点：Windows → Windows）

> 日期：2026-11-16
> 范围：`crates/remote`（会话/传输/QoS/补丁）、`crates/win-video`（采集/编解码）、
> `crates/client-gui/src/remote`（平台实现/上屏）、`crates/rustdesk/src/relay.rs`（中继）。
> 结论基于代码阅读与仓库内既有真机实测记录；未做新的端到端实测。

## 0. 总体结论

分层与工程细节是合理的，特别是：

- 背压出口只在**编码之前**，编码后绝不丢帧（时域参考链不能断）；
- 写半边独立任务 + 四档优先队列，输入/心跳不会被视频写阻塞；
- 解码侧有完整的「序号断层 → 重置 → 等 IDR → 400 ms 限频」恢复状态机；
- WGC 帧池**排空取最新**，主动消掉合成器队列的 1–2 帧；
- 脏矩形补丁模式、在线重配置不重建/不插 IDR，这些不变量都有机械锁。

**主要问题**：整条管线是按「吞吐/带宽」优化的，不是按「交互延迟」优化的。编码之后仍
存在不可丢弃的深队列；关键帧没有大小/节奏约束；可靠层没有 pacing/FEC；采集是固定节拍
轮询。Windows→Windows 在网络尚可时，**主要延迟来自管线缓冲与关键帧尖峰，而不是网络**。

## 1. 端到端延迟预算（1080p / 30fps / 硬件 H.264，典型值）

| 环节 | 典型耗时 | 代码位置 |
|---|---|---|
| 采集采样相位 | 0–33 ms（均 16.7） | `controlled.rs` 的 `capture_tick` |
| WGC/DXGI 采集 + 320×180 变化检测 | 3–5 ms | `win-video/src/capture/` |
| 编码（NVENC / QSV） | 3 ms / 13 ms | `win-video` 真机记录 |
| 画面队列**常驻排队** | 0–133 ms | `SCREEN_QUEUE = 4` |
| 打包 + 加密 + 写 | <1 ms（LAN）；WAN 受码率限制 | `session.rs` |
| 网络单程 | LAN <1 ms；跨网 15–40 ms；中继 +1 跳 | — |
| 解码（DXVA） | 9–14 ms CPU | 仓库实测 |
| 上屏（upload + vsync/FIFO） | 5–33 ms | eframe 默认 present mode |

- LAN 直连：p50 ≈ 40–60 ms，p95（含关键帧恢复）≈ 80–150 ms。
- 跨网/中继：p50 ≈ 200–300 ms 是常态。

其中 **133 ms 的编码后队列**与**关键帧尖峰**属于可消除部分。

## 2. 导致延迟/卡顿的问题（按影响排序）

### P0-1 画面队列 4 深 = 链路上常驻约 133 ms 延迟

`crates/remote/src/controlled.rs`：`SCREEN_QUEUE = 4`。它是**编码之后、不可丢弃**的队列。
链路吞吐一旦接近编码输出码率（QoS 的目标就是如此），队列会长期装满，稳态延迟 =
4 / fps = 133 ms（30fps）。这一层没有任何丢弃阀门。

**建议**：降到 1–2；编码任务在 `send_video` 上阻塞时，采集侧 `try_send` 失败自然跳过
（设计里已有的合法降帧路径）。

### P0-2 关键帧无大小上限、无 pacing、无周期 IDR

IDR 典型 150–400 KB，在 6 Mbps 上是 200–530 ms 的串行化尖峰；`RequestKeyframe` 只要
400 ms 限频，丢包恢复时连续 IDR 会塞满链路。全帧路径没有带宽兜底（补丁路径有）。

**建议**：① 发送侧对大帧分段 pacing；② 增加 5–10 s 周期 IDR 作为兜底；③ 评估
intra-refresh 替代大 IDR；④ 编码器侧限制 IDR 目标尺寸（NVENC/MFT 当前绑定未暴露
`maxFrameSize`，需要另找手段）。

### P0-3 没有硬件 H.264 时回退 VP9 4:4:4 软件编码

`choose_codec` 只在两端都支持 H.264 时选 H.264；MFT 只枚举 `MFT_ENUM_FLAG_HARDWARE`。
仓库记录现场 HD 630 的 VP9 1080p 编码 **113 ms/帧（约 9 fps）**。虚拟机、老核显、
嵌套远控都会落到这条路上。

**建议**：增加**软件 H.264** 回退（Media Foundation 软件 MFT / OpenH264 / x264
ultrafast）。这是 Windows→Windows 最大的体验悬崖。

### P0-4 采集与编码并发共用同一个 D3D11 立即上下文

- 采集线程：`CopyResource` / `VideoProcessorBlt` / staging `Map`；
- 编码线程：VideoProcessor 缩放 / `UpdateSubresource`；
- `SetMultithreadProtected(true)` 只在 MFT 的 `create_with_device` 与 DXVA 中调用；
  **NVENC 路径直接复用采集设备，未开启保护**；
- 设备本身用 `D3D11_CREATE_DEVICE_VIDEO_SUPPORT` 创建。

D3D11 立即上下文不是线程安全的；即便开启保护也是设备级全局锁，且 `Map()` 会等待所有
已提交 GPU 工作完成，采集节拍会被编码耗时耦合。

**建议**：在所有 D3D11 设备的创建路径开启 `ID3D10Multithread::SetMultithreadProtected`；
中期把编码放到专用线程 + deferred context。

### P1-1 补丁模式在主循环里 `await` 画面队列

全帧模式把等待放在编码任务里；补丁模式却在会话 `select!` 循环内逐条
`enqueue_screen(...).await`。网络拥塞时主循环被卡住，期间**无法读取/注入输入**、
采集 tick 也被跳过。

**建议**：补丁投递改为非阻塞 `try_send`；队列满时退出补丁模式并强制一个 IDR 全帧
（与现有「补丁装不下」的路径一致，顺序与正确性都成立）。

### P1-2 打洞（UDP）可靠层：尾部丢包等 200 ms、无 pacing/FEC

RTO min 200 ms，一帧最后一个段丢失不会产生 3 个 dup ack，只能等 RTO；IDR 突发容易在
瓶颈处成片丢失。

**建议**：尾部丢包探测（TLP）；发送 pacing；视频帧内 FEC。长期可评估 QUIC 多 stream。

### P1-3 中继 `pump` 用单任务串行双向转发

`crates/rustdesk/src/relay.rs` 的 `pump` 在 `write_all` 期间不读另一方向；64 KiB 写不
出去时，反方向的输入帧也读不了。

**建议**：`split` 后两条独立转发任务（或 `copy_bidirectional`）。

### P1-4 固定节拍采集 + tick 相位

采样相位平均 16.7 ms（30fps）。WGC 帧池排空已消掉源端排队，但相位仍在。

**建议**：输入到达时触发一次「相位对齐」采集（受 fps 上限约束，替换而非叠加下一次 tick）。

### P1-5 解码队列 16 深、没有按时间戳老化

一次 500 ms 卡顿后队列积 15 帧，解码器要把它们全部解完才追上最新画面。

**建议**：队列降到 4；入队时按 `capture_ts_us` 丢弃超过 ~150 ms 的帧（丢 P 帧按既有
状态机请求 IDR）。

### P2（记录，暂不实施）

- 每帧两次 `spawn_blocking`：MFT/D3D11/NVENC 对象在池线程间迁移，COM 反复初始化；
  建议采集/编码各用专用线程。
- 呈现链 vsync/FIFO：`eframe::NativeOptions` 未设 present mode，本身有 1–2 帧延迟。
- 4K 全帧 staging 整块 clone（`CompositeFrame::take_upload`）后再 `write_texture`。
- 帧打包三份拷贝（`bincode::serialize` → `cipher.seal` → `put_slice`）。
- 采集看门狗 10 s × 3 次：采集挂死时画面冻结最长 30 s 才结束会话。
- 默认 6 Mbps / 30 fps 对 LAN 偏保守，画质不足会被用户感知为「卡/糊」。
- `e2e_ms` 只观测，不参与 QoS 决策。

## 3. Windows→Windows 专项

1. 「流畅与否」高度依赖硬件 H.264；无硬件编码器时体验断崖。
2. 采集/编码共设备是 Windows 数据面最需要修的缺陷（正确性 + 抖动）。
3. 输入注入本身不是瓶颈（实例路径入队、worker 批量 `SendInput`；GUI 路径逐事件微秒级）。
   瓶颈在补丁模式的队列 await 与传输层队头阻塞。
4. 安全桌面回退 BitBlt 是合理取舍，但应在 UI 上明示「当前为安全桌面采集」。

## 4. 验收口径

- LAN 直连、1080p、30fps、NVENC：「按键 → 画面回显」p50 < 50 ms、p95 < 100 ms。
- 跨网直连/打洞：p50 < 120 ms。
- 仅做 P0-1 / P1-1 / P1-5 的队列调整，预期各自减少 30–100 ms。

## 5. 实施计划（本 worktree）

- 分支：`perf/input-aligned-capture`（worktree `F:\Projects\rust-tunnel-latency`）
- 已含前置改动（本次之前遗留、未提交）：输入对齐采集（P1-4 的另一种实现）、
  上屏 `desired_maximum_frame_latency = 1`。默认帧率**保持 30**（用户 2026-11-16 决定
  不接受 30 → 60 的默认值变更）。
- 并行分工（按文件边界，互不重叠）：

| 项 | 文件 | 状态 |
|---|---|---|
| P0-1 画面队列 4 → 2 | `remote/controlled.rs` | 已完成 |
| P1-1 补丁发送改非阻塞 `try_send` | `remote/controlled.rs` | 已完成 |
| P0-2 周期 IDR + 大帧 pacing | `remote/controlled.rs`、`remote/session.rs` | 已完成 |
| P1-5 解码队列 16 → 4 + 时间戳老化 | `remote/controller.rs` | 已完成 |
| P1-2 可靠层尾部丢包探测（TLP） | `remote/reliable.rs` | 已完成 |
| P1-3 中继双向独立转发 | `rustdesk/relay.rs` | 已完成 |
| P0-4 D3D11 multithread protection | `win-video/d3d11.rs`、`mft_encoder/device.rs` | 已完成 |

**未纳入本轮**：P0-3（软件 H.264 回退）、P1-2 的 pacing/FEC、P2 列表。
P0-3 是 Windows→Windows 体验悬崖，建议下一轮优先。

## 5.1 验证结果（2026-11-16）

| 命令 | 结果 |
|---|---|
| `cargo check -p rust-tunnel-remote -p rust-tunnel-rustdesk -p rust-tunnel-win-video --all-targets` | 通过 |
| `cargo check -p rust-tunnel-client-gui` | 通过 |
| `cargo clippy -p rust-tunnel-remote -p rust-tunnel-rustdesk -p rust-tunnel-win-video --all-targets -- -D warnings` | 通过（0 warning） |
| `cargo test -p rust-tunnel-rustdesk --lib` | **98 passed**（含「单方向阻塞不拖住另一方向」回归） |
| `cargo test -p rust-tunnel-win-video --lib` | **128 passed, 3 ignored** |
| `cargo test -p rust-tunnel-remote --lib`（MSVC + pthread 垫片） | **320 passed, 2 failed, 1 ignored** |

`rust-tunnel-remote` 的 2 个失败是本机环境问题（`file::tests::oversize_transfers_are_rejected`、
`transfer::tests::upload_over_cap_is_rejected` 写 1 TiB 稀疏文件报 `StorageFull`），与本次改动无关。
另有 `signaling::tests::nat_probe_*` 在本机真实 UDP 下偶发 flake，也非本次改动引入。

## 5.2 本轮实现要点与残留风险

- **P0-1**：`SCREEN_QUEUE` 4 → 2。稳态额外延迟从 ~133 ms 降到 ~66 ms（30fps）。
- **P1-1**：补丁投递改 `try_enqueue_screen`；满队列 → 退出补丁模式 + 强制 IDR，主循环不再被阻塞。
- **P0-2**：周期 IDR（10s，仅在 Video 模式且距上个实际关键帧超时）；大帧（≥96 KiB）按 32 KiB
  分块写、块间按目标码率 sleep，单间隙 ≤100 ms、总等待 ≤ 2×自然串行时间。**未做**可靠层 pacing/FEC。
- **P1-2**：新增尾部丢包探测（`max(1.5×SRTT, 30ms)`，重传最后一个未确认段，每个 `send_base` 至多一次）；
  不计拥塞、不动 cwnd、不与 RTO/快速重传重复发送。**未做** FEC。
- **P1-3**：中继拆成两条独立转发任务 + 共享活动时钟；空闲超时语义保持不变。
- **P0-4**：所有 D3D11 设备创建时开启 `ID3D10Multithread` 保护（含 NVENC 复用的采集设备）。
  代价是设备级锁会把采集/编码的 D3D 调用串行化；正确性优先。
- **P1-4**（前置改动）：输入到达时把下一拍采集提前到「上一拍后半个周期」；上屏
  `desired_maximum_frame_latency = 1`。
- **P1-5**：解码队列 16 → 4；按 `capture_ts_us` + 积压 ≥2 + 非关键帧三条件老化丢弃；
  时钟未知（`UNKNOWN_OFFSET`）时 fail-open。
- **默认帧率保持 30**：前置改动里的 30 → 60 已按用户决定回退
  （`crates/remote/src/manager.rs` 的 `DEFAULT_FRAME_RATE` 与 `crates/client-gui/src/prefs.rs`
  的对应测试）。用户仍可在会话设置里手动选 60。

## 6. 观测字段对照

| 现象 | 字段 |
|---|---|
| 编码前丢帧 vs 发送背压 | `HostStats.skipped` vs `out_queue_depth/out_queue_skips` |
| 采集慢还是编码慢 | `capture_us` vs `encode_ms` |
| 关键帧尖峰 | `keyframes` / `idr_kbps` / `idr_bytes` |
| 画面延迟位置 | 控制器 `e2e_ms` + UI 的「采集→上屏」 |
| 网络 vs 管线 | `rtt_ms` / `backlog` / `local_backlog` / 可靠层 `rto`、`fast_retransmit`、`out_of_order` |
| 补丁抖动 | `patch_mode` / `changed_permille` / `patches` 速率 |
| 关键帧请求原因 | `SessionStats::KeyframeRequest{reason}` 分桶 |
