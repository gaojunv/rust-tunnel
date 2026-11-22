package com.rusttunnel.controller.ui

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.unit.dp
import com.rusttunnel.controller.BuildConfig
import com.rusttunnel.controller.HostController
import com.rusttunnel.controller.Native
import com.rusttunnel.controller.Prefs
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/**
 * 配置页：服务器地址 / token / 客户端名 / TLS 开关，以及宿主启动与停止。
 *
 * 所有输入即时写入 SharedPreferences；宿主运行中字段锁定（修改需先停止）。
 */
@Composable
fun ConfigScreen(
    running: Boolean,
    canGoBack: Boolean,
    onBack: () -> Unit,
    onStarted: () -> Unit,
    onStopped: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val context = androidx.compose.ui.platform.LocalContext.current
    val prefs = remember { Prefs(context) }
    var server by remember { mutableStateOf(prefs.server) }
    var token by remember { mutableStateOf(prefs.token) }
    var clientName by remember { mutableStateOf(prefs.clientName) }
    var tls by remember { mutableStateOf(prefs.tls) }
    var insecure by remember { mutableStateOf(prefs.insecure) }
    var showToken by remember { mutableStateOf(false) }
    var busy by remember { mutableStateOf(false) }
    var message by remember { mutableStateOf<String?>(null) }
    var confirmStop by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()
    val state by HostController.state.collectAsState()

    val valid = server.isNotBlank() && token.isNotBlank() && clientName.isNotBlank()
    val canStart = valid && !running && !busy && Native.loaded

    BackHandler(enabled = canGoBack) { onBack() }

    fun startHost() {
        busy = true
        message = null
        scope.launch {
            val error = withContext(Dispatchers.IO) {
                HostController.start(
                    server = server,
                    token = token,
                    clientName = clientName,
                    tls = tls,
                    insecure = insecure,
                )
            }
            busy = false
            if (error == null) {
                onStarted()
            } else {
                message = error
            }
        }
    }

    fun stopHost() {
        busy = true
        message = null
        scope.launch {
            withContext(Dispatchers.IO) { HostController.stop() }
            busy = false
            onStopped()
        }
    }

    Column(
        modifier = modifier
            .imePadding()
            .verticalScroll(rememberScrollState())
            .padding(horizontal = 20.dp, vertical = 16.dp),
        verticalArrangement = Arrangement.spacedBy(14.dp),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            if (canGoBack) {
                TextButton(onClick = onBack) { Text("← 返回") }
            }
            Spacer(Modifier.weight(1f))
            Text(
                text = "v${BuildConfig.VERSION_NAME}",
                style = MaterialTheme.typography.labelMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }

        Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
            Text(
                text = "Rust Tunnel",
                style = MaterialTheme.typography.headlineMedium,
                fontWeight = FontWeight.SemiBold,
            )
            Text(
                text = "Android 远程控制器 · 只控制，不被控制",
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }

        if (!Native.loaded) {
            Card(
                colors = CardDefaults.cardColors(
                    containerColor = MaterialTheme.colorScheme.errorContainer,
                ),
            ) {
                Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                    Text(
                        text = "原生库未加载",
                        style = MaterialTheme.typography.titleSmall,
                        color = MaterialTheme.colorScheme.onErrorContainer,
                    )
                    Text(
                        text = "libandroid_jni.so 不可用：${Native.loadError ?: "未知原因"}。\n" +
                            "请用 scripts/build-android-apk.sh 构建（会把 .so 打进 APK）。",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onErrorContainer,
                    )
                }
            }
        }

        Card {
            Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
                SectionTitle("服务器")
                OutlinedTextField(
                    value = server,
                    onValueChange = {
                        server = it
                        prefs.server = it
                    },
                    label = { Text("服务器地址") },
                    placeholder = { Text("例如 192.168.1.10:3000") },
                    supportingText = { Text("host:port，服务端控制通道端口") },
                    singleLine = true,
                    enabled = !running,
                    keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri),
                    modifier = Modifier.fillMaxWidth(),
                )
                OutlinedTextField(
                    value = token,
                    onValueChange = {
                        token = it
                        prefs.token = it
                    },
                    label = { Text("客户端 token") },
                    supportingText = { Text("服务端 Web 管理里的 client_token / 接入口令") },
                    singleLine = true,
                    enabled = !running,
                    visualTransformation = if (showToken) {
                        VisualTransformation.None
                    } else {
                        PasswordVisualTransformation()
                    },
                    trailingIcon = {
                        TextButton(onClick = { showToken = !showToken }) {
                            Text(if (showToken) "隐藏" else "显示")
                        }
                    },
                    modifier = Modifier.fillMaxWidth(),
                )
                OutlinedTextField(
                    value = clientName,
                    onValueChange = {
                        clientName = it
                        prefs.clientName = it
                    },
                    label = { Text("客户端名") },
                    supportingText = {
                        Text("服务端按客户端名唯一标识；同名客户端会互相踢下线，请为每台平板取唯一名称。")
                    },
                    singleLine = true,
                    enabled = !running,
                    modifier = Modifier.fillMaxWidth(),
                )
            }
        }

        Card {
            Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(10.dp)) {
                SectionTitle("安全")
                SwitchRow(
                    title = "使用 TLS",
                    subtitle = "控制通道加密（默认开启）",
                    checked = tls,
                    enabled = !running,
                    onCheckedChange = {
                        tls = it
                        prefs.tls = it
                    },
                )
                SwitchRow(
                    title = "不安全模式",
                    subtitle = "接受自签证书（服务端默认使用自签名证书，建议保持开启）",
                    checked = insecure,
                    enabled = !running && tls,
                    onCheckedChange = {
                        insecure = it
                        prefs.insecure = it
                    },
                )
            }
        }

        message?.let {
            Text(
                text = it,
                color = MaterialTheme.colorScheme.error,
                style = MaterialTheme.typography.bodyMedium,
            )
        }

        if (running) {
            Card(
                colors = CardDefaults.cardColors(
                    containerColor = MaterialTheme.colorScheme.surfaceVariant,
                ),
            ) {
                Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        StatusDot(connectionColor(state))
                        Spacer(Modifier.width(8.dp))
                        Text("宿主已启动", style = MaterialTheme.typography.titleSmall)
                    }
                    Text(
                        text = connectionText(state),
                        style = MaterialTheme.typography.bodyMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                    state.error?.let {
                        Text(
                            text = "最近错误：$it",
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.error,
                        )
                    }
                }
            }
        }

        if (running) {
            OutlinedButton(
                onClick = { confirmStop = true },
                enabled = !busy,
                modifier = Modifier.fillMaxWidth(),
            ) {
                if (busy) {
                    CircularProgressIndicator(Modifier.height(18.dp).width(18.dp), strokeWidth = 2.dp)
                    Spacer(Modifier.width(10.dp))
                }
                Text("停止宿主")
            }
        } else {
            Button(
                onClick = { startHost() },
                enabled = canStart,
                modifier = Modifier.fillMaxWidth(),
            ) {
                if (busy) {
                    CircularProgressIndicator(
                        Modifier.height(18.dp).width(18.dp),
                        strokeWidth = 2.dp,
                        color = MaterialTheme.colorScheme.onPrimary,
                    )
                    Spacer(Modifier.width(10.dp))
                }
                Text("启动宿主")
            }
        }

        if (!valid) {
            Text(
                text = "服务器地址、token 与客户端名都不能为空。",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.error,
            )
        }

        Text(
            text = "启动后本机以控制器身份注册到服务端（不会开放在本机被控的端口）。" +
                "宿主在后台保持连接；退出应用或点击「停止宿主」才会释放。",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Spacer(Modifier.height(8.dp))
    }

    if (confirmStop) {
        ConfirmDialog(
            title = "停止宿主？",
            text = "当前控制通道与会话都会断开，需要重新启动才能再次连接。",
            confirmLabel = "停止",
            onConfirm = { stopHost() },
            onDismiss = { confirmStop = false },
        )
    }
}

@Composable
private fun SwitchRow(
    title: String,
    subtitle: String,
    checked: Boolean,
    enabled: Boolean,
    onCheckedChange: (Boolean) -> Unit,
) {
    Row(verticalAlignment = Alignment.CenterVertically) {
        Column(Modifier.weight(1f)) {
            Text(title, style = MaterialTheme.typography.bodyLarge)
            Text(
                subtitle,
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
        Spacer(Modifier.width(12.dp))
        Switch(checked = checked, onCheckedChange = onCheckedChange, enabled = enabled)
    }
}
