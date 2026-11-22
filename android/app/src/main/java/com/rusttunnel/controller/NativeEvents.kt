package com.rusttunnel.controller

/**
 * Rust → Kotlin 的回调入口（契约 §3）。
 *
 * Rust 在 `nativeStart` 时缓存 `JavaVM` 与本类的全局引用，回调发生在**引擎线程**上；
 * 这里只把数据投递给 [HostController]，由它用 `StateFlow` / `SharedFlow` 交给 UI
 * （收集端在 Compose 主线程，因此不需要在这里 `runOnUiThread`）。
 *
 * 方法必须是静态的：Rust 用 `CallStaticVoidMethod` 按名字查找。
 */
object NativeEvents {
    @JvmStatic
    fun onState(json: String) {
        HostController.onState(json)
    }

    @JvmStatic
    fun onFrameInfo(width: Int, height: Int, display: Int) {
        HostController.onFrameInfo(width, height, display)
    }

    @JvmStatic
    fun onDisplays(json: String) {
        HostController.onDisplays(json)
    }

    @JvmStatic
    fun onClipboard(text: String) {
        HostController.onClipboard(text)
    }

    @JvmStatic
    fun onCursor(shape: Int) {
        HostController.onCursor(shape)
    }

    @JvmStatic
    fun onSessionEnded(reason: String) {
        HostController.onSessionEnded(reason)
    }

    @JvmStatic
    fun onError(message: String) {
        HostController.onError(message)
    }
}
