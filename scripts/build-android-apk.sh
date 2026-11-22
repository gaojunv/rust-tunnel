#!/usr/bin/env bash
#
# 构建 Android 控制器 APK（controller-only）。
#
# 流程：
#   1. cargo ndk 交叉编译 crates/android-jni → target/<triple>/<profile>/libandroid_jni.so
#   2. 把 .so 复制到 android/app/src/main/jniLibs/<abi>/
#   3. android/gradlew assembleDebug
#
# 用法：
#   bash scripts/build-android-apk.sh                  # debug（默认）
#   PROFILE=release GRADLE_TASK=assembleRelease bash scripts/build-android-apk.sh
#   SKIP_NATIVE=1 bash scripts/build-android-apk.sh    # 只跑 Gradle（复用已有 .so）
#
# 环境变量（默认值按本机 Windows + Git Bash 的目录布局给出，允许覆盖）：
#   ANDROID_NDK_HOME  NDK 根目录
#   ANDROID_HOME      SDK 根目录
#   JAVA_HOME         JDK 17 根目录
#   PROFILE           debug | release
#   GRADLE_TASK       assembleDebug（默认）| assembleRelease
#   PACKAGE           Rust 包名（默认 rust-tunnel-android-jni）
#   LIB_NAME          产物库名（默认 android_jni → libandroid_jni.so）
#   SKIP_NATIVE       1 = 跳过 cargo-ndk，直接打包已有 .so
#   NDK_API           cargo-ndk 的 API level（默认 21）
#   STRIP             1 = 用 NDK 的 llvm-strip 去掉 .so 调试符号（默认 1；
#                     debug 产物 105 MB/个 → ~19 MB/个，APK 从 220 MB 降到 ~50 MB）
#                     0 = 保留符号（需要 addr2line 定位原生崩溃时）
#
# 前置：真实构建需要 crates/android-jni 已由 Rust 侧实现；
#       本脚本只负责把产物塞进 Gradle 工程，不生成 Rust 代码。

set -euo pipefail

export ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-/c/Android/Sdk/ndk/29.0.14206865}"
export ANDROID_HOME="${ANDROID_HOME:-/c/Android/Sdk}"
export ANDROID_SDK_ROOT="${ANDROID_SDK_ROOT:-$ANDROID_HOME}"
export JAVA_HOME="${JAVA_HOME:-/c/Android/jdk17}"
export PATH="/c/Android/bin:$JAVA_HOME/bin:$PATH"

PROFILE="${PROFILE:-debug}"
GRADLE_TASK="${GRADLE_TASK:-assembleDebug}"
PACKAGE="${PACKAGE:-rust-tunnel-android-jni}"
LIB_NAME="${LIB_NAME:-android_jni}"
SKIP_NATIVE="${SKIP_NATIVE:-0}"
NDK_API="${NDK_API:-21}"
STRIP="${STRIP:-1}"

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ANDROID_DIR="$ROOT_DIR/android"
JNI_DIR="$ANDROID_DIR/app/src/main/jniLibs"

ABIS=(arm64-v8a x86_64)

abi_to_triple() {
  case "$1" in
    arm64-v8a)   echo "aarch64-linux-android" ;;
    armeabi-v7a) echo "armv7-linux-androideabi" ;;
    x86_64)      echo "x86_64-linux-android" ;;
    x86)         echo "i686-linux-android" ;;
    *)           echo "" ;;
  esac
}

case "$PROFILE" in
  debug|release) ;;
  *) echo "错误：PROFILE 必须是 debug 或 release（收到 $PROFILE）" >&2; exit 1 ;;
esac

if [ ! -d "$ANDROID_DIR" ]; then
  echo "错误：找不到 android/ 工程目录：$ANDROID_DIR" >&2
  exit 1
fi

echo "==> 工程根目录：$ROOT_DIR"
echo "==> NDK：$ANDROID_NDK_HOME"
echo "==> SDK：$ANDROID_HOME"
echo "==> JDK：$JAVA_HOME"
echo "==> 构建 profile：$PROFILE，Gradle 任务：$GRADLE_TASK"

# ── 1/2. 交叉编译并复制 libandroid_jni.so ────────────────────────────────
if [ "$SKIP_NATIVE" = "1" ]; then
  echo "==> SKIP_NATIVE=1，跳过 cargo-ndk（直接使用 jniLibs 里已有的 .so）"
  for abi in "${ABIS[@]}"; do
    if [ ! -f "$JNI_DIR/$abi/lib$LIB_NAME.so" ]; then
      echo "警告：$JNI_DIR/$abi/lib$LIB_NAME.so 不存在，APK 将缺少该 ABI 的原生库。" >&2
    fi
  done
else
  if [ ! -d "$ANDROID_NDK_HOME" ]; then
    echo "错误：ANDROID_NDK_HOME=$ANDROID_NDK_HOME 不是目录。" >&2
    exit 1
  fi
  if ! command -v cargo-ndk >/dev/null 2>&1; then
    echo "错误：未找到 cargo-ndk。请先安装：cargo install cargo-ndk" >&2
    exit 1
  fi
  if [ ! -d "$ROOT_DIR/crates/android-jni" ]; then
    echo "错误：找不到 crates/android-jni（Rust 侧尚未实现？）。" >&2
    echo "      只验证 Kotlin/Gradle 工程可加 SKIP_NATIVE=1 先跑 Gradle。" >&2
    exit 1
  fi

  cargo_args=(ndk -t arm64-v8a -t x86_64 --platform "$NDK_API" build -p "$PACKAGE")
  if [ "$PROFILE" = "release" ]; then
    cargo_args+=(--release)
  fi
  echo "==> cargo ${cargo_args[*]}"
  (cd "$ROOT_DIR" && cargo "${cargo_args[@]}")

  strip_bin=""
  if [ "$STRIP" = "1" ]; then
    case "$(uname -s)" in
      MINGW*|MSYS*|CYGWIN*)
        strip_bin="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/windows-x86_64/bin/llvm-strip.exe" ;;
      Darwin)
        strip_bin="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/darwin-x86_64/bin/llvm-strip" ;;
      *)
        strip_bin="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-strip" ;;
    esac
    if [ ! -x "$strip_bin" ]; then
      echo "警告：找不到 llvm-strip（$strip_bin），保留未裁剪的 .so（APK 会很大）。" >&2
      strip_bin=""
    fi
  fi

  for abi in "${ABIS[@]}"; do
    triple="$(abi_to_triple "$abi")"
    src="$ROOT_DIR/target/$triple/$PROFILE/lib$LIB_NAME.so"
    if [ ! -f "$src" ]; then
      echo "错误：构建产物不存在：$src" >&2
      exit 1
    fi
    mkdir -p "$JNI_DIR/$abi"
    cp -f "$src" "$JNI_DIR/$abi/lib$LIB_NAME.so"
    if [ -n "$strip_bin" ]; then
      "$strip_bin" --strip-unneeded "$JNI_DIR/$abi/lib$LIB_NAME.so"
    fi
    echo "==> 已复制 $(du -h "$src" | cut -f1) → jniLibs/$abi/lib$LIB_NAME.so（打包大小 $(du -h "$JNI_DIR/$abi/lib$LIB_NAME.so" | cut -f1)）"
  done
fi

# ── 3. Gradle 打包 ───────────────────────────────────────────────────────
if [ ! -f "$ANDROID_DIR/local.properties" ]; then
  if command -v cygpath >/dev/null 2>&1; then
    sdk_win="$(cygpath -w "$ANDROID_HOME")"
  else
    sdk_win="$ANDROID_HOME"
  fi
  # .properties 里 Windows 路径需要转义反斜杠与盘符冒号。
  escaped="$(printf '%s' "$sdk_win" | sed -e 's/\\/\\\\/g' -e 's/:/\\:/g')"
  printf 'sdk.dir=%s\n' "$escaped" > "$ANDROID_DIR/local.properties"
  echo "==> 已生成 android/local.properties（sdk.dir=$sdk_win）"
fi

case "$GRADLE_TASK" in
  assembleDebug)   APK_VARIANT="debug" ;;
  assembleRelease) APK_VARIANT="release" ;;
  *)               APK_VARIANT="" ;;
esac
APK="$ANDROID_DIR/app/build/outputs/apk/$APK_VARIANT/app-$APK_VARIANT.apk"
# 先删掉旧 APK：Gradle 的增量打包在原生库体积大幅缩小时会留下未引用的旧数据段，
# 文件看起来仍是旧大小（zip 仍有效但浪费几十 MB）。删掉后重新打包体积才准确。
if [ -n "$APK_VARIANT" ]; then
  rm -f "$APK"
fi

cd "$ANDROID_DIR"
if [ -x "./gradlew" ]; then
  GRADLE_CMD=(./gradlew)
elif command -v gradle >/dev/null 2>&1; then
  GRADLE_CMD=(gradle)
else
  echo "错误：既没有 android/gradlew，也没有 PATH 里的 gradle。" >&2
  echo "      生成 wrapper：cd android && gradle wrapper --gradle-version 8.11.1" >&2
  exit 1
fi

echo "==> ${GRADLE_CMD[*]} --no-daemon $GRADLE_TASK $*"
"${GRADLE_CMD[@]}" --no-daemon "$GRADLE_TASK" "$@"

if [ -n "$APK_VARIANT" ] && [ -f "$APK" ]; then
  echo "==> APK：$APK（$(du -h "$APK" | cut -f1)）"
else
  echo "==> Gradle 完成；请在 android/app/build/outputs/apk/ 下确认产物。"
fi
