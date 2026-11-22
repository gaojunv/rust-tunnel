// 测试代码豁免 panic 风险 lint（生产代码仍告警）。
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::pedantic
    )
)]
// 主机（非 Android）构建只用于跑纯逻辑单测：JNI/EGL 那一半整体被 cfg 掉，纯逻辑模块的
// 消费方随之消失，故主机上允许 dead_code；Android 构建仍照常告警。
#![cfg_attr(not(target_os = "android"), allow(dead_code))]

//! Android 控制端的 JNI 桥与 EGL/GLES 渲染。
//!
//! # 这是什么
//!
//! 把平台无关的 [`rust_tunnel_controller_core`]（控制通道 + 远程控制引擎）暴露给 Kotlin，
//! 并在本 crate 内实现「解码后的画面 → `android.view.Surface`」的渲染：
//!
//! - **JNI 导出**：类 `com.rusttunnel.controller.NativeBridge`，方法签名逐字对齐
//!   `android/JNI_CONTRACT.md` §2；
//! - **回调**：`com.rusttunnel.controller.NativeEvents` 的静态方法（§3），全部在引擎线程触发，
//!   Kotlin 自己切主线程；
//! - **渲染**：专用线程独占 `EGLDisplay`/`EGLContext`/`EGLSurface` 与 GLES 管线（优先
//!   GLES3、回退 GLES2）。支持 VP9 的 RGBA 整帧与 H.264/H.265 的 **NV12 整帧 + 脏矩形补丁**
//!   （协议 v6 `RegionUpdate`）；帧与补丁经**同一条有界 FIFO**（整帧容量 16 / 补丁容量 32，
//!   满则丢最旧），`on_frame`/`on_patch` 绝不阻塞引擎；
//! - **输入**：Android 键码/修饰键映射到协议 v5 键位码，鼠标/滚轮/文本直转引擎命令；
//! - **状态**：聚合快照 → 契约 §5 的 JSON（节流 ≤5 次/秒，只在变化时回调）。
//!
//! # 平台门控
//!
//! 只有 `target_os = "android"` 才编译 JNI/EGL 那一半；纯逻辑（键码映射、修饰键、
//! 状态 JSON、等比 contain 几何、NV12 补丁几何、渲染 FIFO 淘汰）在主机上照常编译并单测。因此：
//!
//! ```text
//! cargo test -p rust-tunnel-android-jni --lib          # 主机：只跑纯逻辑
//! cargo ndk -t arm64-v8a --platform 21 build -p rust-tunnel-android-jni
//! ```
//!
//! # 不依赖什么
//!
//! 不引入 egui / eframe / winit / xcap / enigo / arboard / win-*。VP9 软解仍在引擎里；
//! H.264/H.265 的 **MediaCodec 解码器工厂**（`VideoDecoderFactory`）由宿主经
//! `ControllerConfig::with_h264_decoder` / `with_h265_decoder` 注入（M1b 接线），
//! 本 crate 只负责把解码结果（RGBA 或紧包 NV12）画到 Surface。

mod geometry;
mod keycode;
mod nv12_geom;
mod queue;
mod state_json;

#[cfg(target_os = "android")]
mod bridge;
#[cfg(target_os = "android")]
mod handle;
#[cfg(target_os = "android")]
mod jni_api;
#[cfg(target_os = "android")]
mod logcat;
#[cfg(target_os = "android")]
mod render;
#[cfg(target_os = "android")]
mod sink;
