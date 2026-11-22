# macOS 远程控制对齐（设计 / 交付说明）

> 目标：把 Windows 被控端已有的远程桌面能力对齐到 macOS 14+：硬件 H.264/H.265 编解码、
> 流式采集与脏矩形补丁、远端光标形状、系统音频转发，以及无人值守（登录界面）能力。
>
> 线上协议**不改**：`SESSION_PROTO_VERSION` 保持 12，`ControlMessage` 不变。macOS 只是
> 新增平台实现，引擎（`crates/remote`）已有的协商、QoS、补丁、音频状态机全部复用。

## 1. 与 Windows 的对应关系

| 能力 | Windows | macOS 新增 |
|---|---|---|
| 硬件 H.264/H.265 编码 | NVENC / Intel·AMD MFT（`crates/win-video`） | `crates/mac-video`（VideoToolbox `VTCompressionSession`） |
| 硬件解码 | `MSH264DecoderMFT` / `MSH265DecoderMFT` | `crates/mac-video`（`VTDecompressionSession`） |
| 采集 | WGC（优先）/ DXGI（回退） | `crates/mac-capture`（ScreenCaptureKit，macOS 14 最低） |
| 静止不编码 / 脏矩形 | `capture/change.rs` + `capture/patches.rs` | `mac-video::change` + `mac-capture` 的补丁采集 |
| 远端光标形状 | `GetCursorInfo` + `IDC_*` 比对 | `NSCursor` 系统光标比对 |
| 系统声音 | WASAPI loopback + Opus（`crates/win-audio`） | `crates/mac-audio`（SCK 系统音频 + AudioQueue + Opus） |
| 无人值守 | SYSTEM 服务 + 安全桌面 | 见 `MACOS-UNATTENDED.md`（平台能力边界单独论证） |

引擎侧适配点：

- `crates/client-gui/src/remote/mac_video.rs`：把 `mac-video` 的编/解码器包装成
  `VideoEncoderFactory` / `VideoDecoderFactory`，接入
  `hardware_factories_with_decoders()`。
- `crates/client-gui/src/remote/mac_capture.rs`：把 `mac-capture` 包装成 `ScreenSource`
  （`capture` / `capture_patches` / `cursor_shape` / `capture_info`），接入
  `gui_platform_handles()`。
- `crates/client-gui/src/remote/mac_audio.rs`：把 `mac-audio` 包装成 `AudioSource` /
  `AudioSink`，接入 `audio_endpoints()`。

## 2. 分期与验收

**当前状态（2026-11-21）**：阶段 A/B/C 已完成，58 个纯逻辑测试通过；三个 macOS crate 与
`client-gui`（含真实 `opus` 模块）的 aarch64-apple-darwin 交叉 clippy 均通过（见 3.1
的本机 ABI 桩做法）。真正链接 Apple 框架/VT/SCK/libopus 的构建仍只能在 Mac 或 CI 上做。
阶段 D 结论已出（见下），**未实现登录窗口 agent 打包**。

### 阶段 A：VideoToolbox 硬件编解码（已完成）

产物：`crates/mac-video`

- H.264 / H.265 编码与解码，4:2:0 NV12，BT.709 limited（与 Windows / wgpu 着色器一致）。
- 在线改码率/帧率：`VTSessionSetProperty`，**不重建、不插 IDR**（QoS 闭环每秒调用）。
- 关键帧：`kVTEncodeFrameOptionKey_ForceKeyFrame`；`MaxKeyFrameInterval` 设大，避免周期性 IDR。
- 输出 AVCC → Annex B；关键帧前置 SPS/PPS(/VPS)。
- 质量地板：`MaxAllowedFrameQP = 20`（对齐 Windows `MAX_QP`）。
- 纯逻辑模块（Annex B / 颜色 / 变化检测）在 Windows 上可直接单测。

验收：
- `cargo test -p rust-tunnel-mac-video --lib`（任意平台）
- `cargo check -p rust-tunnel-mac-video --target aarch64-apple-darwin`（类型检查）
- 真 Mac：`mac-video-probe` 编解码往返、1080p/4K 耗时、硬件标志、HEVC。

### 阶段 B：ScreenCaptureKit 采集 + 补丁 + 光标（已完成，真机待验）

产物：`crates/mac-capture`、`client-gui` 的 `ScreenSource` 适配

- `SCStream` 持续采集：BGRA（或 CVPixelBuffer/IOSurface），多显示器。
- 变化检测 ≥320×180 网格，`changed` / `changed_permille`；静止时跳过编码。
- 脏矩形：网格 → 连通域 → 合并 → 目标坐标 NV12 补丁；进入 H.264/H.265 补丁模式。
- `NSCursor.currentSystemCursor` 形状映射（协议 v9 的 `CursorShape`）。
- `capture_info`：后端 `"ScreenCaptureKit"`、是否 GPU 路径、回退计数。

验收：真 Mac 上 1080p/4K 帧率、静止带宽、补丁模式进出门槛、双显示器坐标。

### 阶段 C：系统音频（已完成，真机待验）

产物：`crates/mac-audio`

- SCK 系统音频（`capturesAudio`，排除本进程）→ 48 kHz 立体声 → Opus 96 kbps / 20 ms。
- AudioQueue 播放：抖动缓冲 60–120 ms、欠载自适应、Opus PLC。
- 能力/原因经既有 `AudioStatus` 上报；隐私开关沿用既有语义。
- **构建依赖**：`audiopus_sys` 默认用 CMake 构建 vendored libopus（macOS runner 自带
  CMake，本地 Mac 需 `brew install cmake`）。若机器上有 Homebrew 的静态 libopus，
  `pkg-config` 会优先命中；发布构建可设 `OPUS_NO_PKG=1` 强制用 vendored 源码以保证可复现。

验收：真 Mac 上音画同步、设备切换、权限拒绝路径、长时间抖动统计。

### 阶段 D：无人值守（登录界面）

完整论证见 [`MACOS-UNATTENDED.md`](MACOS-UNATTENDED.md)。**结论（2026-11-21）：**

- **锁屏（用户已登录）**：采集 + 注入都可用，是最可靠的无人值守形态（P0）。
- **登录窗口画面**：**可以**，但必须由 `/Library/LaunchAgents` 的 LoginWindow agent
  （`LimitLoadToSessionType=[Aqua, LoginWindow]`，以 root 运行）采集，且需要
  **macOS 14.4+** 的 ScreenCaptureKit；root LaunchDaemon 够不着 WindowServer。
- **登录窗口输入**：**不可行/不可承诺**。没有公开 API，实测 `CGEventPost` 被 WindowServer
  丢弃；只能作为实验路径，或绕道 macOS 自带屏幕共享的 RFB（P2）。
- **FileVault 开机解锁**：平台外，第三方代码不运行。
- **长期无人值守的闸门**：Sequoia 起屏幕录制需要周期性重新确认，登录窗口没人能点；
  唯一正规解法是 Apple 审批的 `com.apple.developer.persistent-content-capture`
  （「远程桌面」权限）。没有它就只能做到「锁屏可控 + 登录窗口只读（需人工点一次授权）」。
- 推荐路线：P0 锁屏 → P1 登录窗口只读 + 登录交接 → P2 RFB 输入桥 → P3 entitlement/公证 pkg。

## 2.5 真机验收清单（本机 Windows 无法验证的部分）

| 模块 | 检查项 |
|---|---|
| `mac-video` | `is_hardware()` 是否如实；H.265 在 Intel/Apple Silicon 上的可用性；1080p/4K 每帧耗时；首帧是否能落在 2s 超时内；在线改码率/帧率**不插 IDR**；解码输出是否宏块对齐（1080→1088 需按 clean aperture 裁剪）；BT.709 limited 的黑/白端点；patch NV12 与 VT 全帧的颜色一致性 |
| `mac-capture` | 屏幕录制权限首次弹框与拒绝路径；SCK 实际帧率（60fps 配置 vs 引擎 30/60 采集）；静止画面 `changed=false`；补丁模式进/出门槛；多显示器切换与坐标；空闲自动停流（录屏指示熄灭）；`NSCursor` 形状映射 |
| `mac-audio` | SCK 系统音频的 ASBD（应为 48 kHz/32-bit float；44.1k 会被拒绝并写 `last_error`）；AudioQueue 设备延迟；拔耳机/切设备重建；欠载/PLC 计数与听感；与画面的音画同步 |
| 整体 | 两端 0.28.0 互控；macOS 作为控制端解码 Windows H.264/H.265；macOS 作为被控端被 Windows 控制；`RUST_TUNNEL_DISABLE_H265/HWCODEC` 排障开关 |

## 3. 构建与验证

- **本机（Windows）交叉类型检查**：仓库外准备一个 zig 垫片（`CC_aarch64_apple_darwin` /
  `AR_aarch64_apple_darwin` 指向 `zig cc -target aarch64-macos` 的包装脚本），然后：
  ```bash
  CC_aarch64_apple_darwin=... AR_aarch64_apple_darwin=... \
    cargo check -p rust-tunnel-mac-video -p rust-tunnel-mac-capture \
                -p rust-tunnel-mac-audio --no-default-features \
                --target aarch64-apple-darwin
  ```
  `cargo check` 不链接；**不要**对 Apple target 跑 build/test。
  **边界**：直接用这条命令检查 `client-gui` 会停在 `mac-notification-sys` 的
  `#import <Cocoa/Cocoa.h>`；`mac-audio` 默认 feature 会停在 `audiopus_sys` 的 CMake
  交叉构建。两者都有绕法（见 3.1），绕过后 `client-gui` 也能完整类型检查。
- **CI**：`.github/workflows/ci.yml` 的 `macos-check` 在 `macos-latest` 上编译全部 macOS
  crate（含 `client-gui` 与带 Opus 的完整 `mac-audio`），并运行三个 crate 的纯逻辑单测。
- **部署目标**：CI 与 `release-client-gui.yml` 的 macOS job 都设了
  `MACOSX_DEPLOYMENT_TARGET=14.0`。本地 Mac 构建也应显式设置，否则默认 11.0 与
  ScreenCaptureKit（12.3+）链接会冲突。

### 3.1 本机完整类型检查（Windows，无需 Apple SDK）

`client-gui` 的 macOS 目标有两个 C 依赖会挡住 `cargo check`：`mac-notification-sys`
（要 `Cocoa/Cocoa.h`）与 `audiopus_sys`（CMake 交叉构建）。用两个**仓库外**的本地 patch
可以绕过它们、只验证 Rust 类型与 lint（不链接、不改变提交）：

1. 复制 `mac-notification-sys` 到仓库外，把 `build.rs` 换成空的 `fn main() {}`；
2. 写一个同 ABI 声明的 `audiopus_sys` 桩（`OpusEncoder`/`OpusDecoder` 不透明类型、
   6 个 `opus_*` 函数、4 个常量、`static` feature；不需要 build.rs）；
3. 运行（注意**不要**设 `DOCS_RS=1`——它会让 vendored `shiguredo_libvpx` 输出空绑定）：

```bash
CC_aarch64_apple_darwin=<zig> AR_aarch64_apple_darwin=<zigar> \
cargo clippy -p rust-tunnel-client-gui --target aarch64-apple-darwin \
  --config 'patch.crates-io.mac-notification-sys.path="<仓库外副本>"' \
  --config 'patch.crates-io.audiopus_sys.path="<仓库外桩>"' -- -D warnings
```

这会真正编译 `mac_video.rs` / `mac_capture.rs` / `mac_audio.rs` 与 `mac-audio` 的 `opus.rs`。
**跑完务必 `git checkout -- Cargo.lock` 再 `cargo metadata`**：CLI patch 会把 lock 里的
这两个包改写成无 `source` 的 path 条目。

**仍然没有覆盖**：Apple 框架/VideoToolbox/ScreenCaptureKit/AudioQueue 的链接与运行期行为、
真实 libopus 的 CMake 构建、`mac-notification-sys` 的 Objective-C 编译。这些由 macOS CI
与真机探针负责。
- **真机验收**：每个 crate 的 `src/bin/*-probe`（`mac-video-probe` / `capture-probe` /
  `mac-audio-probe`）。

## 4. 权限（macOS TCC）

| 能力 | 系统授权 |
|---|---|
| 屏幕采集 | 屏幕录制（Screen Recording） |
| 输入注入 | 辅助功能（Accessibility） |
| 系统音频 | 屏幕录制（SCK 音频同属该授权） |
| 锁屏 | 无（`CGSession -suspend`） |

已有 `macos::permissions()` 探测；新增能力必须沿用同一套 UI 提示与设置跳转，不能静默失败。

## 5. 版本与发布

- 数据面协议不变（v12），0.27.x 与新版本可互控。
- 功能完成时把 `crates/client` / `client-gui` / `win-platform` / `win-service` 同步抬到
  0.28.0，并让 `v0.28.0` 与 `gui-v0.28.0` 指向同一版本。
- 服务端 `MIN_REMOTE_CLIENT_VERSION` 无需变动（macOS 能力不影响控制面门控）。
