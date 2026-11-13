//! 真机探针：打印适配器、NVENC 版本与 H.264 能力，并跑编码回环。
//!
//! 这是把 FFI 层与驱动对齐的最短回路：先确认 `nvEncodeAPI64.dll` 能加载、
//! 会话能开在独显上、能力值与预期一致，再验证 BGRA/RGBA 输入、在线改码率与
//! （可选的）NVENC 输入缩放。

use crate::d3d11;
use crate::nvenc::{self, EncoderConfig, InputFormat, NvencApi, NvencEncoder};
use crate::WinVideoError;
use std::time::Instant;

/// 构造一帧合成的自顶向下像素：渐变背景 + 随帧号移动的方块。
///
/// 移动方块让 P 帧有真实的帧间差异（全静态画面测不出运动补偿的代价与码率）。
#[expect(
    clippy::cast_possible_truncation,
    reason = "探针里只取低 8 位做测试图案，截断是刻意的"
)]
fn synthetic_pixels(width: u32, height: u32, frame: u32, format: InputFormat) -> Vec<u8> {
    let width_px = usize::try_from(width).unwrap_or(0);
    let height_px = usize::try_from(height).unwrap_or(0);
    let mut buf = vec![0u8; width_px.saturating_mul(height_px).saturating_mul(4)];

    for y in 0..height_px {
        for x in 0..width_px {
            let offset = (y * width_px + x) * 4;
            let blue = (x & 0xFF) as u8;
            let green = (y & 0xFF) as u8;
            let red = ((x + y) & 0xFF) as u8;
            // 语义相同的 r/g/b，只是内存里的通道顺序不同。
            match format {
                InputFormat::Bgra => {
                    buf[offset] = blue;
                    buf[offset + 1] = green;
                    buf[offset + 2] = red;
                }
                InputFormat::Rgba => {
                    buf[offset] = red;
                    buf[offset + 1] = green;
                    buf[offset + 2] = blue;
                }
            }
            buf[offset + 3] = 255;
        }
    }

    // 120x120 的白色方块，随帧号移动。
    let block = 120usize;
    if width_px > block && height_px > block {
        let frame_index = usize::try_from(frame).unwrap_or(0);
        let sx = (frame_index * 17) % (width_px - block);
        let sy = (frame_index * 11) % (height_px - block);
        for y in sy..sy + block {
            for x in sx..sx + block {
                let offset = (y * width_px + x) * 4;
                buf[offset] = 255;
                buf[offset + 1] = 255;
                buf[offset + 2] = 255;
            }
        }
    }
    buf
}

fn run_encode_probe(api: &NvencApi, d3d: &d3d11::D3d11) -> Result<(), WinVideoError> {
    const WIDTH: u32 = 1280;
    const HEIGHT: u32 = 720;
    const FPS: u32 = 60;
    const BITRATE_KBPS: u32 = 4000;
    const P_FRAMES: u32 = 10;

    println!("\n== NVENC H.264 4:2:0 编码（合成 BGRA {WIDTH}x{HEIGHT}）==");
    let config = EncoderConfig::new(WIDTH, HEIGHT, FPS, BITRATE_KBPS)?;
    let mut encoder = NvencEncoder::new(api, d3d, config)?;
    println!(
        "  编码器: {}x{} @ {FPS} fps, CBR {} kbps, H.264 High, 无 B 帧, 输入 {:?}",
        encoder.size().0,
        encoder.size().1,
        BITRATE_KBPS,
        encoder.input_format()
    );

    let mut idr_bytes = 0usize;
    let mut p_bytes_total = 0usize;
    let mut idr_time_ms = 0.0f64;
    let mut p_time_total_ms = 0.0f64;
    let mut idr_dims = None;

    for index in 0..=P_FRAMES {
        let frame = synthetic_pixels(WIDTH, HEIGHT, index, InputFormat::Bgra);
        let upload_start = Instant::now();
        encoder.upload_bgra(&frame)?;
        let upload_ms = upload_start.elapsed().as_secs_f64() * 1000.0;

        let force_idr = index == 0;
        let encode_start = Instant::now();
        let packet = encoder.encode(force_idr)?;
        let encode_ms = encode_start.elapsed().as_secs_f64() * 1000.0;

        println!(
            "  帧 {index:>2}: 上传 {upload_ms:6.2} ms, 编码 {encode_ms:6.2} ms, {:>7} 字节, keyframe={}",
            packet.payload_len(),
            packet.keyframe
        );
        if force_idr {
            idr_bytes = packet.payload_len();
            idr_time_ms = encode_ms;
            idr_dims = parse_sps_dimensions(packet.data.get(4..).unwrap_or_default());
            if !packet.keyframe {
                return Err(WinVideoError::Nvenc(
                    "首帧被要求 IDR 但驱动返回非关键帧".to_owned(),
                ));
            }
        } else {
            p_bytes_total += packet.payload_len();
            p_time_total_ms += encode_ms;
            if packet.keyframe {
                return Err(WinVideoError::Nvenc(format!(
                    "无限 GOP 下第 {index} 帧意外成为关键帧"
                )));
            }
        }
    }

    match idr_dims {
        Some((w, h)) => println!("  SPS 解析输出尺寸: {w}x{h}"),
        None => println!("  警告: 无法从 SPS 解析输出尺寸"),
    }

    let p_count = usize::try_from(P_FRAMES).unwrap_or(1).max(1);
    println!(
        "  汇总: 首帧 IDR {idr_bytes} 字节 / {idr_time_ms:.2} ms；P 帧平均 {} 字节 / {:.2} ms",
        p_bytes_total / p_count,
        p_time_total_ms / f64::from(P_FRAMES.max(1))
    );

    // 在线改码率必须不重建、不插 IDR。
    let reconfigure_start = Instant::now();
    encoder.set_bitrate(2000)?;
    let reconfigure_ms = reconfigure_start.elapsed().as_secs_f64() * 1000.0;
    let frame = synthetic_pixels(WIDTH, HEIGHT, P_FRAMES + 1, InputFormat::Bgra);
    encoder.upload_bgra(&frame)?;
    let encode_start = Instant::now();
    let packet = encoder.encode(false)?;
    let encode_ms = encode_start.elapsed().as_secs_f64() * 1000.0;
    println!(
        "  set_bitrate(2000): 重配置 {reconfigure_ms:.3} ms，随后编码 {encode_ms:.2} ms，{} 字节，keyframe={}",
        packet.payload_len(),
        packet.keyframe
    );
    if packet.keyframe {
        return Err(WinVideoError::Nvenc(
            "set_bitrate 之后出现关键帧：在线重配置不应强制 IDR".to_owned(),
        ));
    }

    Ok(())
}

fn run_rgba_probe(api: &NvencApi, d3d: &d3d11::D3d11) -> Result<(), WinVideoError> {
    const WIDTH: u32 = 1280;
    const HEIGHT: u32 = 720;
    const FRAMES: u32 = 3;

    println!("\n== NVENC H.264 4:2:0 编码（合成 RGBA {WIDTH}x{HEIGHT}）==");
    let config = EncoderConfig::new(WIDTH, HEIGHT, 60, 4000)?.with_input_format(InputFormat::Rgba);
    let mut encoder = NvencEncoder::new(api, d3d, config)?;
    println!(
        "  编码器: {}x{} 输出, 源 {:?}, 输入格式 {:?}（DXGI {:?}）, NVENC 缓冲 {:?}",
        encoder.size().0,
        encoder.size().1,
        encoder.source_size(),
        encoder.input_format(),
        encoder.input_format().dxgi_format(),
        encoder.input_format().buffer_format()
    );

    // 格式不匹配必须被拒绝（防止调用方把 BGRA 当 RGBA 传进来）。
    let first = synthetic_pixels(WIDTH, HEIGHT, 0, InputFormat::Rgba);
    if encoder.upload_bgra(&first).is_ok() {
        return Err(WinVideoError::Config(
            "RGBA 编码器错误地接受了 BGRA 上传".to_owned(),
        ));
    }

    for index in 0..FRAMES {
        let frame = synthetic_pixels(WIDTH, HEIGHT, index, InputFormat::Rgba);
        let upload_start = Instant::now();
        encoder.upload_rgba(&frame)?;
        let upload_ms = upload_start.elapsed().as_secs_f64() * 1000.0;
        let encode_start = Instant::now();
        let packet = encoder.encode(index == 0)?;
        let encode_ms = encode_start.elapsed().as_secs_f64() * 1000.0;
        println!(
            "  帧 {index}: 上传 {upload_ms:6.2} ms, 编码 {encode_ms:6.2} ms, {:>7} 字节, keyframe={}",
            packet.payload_len(),
            packet.keyframe
        );
        if (index == 0) != packet.keyframe {
            return Err(WinVideoError::Nvenc(format!(
                "RGBA 路径第 {index} 帧的 keyframe 标志与预期不符"
            )));
        }
    }

    Ok(())
}

/// 探测 NVENC 能否自行把 4K 输入缩放到 1080p 编码尺寸。
///
/// 失败不视为探针失败：这条能力是可选的，结论会打印出来供适配层决策。
fn run_scaling_probe(api: &NvencApi, d3d: &d3d11::D3d11) -> Result<(), WinVideoError> {
    const ENCODE_WIDTH: u32 = 1920;
    const ENCODE_HEIGHT: u32 = 1080;
    const SOURCE_WIDTH: u32 = 3840;
    const SOURCE_HEIGHT: u32 = 2160;
    const FRAMES: u32 = 3;

    println!(
        "\n== NVENC 输入缩放探测（源 {SOURCE_WIDTH}x{SOURCE_HEIGHT} -> 编码 {ENCODE_WIDTH}x{ENCODE_HEIGHT}）=="
    );
    let config = EncoderConfig::new(ENCODE_WIDTH, ENCODE_HEIGHT, 60, 6000)?
        .with_source_size(SOURCE_WIDTH, SOURCE_HEIGHT);
    let mut encoder = match NvencEncoder::new(api, d3d, config) {
        Ok(encoder) => encoder,
        Err(err) => {
            println!("  编码器创建失败: {err}");
            println!("  结论: NVENC 输入缩放不可用，适配层需要在 CPU 做 box 下采样");
            return Ok(());
        }
    };
    println!(
        "  编码器: 源 {}x{} -> 输出 {}x{}, 输入 {:?}",
        encoder.source_size().0,
        encoder.source_size().1,
        encoder.size().0,
        encoder.size().1,
        encoder.input_format()
    );

    let mut sps_dims = None;
    for index in 0..FRAMES {
        let frame = synthetic_pixels(SOURCE_WIDTH, SOURCE_HEIGHT, index, InputFormat::Bgra);
        let upload_start = Instant::now();
        encoder.upload_bgra(&frame)?;
        let upload_ms = upload_start.elapsed().as_secs_f64() * 1000.0;
        let encode_start = Instant::now();
        let packet = match encoder.encode(index == 0) {
            Ok(packet) => packet,
            Err(err) => {
                println!("  帧 {index} 编码失败: {err}");
                println!("  结论: NVENC 输入缩放不受支持（驱动拒绝 4K 输入 + 1080p 编码），适配层需要 CPU box 下采样");
                return Ok(());
            }
        };
        let encode_ms = encode_start.elapsed().as_secs_f64() * 1000.0;
        println!(
            "  帧 {index}: 上传 {upload_ms:6.2} ms, 编码 {encode_ms:6.2} ms, {:>7} 字节, keyframe={}",
            packet.payload_len(),
            packet.keyframe
        );
        if index == 0 {
            sps_dims = parse_sps_dimensions(packet.data.get(4..).unwrap_or_default());
        }
    }

    match sps_dims {
        Some((w, h)) => {
            println!("  SPS 解析输出尺寸: {w}x{h}");
            if (w, h) == (ENCODE_WIDTH, ENCODE_HEIGHT) {
                println!("  结论: 驱动接受 4K 源 + 1080p 编码，输出尺寸正确；缩放/裁剪判定见下");
            } else {
                println!(
                    "  结论: 驱动接受了输入，但 SPS 输出尺寸 {w}x{h} 与预期 {ENCODE_WIDTH}x{ENCODE_HEIGHT} 不符"
                );
            }
        }
        None => println!("  结论: 驱动接受了输入，但无法解析 SPS 输出尺寸；需要视觉验证"),
    }

    // 区分“真缩放”与“左上角裁剪”：跑两趟独立会话，两帧的唯一差别是
    // 左上 1920x1080 之外的区域。若驱动裁剪，两帧的 P 帧码流应逐字节相同；
    // 若驱动缩放，外部区域会改变画面内容，码流必然不同。
    drop(encoder);
    println!("  缩放/裁剪判定：两趟独立会话，第 2 帧仅左上 {ENCODE_WIDTH}x{ENCODE_HEIGHT} 之外的区域不同");
    let baseline = scaling_test_p_frame(
        api,
        d3d,
        (ENCODE_WIDTH, ENCODE_HEIGHT),
        (SOURCE_WIDTH, SOURCE_HEIGHT),
        false,
    )?;
    let patched = scaling_test_p_frame(
        api,
        d3d,
        (ENCODE_WIDTH, ENCODE_HEIGHT),
        (SOURCE_WIDTH, SOURCE_HEIGHT),
        true,
    )?;
    println!(
        "  基线 P 帧 {} 字节，外部区域变白后 P 帧 {} 字节，码流逐字节相同={}",
        baseline.payload_len(),
        patched.payload_len(),
        baseline.data == patched.data
    );
    if baseline.data == patched.data {
        println!("  结论: 驱动忽略了左上裁剪区域之外的内容 → 实际是**裁剪**而非缩放，适配层需要自行下采样");
    } else {
        println!("  结论: 外部区域改变了码流 → 驱动确实做了**缩放**（非裁剪）");
    }
    Ok(())
}

/// 跑一趟缩放判定会话：第 1 帧黑色 IDR，第 2 帧保持黑色或把左上
/// `crop_width x crop_height` 之外的区域涂白，返回第 2 帧的 P 帧。
fn scaling_test_p_frame(
    api: &NvencApi,
    d3d: &d3d11::D3d11,
    encode: (u32, u32),
    source: (u32, u32),
    white_outside: bool,
) -> Result<nvenc::EncodedPacket, WinVideoError> {
    let config =
        EncoderConfig::new(encode.0, encode.1, 60, 6000)?.with_source_size(source.0, source.1);
    let mut encoder = NvencEncoder::new(api, d3d, config)?;
    let black = solid_pixels(source.0, source.1, 0);
    encoder.upload_bgra(&black)?;
    encoder.encode(true)?;

    let second = if white_outside {
        let mut patched = black.clone();
        whiten_outside_region(&mut patched, source.0, source.1, encode.0, encode.1);
        patched
    } else {
        black
    };
    encoder.upload_bgra(&second)?;
    encoder.encode(false)
}

/// 生成纯色帧（BGR/RGB 三通道同值，alpha=255）。
fn solid_pixels(width: u32, height: u32, value: u8) -> Vec<u8> {
    let width_px = usize::try_from(width).unwrap_or(0);
    let height_px = usize::try_from(height).unwrap_or(0);
    let mut buf = vec![0u8; width_px.saturating_mul(height_px).saturating_mul(4)];
    for pixel in buf.as_chunks_mut::<4>().0 {
        pixel[0] = value;
        pixel[1] = value;
        pixel[2] = value;
        pixel[3] = 255;
    }
    buf
}

/// 把左上 `region_width x region_height` 之外的像素涂白（BGR 三通道）。
fn whiten_outside_region(
    buf: &mut [u8],
    width: u32,
    height: u32,
    region_width: u32,
    region_height: u32,
) {
    let width_px = usize::try_from(width).unwrap_or(0);
    let height_px = usize::try_from(height).unwrap_or(0);
    let region_w = usize::try_from(region_width).unwrap_or(0);
    let region_h = usize::try_from(region_height).unwrap_or(0);
    for y in 0..height_px {
        for x in 0..width_px {
            if x < region_w && y < region_h {
                continue;
            }
            let offset = (y * width_px + x) * 4;
            buf[offset] = 255;
            buf[offset + 1] = 255;
            buf[offset + 2] = 255;
        }
    }
}

/// 把一个带 4 字节大端长度前缀的码流里的 SPS 解析成 `(width, height)`。
fn parse_sps_dimensions(annex_b: &[u8]) -> Option<(u32, u32)> {
    for nal in nal_units(annex_b) {
        let Some(&header) = nal.first() else {
            continue;
        };
        // forbidden_zero_bit(1) + nal_ref_idc(2) + nal_unit_type(5)；SPS = 7。
        if header & 0x1F != 7 {
            continue;
        }
        let rbsp = ebsp_to_rbsp(nal.get(1..).unwrap_or_default());
        if let Some(dims) = parse_sps_rbsp(&rbsp) {
            return Some(dims);
        }
    }
    None
}

/// 按 Annex B start code 切出 NAL 单元（不含 start code）。
fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut index = 0usize;
    let mut payload_start: Option<usize> = None;
    while index + 3 <= data.len() {
        if data[index] == 0 && data[index + 1] == 0 && data[index + 2] == 1 {
            if let Some(start) = payload_start {
                out.push(&data[start..index]);
            }
            payload_start = Some(index + 3);
            index += 3;
        } else {
            index += 1;
        }
    }
    if let Some(start) = payload_start {
        if start < data.len() {
            out.push(&data[start..]);
        }
    }
    out
}

/// 去掉 H.264 的防竞争字节：`00 00 03` -> `00 00`。
fn ebsp_to_rbsp(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0usize;
    for &byte in data {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        out.push(byte);
        if byte == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
    }
    out
}

fn parse_sps_rbsp(rbsp: &[u8]) -> Option<(u32, u32)> {
    let mut reader = BitReader::new(rbsp);
    let profile_idc = reader.read_bits(8)?;
    reader.read_bits(8)?; // constraint_set 标志 + reserved
    reader.read_bits(8)?; // level_idc
    reader.read_ue()?; // seq_parameter_set_id

    // High 系列 profile 才有这一段。
    let mut chroma_format_idc = 1u32;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135 | 144
    ) {
        chroma_format_idc = reader.read_ue()?;
        if chroma_format_idc == 3 {
            reader.read_bit()?; // separate_colour_plane_flag
        }
        reader.read_ue()?; // bit_depth_luma_minus8
        reader.read_ue()?; // bit_depth_chroma_minus8
        reader.read_bit()?; // qpprime_y_zero_transform_bypass_flag
        if reader.read_bit()? == 1 {
            let scaling_lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for list_index in 0..scaling_lists {
                if reader.read_bit()? == 1 {
                    let size = if list_index < 6 { 16 } else { 64 };
                    skip_scaling_list(&mut reader, size)?;
                }
            }
        }
    }

    reader.read_ue()?; // log2_max_frame_num_minus4
    let pic_order_cnt_type = reader.read_ue()?;
    if pic_order_cnt_type == 0 {
        reader.read_ue()?; // log2_max_pic_order_cnt_lsb_minus4
    } else if pic_order_cnt_type == 1 {
        reader.read_bit()?; // delta_pic_order_always_zero_flag
        reader.read_se()?; // offset_for_non_ref_pic
        reader.read_se()?; // offset_for_top_to_bottom_field
        let cycle_length = reader.read_ue()?;
        for _ in 0..cycle_length {
            reader.read_se()?;
        }
    }
    reader.read_ue()?; // max_num_ref_frames
    reader.read_bit()?; // gaps_in_frame_num_value_allowed_flag

    let width_in_mbs_minus1 = reader.read_ue()?;
    let height_in_map_units_minus1 = reader.read_ue()?;
    let frame_mbs_only_flag = reader.read_bit()?;
    if frame_mbs_only_flag == 0 {
        reader.read_bit()?; // mb_adaptive_frame_field_flag
    }
    reader.read_bit()?; // direct_8x8_inference_flag

    // [left, right, top, bottom]
    let mut crop = [0u32; 4];
    if reader.read_bit()? == 1 {
        for value in &mut crop {
            *value = reader.read_ue()?;
        }
    }

    // 4:2:0: subWidthC=2, subHeightC=2；4:2:2: 2/1；4:4:4 与单色: 1/1。
    let sub_width: u32 = if chroma_format_idc == 1 || chroma_format_idc == 2 {
        2
    } else {
        1
    };
    let sub_height: u32 = if chroma_format_idc == 1 { 2 } else { 1 };

    let width = width_in_mbs_minus1
        .checked_add(1)?
        .checked_mul(16)?
        .checked_sub(sub_width.checked_mul(crop[0].checked_add(crop[1])?)?)?;
    let frame_height_in_mbs = 2u32
        .checked_sub(frame_mbs_only_flag)?
        .checked_mul(height_in_map_units_minus1.checked_add(1)?)?;
    let height = frame_height_in_mbs
        .checked_mul(16)?
        .checked_sub(sub_height.checked_mul(crop[2].checked_add(crop[3])?)?)?;
    Some((width, height))
}

fn skip_scaling_list(reader: &mut BitReader<'_>, size: u32) -> Option<()> {
    let mut last_scale = 8i32;
    let mut next_scale = 8i32;
    for _ in 0..size {
        if next_scale != 0 {
            let delta = reader.read_se()?;
            next_scale = (last_scale + delta + 256).rem_euclid(256);
        }
        if next_scale != 0 {
            last_scale = next_scale;
        }
    }
    Some(())
}

/// 按位读取 RBSP。
struct BitReader<'a> {
    data: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0 }
    }

    fn read_bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.bit_pos / 8)?;
        let shift = 7 - (self.bit_pos % 8);
        self.bit_pos += 1;
        Some(u32::from((byte >> shift) & 1))
    }

    fn read_bits(&mut self, count: u32) -> Option<u32> {
        let mut value = 0u64;
        for _ in 0..count {
            value = (value << 1) | u64::from(self.read_bit()?);
            if value > u64::from(u32::MAX) {
                return None;
            }
        }
        u32::try_from(value).ok()
    }

    fn read_ue(&mut self) -> Option<u32> {
        let mut leading_zeros = 0u32;
        while self.read_bit()? == 0 {
            leading_zeros += 1;
            if leading_zeros >= 32 {
                return None;
            }
        }
        let suffix = self.read_bits(leading_zeros)?;
        (1u32.checked_shl(leading_zeros)?.checked_sub(1)?).checked_add(suffix)
    }

    fn read_se(&mut self) -> Option<i32> {
        let code = self.read_ue()?;
        let magnitude = i32::try_from(code.checked_add(1)?.checked_div(2)?).ok()?;
        Some(if code % 2 == 1 { magnitude } else { -magnitude })
    }
}

/// 运行一次探针并把报告打印到 stdout。
///
/// # Errors
/// DXGI/D3D11/NVENC 任一步失败时返回 [`WinVideoError`]。
pub fn run() -> Result<(), WinVideoError> {
    println!("== DXGI 适配器 ==");
    let adapters = d3d11::adapters()?;
    for a in &adapters {
        println!(
            "  [{}] {} vendor=0x{:04X} device=0x{:04X} software={} vram={} MiB luid=0x{:016X}",
            a.index,
            a.name,
            a.vendor_id,
            a.device_id,
            a.software,
            a.dedicated_video_memory / (1024 * 1024),
            a.luid
        );
    }

    println!("\n== NVENC 驱动 ==");
    let api = NvencApi::load()?;
    let (bmaj, bmin) = nvenc::bound_api_version();
    let (dmaj, dmin) = api.driver_api_version();
    println!("  绑定版本: {bmaj}.{bmin}");
    println!("  驱动版本: {dmaj}.{dmin}");
    println!("  请求版本: 0x{:08X}", api.requested_api_version());

    let d3d = d3d11::create_nvidia()?;
    println!(
        "  NVENC 设备: {} (vendor=0x{:04X})",
        d3d.adapter.name, d3d.adapter.vendor_id
    );

    let session = api.open_session(&d3d)?;
    println!("  会话: 已打开");

    let caps = api.h264_caps(&session)?;
    println!("\n== H.264 能力 ==");
    println!("  最大尺寸: {}x{}", caps.max_width, caps.max_height);
    println!("  最大 B 帧: {}", caps.max_bframes);
    println!("  4:4:4 编码: {}", caps.yuv444_encode);
    println!("  10-bit 编码: {}", caps.ten_bit_encode);
    println!("  动态码率: {}", caps.dyn_bitrate);
    println!("  动态分辨率: {}", caps.dyn_resolution);
    println!("  异步编码: {}", caps.async_encode);
    println!("  lookahead: {}", caps.lookahead);
    println!("  temporal AQ: {}", caps.temporal_aq);
    println!("  最大宏块/秒: {}", caps.mb_per_sec_max);

    println!("\n== H.264 profile ==");
    for guid in api.h264_profiles(&session)? {
        let name = if guid == nvenc::NV_ENC_H264_PROFILE_HIGH_GUID {
            "High"
        } else if guid == nvenc::NV_ENC_H264_PROFILE_HIGH_444_GUID {
            "High 4:4:4"
        } else {
            "其他"
        };
        println!("  {name}: {guid:?}");
    }

    println!("\n== H.264 输入格式 ==");
    for fmt in api.h264_input_formats(&session)? {
        println!("  {fmt:?}");
    }

    drop(session);
    run_encode_probe(&api, &d3d)?;
    run_rgba_probe(&api, &d3d)?;
    run_scaling_probe(&api, &d3d)?;

    Ok(())
}
