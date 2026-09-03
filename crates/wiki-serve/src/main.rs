//! wiki-desktop 二进制入口（Tauri 2）。

// Release 构建在 Windows 上隐藏控制台窗口（双击启动仅显示 GUI）
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(feature = "tauri")]
fn main() {
    if let Err(err) = rust_tunnel_wiki_serve::run() {
        eprintln!("wiki-desktop failed to start: {err}");
        std::process::exit(1);
    }
}

#[cfg(not(feature = "tauri"))]
fn main() {
    println!(
        "wiki-desktop: Tauri runtime not enabled.\n\
         Rebuild with `--features tauri` to launch the desktop app.\n\
         Without it this crate is still fully testable:\n\
           cargo test -p rust-tunnel-wiki-serve --lib"
    );
}
