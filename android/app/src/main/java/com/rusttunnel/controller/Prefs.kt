package com.rusttunnel.controller

import android.content.Context

/**
 * 配置持久化（SharedPreferences）。
 *
 * 客户端名默认 `android-tablet`，但服务端按名字唯一标识客户端，同名会互相踢，
 * 因此配置页会显式提示用户改成每台平板唯一的名称。
 */
class Prefs(context: Context) {
    private val sp = context.applicationContext
        .getSharedPreferences("rust_tunnel_controller", Context.MODE_PRIVATE)

    var server: String
        get() = sp.getString(KEY_SERVER, "") ?: ""
        set(value) = sp.edit().putString(KEY_SERVER, value).apply()

    var token: String
        get() = sp.getString(KEY_TOKEN, "") ?: ""
        set(value) = sp.edit().putString(KEY_TOKEN, value).apply()

    var clientName: String
        get() = sp.getString(KEY_CLIENT_NAME, DEFAULT_CLIENT_NAME) ?: DEFAULT_CLIENT_NAME
        set(value) = sp.edit().putString(KEY_CLIENT_NAME, value).apply()

    var tls: Boolean
        get() = sp.getBoolean(KEY_TLS, true)
        set(value) = sp.edit().putBoolean(KEY_TLS, value).apply()

    var insecure: Boolean
        get() = sp.getBoolean(KEY_INSECURE, true)
        set(value) = sp.edit().putBoolean(KEY_INSECURE, value).apply()

    companion object {
        const val DEFAULT_CLIENT_NAME = "android-tablet"
        private const val KEY_SERVER = "server"
        private const val KEY_TOKEN = "token"
        private const val KEY_CLIENT_NAME = "client_name"
        private const val KEY_TLS = "tls"
        private const val KEY_INSECURE = "tls_insecure"
    }
}
