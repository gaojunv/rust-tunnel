# vendored libopus

`x86_64-pc-windows-gnu/libopus.a` 是从 MSYS2 的二进制包直接取出的**静态库**，供
`crates/win-audio` 的 `build.rs` 链接。构建期不下载、不调用 cmake/meson。

## 来源

| 项 | 值 |
|---|---|
| 发行版 | MSYS2 `mingw64` |
| 包 | `mingw-w64-x86_64-opus` |
| 版本 | `1.6.1-2` |
| 构建 CRT | **MSVCRT**（MSYS2 的 mingw64 环境；与出厂 GUI 的 `x86_64-pc-windows-gnu` leg 一致） |
| 文件 | `libopus.a`（734302 字节） |
| SHA-256 | `ded3c065aa1edc47e8acef63aba3ba3bf890952f9fe9554286b120dd4aa31983` |

许可证：BSD-3-Clause，见同目录 `COPYING`。与项目 MIT/Apache-2.0 相容。

## 复现

```bash
# 在 MSYS2 MINGW64 shell 里
pacman -S --noconfirm mingw-w64-x86_64-opus
sha256sum /mingw64/lib/libopus.a
cp /mingw64/lib/libopus.a vendor/libopus/x86_64-pc-windows-gnu/
cp /mingw64/share/licenses/opus/COPYING vendor/libopus/
```

## 其它目标

- `x86_64-pc-windows-msvc`：**暂无**预编译库。`build.rs` 不链接，`src/opus.rs`
  退化为返回 `Unsupported` 的空壳（root `cargo build` 在 MSVC host 上仍可过）。
- macOS / Linux：`crates/win-audio` 不参与编译（`client-gui` 只在
  `cfg(windows)` 下依赖它），因此不需要 libopus。macOS 音频落地时再补
  `aarch64-apple-darwin` 的静态库与对应 cfg。

## 为什么不用 `audiopus_sys` 之类

那类 crate 在构建期要求 cmake/autotools 与目标 C 工具链；本仓库的发布 runner
（MSYS2 mingw64）与 macOS 开发机并不保证这些。预编译静态库 + 明确的目标矩阵，
与 `vendor/shiguredo_libvpx` 的做法一致。
