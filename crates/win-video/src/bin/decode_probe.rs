//! `win-video-decode-probe`：Media Foundation H.264 解码真机探针。
//!
//! 用 NVENC 现场编码合成帧，分别喂给 **系统内存软件解码器** 与 **DXVA/D3D11
//! 硬件解码器**，对照 1280x720 与 1920x1080 两种分辨率的解码耗时、输出尺寸与
//! 像素结果。硬件组优先选 Intel 核显（接近用户现场的 HD 630），并报告 MFT 是否
//! 真的输出了 `IMFDXGIBuffer`。
//!
//! 最后保留两项回归：多包拼接输入、`reconfigure` 尺寸变化。

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
#[cfg(windows)]
use windows::Win32::Foundation::FILETIME;
#[cfg(windows)]
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

/// 编码帧率。
#[cfg(windows)]
const FPS: u32 = 60;
/// 每组对照编码的帧数（1 个 IDR + 9 个 P 帧）。
#[cfg(windows)]
const FRAME_COUNT: u32 = 10;
/// 同一组码流重复解码的轮数。
///
/// Windows 的进程 CPU 时间按系统时钟节拍（约 15.6 ms）更新，单帧差值会被量化
/// 成 0 或一整拍；重复 5 轮把总量放大到几百毫秒，CPU 对照才有意义。
#[cfg(windows)]
const DECODE_REPEATS: u32 = 5;

/// 对照的分辨率：720p 与 1080p（1080p 会触发 1088 行编码高度 + 光圈裁剪）。
#[cfg(windows)]
const RESOLUTIONS: [(u32, u32); 2] = [(1280, 720), (1920, 1080)];

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

/// `FILETIME` → 100ns 计数。
#[cfg(windows)]
fn filetime_ticks(time: FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

/// 当前进程累计 CPU 时间（内核 + 用户，单位 100ns）。
///
/// 用**进程**而不是线程：软件解码 MFT 可能在自己的工作线程里解码，线程级
/// 计时会漏掉它们；这正好是我们要量化的「CPU 到底降了多少」。
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

/// 一次解码运行的统计。
#[cfg(windows)]
struct DecodeRun {
    /// 每帧 decode 调用的墙钟耗时。
    times: Vec<Duration>,
    /// 整组（`FRAME_COUNT * DECODE_REPEATS` 次 decode）消耗的进程 CPU 时间。
    cpu_total: Duration,
    /// 首帧输出尺寸。
    first_dims: Option<(u32, u32)>,
    /// 最后一帧中心像素。
    last_center: [u8; 4],
    /// 最后一帧平均亮度。
    last_mean_brightness: f64,
    /// 最后一帧 RGBA（用于软/硬像素对比）。
    last_rgba: Option<Vec<u8>>,
    /// 实际产出的帧数。
    produced: u32,
}

/// 顺序解码一组码流包并统计。
#[cfg(windows)]
fn decode_packets(
    decoder: &mut H264Decoder,
    packets: &[Vec<u8>],
) -> Result<DecodeRun, WinVideoError> {
    let capacity = packets
        .len()
        .saturating_mul(usize::try_from(DECODE_REPEATS).unwrap_or(1));
    let mut run = DecodeRun {
        times: Vec::with_capacity(capacity),
        cpu_total: Duration::ZERO,
        first_dims: None,
        last_center: [0; 4],
        last_mean_brightness: 0.0,
        last_rgba: None,
        produced: 0,
    };
    let cpu_before = process_cpu_ticks();
    for _ in 0..DECODE_REPEATS {
        for packet in packets {
            let started = Instant::now();
            let output_frame = decoder.decode(packet)?;
            run.times.push(started.elapsed());
            if let Some(frame) = output_frame {
                run.produced = run.produced.saturating_add(1);
                run.first_dims.get_or_insert((frame.width, frame.height));
                let check = check_frame(&frame);
                run.last_center = check.center;
                run.last_mean_brightness = check.mean_brightness;
                run.last_rgba = Some(frame.rgba);
            }
        }
    }
    let cpu_after = process_cpu_ticks();
    run.cpu_total = Duration::from_nanos(cpu_after.saturating_sub(cpu_before).saturating_mul(100));
    Ok(run)
}

/// 两帧 RGBA 的平均绝对差（0–255，仅取前 `min(len)` 字节）。
#[cfg(windows)]
fn mean_abs_diff(left: &[u8], right: &[u8]) -> Option<f64> {
    if left.is_empty() || left.len() != right.len() {
        return None;
    }
    let total: f64 = left
        .iter()
        .zip(right)
        .map(|(&a, &b)| f64::from(a.abs_diff(b)))
        .sum();
    let len = u32::try_from(left.len()).ok()?;
    Some(total / f64::from(len))
}

/// 打印一组解码结果并校验尺寸。
#[cfg(windows)]
fn print_run(
    label: &str,
    run: &DecodeRun,
    decoder: &H264Decoder,
    expected: (u32, u32),
) -> Result<(), WinVideoError> {
    let dims = run
        .first_dims
        .ok_or_else(|| WinVideoError::Decode(format!("{label} 没有解出任何一帧")))?;
    if dims != expected {
        return Err(WinVideoError::Decode(format!(
            "{label} 首帧尺寸不是 {}x{}，实际 {}x{}",
            expected.0, expected.1, dims.0, dims.1
        )));
    }
    let total_frames = FRAME_COUNT.saturating_mul(DECODE_REPEATS);
    let per_frame: Vec<String> = run
        .times
        .iter()
        .take(12)
        .map(|time| format!("{:.1}", time.as_secs_f64() * 1000.0))
        .collect();
    let cpu_ms = run.cpu_total.as_secs_f64() * 1000.0;
    let cpu_per_frame = cpu_ms / f64::from(total_frames);
    println!(
        "  {label}: 路径={}, 适配器={}, 输出 {}/{} 帧, 尺寸 {}x{}, \
         墙钟中位 {:.2} ms / 均值 {:.2} ms, CPU 总计 {:.1} ms（每帧 {:.2} ms）, \
         DXGI 样本={}, 中心 RGBA={:?}, 亮度均值 {:.1}",
        decoder.path().as_str(),
        decoder.adapter_name().unwrap_or("-"),
        run.produced,
        total_frames,
        dims.0,
        dims.1,
        median_ms(&run.times),
        mean_ms(&run.times),
        cpu_ms,
        cpu_per_frame,
        decoder.hardware_buffers_seen(),
        run.last_center,
        run.last_mean_brightness,
    );
    println!(
        "    前 {} 帧耗时(ms): [{}]",
        per_frame.len(),
        per_frame.join(", ")
    );
    Ok(())
}

/// 某分辨率的软件 vs 硬件对照。
#[cfg(windows)]
fn run_comparison(
    api: &NvencApi,
    encode_d3d: &d3d11::D3d11,
    width: u32,
    height: u32,
) -> Result<(), WinVideoError> {
    let bitrate_kbps = if width >= 1920 { 8000 } else { 4000 };
    println!("\n== 对照：{width}x{height} ==");

    let config = EncoderConfig::new(width, height, FPS, bitrate_kbps)?;
    let mut encoder = NvencEncoder::new(api, encode_d3d, config)?;
    let mut packets: Vec<Vec<u8>> = Vec::with_capacity(usize::try_from(FRAME_COUNT).unwrap_or(0));
    for index in 0..FRAME_COUNT {
        let bgra = synthetic_bgra(width, height, index);
        encoder.upload_bgra(&bgra)?;
        packets.push(encoder.encode(index == 0)?.data);
    }
    drop(encoder);

    let mut software = H264Decoder::new_software(width, height)?;
    let soft_run = decode_packets(&mut software, &packets)?;
    print_run("软件", &soft_run, &software, (width, height))?;

    match H264Decoder::new_hardware(width, height) {
        Ok(mut hardware) => {
            let hard_run = decode_packets(&mut hardware, &packets)?;
            print_run("硬件", &hard_run, &hardware, (width, height))?;
            if let (Some(left), Some(right)) = (&soft_run.last_rgba, &hard_run.last_rgba) {
                if let Some(diff) = mean_abs_diff(left, right) {
                    println!("  最后帧软/硬像素平均绝对差: {diff:.3}/255");
                }
            }
            let soft_median = median_ms(&soft_run.times);
            let hard_median = median_ms(&hard_run.times);
            if hard_median > 0.0 {
                println!("  墙钟中位加速比: {:.2}x", soft_median / hard_median);
            }
            let soft_cpu = soft_run.cpu_total.as_secs_f64();
            let hard_cpu = hard_run.cpu_total.as_secs_f64();
            if hard_cpu > 0.0 {
                println!("  CPU 时间加速比: {:.2}x", soft_cpu / hard_cpu);
            }
            if !hardware.hardware_buffers_seen() {
                println!(
                    "  ⚠ 配置了 DXVA 但未收到 IMFDXGIBuffer：硬件解码没有真正生效（MFT 仍在 CPU 解码）"
                );
            }
        }
        Err(err) => {
            println!("  硬件: 初始化失败，本机无法做 DXVA 对照: {err}");
        }
    }
    Ok(())
}

/// 多包拼接输入回归（与路径无关）。
#[cfg(windows)]
fn run_concatenated_probe(api: &NvencApi, encode_d3d: &d3d11::D3d11) -> Result<(), WinVideoError> {
    const WIDTH: u32 = 1280;
    const HEIGHT: u32 = 720;
    let config = EncoderConfig::new(WIDTH, HEIGHT, FPS, 4000)?;
    let mut encoder = NvencEncoder::new(api, encode_d3d, config)?;
    let mut combined = Vec::new();
    for index in 0..3u32 {
        let bgra = synthetic_bgra(WIDTH, HEIGHT, index);
        encoder.upload_bgra(&bgra)?;
        let packet = encoder.encode(index == 0)?;
        combined.extend_from_slice(&packet.data);
    }
    drop(encoder);

    let mut decoder = H264Decoder::new(WIDTH, HEIGHT)?;
    let started = Instant::now();
    let output_frame = decoder.decode(&combined)?;
    let elapsed = started.elapsed();
    println!(
        "\n== 拼接输入（IDR + 2 P 帧，共 {} 字节，路径={}）==",
        combined.len(),
        decoder.path().as_str()
    );
    let frame =
        output_frame.ok_or_else(|| WinVideoError::Decode("拼接输入没有产生输出帧".to_owned()))?;
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
    Ok(())
}

/// `reconfigure` 尺寸变化回归（与路径无关）。
#[cfg(windows)]
fn run_resize_probe(api: &NvencApi, encode_d3d: &d3d11::D3d11) -> Result<(), WinVideoError> {
    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 360;
    let config = EncoderConfig::new(WIDTH, HEIGHT, FPS, 2000)?;
    let mut encoder = NvencEncoder::new(api, encode_d3d, config)?;
    let bgra = synthetic_bgra(WIDTH, HEIGHT, 0);
    encoder.upload_bgra(&bgra)?;
    let packet = encoder.encode(true)?;
    drop(encoder);

    let mut decoder = H264Decoder::new(1280, 720)?;
    let before = decoder.path();
    decoder.reconfigure(WIDTH, HEIGHT)?;
    let started = Instant::now();
    let output_frame = decoder.decode(&packet.data)?;
    let elapsed = started.elapsed();
    println!(
        "\n== 尺寸变化（reconfigure 1280x720 → {WIDTH}x{HEIGHT}，路径={}→{}）==",
        before.as_str(),
        decoder.path().as_str()
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
    if (frame.width, frame.height) != (WIDTH, HEIGHT) {
        return Err(WinVideoError::Decode(format!(
            "尺寸变化后输出不是 {WIDTH}x{HEIGHT}，实际 {}x{}",
            frame.width, frame.height
        )));
    }
    Ok(())
}

#[cfg(windows)]
fn run() -> Result<(), WinVideoError> {
    println!("== 适配器 ==");
    for adapter in d3d11::adapters()? {
        println!(
            "  [{}] {} vendor=0x{:04X} software={} vram={} MiB",
            adapter.index,
            adapter.name,
            adapter.vendor_id,
            adapter.software,
            adapter.dedicated_video_memory / (1024 * 1024)
        );
    }

    let api = NvencApi::load()?;
    let encode_d3d = d3d11::create_nvidia()?;
    println!("\n  NVENC 编码设备: {}", encode_d3d.adapter.name);

    for (width, height) in RESOLUTIONS {
        run_comparison(&api, &encode_d3d, width, height)?;
    }

    run_concatenated_probe(&api, &encode_d3d)?;
    run_resize_probe(&api, &encode_d3d)?;
    Ok(())
}
