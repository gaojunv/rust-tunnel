//! `win-video-decode-probe`：Media Foundation H.264 解码真机探针。
//!
//! 用 NVENC 现场编码 10 帧 1280x720 合成 BGRA（首帧 IDR，其余 P 帧），逐帧喂给
//! [`H264Decoder`]，打印每帧解码耗时、输出尺寸、RGBA 长度与像素校验。最后把
//! 一个 IDR + 两个 P 帧拼接成**一个缓冲区**再解码一次，验证长度前缀解析支持
//! 多包输入。

#[cfg(windows)]
use rust_tunnel_win_video::d3d11;
#[cfg(windows)]
use rust_tunnel_win_video::decode::{DecodedH264Frame, H264Decoder};
#[cfg(windows)]
use rust_tunnel_win_video::nvenc::{EncoderConfig, NvencApi, NvencEncoder};
#[cfg(windows)]
use rust_tunnel_win_video::WinVideoError;
#[cfg(windows)]
use std::time::{Duration, Instant};

/// 测试画面宽度。
#[cfg(windows)]
const WIDTH: u32 = 1280;
/// 测试画面高度。
#[cfg(windows)]
const HEIGHT: u32 = 720;
/// 编码帧率。
#[cfg(windows)]
const FPS: u32 = 60;
/// 编码目标码率。
#[cfg(windows)]
const BITRATE_KBPS: u32 = 4000;
/// 逐帧解码的帧数（1 个 IDR + 9 个 P 帧）。
#[cfg(windows)]
const FRAME_COUNT: u32 = 10;

#[cfg(windows)]
fn main() {
    if let Err(err) = run() {
        eprintln!("解码探针失败: {err}");
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("win-video-decode-probe 只能在 Windows 上运行");
}

/// 构造一帧自顶向下 BGRA：渐变背景 + 随帧号移动的白色方块。
///
/// 与 `win-video-probe` 的合成帧一致：移动方块让 P 帧有真实的帧间差异。
#[cfg(windows)]
fn synthetic_bgra(width: u32, height: u32, frame: u32) -> Vec<u8> {
    let w = usize::try_from(width).unwrap_or(0);
    let h = usize::try_from(height).unwrap_or(0);
    let mut buf = vec![0u8; w.saturating_mul(h).saturating_mul(4)];
    for y in 0..h {
        for x in 0..w {
            let index = y.saturating_mul(w).saturating_add(x).saturating_mul(4);
            let Some(pixel) = buf.get_mut(index..index.saturating_add(4)) else {
                continue;
            };
            // BGRA：蓝=列渐变，绿=行渐变，红=对角线渐变。
            pixel[0] = u8::try_from(x & 0xFF).unwrap_or(0);
            pixel[1] = u8::try_from(y & 0xFF).unwrap_or(0);
            pixel[2] = u8::try_from((x + y) & 0xFF).unwrap_or(0);
            pixel[3] = 255;
        }
    }

    // 120x120 的白色方块，随帧号移动。
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

/// 一帧的像素校验摘要。
#[cfg(windows)]
struct FrameCheck {
    /// 最暗像素的亮度（R+G+B 的均值）。
    min_brightness: u32,
    /// 最亮像素的亮度。
    max_brightness: u32,
    /// 全帧平均亮度。
    mean_brightness: f64,
    /// 中心像素 RGBA。
    center: [u8; 4],
}

/// 采样整帧（逐像素）计算亮度范围与中心像素。
#[cfg(windows)]
fn check_frame(frame: &DecodedH264Frame) -> FrameCheck {
    let width = usize::try_from(frame.width).unwrap_or(0);
    let height = usize::try_from(frame.height).unwrap_or(0);
    let mut min_brightness = u32::MAX;
    let mut max_brightness = 0u32;
    let mut total = 0u64;
    let mut count = 0u64;
    // `as_chunks::<4>` 保证每块恰好 4 字节，索引不会越界。
    for pixel in frame.rgba.as_chunks::<4>().0 {
        let (r, g, b) = (pixel[0], pixel[1], pixel[2]);
        let brightness = (u32::from(r) + u32::from(g) + u32::from(b)) / 3;
        min_brightness = min_brightness.min(brightness);
        max_brightness = max_brightness.max(brightness);
        total = total.saturating_add(u64::from(brightness));
        count = count.saturating_add(1);
    }
    let mean_brightness = total.checked_div(count).map_or(0.0, |average| {
        f64::from(u32::try_from(average).unwrap_or(0))
    });
    let center_index = width
        .saturating_div(2)
        .saturating_add(height.saturating_div(2).saturating_mul(width))
        .saturating_mul(4);
    let center = frame
        .rgba
        .get(center_index..center_index.saturating_add(4))
        .and_then(|slice| <[u8; 4]>::try_from(slice).ok())
        .unwrap_or([0, 0, 0, 0]);
    FrameCheck {
        min_brightness,
        max_brightness,
        mean_brightness,
        center,
    }
}

/// 耗时中位数（毫秒）。
#[cfg(windows)]
fn median_ms(times: &[Duration]) -> f64 {
    let mut values: Vec<f64> = times.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
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
    let total: f64 = times.iter().map(|d| d.as_secs_f64() * 1000.0).sum();
    total / f64::from(u32::try_from(times.len()).unwrap_or(1))
}

/// 跑一次逐帧解码并打印结果。
#[cfg(windows)]
fn run_frame_probe(
    encoder: &mut NvencEncoder<'_>,
    decoder: &mut H264Decoder,
) -> Result<(), WinVideoError> {
    let mut times: Vec<Duration> = Vec::with_capacity(usize::try_from(FRAME_COUNT).unwrap_or(0));
    let mut first_dims: Option<(u32, u32)> = None;
    let mut produced = 0u32;

    println!("\n== 逐帧解码（{WIDTH}x{HEIGHT}，{FRAME_COUNT} 帧）==");
    for index in 0..FRAME_COUNT {
        let bgra = synthetic_bgra(WIDTH, HEIGHT, index);
        encoder.upload_bgra(&bgra)?;
        let packet = encoder.encode(index == 0)?;

        let started = Instant::now();
        let output_frame = decoder.decode(&packet.data)?;
        let elapsed = started.elapsed();
        times.push(elapsed);

        match output_frame {
            Some(frame) => {
                produced = produced.saturating_add(1);
                first_dims.get_or_insert((frame.width, frame.height));
                let check = check_frame(&frame);
                println!(
                    "  帧 {index:>2}: 码流 {:>7} 字节, 解码 {:6.2} ms, 输出 {}x{} RGBA {} 字节, \
                     亮度 [{}, {}] 均值 {:.1}, 中心像素 RGBA={:?}",
                    packet.payload_len(),
                    elapsed.as_secs_f64() * 1000.0,
                    frame.width,
                    frame.height,
                    frame.rgba.len(),
                    check.min_brightness,
                    check.max_brightness,
                    check.mean_brightness,
                    check.center
                );
            }
            None => println!(
                "  帧 {index:>2}: 码流 {:>7} 字节, 解码 {:6.2} ms, 无输出（解码器缓冲中）",
                packet.payload_len(),
                elapsed.as_secs_f64() * 1000.0
            ),
        }
    }

    println!(
        "  汇总: 输出 {produced}/{FRAME_COUNT} 帧，解码中位数 {:.2} ms，均值 {:.2} ms",
        median_ms(&times),
        mean_ms(&times)
    );

    match first_dims {
        Some((width, height)) if (width, height) == (WIDTH, HEIGHT) => {
            println!("  首帧尺寸正确: {width}x{height}");
            Ok(())
        }
        Some((width, height)) => Err(WinVideoError::Decode(format!(
            "首帧尺寸不是 {WIDTH}x{HEIGHT}，实际 {width}x{height}"
        ))),
        None => Err(WinVideoError::Decode("整个探针没有解出任何一帧".to_owned())),
    }
}

/// 把多个包拼成一个缓冲区喂进去，验证长度前缀解析。
#[cfg(windows)]
fn run_concatenated_probe(
    encoder: &mut NvencEncoder<'_>,
    decoder: &mut H264Decoder,
) -> Result<(), WinVideoError> {
    let mut combined = Vec::new();
    for index in 0..3u32 {
        let bgra = synthetic_bgra(WIDTH, HEIGHT, FRAME_COUNT.saturating_add(index));
        encoder.upload_bgra(&bgra)?;
        let packet = encoder.encode(index == 0)?;
        combined.extend_from_slice(&packet.data);
    }
    let started = Instant::now();
    let output_frame = decoder.decode(&combined)?;
    let elapsed = started.elapsed();
    println!(
        "\n== 拼接输入（IDR + 2 P 帧，共 {} 字节）==\n  解码 {:6.2} ms, 输出={}",
        combined.len(),
        elapsed.as_secs_f64() * 1000.0,
        output_frame.is_some()
    );
    let frame =
        output_frame.ok_or_else(|| WinVideoError::Decode("拼接输入没有产生输出帧".to_owned()))?;
    let check = check_frame(&frame);
    println!(
        "  最后一帧 {}x{}，亮度 [{}, {}] 均值 {:.1}",
        frame.width,
        frame.height,
        check.min_brightness,
        check.max_brightness,
        check.mean_brightness
    );
    Ok(())
}

/// 验证 `ScreenConfig` 尺寸变化：重建解码器后能解新分辨率的 IDR。
#[cfg(windows)]
fn run_resize_probe(
    api: &NvencApi,
    d3d: &d3d11::D3d11,
    decoder: &mut H264Decoder,
) -> Result<(), WinVideoError> {
    const RESIZED_WIDTH: u32 = 640;
    const RESIZED_HEIGHT: u32 = 360;
    let config = EncoderConfig::new(RESIZED_WIDTH, RESIZED_HEIGHT, FPS, BITRATE_KBPS)?;
    let mut encoder = NvencEncoder::new(api, d3d, config)?;
    let bgra = synthetic_bgra(RESIZED_WIDTH, RESIZED_HEIGHT, 0);
    encoder.upload_bgra(&bgra)?;
    let packet = encoder.encode(true)?;

    decoder.reconfigure(RESIZED_WIDTH, RESIZED_HEIGHT)?;
    let (width, height, layout) = decoder.negotiated();
    let started = Instant::now();
    let output_frame = decoder.decode(&packet.data)?;
    let elapsed = started.elapsed();
    println!(
        "\n== 尺寸变化（reconfigure → {RESIZED_WIDTH}x{RESIZED_HEIGHT}）==\n  \
         重协商后 MFT 输出: {width}x{height} {}",
        layout.as_str()
    );
    let frame =
        output_frame.ok_or_else(|| WinVideoError::Decode("尺寸变化后的首帧没有输出".to_owned()))?;
    let check = check_frame(&frame);
    println!(
        "  解码 {:6.2} ms, 输出 {}x{}，亮度 [{}, {}] 均值 {:.1}",
        elapsed.as_secs_f64() * 1000.0,
        frame.width,
        frame.height,
        check.min_brightness,
        check.max_brightness,
        check.mean_brightness
    );
    if (frame.width, frame.height) != (RESIZED_WIDTH, RESIZED_HEIGHT) {
        return Err(WinVideoError::Decode(format!(
            "尺寸变化后输出不是 {RESIZED_WIDTH}x{RESIZED_HEIGHT}，实际 {}x{}",
            frame.width, frame.height
        )));
    }
    Ok(())
}

#[cfg(windows)]
fn run() -> Result<(), WinVideoError> {
    println!("== 初始化 ==");
    let api = NvencApi::load()?;
    let d3d = d3d11::create_nvidia()?;
    println!("  NVIDIA 设备: {}", d3d.adapter.name);

    let config = EncoderConfig::new(WIDTH, HEIGHT, FPS, BITRATE_KBPS)?;
    let mut encoder = NvencEncoder::new(&api, &d3d, config)?;
    let mut decoder = H264Decoder::new(WIDTH, HEIGHT)?;
    let (width, height, layout) = decoder.negotiated();
    println!(
        "  MFT 输出: {width}x{height} {}（请求 {}x{}）",
        layout.as_str(),
        decoder.requested_dimensions().0,
        decoder.requested_dimensions().1
    );

    run_frame_probe(&mut encoder, &mut decoder)?;
    run_concatenated_probe(&mut encoder, &mut decoder)?;
    // 释放 720p 编码会话，避免与新分辨率的会话同时占用驱动资源。
    drop(encoder);
    run_resize_probe(&api, &d3d, &mut decoder)?;
    Ok(())
}
