package com.rusttunnel.controller

import android.content.Context
import com.rusttunnel.controller.model.ControllerState
import com.rusttunnel.controller.model.RemoteDisplay
import com.rusttunnel.controller.model.StateParser
import java.io.File
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.coroutines.flow.asStateFlow

/**
 * 进程级控制器宿主门面。
 *
 * - 宿主句柄（[handle]）是 Rust `nativeStart` 返回的不透明 `Long`，`0` 表示未启动；
 * - 所有原生调用都经 [Native]（库缺失/异常时不崩溃）；
 * - Rust 回调（[NativeEvents]）只在这里落成 `StateFlow`/`SharedFlow`，Compose 侧收集。
 */
object HostController {
    /** 远端画面逻辑尺寸（[NativeEvents.onFrameInfo]）。 */
    data class FrameInfo(val width: Int, val height: Int, val display: Int)

    /** 需要以 Snackbar / 对话框呈现的一次性事件。 */
    sealed interface Event {
        data class Info(val message: String) : Event
        data class Error(val message: String) : Event
        data class ClipboardReceived(val text: String) : Event
        data class SessionEnded(val reason: String) : Event
    }

    private lateinit var appContext: Context
    private val startLock = Any()

    private val _handle = MutableStateFlow(0L)
    val handle: StateFlow<Long> = _handle.asStateFlow()

    private val _state = MutableStateFlow(ControllerState())
    val state: StateFlow<ControllerState> = _state.asStateFlow()

    private val _frameInfo = MutableStateFlow(FrameInfo(0, 0, 0))
    val frameInfo: StateFlow<FrameInfo> = _frameInfo.asStateFlow()

    private val _displays = MutableStateFlow<List<RemoteDisplay>>(emptyList())
    val displays: StateFlow<List<RemoteDisplay>> = _displays.asStateFlow()

    private val _display = MutableStateFlow(0)
    val display: StateFlow<Int> = _display.asStateFlow()

    private val _cursor = MutableStateFlow(-1)
    val cursorShape: StateFlow<Int> = _cursor.asStateFlow()

    private val _quality = MutableStateFlow(0)
    val quality: StateFlow<Int> = _quality.asStateFlow()

    private val _frameRate = MutableStateFlow(30)
    val frameRate: StateFlow<Int> = _frameRate.asStateFlow()

    private val _events = MutableSharedFlow<Event>(extraBufferCapacity = 32)
    val events: SharedFlow<Event> = _events.asSharedFlow()

    @Volatile
    private var polling = false

    @Volatile
    private var pollGeneration = 0

    @Volatile
    private var lastEndNoticeAt = 0L

    fun attach(context: Context) {
        if (!::appContext.isInitialized) {
            appContext = context.applicationContext
        }
    }

    fun isRunning(): Boolean = _handle.value != 0L

    /**
     * 启动宿主。返回 `null` 表示成功，否则返回可直接展示的错误文案。
     * 建议在 IO 线程调用（Rust 侧要建 Tokio runtime 并起控制通道）。
     */
    fun start(
        server: String,
        token: String,
        clientName: String,
        tls: Boolean,
        insecure: Boolean,
    ): String? = synchronized(startLock) {
        if (_handle.value != 0L) return null
        if (!::appContext.isInitialized) return "应用上下文未初始化"
        if (!Native.loaded) {
            return "原生库 libandroid_jni.so 未加载：${Native.loadError ?: "未知原因"}"
        }
        val downloadDir = File(appContext.filesDir, "downloads")
        if (!downloadDir.exists() && !downloadDir.mkdirs()) {
            return "无法创建下载目录：${downloadDir.absolutePath}"
        }
        val handle = Native.start(
            server = server.trim(),
            token = token,
            clientName = clientName.trim(),
            downloadDir = downloadDir.absolutePath,
            tls = tls,
            tlsInsecure = insecure,
        )
        if (handle == 0L) {
            return Native.lastError?.let { "启动失败：$it" } ?: "启动失败（地址或 token 无效？）"
        }
        _handle.value = handle
        _state.value = ControllerState(connection = "connecting")
        _frameInfo.value = FrameInfo(0, 0, 0)
        _displays.value = emptyList()
        _display.value = 0
        _cursor.value = -1
        _quality.value = 0
        _frameRate.value = 30
        refreshSnapshot()
        startPolling()
        null
    }

    /** 停止宿主；建议在 IO 线程调用（`nativeStop` 最多等待数秒收尾）。 */
    fun stop() {
        synchronized(startLock) {
            val handle = _handle.value
            if (handle == 0L) return
            stopPolling()
            Native.setSurface(handle, null)
            Native.stop(handle)
            _handle.value = 0L
            _state.value = ControllerState()
            _frameInfo.value = FrameInfo(0, 0, 0)
            _displays.value = emptyList()
            _display.value = 0
            _cursor.value = -1
        }
    }

    // ── 命令 ────────────────────────────────────────────────────────────────

    fun connect(peer: com.rusttunnel.controller.model.PeerInfo, mode: Int = 0) {
        val handle = _handle.value
        if (handle == 0L) return
        Native.connect(handle, peer.clientName, mode)
    }

    fun cancelSession() {
        val handle = _handle.value
        if (handle == 0L) return
        Native.cancelSession(handle)
    }

    fun sendMouseMove(x: Float, y: Float) {
        val handle = _handle.value
        if (handle == 0L) return
        Native.mouseMove(handle, x, y, _display.value)
    }

    fun sendMouseButton(button: Int, pressed: Boolean) {
        val handle = _handle.value
        if (handle == 0L) return
        Native.mouseButton(handle, button, pressed, _display.value)
    }

    fun sendWheel(dy: Float) {
        val handle = _handle.value
        if (handle == 0L) return
        Native.wheel(handle, 0f, dy)
    }

    fun sendKey(code: Int, pressed: Boolean, modifiers: Int) {
        val handle = _handle.value
        if (handle == 0L) return
        Native.sendKey(handle, code, pressed, modifiers)
    }

    /** 把 Android `KeyEvent` 的键码映射后按「按下+抬起」点按发送（软键盘用）。 */
    fun sendAndroidKeyTap(androidKeyCode: Int) {
        val handle = _handle.value
        if (handle == 0L) return
        val code = Native.mapKey(androidKeyCode)
        if (code < 0) return
        val modifiers = Native.modifiers(0)
        Native.sendKey(handle, code, true, modifiers)
        Native.sendKey(handle, code, false, modifiers)
    }

    fun sendText(text: String) {
        if (text.isEmpty()) return
        val handle = _handle.value
        if (handle == 0L) return
        Native.sendText(handle, text)
    }

    fun setQuality(preset: Int) {
        val handle = _handle.value
        if (handle == 0L) return
        val clamped = preset.coerceIn(0, 2)
        _quality.value = clamped
        Native.setQuality(handle, clamped)
    }

    fun setFrameRate(fps: Int) {
        val handle = _handle.value
        if (handle == 0L) return
        val clamped = fps.coerceIn(15, 60)
        _frameRate.value = clamped
        Native.setFrameRate(handle, clamped)
    }

    fun selectDisplay(display: Int) {
        val handle = _handle.value
        if (handle == 0L) return
        _display.value = display
        Native.selectDisplay(handle, display)
    }

    fun sendClipboard(text: String) {
        val handle = _handle.value
        if (handle == 0L) return
        Native.sendClipboard(handle, text)
    }

    fun requestClipboard() {
        val handle = _handle.value
        if (handle == 0L) return
        Native.requestClipboard(handle)
    }

    fun lockScreen() {
        val handle = _handle.value
        if (handle == 0L) return
        Native.lockScreen(handle)
    }

    fun refreshSnapshot() {
        val handle = _handle.value
        if (handle == 0L) return
        val json = Native.snapshot(handle)
        if (json.isNotBlank()) onState(json)
    }

    /** 从界面发一条提示（Snackbar）。 */
    fun info(message: String) {
        _events.tryEmit(Event.Info(message))
    }

    // ── Rust 回调 ───────────────────────────────────────────────────────────

    fun onState(json: String) {
        val previous = _state.value
        val parsed = StateParser.parse(json, previous)
        _state.value = parsed
        if (parsed.remoteWidth > 0 && parsed.remoteHeight > 0 && _frameInfo.value.width == 0) {
            _frameInfo.value = FrameInfo(
                parsed.remoteWidth,
                parsed.remoteHeight,
                _display.value,
            )
        }
        // Rust 的快照带 quality 字符串；以引擎为准同步工具条选中态。
        val qualityPreset = when (parsed.quality) {
            "smooth" -> 1
            "balanced" -> 0
            "hd" -> 2
            else -> null
        }
        if (qualityPreset != null && _quality.value != qualityPreset) {
            _quality.value = qualityPreset
        }
        if (parsed.fps in 15..60 && _frameRate.value != parsed.fps) {
            _frameRate.value = parsed.fps
        }
        if (parsed.display >= 0 && _display.value != parsed.display) {
            _display.value = parsed.display
        }
        val sessionChanged = previous.session != parsed.session
        if (sessionChanged && parsed.session == "ended") {
            notifySessionEnded(parsed.reason ?: "会话已结束")
        } else if (sessionChanged && previous.isActiveSession && parsed.session == "none") {
            notifySessionEnded(parsed.reason ?: "会话已结束")
        }
        if (sessionChanged && !parsed.isActiveSession) {
            _frameInfo.value = FrameInfo(0, 0, _display.value)
        }
    }

    fun onFrameInfo(width: Int, height: Int, display: Int) {
        if (width <= 0 || height <= 0) return
        _frameInfo.value = FrameInfo(width, height, display)
        if (display >= 0 && _display.value != display) {
            _display.value = display
        }
    }

    fun onDisplays(json: String) {
        val parsed = StateParser.parseDisplays(json)
        if (parsed.isNotEmpty()) {
            _displays.value = parsed
        }
    }

    fun onClipboard(text: String) {
        _events.tryEmit(Event.ClipboardReceived(text))
    }

    fun onCursor(shape: Int) {
        _cursor.value = shape
    }

    fun onSessionEnded(reason: String) {
        notifySessionEnded(reason.ifBlank { "会话已结束" })
    }

    fun onError(message: String) {
        _events.tryEmit(Event.Error(message))
    }

    private fun notifySessionEnded(reason: String) {
        val now = System.currentTimeMillis()
        if (now - lastEndNoticeAt < 2000L) return
        lastEndNoticeAt = now
        _events.tryEmit(Event.SessionEnded(reason))
    }

    // ── 快照轮询（onState 是推送，这里兜底；每秒一次）─────────────────────

    private fun startPolling() {
        if (polling) return
        polling = true
        val generation = ++pollGeneration
        Thread({
            while (polling && generation == pollGeneration) {
                try {
                    Thread.sleep(1000)
                } catch (_: InterruptedException) {
                    break
                }
                val handle = _handle.value
                if (handle == 0L) break
                val json = Native.snapshot(handle)
                if (json.isNotBlank() && json.trimStart().startsWith("{")) {
                    onState(json)
                }
            }
        }, "rust-tunnel-snapshot").apply { isDaemon = true }.start()
    }

    private fun stopPolling() {
        polling = false
        pollGeneration++
    }
}
