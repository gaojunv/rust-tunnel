//! bindgen 生成的 `nvEncodeAPI.h` 绑定（仅编码侧；不需要 CUVID）。
//!
//! 保持上游的 `pub mod nvEncodeAPI`：`sys/guid.rs` 与 `sys/version.rs` 通过
//! `super::nvEncodeAPI` 引用类型，`sys/mod.rs` 的 `pub use windows_sys::*`
//! 会把该模块一并重导出。

pub mod nvEncodeAPI;

pub use nvEncodeAPI::*;
