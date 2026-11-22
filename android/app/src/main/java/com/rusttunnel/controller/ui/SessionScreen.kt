package com.rusttunnel.controller.ui

import android.app.Activity
import android.content.Context
import android.content.ContextWrapper
import android.graphics.Color as AndroidColor
import android.view.PointerIcon
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.WindowManager
import android.widget.FrameLayout
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.background
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.lifecycle.compose.LocalLifecycleOwner
import com.rusttunnel.controller.HostController
import com.rusttunnel.controller.Native
import com.rusttunnel.controller.input.RemoteInputView
import com.rusttunnel.controller.input.RemoteTextInputView
import com.rusttunnel.controller.model.ControllerState
import com.rusttunnel.controller.model.RemoteDisplay

/** 会话页持有的原生 View 引用（在 [AndroidView] 的 factory 里填充）。 */
private class SessionViewRefs {
    var surfaceView: SurfaceView? = null
    var inputView: RemoteInputView? = null
    var textView: RemoteTextInputView? = null
}

/**
 * 会话页：全屏渲染 Surface + 顶部/底部半透明工具条。
 *
 * - Surface 的创建/销毁/Activity 前后台都走 `nativeSetSurface(handle, surface|null)`；
 * - 手势层（[RemoteInputView]）覆盖整个 Surface，负责把触摸换算成远端像素坐标；
 * - 全部工具条按钮都直接调用 [HostController]（内部再进 JNI）。
 */
@Composable
fun SessionScreen(modifier: Modifier = Modifier) {
    val context = LocalContext.current
    val activity = remember(context) { context.findActivity() }
    val state by HostController.state.collectAsState()
    val frameInfo by HostController.frameInfo.collectAsState()
    val displays by HostController.displays.collectAsState()
    val currentDisplay by HostController.display.collectAsState()
    val quality by HostController.quality.collectAsState()
    val frameRate by HostController.frameRate.collectAsState()
    val cursor by HostController.cursorShape.collectAsState()

    val views = remember { SessionViewRefs() }
    val lifecycleOwner = LocalLifecycleOwner.current

    var statsVisible by remember { mutableStateOf(true) }
    var zoom by remember { mutableStateOf(1f) }
    var confirmDisconnect by remember { mutableStateOf(false) }
    var confirmLock by remember { mutableStateOf(false) }
    var displayMenuOpen by remember { mutableStateOf(false) }

    fun attachSurface() {
        val surface = views.surfaceView?.holder?.surface ?: return
        val handle = HostController.handle.value
        if (handle != 0L && surface.isValid) {
            Native.setSurface(handle, surface)
        }
    }

    fun detachSurface() {
        val handle = HostController.handle.value
        if (handle != 0L) {
            Native.setSurface(handle, null)
        }
    }

    // 会话期间保持屏幕常亮 + 沉浸式（工具条仍显示在画面之上）。
    DisposableEffect(activity) {
        activity?.window?.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        activity?.applyImmersive()
        onDispose {
            activity?.window?.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
            activity?.clearImmersive()
        }
    }

    // SurfaceView 的 surface 在前后台切换时会被销毁重建；Activity 生命周期也兜一层。
    DisposableEffect(lifecycleOwner) {
        val observer = LifecycleEventObserver { _, event ->
            when (event) {
                Lifecycle.Event.ON_STOP -> detachSurface()
                Lifecycle.Event.ON_START -> attachSurface()
                else -> Unit
            }
        }
        lifecycleOwner.lifecycle.addObserver(observer)
        onDispose {
            lifecycleOwner.lifecycle.removeObserver(observer)
            detachSurface()
        }
    }

    // 光标形状跟随（协议 v9；只影响本机指针图标）。
    LaunchedEffect(cursor) {
        views.inputView?.pointerIcon = pointerIconFor(context, cursor)
    }

    BackHandler(enabled = true) { confirmDisconnect = true }

    Box(
        modifier = modifier
            .fillMaxSize()
            .background(Color.Black),
    ) {
        AndroidView(
            modifier = Modifier.fillMaxSize(),
            factory = { ctx ->
                FrameLayout(ctx).apply {
                    setBackgroundColor(AndroidColor.BLACK)
                    val surface = SurfaceView(ctx)
                    val input = RemoteInputView(ctx)
                    val text = RemoteTextInputView(ctx)

                    input.bindSurface(surface)
                    input.listener = object : RemoteInputView.Listener {
                        override fun onRemoteMove(x: Float, y: Float) {
                            HostController.sendMouseMove(x, y)
                        }

                        override fun onRemoteButton(button: Int, pressed: Boolean) {
                            HostController.sendMouseButton(button, pressed)
                        }

                        override fun onRemoteWheel(dy: Float) {
                            HostController.sendWheel(dy)
                        }
                    }
                    input.onZoomChanged = { zoom = it }

                    addView(
                        surface,
                        FrameLayout.LayoutParams(
                            FrameLayout.LayoutParams.MATCH_PARENT,
                            FrameLayout.LayoutParams.MATCH_PARENT,
                        ),
                    )
                    addView(
                        input,
                        FrameLayout.LayoutParams(
                            FrameLayout.LayoutParams.MATCH_PARENT,
                            FrameLayout.LayoutParams.MATCH_PARENT,
                        ),
                    )
                    addView(
                        text,
                        FrameLayout.LayoutParams(1, 1),
                    )

                    views.surfaceView = surface
                    views.inputView = input
                    views.textView = text

                    surface.holder.addCallback(object : SurfaceHolder.Callback {
                        override fun surfaceCreated(holder: SurfaceHolder) {
                            val handle = HostController.handle.value
                            if (handle != 0L) {
                                Native.setSurface(handle, holder.surface)
                            }
                        }

                        override fun surfaceChanged(
                            holder: SurfaceHolder,
                            format: Int,
                            width: Int,
                            height: Int,
                        ) {
                            val handle = HostController.handle.value
                            if (handle != 0L) {
                                Native.setSurface(handle, holder.surface)
                            }
                        }

                        override fun surfaceDestroyed(holder: SurfaceHolder) {
                            val handle = HostController.handle.value
                            if (handle != 0L) {
                                Native.setSurface(handle, null)
                            }
                        }
                    })
                }
            },
            update = {
                views.inputView?.setRemoteSize(frameInfo.width, frameInfo.height)
            },
        )

        SessionTopBar(
            state = state,
            statsVisible = statsVisible,
            onToggleStats = { statsVisible = !statsVisible },
            onDisconnect = { confirmDisconnect = true },
            modifier = Modifier.align(Alignment.TopCenter),
        )

        if (frameInfo.width == 0) {
            Text(
                text = "等待画面…",
                color = Color.White.copy(alpha = 0.7f),
                style = MaterialTheme.typography.bodyMedium,
                modifier = Modifier.align(Alignment.Center),
            )
        }

        if (statsVisible) {
            StatsPanel(
                state = state,
                frameWidth = frameInfo.width,
                frameHeight = frameInfo.height,
                display = currentDisplay,
                cursor = cursor,
                zoom = zoom,
                modifier = Modifier
                    .align(Alignment.CenterEnd)
                    .padding(end = 10.dp),
            )
        }

        SessionBottomBar(
            modifier = Modifier.align(Alignment.BottomCenter),
            quality = quality,
            frameRate = frameRate,
            display = currentDisplay,
            displays = displays,
            displayMenuOpen = displayMenuOpen,
            onDisplayMenuOpenChange = { displayMenuOpen = it },
            zoom = zoom,
            onQuality = { HostController.setQuality(it) },
            onFrameRate = { HostController.setFrameRate(it) },
            onSelectDisplay = { HostController.selectDisplay(it) },
            onLockScreen = { confirmLock = true },
            onSendClipboard = {
                val text = readClipboardText(context)
                if (text == null) {
                    HostController.info("本机剪贴板为空或不是文本")
                } else {
                    HostController.sendClipboard(text)
                    HostController.info("已发送本机剪贴板（${text.length} 字）")
                }
            },
            onRequestClipboard = { HostController.requestClipboard() },
            onKeyboard = {
                val text = views.textView
                if (text == null) {
                    HostController.info("键盘输入视图尚未就绪")
                } else if (text.isKeyboardVisible()) {
                    text.hideKeyboard()
                } else {
                    text.showKeyboard()
                }
            },
            onResetZoom = { views.inputView?.resetZoom() },
        )
    }

    if (confirmDisconnect) {
        ConfirmDialog(
            title = "断开远程会话？",
            text = "画面与控制会立即结束（被控端不受影响）。会话中未完成的文件传输会中断。",
            confirmLabel = "断开",
            onConfirm = { HostController.cancelSession() },
            onDismiss = { confirmDisconnect = false },
        )
    }

    if (confirmLock) {
        AlertDialog(
            onDismissRequest = { confirmLock = false },
            title = { Text("锁定远端屏幕？") },
            text = { Text("被控端会立即锁定。GUI 模式的被控端锁屏后无法继续采集画面，会话可能需要重新连接。") },
            confirmButton = {
                TextButton(
                    onClick = {
                        confirmLock = false
                        HostController.lockScreen()
                    },
                ) { Text("锁定") }
            },
            dismissButton = {
                TextButton(onClick = { confirmLock = false }) { Text("取消") }
            },
        )
    }
}

@Composable
private fun SessionTopBar(
    state: ControllerState,
    statsVisible: Boolean,
    onToggleStats: () -> Unit,
    onDisconnect: () -> Unit,
    modifier: Modifier = Modifier,
) {
    Surface(
        modifier = modifier.fillMaxWidth(),
        color = Color(0xB3111820),
        contentColor = Color.White,
    ) {
        Row(
            modifier = Modifier
                .statusBarsPadding()
                .padding(horizontal = 10.dp, vertical = 6.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            ToolChip(label = "断开", onClick = onDisconnect)
            Column(Modifier.weight(1f)) {
                Text(
                    text = state.target?.takeIf { it.isNotBlank() } ?: "远程会话",
                    style = MaterialTheme.typography.titleSmall,
                    fontWeight = FontWeight.SemiBold,
                    color = Color.White,
                )
                Row(verticalAlignment = Alignment.CenterVertically) {
                    StatusDot(Color(0xFF4ADE80))
                    Spacer(Modifier.width(6.dp))
                    Text(
                        text = buildString {
                            append("已连接")
                            state.path?.let { append(" · ").append(pathText(it)) }
                            if (state.fps > 0) append(" · ").append(state.fps).append(" fps")
                        },
                        style = MaterialTheme.typography.labelSmall,
                        color = Color.White.copy(alpha = 0.75f),
                    )
                }
            }
            ToolChip(
                label = "统计",
                selected = statsVisible,
                onClick = onToggleStats,
            )
        }
    }
}

@Composable
private fun SessionBottomBar(
    modifier: Modifier = Modifier,
    quality: Int,
    frameRate: Int,
    display: Int,
    displays: List<RemoteDisplay>,
    displayMenuOpen: Boolean,
    onDisplayMenuOpenChange: (Boolean) -> Unit,
    zoom: Float,
    onQuality: (Int) -> Unit,
    onFrameRate: (Int) -> Unit,
    onSelectDisplay: (Int) -> Unit,
    onLockScreen: () -> Unit,
    onSendClipboard: () -> Unit,
    onRequestClipboard: () -> Unit,
    onKeyboard: () -> Unit,
    onResetZoom: () -> Unit,
) {
    val displayItems = displays.ifEmpty { listOf(RemoteDisplay(display, 0, 0, true)) }
    Surface(
        modifier = modifier.fillMaxWidth(),
        color = Color(0xB3111820),
        contentColor = Color.White,
    ) {
        Row(
            modifier = Modifier
                .imePadding()
                .navigationBarsPadding()
                .horizontalScroll(rememberScrollState())
                .padding(horizontal = 10.dp, vertical = 8.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(0.dp),
        ) {
            ToolChip(label = "流畅", selected = quality == 1, onClick = { onQuality(1) })
            ToolChip(label = "均衡", selected = quality == 0, onClick = { onQuality(0) })
            ToolChip(label = "高清", selected = quality == 2, onClick = { onQuality(2) })

            Spacer(Modifier.width(10.dp))
            ToolChip(label = "30 fps", selected = frameRate == 30, onClick = { onFrameRate(30) })
            ToolChip(label = "60 fps", selected = frameRate == 60, onClick = { onFrameRate(60) })

            Spacer(Modifier.width(10.dp))
            Box {
                ToolChip(
                    label = "显示器 ${display + 1}/${displayItems.size}",
                    onClick = { onDisplayMenuOpenChange(true) },
                )
                DropdownMenu(
                    expanded = displayMenuOpen,
                    onDismissRequest = { onDisplayMenuOpenChange(false) },
                ) {
                    displayItems.forEach { item ->
                        DropdownMenuItem(
                            text = { Text(item.label) },
                            onClick = {
                                onDisplayMenuOpenChange(false)
                                onSelectDisplay(item.id)
                            },
                        )
                    }
                }
            }

            Spacer(Modifier.width(10.dp))
            ToolChip(label = "锁屏", onClick = onLockScreen)
            ToolChip(label = "发送剪贴板", onClick = onSendClipboard)
            ToolChip(label = "请求剪贴板", onClick = onRequestClipboard)
            ToolChip(label = "键盘", onClick = onKeyboard)
            ToolChip(
                label = "重置缩放",
                enabled = zoom > 1.001f,
                onClick = onResetZoom,
            )
            Spacer(Modifier.width(4.dp))
        }
    }
}

@Composable
private fun StatsPanel(
    state: ControllerState,
    frameWidth: Int,
    frameHeight: Int,
    display: Int,
    cursor: Int,
    zoom: Float,
    modifier: Modifier = Modifier,
) {
    Surface(
        modifier = modifier,
        color = Color(0xC40B1018),
        contentColor = Color.White,
        shape = RoundedCornerShape(10.dp),
    ) {
        Column(
            modifier = Modifier.padding(horizontal = 12.dp, vertical = 10.dp),
            verticalArrangement = Arrangement.spacedBy(2.dp),
        ) {
            Text(
                text = "会话统计",
                style = MaterialTheme.typography.labelMedium,
                color = Color.White.copy(alpha = 0.7f),
            )
            StatRow("画面", if (frameWidth > 0) "${frameWidth}×${frameHeight}" else "—")
            StatRow("会话", sessionText(state))
            StatRow("编码", codecText(state.codec))
            StatRow("路径", pathText(state.path))
            StatRow("帧率", if (state.fps > 0) "${state.fps} fps" else "—")
            StatRow("码率", formatKbps(state.kbps))
            StatRow("延迟", state.latencyMs?.let { "$it ms" } ?: "—")
            StatRow("显示器", "${display + 1}")
            StatRow("缩放", "${(zoom * 100).toInt()}%")
            StatRow("光标", cursorText(cursor))
            StatRow("音频", if (state.audioAvailable) "可用（本期不转发）" else "不可用")
        }
    }
}

@Composable
private fun StatRow(label: String, value: String) {
    Row(verticalAlignment = Alignment.CenterVertically) {
        Text(
            text = label,
            style = MaterialTheme.typography.labelSmall,
            color = Color.White.copy(alpha = 0.6f),
            modifier = Modifier.width(52.dp),
        )
        Text(
            text = value,
            style = MaterialTheme.typography.labelSmall,
            color = Color.White,
        )
    }
}

private fun formatKbps(kbps: Int): String = when {
    kbps <= 0 -> "—"
    kbps >= 1000 -> String.format(java.util.Locale.US, "%.1f Mbps", kbps / 1000f)
    else -> "$kbps kbps"
}

private fun pointerIconFor(context: Context, shape: Int): PointerIcon = PointerIcon.getSystemIcon(
    context,
    when (shape) {
        1 -> PointerIcon.TYPE_TEXT
        2 -> PointerIcon.TYPE_HAND
        3 -> PointerIcon.TYPE_CROSSHAIR
        4, 5 -> PointerIcon.TYPE_WAIT
        6 -> PointerIcon.TYPE_HELP
        7 -> PointerIcon.TYPE_VERTICAL_DOUBLE_ARROW
        8 -> PointerIcon.TYPE_HORIZONTAL_DOUBLE_ARROW
        9 -> PointerIcon.TYPE_TOP_LEFT_DIAGONAL_DOUBLE_ARROW
        10 -> PointerIcon.TYPE_TOP_RIGHT_DIAGONAL_DOUBLE_ARROW
        11 -> PointerIcon.TYPE_ALL_SCROLL
        12 -> PointerIcon.TYPE_NO_DROP
        else -> PointerIcon.TYPE_ARROW
    },
)

private fun Context.findActivity(): Activity? {
    var ctx: Context? = this
    while (ctx is ContextWrapper) {
        if (ctx is Activity) return ctx
        ctx = ctx.baseContext
    }
    return null
}

private fun Activity.applyImmersive() {
    val controller = WindowCompat.getInsetsController(window, window.decorView)
    controller.systemBarsBehavior =
        WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
    controller.hide(WindowInsetsCompat.Type.systemBars())
}

private fun Activity.clearImmersive() {
    val controller = WindowCompat.getInsetsController(window, window.decorView)
    controller.show(WindowInsetsCompat.Type.systemBars())
}
