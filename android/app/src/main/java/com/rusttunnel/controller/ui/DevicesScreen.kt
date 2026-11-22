package com.rusttunnel.controller.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import com.rusttunnel.controller.HostController
import com.rusttunnel.controller.model.ControllerState
import com.rusttunnel.controller.model.PeerInfo

/**
 * 设备列表页：控制通道状态 + 在线可被控客户端 + 会话进度。
 *
 * 列表来自 `onState` 的 `peers` 字段；点一项调用 `nativeConnect(handle, name, 0)`。
 */
@Composable
fun DevicesScreen(
    state: ControllerState,
    onOpenSettings: () -> Unit,
    onRefresh: () -> Unit,
    modifier: Modifier = Modifier,
) {
    Column(modifier.fillMaxSize()) {
        Header(
            state = state,
            onOpenSettings = onOpenSettings,
            onRefresh = onRefresh,
        )
        LazyColumn(
            modifier = Modifier.fillMaxSize(),
            contentPadding = PaddingValues(horizontal = 16.dp, vertical = 12.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            item { ConnectionCard(state) }
            if (state.isPendingSession) {
                item { PendingCard(state) }
            }
            item {
                Row(
                    modifier = Modifier.fillMaxWidth().padding(top = 6.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Text(
                        text = "可被控的客户端",
                        style = MaterialTheme.typography.titleMedium,
                        fontWeight = FontWeight.SemiBold,
                    )
                    Spacer(Modifier.weight(1f))
                    Text(
                        text = "${state.peers.size} 台在线",
                        style = MaterialTheme.typography.labelMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }
            if (state.peers.isEmpty()) {
                item { EmptyCard(state) }
            } else {
                items(state.peers, key = { it.clientName }) { peer ->
                    PeerCard(
                        peer = peer,
                        enabled = state.connection == "connected" && !state.isPendingSession,
                        onConnect = { HostController.connect(peer, 0) },
                    )
                }
            }
            item { GestureHelpCard() }
            item { Spacer(Modifier.height(12.dp)) }
        }
    }
}

@Composable
private fun Header(
    state: ControllerState,
    onOpenSettings: () -> Unit,
    onRefresh: () -> Unit,
) {
    Surface(color = MaterialTheme.colorScheme.surface, tonalElevation = 1.dp) {
        Column(Modifier.padding(horizontal = 16.dp, vertical = 12.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Column(Modifier.weight(1f)) {
                    Text(
                        text = "Rust Tunnel",
                        style = MaterialTheme.typography.titleLarge,
                        fontWeight = FontWeight.SemiBold,
                    )
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        StatusDot(connectionColor(state))
                        Spacer(Modifier.width(6.dp))
                        Text(
                            text = connectionText(state),
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                }
                TextButton(onClick = onRefresh) { Text("刷新") }
                TextButton(onClick = onOpenSettings) { Text("设置") }
            }
        }
    }
}

@Composable
private fun ConnectionCard(state: ControllerState) {
    val error = state.error ?: state.reason
    Card(
        colors = CardDefaults.cardColors(
            containerColor = MaterialTheme.colorScheme.surfaceVariant,
        ),
    ) {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                StatusDot(connectionColor(state))
                Spacer(Modifier.width(8.dp))
                Text(connectionText(state), style = MaterialTheme.typography.titleSmall)
            }
            when (state.connection) {
                "connected" -> Text(
                    text = "控制通道已就绪。点下面任意一台在线设备发起远程控制；" +
                        "被控端（默认）会弹窗请求确认。",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )

                "connecting" -> Text(
                    text = "正在连接服务端并注册本机客户端名…",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )

                else -> Text(
                    text = "无法使用服务端。请检查服务器地址、token、TLS 设置与网络；" +
                        "宿主会自动重连，也可回设置页停止后重启。",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            if (state.capability == false) {
                Text(
                    text = "服务端未启用远程控制（RustDesk 会合/中继），在线设备列表会一直为空。",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.error,
                )
            }
            if (!error.isNullOrBlank()) {
                Text(
                    text = "原因：$error",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.error,
                )
            }
            if (state.session == "ended" && !state.reason.isNullOrBlank()) {
                Text(
                    text = "上次会话：${state.reason}",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
    }
}

@Composable
private fun PendingCard(state: ControllerState) {
    Card(
        colors = CardDefaults.cardColors(
            containerColor = MaterialTheme.colorScheme.primaryContainer,
        ),
    ) {
        Row(
            modifier = Modifier.fillMaxWidth().padding(16.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            CircularProgressIndicator(
                modifier = Modifier.size(22.dp),
                strokeWidth = 2.5.dp,
                color = MaterialTheme.colorScheme.onPrimaryContainer,
            )
            Spacer(Modifier.width(14.dp))
            Column(Modifier.weight(1f)) {
                Text(
                    text = sessionText(state),
                    style = MaterialTheme.typography.titleSmall,
                    color = MaterialTheme.colorScheme.onPrimaryContainer,
                )
                Text(
                    text = if (state.session == "awaiting_approval") {
                        "请在被控端电脑上点击「允许」。超过 45 秒未确认会自动拒绝。"
                    } else {
                        "正在通过打洞/直连/中继建立数据面，通常 1–5 秒。"
                    },
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onPrimaryContainer,
                )
            }
            TextButton(onClick = { HostController.cancelSession() }) {
                Text("取消", color = MaterialTheme.colorScheme.onPrimaryContainer)
            }
        }
    }
}

@Composable
private fun PeerCard(peer: PeerInfo, enabled: Boolean, onConnect: () -> Unit) {
    val shape = MaterialTheme.shapes.medium
    Card(
        modifier = Modifier
            .fillMaxWidth()
            .clip(shape)
            .clickable(enabled = enabled, onClick = onConnect),
    ) {
        Row(
            modifier = Modifier.fillMaxWidth().padding(14.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Box(
                modifier = Modifier
                    .size(42.dp)
                    .clip(CircleShape),
                contentAlignment = Alignment.Center,
            ) {
                Surface(
                    modifier = Modifier.size(42.dp),
                    shape = CircleShape,
                    color = MaterialTheme.colorScheme.primaryContainer,
                ) {
                    Box(contentAlignment = Alignment.Center) {
                        Text(
                            text = peer.label.take(1).uppercase(),
                            style = MaterialTheme.typography.titleMedium,
                            color = MaterialTheme.colorScheme.onPrimaryContainer,
                            fontWeight = FontWeight.SemiBold,
                        )
                    }
                }
            }
            Spacer(Modifier.width(14.dp))
            Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(2.dp)) {
                Text(
                    text = peer.label,
                    style = MaterialTheme.typography.titleMedium,
                    fontWeight = FontWeight.SemiBold,
                )
                val details = buildList {
                    if (peer.platform.isNotBlank()) add(peer.platform)
                    add("${peer.displays} 个显示器")
                    if (peer.rdId.isNotBlank()) add("ID ${peer.rdId}")
                }.joinToString(" · ")
                Text(
                    text = details,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                if (peer.clientName.isNotBlank() && peer.clientName != peer.label) {
                    Text(
                        text = "客户端名：${peer.clientName}",
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }
            Text(
                text = if (enabled) "连接" else "不可用",
                style = MaterialTheme.typography.labelLarge,
                color = if (enabled) {
                    MaterialTheme.colorScheme.primary
                } else {
                    MaterialTheme.colorScheme.onSurfaceVariant
                },
            )
        }
    }
}

@Composable
private fun EmptyCard(state: ControllerState) {
    Card {
        Column(
            Modifier.fillMaxWidth().padding(20.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            Text("暂无在线可被控的客户端", style = MaterialTheme.typography.titleSmall)
            Text(
                text = if (state.connection == "connected") {
                    "请确认：\n" +
                        "1. 服务端已启用远程控制（RustDesk 会合/中继）；\n" +
                        "2. 被控端客户端已上线，并在「远程」里打开「允许被远程控制」；\n" +
                        "3. 被控端与本控制器连的是同一台服务端。"
                } else {
                    "控制通道尚未就绪，先让上面的状态变成「已连接服务器」。"
                },
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
    }
}

@Composable
private fun GestureHelpCard() {
    Card(colors = CardDefaults.cardColors(containerColor = Color.Transparent)) {
        Column(Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
            Text(
                text = "触控手势",
                style = MaterialTheme.typography.labelLarge,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Text(
                text = "单指点按 = 左键 · 单指拖动 = 左键拖动 · 长按 = 右键\n" +
                    "双指上下滑 = 滚轮 · 双指轻点 = 右键 · 双指捏合 = 本机缩放/平移\n" +
                    "软键盘文字会整段发送（输入法组合过程不发送）",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
    }
}
