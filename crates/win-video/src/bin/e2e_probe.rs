//! `win-video-e2e-probe`：真实屏幕 → 下采样 → NVENC → Media Foundation 解码的全链路验证。
//!
//! GUI 客户端在本机缺少 mingw 链接工具链（vendored libvpx 是 mingw 预编译产物），
//! 跑不起来；这条探针用同一套 `win-video` API 把「采集 → BGRA→RGBA → 盒式下采样
//! → NVENC H.264 → MFT 解码」串起来，验证的是集成层真正会走的路径。
//!
//! 输出：每帧各阶段耗时、解码相对源图的平均绝对误差、以及第一帧解码结果的 BMP
//! （`%TEMP%\rust-tunnel-e2e-probe.bmp`，自顶向下 32bpp）。

#[cfg(windows)]
fn main() {
    if let Err(err) = windows_probe::run() {
        eprintln!("全链路探针失败: {err}");
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("win-video-e2e-probe 只能在 Windows 上运行");
}

#[cfg(windows)]
mod windows_probe {
    use rust_tunnel_win_video::capture;
    use rust_tunnel_win_video::d3d11;
    use rust_tunnel_win_video::decode::H264Decoder;
    use rust_tunnel_win_video::nvenc::{EncoderConfig, InputFormat, NvencApi, NvencEncoder};
    use rust_tunnel_win_video::pixels::{bgra_to_rgba, scale_bgra_to_rgba};
    use rust_tunnel_win_video::WinVideoError;
    use std::fs::File;
    use std::io::{BufWriter, Write};
    use std::path::Path;
    use std::time::Instant;

    /// 最多采集/编解码的帧数。
    const FRAMES: u32 = 10;
    /// 编码宽度上限（与 remote 的 `MAX_ENCODE_WIDTH` 一致）。
    const MAX_WIDTH: u32 = 1920;
    /// 目标码率（kbps）。
    const BITRATE_KBPS: u32 = 8_000;
    /// 目标帧率（只写进码流 VUI/码率控制，探针按实际采集节奏跑）。
    const FPS: u32 = 30;

    fn io_err(context: &str, err: &std::io::Error) -> WinVideoError {
        WinVideoError::Config(format!("{context}: {err}"))
    }

    #[expect(
        clippy::cast_possible_truncation,
        reason = "目标高度由有界尺寸比例推出，远小于 u32 上限"
    )]
    fn aspect_height(src_w: u32, src_h: u32, dst_w: u32) -> u32 {
        let scaled = u64::from(src_h) * u64::from(dst_w) / u64::from(src_w.max(1));
        (scaled as u32).max(2) & !1
    }

    /// 两帧 RGBA 的 RGB 平均绝对误差（忽略 alpha）。
    #[expect(
        clippy::cast_precision_loss,
        reason = "像素数远小于 2^53，转 f64 求均值不损精度"
    )]
    fn mean_abs_diff(a: &[u8], b: &[u8]) -> f64 {
        let pixels = a.len().min(b.len()) / 4;
        if pixels == 0 {
            return 0.0;
        }
        let mut sum: u64 = 0;
        let (a_pixels, _) = a.as_chunks::<4>();
        let (b_pixels, _) = b.as_chunks::<4>();
        for (pa, pb) in a_pixels.iter().zip(b_pixels).take(pixels) {
            sum += u64::from(pa[0].abs_diff(pb[0]))
                + u64::from(pa[1].abs_diff(pb[1]))
                + u64::from(pa[2].abs_diff(pb[2]));
        }
        sum as f64 / (pixels as f64 * 3.0)
    }

    /// 统计 `(中位数, 均值)`。
    #[expect(
        clippy::cast_precision_loss,
        reason = "样本数远小于 2^53，转 f64 求均值不损精度"
    )]
    fn stats(samples: &[f64]) -> (f64, f64) {
        if samples.is_empty() {
            return (0.0, 0.0);
        }
        let mut sorted = samples.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = sorted[sorted.len() / 2];
        let mean = sorted.iter().sum::<f64>() / samples.len() as f64;
        (median, mean)
    }

    /// 把 RGBA 写成自顶向下 32bpp BMP（负高度）。
    fn write_bmp(path: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), WinVideoError> {
        let row_bytes = usize::try_from(width)
            .map_err(|_| WinVideoError::Config("宽度无法转为 usize".to_owned()))?
            .checked_mul(4)
            .ok_or_else(|| WinVideoError::Config("BMP 行字节数溢出".to_owned()))?;
        let image_size = row_bytes
            .checked_mul(usize::try_from(height).unwrap_or(0))
            .ok_or_else(|| WinVideoError::Config("BMP 图像尺寸溢出".to_owned()))?;
        let file_size = image_size
            .checked_add(54)
            .ok_or_else(|| WinVideoError::Config("BMP 文件尺寸溢出".to_owned()))?;
        let file_size = u32::try_from(file_size)
            .map_err(|_| WinVideoError::Config("BMP 超过 4 GiB".to_owned()))?;
        let image_size_u32 = u32::try_from(image_size)
            .map_err(|_| WinVideoError::Config("BMP 图像数据超过 u32".to_owned()))?;
        let height_i32 = i32::try_from(height)
            .map_err(|_| WinVideoError::Config("BMP 高度超过 i32".to_owned()))?;

        let file = File::create(path).map_err(|err| io_err("创建 BMP 失败", &err))?;
        let mut out = BufWriter::new(file);
        out.write_all(b"BM")
            .map_err(|err| io_err("写 BMP 头失败", &err))?;
        out.write_all(&file_size.to_le_bytes())
            .map_err(|err| io_err("写 BMP 头失败", &err))?;
        out.write_all(&[0u8; 4])
            .map_err(|err| io_err("写 BMP 头失败", &err))?;
        out.write_all(&54u32.to_le_bytes())
            .map_err(|err| io_err("写 BMP 头失败", &err))?;
        // BITMAPINFOHEADER
        out.write_all(&40u32.to_le_bytes())
            .map_err(|err| io_err("写 BMP 头失败", &err))?;
        out.write_all(&i32::try_from(width).unwrap_or(0).to_le_bytes())
            .map_err(|err| io_err("写 BMP 头失败", &err))?;
        out.write_all(&(-height_i32).to_le_bytes())
            .map_err(|err| io_err("写 BMP 头失败", &err))?;
        out.write_all(&1u16.to_le_bytes())
            .map_err(|err| io_err("写 BMP 头失败", &err))?;
        out.write_all(&32u16.to_le_bytes())
            .map_err(|err| io_err("写 BMP 头失败", &err))?;
        out.write_all(&0u32.to_le_bytes())
            .map_err(|err| io_err("写 BMP 头失败", &err))?;
        out.write_all(&image_size_u32.to_le_bytes())
            .map_err(|err| io_err("写 BMP 头失败", &err))?;
        out.write_all(&2835i32.to_le_bytes())
            .map_err(|err| io_err("写 BMP 头失败", &err))?;
        out.write_all(&2835i32.to_le_bytes())
            .map_err(|err| io_err("写 BMP 头失败", &err))?;
        out.write_all(&0u32.to_le_bytes())
            .map_err(|err| io_err("写 BMP 头失败", &err))?;
        out.write_all(&0u32.to_le_bytes())
            .map_err(|err| io_err("写 BMP 头失败", &err))?;

        let mut bgra = Vec::with_capacity(image_size.min(rgba.len()));
        let (pixels, _) = rgba.as_chunks::<4>();
        for px in pixels {
            bgra.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
        out.write_all(&bgra)
            .map_err(|err| io_err("写 BMP 像素失败", &err))?;
        out.flush().map_err(|err| io_err("刷新 BMP 失败", &err))?;
        Ok(())
    }

    /// 跑一次全链路探针。
    ///
    /// # Errors
    /// 采集、NVENC、MFT 或文件写入失败时返回 [`WinVideoError`]。
    #[expect(
        clippy::too_many_lines,
        reason = "采集 → 转换 → 下采样 → 编码 → 解码 → 统计是一条不可分割的验证链"
    )]
    pub fn run() -> Result<(), WinVideoError> {
        let monitors = capture::enumerate_monitors()?;
        let monitor = monitors
            .iter()
            .find(|monitor| monitor.is_primary)
            .or_else(|| monitors.first())
            .ok_or_else(|| WinVideoError::Config("没有可用显示器".to_owned()))?;
        println!(
            "显示器 [{}] {}x{} @ ({}, {}) primary={}",
            monitor.index,
            monitor.width,
            monitor.height,
            monitor.left,
            monitor.top,
            monitor.is_primary
        );

        let mut source = capture::open_best(monitor)?;
        println!("采集后端: {}", source.backend());

        let first = source.capture()?;
        let (src_w, src_h) = (first.width, first.height);
        let dst_w = src_w.clamp(2, MAX_WIDTH) & !1;
        let dst_h = aspect_height(src_w, src_h, dst_w);
        println!("源 {src_w}x{src_h} → 编码 {dst_w}x{dst_h}");

        let api = NvencApi::load()?;
        let d3d = d3d11::create_nvidia()?;
        let config = EncoderConfig::new(dst_w, dst_h, FPS, BITRATE_KBPS)?
            .with_input_format(InputFormat::Rgba);
        let mut encoder = NvencEncoder::new(&api, &d3d, config)?;
        let mut decoder = H264Decoder::new(dst_w, dst_h)?;
        println!(
            "NVENC 会话已打开（驱动 {:?}），MFT 已初始化",
            api.driver_api_version()
        );

        let mut capture_ms = Vec::with_capacity(FRAMES as usize);
        let mut convert_scale_ms = Vec::with_capacity(FRAMES as usize);
        let mut encode_ms = Vec::with_capacity(FRAMES as usize);
        let mut decode_ms = Vec::with_capacity(FRAMES as usize);
        let mut first_decoded: Option<(u32, u32, Vec<u8>)> = None;

        for index in 0..FRAMES {
            let capture_start = Instant::now();
            let frame = if index == 0 {
                first.clone()
            } else {
                source.capture()?
            };
            let capture_elapsed = capture_start.elapsed().as_secs_f64() * 1000.0;

            let convert_start = Instant::now();
            // 与集成层一致：一步 BGRA → 目标尺寸 RGBA，避免 4K 中间缓冲。
            let scaled = if (frame.width, frame.height) == (dst_w, dst_h) {
                bgra_to_rgba(&frame.bgra)
            } else {
                scale_bgra_to_rgba(&frame.bgra, frame.width, frame.height, dst_w, dst_h)?
            };
            let convert_elapsed = convert_start.elapsed().as_secs_f64() * 1000.0;

            let encode_start = Instant::now();
            let packet = encoder.encode_rgba(&scaled, index == 0)?;
            let encode_elapsed = encode_start.elapsed().as_secs_f64() * 1000.0;

            let decode_start = Instant::now();
            let decoded_frame = decoder.decode(&packet.data)?;
            let decode_elapsed = decode_start.elapsed().as_secs_f64() * 1000.0;

            capture_ms.push(capture_elapsed);
            convert_scale_ms.push(convert_elapsed);
            encode_ms.push(encode_elapsed);
            decode_ms.push(decode_elapsed);

            match decoded_frame {
                Some(frame) => {
                    let mad = mean_abs_diff(&frame.rgba, &scaled);
                    println!(
                        "帧 {index:>2}: 采集 {capture_elapsed:6.2} ms, 转换+缩放 {convert_elapsed:6.2} ms, \
                         编码 {encode_elapsed:6.2} ms, 解码 {decode_elapsed:6.2} ms, {} 字节, \
                         keyframe={}, 解码 {}x{}, MAD={mad:.2}",
                        packet.payload_len(),
                        packet.keyframe,
                        frame.width,
                        frame.height
                    );
                    if first_decoded.is_none() {
                        first_decoded = Some((frame.width, frame.height, frame.rgba));
                    }
                }
                None => println!(
                    "帧 {index:>2}: 采集 {capture_elapsed:6.2} ms, 转换+缩放 {convert_elapsed:6.2} ms, \
                     编码 {encode_elapsed:6.2} ms, 解码 {decode_elapsed:6.2} ms, {} 字节, keyframe={}, 无输出",
                    packet.payload_len(),
                    packet.keyframe
                ),
            }
        }

        let (cap_med, cap_mean) = stats(&capture_ms);
        let (conv_med, conv_mean) = stats(&convert_scale_ms);
        let (enc_med, enc_mean) = stats(&encode_ms);
        let (dec_med, dec_mean) = stats(&decode_ms);
        println!(
            "\n汇总（{FRAMES} 帧）:\n  采集      中位 {cap_med:6.2} ms / 均值 {cap_mean:6.2} ms\n  \
             转换+缩放 中位 {conv_med:6.2} ms / 均值 {conv_mean:6.2} ms\n  \
             编码      中位 {enc_med:6.2} ms / 均值 {enc_mean:6.2} ms\n  \
             解码      中位 {dec_med:6.2} ms / 均值 {dec_mean:6.2} ms"
        );

        if let Some((width, height, rgba)) = first_decoded {
            let path = std::env::temp_dir().join("rust-tunnel-e2e-probe.bmp");
            write_bmp(&path, width, height, &rgba)?;
            println!("第一帧解码结果已写入 {}", path.display());
        } else {
            println!("警告：没有任何解码输出，BMP 未写入");
        }

        Ok(())
    }
}
