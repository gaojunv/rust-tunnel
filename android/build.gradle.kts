// rust-tunnel Android 控制器（controller-only）
//
// 原生库 libandroid_jni.so 由 scripts/build-android-apk.sh 用 cargo-ndk 构建后
// 复制到 app/src/main/jniLibs/<abi>/；本工程不做 externalNativeBuild。
plugins {
    id("com.android.application") version "8.10.1" apply false
    id("org.jetbrains.kotlin.android") version "2.1.20" apply false
    id("org.jetbrains.kotlin.plugin.compose") version "2.1.20" apply false
}
