//! `win-video-mem-probe`：采集 / 编码 / 补丁路径的**进程内存**真机探针。
//!
//! 目的是把「被控端内存占用过高」拆成可复核的数字：同一个进程里依次
//! 「打开采集源 → 首帧（建纹理池）→ 建编码器 → 连续跑 N 帧 → 释放编码器 → 释放采集源」，
//! 每一步打印 working set / 峰值 working set / commit（`GetProcessMemoryInfo`）。
//!
//! 三种模式：
//! - `gpu`（默认）：`capture_texture_with_change` + 硬件编码（NVENC 优先，否则 MFT）。
//! - `patch`：`capture_texture_with_dirty` + `extract_patches`（走补丁 staging 图集）。
//! - `cpu`：`capture()` 的整帧 BGRA → RGBA（VP9 会话/CPU 回退路径）。
//!
//! 用法：
//! ```text
//! win-video-mem-probe [--mode gpu|patch|cpu] [--frames 600] [--fps 30] [--hold 3]
//! ```
//!
//! **不落盘**（不写截图）；只读主显示器画面，跑完即退出。

#[cfg(windows)]
fn main() {
    if let Err(err) = probe::run() {
        eprintln!("内存探针失败: {err}");
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("win-video-mem-probe 只能在 Windows 上运行");
}

#[cfg(windows)]
mod probe {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use rust_tunnel_win_video::capture::{self, CaptureSource, CapturedTexture};
    use rust_tunnel_win_video::codec::CodecKind;
    use rust_tunnel_win_video::d3d11;
    use rust_tunnel_win_video::decode::MfVideoDecoder;
    use rust_tunnel_win_video::mft_encoder::{enumerate_encoders, MfVideoEncoder};
    use rust_tunnel_win_video::nvenc::{EncoderConfig, InputFormat, NvencApi, NvencEncoder};
    use rust_tunnel_win_video::pixels::bgra_to_rgba;
    use rust_tunnel_win_video::scale::TextureScaler;
    use rust_tunnel_win_video::WinVideoError;
    use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::GetCurrentProcess;
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };

    /// 默认帧数（30 fps 下 20 秒）。
    const DEFAULT_FRAMES: u32 = 600;
    /// 默认帧率。
    const DEFAULT_FPS: u32 = 30;
    /// 目标码率（kbps）。
    const BITRATE_KBPS: u32 = 20_000;
    /// 每个采样点之间的帧数。
    const SAMPLE_EVERY: u32 = 60;

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mode {
        Gpu,
        Patch,
        Cpu,
        /// 只测硬件 MFT 编码器（合成帧、CPU 输入）：用于在无采集设备绑定时量化 QSV。
        Qsv,
        /// 每轮**新建一块 NVIDIA 设备**再建 NVENC（模拟「采集在核显、编码在独显」的
        /// 混合机型：生产为每个编码器新建设备），用来验证多场会话是否累积。
        NvencFresh,
        /// 每轮建/销一个硬件解码器（验证启动探测解码器的常驻代价）。
        Decoder,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum EncoderKind {
        Auto,
        Nvenc,
        Mft,
    }

    #[expect(
        clippy::struct_excessive_bools,
        reason = "探针开关与命令行一一对应，按 flag 建模最直观"
    )]
    struct Args {
        mode: Mode,
        encoder: EncoderKind,
        codec: CodecKind,
        frames: u32,
        fps: u32,
        hold_s: u64,
        width: u32,
        height: u32,
        /// `gpu` 模式下重复「建编码器 → 编几帧 → 丢编码器」，模拟多场会话。
        cycles: u32,
        /// 同时打开几台显示器的采集源（每台各有一套 WGC 帧池 + 纹理池）。
        displays: u32,
        /// 编码宽度（0 = 跟采集纹理一致）。小于采集宽度时用 GPU VideoProcessor 先缩放，
        /// 模拟 `native_resolution = false` 的 1920 编码。
        encode_width: u32,
        /// `qsv` 模式复用同一块（Intel）D3D11 设备，而不是每次新建。
        ///
        /// 生产在被控端采集设备是 Intel 时就是复用采集设备；`false` 才等价于
        /// 「每次会话新建设备」。
        shared_device: bool,
        /// `qsv` 模式只做 MFT 枚举，不激活编码器（隔离枚举开销）。
        enum_only: bool,
        /// `qsv` 模式只建/销 D3D11 设备，不碰 MFT（隔离设备开销）。
        device_only: bool,
        /// `qsv` 模式只建一次编码器，在多个「会话」之间复用（验证池化修复）。
        reuse: bool,
    }

    impl Default for Args {
        fn default() -> Self {
            Self {
                mode: Mode::Gpu,
                encoder: EncoderKind::Auto,
                codec: CodecKind::H264,
                frames: DEFAULT_FRAMES,
                fps: DEFAULT_FPS,
                hold_s: 2,
                width: 3840,
                height: 2160,
                cycles: 1,
                displays: 1,
                encode_width: 0,
                shared_device: false,
                enum_only: false,
                device_only: false,
                reuse: false,
            }
        }
    }

    fn parse_args(raw: impl Iterator<Item = String>) -> Result<Args, String> {
        let mut args = Args::default();
        let mut it = raw.peekable();
        while let Some(flag) = it.next() {
            let mut value = || it.next().ok_or_else(|| format!("{flag} 缺参数值"));
            match flag.as_str() {
                "--mode" => {
                    args.mode = match value()?.as_str() {
                        "gpu" => Mode::Gpu,
                        "patch" => Mode::Patch,
                        "cpu" => Mode::Cpu,
                        "qsv" => Mode::Qsv,
                        "nvenc-fresh" => Mode::NvencFresh,
                        "decoder" => Mode::Decoder,
                        other => return Err(format!("未知 mode：{other}")),
                    };
                }
                "--encoder" => {
                    args.encoder = match value()?.as_str() {
                        "auto" => EncoderKind::Auto,
                        "nvenc" => EncoderKind::Nvenc,
                        "mft" => EncoderKind::Mft,
                        other => return Err(format!("未知 encoder：{other}")),
                    };
                }
                "--codec" => {
                    args.codec = match value()?.as_str() {
                        "h264" => CodecKind::H264,
                        "h265" => CodecKind::H265,
                        other => return Err(format!("未知 codec：{other}")),
                    };
                }
                "--width" => args.width = value()?.parse().map_err(|e| format!("width: {e}"))?,
                "--height" => args.height = value()?.parse().map_err(|e| format!("height: {e}"))?,
                "--cycles" => args.cycles = value()?.parse().map_err(|e| format!("cycles: {e}"))?,
                "--displays" => {
                    args.displays = value()?.parse().map_err(|e| format!("displays: {e}"))?;
                }
                "--encode-width" => {
                    args.encode_width =
                        value()?.parse().map_err(|e| format!("encode-width: {e}"))?;
                }
                "--shared-device" => args.shared_device = true,
                "--enum-only" => args.enum_only = true,
                "--device-only" => args.device_only = true,
                "--reuse" => args.reuse = true,
                "--frames" => args.frames = value()?.parse().map_err(|e| format!("frames: {e}"))?,
                "--fps" => args.fps = value()?.parse().map_err(|e| format!("fps: {e}"))?,
                "--hold" => args.hold_s = value()?.parse().map_err(|e| format!("hold: {e}"))?,
                "--help" | "-h" => {
                    println!(
                        "用法: win-video-mem-probe [--mode gpu|patch|cpu|qsv] [--encoder auto|nvenc|mft] [--codec h264|h265] [--frames N] [--fps N] [--hold 秒] [--cycles N] [--displays N] [--width W] [--height H]"
                    );
                    std::process::exit(0);
                }
                other => return Err(format!("未知参数：{other}")),
            }
        }
        if args.fps == 0 {
            return Err("fps 必须 > 0".to_owned());
        }
        Ok(args)
    }

    /// 进程内存读数（字节）。
    #[derive(Debug, Clone, Copy)]
    struct Mem {
        working_set: usize,
        peak_working_set: usize,
        commit: usize,
    }

    fn read_mem() -> Mem {
        // SAFETY: `GetCurrentProcess` 是伪句柄；counters 是本栈帧上的 POD，cb 已按
        // 结构体大小填写。失败时保留零值（探针不因读数失败而中断）。
        unsafe {
            let mut counters = PROCESS_MEMORY_COUNTERS {
                cb: u32::try_from(std::mem::size_of::<PROCESS_MEMORY_COUNTERS>()).unwrap_or(0),
                ..PROCESS_MEMORY_COUNTERS::default()
            };
            let _ = GetProcessMemoryInfo(GetCurrentProcess(), &raw mut counters, counters.cb);
            Mem {
                working_set: counters.WorkingSetSize,
                peak_working_set: counters.PeakWorkingSetSize,
                commit: counters.PagefileUsage,
            }
        }
    }

    /// 由采集尺寸与 `--encode-width` 算目标编码尺寸（0 = 不缩放；高度按宽高比、对齐偶数）。
    fn encode_size(src_w: u32, src_h: u32, max_width: u32) -> (u32, u32) {
        if max_width == 0 || max_width >= src_w {
            return (src_w, src_h);
        }
        let width = max_width & !1;
        let scaled = u64::from(src_h)
            .saturating_mul(u64::from(width))
            .checked_div(u64::from(src_w.max(1)))
            .unwrap_or(u64::from(src_h));
        let height = u32::try_from(scaled).unwrap_or(src_h) & !1;
        (width, height.max(2))
    }

    fn mb(bytes: usize) -> f64 {
        // 诊断输出：精度损失无所谓。
        #[expect(clippy::cast_precision_loss, reason = "诊断读数")]
        let value = bytes as f64;
        value / (1024.0 * 1024.0)
    }

    fn print_mem(tag: &str) {
        let mem = read_mem();
        println!(
            "[mem] {tag:<28} ws={:>8.1} MB  peak_ws={:>8.1} MB  commit={:>8.1} MB",
            mb(mem.working_set),
            mb(mem.peak_working_set),
            mb(mem.commit)
        );
    }

    /// 编码器：NVENC 或 MFT（会话里也是这两条）。
    enum Encoder {
        // 两种编码器体积差很大（pedantic 要求装箱），顺带让枚举本身保持小对象。
        Nvenc(Box<NvencEncoder<'static>>),
        Mft(Box<MfVideoEncoder>),
    }

    impl Encoder {
        fn encode(&mut self, texture: &CapturedTexture, idr: bool) -> Result<usize, WinVideoError> {
            match self {
                Self::Nvenc(enc) => enc
                    .encode_texture(&texture.texture, idr)
                    .map(|p| p.data.len()),
                Self::Mft(enc) => enc.encode_texture(texture, idr).map(|p| p.data.len()),
            }
        }

        /// 编码一张**已缩放**的裸纹理（`native_resolution = false` 的 NVENC 路径）。
        fn encode_raw(
            &mut self,
            texture: &ID3D11Texture2D,
            idr: bool,
        ) -> Result<usize, WinVideoError> {
            match self {
                Self::Nvenc(enc) => enc.encode_texture(texture, idr).map(|p| p.data.len()),
                Self::Mft(_) => Err(WinVideoError::Config(
                    "MFT 缩放路径未在探针里实现（只用于 NVENC 对比）".to_owned(),
                )),
            }
        }
    }

    fn create_encoder(
        texture: &CapturedTexture,
        width: u32,
        height: u32,
        fps: u32,
        kind: EncoderKind,
        codec: CodecKind,
    ) -> Result<Encoder, WinVideoError> {
        let device = texture.device.clone();
        let nvenc_available = device.adapter.is_nvidia() && kind != EncoderKind::Mft;
        if nvenc_available {
            if let Ok(api) = NvencApi::load() {
                // 与生产路径一致：函数表活到进程结束，编码器只借用它。
                let api: &'static NvencApi = Box::leak(Box::new(api));
                let mut config = EncoderConfig::new(width, height, fps, BITRATE_KBPS)?;
                config.source_width = width;
                config.source_height = height;
                config.codec = codec;
                return NvencEncoder::new(api, device, config).map(Encoder::wrap_nvenc);
            }
        }
        MfVideoEncoder::new_with_device_codec(device, width, height, fps, BITRATE_KBPS, codec)
            .map(|enc| Encoder::Mft(Box::new(enc)))
    }

    /// `qsv` 模式：只建硬件编码器（自己选设备，不绑采集设备），喂合成 RGBA。
    ///
    /// 用于量化「Intel/AMD 硬件 MFT 自身」的内存。GPU 零拷贝路径不能在 NVIDIA 采集
    /// 设备上绑 Intel MFT（`SET_D3D_MANAGER` 会返回参数错误），所以这里走 CPU 输入。
    #[expect(
        clippy::too_many_lines,
        reason = "隔离实验（枚举/设备/建销/复用）平铺在同一函数里，便于对照同一组打印"
    )]
    fn run_qsv(args: &Args) -> Result<(), WinVideoError> {
        // 两种隔离实验：只枚举 MFT / 只建销 D3D11 设备。
        if args.enum_only || args.device_only {
            for cycle in 0..args.cycles.max(1) {
                if args.enum_only {
                    let found = enumerate_encoders(args.codec, true)?;
                    drop(found);
                } else {
                    let device = d3d11::create(None)?;
                    drop(device);
                }
                print_mem(&format!("cycle {} done", cycle + 1));
            }
            return Ok(());
        }

        let pixels = u64::from(args.width)
            .saturating_mul(u64::from(args.height))
            .saturating_mul(4);
        let len =
            usize::try_from(pixels).map_err(|_| WinVideoError::Config("合成帧过大".to_owned()))?;
        let rgba = vec![128_u8; len];
        print_mem("rgba buffer");

        // 复用设备时先建一次 Intel 设备（生产：采集设备就是它）。
        let shared = if args.shared_device {
            let adapters = d3d11::adapters()?;
            let intel = adapters
                .iter()
                .find(|a| a.vendor_id == d3d11::VENDOR_INTEL && !a.software)
                .map(|a| a.index);
            let device = Arc::new(d3d11::create(intel)?);
            println!("  共享设备: {}", device.adapter.name);
            print_mem("shared device");
            Some(device)
        } else {
            None
        };

        let interval = Duration::from_secs_f64(1.0 / f64::from(args.fps));

        // 池化验证：编码器只建一次，多个「会话」循环喂帧（每轮首帧强制 IDR）。
        if args.reuse {
            let mut encoder = match shared.as_ref() {
                Some(device) => MfVideoEncoder::new_with_device_codec(
                    Arc::clone(device),
                    args.width,
                    args.height,
                    args.fps,
                    BITRATE_KBPS,
                    args.codec,
                )?,
                None => MfVideoEncoder::new_codec(
                    args.width,
                    args.height,
                    args.fps,
                    BITRATE_KBPS,
                    args.codec,
                )?,
            };
            print_mem("reuse encoder created");
            for cycle in 0..args.cycles.max(1) {
                for index in 0..args.frames {
                    let _ = encoder.encode_rgba(&rgba, index == 0)?;
                }
                print_mem(&format!("reuse cycle {}", cycle + 1));
                std::thread::sleep(Duration::from_millis(200));
            }
            return Ok(());
        }

        for cycle in 0..args.cycles.max(1) {
            let mut encoder = match shared.as_ref() {
                Some(device) => MfVideoEncoder::new_with_device_codec(
                    Arc::clone(device),
                    args.width,
                    args.height,
                    args.fps,
                    BITRATE_KBPS,
                    args.codec,
                )?,
                None => MfVideoEncoder::new_codec(
                    args.width,
                    args.height,
                    args.fps,
                    BITRATE_KBPS,
                    args.codec,
                )?,
            };
            println!(
                "  cycle {} 编码器: MFT ({})",
                cycle + 1,
                encoder.adapter_name()
            );
            print_mem(&format!("cycle {} encoder created", cycle + 1));

            let mut next_tick = Instant::now();
            for index in 0..args.frames {
                let _ = encoder.encode_rgba(&rgba, index == 0)?;
                next_tick += interval;
                let now = Instant::now();
                if next_tick > now {
                    std::thread::sleep(next_tick - now);
                } else {
                    next_tick = now;
                }
            }
            drop(encoder);
            print_mem(&format!("cycle {} encoder dropped", cycle + 1));
            if args.cycles > 1 {
                std::thread::sleep(Duration::from_millis(500));
                print_mem(&format!("cycle {} +500ms", cycle + 1));
            }
        }
        if args.hold_s > 0 {
            std::thread::sleep(Duration::from_secs(args.hold_s));
            print_mem("after hold (driver release)");
        }
        Ok(())
    }

    /// `decoder`：每轮建/销一个硬件解码器，量化启动探测的常驻代价。
    fn run_decoder(args: &Args) -> Result<(), WinVideoError> {
        for cycle in 0..args.cycles.max(1) {
            let decoder = match args.codec {
                CodecKind::H264 => MfVideoDecoder::new(args.width, args.height)?,
                CodecKind::H265 => MfVideoDecoder::new_hevc(args.width, args.height)?,
            };
            print_mem(&format!("cycle {} decoder created", cycle + 1));
            drop(decoder);
            print_mem(&format!("cycle {} decoder dropped", cycle + 1));
        }
        Ok(())
    }

    /// `nvenc-fresh`：每轮新建 NVIDIA 设备 + NVENC 会话，喂合成 RGBA。
    ///
    /// 混合机型（显示/采集在核显、编码在独显）上，生产路径每个编码器都会
    /// `d3d11::create_nvidia()`，而不是复用采集设备。这里专门验证这条路径在
    /// 多场会话之间是否把设备/会话内存还回去。
    fn run_nvenc_fresh(args: &Args) -> Result<(), WinVideoError> {
        let api: &'static NvencApi = Box::leak(Box::new(NvencApi::load()?));
        let pixels = u64::from(args.width)
            .saturating_mul(u64::from(args.height))
            .saturating_mul(4);
        let len =
            usize::try_from(pixels).map_err(|_| WinVideoError::Config("合成帧过大".to_owned()))?;
        let rgba = vec![128_u8; len];
        print_mem("rgba buffer");

        for cycle in 0..args.cycles.max(1) {
            let device = Arc::new(d3d11::create_nvidia()?);
            print_mem(&format!("cycle {} device", cycle + 1));
            let mut config = EncoderConfig::new(args.width, args.height, args.fps, BITRATE_KBPS)?;
            config.source_width = args.width;
            config.source_height = args.height;
            config.input_format = InputFormat::Rgba;
            config.codec = args.codec;
            let mut encoder = NvencEncoder::new(api, device, config)?;
            print_mem(&format!("cycle {} encoder", cycle + 1));
            for index in 0..args.frames {
                let _ = encoder.encode_rgba(&rgba, index == 0)?;
            }
            print_mem(&format!("cycle {} encoded", cycle + 1));
            drop(encoder);
            print_mem(&format!("cycle {} encoder dropped", cycle + 1));
            std::thread::sleep(Duration::from_millis(500));
            print_mem(&format!("cycle {} +500ms", cycle + 1));
        }
        if args.hold_s > 0 {
            std::thread::sleep(Duration::from_secs(args.hold_s));
            print_mem("after hold");
        }
        Ok(())
    }

    impl Encoder {
        fn wrap_nvenc(enc: NvencEncoder<'static>) -> Self {
            Self::Nvenc(Box::new(enc))
        }

        fn label(&self) -> &'static str {
            match self {
                Self::Nvenc(_) => "NVENC",
                Self::Mft(_) => "MFT",
            }
        }
    }

    #[expect(
        clippy::too_many_lines,
        clippy::similar_names,
        reason = "探针按「打开 → 采样 → 循环 → 释放」顺序平铺，拆开反而看不出内存检查点顺序；\
                  encoder/enc、scaler/scaled 是同一物体的「槽位」与「借用」"
    )]
    pub fn run() -> Result<(), WinVideoError> {
        // 与采集探针同款：不声明 per-monitor DPI 感知的话 4K@150% 会被报成 2560×1440。
        // SAFETY: 进程启动早期调用，无其它 UI 依赖。
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        let args = parse_args(std::env::args().skip(1)).map_err(WinVideoError::Config)?;
        println!(
            "== win-video-mem-probe ==\n  mode={}  encoder={}  codec={}  frames={}  fps={}  hold={}s",
            match args.mode {
                Mode::Gpu => "gpu",
                Mode::Patch => "patch",
                Mode::Cpu => "cpu",
                Mode::Qsv => "qsv",
                Mode::NvencFresh => "nvenc-fresh",
                Mode::Decoder => "decoder",
            },
            match args.encoder {
                EncoderKind::Auto => "auto",
                EncoderKind::Nvenc => "nvenc",
                EncoderKind::Mft => "mft",
            },
            match args.codec {
                CodecKind::H264 => "h264",
                CodecKind::H265 => "h265",
            },
            args.frames,
            args.fps,
            args.hold_s
        );
        print_mem("start");
        if args.mode == Mode::Qsv {
            return run_qsv(&args);
        }
        if args.mode == Mode::NvencFresh {
            return run_nvenc_fresh(&args);
        }
        if args.mode == Mode::Decoder {
            return run_decoder(&args);
        }

        let monitors = capture::enumerate_monitors()?;
        let monitor = monitors
            .iter()
            .find(|m| m.is_primary)
            .or_else(|| monitors.first())
            .ok_or_else(|| WinVideoError::Config("没有可用显示器".to_owned()))?;
        println!(
            "  显示器 [{}] {} {}x{}  backend=待打开",
            monitor.index, monitor.device_name, monitor.width, monitor.height
        );

        let mut source = capture::open_best(monitor)?;
        println!("  采集后端: {}", source.backend());
        print_mem("source opened");

        // 引擎的采集源按显示器缓存、会话之间不释放。这里把另外几台也打开并各采一帧
        // （触发纹理池分配），量化「多显示器被先后访问过」的常驻增量。
        let mut extra_sources: Vec<Box<dyn CaptureSource>> = Vec::new();
        for other in monitors
            .iter()
            .filter(|m| m.index != monitor.index)
            .take(usize::try_from(args.displays.saturating_sub(1)).unwrap_or(0))
        {
            let mut extra = capture::open_best(other)?;
            let (captured, _report) = extra.capture_texture_with_change()?;
            println!(
                "  额外显示器 [{}] {}x{} backend={}",
                other.index,
                captured.width,
                captured.height,
                extra.backend()
            );
            print_mem(&format!("display {} pool", other.index));
            extra_sources.push(extra);
        }

        // 多场会话：每轮建编码器、编 `frames` 帧、丢弃。若驱动/编码器资源不归还，
        // commit 会一轮一轮往上爬——这正是「跑久了超过 1G」的嫌疑形态。
        if args.mode == Mode::Gpu && args.cycles > 1 {
            for cycle in 0..args.cycles {
                let (captured, _report) = source.capture_texture_with_change()?;
                let (w, h) = (captured.width, captured.height);
                print_mem(&format!("cycle {} (pool)", cycle + 1));
                let mut enc = create_encoder(&captured, w, h, args.fps, args.encoder, args.codec)?;
                print_mem(&format!("cycle {} encoder created", cycle + 1));
                for index in 0..args.frames {
                    let _ = enc.encode(&captured, index == 0)?;
                }
                print_mem(&format!("cycle {} encoded", cycle + 1));
                drop(enc);
                print_mem(&format!("cycle {} encoder dropped", cycle + 1));
                std::thread::sleep(Duration::from_millis(500));
                print_mem(&format!("cycle {} +500ms", cycle + 1));
            }
            return Ok(());
        }

        let interval = Duration::from_secs_f64(1.0 / f64::from(args.fps));
        let mut encoder: Option<Encoder> = None;
        let mut scaler: Option<TextureScaler> = None;
        let mut encode_dims: Option<(u32, u32)> = None;
        let mut next_tick = Instant::now();
        let mut frames = 0_u32;
        let mut encoded_bytes = 0_u64;
        let mut last_texture: Option<CapturedTexture> = None;

        for index in 0..args.frames {
            let frame_started = Instant::now();
            match args.mode {
                Mode::Gpu => {
                    let (captured, report) = source.capture_texture_with_change()?;
                    if index == 0 {
                        println!(
                            "  采集纹理: {}x{}（变化面积 {}‰）",
                            captured.width, captured.height, report.changed_permille
                        );
                        print_mem("first capture (texture pool)");
                        let (sw, sh) = (captured.width, captured.height);
                        let (ew, eh) = encode_size(sw, sh, args.encode_width);
                        encode_dims = Some((ew, eh));
                        if (ew, eh) != (sw, sh) {
                            // 与生产 `native_resolution = false` 同一条路：VideoProcessor
                            // 先把 4K 缩到目标尺寸，再交给编码器。
                            scaler = Some(TextureScaler::new(captured.device.clone())?);
                            println!("  缩放: {sw}x{sh} -> {ew}x{eh}");
                        }
                        let enc =
                            create_encoder(&captured, ew, eh, args.fps, args.encoder, args.codec)?;
                        println!("  编码器: {}（{}x{}）", enc.label(), ew, eh);
                        encoder = Some(enc);
                        print_mem("encoder created");
                    }
                    if let Some(enc) = encoder.as_mut() {
                        // 与生产一致：静止帧也每 SAMPLE_EVERY 帧强制编一次，保持参考链与
                        // 统计可比；变化帧照常编码。
                        let force = report.changed || index % SAMPLE_EVERY == 0;
                        if force {
                            let encoded = match (scaler.as_mut(), encode_dims) {
                                (Some(scaler), Some((ew, eh))) => {
                                    let scaled = scaler.scale(
                                        &captured.texture,
                                        (captured.width, captured.height),
                                        (ew, eh),
                                    )?;
                                    enc.encode_raw(&scaled, index == 0)?
                                }
                                _ => enc.encode(&captured, index == 0)?,
                            };
                            encoded_bytes =
                                encoded_bytes.saturating_add(u64::try_from(encoded).unwrap_or(0));
                        }
                    }
                    last_texture = Some(captured);
                }
                Mode::Patch => {
                    let (captured, report, grid) = source.capture_texture_with_dirty()?;
                    if index == 0 {
                        println!(
                            "  采集纹理: {}x{}（变化面积 {}‰）",
                            captured.width, captured.height, report.changed_permille
                        );
                    }
                    if let Some(grid) = &grid {
                        // 目标尺寸取采集尺寸：让 staging 图集按最大比例分配（最坏内存）。
                        let _ = capture::extract_patches(
                            &captured,
                            grid,
                            captured.width,
                            captured.height,
                            &capture::PatchParams::default(),
                        )?;
                    }
                    last_texture = Some(captured);
                }
                Mode::Cpu => {
                    let frame = source.capture()?;
                    let rgba = bgra_to_rgba(&frame.bgra);
                    if index == 0 {
                        println!(
                            "  CPU 采集帧: {}x{} RGBA={}B",
                            frame.width,
                            frame.height,
                            rgba.len()
                        );
                    }
                    // 用完即丢：复现引擎 CPU 路径每帧分配/释放的节奏。
                    drop(rgba);
                }
                // `qsv` / `nvenc-fresh` 在 `run` 开头就分流走了，不会走到这条采集循环。
                Mode::Qsv | Mode::NvencFresh | Mode::Decoder => {
                    unreachable!("该模式不经过采集路径")
                }
            }

            frames = frames.saturating_add(1);
            if (index + 1) % SAMPLE_EVERY == 0 {
                print_mem(&format!("frames={}", index + 1));
            }

            next_tick += interval;
            let now = Instant::now();
            if next_tick > now {
                std::thread::sleep(next_tick - now);
            } else {
                next_tick = now;
            }
            let _ = frame_started.elapsed();
        }

        // 引擎在会话里**不会**丢弃采集源；这里先只丢编码器，量化「会话结束但源还在」。
        drop(encoder);
        drop(last_texture);
        print_mem("encoder dropped (source kept)");

        drop(source);
        print_mem("source dropped");

        if args.hold_s > 0 {
            std::thread::sleep(Duration::from_secs(args.hold_s));
            print_mem("after hold (driver release)");
        }

        println!(
            "\n== 汇总 ==\n  帧数: {frames}  编码字节合计: {encoded_bytes}  最终峰值 ws: {:.1} MB",
            mb(read_mem().peak_working_set)
        );
        Ok(())
    }
}
