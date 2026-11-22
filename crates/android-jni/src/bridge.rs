//! Rust → Kotlin 回调桥（契约 §3）。
//!
//! 所有回调都打在 `com.rusttunnel.controller.NativeEvents` 的**静态方法**上。
//! 类引用在 `nativeStart`（Java 线程、App class loader 可见）里取一次并转成全局引用：
//! 由原生代码 attach 出来的线程调用 `FindClass` 时用的是系统 class loader，找不到 App 的类，
//! 所以**不能**在回调线程里按名字查类。
//!
//! 回调来自引擎的运行时线程（tokio worker）或渲染/镜像任务，均非 Java 线程；
//! 每次回调按需 `AttachCurrentThread`（已是附加线程时为嵌套附加，不会提前 detach）。
//! 回调失败（类/方法不在、Java 端抛异常）只记日志并清掉待处理异常，绝不让异常穿越 JNI
//! 边界——那会让下一次 JNI 调用看到「pending exception」而行为不可预期。

use jni::objects::{GlobalRef, JValue};
use jni::{JNIEnv, JavaVM};

/// 回调类（Kotlin 侧全限定名，JNI 描述符用 `/` 分隔）。
const EVENTS_CLASS: &str = "com/rusttunnel/controller/NativeEvents";

/// Rust → Kotlin 回调桥。
pub(crate) struct JniBridge {
    /// 进程级 JavaVM（回调线程用它附加）。
    vm: JavaVM,
    /// `NativeEvents` 类的全局引用。
    events: GlobalRef,
}

impl JniBridge {
    /// 在 `nativeStart` 的 JNI 线程上构造（此时 class loader 正确）。
    ///
    /// # Errors
    ///
    /// 取 JavaVM、查找 `NativeEvents` 类或创建全局引用失败时返回 JNI 错误。
    pub(crate) fn new(env: &mut JNIEnv<'_>) -> jni::errors::Result<Self> {
        let vm = env.get_java_vm()?;
        let class = env.find_class(EVENTS_CLASS)?;
        let events = env.new_global_ref(class)?;
        Ok(Self { vm, events })
    }

    /// `onState(json)`：控制通道/会话状态变化（已节流）。
    pub(crate) fn state(&self, json: &str) {
        self.call_string("onState", json);
    }

    /// `onDisplays(json)`：显示器列表。
    pub(crate) fn displays(&self, json: &str) {
        self.call_string("onDisplays", json);
    }

    /// `onClipboard(text)`：远端剪贴板内容。
    pub(crate) fn clipboard(&self, text: &str) {
        self.call_string("onClipboard", text);
    }

    /// `onSessionEnded(reason)`：会话结束原因。
    pub(crate) fn session_ended(&self, reason: &str) {
        self.call_string("onSessionEnded", reason);
    }

    /// `onError(message)`：可恢复/不可恢复错误提示。
    pub(crate) fn error(&self, message: &str) {
        self.call_string("onError", message);
    }

    /// `onCursor(shape)`：光标形状（协议 v9 判别式；`-1` = 未知）。
    pub(crate) fn cursor(&self, shape: i32) {
        self.call("onCursor", "(I)V", &[JValue::Int(shape)]);
    }

    /// `onFrameInfo(width, height, display)`：远端画面逻辑尺寸变化。
    pub(crate) fn frame_info(&self, width: i32, height: i32, display: i32) {
        self.call(
            "onFrameInfo",
            "(III)V",
            &[
                JValue::Int(width),
                JValue::Int(height),
                JValue::Int(display),
            ],
        );
    }

    /// 以 `(Ljava/lang/String;)V` 签名调用一个回调。
    fn call_string(&self, method: &str, text: &str) {
        let Ok(mut guard) = self.vm.attach_current_thread() else {
            tracing::warn!("回调 {method} 失败：无法附加到 JVM");
            return;
        };
        let env = &mut *guard;
        let result = env
            .new_string(text)
            .and_then(|string| {
                env.call_static_method(
                    &self.events,
                    method,
                    "(Ljava/lang/String;)V",
                    &[JValue::Object(&string)],
                )
            })
            .map(|_| ());
        if let Err(err) = result {
            tracing::debug!("回调 {method} 失败：{err}");
        }
        clear_exception(env);
    }

    /// 调用任意签名的回调。
    fn call(&self, method: &str, signature: &str, args: &[JValue<'_, '_>]) {
        let Ok(mut guard) = self.vm.attach_current_thread() else {
            tracing::warn!("回调 {method} 失败：无法附加到 JVM");
            return;
        };
        let env = &mut *guard;
        if let Err(err) = env.call_static_method(&self.events, method, signature, args) {
            tracing::debug!("回调 {method} 失败：{err}");
        }
        clear_exception(env);
    }
}

/// 清掉 Java 异常，避免它在下一次 JNI 调用时被误认为「本次调用的异常」。
fn clear_exception(env: &mut JNIEnv<'_>) {
    if let Ok(true) = env.exception_check() {
        // `JThrowable` 不实现 `Debug`（jni 0.21 的引用类型），故只记一条通用日志。
        tracing::warn!("Kotlin 回调抛出未捕获异常，已清除（避免污染后续 JNI 调用）");
        let _ = env.exception_clear();
    }
}
