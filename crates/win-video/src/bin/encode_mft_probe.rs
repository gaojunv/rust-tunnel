//! `win-video-encode-mft-probe`：Media Foundation 硬件 H.264 编码真机探针。
//!
//! 1. 列出硬件 / 全部 H.264 编码器 MFT（名称 + CLSID）。
//! 2. 用硬件 MFT 编码 1280x720 与 1920x1080 各 10 帧合成 RGBA，打印每帧墙钟、
//!    进程 CPU、码流字节、keyframe、IDR/SPS/PPS，并验证输出是 Annex B。
//! 3. 在线 `set_bitrate` / `set_fps` 后再编码一帧，确认**不产生 IDR**（不重建）。
//! 4. 与现场 HD 630 的 VP9 软件编码基线（113 ms/帧）对照，报告硬件是否真的更快。

#[cfg(windows)]
use rust_tunnel_win_video::decode::H264Decoder;
#[cfg(windows)]
use rust_tunnel_win_video::mft_encoder::{enumerate_h264_encoders, MftH264Encoder};
#[cfg(windows)]
use rust_tunnel_win_video::WinVideoError;
#[cfg(windows)]
use std::time::{Duration, Instant};
#[cfg(windows)]
use windows::Win32::Foundation::FILETIME;
#[cfg(windows)]
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

/// 编码帧率。
#[cfg(windows)]
const FPS: u32 = 60;
/// 每组分辨率的帧数（1 IDR + 9 P 帧）。
#[cfg(windows)]
const FRAME_COUNT: u32 = 10;
/// 对照的分辨率。
#[cfg(windows)]
const RESOLUTIONS: [(u32, u32); 2] = [(1280, 720), (1920, 1080)];
/// 在码流中途强制一次关键帧的帧号（验证 `CODECAPI_AVEncVideoForceKeyFrame`
/// 不是只有首帧碰巧是 IDR）。
#[cfg(windows)]
const FORCED_KEYFRAME_INDEX: u32 = 5;
/// 现场 HD 630 的 VP9 软件编码基线（毫秒/帧）。
#[cfg(windows)]
const VP9_SOFTWARE_BASELINE_MS: f64 = 113.0;

#[cfg(windows)]
fn main() {
    if let Err(err) = run() {
        eprintln!("编码探针失败: {err}");
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("win-video-encode-mft-probe 只能在 Windows 上运行");
}

/// 构造一帧自顶向下 RGBA：渐变背景 + 随帧号移动的白色方块。
#[cfg(windows)]
fn synthetic_rgba(width: u32, height: u32, frame: u32) -> Vec<u8> {
    let w = usize::try_from(width).unwrap_or(0);
    let h = usize::try_from(height).unwrap_or(0);
    let mut buf = vec![0u8; w.saturating_mul(h).saturating_mul(4)];
    for y in 0..h {
        for x in 0..w {
            let index = y.saturating_mul(w).saturating_add(x).saturating_mul(4);
            let Some(pixel) = buf.get_mut(index..index.saturating_add(4)) else {
                continue;
            };
            pixel[0] = u8::try_from(x & 0xFF).unwrap_or(0);
            pixel[1] = u8::try_from(y & 0xFF).unwrap_or(0);
            pixel[2] = u8::try_from((x + y) & 0xFF).unwrap_or(0);
            pixel[3] = 255;
        }
    }
    let block = 120usize;
    if w > block && h > block {
        let shift = usize::try_from(frame).unwrap_or(0);
        let start_x = shift.saturating_mul(17) % (w - block);
        let start_y = shift.saturating_mul(11) % (h - block);
        for y in start_y..start_y.saturating_add(block) {
            for x in start_x..start_x.saturating_add(block) {
                let index = y.saturating_mul(w).saturating_add(x).saturating_mul(4);
                let Some(pixel) = buf.get_mut(index..index.saturating_add(4)) else {
                    continue;
                };
                pixel[0] = 255;
                pixel[1] = 255;
                pixel[2] = 255;
            }
        }
    }
    buf
}

/// `FILETIME` → 100ns 计数。
#[cfg(windows)]
fn filetime_ticks(time: FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

/// 当前进程累计 CPU 时间（内核 + 用户，100ns）。
#[cfg(windows)]
fn process_cpu_ticks() -> u64 {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: 四个输出指针都指向本栈帧；`GetCurrentProcess` 返回伪句柄。
    let result = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &raw mut creation,
            &raw mut exit,
            &raw mut kernel,
            &raw mut user,
        )
    };
    if result.is_err() {
        return 0;
    }
    filetime_ticks(kernel).saturating_add(filetime_ticks(user))
}

/// 耗时中位数（毫秒）。
#[cfg(windows)]
fn median_ms(times: &[Duration]) -> f64 {
    let mut values: Vec<f64> = times
        .iter()
        .map(|time| time.as_secs_f64() * 1000.0)
        .collect();
    values.sort_by(f64::total_cmp);
    if values.is_empty() {
        return 0.0;
    }
    values.get(values.len() / 2).copied().unwrap_or_default()
}

/// 耗时均值（毫秒）。
#[cfg(windows)]
fn mean_ms(times: &[Duration]) -> f64 {
    if times.is_empty() {
        return 0.0;
    }
    let total: f64 = times.iter().map(|time| time.as_secs_f64() * 1000.0).sum();
    total / f64::from(u32::try_from(times.len()).unwrap_or(1))
}

/// 统计 Annex B 码流里的 NAL：`(IDR, SPS, PPS)`。
#[cfg(windows)]
fn nal_summary(annex_b: &[u8]) -> (bool, bool, bool) {
    let mut idr = false;
    let mut sps = false;
    let mut pps = false;
    let mut index = 0usize;
    while index + 3 <= annex_b.len() {
        if annex_b[index] == 0 && annex_b[index + 1] == 0 && annex_b[index + 2] == 1 {
            let start = index + 3;
            if let Some(&header) = annex_b.get(start) {
                match header & 0x1F {
                    5 => idr = true,
                    7 => sps = true,
                    8 => pps = true,
                    _ => {}
                }
            }
            index = start;
        } else {
            index += 1;
        }
    }
    (idr, sps, pps)
}

/// 两帧 RGBA 的平均绝对差（只比较 R/G/B，0–255）。
#[cfg(windows)]
fn mean_abs_diff_rgba(original: &[u8], decoded: &[u8]) -> Option<f64> {
    if original.is_empty() || decoded.len() < original.len() {
        return None;
    }
    let mut total = 0.0f64;
    let mut count = 0u32;
    for (source, target) in original
        .as_chunks::<4>()
        .0
        .iter()
        .zip(decoded.as_chunks::<4>().0)
    {
        for channel in 0..3 {
            total += f64::from(source[channel].abs_diff(target[channel]));
            count = count.saturating_add(1);
        }
    }
    if count == 0 {
        None
    } else {
        Some(total / f64::from(count))
    }
}

/// 打印硬件与全部编码器列表。
#[cfg(windows)]
fn print_encoders() -> Result<(), WinVideoError> {
    let hardware = enumerate_h264_encoders(true)?;
    println!(
        "== 硬件 H.264 编码器（MFT_ENUM_FLAG_HARDWARE，{} 个）==",
        hardware.len()
    );
    for info in &hardware {
        println!("  {} | {}", info.name, info.clsid);
    }
    let all = enumerate_h264_encoders(false)?;
    let hardware_ids: Vec<&str> = hardware.iter().map(|info| info.clsid.as_str()).collect();
    println!(
        "\n== 全部 H.264 编码器（MFT_ENUM_FLAG_ALL，{} 个）==",
        all.len()
    );
    for info in &all {
        let tag = if hardware_ids.contains(&info.clsid.as_str()) {
            "硬件"
        } else {
            "软件/其他"
        };
        println!("  [{tag}] {} | {}", info.name, info.clsid);
    }
    Ok(())
}

/// 一次编码运行的统计。
#[cfg(windows)]
struct EncodeStats {
    /// 每帧 `encode_rgba` 的墙钟耗时。
    times: Vec<Duration>,
    /// 实际产出非空码流的帧数。
    produced: u32,
    /// 第一个非空包是否关键帧。
    first_keyframe: bool,
    /// 第一个非空包是否带 SPS+PPS。
    first_has_parameter_sets: bool,
    /// 第一个非空包的字节数（不含 4 字节前缀）。
    first_bytes: usize,
    /// 第一个非空包前 8 字节（验证 Annex B 起始码）。
    first_preview: Vec<u8>,
    /// P 帧总字节数 / 数量。
    p_bytes: u64,
    p_count: u32,
    /// 中途强制的那一帧是否真的产出了 IDR + SPS/PPS。
    midstream_keyframe_ok: bool,
    /// 整组编码消耗的进程 CPU 时间。
    cpu_total: Duration,
}

/// 编码一组合成帧并打印逐帧结果。
#[cfg(windows)]
fn encode_frames(
    encoder: &mut MftH264Encoder,
    width: u32,
    height: u32,
) -> Result<EncodeStats, WinVideoError> {
    let mut times = Vec::with_capacity(usize::try_from(FRAME_COUNT).unwrap_or(0));
    let mut stats = EncodeStats {
        times: Vec::new(),
        produced: 0,
        first_keyframe: false,
        first_has_parameter_sets: false,
        first_bytes: 0,
        first_preview: Vec::new(),
        p_bytes: 0,
        p_count: 0,
        midstream_keyframe_ok: false,
        cpu_total: Duration::ZERO,
    };
    let cpu_before = process_cpu_ticks();
    for index in 0..FRAME_COUNT {
        let rgba = synthetic_rgba(width, height, index);
        let force_keyframe = index == 0 || index == FORCED_KEYFRAME_INDEX;
        let started = Instant::now();
        let packet = encoder.encode_rgba(&rgba, force_keyframe)?;
        let elapsed = started.elapsed();
        times.push(elapsed);
        if packet.data.is_empty() {
            println!(
                "  帧 {index:>2}: {:6.2} ms, 无输出（编码器流水线缓冲，下一帧补齐）",
                elapsed.as_secs_f64() * 1000.0
            );
            continue;
        }
        let payload = packet.data.get(4..).unwrap_or_default();
        let (idr, sps, pps) = nal_summary(payload);
        let bytes = payload.len();
        if stats.produced == 0 {
            stats.first_keyframe = packet.keyframe;
            stats.first_has_parameter_sets = sps && pps;
            stats.first_bytes = bytes;
            stats.first_preview = payload.iter().take(8).copied().collect();
        } else if !packet.keyframe {
            stats.p_bytes = stats
                .p_bytes
                .saturating_add(u64::try_from(bytes).unwrap_or(0));
            stats.p_count = stats.p_count.saturating_add(1);
        }
        stats.produced = stats.produced.saturating_add(1);
        if index == FORCED_KEYFRAME_INDEX {
            stats.midstream_keyframe_ok = packet.keyframe && idr && sps && pps;
            if !stats.midstream_keyframe_ok {
                return Err(WinVideoError::Encode(
                    "中途强制关键帧失败：输出不是带 SPS/PPS 的 IDR".to_owned(),
                ));
            }
        }
        println!(
            "  帧 {index:>2}: {:6.2} ms, {bytes:>7} 字节, keyframe={}, IDR={idr}, SPS={sps}, PPS={pps}{}",
            elapsed.as_secs_f64() * 1000.0,
            packet.keyframe,
            if force_keyframe { " [强制]" } else { "" }
        );
    }
    let cpu_after = process_cpu_ticks();
    stats.cpu_total =
        Duration::from_nanos(cpu_after.saturating_sub(cpu_before).saturating_mul(100));
    stats.times = times;
    Ok(stats)
}

/// 打印一组结果并校验关键帧/SPS/PPS/格式。
#[cfg(windows)]
fn print_stats(
    stats: &EncodeStats,
    width: u32,
    height: u32,
    encoder: &MftH264Encoder,
) -> Result<(), WinVideoError> {
    let cpu_ms = stats.cpu_total.as_secs_f64() * 1000.0;
    println!(
        "  汇总: 输出 {}/{} 帧, 墙钟中位 {:.2} ms / 均值 {:.2} ms, CPU 总计 {:.1} ms（每帧 {:.2} ms）",
        stats.produced,
        FRAME_COUNT,
        median_ms(&stats.times),
        mean_ms(&stats.times),
        cpu_ms,
        cpu_ms / f64::from(FRAME_COUNT),
    );
    println!(
        "  首包: {} 字节, keyframe={}, SPS+PPS={}, 前 8 字节={:02X?}",
        stats.first_bytes,
        stats.first_keyframe,
        stats.first_has_parameter_sets,
        stats.first_preview
    );
    if stats.produced > 0 {
        println!(
            "  P 帧平均 {} 字节（{} 帧）",
            stats
                .p_bytes
                .checked_div(u64::from(stats.p_count))
                .unwrap_or(0),
            stats.p_count
        );
    }
    if !stats.first_keyframe || !stats.first_has_parameter_sets {
        return Err(WinVideoError::Encode(format!(
            "首包不是带 SPS/PPS 的关键帧（{width}x{height}）"
        )));
    }
    if !stats.midstream_keyframe_ok {
        return Err(WinVideoError::Encode(format!(
            "中途强制关键帧未生效（{width}x{height}，帧 {FORCED_KEYFRAME_INDEX}）"
        )));
    }
    if stats.first_preview.get(..4) != Some(&[0, 0, 0, 1]) {
        return Err(WinVideoError::Encode(format!(
            "首包不是 Annex B（前 4 字节 {:02X?}）",
            stats.first_preview
        )));
    }
    let mean = mean_ms(&stats.times);
    let ratio = VP9_SOFTWARE_BASELINE_MS / mean.max(f64::MIN_POSITIVE);
    println!(
        "  对照现场 HD 630 VP9 软件编码 {VP9_SOFTWARE_BASELINE_MS:.0} ms/帧 → 硬件约 {ratio:.1}x（{}）",
        encoder.mft_name()
    );
    Ok(())
}

/// 在线改码率/帧率后各编码一帧，确认不产生 IDR。
#[cfg(windows)]
fn check_runtime_changes(
    encoder: &mut MftH264Encoder,
    width: u32,
    height: u32,
    bitrate_kbps: u32,
) -> Result<(), WinVideoError> {
    println!("\n  == 在线改码率（不重建、不插 IDR） ==");
    match encoder.set_bitrate(bitrate_kbps / 2) {
        Ok(()) => {
            let packet = encoder.encode_rgba(&synthetic_rgba(width, height, FRAME_COUNT), false)?;
            println!(
                "  set_bitrate({}) 成功；下一帧 {} 字节, keyframe={}（期望 false）",
                bitrate_kbps / 2,
                packet.data.len().saturating_sub(4),
                packet.keyframe
            );
            if packet.keyframe {
                return Err(WinVideoError::Encode(
                    "set_bitrate 后出现关键帧：运行期改码率不应强制 IDR".to_owned(),
                ));
            }
        }
        Err(err) => println!("  set_bitrate 不支持运行期修改: {err}"),
    }

    println!("\n  == 在线改帧率（不重建、不插 IDR） ==");
    match encoder.set_fps(30) {
        Ok(()) => {
            let packet =
                encoder.encode_rgba(&synthetic_rgba(width, height, FRAME_COUNT + 1), false)?;
            println!(
                "  set_fps(30) 成功；下一帧 {} 字节, keyframe={}（期望 false）",
                packet.data.len().saturating_sub(4),
                packet.keyframe
            );
            if packet.keyframe {
                return Err(WinVideoError::Encode(
                    "set_fps 后出现关键帧：运行期改帧率不应强制 IDR".to_owned(),
                ));
            }
        }
        Err(err) => println!("  set_fps 不支持运行期修改: {err}"),
    }
    Ok(())
}

/// 某个分辨率的完整探针。
#[cfg(windows)]
fn run_resolution(width: u32, height: u32) -> Result<(), WinVideoError> {
    let bitrate_kbps = if width >= 1920 { 8000 } else { 4000 };
    println!("\n== 硬件编码：{width}x{height} @ {FPS} fps, CBR {bitrate_kbps} kbps ==");
    let mut encoder = MftH264Encoder::new(width, height, FPS, bitrate_kbps)?;
    println!(
        "  MFT: {} | {}\n  D3D11 适配器: {}\n  尺寸: {:?} @ {} fps, {} kbps",
        encoder.mft_name(),
        encoder.mft_clsid(),
        encoder.adapter_name(),
        encoder.dimensions(),
        encoder.fps(),
        encoder.bitrate_kbps()
    );
    let stats = encode_frames(&mut encoder, width, height)?;
    print_stats(&stats, width, height, &encoder)?;
    check_runtime_changes(&mut encoder, width, height, bitrate_kbps)?;
    Ok(())
}

/// QSV 编码 → 现有 H.264 解码器的端到端回环。
///
/// 验证：4 字节长度前缀、Annex B 起始码、首帧 IDR+SPS/PPS、解码尺寸
/// （1080p 必须裁到 1920x1080，而不是编码高度 1088），以及输入/输出像素 MAD。
#[cfg(windows)]
fn run_end_to_end(width: u32, height: u32) -> Result<(), WinVideoError> {
    let bitrate_kbps = if width >= 1920 { 8000 } else { 4000 };
    println!("\n== 端到端：QSV 编码 → H264Decoder（{width}x{height}）==");
    let mut encoder = match MftH264Encoder::new(width, height, FPS, bitrate_kbps) {
        Ok(encoder) => encoder,
        Err(err) => {
            println!("  跳过：硬件编码器不可用: {err}");
            return Ok(());
        }
    };
    let mut decoder = match H264Decoder::new(width, height) {
        Ok(decoder) => decoder,
        Err(err) => {
            println!("  跳过：H264Decoder 不可用: {err}");
            return Ok(());
        }
    };
    println!(
        "  编码 MFT: {}；解码路径: {}",
        encoder.mft_name(),
        decoder.path().as_str()
    );

    let mut decoded_count = 0u32;
    let mut mad_total = 0.0f64;
    for index in 0..FRAME_COUNT {
        let rgba = synthetic_rgba(width, height, index);
        let packet = encoder.encode_rgba(&rgba, index == 0)?;
        if packet.data.is_empty() {
            println!("  帧 {index:>2}: 编码器缓冲，跳过");
            continue;
        }
        let Some(prefix) = packet.data.get(..4) else {
            return Err(WinVideoError::Encode("编码包缺少长度前缀".to_owned()));
        };
        let declared = usize::try_from(u32::from_be_bytes([
            prefix[0], prefix[1], prefix[2], prefix[3],
        ]))
        .unwrap_or(usize::MAX);
        let payload = packet.data.get(4..).unwrap_or_default();
        if declared != payload.len() {
            return Err(WinVideoError::Encode(format!(
                "长度前缀 {declared} 与负载 {} 不一致",
                payload.len()
            )));
        }
        if !(payload.starts_with(&[0, 0, 1]) || payload.starts_with(&[0, 0, 0, 1])) {
            return Err(WinVideoError::Encode("码流不是 Annex B".to_owned()));
        }
        let (idr, sps, pps) = nal_summary(payload);
        if index == 0 && !(packet.keyframe && idr && sps && pps) {
            return Err(WinVideoError::Encode(
                "首帧不是带 SPS/PPS 的 IDR".to_owned(),
            ));
        }

        let started = Instant::now();
        let output_frame = decoder.decode(&packet.data)?;
        let elapsed = started.elapsed();
        match output_frame {
            Some(frame) => {
                if (frame.width, frame.height) != (width, height) {
                    return Err(WinVideoError::Encode(format!(
                        "解码尺寸 {}x{} 与请求 {width}x{height} 不一致（aperture 裁剪失败？）",
                        frame.width, frame.height
                    )));
                }
                let mad = mean_abs_diff_rgba(&rgba, &frame.rgba).ok_or_else(|| {
                    WinVideoError::Encode("原始 RGBA 与解码 RGBA 长度不匹配".to_owned())
                })?;
                mad_total += mad;
                decoded_count = decoded_count.saturating_add(1);
                println!(
                    "  帧 {index:>2}: 码流 {:>6} B, 解码 {:5.2} ms, 输出 {}x{}, MAD {mad:.3}",
                    payload.len(),
                    elapsed.as_secs_f64() * 1000.0,
                    frame.width,
                    frame.height,
                );
            }
            None => println!("  帧 {index:>2}: 解码器无输出"),
        }
    }
    if decoded_count == 0 {
        return Err(WinVideoError::Encode("端到端没有任何解码输出".to_owned()));
    }
    println!(
        "  汇总: 解码 {decoded_count}/{FRAME_COUNT} 帧, 平均 MAD {:.3}",
        mad_total / f64::from(decoded_count)
    );
    Ok(())
}

#[cfg(windows)]
fn run() -> Result<(), WinVideoError> {
    print_encoders()?;
    for (width, height) in RESOLUTIONS {
        run_resolution(width, height)?;
    }
    run_end_to_end(1280, 720)?;
    run_end_to_end(1920, 1080)?;
    Ok(())
}
