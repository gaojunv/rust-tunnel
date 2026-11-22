package com.rusttunnel.controller.ui

import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarDuration
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import com.rusttunnel.controller.HostController

/**
 * 顶层导航：
 * - 宿主未启动 / 用户打开设置 → [ConfigScreen]
 * - 会话已建立 → [SessionScreen]（全屏）
 * - 其余 → [DevicesScreen]
 */
@Composable
fun AppRoot() {
    val context = LocalContext.current
    val handle by HostController.handle.collectAsState()
    val state by HostController.state.collectAsState()
    val snackbarHostState = remember { SnackbarHostState() }
    var showSettings by remember { mutableStateOf(false) }

    LaunchedEffect(Unit) {
        HostController.events.collect { event ->
            when (event) {
                is HostController.Event.Info -> snackbarHostState.showSnackbar(event.message)
                is HostController.Event.Error -> snackbarHostState.showSnackbar(
                    message = "错误：${event.message}",
                    withDismissAction = true,
                    duration = SnackbarDuration.Long,
                )

                is HostController.Event.ClipboardReceived -> {
                    if (event.text.isEmpty()) {
                        snackbarHostState.showSnackbar("远端剪贴板为空")
                    } else {
                        copyTextToClipboard(context, event.text)
                        snackbarHostState.showSnackbar("已接收远端剪贴板（${event.text.length} 字）")
                    }
                }

                is HostController.Event.SessionEnded ->
                    snackbarHostState.showSnackbar(
                        message = "会话已结束：${event.reason}",
                        withDismissAction = true,
                        duration = SnackbarDuration.Long,
                    )
            }
        }
    }

    val sessionActive = handle != 0L && state.isActiveSession

    Scaffold(
        snackbarHost = { SnackbarHost(snackbarHostState) },
        containerColor = MaterialTheme.colorScheme.background,
    ) { padding ->
        Box(Modifier.fillMaxSize()) {
            if (sessionActive) {
                SessionScreen(modifier = Modifier.fillMaxSize())
            } else if (handle == 0L || showSettings) {
                ConfigScreen(
                    running = handle != 0L,
                    canGoBack = handle != 0L,
                    onBack = { showSettings = false },
                    onStarted = { showSettings = false },
                    onStopped = { showSettings = false },
                    modifier = Modifier
                        .padding(padding)
                        .fillMaxSize(),
                )
            } else {
                DevicesScreen(
                    state = state,
                    onOpenSettings = { showSettings = true },
                    onRefresh = { HostController.refreshSnapshot() },
                    modifier = Modifier
                        .padding(padding)
                        .fillMaxSize(),
                )
            }
        }
    }
}

/** 通用确认对话框。 */
@Composable
fun ConfirmDialog(
    title: String,
    text: String,
    confirmLabel: String,
    onConfirm: () -> Unit,
    onDismiss: () -> Unit,
) {
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(title) },
        text = { Text(text) },
        confirmButton = {
            TextButton(
                onClick = {
                    onDismiss()
                    onConfirm()
                },
            ) { Text(confirmLabel) }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) { Text("取消") }
        },
    )
}
