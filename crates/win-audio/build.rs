//! 链接 vendored libopus。
//!
//! 与 `vendor/shiguredo_libvpx` 的取舍一致：**预编译静态库随仓库分发**，构建期不做
//! 下载、不要求 cmake/autotools。只覆盖出厂 GUI 的那条 leg（`x86_64-pc-windows-gnu`）；
//! 其它 Windows target 不链接库，`src/opus.rs` 退化为返回 `Unsupported` 的空壳。
//!
//! `libopus.a` 的来源、版本与 SHA-256 见 `vendor/libopus/README.md`。

fn main() {
    println!("cargo:rerun-if-changed=vendor/libopus");

    // 非 Windows 不碰任何库（crate 本身是空壳）。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let target = std::env::var("TARGET").unwrap_or_default();
    if target != "x86_64-pc-windows-gnu" {
        // MSVC 等目标暂无预编译库：不链接，opus.rs 走 `Unsupported` 分支。
        // 这条 `cargo:warning` 是**有意**的：root `cargo build`（host = MSVC）
        // 会构建本 crate，不能因此失败。
        println!(
            "cargo:warning=rust-tunnel-win-audio: 目标 {target} 没有 vendored libopus，音频实现退化为空壳"
        );
        return;
    }

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let dir = std::path::Path::new(&manifest_dir)
        .join("vendor/libopus")
        .join("x86_64-pc-windows-gnu");
    let lib = dir.join("libopus.a");
    let lib_display = lib.display();
    assert!(
        lib.is_file(),
        "缺少 {lib_display}：无法链接 libopus。见 crates/win-audio/vendor/libopus/README.md"
    );
    println!("cargo:rustc-link-search=native={}", dir.display());
    println!("cargo:rustc-link-lib=static=opus");
}
