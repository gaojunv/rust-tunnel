//! 真机探针：屏幕采集的显示器枚举、采集/补丁基准与光标形状采样。
//!
//! 用法（在 Mac 上）：
//!
//! ```text
//! cargo run -p rust-tunnel-mac-capture --bin capture-probe -- list
//! cargo run -p rust-tunnel-mac-capture --bin capture-probe -- bench 5
//! cargo run -p rust-tunnel-mac-capture --bin capture-probe -- gpu 5 [编码宽度]
//! cargo run -p rust-tunnel-mac-capture --bin capture-probe -- xfer 5 [编码宽度]
//! cargo run -p rust-tunnel-mac-capture --bin capture-probe -- enc 5 [编码宽度]
//! cargo run -p rust-tunnel-mac-capture --bin capture-probe -- cursor
//! ```
//!
//! - `gpu` 复刻被控端真实数据面的前两段（`capture_gpu` → `VtEncoder::encode_pixel_buffer`），
//!   每帧都强制编码（跳过引擎的「静止不编码」闸门），用来回答「卡在采集还是编码」；
//! - `xfer` 只测 `VTPixelTransferSession`（4K→目标尺寸的缩放 + `420v` 转换）；
//! - `enc` 只测 VideoToolbox 编码本身（先转换一次，再反复直接提交同一块 `420v` 缓冲）。
//!
//! `enc`/`gpu` 支持排障环境变量：`RT_VT_CODEC=h265`、`RT_VT_KBPS=<kbps>`、
//! `RT_VT_MAXQP=<1..51>`、`RT_VT_FPS=<fps>`（只影响编码器配置，不改采集）。
//!
//! `stream` 复刻适配层的流水线节奏（提交本帧、取回最老一帧），`RT_STREAM_DEPTH` 调在途
//! 深度；`pipe` 是「一次提交很多帧再统一取回」的吞吐上限对照。
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

    use objc2_core_foundation::CFRetained;
    use objc2_core_video::{kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, CVPixelBuffer};
    use rust_tunnel_mac_capture::{CapturedGpuFrame, CursorShape, MacCapture};
    use rust_tunnel_mac_video::pixel_transfer::{create_pixel_buffer, PixelTransfer};
    use rust_tunnel_mac_video::{CodecKind, EncodeConfig, VtEncoder};

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
            "gpu" => {
                let seconds = args
                    .get(2)
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(5);
                let width = args
                    .get(3)
                    .and_then(|value| value.parse::<u32>().ok())
                    .unwrap_or(MAX_ENCODE_WIDTH);
                gpu_bench(seconds, width);
            }
            "xfer" => {
                let seconds = args
                    .get(2)
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(5);
                let width = args
                    .get(3)
                    .and_then(|value| value.parse::<u32>().ok())
                    .unwrap_or(MAX_ENCODE_WIDTH);
                transfer_bench(seconds, width);
            }
            "stream" => {
                let frames = args
                    .get(2)
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(120);
                let width = args
                    .get(3)
                    .and_then(|value| value.parse::<u32>().ok())
                    .unwrap_or(MAX_ENCODE_WIDTH);
                stream_bench(frames, width);
            }
            "pipe" => {
                let frames = args
                    .get(2)
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(120);
                let width = args
                    .get(3)
                    .and_then(|value| value.parse::<u32>().ok())
                    .unwrap_or(MAX_ENCODE_WIDTH);
                pipeline_bench(frames, width);
            }
            "enc" => {
                let seconds = args
                    .get(2)
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(5);
                let width = args
                    .get(3)
                    .and_then(|value| value.parse::<u32>().ok())
                    .unwrap_or(MAX_ENCODE_WIDTH);
                encode_bench(seconds, width);
            }
            other => {
                eprintln!(
                    "未知命令：{other}（可用：list / bench [秒] / gpu [秒] [宽度] / \
                     xfer [秒] [宽度] / enc [秒] [宽度] / pipe [帧数] [宽度] / stream [帧数] [宽度] / cursor）"
                );
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
        let enumerate_started = Instant::now();
        let displays = capture.displays();
        let enumerate_ms = enumerate_started.elapsed().as_secs_f64() * 1000.0;
        println!("枚举显示器耗时：{enumerate_ms:.1} ms");
        // 引擎每 5 秒复查一次；第一次之后的调用必须是亚毫秒（首调用会建立 WindowServer
        // 连接的显示列表，一次性的几十毫秒可以接受）。
        for round in 1..=3 {
            let repeat_started = Instant::now();
            let repeat = capture.displays();
            println!(
                "第 {} 次复查：{:.3} ms（{} 台）",
                round + 1,
                repeat_started.elapsed().as_secs_f64() * 1000.0,
                repeat.len()
            );
        }
        if displays.is_empty() {
            eprintln!("没有枚举到显示器（屏幕录制权限未授予？）");
            return;
        }
        for display in &displays {
            // DisplayInfo 是**逻辑点**（控制端输入映射用），采集流仍按物理像素。
            let coords = match rust_tunnel_mac_capture::display_physical_size(display.id) {
                Some((width, height)) => format!(
                    "逻辑 {}x{} / 物理 {}x{}",
                    display.width, display.height, width, height
                ),
                None => format!("逻辑 {}x{}（物理尺寸不可用）", display.width, display.height),
            };
            println!(
                "显示器 {} · {} · {}",
                display.id,
                coords,
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

    /// GPU 直通采集 + VideoToolbox 硬件编码基准（复刻被控端真实数据面前两段）。
    ///
    /// 按 30 fps 节拍取帧（与引擎默认档位一致），**每帧都强制编码**（跳过「静止不编码」
    /// 闸门），分别统计采集耗时与编码耗时——一眼看出卡在采集还是编码。
    #[expect(
        clippy::cast_precision_loss,
        reason = "探针只打印统计平均值，帧数字远小于 f64 精度上限"
    )]
    fn gpu_bench(seconds: u64, width: u32) {
        let Some(mut context) = GpuContext::open(width) else {
            return;
        };
        let (frame_w, frame_h) = (context.frame.width, context.frame.height);
        let Some(mut encoder) = new_encoder(encoder_config(frame_w, frame_h)) else {
            return;
        };
        println!(
            "编码器：{frame_w}x{frame_h} · hardware={}",
            encoder.is_hardware()
        );

        let mut frames = 0_u64;
        let mut changed_frames = 0_u64;
        let mut capture_us = 0_u128;
        let mut encode_us = 0_u128;
        let mut encode_max_us = 0_u128;
        let mut sent_bytes = 0_u64;
        let period = Duration::from_millis(33);
        let deadline = Instant::now() + Duration::from_secs(seconds.max(1));
        let mut keyframe = true;
        while Instant::now() < deadline {
            let tick = Instant::now();
            let capture_started = Instant::now();
            let Ok(next) = context.capture.capture_gpu(context.display) else {
                eprintln!("GPU 采集失败");
                break;
            };
            context.frame = next;
            let capture_elapsed = capture_started.elapsed().as_micros();
            let encode_started = Instant::now();
            match encoder.encode_pixel_buffer(&context.frame.buffer, keyframe) {
                Ok(packet) => {
                    sent_bytes = sent_bytes.saturating_add(packet.data.len() as u64);
                    keyframe = false;
                }
                Err(err) => {
                    eprintln!("编码失败：{err}");
                    break;
                }
            }
            let encode_elapsed = encode_started.elapsed().as_micros();
            frames = frames.saturating_add(1);
            if context.frame.changed {
                changed_frames = changed_frames.saturating_add(1);
            }
            capture_us = capture_us.saturating_add(capture_elapsed);
            encode_us = encode_us.saturating_add(encode_elapsed);
            encode_max_us = encode_max_us.max(encode_elapsed);
            if let Some(rest) = period.checked_sub(tick.elapsed()) {
                std::thread::sleep(rest);
            }
        }
        if frames == 0 {
            eprintln!("GPU 路径没有采到任何帧");
            return;
        }
        let measured_fps = frames as f64 / seconds.max(1) as f64;
        let average_capture_ms = capture_us as f64 / frames as f64 / 1000.0;
        let average_encode_ms = encode_us as f64 / frames as f64 / 1000.0;
        let max_encode_ms = encode_max_us as f64 / 1000.0;
        let changed_pct = changed_frames as f64 * 100.0 / frames as f64;
        let sent_kbps = sent_bytes.saturating_mul(8) / seconds.max(1) / 1000;
        println!(
            "GPU 直通：{frames} 帧 · {measured_fps:.1} fps · 采集均 {average_capture_ms:.2} ms · \
             编码均 {average_encode_ms:.2} ms（max {max_encode_ms:.2}）· 出码 {sent_kbps} kbps · \
             有变化 {changed_pct:.0}%"
        );
    }

    /// 只测 `VTPixelTransferSession`：把采集帧缩到编码尺寸的 `420v`。
    ///
    /// `gpu` 命令里「编码耗时」= 本命令的转换 + VT 编码本身；两者分开才能判断是不是
    /// 缩放转换退化成软件实现。
    #[expect(
        clippy::cast_precision_loss,
        reason = "探针只打印统计平均值，帧数字远小于 f64 精度上限"
    )]
    fn transfer_bench(seconds: u64, width: u32) {
        let Some(context) = GpuContext::open(width) else {
            return;
        };
        println!(
            "转换：源 {}x{} → 目标 {}x{} 420v（BT.709 limited）",
            context.frame.source_width,
            context.frame.source_height,
            context.frame.width,
            context.frame.height
        );
        let mut count = 0_u64;
        let mut total_us = 0_u128;
        let mut max_us = 0_u128;
        let deadline = Instant::now() + Duration::from_secs(seconds.max(1));
        while Instant::now() < deadline {
            let started = Instant::now();
            if let Err(err) = context.transfer.transfer(&context.frame.buffer, &context.target) {
                eprintln!("转换失败：{err}");
                break;
            }
            let elapsed = started.elapsed().as_micros();
            count = count.saturating_add(1);
            total_us = total_us.saturating_add(elapsed);
            max_us = max_us.max(elapsed);
        }
        if count == 0 {
            eprintln!("转换基准没有跑出任何样本");
            return;
        }
        println!(
            "转换：{count} 次 · 平均 {} ms · max {} ms",
            total_us as f64 / count as f64 / 1000.0,
            max_us as f64 / 1000.0
        );
    }

    /// 只测 **VideoToolbox 编码本身**：先把一帧转换到编码尺寸的 `420v`，再反复直接提交
    /// 同一块缓冲（`encode_pixel_buffer` 的零拷贝分支，不含像素转换）。
    #[expect(
        clippy::cast_precision_loss,
        reason = "探针只打印统计值，帧数字远小于 f64 精度上限"
    )]
    fn encode_bench(seconds: u64, width: u32) {
        let Some(context) = GpuContext::open(width) else {
            return;
        };
        if let Err(err) = context.transfer.transfer(&context.frame.buffer, &context.target) {
            eprintln!("预转换失败：{err}");
            return;
        }
        let Some(mut encoder) = new_encoder(encoder_config(context.frame.width, context.frame.height))
        else {
            return;
        };
        println!(
            "编码：{}x{} 420v 直通提交 · hardware={}",
            context.frame.width,
            context.frame.height,
            encoder.is_hardware()
        );
        let mut samples_us: Vec<u128> = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(seconds.max(1));
        while Instant::now() < deadline {
            let started = Instant::now();
            match encoder.encode_pixel_buffer(&context.target, samples_us.is_empty()) {
                Ok(_) => samples_us.push(started.elapsed().as_micros()),
                Err(err) => {
                    eprintln!("编码失败：{err}");
                    break;
                }
            }
        }
        if samples_us.is_empty() {
            eprintln!("编码基准没有跑出任何样本");
            return;
        }
        samples_us.sort_unstable();
        let average = samples_us.iter().sum::<u128>() as f64 / samples_us.len() as f64 / 1000.0;
        let p50 = samples_us[samples_us.len() / 2] as f64 / 1000.0;
        let max = samples_us[samples_us.len() - 1] as f64 / 1000.0;
        println!(
            "编码：{} 帧 · 平均 {average:.2} ms · 中位 {p50:.2} ms · max {max:.2} ms",
            samples_us.len()
        );
    }

    /// 三段 GPU 基准共用的上下文：采集源 + 首帧 + 一块按编码尺寸预建的 `420v` 目标。
    struct GpuContext {
        capture: MacCapture,
        display: u32,
        frame: CapturedGpuFrame,
        transfer: PixelTransfer,
        target: CFRetained<CVPixelBuffer>,
    }

    impl GpuContext {
        /// 打开采集、按 `max_encode_width` 取首帧，并建好转换会话与目标缓冲。
        fn open(width: u32) -> Option<Self> {
            let mut capture = open()?;
            let display = pick_display(&capture)?;
            capture.set_max_encode_width(width);
            let started = Instant::now();
            let frame = match capture.capture_gpu(display) {
                Ok(frame) => frame,
                Err(err) => {
                    eprintln!("首帧 GPU 采集失败：{err}");
                    return None;
                }
            };
            println!(
                "首帧 GPU：{}x{}（源 {}x{}），耗时 {} ms",
                frame.width,
                frame.height,
                frame.source_width,
                frame.source_height,
                started.elapsed().as_millis()
            );
            let transfer = match PixelTransfer::new() {
                Ok(transfer) => transfer,
                Err(err) => {
                    eprintln!("像素转换会话创建失败：{err}");
                    return None;
                }
            };
            let target = match create_pixel_buffer(
                frame.width,
                frame.height,
                kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
                true,
            ) {
                Ok(target) => target,
                Err(err) => {
                    eprintln!("转换目标缓冲创建失败：{err}");
                    return None;
                }
            };
            Some(Self {
                capture,
                display,
                frame,
                transfer,
                target,
            })
        }
    }

    /// 编码器配置的排障覆盖：`RT_VT_CODEC=h265`、`RT_VT_KBPS=<kbps>`、`RT_VT_FPS=<fps>`。
    fn encoder_config(width: u32, height: u32) -> EncodeConfig {
        let fps = std::env::var("RT_VT_FPS")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(30);
        let kbps = std::env::var("RT_VT_KBPS")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(6000);
        let max_qp = std::env::var("RT_VT_MAXQP")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(20)
            .clamp(1, 51);
        let codec = match std::env::var("RT_VT_CODEC").as_deref() {
            Ok("h265" | "hevc") => CodecKind::H265,
            _ => CodecKind::H264,
        };
        EncodeConfig {
            max_qp,
            ..EncodeConfig::new(codec, width, height, kbps, fps)
        }
    }

    /// 建一个 VideoToolbox 编码器（失败时打印原因）。
    fn new_encoder(config: EncodeConfig) -> Option<VtEncoder> {
        match VtEncoder::new(config) {
            Ok(encoder) => Some(encoder),
            Err(err) => {
                eprintln!("VideoToolbox 编码器创建失败：{err}");
                None
            }
        }
    }

    /// 复刻适配层的**一帧深流水线**：提交本帧、取回上一帧（交替），测真实节奏下的帧率。
    #[expect(
        clippy::cast_precision_loss,
        reason = "探针只打印统计值，帧数字远小于 f64 精度上限"
    )]
    fn stream_bench(frames: u64, width: u32) {
        let Some(context) = GpuContext::open(width) else {
            return;
        };
        if let Err(err) = context.transfer.transfer(&context.frame.buffer, &context.target) {
            eprintln!("预转换失败：{err}");
            return;
        }
        let Some(mut encoder) = new_encoder(encoder_config(context.frame.width, context.frame.height))
        else {
            return;
        };
        let depth = std::env::var("RT_STREAM_DEPTH")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(1)
            .max(1);
        println!(
            "流水线：{}x{} · {frames} 帧 · depth={depth} · hardware={}",
            context.frame.width,
            context.frame.height,
            encoder.is_hardware()
        );
        let started = Instant::now();
        let mut submitted = 0_u64;
        let mut drained = 0_u64;
        let mut bytes = 0_u64;
        let mut max_wait_us = 0_u128;
        while submitted < frames.min(depth) {
            if let Err(err) = encoder.submit_only(&context.target, submitted == 0) {
                eprintln!("提交第 {submitted} 帧失败：{err}");
                return;
            }
            submitted = submitted.saturating_add(1);
        }
        while submitted < frames {
            let wait_started = Instant::now();
            match encoder.next_packet() {
                Ok(packet) => {
                    bytes = bytes.saturating_add(packet.data.len() as u64);
                    drained = drained.saturating_add(1);
                }
                Err(err) => {
                    eprintln!("取第 {drained} 帧输出失败：{err}");
                    return;
                }
            }
            max_wait_us = max_wait_us.max(wait_started.elapsed().as_micros());
            if let Err(err) = encoder.submit_only(&context.target, false) {
                eprintln!("提交第 {submitted} 帧失败：{err}");
                return;
            }
            submitted = submitted.saturating_add(1);
        }
        if let Err(err) = encoder.flush() {
            eprintln!("flush 失败：{err}");
        }
        while drained < frames {
            match encoder.next_packet() {
                Ok(packet) => {
                    bytes = bytes.saturating_add(packet.data.len() as u64);
                    drained = drained.saturating_add(1);
                }
                Err(err) => {
                    eprintln!("收尾取第 {drained} 帧输出失败：{err}");
                    break;
                }
            }
        }
        let total = started.elapsed();
        println!(
            "一帧深流水线：提交 {frames} 帧 · 取回 {drained} 帧 · 总 {:.1} ms · {:.0} fps · 最大等待 {:.1} ms · 出码 {} KB",
            total.as_secs_f64() * 1000.0,
            drained as f64 / total.as_secs_f64().max(1e-9),
            max_wait_us as f64 / 1000.0,
            bytes / 1024
        );
    }

    /// **流水线吞吐**基准：连续提交 `frames` 帧、再统一取回输出。
    ///
    /// 同步逐帧「提交 → 等完成」把硬件编码器的流水线延迟当成吞吐；本命令量化同一台
    /// 机器上异步提交能达到的真实帧率（4K 上差别最明显）。
    #[expect(
        clippy::cast_precision_loss,
        reason = "探针只打印统计值，帧数字远小于 f64 精度上限"
    )]
    fn pipeline_bench(frames: u64, width: u32) {
        let Some(context) = GpuContext::open(width) else {
            return;
        };
        if let Err(err) = context.transfer.transfer(&context.frame.buffer, &context.target) {
            eprintln!("预转换失败：{err}");
            return;
        }
        let Some(mut encoder) = new_encoder(encoder_config(context.frame.width, context.frame.height))
        else {
            return;
        };
        println!(
            "流水线：{}x{} · {frames} 帧 · hardware={}",
            context.frame.width,
            context.frame.height,
            encoder.is_hardware()
        );
        let started = Instant::now();
        for index in 0..frames {
            if let Err(err) = encoder.submit_only(&context.target, index == 0) {
                eprintln!("提交第 {index} 帧失败：{err}");
                return;
            }
        }
        let submit_elapsed = started.elapsed();
        if let Err(err) = encoder.flush() {
            eprintln!("flush 失败：{err}");
        }
        let drain_started = Instant::now();
        let mut drained = 0_u64;
        let mut bytes = 0_u64;
        while drained < frames {
            match encoder.next_packet() {
                Ok(packet) => {
                    bytes = bytes.saturating_add(packet.data.len() as u64);
                    drained = drained.saturating_add(1);
                }
                Err(err) => {
                    eprintln!("取第 {drained} 帧输出失败：{err}");
                    break;
                }
            }
        }
        let total = started.elapsed();
        println!(
            "流水线：提交 {} 帧 {:.1} ms · 取回 {} 帧 {:.1} ms · 总 {:.1} ms · {:.0} fps · 出码 {} KB",
            frames,
            submit_elapsed.as_secs_f64() * 1000.0,
            drained,
            drain_started.elapsed().as_secs_f64() * 1000.0,
            total.as_secs_f64() * 1000.0,
            drained as f64 / total.as_secs_f64().max(1e-9),
            bytes / 1024
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
