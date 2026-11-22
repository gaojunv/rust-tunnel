//! macOS 硬件编解码往返探针。
//!
//! ```text
//! cargo run -p rust-tunnel-mac-video --bin mac-video-probe
//! ```
//!
//! 对 H.264 与 H.265 各做一次「编 30 帧 → 解 30 帧」，打印每帧耗时与解码尺寸/像素
//! 误差。非 macOS 平台只打印一行说明（本 crate 的纯逻辑模块仍可在其它平台单测）。

/// 探针帧数。
#[cfg(target_os = "macos")]
const FRAMES: u32 = 30;

/// 探针分辨率。
#[cfg(target_os = "macos")]
const WIDTH: u32 = 1920;

/// 探针分辨率。
#[cfg(target_os = "macos")]
const HEIGHT: u32 = 1080;

fn main() {
    #[cfg(target_os = "macos")]
    if !run() {
        std::process::exit(1);
    }
    #[cfg(not(target_os = "macos"))]
    println!("mac-video-probe: 仅 macOS 支持（本机平台没有 VideoToolbox）");
}

/// 跑完两个编码标准；任一环节失败返回 `false`。
#[cfg(target_os = "macos")]
fn run() -> bool {
    use rust_tunnel_mac_video::CodecKind;

    let mut ok = true;
    for codec in [CodecKind::H264, CodecKind::H265] {
        println!("=== {} ===", codec.label());
        let encoder_available = rust_tunnel_mac_video::encoder_available(codec);
        let decoder_available = rust_tunnel_mac_video::decoder_available(codec);
        println!("编码器可用: {encoder_available}  解码器可用: {decoder_available}");
        if !encoder_available || !decoder_available {
            println!("跳过往返测试");
            ok = false;
            continue;
        }
        if !roundtrip(codec) {
            ok = false;
        }
    }
    ok
}

/// 单个编码标准的编解码往返。
#[cfg(target_os = "macos")]
#[expect(
    clippy::too_many_lines,
    reason = "探针是线性脚本：创建 → 编解码循环 → 汇总，拆函数反而更难对照输出"
)]
fn roundtrip(codec: rust_tunnel_mac_video::CodecKind) -> bool {
    use rust_tunnel_mac_video::{color, EncodeConfig, VtDecoder, VtEncoder};
    use std::time::Instant;

    let config = EncodeConfig::new(codec, WIDTH, HEIGHT, 8000, 60);
    let mut encoder = match VtEncoder::new(config) {
        Ok(encoder) => encoder,
        Err(err) => {
            println!("编码器创建失败: {err}");
            return false;
        }
    };
    let info = encoder.info();
    println!(
        "编码器: backend={} name={} hardware={} {}x{} {}kbps {}fps",
        info.backend,
        info.name,
        info.hardware,
        info.width,
        info.height,
        info.bitrate_kbps,
        info.fps
    );
    let mut decoder = VtDecoder::new();
    if let Err(err) = decoder.configure(WIDTH, HEIGHT) {
        println!("解码器 configure 失败: {err}");
        return false;
    }
    let mut encode_ms = Vec::with_capacity(FRAMES as usize);
    let mut decode_ms = Vec::with_capacity(FRAMES as usize);
    let mut decoded_frames = 0u32;
    let mut keyframes = 0u32;
    // 真机排障用：`RT_PROBE_DUMP_PREFIX` 设置时，把实际写给控制端的帧原样落盘
    // （4 字节大端长度前缀 + Annex B；不改任何编码参数）。
    use std::io::Write as _;
    let dump_prefix = std::env::var("RT_PROBE_DUMP_PREFIX").ok();
    let extension = match codec {
        rust_tunnel_mac_video::CodecKind::H264 => "h264",
        rust_tunnel_mac_video::CodecKind::H265 => "h265",
    };
    let mut dump = dump_prefix.as_deref().and_then(|prefix| {
        let path = format!("{prefix}.{extension}");
        match std::fs::File::create(&path) {
            Ok(file) => {
                println!("码流 dump: {path}");
                Some(file)
            }
            Err(err) => {
                println!("无法创建码流 dump {path}: {err}");
                None
            }
        }
    });
    let mut first_keyframe_dumped = false;
    for index in 0..FRAMES {
        let frame = synthetic_frame(index);
        let started = Instant::now();
        let packet = match encoder.encode_bgra(&frame, WIDTH, HEIGHT, index == 0) {
            Ok(packet) => packet,
            Err(err) => {
                println!("第 {index} 帧编码失败: {err}");
                return false;
            }
        };
        encode_ms.push(started.elapsed().as_secs_f64() * 1000.0);
        if let Some(prefix) = dump_prefix.as_deref() {
            let mut dump_failed = false;
            if let Some(file) = dump.as_mut() {
                if let Err(err) = file.write_all(&packet.data) {
                    println!("写入码流 dump 失败: {err}");
                    dump_failed = true;
                }
            }
            if dump_failed {
                dump = None;
            }
            // 首个关键帧单独一份，便于直接喂给解码器/工具单独验证参数集与 IDR。
            if packet.keyframe && !first_keyframe_dumped {
                first_keyframe_dumped = true;
                let path = format!("{prefix}.first-keyframe.{extension}");
                match std::fs::write(&path, &packet.data) {
                    Ok(()) => println!("首个关键帧 dump: {path}"),
                    Err(err) => println!("无法写入首个关键帧 {path}: {err}"),
                }
            }
        }
        if packet.keyframe {
            keyframes = keyframes.saturating_add(1);
        }
        let started = Instant::now();
        match decoder.decode(&packet.data) {
            Ok(Some(frame_out)) => {
                decode_ms.push(started.elapsed().as_secs_f64() * 1000.0);
                decoded_frames = decoded_frames.saturating_add(1);
                if frame_out.width != WIDTH || frame_out.height != HEIGHT {
                    println!(
                        "第 {index} 帧尺寸不符: {}x{}",
                        frame_out.width, frame_out.height
                    );
                    return false;
                }
                if index == 0 {
                    // 首帧做一次像素校验（NV12 → RGBA，与源亮度比较）。
                    match color::nv12_limited_to_rgba(
                        &frame_out.y,
                        &frame_out.uv,
                        frame_out.width,
                        frame_out.height,
                    ) {
                        Ok(rgba) => {
                            let mut sum = 0u64;
                            let mut count = 0u64;
                            for (source, decoded) in frame
                                .as_chunks::<4>()
                                .0
                                .iter()
                                .zip(rgba.as_chunks::<4>().0.iter())
                            {
                                let source_luma =
                                    u32::from(color::bgra_luma(source[0], source[1], source[2]));
                                let decoded_luma = u32::from(decoded[1]);
                                sum = sum
                                    .saturating_add(u64::from(source_luma.abs_diff(decoded_luma)));
                                count = count.saturating_add(1);
                            }
                            let mean = sum.checked_div(count).unwrap_or(0);
                            println!("首帧亮度平均绝对误差: {mean}/255");
                        }
                        Err(err) => println!("NV12 转 RGBA 失败: {err}"),
                    }
                }
            }
            Ok(None) => {}
            Err(err) => {
                println!("第 {index} 帧解码失败: {err}");
                return false;
            }
        }
    }
    println!(
        "编码 {} 帧（关键帧 {keyframes}）：{}",
        FRAMES,
        summarize(&encode_ms)
    );
    println!("解码 {} 帧：{}", decoded_frames, summarize(&decode_ms));
    let info = decoder.info();
    println!(
        "解码器: backend={} name={} hardware={}",
        info.backend, info.name, info.hardware
    );
    decoded_frames == FRAMES
}

/// 造一帧带纹理与运动块的 BGRA（纯色会被编码器退化，运动块保证 P 帧有内容）。
#[cfg(target_os = "macos")]
fn synthetic_frame(index: u32) -> Vec<u8> {
    let mut frame = vec![0u8; (WIDTH * HEIGHT) as usize * 4];
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let offset = ((y * WIDTH + x) as usize) * 4;
            frame[offset] = u8::try_from((x * 3) % 256).unwrap_or(0);
            frame[offset + 1] = u8::try_from((y * 5) % 256).unwrap_or(0);
            frame[offset + 2] = u8::try_from((x + y) % 256).unwrap_or(0);
            frame[offset + 3] = 255;
        }
    }
    // 一个逐帧移动的白色方块。
    let block = 160u32;
    let left = (index * 24) % (WIDTH - block);
    let top = HEIGHT / 2 - block / 2;
    for y in top..top + block {
        for x in left..left + block {
            let offset = ((y * WIDTH + x) as usize) * 4;
            frame[offset..offset + 4].copy_from_slice(&[255, 255, 255, 255]);
        }
    }
    frame
}

/// 打印 min / 中位 / max（毫秒）。
#[cfg(target_os = "macos")]
fn summarize(samples: &[f64]) -> String {
    if samples.is_empty() {
        return "无样本".to_owned();
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let min = sorted.first().copied().unwrap_or(0.0);
    let max = sorted.last().copied().unwrap_or(0.0);
    let median = sorted.get(sorted.len() / 2).copied().unwrap_or(0.0);
    let count = u32::try_from(sorted.len()).map_or(1.0, f64::from);
    let mean = sorted.iter().sum::<f64>() / count;
    format!("min {min:.2}ms / 中位 {median:.2}ms / 均值 {mean:.2}ms / max {max:.2}ms")
}
