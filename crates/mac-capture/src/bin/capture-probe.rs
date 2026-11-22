//! 真机探针：屏幕采集的显示器枚举、采集/补丁基准与光标形状采样。
//!
//! 用法（在 Mac 上）：
//!
//! ```text
//! cargo run -p rust-tunnel-mac-capture --bin capture-probe -- list
//! cargo run -p rust-tunnel-mac-capture --bin capture-probe -- bench 5
//! cargo run -p rust-tunnel-mac-capture --bin capture-probe -- cursor
//! ```
//!
//! 第一次运行会触发系统「屏幕录制」授权对话框；没授权时所有命令都会给出中文提示。
//! 非 macOS 平台只打印一行说明（本 crate 在那边是空壳）。

fn main() {
    #[cfg(target_os = "macos")]
    macos::run();
    #[cfg(not(target_os = "macos"))]
    println!("capture-probe：仅 macOS 支持 ScreenCaptureKit 采集");
}

#[cfg(target_os = "macos")]
mod macos {
    use std::time::{Duration, Instant};

    use rust_tunnel_mac_capture::{CursorShape, MacCapture};

    /// 探针里固定用的编码宽度上限（与引擎默认一致）。
    const MAX_ENCODE_WIDTH: u32 = 1920;

    pub fn run() {
        let args: Vec<String> = std::env::args().collect();
        let command = args.get(1).map_or("list", String::as_str);
        match command {
            "list" => list(),
            "bench" => {
                let seconds = args
                    .get(2)
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(5);
                bench(seconds);
            }
            "cursor" => cursor(),
            other => {
                eprintln!("未知命令：{other}（可用：list / bench [秒] / cursor）");
                std::process::exit(2);
            }
        }
    }

    /// 打开采集源（不建流；失败时打印原因并返回 `None`）。
    fn open() -> Option<MacCapture> {
        match MacCapture::new() {
            Ok(capture) => Some(capture),
            Err(err) => {
                eprintln!("创建采集源失败：{err}");
                None
            }
        }
    }

    /// 选主显示器；没有主显示器时取第一台。
    fn pick_display(capture: &MacCapture) -> Option<u32> {
        let displays = capture.displays();
        if displays.is_empty() {
            eprintln!("没有枚举到显示器（屏幕录制权限未授予？）");
            return None;
        }
        let chosen = displays
            .iter()
            .find(|display| display.primary)
            .unwrap_or_else(|| &displays[0]);
        Some(chosen.id)
    }

    fn list() {
        let Some(capture) = open() else {
            return;
        };
        let displays = capture.displays();
        if displays.is_empty() {
            eprintln!("没有枚举到显示器（屏幕录制权限未授予？）");
            return;
        }
        for display in &displays {
            println!(
                "显示器 {} · {}x{} 物理像素 · {}",
                display.id,
                display.width,
                display.height,
                if display.primary {
                    "主显示器"
                } else {
                    "副显示器"
                }
            );
        }
        let info = capture.info();
        println!(
            "采集后端：{} · GPU 采集：{} · 无新帧累计：{}",
            info.backend, info.gpu_capture, info.no_new_frame
        );
    }

    /// 采集 + 补丁两段基准（各自跑 `seconds` 秒）。
    fn bench(seconds: u64) {
        let Some(mut capture) = open() else {
            return;
        };
        let Some(display) = pick_display(&capture) else {
            return;
        };
        capture.set_max_encode_width(MAX_ENCODE_WIDTH);
        let first = Instant::now();
        match capture.capture(display) {
            Ok(frame) => println!(
                "首帧就绪：{}x{}，耗时 {} ms",
                frame.width,
                frame.height,
                first.elapsed().as_millis()
            ),
            Err(err) => {
                eprintln!("首帧采集失败：{err}");
                return;
            }
        }
        let seconds = seconds.max(1);
        bench_frames(&mut capture, display, seconds);
        bench_patches(&mut capture, display, seconds);
        let info = capture.info();
        println!(
            "采集后端：{} · 无新帧累计：{} · 回退：{}",
            info.backend, info.no_new_frame, info.fallbacks
        );
        capture.stop();
    }

    /// 整帧路径基准。
    #[expect(
        clippy::cast_precision_loss,
        reason = "探针只打印统计平均值，帧数字远小于 f64 精度上限"
    )]
    fn bench_frames(capture: &mut MacCapture, display: u32, seconds: u64) {
        let mut frames = 0_u64;
        let mut changed_frames = 0_u64;
        let mut frame_us = 0_u128;
        let mut rgba_bytes = 0_u64;
        let deadline = Instant::now() + Duration::from_secs(seconds);
        while Instant::now() < deadline {
            let started = Instant::now();
            match capture.capture(display) {
                Ok(frame) => {
                    frames = frames.saturating_add(1);
                    frame_us = frame_us.saturating_add(started.elapsed().as_micros());
                    rgba_bytes = rgba_bytes.saturating_add(frame.rgba.len() as u64);
                    if frame.changed {
                        changed_frames = changed_frames.saturating_add(1);
                    }
                }
                Err(err) => {
                    eprintln!("采集失败：{err}");
                    break;
                }
            }
        }
        if frames == 0 {
            eprintln!("整帧路径没有采到任何帧");
            return;
        }
        let fps = frames as f64 / seconds as f64;
        let average_ms = frame_us as f64 / frames as f64 / 1000.0;
        let average_kb = rgba_bytes as f64 / frames as f64 / 1024.0;
        let changed_pct = changed_frames as f64 * 100.0 / frames as f64;
        println!(
            "整帧：{frames} 帧 · {fps:.1} fps · 平均采集 {average_ms:.2} ms · 平均 {average_kb:.0} KB · 有变化 {changed_pct:.0}%"
        );
    }

    /// 补丁路径基准（模拟引擎的低活跃度分支）。
    #[expect(
        clippy::cast_precision_loss,
        reason = "探针只打印统计平均值，帧数字远小于 f64 精度上限"
    )]
    fn bench_patches(capture: &mut MacCapture, display: u32, seconds: u64) {
        let mut patches = 0_u64;
        let mut keepalives = 0_u64;
        let mut unavailable = 0_u64;
        let mut rects = 0_u64;
        let mut nv12_bytes = 0_u64;
        let mut patch_us = 0_u128;
        let deadline = Instant::now() + Duration::from_secs(seconds);
        while Instant::now() < deadline {
            let started = Instant::now();
            match capture.capture_patches(display, 1.0, MAX_ENCODE_WIDTH) {
                Ok(Some(result)) => {
                    patch_us = patch_us.saturating_add(started.elapsed().as_micros());
                    rects = rects.saturating_add(result.rects.len() as u64);
                    for rect in &result.rects {
                        nv12_bytes = nv12_bytes.saturating_add(rect.nv12.len() as u64);
                    }
                    if result.rects.is_empty() {
                        keepalives = keepalives.saturating_add(1);
                    } else {
                        patches = patches.saturating_add(1);
                    }
                }
                Ok(None) => unavailable = unavailable.saturating_add(1),
                Err(err) => {
                    eprintln!("补丁采集失败：{err}");
                    break;
                }
            }
        }
        let total = patches
            .saturating_add(keepalives)
            .saturating_add(unavailable);
        if total == 0 {
            eprintln!("补丁路径没有采到任何拍");
            return;
        }
        let average_ms = patch_us as f64 / total as f64 / 1000.0;
        let average_rects = rects as f64 / total as f64;
        let average_kb = nv12_bytes as f64 / total as f64 / 1024.0;
        println!(
            "补丁：{total} 拍 · 有补丁 {patches} · 保活 {keepalives} · 不适用 {unavailable} · 平均 {average_rects:.1} 块 · 平均 {average_kb:.0} KB · 平均采集 {average_ms:.2} ms"
        );
    }

    /// 1 秒内采样光标形状（变化时打印）。
    fn cursor() {
        let Some(capture) = open() else {
            return;
        };
        let mut previous: Option<CursorShape> = None;
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            let shape = capture.cursor_shape();
            if shape != previous {
                match shape {
                    Some(shape) => println!("光标形状：{shape:?}"),
                    None => println!("光标形状：读不到（None）"),
                }
                previous = shape;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if previous.is_none() {
            println!("NSCursor.currentSystemCursor 返回了 nil（macOS 版本差异），光标形状不可用");
        }
    }
}
