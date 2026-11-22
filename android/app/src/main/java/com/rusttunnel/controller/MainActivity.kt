package com.rusttunnel.controller

import android.os.Bundle
import android.view.KeyEvent
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import com.rusttunnel.controller.ui.AppRoot
import com.rusttunnel.controller.ui.RustTunnelTheme

/**
 * 唯一的 Activity：Compose 界面 + 硬件键盘转发。
 *
 * 旋转/分屏等配置变更由 Manifest 的 `configChanges` 吃掉，Activity 不重建；
 * 宿主句柄在 [App] 上，因此 Surface 重建不会影响会话。
 */
class MainActivity : ComponentActivity() {
    /** 已转达「按下」的协议键位码（窗口失焦/退后台时回收，避免远端卡键）。 */
    private val forwardedKeys = mutableSetOf<Int>()
    private var lastModifiers = 0

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        setContent {
            RustTunnelTheme {
                AppRoot()
            }
        }
    }

    /**
     * 硬件键盘：会话已建立时把按键映射成协议键位码转发，未映射的键交还系统。
     *
     * 软键盘的输入走 [com.rusttunnel.controller.input.RemoteTextInputView] 的
     * `InputConnection`，不会经过这里，因此不会重复发送。
     */
    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
        // 返回键永远留给系统/Compose（BackHandler 负责弹断开确认）。
        if (event.keyCode == KeyEvent.KEYCODE_BACK) {
            return super.dispatchKeyEvent(event)
        }
        val handle = HostController.handle.value
        val active = handle != 0L && HostController.state.value.isActiveSession
        if (active && Native.loaded) {
            val code = Native.mapKey(event.keyCode)
            if (code >= 0) {
                val modifiers = Native.modifiers(event.metaState)
                lastModifiers = modifiers
                when (event.action) {
                    KeyEvent.ACTION_DOWN -> {
                        // 自动重复照原样再发「按下」：与桌面 InputGate 一致——注入的 keydown
                        // 是否在远端触发重复取决于被控端前台程序，折成单次会丢掉长按重复。
                        forwardedKeys.add(code)
                        Native.sendKey(handle, code, true, modifiers)
                    }

                    KeyEvent.ACTION_UP -> {
                        forwardedKeys.remove(code)
                        Native.sendKey(handle, code, false, modifiers)
                    }
                }
                return true
            }
        }
        return super.dispatchKeyEvent(event)
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        if (!hasFocus) releaseForwardedKeys()
        super.onWindowFocusChanged(hasFocus)
    }

    override fun onPause() {
        releaseForwardedKeys()
        super.onPause()
    }

    /** 回收所有「已转达按下」的键：Android 在失焦时不会补发 ACTION_UP。 */
    private fun releaseForwardedKeys() {
        if (forwardedKeys.isEmpty()) return
        val handle = HostController.handle.value
        if (handle != 0L) {
            for (code in forwardedKeys.toList()) {
                Native.sendKey(handle, code, false, lastModifiers)
            }
        }
        forwardedKeys.clear()
    }
}
