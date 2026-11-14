//! `mft-gpu-probe`：GPU 纹理直通 Intel QSV 的真机探针（阶段 2a）。
//!
//! 验证 [`CapturedTexture`] + [`MftH264Encoder::encode_texture`] 这条链路：
//!
//! 1. 枚举适配器并选 Intel（其次第一块非软件适配器），在它上面建 D3D11 设备；
//! 2. 用同一块设备创建 1920x1080 BGRA 测试图案纹理（`RENDER_TARGET |
//!    SHADER_RESOURCE`，与采集池纹理同构），每帧 `UpdateSubresource` 上传；
//! 3. `MftH264Encoder::new_with_device` 绑定同一设备，循环 60 次
//!    `encode_texture`，打印每帧耗时、码流字节与关键帧标志；
//! 4. 把首帧 IDR 用现有 `H264Decoder` 解出来，与源 RGBA 比 MAD——颜色空间
//!    （BT.709 limited）与通道顺序错了会立刻反映为很大的 MAD。
//!
//! 不依赖 libvpx / remote，可在 MSVC 下直接构建运行。

#[cfg(windows)]
use rust_tunnel_win_video::capture::CapturedTexture;
#[cfg(windows)]
use rust_tunnel_win_video::d3d11::{self, AdapterInfo, VENDOR_INTEL};
#[cfg(windows)]
use rust_tunnel_win_video::decode::H264Decoder;
#[cfg(windows)]
use rust_tunnel_win_video::mft_encoder::MftH264Encoder;
#[cfg(windows)]
use rust_tunnel_win_video::WinVideoError;
#[cfg(windows)]
use std::sync::Arc;
#[cfg(windows)]
use std::time::{Duration, Instant};
#[cfg(windows)]
use windows::Win32::Foundation::FILETIME;
#[cfg(windows)]
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
#[cfg(windows)]
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
#[cfg(windows)]
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

/// 编码尺寸。
#[cfg(windows)]
const WIDTH: u32 = 1920;
/// 编码尺寸。
#[cfg(windows)]
const HEIGHT: u32 = 1080;
/// 编码帧率。
#[cfg(windows)]
const FPS: u32 = 30;
/// 编码码率（kbps）。
#[cfg(windows)]
const BITRATE_KBPS: u32 = 8000;
/// 编码帧数。
#[cfg(windows)]
const FRAME_COUNT: u32 = 60;

#[cfg(windows)]
fn main() {
    if let Err(err) = run() {
        eprintln!("GPU 编码探针失败: {err}");
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("mft-gpu-probe 只能在 Windows 上运行");
}

/// 选 Intel 适配器（其次第一块非软件适配器）。
#[cfg(windows)]
fn pick_adapter() -> Result<AdapterInfo, WinVideoError> {
    let all = d3d11::adapters()?;
    println!("== DXGI 适配器（{} 块）==", all.len());
    for adapter in &all {
        println!(
            "  [{}] {}{} vendor=0x{:04X} device=0x{:04X} vram={} MiB luid=0x{:X}",
            adapter.index,
            adapter.name,
            if adapter.software { "（软件）" } else { "" },
            adapter.vendor_id,
            adapter.device_id,
            adapter.dedicated_video_memory / (1024 * 1024),
            adapter.luid,
        );
    }
    all.iter()
        .find(|adapter| !adapter.software && adapter.vendor_id == VENDOR_INTEL)
        .or_else(|| all.iter().find(|adapter| !adapter.software))
        .cloned()
        .ok_or_else(|| WinVideoError::Config("没有可用的硬件适配器".to_owned()))
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

/// 创建一张与采集池纹理同构的 BGRA 纹理。
#[cfg(windows)]
fn create_test_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<ID3D11Texture2D, WinVideoError> {
    let bind = D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0;
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: u32::try_from(bind).unwrap_or(0),
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture: Option<ID3D11Texture2D> = None;
    // SAFETY: `desc` 合法；输出指针指向本栈帧。
    unsafe { device.CreateTexture2D(&raw const desc, None, Some(&raw mut texture)) }
        .map_err(|err| WinVideoError::Encode(format!("创建探针纹理失败: {err}")))?;
    texture.ok_or_else(|| WinVideoError::Encode("CreateTexture2D 返回空纹理".to_owned()))
}

/// 把 RGBA 源图按 BGRA 顺序上传到纹理。
#[cfg(windows)]
fn upload_rgba(
    context: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
    rgba: &[u8],
    width: u32,
) -> Result<(), WinVideoError> {
    let mut bgra = Vec::with_capacity(rgba.len());
    for pixel in rgba.as_chunks::<4>().0 {
        bgra.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
    }
    let row_pitch = width
        .checked_mul(4)
        .ok_or_else(|| WinVideoError::Config("行字节数溢出".to_owned()))?;
    // SAFETY: `texture` 是 DEFAULT 用途纹理；`bgra` 在调用期间有效且按行紧凑排列，
    // 源行距 = 宽 × 4。
    unsafe {
        context.UpdateSubresource(texture, 0, None, bgra.as_ptr().cast(), row_pitch, 0);
    }
    Ok(())
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

/// 均值（毫秒）。
#[cfg(windows)]
fn mean_ms(times: &[Duration]) -> f64 {
    if times.is_empty() {
        return 0.0;
    }
    let total: f64 = times.iter().map(|time| time.as_secs_f64() * 1000.0).sum();
    total / f64::from(u32::try_from(times.len()).unwrap_or(1))
}

/// 中位数（毫秒）。
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

/// 一轮编码的统计与解码回环数据。
#[cfg(windows)]
struct EncodeRun {
    times: Vec<Duration>,
    /// 整轮里 `encode_texture` 本身的进程 CPU 时间（100ns）。
    cpu_ticks: u64,
    produced: u32,
    /// 按输出顺序收集的非空码流（用于顺序解码回环）。
    packets: Vec<Vec<u8>>,
    /// 首帧源 RGBA（与 `packets[0]` 对应）。
    first_source: Option<Vec<u8>>,
    /// 中段源 RGBA（与 `packets[len/2]` 对应）。
    mid_source: Option<Vec<u8>>,
}

/// 循环上传测试图案并调用 `encode_texture`。
#[cfg(windows)]
fn encode_frames(
    encoder: &mut MftH264Encoder,
    device: &Arc<d3d11::D3d11>,
    texture: &ID3D11Texture2D,
) -> Result<EncodeRun, WinVideoError> {
    let mut run = EncodeRun {
        times: Vec::with_capacity(usize::try_from(FRAME_COUNT).unwrap_or(0)),
        cpu_ticks: 0,
        produced: 0,
        packets: Vec::with_capacity(usize::try_from(FRAME_COUNT).unwrap_or(0)),
        first_source: None,
        mid_source: None,
    };
    for index in 0..FRAME_COUNT {
        let rgba = synthetic_rgba(WIDTH, HEIGHT, index);
        upload_rgba(&device.context, texture, &rgba, WIDTH)?;
        let frame =
            CapturedTexture::from_texture(device.clone(), texture.clone(), 0, WIDTH, HEIGHT);
        let cpu_start = process_cpu_ticks();
        let started = Instant::now();
        let packet = encoder.encode_texture(&frame, index == 0)?;
        let elapsed = started.elapsed();
        run.cpu_ticks = run
            .cpu_ticks
            .saturating_add(process_cpu_ticks().saturating_sub(cpu_start));
        run.times.push(elapsed);
        if packet.data.is_empty() {
            println!(
                "  帧 {index:>2}: {:6.2} ms, 无输出（编码器流水线缓冲）",
                elapsed.as_secs_f64() * 1000.0
            );
            continue;
        }
        let bytes = packet.data.len().saturating_sub(4);
        run.produced = run.produced.saturating_add(1);
        if run.packets.is_empty() {
            run.first_source = Some(rgba.clone());
            println!(
                "  首帧 VideoProcessor + QSV 成功（GPU 转换 {:?} ms, MFT {:?} ms）",
                encoder.last_convert_ms(),
                encoder.last_mft_ms()
            );
        }
        if index == FRAME_COUNT / 2 {
            run.mid_source = Some(rgba);
        }
        run.packets.push(packet.data);
        println!(
            "  帧 {index:>2}: {:6.2} ms, {bytes:>7} 字节, keyframe={}",
            elapsed.as_secs_f64() * 1000.0,
            packet.keyframe
        );
    }
    Ok(run)
}

/// 顺序解码整轮码流，用首帧与中段帧与源 RGBA 比 MAD。
///
/// 中段帧是 P 帧，不能单独解码，所以按顺序喂给同一个解码器——顺带验证了
/// 每帧的 `UpdateSubresource` 确实进入了编码器（若 GPU 纹理被复用覆盖，中段
/// 帧的 MAD 会明显变大）。
#[cfg(windows)]
fn verify_loopback(run: &EncodeRun) -> Result<(), WinVideoError> {
    let (Some(first_source), Some(mid_source)) =
        (run.first_source.as_ref(), run.mid_source.as_ref())
    else {
        return Err(WinVideoError::Encode(
            "缺少首帧/中段帧的源数据，无法回环".to_owned(),
        ));
    };
    let mid_index = run.packets.len() / 2;
    let mut decoder = H264Decoder::new(WIDTH, HEIGHT)?;
    println!(
        "  解码路径: {}（顺序解码 {} 个包）",
        decoder.path().as_str(),
        run.packets.len()
    );
    let mut outputs = Vec::with_capacity(run.packets.len());
    for packet in &run.packets {
        if let Some(frame) = decoder.decode(packet)? {
            outputs.push(frame);
        }
    }

    let checks = [
        (0usize, first_source, "首帧"),
        (mid_index, mid_source, "中段帧"),
    ];
    for (index, source, label) in checks {
        let frame = outputs.get(index).ok_or_else(|| {
            WinVideoError::Encode(format!("{label}（输出 #{index}）没有解码输出"))
        })?;
        if (frame.width, frame.height) != (WIDTH, HEIGHT) {
            return Err(WinVideoError::Encode(format!(
                "{label}解码尺寸 {}x{} 与请求 {WIDTH}x{HEIGHT} 不一致",
                frame.width, frame.height
            )));
        }
        let mad = mean_abs_diff_rgba(source, &frame.rgba).ok_or_else(|| {
            WinVideoError::Encode(format!("{label}源 RGBA 与解码 RGBA 长度不匹配"))
        })?;
        println!(
            "  {label}回环（输出 #{index}）: 解码 {}x{}, MAD {mad:.3}（正确时应 < 3）",
            frame.width, frame.height
        );
        if mad > 8.0 {
            return Err(WinVideoError::Encode(format!(
                "{label} MAD {mad:.3} 过大：检查 VideoProcessor 颜色空间或输出纹理轮转"
            )));
        }
    }
    Ok(())
}

/// 4K 输入纹理的尺寸（本机采集实际尺寸）。
#[cfg(windows)]
const SCALE_INPUT_WIDTH: u32 = 3840;
/// 4K 输入纹理的尺寸。
#[cfg(windows)]
const SCALE_INPUT_HEIGHT: u32 = 2160;
/// 缩放路径的帧数。
#[cfg(windows)]
const SCALE_FRAMES: u32 = 12;

/// 4K RGBA 按 2×2 盒式下采样到 1080p（与 VideoProcessor 的缩放做近似对照）。
#[cfg(windows)]
fn downsample_rgba_2x2(rgba: &[u8], width: u32) -> Vec<u8> {
    let source_width = usize::try_from(width).unwrap_or(0);
    if source_width == 0 {
        return Vec::new();
    }
    let source_height = rgba.len() / source_width / 4;
    let out_width = source_width / 2;
    let out_height = source_height / 2;
    let mut out = vec![0u8; out_width.saturating_mul(out_height).saturating_mul(4)];
    for y in 0..out_height {
        for x in 0..out_width {
            let mut sums = [0u32; 3];
            for dy in 0..2 {
                for dx in 0..2 {
                    let source = ((y * 2 + dy) * source_width + (x * 2 + dx)) * 4;
                    for (sum, channel) in sums.iter_mut().zip(&rgba[source..source + 3]) {
                        *sum = sum.saturating_add(u32::from(*channel));
                    }
                }
            }
            let destination = (y * out_width + x) * 4;
            for (sum, target) in sums.iter().zip(&mut out[destination..destination + 3]) {
                *target = u8::try_from(sum.saturating_add(2) / 4).unwrap_or(0);
            }
            out[destination + 3] = 255;
        }
    }
    out
}

/// 验证真实采集尺寸（4K）经 VideoProcessor 缩放到编码尺寸（1080p）。
///
/// 解码回环拿到的画面与源图 2×2 盒式下采样比较；缩放算法不同（VideoProcessor
/// 通常用双线性），MAD 会比 1:1 路径略大，但颜色错误仍会明显超出阈值。
#[cfg(windows)]
fn run_scaling_pass(device: &Arc<d3d11::D3d11>) -> Result<(), WinVideoError> {
    println!(
        "\n== GPU 纹理缩放：{SCALE_INPUT_WIDTH}x{SCALE_INPUT_HEIGHT} → {WIDTH}x{HEIGHT}，{SCALE_FRAMES} 帧 =="
    );
    let texture = create_test_texture(&device.device, SCALE_INPUT_WIDTH, SCALE_INPUT_HEIGHT)?;
    let mut encoder =
        MftH264Encoder::new_with_device(device.clone(), WIDTH, HEIGHT, FPS, BITRATE_KBPS)?;
    let mut packets = Vec::new();
    let mut first_source = None;
    let mut times = Vec::new();
    for index in 0..SCALE_FRAMES {
        let rgba = synthetic_rgba(SCALE_INPUT_WIDTH, SCALE_INPUT_HEIGHT, index);
        upload_rgba(&device.context, &texture, &rgba, SCALE_INPUT_WIDTH)?;
        let frame = CapturedTexture::from_texture(
            device.clone(),
            texture.clone(),
            0,
            SCALE_INPUT_WIDTH,
            SCALE_INPUT_HEIGHT,
        );
        let started = Instant::now();
        let packet = encoder.encode_texture(&frame, index == 0)?;
        let elapsed = started.elapsed();
        times.push(elapsed);
        if packet.data.is_empty() {
            println!(
                "  帧 {index:>2}: {:6.2} ms, 无输出（编码器流水线缓冲）",
                elapsed.as_secs_f64() * 1000.0
            );
            continue;
        }
        let bytes = packet.data.len().saturating_sub(4);
        if packets.is_empty() {
            first_source = Some(rgba);
        }
        println!(
            "  帧 {index:>2}: {:6.2} ms, {bytes:>7} 字节, keyframe={}",
            elapsed.as_secs_f64() * 1000.0,
            packet.keyframe
        );
        packets.push(packet.data);
    }
    println!(
        "  缩放路径墙钟中位 {:.2} ms / 均值 {:.2} ms",
        median_ms(&times),
        mean_ms(&times),
    );

    let source =
        first_source.ok_or_else(|| WinVideoError::Encode("缩放路径没有任何编码输出".to_owned()))?;
    let expected = downsample_rgba_2x2(&source, SCALE_INPUT_WIDTH);
    let mut decoder = H264Decoder::new(WIDTH, HEIGHT)?;
    let mut outputs = Vec::with_capacity(packets.len());
    for packet in &packets {
        if let Some(frame) = decoder.decode(packet)? {
            outputs.push(frame);
        }
    }
    let frame = outputs
        .first()
        .ok_or_else(|| WinVideoError::Encode("缩放路径没有解码输出".to_owned()))?;
    if (frame.width, frame.height) != (WIDTH, HEIGHT) {
        return Err(WinVideoError::Encode(format!(
            "缩放路径解码尺寸 {}x{} 与编码尺寸 {WIDTH}x{HEIGHT} 不一致",
            frame.width, frame.height
        )));
    }
    let mad = mean_abs_diff_rgba(&expected, &frame.rgba)
        .ok_or_else(|| WinVideoError::Encode("缩放源图与解码图长度不匹配".to_owned()))?;
    println!(
        "  缩放下采样回环: 解码 {}x{}, MAD {mad:.3}（双线性 vs 盒式平均，< 8 视为正常）",
        frame.width, frame.height
    );
    if mad > 12.0 {
        return Err(WinVideoError::Encode(format!(
            "缩放路径 MAD {mad:.3} 过大：检查 VideoProcessor 缩放或颜色空间"
        )));
    }
    Ok(())
}

/// 探针主体。
#[cfg(windows)]
fn run() -> Result<(), WinVideoError> {
    let adapter = pick_adapter()?;
    println!("\n== 使用适配器 [{}] {} ==", adapter.index, adapter.name);
    let device = Arc::new(d3d11::create(Some(adapter.index))?);
    let texture = create_test_texture(&device.device, WIDTH, HEIGHT)?;
    println!("  测试纹理已创建：{WIDTH}x{HEIGHT} BGRA8（RENDER_TARGET | SHADER_RESOURCE）");

    let mut encoder =
        MftH264Encoder::new_with_device(device.clone(), WIDTH, HEIGHT, FPS, BITRATE_KBPS)?;
    println!(
        "  MFT: {} | CLSID: {}\n  编码器设备: {} | 尺寸 {:?} @ {} fps, {} kbps",
        encoder.mft_name(),
        encoder.mft_clsid(),
        encoder.adapter_name(),
        encoder.dimensions(),
        encoder.fps(),
        encoder.bitrate_kbps()
    );
    println!(
        "\n== GPU 纹理编码：{WIDTH}x{HEIGHT} @ {FPS} fps, CBR {BITRATE_KBPS} kbps, {FRAME_COUNT} 帧 =="
    );

    let run = encode_frames(&mut encoder, &device, &texture)?;
    let cpu_ms =
        f64::from(u32::try_from(run.cpu_ticks.saturating_mul(100) / 1_000_000).unwrap_or(u32::MAX));
    println!(
        "\n  汇总: 输出 {}/{FRAME_COUNT} 帧, 墙钟中位 {:.2} ms / 均值 {:.2} ms, encode_texture CPU 合计 {:.1} ms（每帧 {:.2} ms）",
        run.produced,
        median_ms(&run.times),
        mean_ms(&run.times),
        cpu_ms,
        cpu_ms / f64::from(FRAME_COUNT),
    );

    verify_loopback(&run)?;
    drop(encoder);
    drop(texture);
    run_scaling_pass(&device)
}
