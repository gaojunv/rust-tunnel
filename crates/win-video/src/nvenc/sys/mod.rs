//! NVIDIA NVENC 的机器生成绑定（vendored）。
//!
//! 来源：`moq-nvenc` 0.1.2 的 `sys/windows_sys`（MIT，见同目录
//! `LICENSE-MIT`），由 bindgen 从 NVIDIA `nv-codec-headers`（MIT）生成。
//! 保留原始生成结果，**不要手改**；需要升级时整体替换。
//!
//! 绑定对应 NVENC API 12.1。驱动向后兼容，较新的驱动照样接受这些结构体版本。

#[allow(warnings)]
#[rustfmt::skip]
mod guid;
#[allow(warnings)]
#[rustfmt::skip]
mod version;

// 生成代码包含 bindgen 的布局断言测试与大量非 snake_case 符号；workspace 的
// pedantic/restriction lint 在这里不适用（与上游 crate 的处理一致）。
// **`#[rustfmt::skip]` 必须保留**：否则 `cargo fmt` 会重排 1.1 万行生成代码，
// 下次升级绑定时会制造一片无意义的 diff。
#[allow(warnings, clippy::all, clippy::pedantic, clippy::restriction, missing_docs)]
#[rustfmt::skip]
mod windows_sys;

pub use windows_sys::*;
