#!/usr/bin/env bash
#
# 交叉编译 rust-tunnel 的 Android 版本（含 vendored libvpx）。
#
# 背景：vendor/shiguredo_libvpx 没有 Android 预编译包，build.rs 对
# `target_os = "android"` 自动走源码构建（依赖方不需要打开 source-build）。
# 工具链由 cargo-ndk 按 target triple 注入 CC_/CXX_/AR_/CFLAGS_，build.rs 再把
# 它们映射成 libvpx configure 认的普通 CC/CXX/AR/AS。
#
# 本脚本只负责把**完整调用**写清楚，不引入 gradle / Android Studio 工程。
#
# 前置（缺一不可）：
#   1. Android NDK（r23+；本仓库按 r26/r27 的目录结构实现），并设置 ANDROID_NDK_HOME
#   2. Rust targets：rustup target add aarch64-linux-android x86_64-linux-android
#      （可选 armv7：rustup target add armv7-linux-androideabi）
#   3. cargo-ndk：cargo install cargo-ndk
#   4. 构建机上的 git / make / perl（ARM 汇编转 GAS）
#   5. x86_64 模拟器目标还需要 nasm 或 yasm（libvpx 的 x86 汇编是 NASM 语法）
#   6. rustup component add llvm-tools（build.rs 用 llvm-nm/llvm-objcopy 改写符号）
#   7. NDK/工程路径不要含空格
#
# 用法：
#   bash scripts/build-android-libvpx.sh                # arm64-v8a + x86_64（默认）
#   bash scripts/build-android-libvpx.sh arm64-v8a      # 只编 arm64
#   bash scripts/build-android-libvpx.sh arm64-v8a x86_64 armeabi-v7a
#   PACKAGE=rust-tunnel-remote PROFILE=release bash scripts/build-android-libvpx.sh
#
# 本脚本只调用 `cargo ndk`，不做任何 SDK 安装；失败时按 cargo-ndk / build.rs 的错误
# 提示补齐前置即可。

set -euo pipefail

# 允许用环境变量覆盖
NDK_API="${NDK_API:-21}"                 # cargo-ndk 默认 21
PACKAGE="${PACKAGE:-rust-tunnel-remote}"
PROFILE="${PROFILE:-debug}"              # 可设 release

if [ -z "${ANDROID_NDK_HOME:-}" ]; then
  echo "错误：未设置 ANDROID_NDK_HOME（指向 NDK 根目录）。" >&2
  echo "  macOS 示例：export ANDROID_NDK_HOME=\"\$HOME/Library/Android/sdk/ndk/27.0.12077973\"" >&2
  echo "  Linux 示例：export ANDROID_NDK_HOME=\"\$HOME/Android/Sdk/ndk/27.0.12077973\"" >&2
  exit 1
fi
if [ ! -d "${ANDROID_NDK_HOME}" ]; then
  echo "错误：ANDROID_NDK_HOME=${ANDROID_NDK_HOME} 不是目录。" >&2
  exit 1
fi

if ! command -v cargo-ndk >/dev/null 2>&1; then
  echo "错误：未找到 cargo-ndk。请先安装：cargo install cargo-ndk" >&2
  exit 1
fi

# ABI -> Rust target（cargo-ndk 自己也能做映射，这里显式列出来便于报错）
abi_to_target() {
  case "$1" in
    arm64-v8a)   echo "aarch64-linux-android" ;;
    armeabi-v7a) echo "armv7-linux-androideabi" ;;
    x86_64)      echo "x86_64-linux-android" ;;
    x86)         echo "i686-linux-android" ;;
    *)           echo "" ;;
  esac
}

ABIS=("$@")
if [ ${#ABIS[@]} -eq 0 ]; then
  ABIS=(arm64-v8a x86_64)
fi

# 1) 缺哪个 Rust target 就提示（cargo-ndk 也会提示，这里提前给出可复制的命令）
missing_targets=()
for abi in "${ABIS[@]}"; do
  target="$(abi_to_target "$abi")"
  if [ -z "$target" ]; then
    echo "错误：不支持的 ABI：$abi（支持 arm64-v8a / armeabi-v7a / x86_64 / x86）" >&2
    exit 1
  fi
  if ! rustup target list --installed | grep -qx "$target"; then
    missing_targets+=("$target")
  fi
done
if [ ${#missing_targets[@]} -gt 0 ]; then
  echo "错误：缺少 Rust target：${missing_targets[*]}" >&2
  echo "  rustup target add ${missing_targets[*]}" >&2
  exit 1
fi

# 2) x86_64 模拟器目标需要 NASM/YASM（libvpx 的 x86 汇编）
for abi in "${ABIS[@]}"; do
  if [ "$abi" = "x86_64" ] || [ "$abi" = "x86" ]; then
    if ! command -v nasm >/dev/null 2>&1 && ! command -v yasm >/dev/null 2>&1; then
      echo "错误：$abi 目标需要 nasm 或 yasm（NDK 不含 x86 汇编器）。" >&2
      echo "  macOS: brew install nasm ; Debian/Ubuntu: sudo apt-get install nasm" >&2
      exit 1
    fi
  fi
done

# 3) build.rs 用 llvm-tools 改写符号
if ! command -v rustup >/dev/null 2>&1; then
  echo "错误：未找到 rustup（需要它对 llvm-tools 做检查）。" >&2
  exit 1
fi

# 4) 组装 cargo ndk 参数
ndk_args=()
for abi in "${ABIS[@]}"; do
  ndk_args+=(-t "$abi")
done
# cargo-ndk 4.x 的 API level 短选项是大写 -P（小写 -p 会被当成 cargo 的 --package）。
ndk_args+=(--platform "$NDK_API")

cargo_args=(build -p "$PACKAGE")
if [ "$PROFILE" = "release" ]; then
  cargo_args+=(--release)
fi

echo "==> cargo ndk ${ndk_args[*]} ${cargo_args[*]}"
echo "    ANDROID_NDK_HOME=$ANDROID_NDK_HOME"
echo "    （libvpx 会从源码构建：git clone v1.16.0 + configure/make，首次较慢）"

exec cargo ndk "${ndk_args[@]}" "${cargo_args[@]}"
