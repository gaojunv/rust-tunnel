# JNI 入口不能被混淆：NativeBridge 的 native 方法与 NativeEvents 的静态回调
# 由 Rust 侧按名字查找（见 android/JNI_CONTRACT.md）。
-keep class com.rusttunnel.controller.NativeBridge { *; }
-keep class com.rusttunnel.controller.NativeEvents { *; }
-keepclasseswithmembernames class * {
    native <methods>;
}
