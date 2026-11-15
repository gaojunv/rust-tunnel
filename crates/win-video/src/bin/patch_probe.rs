//! `win-video-patch-probe`：脏矩形补丁提取的真机探针。
//!
//! 用 [`capture::open_best`]（WGC → DXGI）连续采 [`FRAME_COUNT`] 帧，每帧跑一次
//! [`capture::extract_patches`]，打印变化面积、矩形数、NV12 总字节与单次耗时。
//!
//! 目标尺寸固定为 [`TARGET`]（1920×1080）：探针只关心「补丁路径本身的开销」，不参与
//! 引擎的编码尺寸协商；真实会话里目标尺寸由 `encode_dimensions` 按采集纹理实际尺寸算。
//!
//! 静止桌面上大部分帧会得到「空补丁」（`changed == false`），这是预期结果——补丁模式
//! 的省带宽来自「不编码」，不是来自「编码得更小」。

#[cfg(windows)]
use rust_tunnel_win_video::capture::{self, PatchParams};
#[cfg(windows)]
use rust_tunnel_win_video::WinVideoError;
#[cfg(windows)]
use std::time::{Duration, Instant};

/// 连续采集的帧数（60 帧约 1 秒 @60 fps）。
#[cfg(windows)]
const FRAME_COUNT: usize = 60;

/// 探针使用的固定目标尺寸（编码尺寸坐标系）。
#[cfg(windows)]
const TARGET: (u32, u32) = (1920, 1080);

#[cfg(windows)]
fn main() {
    if let Err(err) = run() {
        eprintln!("补丁探针失败: {err}");
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("win-video-patch-probe 只能在 Windows 上运行");
}

#[cfg(windows)]
fn run() -> Result<(), WinVideoError> {
    let monitors = capture::enumerate_monitors()?;
    let monitor = monitors
        .iter()
        .find(|monitor| monitor.is_primary)
        .or_else(|| monitors.first())
        .ok_or_else(|| WinVideoError::Config("没有可用显示器".to_owned()))?;
    let mut source = capture::open_best(monitor)?;
    println!(
        "== 补丁探针 ==\n  显示器: [{}] {} {}x{}\n  后端: {}\n  目标: {}x{}",
        monitor.index,
        monitor.device_name,
        monitor.width,
        monitor.height,
        source.backend(),
        TARGET.0,
        TARGET.1
    );

    let mut durations = Vec::with_capacity(FRAME_COUNT);
    let mut frames = 0_usize;
    let mut changed_frames = 0_usize;
    let mut rects_total = 0_usize;
    let mut bytes_total = 0_usize;
    for index in 0..FRAME_COUNT {
        let started = Instant::now();
        let (captured, report, grid) = match source.capture_texture_with_dirty() {
            Ok(result) => result,
            Err(err) => {
                println!("  #{:02} 采集失败: {err}", index + 1);
                break;
            }
        };
        if index == 0 {
            println!("  采集纹理: {}x{}", captured.width, captured.height);
        }
        let mut rects = 0_usize;
        let mut bytes = 0_usize;
        let mut status = "无网格";
        if let Some(grid) = &grid {
            match capture::extract_patches(
                &captured,
                grid,
                TARGET.0,
                TARGET.1,
                &PatchParams::default(),
            ) {
                Ok(Some(patches)) => {
                    rects = patches.len();
                    bytes = patches.iter().map(|patch| patch.nv12.len()).sum();
                    status = if patches.is_empty() {
                        "空补丁"
                    } else {
                        "补丁"
                    };
                    rects_total = rects_total.saturating_add(rects);
                    bytes_total = bytes_total.saturating_add(bytes);
                }
                Ok(None) => status = "不适合补丁",
                Err(err) => {
                    println!("  #{:02} 补丁提取失败: {err}", index + 1);
                    status = "提取失败";
                }
            }
        }
        if report.changed {
            changed_frames = changed_frames.saturating_add(1);
        }
        let elapsed = started.elapsed();
        durations.push(elapsed);
        frames = frames.saturating_add(1);
        println!(
            "  #{:02} 变化={:>4}‰ 矩形={rects} NV12={bytes}B 状态={status} 耗时={:>7.3} ms",
            index + 1,
            report.changed_permille,
            elapsed.as_secs_f64() * 1000.0,
        );
    }

    print_summary(frames, changed_frames, rects_total, bytes_total, &durations);
    Ok(())
}

/// 打印总帧数、变化帧数、矩形/NV12 总量与耗时中位数。
#[cfg(windows)]
fn print_summary(
    frames: usize,
    changed_frames: usize,
    rects_total: usize,
    bytes_total: usize,
    durations: &[Duration],
) {
    println!("\n== 汇总 ==");
    println!("  帧数: {frames}（有变化 {changed_frames}）");
    println!("  矩形总数: {rects_total}，NV12 总字节: {bytes_total}");
    match median(durations) {
        Some(median) => println!(
            "  单次（采集 + 补丁提取）耗时中位数: {:.3} ms",
            median.as_secs_f64() * 1000.0
        ),
        None => println!("  没有成功采集到任何帧"),
    }
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
