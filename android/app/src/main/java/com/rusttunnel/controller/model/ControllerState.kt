package com.rusttunnel.controller.model

import org.json.JSONArray
import org.json.JSONObject

/**
 * 一个可被远程控制的在线客户端（`onState` 的 `peers` 元素）。
 *
 * 字段名以 Rust 侧 `RemotePeerInfo` 的序列化为准；解析时同时接受 snake_case 与
 * camelCase，未知字段一律忽略（契约 §5：Kotlin 必须容忍新增字段）。
 */
data class PeerInfo(
    val clientName: String,
    val displayName: String,
    val platform: String,
    val displays: Int,
    val rdId: String,
) {
    /** 界面首选展示名；空则回落到客户端名。 */
    val label: String
        get() = displayName.ifBlank { clientName }
}

/** `onDisplays` 里的一个显示器。 */
data class RemoteDisplay(
    val id: Int,
    val width: Int,
    val height: Int,
    val primary: Boolean,
) {
    val label: String
        get() {
            val size = if (width > 0 && height > 0) " · ${width}×${height}" else ""
            val suffix = if (primary) "（主）" else ""
            return "显示器 ${id + 1}$size$suffix"
        }
}

/**
 * 控制通道 + 远程会话的聚合状态（契约 §5 的最小字段集）。
 *
 * 所有字段都有默认值，Rust 侧新增/缺失字段不会让解析失败。
 */
data class ControllerState(
    val connection: String = "idle",
    val session: String = "none",
    val target: String? = null,
    val reason: String? = null,
    val error: String? = null,
    val remoteWidth: Int = 0,
    val remoteHeight: Int = 0,
    val display: Int = 0,
    val fps: Int = 0,
    val kbps: Int = 0,
    val path: String? = null,
    val audioAvailable: Boolean = false,
    val latencyMs: Int? = null,
    val peers: List<PeerInfo> = emptyList(),
    val quality: String? = null,
    val codec: String? = null,
    val selfRdId: String? = null,
    val capability: Boolean? = null,
) {
    /** 会话已建立、应当进入画面页。 */
    val isActiveSession: Boolean
        get() = session == "active"

    /** 等待被控端确认 / 正在建立数据面（仍停在设备列表页显示进度）。 */
    val isPendingSession: Boolean
        get() = session == "awaiting_approval" || session == "connecting"
}

/** 状态 JSON 解析（容错：坏 JSON 返回上一次的状态）。 */
object StateParser {
    fun parse(json: String, previous: ControllerState): ControllerState {
        val obj = try {
            JSONObject(json)
        } catch (_: Exception) {
            return previous
        }
        return ControllerState(
            connection = obj.stringOr("connection", previous.connection),
            session = obj.stringOr("session", previous.session),
            target = obj.nullableString("target", previous.target),
            reason = obj.nullableString("reason", previous.reason),
            error = obj.nullableString("error", previous.error),
            remoteWidth = obj.intOr("remote_width", previous.remoteWidth),
            remoteHeight = obj.intOr("remote_height", previous.remoteHeight),
            display = obj.intOr("display", previous.display),
            fps = obj.intOr("fps", previous.fps),
            kbps = obj.intOr("kbps", previous.kbps),
            path = obj.nullableString("path", previous.path),
            audioAvailable = obj.boolOr("audio_available", previous.audioAvailable),
            latencyMs = obj.nullableInt("latency_ms", previous.latencyMs),
            peers = obj.arrayOrNull("peers")?.let(::parsePeers) ?: previous.peers,
            quality = obj.nullableString("quality", previous.quality),
            codec = obj.nullableString("codec", previous.codec),
            selfRdId = obj.nullableString("self_rd_id", previous.selfRdId),
            capability = obj.nullableBoolean("capability", previous.capability),
        )
    }

    fun parsePeers(array: JSONArray): List<PeerInfo> = buildList {
        for (i in 0 until array.length()) {
            val o = array.optJSONObject(i) ?: continue
            val clientName = o.firstString("client_name", "clientName", "name").orEmpty()
            val displayName = o.firstString("display_name", "displayName").orEmpty()
            if (clientName.isBlank() && displayName.isBlank()) continue
            add(
                PeerInfo(
                    clientName = clientName.ifBlank { displayName },
                    displayName = displayName,
                    platform = o.firstString("platform", "os").orEmpty(),
                    displays = o.firstInt("displays", "display_count", "displayCount") ?: 1,
                    rdId = o.firstString("rd_id", "rdId", "id").orEmpty(),
                ),
            )
        }
    }

    fun parseDisplays(json: String): List<RemoteDisplay> {
        val array = try {
            JSONArray(json)
        } catch (_: Exception) {
            return emptyList()
        }
        return buildList {
            for (i in 0 until array.length()) {
                val o = array.optJSONObject(i) ?: continue
                add(
                    RemoteDisplay(
                        id = o.firstInt("id", "index", "display") ?: i,
                        width = o.firstInt("width", "w") ?: 0,
                        height = o.firstInt("height", "h") ?: 0,
                        primary = o.optBoolean("primary", i == 0),
                    ),
                )
            }
        }
    }
}

private fun JSONObject.stringOr(key: String, fallback: String): String {
    if (!has(key) || isNull(key)) return fallback
    val value = optString(key)
    return value.ifBlank { fallback }
}

private fun JSONObject.nullableString(key: String, fallback: String?): String? {
    if (!has(key)) return fallback
    if (isNull(key)) return null
    return optString(key).ifBlank { null }
}

private fun JSONObject.intOr(key: String, fallback: Int): Int {
    if (!has(key) || isNull(key)) return fallback
    return optInt(key, fallback)
}

private fun JSONObject.nullableInt(key: String, fallback: Int?): Int? {
    if (!has(key)) return fallback
    if (isNull(key)) return null
    return optInt(key, fallback ?: 0)
}

private fun JSONObject.boolOr(key: String, fallback: Boolean): Boolean {
    if (!has(key) || isNull(key)) return fallback
    return optBoolean(key, fallback)
}

private fun JSONObject.nullableBoolean(key: String, fallback: Boolean?): Boolean? {
    if (!has(key)) return fallback
    if (isNull(key)) return null
    return optBoolean(key, fallback ?: false)
}

private fun JSONObject.arrayOrNull(key: String): JSONArray? {
    if (!has(key) || isNull(key)) return null
    return optJSONArray(key)
}

private fun JSONObject.firstString(vararg keys: String): String? {
    for (key in keys) {
        if (!has(key) || isNull(key)) continue
        val value = optString(key)
        if (value.isNotBlank()) return value
    }
    return null
}

private fun JSONObject.firstInt(vararg keys: String): Int? {
    for (key in keys) {
        if (!has(key) || isNull(key)) continue
        if (opt(key) is Number) return optInt(key)
    }
    return null
}
