//! JNI 导出（`com.rusttunnel.controller.NativeBridge`，契约 §2）。
//!
//! 所有函数名按 JNI 规范 `Java_<包>_<类>_<方法>` 手工书写（类名 `NativeBridge` 在
//! `com.rusttunnel.controller` 包下），全部 `extern "system"` + `#[no_mangle]`。
//! 签名必须与 `android/JNI_CONTRACT.md` §2 **逐字一致**；任何改动都要同步契约与 Kotlin。
//!
//! 线程模型：这些函数可以在任意线程被调用，内部只做「查句柄 → 投递/转发」：
//! - 输入/命令直接进引擎的通道（引擎内部无阻塞）；
//! - `nativeSetSurface` 只把窗口投进渲染线程；
//! - `nativeStop` 是唯一可能阻塞的调用（等宿主收尾，上限约 5 秒）。
//!
//! 错误一律经 `onError` 回调或 `tracing` 记录，**不向 Java 抛异常**（契约未定义异常语义）。

use std::sync::{Arc, Mutex};

use jni::objects::{JClass, JObject, JString};
use jni::sys::{jboolean, jfloat, jint, jlong, jstring};
use jni::JNIEnv;
use ndk::native_window::NativeWindow;
use rust_tunnel_controller_core::{
    ControllerConfig, ControllerHost, InputEvent, KeyEvent, MouseButton, QualityPreset, RemoteMode,
};
use rust_tunnel_remote::CodecId;

use crate::bridge::JniBridge;
use crate::handle::{self, HostSlot, WORKER_THREADS};
use crate::keycode::{keycode, map_android_key, modifiers_from_meta_state};
use crate::logcat;
use crate::render;
use crate::sink::{AndroidFrameSink, FrameInfo};

/// 读取 Java 字符串；失败（含待处理异常）时返回 `None` 并清掉异常。
fn read_string(env: &mut JNIEnv<'_>, value: &JString<'_>) -> Option<String> {
    match env.get_string(value) {
        Ok(text) => Some(text.into()),
        Err(err) => {
            tracing::warn!("读取 Java 字符串失败：{err}");
            let _ = env.exception_clear();
            None
        }
    }
}

/// 句柄查找（未知句柄返回 `None`，调用方静默忽略）。
fn lookup(handle: jlong) -> Option<Arc<HostSlot>> {
    let id = u64::try_from(handle).ok()?;
    handle::get(id)
}

/// 浮点坐标 → 整数像素（NaN/无穷/超范围一律给 0，避免未定义行为）。
#[expect(
    clippy::cast_possible_truncation,
    reason = "坐标/滚轮量的有效范围远小于 i32，且已先做 is_finite 与 clamp"
)]
fn round_to_i32(value: f32) -> i32 {
    if !value.is_finite() {
        return 0;
    }
    let clamped = value.round().clamp(-1.0e9, 1.0e9);
    clamped as i32
}

/// 停止并释放一个句柄（`nativeStop`；重复调用安全）。
///
/// # Safety
///
/// 仅由 JNI 运行时按 `NativeBridge.nativeStop` 的 ABI 调用。
#[no_mangle]
pub extern "system" fn Java_com_rusttunnel_controller_NativeBridge_nativeStop<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    let Ok(id) = u64::try_from(handle) else {
        return;
    };
    if let Some(slot) = handle::remove(id) {
        tracing::info!("停止控制器宿主（handle={id}）");
        slot.shutdown();
    } else {
        tracing::debug!("nativeStop 收到未知句柄 {id}（已停止？）");
    }
}

/// 启动控制通道 + 引擎，返回不透明句柄（`0` = 失败）。
///
/// # Safety
///
/// 仅由 JNI 运行时按契约 §2 的 ABI 调用；参数必须是合法的 Java 字符串。
#[no_mangle]
pub extern "system" fn Java_com_rusttunnel_controller_NativeBridge_nativeStart<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    server: JString<'local>,
    token: JString<'local>,
    client_name: JString<'local>,
    download_dir: JString<'local>,
    tls: jboolean,
    tls_insecure: jboolean,
) -> jlong {
    let bridge = match JniBridge::new(&mut env) {
        Ok(bridge) => Arc::new(bridge),
        Err(err) => {
            tracing::error!("初始化回调桥失败：{err}");
            return 0;
        }
    };

    let (Some(server), Some(token), Some(name), Some(dir)) = (
        read_string(&mut env, &server),
        read_string(&mut env, &token),
        read_string(&mut env, &client_name),
        read_string(&mut env, &download_dir),
    ) else {
        bridge.error("nativeStart 参数读取失败（字符串非法）");
        return 0;
    };

    let mut config = ControllerConfig::new(server, token)
        .with_client_name(name)
        .with_download_dir(dir)
        .with_tls(tls != 0, tls_insecure != 0);
    logcat::install(&config.log_level);

    // 硬解优先：**创建式探测**，真的能建出 MediaCodec 解码器才声明对应 codec。
    // 探测失败时 `CodecOffer` 里不出现该 codec，协商自动回退（H.265 → H.264 → VP9），
    // 不会出现「协商成功、解码时才失败」的半失效状态。
    if rust_tunnel_android_video::codec_available(CodecId::H264) {
        config = config.with_h264_decoder(rust_tunnel_android_video::h264_decoder_factory());
        tracing::info!("MediaCodec H.264 解码器可用");
    } else {
        tracing::warn!("本机 MediaCodec 无法创建 H.264 解码器，仅声明 VP9");
    }
    if rust_tunnel_android_video::codec_available(CodecId::H265) {
        config = config.with_h265_decoder(rust_tunnel_android_video::h265_decoder_factory());
        tracing::info!("MediaCodec H.265 解码器可用");
    } else {
        tracing::info!("本机 MediaCodec 无法创建 H.265 解码器，协商时回退 H.264");
    }

    let (render_handle, render_thread) = match render::spawn() {
        Ok(pair) => pair,
        Err(err) => {
            bridge.error(&format!("创建渲染线程失败：{err}"));
            return 0;
        }
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(WORKER_THREADS)
        .thread_name("rt-ctl")
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            render_handle.close();
            let _ = render_thread.join();
            bridge.error(&format!("创建 Tokio 运行时失败：{err}"));
            return 0;
        }
    };

    let host = match runtime.block_on(async { ControllerHost::start(config) }) {
        Ok(host) => host,
        Err(err) => {
            render_handle.close();
            let _ = render_thread.join();
            runtime.shutdown_timeout(std::time::Duration::from_millis(200));
            bridge.error(&format!("启动控制器失败：{err}"));
            return 0;
        }
    };

    let frame_info = Arc::new(FrameInfo::new());
    host.set_frame_sink(Arc::new(AndroidFrameSink::new(
        Arc::clone(&bridge),
        render_handle.clone(),
        Arc::clone(&frame_info),
    )));

    let snapshots = host.snapshots();
    let remote_state = host.remote_state();
    // 状态镜像任务与句柄**共用**同一个 target，避免 `nativeSnapshot` 与 `onState` 不一致。
    let target = Arc::new(Mutex::new(String::new()));
    let mirror_task = runtime.spawn(handle::mirror_loop(
        snapshots.clone(),
        remote_state.clone(),
        Arc::clone(&bridge),
        Arc::clone(&frame_info),
        Arc::clone(&target),
    ));

    let commands = host.commands();
    let id = handle::next_handle();
    let slot = Arc::new(HostSlot::new(
        commands,
        snapshots,
        remote_state,
        render_handle,
        render_thread,
        bridge,
        frame_info,
        host,
        runtime,
        mirror_task,
        target,
    ));
    handle::insert(id, slot);

    let Ok(id) = jlong::try_from(id) else {
        return 0;
    };
    tracing::info!("控制器宿主已启动（handle={id}）");
    id
}

/// 设置/清除渲染目标（`Surface?`；`null` = 清除）。
///
/// # Safety
///
/// 仅由 JNI 运行时按契约 §2 的 ABI 调用；`surface` 必须是 `android.view.Surface` 或 null。
#[no_mangle]
pub extern "system" fn Java_com_rusttunnel_controller_NativeBridge_nativeSetSurface<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    surface: JObject<'local>,
) {
    let Some(slot) = lookup(handle) else {
        return;
    };
    if surface.is_null() {
        slot.render.set_surface(None);
        return;
    }
    // SAFETY: `surface` 是本线程 JNIEnv 上的局部引用，在本次调用期间有效；
    // `NativeWindow::from_surface` 内部 `ANativeWindow_fromSurface` 会持有自己的引用。
    if let Some(window) =
        unsafe { NativeWindow::from_surface(env.get_native_interface(), surface.as_raw()) }
    {
        slot.render.set_surface(Some(window));
    } else {
        tracing::warn!("Surface 无法转换成 ANativeWindow");
        slot.bridge.error("Surface 无法转换成 ANativeWindow");
    }
}

/// 连接某个在线客户端；`mode`：0=Control、1=ViewOnly、2=FileTransfer。
///
/// # Safety
///
/// 仅由 JNI 运行时按契约 §2 的 ABI 调用。
#[no_mangle]
pub extern "system" fn Java_com_rusttunnel_controller_NativeBridge_nativeConnect<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    target_client: JString<'local>,
    mode: jint,
) {
    let Some(slot) = lookup(handle) else {
        return;
    };
    let Some(target) = read_string(&mut env, &target_client) else {
        return;
    };
    let mode = match mode {
        0 => RemoteMode::Control,
        1 => RemoteMode::ViewOnly,
        2 => RemoteMode::FileTransfer,
        other => {
            slot.bridge
                .error(&format!("nativeConnect 收到未知 mode={other}"));
            return;
        }
    };
    slot.set_target(&target);
    if let Err(err) = slot.commands.connect(&target, mode) {
        slot.bridge.error(&format!("连接失败：{err}"));
    }
}

/// 取消当前会话。
///
/// # Safety
///
/// 仅由 JNI 运行时按契约 §2 的 ABI 调用。
#[no_mangle]
pub extern "system" fn Java_com_rusttunnel_controller_NativeBridge_nativeCancelSession<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    if let Some(slot) = lookup(handle) {
        slot.commands.cancel_session();
    }
}

/// 鼠标移动（`x`/`y` 为**远端像素**坐标）。
///
/// # Safety
///
/// 仅由 JNI 运行时按契约 §2 的 ABI 调用。
#[no_mangle]
pub extern "system" fn Java_com_rusttunnel_controller_NativeBridge_nativeSendMouseMove<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    x: jfloat,
    y: jfloat,
    display: jint,
) {
    if let Some(slot) = lookup(handle) {
        slot.commands.send_input(InputEvent::MouseMove {
            x: round_to_i32(x),
            y: round_to_i32(y),
            display: u32::try_from(display).unwrap_or(0),
        });
    }
}

/// 鼠标按键；`button`：0=左、1=右、2=中。
///
/// `display` 在协议里不随按键事件发送（`InputEvent::MouseButton` 没有该字段），
/// 保留参数只为与契约签名一致。
///
/// # Safety
///
/// 仅由 JNI 运行时按契约 §2 的 ABI 调用。
#[no_mangle]
pub extern "system" fn Java_com_rusttunnel_controller_NativeBridge_nativeSendMouseButton<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    button: jint,
    pressed: jboolean,
    _display: jint,
) {
    let Some(slot) = lookup(handle) else {
        return;
    };
    let button = match button {
        0 => MouseButton::Left,
        1 => MouseButton::Right,
        2 => MouseButton::Middle,
        other => {
            slot.bridge
                .error(&format!("nativeSendMouseButton 收到未知 button={other}"));
            return;
        }
    };
    slot.commands.send_input(InputEvent::MouseButton {
        button,
        pressed: pressed != 0,
    });
}

/// 滚轮增量（一格 = 1.0）。
///
/// # Safety
///
/// 仅由 JNI 运行时按契约 §2 的 ABI 调用。
#[no_mangle]
pub extern "system" fn Java_com_rusttunnel_controller_NativeBridge_nativeSendWheel<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    dx: jfloat,
    dy: jfloat,
) {
    if let Some(slot) = lookup(handle) {
        slot.commands.send_input(InputEvent::Wheel {
            dx: round_to_i32(dx),
            dy: round_to_i32(dy),
        });
    }
}

/// 键盘事件；`code` 是协议 v5 键位码，`modifiers` 是契约 §4 的位图。
///
/// # Safety
///
/// 仅由 JNI 运行时按契约 §2 的 ABI 调用。
#[no_mangle]
pub extern "system" fn Java_com_rusttunnel_controller_NativeBridge_nativeSendKey<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    code: jint,
    pressed: jboolean,
    modifiers: jint,
) {
    let Some(slot) = lookup(handle) else {
        return;
    };
    let Ok(code) = u32::try_from(code) else {
        tracing::debug!("nativeSendKey 收到负键位码 {code}");
        return;
    };
    slot.commands.send_input(InputEvent::Key(KeyEvent {
        code,
        pressed: pressed != 0,
        text: None,
        modifiers: u8::try_from(modifiers).unwrap_or(0),
    }));
}

/// Android `KeyEvent.getKeyCode()` → 协议键位码；不支持返回 `-1`（纯函数，不需要句柄）。
///
/// # Safety
///
/// 仅由 JNI 运行时按契约 §2 的 ABI 调用。
#[no_mangle]
pub extern "system" fn Java_com_rusttunnel_controller_NativeBridge_nativeMapKey<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    android_key_code: jint,
) -> jint {
    map_android_key(android_key_code)
        .and_then(|code| i32::try_from(code).ok())
        .unwrap_or(-1)
}

/// Android `KeyEvent.getMetaState()` → 协议 modifiers 位图（纯函数）。
///
/// # Safety
///
/// 仅由 JNI 运行时按契约 §2 的 ABI 调用。
#[no_mangle]
pub extern "system" fn Java_com_rusttunnel_controller_NativeBridge_nativeModifiers<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    android_meta_state: jint,
) -> jint {
    i32::from(modifiers_from_meta_state(android_meta_state))
}

/// 以 `keycode::TEXT` 遗留通路发送文本（软键盘/中文输入）。
///
/// 被控端把 `text` 逐字符按 Unicode 注入（`KEYEVENTF_UNICODE`），绕过键盘布局与输入法。
///
/// # Safety
///
/// 仅由 JNI 运行时按契约 §2 的 ABI 调用。
#[no_mangle]
pub extern "system" fn Java_com_rusttunnel_controller_NativeBridge_nativeSendText<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    text: JString<'local>,
) {
    let Some(slot) = lookup(handle) else {
        return;
    };
    let Some(text) = read_string(&mut env, &text) else {
        return;
    };
    if text.is_empty() {
        return;
    }
    slot.commands.send_input(InputEvent::Key(KeyEvent {
        code: keycode::TEXT,
        pressed: true,
        text: Some(text),
        modifiers: 0,
    }));
}

/// 画质档位；`preset`：0=Balanced、1=Smooth、2=Hd。
///
/// # Safety
///
/// 仅由 JNI 运行时按契约 §2 的 ABI 调用。
#[no_mangle]
pub extern "system" fn Java_com_rusttunnel_controller_NativeBridge_nativeSetQuality<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    preset: jint,
) {
    let Some(slot) = lookup(handle) else {
        return;
    };
    let preset = match preset {
        0 => QualityPreset::Balanced,
        1 => QualityPreset::Smooth,
        2 => QualityPreset::Hd,
        other => {
            slot.bridge
                .error(&format!("nativeSetQuality 收到未知 preset={other}"));
            return;
        }
    };
    slot.commands.set_quality(preset);
}

/// 帧率上限（15..=60）。
///
/// # Safety
///
/// 仅由 JNI 运行时按契约 §2 的 ABI 调用。
#[no_mangle]
pub extern "system" fn Java_com_rusttunnel_controller_NativeBridge_nativeSetFrameRate<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    fps: jint,
) {
    let Some(slot) = lookup(handle) else {
        return;
    };
    if let Err(err) = slot
        .commands
        .set_frame_rate(u8::try_from(fps).unwrap_or(u8::MAX))
    {
        slot.bridge.error(&format!("设置帧率失败：{err}"));
    }
}

/// 切换被控端显示器。
///
/// # Safety
///
/// 仅由 JNI 运行时按契约 §2 的 ABI 调用。
#[no_mangle]
pub extern "system" fn Java_com_rusttunnel_controller_NativeBridge_nativeSelectDisplay<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    display: jint,
) {
    if let Some(slot) = lookup(handle) {
        slot.commands
            .select_display(u32::try_from(display).unwrap_or(0));
    }
}

/// 写远端剪贴板。
///
/// # Safety
///
/// 仅由 JNI 运行时按契约 §2 的 ABI 调用。
#[no_mangle]
pub extern "system" fn Java_com_rusttunnel_controller_NativeBridge_nativeSendClipboard<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    text: JString<'local>,
) {
    let Some(slot) = lookup(handle) else {
        return;
    };
    if let Some(text) = read_string(&mut env, &text) {
        slot.commands.send_clipboard(text);
    }
}

/// 请求远端剪贴板（结果经 `onClipboard` 回调）。
///
/// # Safety
///
/// 仅由 JNI 运行时按契约 §2 的 ABI 调用。
#[no_mangle]
pub extern "system" fn Java_com_rusttunnel_controller_NativeBridge_nativeRequestClipboard<
    'local,
>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    if let Some(slot) = lookup(handle) {
        slot.commands.request_clipboard();
    }
}

/// 锁定被控端屏幕。
///
/// # Safety
///
/// 仅由 JNI 运行时按契约 §2 的 ABI 调用。
#[no_mangle]
pub extern "system" fn Java_com_rusttunnel_controller_NativeBridge_nativeLockScreen<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    if let Some(slot) = lookup(handle) {
        slot.commands.lock_screen();
    }
}

/// 当前状态 JSON（字段见契约 §5）；未知句柄返回 `null`。
///
/// # Safety
///
/// 仅由 JNI 运行时按契约 §2 的 ABI 调用。
#[no_mangle]
pub extern "system" fn Java_com_rusttunnel_controller_NativeBridge_nativeSnapshot<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jstring {
    let Some(slot) = lookup(handle) else {
        return std::ptr::null_mut();
    };
    let json = slot.snapshot_json();
    match env.new_string(json) {
        Ok(string) => string.into_raw(),
        Err(err) => {
            tracing::warn!("创建状态 JSON 的 Java 字符串失败：{err}");
            std::ptr::null_mut()
        }
    }
}
