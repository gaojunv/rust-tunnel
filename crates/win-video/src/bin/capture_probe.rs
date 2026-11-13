//! `win-video-capture-probe`：真机采集探针。
//!
//! 打印显示器列表，用 [`capture::open_best`]（WGC → DXGI）连续采 10 帧并统计
//! 耗时，把第一帧写成 32 位 BMP（负高度 = 自顶向下），最后分别强制尝试 DXGI
//! 与 WGC，便于对比两个后端在本机上的可用性。

#[cfg(windows)]
use rust_tunnel_win_video::capture::{self, CaptureSource, CapturedFrame, MonitorInfo};
#[cfg(windows)]
use rust_tunnel_win_video::WinVideoError;
#[cfg(windows)]
use std::collections::hash_map::DefaultHasher;
#[cfg(windows)]
use std::hash::{Hash, Hasher};
#[cfg(windows)]
use std::path::Path;
#[cfg(windows)]
use std::time::{Duration, Instant};

/// 连续采集的帧数。
#[cfg(windows)]
const FRAME_COUNT: usize = 10;

#[cfg(windows)]
fn main() {
    if let Err(err) = run() {
        eprintln!("采集探针失败: {err}");
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("win-video-capture-probe 只能在 Windows 上运行");
}

#[cfg(windows)]
fn run() -> Result<(), WinVideoError> {
    let monitors = capture::enumerate_monitors()?;
    print_monitors(&monitors);

    let monitor = monitors
        .iter()
        .find(|m| m.is_primary)
        .or_else(|| monitors.first())
        .ok_or_else(|| WinVideoError::Config("没有可用显示器".to_owned()))?;

    println!("\n== open_best ==");
    let mut source = capture::open_best(monitor)?;
    println!(
        "  目标显示器: [{}] {} {}x{}",
        monitor.index, monitor.device_name, monitor.width, monitor.height
    );
    println!("  实际后端: {}", source.backend());

    println!("\n== 连续采集 {FRAME_COUNT} 帧 ==");
    let (durations, first_frame) = collect_frames(source.as_mut());
    print_timings(&durations);

    println!("\n== 写入 BMP ==");
    match &first_frame {
        Some(frame) => write_bmp_report(frame)?,
        None => println!("  没有可写的帧"),
    }

    // 先释放首个采集源，再做两个后端的独立可用性对比，避免同时占用多个会话。
    drop(source);
    probe_forced_backends(monitor);
    Ok(())
}

/// 打印显示器列表。
#[cfg(windows)]
fn print_monitors(monitors: &[MonitorInfo]) {
    println!("== 显示器（{} 块）==", monitors.len());
    for monitor in monitors {
        println!(
            "  [{}] HMONITOR={:?} 位置=({}, {}) 尺寸={}x{} 主屏={} 设备={}",
            monitor.index,
            monitor.hmonitor,
            monitor.left,
            monitor.top,
            monitor.width,
            monitor.height,
            monitor.is_primary,
            monitor.device_name
        );
    }
}

/// 连续采集并打印每帧耗时，返回耗时列表与第一帧。
#[cfg(windows)]
fn collect_frames(source: &mut dyn CaptureSource) -> (Vec<Duration>, Option<CapturedFrame>) {
    let mut durations = Vec::with_capacity(FRAME_COUNT);
    let mut first_frame: Option<CapturedFrame> = None;
    let mut previous_hash: Option<u64> = None;
    for index in 0..FRAME_COUNT {
        let started = Instant::now();
        match source.capture() {
            Ok(frame) => {
                let elapsed = started.elapsed();
                durations.push(elapsed);
                let hash = hash_bytes(&frame.bgra);
                let relation = if previous_hash == Some(hash) {
                    "与上一帧相同"
                } else {
                    "新帧"
                };
                previous_hash = Some(hash);
                println!(
                    "  #{:02} {:>7.2} ms {}x{} {relation} hash={hash:016x}",
                    index + 1,
                    elapsed.as_secs_f64() * 1000.0,
                    frame.width,
                    frame.height
                );
                if first_frame.is_none() {
                    first_frame = Some(frame);
                }
            }
            Err(err) => {
                println!("  #{:02} 采集失败: {err}", index + 1);
                break;
            }
        }
    }
    (durations, first_frame)
}

/// 打印耗时统计。
#[cfg(windows)]
fn print_timings(durations: &[Duration]) {
    if durations.is_empty() {
        println!("  没有成功采集到任何帧");
        return;
    }
    let min = durations.iter().min().copied().unwrap_or_default();
    let max = durations.iter().max().copied().unwrap_or_default();
    let median = median(durations).unwrap_or_default();
    println!(
        "  中位数 {:.2} ms / 最小 {:.2} ms / 最大 {:.2} ms（成功 {} 帧）",
        median.as_secs_f64() * 1000.0,
        min.as_secs_f64() * 1000.0,
        max.as_secs_f64() * 1000.0,
        durations.len()
    );
}

/// 写 BMP 并打印路径/尺寸。
#[cfg(windows)]
fn write_bmp_report(frame: &CapturedFrame) -> Result<(), WinVideoError> {
    let path = std::env::temp_dir().join("rust-tunnel-capture-probe.bmp");
    write_bmp(&path, frame)?;
    println!("  路径: {}", path.display());
    println!(
        "  尺寸: {}x{}，像素 {} 字节（32 位 BGRA，负高度 = 自顶向下）",
        frame.width,
        frame.height,
        frame.bgra.len()
    );
    Ok(())
}

/// 分别强制尝试 DXGI 与 WGC，打印可用性与首帧结果。
#[cfg(windows)]
fn probe_forced_backends(monitor: &MonitorInfo) {
    println!("\n== 强制 DXGI ==");
    match capture::open_dxgi(monitor) {
        Ok(mut source) => {
            println!("  DXGI 可用，后端={}", source.backend());
            match source.capture() {
                Ok(frame) => println!("  首帧 {}x{}", frame.width, frame.height),
                Err(err) => println!("  首帧采集失败: {err}"),
            }
        }
        Err(err) => println!("  DXGI 不可用: {err}"),
    }

    println!("\n== 强制 WGC ==");
    match capture::open_wgc(monitor) {
        Ok(mut source) => {
            println!("  WGC 可用，后端={}", source.backend());
            match source.capture() {
                Ok(frame) => println!("  首帧 {}x{}", frame.width, frame.height),
                Err(err) => println!("  首帧采集失败: {err}"),
            }
        }
        Err(err) => println!("  WGC 不可用: {err}"),
    }
}

/// 对像素做进程内哈希，用来判断相邻两帧是否字节相同（探针诊断用）。
#[cfg(windows)]
fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// 中位数（拷贝后排序）。
#[cfg(windows)]
fn median(durations: &[Duration]) -> Option<Duration> {
    if durations.is_empty() {
        return None;
    }
    let mut sorted = durations.to_vec();
    sorted.sort_unstable();
    sorted.get(sorted.len() / 2).copied()
}

/// 写 32 位 BMP：`biHeight` 取负表示自顶向下，像素按 BGRA 原样写入。
#[cfg(windows)]
fn write_bmp(path: &Path, frame: &CapturedFrame) -> Result<(), WinVideoError> {
    let width = i32::try_from(frame.width)
        .map_err(|_| WinVideoError::Config(format!("帧宽 {} 超出 BMP 范围", frame.width)))?;
    let height = i32::try_from(frame.height)
        .map_err(|_| WinVideoError::Config(format!("帧高 {} 超出 BMP 范围", frame.height)))?;
    let image_size = u32::try_from(frame.bgra.len())
        .map_err(|_| WinVideoError::Config("像素数据超过 4 GiB".to_owned()))?;
    // 14 字节文件头 + 40 字节信息头。
    let file_size = image_size
        .checked_add(54)
        .ok_or_else(|| WinVideoError::Config("BMP 文件大小溢出".to_owned()))?;

    let mut out = Vec::with_capacity(usize::try_from(file_size).unwrap_or_default());
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&file_size.to_le_bytes());
    out.extend_from_slice(&0_u32.to_le_bytes()); // bfReserved1 + bfReserved2
    out.extend_from_slice(&54_u32.to_le_bytes()); // bfOffBits

    out.extend_from_slice(&40_u32.to_le_bytes()); // biSize
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&(-height).to_le_bytes()); // 负高度 = 自顶向下
    out.extend_from_slice(&1_u16.to_le_bytes()); // biPlanes
    out.extend_from_slice(&32_u16.to_le_bytes()); // biBitCount
    out.extend_from_slice(&0_u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&image_size.to_le_bytes());
    out.extend_from_slice(&2835_i32.to_le_bytes()); // 约 72 DPI
    out.extend_from_slice(&2835_i32.to_le_bytes());
    out.extend_from_slice(&0_u32.to_le_bytes()); // biClrUsed
    out.extend_from_slice(&0_u32.to_le_bytes()); // biClrImportant

    out.extend_from_slice(&frame.bgra);

    std::fs::write(path, &out).map_err(|err| {
        WinVideoError::Capture(format!("写入 BMP 失败（{}）: {err}", path.display()))
    })
}
