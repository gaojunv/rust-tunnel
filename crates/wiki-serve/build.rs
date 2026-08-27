#![allow(missing_docs)]

//! wiki-serve 构建脚本。
//!
//! 仅当启用 `tauri` feature 时才调用 `tauri_build::build()`（生成 Tauri
//! context、校验 icons、注入 Windows 资源）。未启用时是空 no-op，使不带
//! feature 的 `cargo check` 无需 webkit2gtk 等系统库即可通过。
//!
//! 门控方式：`#[cfg(feature = "tauri")]`。经实测，Cargo 会为 build script
//! 透传 crate 的 feature cfg，`cfg(feature = "search")` 随
//! `--features`/`--no-default-features` 正确翻转（与 `CARGO_FEATURE_*` 环境
//! 变量一致），因此 `cfg` 门控既可用也更直接；`CARGO_FEATURE_TAURI` 环境
//! 变量为等价的备选方案。

#[cfg(feature = "tauri")]
fn run_tauri_build() {
    tauri_build::build();
}

#[cfg(not(feature = "tauri"))]
fn run_tauri_build() {}

fn main() {
    run_tauri_build();
}
