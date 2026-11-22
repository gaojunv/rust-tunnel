package com.rusttunnel.controller

import android.view.Surface

/**
 * JNI 方法声明。
 *
 * 类名、包名、方法名与签名必须与 `android/JNI_CONTRACT.md` 逐字一致——Rust 侧
 * （`crates/android-jni`）按这些名字导出符号。任何改动都要同步契约与 Rust。
 */
@Suppress("TooManyFunctions")
object NativeBridge {
    @JvmStatic
    external fun nativeStart(
        server: String,
        token: String,
        clientName: String,
        downloadDir: String,
        tls: Boolean,
        tlsInsecure: Boolean,
    ): Long

    @JvmStatic
    external fun nativeStop(handle: Long)

    @JvmStatic
    external fun nativeSetSurface(handle: Long, surface: Surface?)

    @JvmStatic
    external fun nativeConnect(handle: Long, targetClient: String, mode: Int)

    @JvmStatic
    external fun nativeCancelSession(handle: Long)

    @JvmStatic
    external fun nativeSendMouseMove(handle: Long, x: Float, y: Float, display: Int)

    @JvmStatic
    external fun nativeSendMouseButton(handle: Long, button: Int, pressed: Boolean, display: Int)

    @JvmStatic
    external fun nativeSendWheel(handle: Long, dx: Float, dy: Float)

    @JvmStatic
    external fun nativeSendKey(handle: Long, code: Int, pressed: Boolean, modifiers: Int)

    @JvmStatic
    external fun nativeMapKey(androidKeyCode: Int): Int

    @JvmStatic
    external fun nativeModifiers(androidMetaState: Int): Int

    @JvmStatic
    external fun nativeSendText(handle: Long, text: String)

    @JvmStatic
    external fun nativeSetQuality(handle: Long, preset: Int)

    @JvmStatic
    external fun nativeSetFrameRate(handle: Long, fps: Int)

    @JvmStatic
    external fun nativeSelectDisplay(handle: Long, display: Int)

    @JvmStatic
    external fun nativeSendClipboard(handle: Long, text: String)

    @JvmStatic
    external fun nativeRequestClipboard(handle: Long)

    @JvmStatic
    external fun nativeLockScreen(handle: Long)

    @JvmStatic
    external fun nativeSnapshot(handle: Long): String
}

/**
 * 原生库的安全门面：库缺失或符号解析失败时不崩溃，返回中性值并记录原因。
 *
 * `System.loadLibrary("android_jni")` 在 [App.onCreate] 里调用一次（契约 §1）；
 * 失败时界面会显示原因，配置页的「启动」按钮保持禁用。
 */
@Suppress("TooManyFunctions")
object Native {
    @Volatile
    var loaded: Boolean = false
        private set

    @Volatile
    var loadError: String? = null
        private set

    /** 最近一次调用原生方法抛出的异常（用于界面上给出可读错误）。 */
    @Volatile
    var lastError: String? = null
        private set

    fun load() {
        if (loaded) return
        try {
            System.loadLibrary("android_jni")
            loaded = true
            loadError = null
        } catch (t: Throwable) {
            loadError = t.message ?: t.javaClass.simpleName
        }
    }

    private inline fun <T> guard(default: T, block: () -> T): T {
        if (!loaded) return default
        return try {
            block()
        } catch (t: Throwable) {
            lastError = t.message ?: t.javaClass.simpleName
            default
        }
    }

    fun start(
        server: String,
        token: String,
        clientName: String,
        downloadDir: String,
        tls: Boolean,
        tlsInsecure: Boolean,
    ): Long {
        lastError = null
        return guard(0L) {
            NativeBridge.nativeStart(server, token, clientName, downloadDir, tls, tlsInsecure)
        }
    }

    fun stop(handle: Long) = guard(Unit) { NativeBridge.nativeStop(handle) }

    fun setSurface(handle: Long, surface: Surface?) =
        guard(Unit) { NativeBridge.nativeSetSurface(handle, surface) }

    fun connect(handle: Long, target: String, mode: Int) =
        guard(Unit) { NativeBridge.nativeConnect(handle, target, mode) }

    fun cancelSession(handle: Long) = guard(Unit) { NativeBridge.nativeCancelSession(handle) }

    fun mouseMove(handle: Long, x: Float, y: Float, display: Int) =
        guard(Unit) { NativeBridge.nativeSendMouseMove(handle, x, y, display) }

    fun mouseButton(handle: Long, button: Int, pressed: Boolean, display: Int) =
        guard(Unit) { NativeBridge.nativeSendMouseButton(handle, button, pressed, display) }

    fun wheel(handle: Long, dx: Float, dy: Float) =
        guard(Unit) { NativeBridge.nativeSendWheel(handle, dx, dy) }

    fun sendKey(handle: Long, code: Int, pressed: Boolean, modifiers: Int) =
        guard(Unit) { NativeBridge.nativeSendKey(handle, code, pressed, modifiers) }

    /** 纯函数，不需要 handle；不支持的键返回 `-1`。 */
    fun mapKey(androidKeyCode: Int): Int = guard(-1) { NativeBridge.nativeMapKey(androidKeyCode) }

    /** 纯函数：Android `metaState` → 协议 modifiers 位图。 */
    fun modifiers(androidMetaState: Int): Int =
        guard(0) { NativeBridge.nativeModifiers(androidMetaState) }

    fun sendText(handle: Long, text: String) = guard(Unit) { NativeBridge.nativeSendText(handle, text) }

    fun setQuality(handle: Long, preset: Int) = guard(Unit) { NativeBridge.nativeSetQuality(handle, preset) }

    fun setFrameRate(handle: Long, fps: Int) = guard(Unit) { NativeBridge.nativeSetFrameRate(handle, fps) }

    fun selectDisplay(handle: Long, display: Int) =
        guard(Unit) { NativeBridge.nativeSelectDisplay(handle, display) }

    fun sendClipboard(handle: Long, text: String) =
        guard(Unit) { NativeBridge.nativeSendClipboard(handle, text) }

    fun requestClipboard(handle: Long) = guard(Unit) { NativeBridge.nativeRequestClipboard(handle) }

    fun lockScreen(handle: Long) = guard(Unit) { NativeBridge.nativeLockScreen(handle) }

    fun snapshot(handle: Long): String = guard("") { NativeBridge.nativeSnapshot(handle) }
}
