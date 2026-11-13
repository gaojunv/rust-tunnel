//! `win-video-probe`：Windows 硬件编码/采集探针。

#[cfg(windows)]
fn main() {
    if let Err(err) = rust_tunnel_win_video::probe::run() {
        eprintln!("探针失败: {err}");
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("win-video-probe 只能在 Windows 上运行");
}
