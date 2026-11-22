# Android JNI 契约（M1a）

Rust 侧：`crates/android-jni`（cdylib，产物 `libandroid_jni.so`）
Kotlin 侧：`android/app`，包名 `com.rusttunnel.controller`

两侧**必须**按本文件实现；任何一方改签名都要同步改这里。

## 1. 线程与生命周期

- Kotlin 在 `Application.onCreate` 里 `System.loadLibrary("android_jni")`。
- Kotlin 必须先创建一个进程级 Tokio runtime（Rust 侧的 `nativeStart` 会在其中创建宿主），
  或直接调用 `nativeStart`，由 Rust 自己创建并持有 runtime（推荐，见下）。
- `nativeStart` 返回的 `Long` 是**不透明句柄**；`0` 表示失败（失败原因经 `onError` 回调）。
- `nativeStop(handle)` 之后句柄失效，不可再用。
- 渲染 surface 可能被创建/销毁多次（旋转、退到后台），必须支持 `nativeSetSurface(null)` 后再设置新 surface。

## 2. JNI 方法（`com.rusttunnel.controller.NativeBridge`，全部 `@JvmStatic external`）

| 方法 | 说明 |
|---|---|
| `nativeStart(server, token, clientName, downloadDir, tls, tlsInsecure): Long` | 启动控制通道 + 引擎；`server` 形如 `host:port`；`downloadDir` 必须是应用私有目录 |
| `nativeStop(handle)` | 停止并释放 |
| `nativeSetSurface(handle, surface: Surface?)` | 设置/清除渲染目标（ANativeWindow） |
| `nativeConnect(handle, targetClient: String, mode: Int)` | `mode`: 0=Control，1=ViewOnly，2=FileTransfer |
| `nativeCancelSession(handle)` | 取消当前会话 |
| `nativeSendMouseMove(handle, x: Float, y: Float, display: Int)` | 远端像素坐标 |
| `nativeSendMouseButton(handle, button: Int, pressed: Boolean, display: Int)` | `button`: 0=左，1=右，2=中 |
| `nativeSendWheel(handle, dx: Float, dy: Float)` | 滚轮增量（一格 = 1.0） |
| `nativeSendKey(handle, code: Int, pressed: Boolean, modifiers: Int)` | `code` 是协议 v5 键位码；`modifiers` 位图见 §4 |
| `nativeMapKey(androidKeyCode: Int): Int` | 把 Android `KeyEvent.getKeyCode()` 转成协议键位码；不支持返回 `-1`（**纯函数，不需要 handle**） |
| `nativeModifiers(androidMetaState: Int): Int` | 把 Android `KeyEvent.getMetaState()` 转成协议 modifiers 位图 |
| `nativeSendText(handle, text: String)` | 走 `keycode::TEXT` 遗留通路（被控端 `KEYEVENTF_UNICODE`），用于软键盘/中文输入 |
| `nativeSetQuality(handle, preset: Int)` | 0=Balanced，1=Smooth，2=Hd |
| `nativeSetFrameRate(handle, fps: Int)` | 15..=60 |
| `nativeSelectDisplay(handle, display: Int)` | 切换显示器 |
| `nativeSendClipboard(handle, text: String)` | 写远端剪贴板 |
| `nativeRequestClipboard(handle)` | 请求远端剪贴板 |
| `nativeLockScreen(handle)` | 锁定远端 |
| `nativeSnapshot(handle): String` | 当前状态 JSON（字段见 §5） |

所有方法都允许在任意线程调用；Rust 内部只做入队/转发，不做阻塞操作。

## 3. 回调（Rust → Kotlin，`com.rusttunnel.controller.NativeEvents` 的静态方法）

Rust 在 `nativeStart` 时缓存 `JavaVM` + 全局引用，回调都在 Rust 的引擎线程上触发；
Kotlin 侧必须自己 `runOnUiThread`。

| 方法 | 说明 |
|---|---|
| `onState(json: String)` | 控制通道/会话状态变化（结构见 §5），节流后可每秒多次 |
| `onFrameInfo(width: Int, height: Int, display: Int)` | 远端画面逻辑尺寸变化（用于把触摸坐标映射到远端像素） |
| `onDisplays(json: String)` | 显示器列表 `[{"id":0,"width":1920,"height":1080,"primary":true}]` |
| `onClipboard(text: String)` | 远端剪贴板内容 |
| `onCursor(shape: Int)` | 光标形状（协议 v9 判别式：0=Arrow…）；`-1` 表示未知 |
| `onSessionEnded(reason: String)` | 会话结束原因 |
| `onError(message: String)` | 可恢复/不可恢复错误提示 |

## 4. 输入编码

- 键位码表：从 `crates/client-gui/src/remote/mod.rs` 的 `keycode` 常量抽取到
  `crates/android-jni/src/keycode.rs`（Android 的 `KeyEvent.getKeyCode()` 映射到同一空间）。
- `modifiers` 位：**必须与桌面 `crates/client-gui/src/remote/mod.rs` 的 `modifier` 模块逐位一致**——
  `ALT=1<<0`、`CTRL=1<<1`、`SHIFT=1<<2`、`META=1<<3`（即 `ALT=0x01/CTRL=0x02/SHIFT=0x04/META=0x08`）。
  Kotlin 侧不需要自己拼这个位图，直接调 `nativeModifiers(metaState)`。
- 触摸 → 鼠标的**默认手势**（Kotlin 实现，坐标先换算成远端像素）：
  - 单指轻点：左键按下+抬起
  - 单指拖动：左键拖动
  - 长按：右键点击
  - 双指上下滑动：滚轮
  - 双指轻点：右键点击
  - 双指捏合：本机缩放/平移（不发给远端）

## 5. 状态 JSON（`nativeSnapshot` / `onState`）

最小字段集（后续可加，Kotlin 必须忽略未知字段）：

```json
{
  "connection": "connecting|connected|disconnected|rejected",
  "session": "none|awaiting_approval|connecting|active|ended",
  "target": "home-pc",
  "reason": null,
  "remote_width": 1920,
  "remote_height": 1080,
  "display": 0,
  "fps": 30,
  "kbps": 2400,
  "path": "relay|direct|punch",
  "audio_available": false,
  "error": null
}
```

## 6. 渲染

- Rust 的 `FrameSink::on_frame` 在 VP9 会话里拿到整帧 RGBA8；在 H.264/H.265 会话里拿到
  **紧包 NV12**（`DecodedFrame::nv12`，Y `w*h` + 交织 UV `w*h/2`）。
- Rust 用 EGL + GLES 直接渲染到 `nativeSetSurface` 给的 `ANativeWindow`：
  - EGL 上下文**优先 GLES3**（`EGL_OPENGL_ES3_BIT`），失败回退 GLES2 并记日志；
    NV12 平面纹理（`GL_R8` / `GL_RG8`）只在 GLES3 下可用，GLES2 时 NV12 帧会被跳过。
  - 创建 EGLDisplay/EGLContext/EGLSurface（`eglCreateWindowSurface`）。
  - RGBA：每帧 `glTexSubImage2D` 上传，绘制全屏矩形，`eglSwapBuffers`。
  - NV12：维护一张持久合成缓冲（Y + UV 两张平面纹理，尺寸 = 远端编码尺寸）；整帧替换全部
    内容，着色器按 BT.709 limited 转 RGB 后上屏。
  - 等比 contain 适配；尺寸变化时 `eglMakeCurrent` 不变、重建纹理。
  - `nativeSetSurface(null)` 时销毁 EGLSurface；新 surface 到来时重建。
- 渲染线程由 Rust 拥有（持有一个专用线程 + `EGL_NO_CONTEXT` 初始化的主线程无关上下文）；
  `on_frame` / `on_patch` 只把消息投进**同一条有界 FIFO**（整帧容量 16、补丁容量 32，
  满则丢最旧；控制消息永不淘汰），**绝不阻塞引擎**。
- 触摸映射由 Kotlin 用 `onFrameInfo` 的尺寸 + SurfaceView 的显示矩形计算。

### 6.1 脏矩形补丁（协议 v6 `RegionUpdate`）

- `FrameSink::on_patch` 收到的是**已解压**的紧包 NV12 矩形（`ScreenPatch`：`x`/`y`/`width`/
  `height` 全偶数，`nv12` 长度 `w*h*3/2`），必须**叠加在上一次 `on_frame` 的整帧之上**；
  两者经同一条 FIFO，顺序与引擎解码队列一致。
- 补丁上传：Y 平面写到 `(x, y)`、宽高 `(w, h)`、源行宽 `w`；UV 平面写到 `(x/2, y/2)`、
  目标宽高 `(w/2, h/2)`、源行宽仍是 `w` 字节（交织 `U,V` 对）。
- 没有基底帧、目标尺寸不符或几何非法时**整批丢弃**并记 warning（引擎侧会在尺寸不符时
  请求关键帧，下一个整帧自愈）；空矩形 = 保活，不重绘。

## 7. M1b（后续工作）

- 渲染侧的 NV12 着色器与脏矩形补丁合成**已完成**（见 §6.1）。
- 仍待接线：
  - `MediaCodec` 硬件解码 `VideoDecoderApi` 实现（H.264/H.265 → NV12），经
    `ControllerConfig::with_h264_decoder` / `with_h265_decoder` / `with_prefer_h265`
    注入（`crates/controller-core` 已支持）；
  - Kotlin 侧画质/编码偏好开关。
- 音频 `AudioSink`（AAudio + Opus）仍未实现。
