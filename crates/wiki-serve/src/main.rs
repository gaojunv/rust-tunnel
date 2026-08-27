//! wiki-desktop 二进制入口（Tauri 2）。

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
