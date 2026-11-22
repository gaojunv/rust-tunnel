package com.rusttunnel.controller.ui

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import com.rusttunnel.controller.model.ControllerState

/** 工具条上的胶囊按钮（会话页用，深色半透明背景）。 */
@Composable
fun ToolChip(
    label: String,
    selected: Boolean = false,
    enabled: Boolean = true,
    onClick: () -> Unit,
) {
    val shape = RoundedCornerShape(50)
    val background = when {
        !enabled -> Color.White.copy(alpha = 0.06f)
        selected -> MaterialTheme.colorScheme.primary.copy(alpha = 0.9f)
        else -> Color.White.copy(alpha = 0.16f)
    }
    Surface(
        modifier = Modifier
            .padding(end = 8.dp)
            .clip(shape)
            .clickable(enabled = enabled, onClick = onClick),
        shape = shape,
        color = background,
        contentColor = if (enabled) Color.White else Color.White.copy(alpha = 0.35f),
    ) {
        Box(
            modifier = Modifier.padding(horizontal = 14.dp, vertical = 7.dp),
            contentAlignment = Alignment.Center,
        ) {
            Text(
                text = label,
                style = MaterialTheme.typography.labelLarge,
                fontWeight = if (selected) FontWeight.SemiBold else FontWeight.Normal,
            )
        }
    }
}

/** 一个小圆点，用来表示连接/会话状态。 */
@Composable
fun StatusDot(color: Color, modifier: Modifier = Modifier) {
    Box(
        modifier = modifier
            .size(9.dp)
            .clip(CircleShape)
            .background(color),
    )
}

/** 控制通道状态文案。 */
fun connectionText(state: ControllerState): String = when (state.connection) {
    "connecting" -> "正在连接服务器…"
    "connected" -> "已连接服务器"
    "disconnected" -> "已断开（自动重连中）"
    "rejected" -> "注册被拒绝"
    "stopped" -> "宿主已停止"
    else -> "未启动"
}

fun connectionColor(state: ControllerState): Color = when (state.connection) {
    "connected" -> Color(0xFF4ADE80)
    "connecting" -> Color(0xFFFBBF24)
    "disconnected", "rejected" -> Color(0xFFF87171)
    else -> Color(0xFF94A3B8)
}

/** 会话状态文案（设备列表页的进度条区域）。 */
fun sessionText(state: ControllerState): String = when (state.session) {
    "awaiting_approval" -> "等待被控端确认…"
    "connecting" -> "已确认，正在建立数据面…"
    "active" -> "已连接${state.target?.let { "：$it" }.orEmpty()}"
    "ended" -> "已断开${state.reason?.let { "：$it" }.orEmpty()}"
    else -> "无会话"
}

/** 协商出的编码器名。 */
fun codecText(codec: String?): String = when (codec) {
    "vp9" -> "VP9 4:4:4（软件）"
    "h264" -> "H.264 4:2:0"
    "h265" -> "H.265 4:2:0"
    null, "" -> "—"
    else -> codec
}

/** 路径的中文名。 */
fun pathText(path: String?): String = when (path) {
    "punch" -> "P2P 打洞"
    "direct" -> "局域网直连"
    "relay" -> "服务器中继"
    null, "" -> "—"
    else -> path
}

/** 协议 v9 光标形状（判别式见 `crates/remote/src/screen.rs`）。 */
fun cursorText(shape: Int): String = when (shape) {
    0 -> "箭头"
    1 -> "文本"
    2 -> "手型"
    3 -> "十字"
    4 -> "忙"
    5 -> "后台忙"
    6 -> "帮助"
    7 -> "上下调整"
    8 -> "左右调整"
    9 -> "左上-右下"
    10 -> "右上-左下"
    11 -> "四向移动"
    12 -> "禁止"
    13 -> "向上箭头"
    14 -> "未知"
    15 -> "隐藏"
    else -> "—"
}

/** 把文本写进本机剪贴板。 */
fun copyTextToClipboard(context: Context, text: String) {
    val manager = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
    manager.setPrimaryClip(ClipData.newPlainText("Rust Tunnel", text))
}

/** 读取本机剪贴板文本（空/非文本时返回 `null`）。 */
fun readClipboardText(context: Context): String? {
    val manager = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
    val clip = manager.primaryClip ?: return null
    if (clip.itemCount == 0) return null
    val text = clip.getItemAt(0).coerceToText(context)?.toString().orEmpty()
    return text.takeIf { it.isNotEmpty() }
}

/** 小标题行（配置页分区）。 */
@Composable
fun SectionTitle(text: String, modifier: Modifier = Modifier, contentPadding: PaddingValues = PaddingValues(0.dp)) {
    Row(modifier = modifier.padding(contentPadding), verticalAlignment = Alignment.CenterVertically) {
        Text(
            text = text,
            style = MaterialTheme.typography.titleSmall,
            color = MaterialTheme.colorScheme.primary,
            fontWeight = FontWeight.SemiBold,
        )
    }
}
