# libvpx-rs

[![crates.io](https://img.shields.io/crates/v/shiguredo_libvpx.svg)](https://crates.io/crates/shiguredo_libvpx)
[![docs.rs](https://docs.rs/shiguredo_libvpx/badge.svg)](https://docs.rs/shiguredo_libvpx)
[![License](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)
[![GitHub Actions](https://github.com/shiguredo/libvpx-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/shiguredo/libvpx-rs/actions/workflows/ci.yml)
[![Discord](https://img.shields.io/badge/Discord-%235865F2.svg?logo=discord&logoColor=white)](https://discord.gg/shiguredo)

## About Shiguredo's open source software

We will not respond to PRs or issues that have not been discussed on Discord. Also, Discord is only available in Japanese.

Please read <https://github.com/shiguredo/oss> before use.

## 時雨堂のオープンソースソフトウェアについて

利用前に <https://github.com/shiguredo/oss> をお読みください。

## 概要

[libvpx](https://github.com/webmproject/libvpx) を利用した VP8 / VP9 エンコーダーおよびデコーダーの Rust バインディングです。

## 特徴

- VP8 エンコーダー / デコーダー
- VP9 エンコーダー / デコーダー
- VP9 High Bitdepth (10-bit / Profile 2) デコード対応
- 複数の画像フォーマット対応 (I420, YV12, NV12, I422, I444, I440, I42016, I42216, I44416, I44016)
- エンコーダーの詳細設定 (レート制御、品質、速度)
- VP9 固有設定 (適応的量子化、タイル分割、行マルチスレッド)
- VP8 固有設定 (デノイザー、ARNR フィルタ)
- シンボル書き換えによる他ライブラリとの衝突回避 (`shiguredo_vpx_` プレフィックス付与)
- prebuilt バイナリによる高速ビルド (デフォルト)
- ソースからのビルドも可能 (`--features source-build`)

## 動作要件

- Ubuntu 24.04 x86_64
- Ubuntu 24.04 arm64
- Ubuntu 22.04 x86_64
- Ubuntu 22.04 arm64
- macOS 26 arm64
- macOS 15 arm64
- Windows Server 2025 x86_64
- Windows 11 x86_64

### ソースビルド時の追加要件

- Git
- C コンパイラ (`build-essential` 等)
- YASM または NASM (libvpx のアセンブリ最適化に必要)
- Windows の場合は MSYS2 + MINGW64 (`gcc` / `make` / `nasm` / `clang`) が必要

```bash
# Ubuntu
sudo apt-get install -y build-essential nasm

# macOS
brew install nasm

# Windows (MSYS2 MINGW64)
pacman -S git make nasm mingw-w64-x86_64-gcc mingw-w64-x86_64-binutils mingw-w64-x86_64-clang
```

## ビルド

デフォルトでは GitHub Releases から prebuilt バイナリをダウンロードしてビルドします。

```bash
cargo build
```

### ソースからビルド

libvpx をソースからビルドする場合は `source-build` feature を有効にしてください。

```bash
cargo build --features source-build
```

### Android（本仓库新增，2026-11）

> 本节是本仓库 vendor 化之后新增的说明，上游 shiguredo/libvpx-rs 没有 Android 支持。

**为什么必须源码构建**：上游只在 GitHub Releases 发布 linux / macOS / Windows 六种
组合的预编译包，**没有任何 Android 产物**。因此本仓库的 `build.rs` 对
`target_os = "android"` 恒走源码构建（`should_use_prebuilt()` 直接返回 false），
依赖方（`crates/remote`）**不需要**打开 `source-build` feature——桌面目标的 feature
集合因此与改动前完全一致。

**支持的 target**

| Rust target | ABI | libvpx `--target=` | 状态 |
|---|---|---|---|
| `aarch64-linux-android` | arm64-v8a | `arm64-android-gcc` | 必须支持 |
| `armv7-linux-androideabi` | armeabi-v7a | `armv7-android-gcc` | 可选 |
| `x86_64-linux-android` | x86_64（模拟器） | `x86_64-android-gcc` | 必须支持 |
| `i686-linux-android` | x86（模拟器） | `x86-android-gcc` | 顺带支持 |

**前置条件**

- Android NDK（r23+；按 `toolchains/llvm/prebuilt/<host>/bin` 的通用布局实现。
  本机没有 NDK，未实际编译，见文末「未验证项」）
- `git`（构建期 clone libvpx `v1.16.0`）、`make`
- `rustup component add llvm-tools`（符号改写用 llvm-nm / llvm-objcopy）
- ARM 目标：`perl`（`build/make/ads2gas.pl` 把 ARM 汇编转成 GAS 语法）
- x86_64 / x86 目标：host 上的 `nasm` 或 `yasm`（libvpx 的 x86 汇编是 NASM 语法，
  NDK 只带 clang/llvm，不含汇编器）
- NDK / 工程路径**不要含空格**（MSYS2 下 make 展开未加引号的 `$(CC)` 会断）
- Windows 上交叉编译必须在 **MSYS2 / Git Bash** 里运行（libvpx 的 `configure` 是
  POSIX shell 脚本，`build.rs` 会经 `sh` 启动它；`make` 也要在 PATH 上）

**推荐命令（cargo-ndk，不需要 gradle）**

```bash
export ANDROID_NDK_HOME=/path/to/android-ndk
rustup target add aarch64-linux-android x86_64-linux-android
cargo install cargo-ndk

# arm64-v8a + x86_64 模拟器；--platform 21 是 API level（默认值）
# 注意：cargo-ndk 4.x 的短选项是大写 -P，小写 -p 会被当成 cargo 的 --package
cargo ndk -t arm64-v8a -t x86_64 --platform 21 build -p rust-tunnel-remote

# 可选：armv7
rustup target add armv7-linux-androideabi
cargo ndk -t armeabi-v7a --platform 21 build -p rust-tunnel-remote
```

完整脚本见仓库根目录 `scripts/build-android-libvpx.sh`。

**工具链注入方式**（`build.rs` 的实现口径）

1. 优先读 cargo-ndk（与 `cc` crate 同一套命名）注入的
   `CC_<target-triple>` / `CXX_<target-triple>` / `AR_<target-triple>` /
   `CFLAGS_<target-triple>`（连字符原样与下划线两种写法都认，另外认 `TARGET_CC` 等）。
2. 只设置 `ANDROID_NDK_HOME` 时，从
   `<ndk>/toolchains/llvm/prebuilt/<host>/bin` 取 `clang` / `clang++` / `llvm-ar`，
   并补 `--target=<clang-triple><api>`（API 取 `ANDROID_PLATFORM` /
   `CARGO_NDK_PLATFORM`，缺省 21；**不要**用 `CARGO_NDK_ANDROID_PLATFORM`——cargo-ndk
   实际把 ABI 名（`arm64-v8a`）写进了那个变量，取数字会得到非法的 API level）。
3. 两者都没有时 `panic!`，错误信息会列出缺少的变量名和可复制的 `cargo ndk` 命令。

注意 libvpx 1.16 的 configure **不认** `CC_<libvpx-target>`（如
`CC_arm64-android-gcc`）：`build/make/configure.sh` 的 `setup_gnu_toolchain` 只读
普通的 `CC`/`CXX`/`AR`/`AS`/`LD`/`STRIP`，而带连字符的变量名在 POSIX shell 里根本
无法用 `$VAR` 展开。这层映射在 `build.rs` 里完成。

**构建参数**：`--target=<libvpx target>`、`--disable-shared`、
`--enable-vp9-highbitdepth`（4:4:4 profile 1 的前提，不能删）、
`--disable-examples --disable-tools --disable-unit-tests --disable-docs`
（Android 只要静态库，避免交叉链接）。

**产物与链接**：仍然安装到 `<build>/lib/libvpx.a`，下游照旧
`cargo:rustc-link-lib=static=vpx`；ARM 汇编由 clang 驱动（NDK r24 起没有独立 GNU as，
`build.rs` 显式设置 `AS=<clang>`）。libvpx 是纯 C，不依赖 libc++/libstdc++，也不需要
额外的 `-lstdc++`/`-lc++_static`；pthread / libm 由 bionic 的 libc 提供。

**未验证项**：本仓库开发机没有 Android SDK/NDK，Android 路径只做了代码与参数层面的
实现（对照 libvpx v1.16.0 的 `configure` 源码核对）。首次真机构建时请重点确认：
`perl`/`nasm` 前置、符号改写后的 `libvpx.a` 能被 NDK 链接、以及 armv7 的
`armv7a-linux-androideabi` 目标 flag 是否被 clang 接受。

### docs.rs 向けビルド

libvpx がない環境では、docs.rs 向けのドキュメント生成のみ可能です。

```bash
DOCS_RS=1 cargo doc --no-deps
```

## 使い方

### エンコード

入力は `ImageData` 列挙型で画像フォーマットと各プレーンのデータを渡します。

```rust
use shiguredo_libvpx::{
    CodecConfig, EncodeOptions, Encoder, EncoderConfig,
    EncodingDeadline, ImageData, ImageFormat, RateControlMode, Vp9Config,
};

// 必須パラメータを指定して設定を生成
let mut config = EncoderConfig::new(
    1920,                                    // width
    1080,                                    // height
    ImageFormat::I420,                       // image_format
    CodecConfig::Vp9(Vp9Config::default()),  // codec
);

// 必要に応じてオプションパラメータを変更
config.deadline = EncodingDeadline::Realtime;
config.rate_control = RateControlMode::Cbr;
config.cpu_used = Some(4);
config.threads = std::num::NonZeroUsize::new(4);

// エンコーダーを作成
let mut encoder = Encoder::new(config)?;

// I420 形式の YUV データをエンコード
let image = ImageData::I420 { y: &y_data, u: &u_data, v: &v_data };
encoder.encode(&image, &EncodeOptions { force_keyframe: false })?;

// キーフレームを強制する場合
encoder.encode(&image, &EncodeOptions {
    force_keyframe: true,
})?;

// エンコード済みフレームを取得
while let Some(frame) = encoder.next_frame() {
    let data = frame.data();
    let is_key = frame.is_keyframe();
    println!("encoded: {} bytes, keyframe: {}", data.len(), is_key);
}

// 残りのフレームをフラッシュ
encoder.finish()?;
while let Some(frame) = encoder.next_frame() {
    // ...
}
```

VP8 の場合は `CodecConfig::Vp8(Vp8Config::default())` を指定してください。

### デコード

```rust
use shiguredo_libvpx::{Decoder, DecoderCodec, DecoderConfig};

// VP9 デコーダーを作成
let mut decoder = Decoder::new(DecoderConfig::new(DecoderCodec::Vp9))?;

// 圧縮データをデコード
decoder.decode(&compressed_data)?;

// デコード済みフレームを取得
while let Some(frame) = decoder.next_frame() {
    let y = frame.y_plane();
    let u = frame.u_plane();
    let v = frame.v_plane();
    let y_stride = frame.y_stride();
    let u_stride = frame.u_stride();
    let v_stride = frame.v_stride();
    let is_high_depth = frame.is_high_depth();
    println!("{}x{} high_depth={}", frame.width(), frame.height(), is_high_depth);
}

// 残りのフレームをフラッシュ
decoder.finish()?;
while let Some(frame) = decoder.next_frame() {
    // ...
}
```

VP8 の場合は `codec: DecoderCodec::Vp8` を指定してください。

## 設定

### `EncoderConfig`

| フィールド | 型 | 説明 |
|---|---|---|
| `width` | `usize` | 映像の幅 |
| `height` | `usize` | 映像の高さ |
| `image_format` | `ImageFormat` | 入力画像フォーマット |
| `fps_numerator` | `usize` | フレームレートの分子 |
| `fps_denominator` | `usize` | フレームレートの分母 |
| `target_bitrate` | `usize` | ターゲットビットレート (bps) |
| `min_quantizer` | `usize` | 最小量子化パラメーター |
| `max_quantizer` | `usize` | 最大量子化パラメーター |
| `cq_level` | `usize` | CQ レベル |
| `cpu_used` | `Option<usize>` | エンコード速度 (VP8: 0-16, VP9: 0-9) |
| `deadline` | `EncodingDeadline` | エンコード期限 |
| `rate_control` | `RateControlMode` | レート制御モード |
| `lag_in_frames` | `Option<NonZeroUsize>` | 先読みフレーム数 |
| `threads` | `Option<NonZeroUsize>` | スレッド数 |
| `error_resilient` | `bool` | エラー耐性モード |
| `keyframe_interval` | `Option<NonZeroUsize>` | キーフレーム間隔 |
| `frame_drop_threshold` | `Option<usize>` | フレームドロップ閾値 (0-100) |
| `codec` | `CodecConfig` | コーデック固有設定 |

### `EncodingDeadline`

| バリアント | 説明 |
|---|---|
| `Best` | 最高品質 (最も時間がかかる) |
| `Good` | 良い品質 (品質と速度のバランス) |
| `Realtime` | リアルタイム (最も高速) |

### `RateControlMode`

| バリアント | 説明 |
|---|---|
| `Vbr` | Variable Bitrate (可変ビットレート) |
| `Cbr` | Constant Bitrate (固定ビットレート) |
| `Cq` | Constant Quality (固定品質) |

### `ImageFormat`

| バリアント | 説明 |
|---|---|
| `I420` | YUV 4:2:0 planar (3 プレーン: Y, U, V) |
| `Yv12` | YUV 4:2:0 planar (3 プレーン: Y, V, U) |
| `Nv12` | YUV 4:2:0 semi-planar (2 プレーン: Y, UV interleaved) |
| `I422` | YUV 4:2:2 planar (3 プレーン: Y, U, V) |
| `I444` | YUV 4:4:4 planar (3 プレーン: Y, U, V) |
| `I440` | YUV 4:4:0 planar (3 プレーン: Y, U, V) |
| `I42016` | YUV 4:2:0 planar 16-bit (3 プレーン: Y, U, V) |
| `I42216` | YUV 4:2:2 planar 16-bit (3 プレーン: Y, U, V) |
| `I44416` | YUV 4:4:4 planar 16-bit (3 プレーン: Y, U, V) |
| `I44016` | YUV 4:4:0 planar 16-bit (3 プレーン: Y, U, V) |

### `ImageData`

| バリアント | 説明 |
|---|---|
| `I420 { y, u, v }` | I420 (3 プレーン: Y, U, V) |
| `Yv12 { y, u, v }` | YV12 (3 プレーン: Y, V, U) |
| `Nv12 { y, uv }` | NV12 (2 プレーン: Y, UV interleaved) |
| `I422 { y, u, v }` | I422 (3 プレーン: Y, U, V) |
| `I444 { y, u, v }` | I444 (3 プレーン: Y, U, V) |
| `I440 { y, u, v }` | I440 (3 プレーン: Y, U, V) |
| `I42016 { y, u, v }` | I42016 (3 プレーン: Y, U, V / 16-bit) |
| `I42216 { y, u, v }` | I42216 (3 プレーン: Y, U, V / 16-bit) |
| `I44416 { y, u, v }` | I44416 (3 プレーン: Y, U, V / 16-bit) |
| `I44016 { y, u, v }` | I44016 (3 プレーン: Y, U, V / 16-bit) |

### `CodecConfig`

| バリアント | 説明 |
|---|---|
| `Vp8(Vp8Config)` | VP8 コーデック設定 |
| `Vp9(Vp9Config)` | VP9 コーデック設定 |

### `EncodeOptions`

| フィールド | 型 | デフォルト | 説明 |
|---|---|---|---|
| `force_keyframe` | `bool` | false | キーフレームを強制する |

### `DecoderConfig`

| フィールド | 型 | 説明 |
|---|---|---|
| `codec` | `DecoderCodec` | デコードするコーデック |

### `DecoderCodec`

| バリアント | 説明 |
|---|---|
| `Vp8` | VP8 |
| `Vp9` | VP9 |

### `Vp9Profile`

| バリアント | 説明 |
|---|---|
| `Profile0` | 8-bit 4:2:0 (デフォルト) |
| `Profile2` | 10/12-bit 4:2:0 |

### `Vp9Config`

| フィールド | 型 | 説明 |
|---|---|---|
| `profile` | `Vp9Profile` | プロファイル |
| `aq_mode` | `Option<i32>` | 適応的量子化モード (0-3) |
| `noise_sensitivity` | `Option<i32>` | デノイザー設定 (0-3) |
| `tile_columns` | `Option<i32>` | タイル列数 (並列処理用) |
| `tile_rows` | `Option<i32>` | タイル行数 (並列処理用) |
| `row_mt` | `bool` | 行マルチスレッド |
| `frame_parallel_decoding` | `bool` | フレーム並列デコード |
| `tune_content` | `Option<ContentType>` | コンテンツタイプ最適化 |

### `Vp8Config`

| フィールド | 型 | 説明 |
|---|---|---|
| `noise_sensitivity` | `Option<i32>` | デノイザー設定 (0-3) |
| `static_threshold` | `Option<i32>` | 静的閾値 |
| `token_partitions` | `Option<i32>` | トークンパーティション数 |
| `max_intra_bitrate_pct` | `Option<i32>` | 最大イントラビットレート率 |
| `arnr_config` | `Option<ArnrConfig>` | ARNR フィルタ設定 |

## サポートコーデック

### エンコード

| コーデック | `CodecConfig` |
|---|---|
| VP8 | `CodecConfig::Vp8(Vp8Config { .. })` |
| VP9 | `CodecConfig::Vp9(Vp9Config { .. })` |

### デコード

| コーデック | `DecoderCodec` |
|---|---|
| VP8 | `DecoderCodec::Vp8` |
| VP9 | `DecoderCodec::Vp9` |

## 環境変数

| 変数 | 説明 |
|---|---|
| `LIBVPX_TARGET` | prebuilt バイナリのプラットフォーム名を明示的に指定する |

## libvpx ライセンス

<https://chromium.googlesource.com/webm/libvpx/+/refs/heads/main/LICENSE>

```text
Copyright (c) 2010, The WebM Project authors. All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are
met:

  * Redistributions of source code must retain the above copyright
    notice, this list of conditions and the following disclaimer.

  * Redistributions in binary form must reproduce the above copyright
    notice, this list of conditions and the following disclaimer in
    the documentation and/or other materials provided with the
    distribution.

  * Neither the name of Google, nor the WebM Project, nor the names
    of its contributors may be used to endorse or promote products
    derived from this software without specific prior written
    permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
"AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

## ライセンス

Apache License 2.0

```text
Copyright 2026-2026, Shiguredo Inc.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
```
