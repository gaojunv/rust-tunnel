# rust-tunnel-android-jni

Android 控制端的 JNI 桥 + EGL/GLES 渲染线程。接口契约见
[`android/JNI_CONTRACT.md`](../../android/JNI_CONTRACT.md)（两侧唯一真源）。

- 包名：`rust-tunnel-android-jni`，`crate-type = ["cdylib"]`，产物 `libandroid_jni.so`
  （Kotlin 侧 `System.loadLibrary("android_jni")`）。
- 引擎：`crates/controller-core` 的无头宿主（控制通道 + 远程控制引擎）。VP9 软件解码是
  默认路径；H.264/H.265 的 `VideoDecoderFactory` 可由宿主经
  `ControllerConfig::with_h264_decoder` / `with_h265_decoder` 注入（MediaCodec 实现属于后续
  接线，本 crate 只负责渲染解码结果）。
- 不依赖 egui / eframe / winit / xcap / enigo / arboard / win-*。

## 构建

```bash
export ANDROID_NDK_HOME='C:\Android\Sdk\ndk\29.0.14206865'
cargo ndk -t arm64-v8a --platform 21 build -p rust-tunnel-android-jni
cargo ndk -t x86_64     --platform 21 build -p rust-tunnel-android-jni
# 产物：target/<triple>/debug/libandroid_jni.so
```

主机上只能跑纯逻辑单测（JNI/EGL 部分至少需要 Android 目标）：

```bash
# Windows 上请指定 GNU 目标：`rust-tunnel-remote` 的 vendored libvpx 预编译是 mingw 味，
# MSVC 宿主目标会在链接阶段报 pthread/sem 未解析（仓库级已知限制，与 remote 的单测相同）。
cargo test --target x86_64-pc-windows-gnu -p rust-tunnel-android-jni --lib
# macOS / Linux 直接 `cargo test -p rust-tunnel-android-jni --lib`。
```

覆盖：Android 键码 → 协议键位码、`getMetaState()` → modifiers 位、状态 JSON 字段/取值、
显示器列表 JSON、等比 contain 视口几何、NV12 平面/补丁几何（偶数校验、UV 偏移、越界与长度
拒绝）、渲染 FIFO 的淘汰顺序（整帧/补丁/控制消息三类）、logcat writer。

## JNI 方法（`com.rusttunnel.controller.NativeBridge`）

签名逐字对齐契约 §2：`nativeStart(server, token, clientName, downloadDir, tls, tlsInsecure): Long`、
`nativeStop(handle)`、`nativeSetSurface(handle, surface: Surface?)`、`nativeConnect(handle, targetClient, mode)`、
`nativeCancelSession(handle)`、`nativeSendMouseMove(handle, x, y, display)`、
`nativeSendMouseButton(handle, button, pressed, display)`、`nativeSendWheel(handle, dx, dy)`、
`nativeSendKey(handle, code, pressed, modifiers)`、`nativeMapKey(androidKeyCode): Int`、
`nativeModifiers(androidMetaState): Int`、`nativeSendText(handle, text)`、
`nativeSetQuality(handle, preset)`、`nativeSetFrameRate(handle, fps)`、`nativeSelectDisplay(handle, display)`、
`nativeSendClipboard(handle, text)`、`nativeRequestClipboard(handle)`、`nativeLockScreen(handle)`、
`nativeSnapshot(handle): String`。

约定：

- 句柄 `0` = 启动失败；失败/可恢复错误经 `onError(String)` 回调。
- `nativeStop` 重复调用安全（第二次起为空操作）。
- `nativeSetSurface(null)` 后再设置新 Surface 是正常流程（旋转/退后台）。
- `nativeMapKey` 不支持时返回 `-1`；`nativeSendMouseButton` 的 `button` 0=左、1=右、2=中。
- `nativeSetQuality` 的 `preset`：0=Balanced、1=Smooth、2=Hd。

## 回调（`com.rusttunnel.controller.NativeEvents`）

`onState(json)`、`onFrameInfo(width, height, display)`、`onDisplays(json)`、`onClipboard(text)`、
`onCursor(shape)`、`onSessionEnded(reason)`、`onError(message)`。

全部在 Rust 的引擎线程/镜像任务上触发，**Kotlin 必须自己切主线程**。Rust 侧在回调失败
（方法不存在、Java 端抛异常）时只记日志并清掉待处理异常，绝不会把异常带过 JNI 边界。

## 状态 JSON（`nativeSnapshot` / `onState`）

契约 §5 的最小字段集（类型即示例）：

| 字段 | 类型 | 说明 |
|---|---|---|
| `connection` | string | `connecting` / `connected` / `disconnected` / `rejected`（`Idle`、`Stopped` 折算为 `disconnected`） |
| `session` | string | `none` / `awaiting_approval` / `active` / `ended`（内部 `Connecting` 折算为 `awaiting_approval`） |
| `target` | string | 有活动会话时是会话对端名，否则是最近一次 `nativeConnect` 的目标（可能为空串） |
| `reason` | string? | 会话结束原因（`session == "ended"` 时有值） |
| `remote_width` / `remote_height` | number | 远端画面逻辑尺寸；尚未出帧时回落到协商出的编码尺寸，再没有就是 `0` |
| `display` | number | 当前观看的显示器编号 |
| `fps` | number | 帧率上限（15..=60） |
| `kbps` | number | 控制器实测接收码率；尚无样本时为 `0` |
| `path` | string? | `direct` / `relay` / `punch`；无会话时为 `null` |
| `audio_available` | bool | 被控端是否具备音频能力（M1a 恒 `false`） |
| `error` | string? | 最近一次错误 |

Rust 侧追加的字段（Kotlin 忽略即可）：

| 字段 | 类型 | 说明 |
|---|---|---|
| `peers` | array | 可被远程控制的在线客户端，见下表 |
| `quality` | string | `smooth` / `balanced` / `hd` |
| `latency_ms` | number? | 最近实测延迟 |
| `codec` | string? | `vp9` / `h264` / `h265` |
| `session_id` | string? | 当前会话 ID |
| `self_rd_id` | string | 本端 RustDesk ID（服务端分配） |
| `capability` | bool | 服务端是否启用远程控制 |
| `connected` | bool | 控制通道是否已注册（`connection == "connected"` 的布尔形式） |

`peers[]` 的字段：

| 字段 | 类型 | 说明 |
|---|---|---|
| `name` | string | 展示名（`display_name` 为空时回落到 `client_name`），发起连接时用它 |
| `display_name` | string | 服务端下发的展示名（可能是空串） |
| `client_name` | string | 客户端内部标识（与 `name` 的区别仅在展示名缺失时） |
| `platform` | string | `windows` / `macos` / `linux` |
| `rd_id` | string | 服务端分配的 RustDesk ID |
| `displays` | number | 显示器数量 |

## 渲染线程设计

```
FrameSink::on_frame  (引擎 tokio 线程)
   └─ FrameInfo::note(...)  ──尺寸/display 变化时才回调 onFrameInfo
   └─ RenderHandle::push_frame ──> FullRgba / FullNv12（按 frame.nv12 选择）
FrameSink::on_patch  (引擎 tokio 线程)
   └─ RenderHandle::push_patch ──> Patch（与整帧同一条 FIFO，保持引擎侧顺序）
nativeSetSurface (JNI 线程)
   └─ ANativeWindow::from_surface ──> SetSurface（控制消息，不参与容量限制）

单一有界 FIFO（Mutex + Condvar；分类/淘汰逻辑在 src/queue.rs，主机可单测）
   整帧容量 16（满则丢最旧，trace 日志）
   补丁容量 32（满则丢最旧，warning 日志）
   SetSurface / Shutdown 永不淘汰

渲染线程 "rt-render"
   EGLDisplay / EGLConfig / EGLContext / EGLSurface / glow::Context（独占）
   优先 GLES3（EGL_OPENGL_ES3_BIT；失败回退 GLES2 并记日志）
   RGBA 整帧：glTexSubImage2D(RGBA) → contain 视口 → 清黑边 → draw → eglSwapBuffers
   NV12 整帧：替换持久缓冲（Y GL_R8 + UV GL_RG8）→ 同一套 contain/draw（BT.709 limited）
   补丁：     glTexSubImage2D 覆写矩形（Y 在 (x,y)，UV 在 (x/2,y/2)）→ 重绘持久缓冲
```

- 尺寸用 `eglQuerySurface(EGL_WIDTH/HEIGHT)` 每帧读取，旋转/窗口变化无需重建上下文；
  纹理维度变化时只重建纹理对象（NV12 重建会重置「有基底帧」标志，补丁在无基底时丢弃）。
- `nativeSetSurface(null)` / 换 Surface：先 `eglMakeCurrent(..., EGL_NO_SURFACE, ...)` 再
  `eglDestroySurface`；线程退出时按同样顺序销毁 GL 对象 → EGLSurface → EGLContext → EGLDisplay。
- 字体/画面方向：CPU 侧 RGBA/NV12 都是自顶向下，纹理坐标已上下翻转，`v=0` 对应屏幕上边。
- 没有 Surface、管线未就绪或帧数据不可用（RGBA 长度不符 / GLES2 下的 NV12）时静默跳过该帧，
  异常情况记 `tracing` 日志。
- 补丁**只**叠在上一次 `on_frame` 的整帧之上：没有基底帧、目标尺寸不符或几何非法时整批丢弃
  并记 warning（引擎侧会在尺寸不符时请求关键帧，下一个整帧自愈）。

## 线程与生命周期

- `nativeStart` 在调用线程 `block_on` 启动宿主：创建 3 worker 的 Tokio 运行时（线程名 `rt-ctl`）、
  渲染线程（`rt-render`）、状态镜像任务；返回句柄。
- `nativeStop` 顺序：关闭渲染队列并 join 渲染线程 → abort 状态镜像 → `host.stop().await` →
  关闭运行时（上限 2 秒）→ 从句柄表移除。重复调用安全。
- `nativeSnapshot` / 所有输入命令都只查句柄并转发，不做阻塞操作。

## 已知限制 / 真机才能验证的部分

- 本 crate 的 JNI 与 EGL/GLES 代码无法在主机运行：已验证的是**交叉编译**
  （`aarch64-linux-android`、`x86_64-linux-android`）与纯逻辑单测；真机上的 Surface 生命周期、
  驱动差异（`eglGetProcAddress` 是否覆盖核心函数）、旋转/退后台行为需要在 Android 设备上验收。
- 渲染器已支持 VP9 的 RGBA 整帧与 H.264/H.265 的 NV12 整帧 + 脏矩形补丁（GLES3）；
  **MediaCodec 解码器工厂尚未接线**——`ControllerConfig` 已能注入
  `h264_decoder_factory` / `h265_decoder_factory` 与 `prefer_h265`，实际实现与 Kotlin 侧开关
  属于后续工作。音频播放（AAudio + Opus）仍属于 M1b。
- 契约 §2 没有文件浏览/上传下载的 JNI 方法，故本 crate 未导出它们；`nativeConnect(mode=2)`
  只建立「仅文件传输」会话（引擎侧的 `ControllerCommands` 已有完整文件 API，等契约扩展后再接）。
- 修饰键位序**与桌面控制器 `crates/client-gui/src/remote/mod.rs` 的 `modifier` 模块逐位一致**：
  `ALT=1<<0`、`CTRL=1<<1`、`SHIFT=1<<2`、`META=1<<3`。当前引擎注入侧不读取
  `KeyEvent.modifiers`（修饰键走独立的键位事件），但这是线上字段，故不另起一套位序。
