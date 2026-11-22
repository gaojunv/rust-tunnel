package com.rusttunnel.controller

import android.app.Application

/**
 * 进程级入口：加载原生库并挂上 [HostController]。
 *
 * 宿主句柄放在这里（而不是 Activity/ViewModel），配置变更与旋转都不会重建宿主；
 * 只有用户显式「停止宿主」或进程退出才会释放。
 */
class App : Application() {
    override fun onCreate() {
        super.onCreate()
        instance = this
        HostController.attach(this)
        Native.load()
    }

    companion object {
        lateinit var instance: App
            private set
    }
}
