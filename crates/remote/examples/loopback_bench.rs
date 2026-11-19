//! 本机回环压测：**同一进程**里跑真实被控端引擎 + 控制器引擎，走真实数据面。
//!
//! 目的：在单机上复现真机日志（`lt3/att1.log` / `att2.log`）里的那条链路——
//! **DXGI Desktop Duplication 采集 → Intel QSV Media Foundation H.264 硬编 → 会话帧 →
//! MSH264DecoderMFT + DXVA 解码**——并**自采**指标（不解析日志），从而量化「越改越卡」
//! 的回归与修复。
//!
//! # 与真机的差异（**必须知道**）
//!
//! - 传输用一对 `tokio::io::duplex` + 每方向固定延迟（`--delay-ms`，默认 8 ms，即 RTT≈16 ms），
//!   **不是** RustDesk UDP 打洞可靠层：没有丢包/乱序/重传，也**没有带宽上限**。
//! - 两端在**同一进程**、同一个 `clock::now_us()` 时钟起点上，故端到端延迟可以直接
//!   `now_us - capture_ts_us`，不需要跨端时钟偏差估计（真机两者都有误差）。
//! - 不注入真实输入（`NullInputSink`）、不做文件传输、没有服务端控制通道与会合服务器。
//! - 会话建立直接把 `BoxDuplex` 交给 `run_controlled` / `run_controller`（与 crate 自带
//!   `integration_tests` 同一条路径）。因此**握手 / HMAC 鉴权 / X25519 + ChaCha20-Poly1305
//!   加密 / 编解码协商 / 补丁模式 / QoS 闭环全部是真代码**，跳过的只有 `RemoteManager`
//!   到传输建立那一层（信令、中继、打洞）。
//!
//! # 负载
//!
//! 主显示器上创建 1280×720 置顶窗口，用 GDI 以 30 Hz 重绘「滚动文字 + 移动色块」。
//! 窗口必须真的可见（DXGI 采的是合成后的桌面）。除此之外桌面应保持静止，否则其它窗口的
//! 动画会混进指标。
//!
//! # 跑法
//!
//! ```text
//! export PATH="/c/msys64/mingw64/bin:$PATH"
//! cargo build -p rust-tunnel-remote --example loopback_bench --target x86_64-pc-windows-gnu
//! ./target/x86_64-pc-windows-gnu/debug/examples/loopback_bench.exe --seconds 60 --delay-ms 8
//! ```
//!
//! # 采集后端（`--capture`）
//!
//! 默认 `--capture dxgi`：强制 DXGI Desktop Duplication，**不试 WGC**，与真机日志一致。
//! 但 DXGI Desktop Duplication 在某些机器/显示拓扑上**能打开却不交付任何帧**
//! （`AcquireNextFrame` 一直 `WAIT_TIMEOUT`；2026-11-18 本机 4K@RTX 5070 Ti 上实测如此，
//! 同期 `win-video-capture-probe` 的「强制 DXGI」也复现）。此时回路跑不起来，用
//! `--capture wgc` 显式换到 WGC 的 **CPU 回读**路径：编码器（Intel QSV MF H.264）与
//! 解码器（MSH264DecoderMFT/DXVA）、会话引擎、链路、QoS、补丁判定逻辑都不变，但
//! **采集耗时会明显变高、且补丁模式关闭**（`supports_patches() == false`）——输出里会
//! 用 ⚠ 标注，汇总里也会写出实际采集后端，别把它的数字当成「DXGI 模式」的读数。
//! `--display N` 可以把窗口和采集一起换到第 N 台显示器。

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::too_many_lines,
    reason = "loopback_bench 是本机压测工具（等价于测试替身），不是生产代码；\
              失败时应立即带着上下文崩溃，尺寸/计数到 f64/u32 的转换都在已知安全范围内"
)]

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "loopback_bench 只能在 Windows 上运行：需要 DXGI Desktop Duplication + \
         Media Foundation 硬件 H.264 编解码"
    );
}

#[cfg(windows)]
fn main() {
    bench::run();
}

#[cfg(windows)]
mod bench {
    use std::any::Any;
    use std::collections::HashMap;
    use std::io;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};
    use std::task::{Context, Poll};
    use std::time::{Duration, Instant};

    use rust_tunnel_common::protocol::RemoteMode;
    use rust_tunnel_remote::auth::{PendingSession, PendingSessions};
    use rust_tunnel_remote::clock::now_us;
    use rust_tunnel_remote::codec::{
        DecodeTimings, VideoDecoderApi, VideoDecoderFactory, VideoEncoderApi, VideoEncoderFactory,
    };
    use rust_tunnel_remote::controlled::{
        run_controlled, ControlledConfig, NullInputSink, CAPTURE_WATCHDOG, UNCHANGED_RESEND_INTERVAL,
    };
    use rust_tunnel_remote::controller::{run_controller, ControllerConfig};
    use rust_tunnel_remote::screen::{
        CaptureInfo, CapturedPatches, DecodedFrame, DecodedPatch, DecoderInfo, DisplayInfo,
        EncoderInfo, EncodeOptions, FrameSink, GpuFrame, RawFrame, ScreenPatch, ScreenSource,
    };
    use rust_tunnel_remote::session::plan_controller_handshake;
    use rust_tunnel_remote::stats::{KeyframeReason, SessionStats, SessionStatsReporter};
    use rust_tunnel_remote::transfer::TransferHub;
    use rust_tunnel_remote::transport::BoxDuplex;
    use rust_tunnel_remote::video::{encode_dimensions, MAX_ENCODE_WIDTH};
    use rust_tunnel_remote::{
        CodecId, DecodedNv12, DecodedVideoFrame, EncodedVideo, RemoteError, RemoteResult,
    };
    use rust_tunnel_win_video::capture::{self, CaptureSource};
    use rust_tunnel_win_video::d3d11;
    use rust_tunnel_win_video::decode::{
        DecodePhaseTimings, H264DecodePath, H264Decoder, H264HardwarePreference, H264OutputMode,
    };
    use rust_tunnel_win_video::mft_encoder::MftH264Encoder;
    use rust_tunnel_win_video::nvenc::EncodedPacket;
    use rust_tunnel_win_video::pixels::{bgra_to_rgba, scale_bgra_to_rgba, scale_rgba_box};
    use rust_tunnel_win_video::WinVideoError;

    use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::{
        BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateSolidBrush, DeleteDC,
        DeleteObject, EndPaint, FillRect, GetStockObject, InvalidateRect, SelectObject, SetBkMode,
        SetTextColor, TextOutW, UpdateWindow, DEFAULT_GUI_FONT, HBRUSH, HBITMAP, HDC, HGDIOBJ,
        PAINTSTRUCT, SRCCOPY, TRANSPARENT,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        AdjustWindowRect, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
        GetClientRect, LoadCursorW, MSG, PeekMessageW, PostMessageW, PostQuitMessage, RegisterClassW,
        SW_SHOW, SetForegroundWindow, ShowWindow, TranslateMessage, CS_OWNDC, IDC_ARROW, PM_REMOVE,
        WM_CLOSE, WM_CREATE, WM_DESTROY, WM_ERASEBKGND, WM_PAINT, WM_QUIT, WNDCLASSW,
        WS_EX_TOPMOST, WS_OVERLAPPEDWINDOW,
    };

    /// 压测窗口的客户区尺寸（题目要求 1280×720）。
    const WINDOW_W: i32 = 1280;
    /// 压测窗口的客户区高度。
    const WINDOW_H: i32 = 720;
    /// 负载动画节拍（Hz）。
    const ANIMATION_HZ: u64 = 30;
    /// 链路单方向缓冲（每个 `duplex` 端点）。
    const LINK_BUF: usize = 1024 * 1024;
    /// 延迟管道每次搬运的块大小。
    const PIPE_CHUNK: usize = 64 * 1024;
    /// 延迟管道的队列深度（块数）：满了就回压到写端，模拟链路背压。
    const PIPE_QUEUE: usize = 256;
    /// 预热时最多尝试几次 DXGI 首帧（每次内部等 `FIRST_FRAME_TIMEOUT` = 2 s）。
    const WARMUP_ATTEMPTS: u32 = 6;
    /// 两次预热尝试之间的间隔。
    const WARMUP_RETRY_INTERVAL: Duration = Duration::from_millis(300);
    /// 建窗后等动画跑起来再预热采集（DXGI 首帧依赖桌面真的发生过更新）。
    const WINDOW_SETTLE: Duration = Duration::from_millis(800);
    /// 会话 ID（同进程内唯一即可）。
    const SESSION_ID: &str = "loopback-bench-session";
    /// 一次性 token（两端共用；真机由控制通道下发，这里直接构造）。
    const TOKEN: &str = "loopback-bench-token";

    // ─────────────────────────── 参数与入口 ───────────────────────────

    /// 采集后端。真机日志走 DXGI，因此默认 DXGI；`--capture wgc` 是本机 DXGI
    /// Desktop Duplication 不可用时的**显式**退路（会在输出里高亮标注模式不同）。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum CaptureMode {
        /// DXGI Desktop Duplication（与真机日志一致）。
        Dxgi,
        /// Windows.Graphics.Capture（退路：CPU 回读路径，没有 GPU 零拷贝/补丁）。
        Wgc,
    }

    /// 命令行参数。
    struct Args {
        /// 会话持续秒数。
        seconds: u64,
        /// 每方向固定延迟（毫秒）。
        delay_ms: u64,
        /// 编码目标码率（kbps）。
        bitrate_kbps: u32,
        /// 编码/采集帧率上限。
        fps: u8,
        /// 采集后端。
        capture: CaptureMode,
        /// 采集/建窗的目标显示器序号（`None` = 主显示器）。
        display: Option<u32>,
        /// 是否安装 tracing 订阅器（默认关；排查丢帧原因时配合
        /// `RUST_TUNNEL_LATENCY_LOG=1` 打开，引擎会打 `controller_drop reason=...`）。
        log: bool,
    }

    impl Default for Args {
        fn default() -> Self {
            Self {
                seconds: 30,
                delay_ms: 8,
                bitrate_kbps: 10_000,
                fps: 30,
                capture: CaptureMode::Dxgi,
                display: None,
                log: false,
            }
        }
    }

    /// 解析 `--seconds N --delay-ms N --bitrate-kbps N --fps N --capture dxgi|wgc --display N`。
    fn parse_args() -> Args {
        let mut args = Args::default();
        let mut rest = std::env::args().skip(1);
        while let Some(flag) = rest.next() {
            let mut next_number = || {
                rest.next()
                    .unwrap_or_else(|| panic!("{flag} 缺少取值"))
                    .parse::<u64>()
                    .unwrap_or_else(|err| panic!("{flag} 取值非法: {err}"))
            };
            match flag.as_str() {
                "--seconds" => args.seconds = next_number().max(1),
                "--delay-ms" => args.delay_ms = next_number(),
                "--bitrate-kbps" => {
                    args.bitrate_kbps = u32::try_from(next_number()).unwrap_or(10_000);
                }
                "--fps" => args.fps = u8::try_from(next_number().clamp(1, 120)).unwrap_or(30),
                "--display" => {
                    args.display = Some(u32::try_from(next_number()).unwrap_or(0));
                }
                "--log" => args.log = true,
                "--capture" => {
                    let value = rest.next().unwrap_or_else(|| panic!("{flag} 缺少取值"));
                    args.capture = match value.as_str() {
                        "dxgi" => CaptureMode::Dxgi,
                        "wgc" => CaptureMode::Wgc,
                        other => panic!("--capture 只接受 dxgi / wgc，实得 {other}"),
                    };
                }
                "--help" | "-h" => {
                    println!(
                        "loopback_bench [--seconds N] [--delay-ms N] [--bitrate-kbps N] [--fps N] \
                         [--capture dxgi|wgc] [--display N] [--log]"
                    );
                    std::process::exit(0);
                }
                other => {
                    eprintln!("未知参数 {other}（--help 查看用法）");
                    std::process::exit(2);
                }
            }
        }
        args
    }

    /// 进程入口：建窗 → 起引擎 → 打印 → 收尾。
    pub fn run() {
        let args = parse_args();
        // 排查丢帧原因时打开（`--log` + `RUST_TUNNEL_LATENCY_LOG=1` 会打出
        // `controller_drop reason=awaiting|stale|...` 与 `controller seq=...` 明细）。
        if args.log {
            rust_tunnel_common::logging::init_logging_with_level("info");
        }
        print_build_banner(&args);
        // DPI 感知必须在建窗与枚举显示器之前声明，否则 150% 缩放的 4K 屏会被报成 2560×1440。
        // SAFETY: 无指针参数；失败只意味着已设置过，沿用当前感知级别。
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        // 与 GUI 一致：定时器粒度降到 1 ms，让 30 Hz 动画节拍稳定（默认 15.6 ms 粒度会把
        // 33 ms 的 sleep 放大成 31/47 ms 两档）。
        let timer_period = unsafe { windows_sys::Win32::Media::timeBeginPeriod(1) };
        if timer_period != 0 {
            eprintln!("timeBeginPeriod(1) 失败（{timer_period}），动画节拍可能抖动");
        }

        let monitors =
            capture::enumerate_monitors().unwrap_or_else(|err| panic!("枚举显示器失败: {err}"));
        let requested = args
            .display
            .and_then(|id| {
                monitors
                    .iter()
                    .find(|monitor| u32::try_from(monitor.index) == Ok(id))
            })
            .or_else(|| monitors.iter().find(|monitor| monitor.is_primary))
            .unwrap_or_else(|| &monitors[0]);
        let display = DisplayInfo {
            id: u32::try_from(requested.index).unwrap_or(0),
            width: requested.width,
            height: requested.height,
            name: requested.device_name.clone(),
            primary: requested.is_primary,
        };
        let window_area = (
            requested.left,
            requested.top,
            i32::try_from(requested.width).unwrap_or(1920),
            i32::try_from(requested.height).unwrap_or(1080),
        );
        println!(
            "显示器: #{} {} {}x{}{} · 窗口 {}x{} @ {} Hz",
            display.id,
            display.name,
            display.width,
            display.height,
            if display.primary { " (主)" } else { "" },
            WINDOW_W,
            WINDOW_H,
            ANIMATION_HZ
        );
        println!(
            "采集后端: {}{}",
            match args.capture {
                CaptureMode::Dxgi => "DXGI Desktop Duplication",
                CaptureMode::Wgc => "WGC（显式退路）",
            },
            if args.capture == CaptureMode::Wgc {
                "  ⚠ 与真机日志的 DXGI 不同：CPU 回读、无 GPU 纹理输入、补丁模式关闭"
            } else {
                ""
            },
        );

        // 硬编/硬解先探测：缺失就直接失败——不能悄悄换成 VP9/软件编码，那会丢掉「模式一致」。
        probe_codecs();

        let mut window =
            spawn_window(window_area).unwrap_or_else(|err| panic!("创建负载窗口失败: {err}"));
        // 让动画先真的跑起来：DXGI Desktop Duplication 的首帧依赖桌面发生过更新，
        // 静止桌面下 `AcquireNextFrame` 会一直超时（本机实测）。
        std::thread::sleep(WINDOW_SETTLE);
        println!(
            "负载窗口已运行 {WINDOW_SETTLE:?}，动画 tick={}",
            ANIMATION_TICKS.load(Ordering::Relaxed)
        );

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(8)
            .enable_all()
            .build()
            .expect("创建 tokio 运行时");
        let summary = runtime.block_on(run_session(&args, &display));

        window.stop();
        unsafe {
            let _ = windows_sys::Win32::Media::timeEndPeriod(1);
        }
        match summary {
            Ok(accumulator) => print_summary(&args, &display, &accumulator),
            Err(err) => {
                eprintln!("压测失败: {err}");
                std::process::exit(1);
            }
        }
    }

    /// 打印版本/编译时间/参数，便于「修复前后」逐字对比。
    fn print_build_banner(args: &Args) {
        let head = std::process::Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map_or_else(
                || "unknown".to_owned(),
                |output| String::from_utf8_lossy(&output.stdout).trim().to_owned(),
            );
        let built = std::env::current_exe()
            .ok()
            .and_then(|path| std::fs::metadata(path).ok())
            .and_then(|meta| meta.modified().ok())
            .map_or_else(|| "unknown".to_owned(), |time| format!("{time:?}"));
        println!("=== rust-tunnel loopback_bench ===");
        println!(
            "crate {} v{} · git {head} · 可执行文件编译时间 {built}",
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION"),
        );
        println!(
            "参数: seconds={} delay={}ms(单向, RTT≈{}ms) bitrate={}kbps fps={} capture={:?} display={}",
            args.seconds,
            args.delay_ms,
            args.delay_ms.saturating_mul(2),
            args.bitrate_kbps,
            args.fps,
            args.capture,
            args.display.map_or_else(|| "primary".to_owned(), |id| id.to_string()),
        );
        println!("本机回环：无丢包/乱序/带宽上限，e2e 用同进程时钟直减（详见文件头注释）");
    }

    /// 在专用线程里探测硬件 MFT 编码器与 MF H.264 解码器（COM 套间只初始化在那条一次性线程）。
    ///
    /// `JoinHandle::join` 的错误是 `Box<dyn Any>`（不含可读信息），所以 panic 分支只能写死原因。
    #[allow(
        clippy::match_wild_err_arm,
        reason = "线程 panic 的载荷是 Box<dyn Any>，没有可实现 Debug/Display 的错误可读"
    )]
    fn probe_codecs() {
        let encoder = std::thread::spawn(|| MftH264Encoder::new(1280, 720, 30, 2000).map(|_| ()));
        match encoder.join() {
            Ok(Ok(())) => println!("硬件 H.264 编码 MFT: 可用"),
            Ok(Err(err)) => {
                panic!("没有可用的硬件 H.264 编码 MFT（需要 Intel QSV/AMD VCE）: {err}");
            }
            Err(_) => panic!("编码器探测线程 panic"),
        }
        let decoder = std::thread::spawn(|| H264Decoder::new_nv12(64, 64).map(|_| ()));
        match decoder.join() {
            Ok(Ok(())) => println!("Media Foundation H.264 解码器: 可用"),
            Ok(Err(err)) => panic!("MSH264DecoderMFT 不可用: {err}"),
            Err(_) => panic!("解码器探测线程 panic"),
        }
    }

    // ─────────────────────────── 指标容器 ───────────────────────────

    /// 会一直增长到被取走的样本直方图（单位由调用方约定）。
    #[derive(Debug, Default)]
    struct Histogram {
        /// 样本。
        values: Mutex<Vec<u32>>,
    }

    impl Histogram {
        /// 追加一个样本。
        fn record(&self, value: u32) {
            self.values
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(value);
        }

        /// 取走当前全部样本（每秒打印 / 收尾汇总时调用）。
        fn take(&self) -> Vec<u32> {
            let mut guard = self.values.lock().unwrap_or_else(PoisonError::into_inner);
            std::mem::take(&mut *guard)
        }
    }

    /// 控制器侧自采指标（sink + 解码适配器共同写入）。
    #[derive(Debug, Default)]
    struct CtrlMetrics {
        /// 解出并交给 sink 的帧数。
        frames: AtomicU64,
        /// 收到的补丁批数。
        patches: AtomicU64,
        /// 帧间隔 ≥100 ms 的次数。
        gaps_ge_100: AtomicU64,
        /// 帧间隔 ≥500 ms 的次数（BRIEF §4.1 的「0.5 s 冻结」）。
        gaps_ge_500: AtomicU64,
        /// 最大帧间隔（微秒）。
        max_gap_us: AtomicU64,
        /// 上一帧交给 sink 的时刻（`now_us`，0 = 还没收到）。
        last_frame_us: AtomicU64,
        /// 解码耗时 ≥100 ms 的帧数（解码停顿）。
        decode_stalls: AtomicU64,
        /// 端到端延迟（采集 → sink 回调，毫秒）。
        e2e_ms: Histogram,
        /// 解码耗时（毫秒）。
        decode_ms: Histogram,
        /// DXVA staging 回读耗时（毫秒；软件解码恒 0）。
        readback_ms: Histogram,
        /// 补丁端到端延迟（毫秒）。
        patch_e2e_ms: Histogram,
        /// 「采集 → 编码完成」分段（毫秒，诊断用）。
        capture_to_encode_ms: Histogram,
        /// 「编码完成 → sink 回调」分段（毫秒，诊断用）。
        encode_to_sink_ms: Histogram,
        /// 本秒的端到端样本。
        e2e_second: Histogram,
        /// 本秒的解码样本。
        decode_second: Histogram,
    }

    impl CtrlMetrics {
        /// 记一帧端到端 + 帧间隔。
        fn record_frame(&self, e2e_ms: u32) {
            self.frames.fetch_add(1, Ordering::Relaxed);
            self.e2e_ms.record(e2e_ms);
            self.e2e_second.record(e2e_ms);
            let now = now_us();
            let previous = self.last_frame_us.swap(now, Ordering::Relaxed);
            if previous != 0 {
                let gap = now.saturating_sub(previous);
                if gap >= 100_000 {
                    self.gaps_ge_100.fetch_add(1, Ordering::Relaxed);
                }
                if gap >= 500_000 {
                    self.gaps_ge_500.fetch_add(1, Ordering::Relaxed);
                }
                self.max_gap_us.fetch_max(gap, Ordering::Relaxed);
            }
        }

        /// 记一帧解码（毫秒）与 DXVA 回读分段。
        fn record_decode(&self, decode_ms: u32, readback_ms: u32) {
            self.decode_ms.record(decode_ms);
            self.decode_second.record(decode_ms);
            self.readback_ms.record(readback_ms);
            if decode_ms >= 100 {
                self.decode_stalls.fetch_add(1, Ordering::Relaxed);
            }
        }

        /// 记一批补丁的端到端延迟。
        fn record_patch(&self, e2e_ms: u32) {
            self.patches.fetch_add(1, Ordering::Relaxed);
            self.patch_e2e_ms.record(e2e_ms);
        }
    }

    /// 被控端侧自采指标（MFT 编码适配器写入）。
    #[derive(Debug, Default)]
    struct CtldMetrics {
        /// 编码调用次数（含空包）。
        encode_calls: AtomicU64,
        /// 真正产出码流的帧数。
        encoded_frames: AtomicU64,
        /// 关键帧（IDR）张数。
        keyframes: AtomicU64,
        /// 关键帧字节数。
        idr_bytes: AtomicU64,
        /// 全部出码字节数。
        encoded_bytes: AtomicU64,
        /// 单帧编码耗时（毫秒，含 CPU 缩放 / 转换 / MFT）。
        encode_ms: Histogram,
        /// 本秒的编码耗时样本。
        encode_second: Histogram,
        /// 每个**非空**出码包完成编码的时刻（`now_us`），下标 = 引擎分配的 `seq - 1`。
        ///
        /// 引擎的 `seq` 只对非空包递增（见 `controlled.rs` 的 `encode_loop`），所以这份
        /// 追加顺序与线上 `seq` 一一对应。控制器 sink 用它把端到端拆成
        /// 「采集 → 编码完成」与「编码完成 → sink」两段（只用于诊断，不上线）。
        encode_done_us: Mutex<Vec<u64>>,
    }

    impl CtldMetrics {
        /// 记一帧编码结果。
        fn record(&self, elapsed_ms: u32, keyframe: bool, bytes: usize) {
            self.encode_calls.fetch_add(1, Ordering::Relaxed);
            if bytes > 0 {
                self.encoded_frames.fetch_add(1, Ordering::Relaxed);
                self.encoded_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
                self.encode_done_us
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(now_us());
            }
            if keyframe {
                self.keyframes.fetch_add(1, Ordering::Relaxed);
                self.idr_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
            }
            self.encode_ms.record(elapsed_ms);
            self.encode_second.record(elapsed_ms);
        }
    }

    /// 链路写端统计（`poll_write` 被阻塞 = 对端/链路来不及消费）。
    #[derive(Debug, Default)]
    struct WriteStats {
        /// `poll_write` 成功返回的调用数。
        calls: AtomicU64,
        /// 成功写出的字节数。
        bytes: AtomicU64,
        /// 发生阻塞（`Pending` → 之后成功）的次数。
        stalls: AtomicU64,
        /// 阻塞累计耗时（微秒）。
        stall_us: AtomicU64,
        /// 单次阻塞最大值（微秒）。
        max_stall_us: AtomicU64,
        /// 每次阻塞的耗时（微秒，用于 p95）。
        stall_samples: Histogram,
    }

    impl WriteStats {
        /// 记一次成功写。
        fn record_write(&self, bytes: usize) {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
        }

        /// 记一次阻塞。
        fn record_stall(&self, us: u64) {
            self.stalls.fetch_add(1, Ordering::Relaxed);
            self.stall_us.fetch_add(us, Ordering::Relaxed);
            self.max_stall_us.fetch_max(us, Ordering::Relaxed);
            self.stall_samples
                .record(u32::try_from(us).unwrap_or(u32::MAX));
        }
    }

    // ─────────────────────────── 采集源（强制 DXGI） ───────────────────────────

    /// 进程级编码器设备句柄：采集成功后写入，MFT 工厂据此复用同一块 D3D11 设备（零拷贝）。
    static GPU_DEVICE: Mutex<Option<Arc<d3d11::D3d11>>> = Mutex::new(None);

    /// [`GpuFrame`] 的压测载体（一张采集纹理）。
    struct BenchGpuFrame {
        /// 采集纹理（持有池槽位租约）。纹理自带的宽高就是物理尺寸。
        texture: capture::CapturedTexture,
    }

    impl GpuFrame for BenchGpuFrame {
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    /// 采集源：默认强制 DXGI Desktop Duplication（与真机日志同一后端，**不试 WGC**）；
    /// `--capture wgc` 时显式改用 WGC（CPU 回读路径）。
    struct BenchScreen {
        /// 采集后端（会话内固定）。
        mode: CaptureMode,
        /// 每个显示器 id 对应的采集源。
        sources: Mutex<HashMap<u32, Box<dyn CaptureSource>>>,
        /// 本会话编码宽度上限（引擎在协商后设定）。
        max_encode_width: AtomicU32,
        /// 最近一次采集的 `(后端, 适配器名)`（**独立小锁**，避免采集期间阻塞遥测读取）。
        identity: Mutex<(String, String)>,
        /// 最近一次采集是否真的走了 GPU 纹理。
        last_capture_was_gpu: AtomicBool,
        /// 采集失败次数。
        capture_failures: AtomicU64,
        /// 「无新帧」累计次数。
        no_new_frame: AtomicU64,
    }

    impl BenchScreen {
        /// 构造（不预打开任何设备）。
        fn new(mode: CaptureMode) -> Self {
            Self {
                mode,
                sources: Mutex::new(HashMap::new()),
                max_encode_width: AtomicU32::new(MAX_ENCODE_WIDTH),
                identity: Mutex::new((String::new(), String::new())),
                last_capture_was_gpu: AtomicBool::new(false),
                capture_failures: AtomicU64::new(0),
                no_new_frame: AtomicU64::new(0),
            }
        }

        /// 预热：打开指定显示器的采集源并采到第一帧。
        ///
        /// **必须容忍首帧超时**：DXGI Desktop Duplication 只有在创建之后桌面**发生过更新**时
        /// 才会交付首帧；本机实测静止桌面下 `AcquireNextFrame` 会一直 `WAIT_TIMEOUT`。
        /// 因此这里在动画窗口跑起来之后重试（引擎的采集循环本身也会逐拍重试），
        /// 只有连续多次拿不到才报错。
        fn warmup(&self, display: u32) -> Result<(), String> {
            let backend = match self.mode {
                CaptureMode::Dxgi => "DXGI",
                CaptureMode::Wgc => "WGC",
            };
            let mut source = open_source(display, self.mode).map_err(|err| err.to_string())?;
            let adapter = source
                .device()
                .map_or_else(String::new, |device| device.adapter.name.clone());
            *self
                .identity
                .lock()
                .unwrap_or_else(PoisonError::into_inner) =
                (source.backend().to_string(), adapter.clone());
            let mut first = None;
            for attempt in 1..=WARMUP_ATTEMPTS {
                match source.capture() {
                    Ok(frame) => {
                        first = Some(frame);
                        break;
                    }
                    Err(err) => {
                        if attempt == WARMUP_ATTEMPTS {
                            return Err(format!(
                                "{backend} 预热采集连续 {WARMUP_ATTEMPTS} 次失败: {err}"
                            ));
                        }
                        println!("{backend} 首帧等待中（第 {attempt} 次超时: {err}）");
                        std::thread::sleep(WARMUP_RETRY_INTERVAL);
                    }
                }
            }
            let Some(frame) = first else {
                return Err(format!("{backend} 预热采集没有拿到帧"));
            };
            println!(
                "{backend} 采集源已打开: {}x{} 适配器={adapter}",
                frame.width, frame.height
            );
            self.sources
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(display, source);
            Ok(())
        }

        /// 在本显示器的采集源上执行一次操作（失败即丢弃缓存，下次重开）。
        fn with_source<R>(
            &self,
            display: u32,
            // 显式写 `dyn CaptureSource + 'static`：不写时会被推断成 `dyn CaptureSource + 'a`，
            // 函数项 `CaptureSource::capture` 的 `for<'a>` 量化对不上（"FnOnce is not general enough"）。
            action: impl FnOnce(&mut (dyn CaptureSource + 'static)) -> Result<R, WinVideoError>,
        ) -> RemoteResult<R> {
            let mut sources = self.sources.lock().unwrap_or_else(PoisonError::into_inner);
            if let std::collections::hash_map::Entry::Vacant(slot) = sources.entry(display) {
                let source = open_source(display, self.mode).map_err(|err| {
                    RemoteError::Platform(format!("打开采集源失败: {err}"))
                })?;
                let adapter = source
                    .device()
                    .map_or_else(String::new, |device| device.adapter.name.clone());
                *self
                    .identity
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) =
                    (source.backend().to_string(), adapter);
                slot.insert(source);
            }
            let Some(source) = sources.get_mut(&display) else {
                return Err(RemoteError::Platform("采集源缓存丢失".to_owned()));
            };
            let no_new_before = source.stats().no_new_frame;
            let result = action(source.as_mut());
            let no_new_after = source.stats().no_new_frame;
            if no_new_after > no_new_before {
                self.no_new_frame
                    .fetch_add(no_new_after - no_new_before, Ordering::Relaxed);
            }
            match result {
                Ok(value) => Ok(value),
                Err(err) => {
                    sources.remove(&display);
                    self.capture_failures.fetch_add(1, Ordering::Relaxed);
                    Err(RemoteError::Platform(format!("采集失败: {err}")))
                }
            }
        }

        /// 本会话编码宽度上限。
        fn max_width(&self) -> u32 {
            self.max_encode_width.load(Ordering::Relaxed)
        }
    }

    /// 枚举显示器并按指定后端打开对应 id（找不到时退回第一台）。
    fn open_source(
        display: u32,
        mode: CaptureMode,
    ) -> Result<Box<dyn CaptureSource>, WinVideoError> {
        let monitors = capture::enumerate_monitors()?;
        let monitor = monitors
            .iter()
            .find(|monitor| u32::try_from(monitor.index) == Ok(display))
            .or_else(|| monitors.first())
            .ok_or_else(|| WinVideoError::Config("没有可用的显示器".to_owned()))?;
        match mode {
            CaptureMode::Dxgi => capture::open_dxgi(monitor),
            CaptureMode::Wgc => capture::open_wgc(monitor),
        }
    }

    impl ScreenSource for BenchScreen {
        fn displays(&self) -> Vec<DisplayInfo> {
            capture::enumerate_monitors().map_or_else(
                |_| Vec::new(),
                |monitors| {
                    monitors
                        .into_iter()
                        .map(|monitor| DisplayInfo {
                            id: u32::try_from(monitor.index).unwrap_or(0),
                            width: monitor.width,
                            height: monitor.height,
                            name: monitor.device_name,
                            primary: monitor.is_primary,
                        })
                        .collect()
                },
            )
        }

        fn capture(&self, display: u32) -> RemoteResult<RawFrame> {
            self.last_capture_was_gpu.store(false, Ordering::Relaxed);
            let captured = self.with_source(display, CaptureSource::capture)?;
            let (width, height) =
                encode_dimensions(captured.width, captured.height, 1.0, self.max_width());
            let rgba = if (captured.width, captured.height) == (width, height) {
                bgra_to_rgba(&captured.bgra)
            } else {
                scale_bgra_to_rgba(&captured.bgra, captured.width, captured.height, width, height)
                    .map_err(|err| RemoteError::Platform(format!("采集下采样失败: {err}")))?
            };
            Ok(RawFrame {
                display,
                width,
                height,
                rgba,
                gpu: None,
                changed: true,
                changed_permille: 1000,
            })
        }

        fn set_max_encode_width(&self, width: u32) {
            self.max_encode_width.store(width, Ordering::Relaxed);
        }

        fn supports_gpu_capture(&self) -> bool {
            // WGC 的 GPU 纹理路径在本机实测拿不到首帧（10 ms 内没有新帧），且 `--capture wgc`
            // 本来就是「CPU 回读」退路：明确只走 CPU 采集，避免每拍失败后反复重开采集源。
            self.mode == CaptureMode::Dxgi
        }

        fn capture_gpu(&self, display: u32) -> RemoteResult<RawFrame> {
            let (captured, report) =
                self.with_source(display, CaptureSource::capture_texture_with_change)?;
            self.last_capture_was_gpu.store(true, Ordering::Relaxed);
            let (source_width, source_height) = (captured.width, captured.height);
            *GPU_DEVICE.lock().unwrap_or_else(PoisonError::into_inner) =
                Some(Arc::clone(&captured.device));
            let (width, height) =
                encode_dimensions(source_width, source_height, 1.0, self.max_width());
            Ok(RawFrame {
                display,
                width,
                height,
                rgba: Vec::new(),
                gpu: Some(Arc::new(BenchGpuFrame { texture: captured })),
                changed: report.changed,
                changed_permille: report.changed_permille,
            })
        }

        fn supports_patches(&self) -> bool {
            // 补丁模式建立在 GPU 纹理 + 变化网格上，CPU 回读路径不启用。
            self.mode == CaptureMode::Dxgi
        }

        fn capture_patches(
            &self,
            display: u32,
            scale: f32,
            max_width: u32,
        ) -> RemoteResult<Option<CapturedPatches>> {
            let collected = self.with_source(display, CaptureSource::capture_texture_with_dirty);
            let (captured, report, grid) = match collected {
                Ok(value) => value,
                Err(err) => {
                    eprintln!("补丁采集失败，回退全帧: {err}");
                    return Ok(None);
                }
            };
            self.last_capture_was_gpu.store(true, Ordering::Relaxed);
            *GPU_DEVICE.lock().unwrap_or_else(PoisonError::into_inner) =
                Some(Arc::clone(&captured.device));
            // 与 client-gui 同一套双重换算：先按会话上限封顶，再施加 QoS 的 scale。
            let (cap_width, cap_height) =
                encode_dimensions(captured.width, captured.height, 1.0, max_width);
            let (width, height) = encode_dimensions(cap_width, cap_height, scale, max_width);
            let Some(grid) = grid else {
                if report.changed {
                    return Ok(None);
                }
                return Ok(Some(CapturedPatches {
                    display,
                    width,
                    height,
                    changed_permille: 0,
                    rects: Vec::new(),
                }));
            };
            let params = capture::PatchParams::default();
            let Ok(Some(rects)) =
                capture::extract_patches(&captured, &grid, width, height, &params)
            else {
                return Ok(None);
            };
            let rects = rects
                .into_iter()
                .map(|patch| ScreenPatch {
                    x: patch.x,
                    y: patch.y,
                    width: patch.width,
                    height: patch.height,
                    nv12: patch.nv12,
                })
                .collect();
            Ok(Some(CapturedPatches {
                display,
                width,
                height,
                changed_permille: report.changed_permille,
                rects,
            }))
        }

        fn capture_info(&self) -> CaptureInfo {
            let (backend, adapter) = self
                .identity
                .lock()
                .map_or_else(|err| err.into_inner().clone(), |slot| slot.clone());
            CaptureInfo {
                backend,
                adapter,
                gpu_capture: self.last_capture_was_gpu.load(Ordering::Relaxed),
                gpu_disabled: false,
                gpu_disabled_reason: String::new(),
                fallbacks: 0,
                no_new_frame: self.no_new_frame.load(Ordering::Relaxed),
                open_fallbacks: 0,
            }
        }
    }

    // ─────────────────────────── 编码器（只走 Intel QSV MFT） ───────────────────────────

    /// 只产出硬件 MFT H.264 的工厂（**不碰 NVENC**，与真机日志的编码后端一致）。
    #[derive(Debug)]
    struct MftOnlyFactory {
        /// 自采指标（与主线程共享）。
        metrics: Arc<CtldMetrics>,
    }

    impl VideoEncoderFactory for MftOnlyFactory {
        fn codec(&self) -> CodecId {
            CodecId::H264
        }

        fn create(
            &self,
            width: u32,
            height: u32,
            target_bitrate_kbps: u32,
            max_fps: u8,
        ) -> RemoteResult<Box<dyn VideoEncoderApi>> {
            let fps = u32::from(max_fps.max(1));
            let device = GPU_DEVICE
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
                .filter(|device| device.adapter.vendor_id == d3d11::VENDOR_INTEL);
            let (encoder, gpu) = if let Some(device) = device {
                let adapter = device.adapter.name.clone();
                match MftH264Encoder::new_with_device(
                    Arc::clone(&device),
                    width,
                    height,
                    fps,
                    target_bitrate_kbps,
                ) {
                    Ok(encoder) => {
                        println!(
                            "MFT 编码器复用采集 D3D11 设备（GPU 零拷贝） {width}x{height} {fps}fps \
                             {target_bitrate_kbps}kbps adapter={adapter}"
                        );
                        (encoder, true)
                    }
                    Err(err) => {
                        println!("MFT 复用采集设备失败（回退 CPU 输入）: {err}");
                        let encoder = MftH264Encoder::new(width, height, fps, target_bitrate_kbps)
                            .map_err(|err| {
                                RemoteError::Encode(format!("MFT 创建失败: {err}"))
                            })?;
                        (encoder, false)
                    }
                }
            } else {
                let encoder = MftH264Encoder::new(width, height, fps, target_bitrate_kbps)
                    .map_err(|err| RemoteError::Encode(format!("MFT 创建失败: {err}")))?;
                (encoder, false)
            };
            println!(
                "编码器已就绪: {} adapter={} 尺寸={width}x{height} gpu_input={gpu}",
                encoder.mft_name(),
                encoder.adapter_name(),
            );
            Ok(Box::new(MftAdapter {
                encoder,
                fps,
                gpu,
                gpu_failed: false,
                metrics: Arc::clone(&self.metrics),
            }))
        }
    }

    /// [`MftH264Encoder`] 的 remote 适配器（记录每帧耗时与 IDR 字节）。
    struct MftAdapter {
        /// 硬件编码器。
        encoder: MftH264Encoder,
        /// 逻辑帧率（Intel MFT 不支持在线改帧率，实际节奏由采集控制）。
        fps: u32,
        /// 编码器是否复用采集设备（`encode_texture` 可用）。
        gpu: bool,
        /// GPU 纹理编码在本会话失败过一次：之后走 CPU 回读。
        gpu_failed: bool,
        /// 自采指标。
        metrics: Arc<CtldMetrics>,
    }

    /// 从 MFT 友好名称推断后端名（遥测用）。
    fn mft_backend_name(name: &str) -> &'static str {
        let lowered = name.to_lowercase();
        if lowered.contains("intel") || lowered.contains("quick sync") {
            "Intel QSV"
        } else if lowered.contains("amd") {
            "AMD VCE"
        } else {
            "MFT"
        }
    }

    impl MftAdapter {
        /// 编码 RGBA（尺寸不同时先盒式下采样）。
        fn encode_rgba_packet(
            &mut self,
            rgba: &[u8],
            src_width: u32,
            src_height: u32,
            force_keyframe: bool,
        ) -> RemoteResult<EncodedPacket> {
            let (dst_width, dst_height) = self.encoder.dimensions();
            if (src_width, src_height) == (dst_width, dst_height) {
                self.encoder
                    .encode_rgba(rgba, force_keyframe)
                    .map_err(|err| RemoteError::Encode(format!("MFT 编码失败: {err}")))
            } else {
                let scaled = scale_rgba_box(rgba, src_width, src_height, dst_width, dst_height)
                    .map_err(|err| RemoteError::Encode(format!("输入下采样失败: {err}")))?;
                self.encoder
                    .encode_rgba(&scaled, force_keyframe)
                    .map_err(|err| RemoteError::Encode(format!("MFT 编码失败: {err}")))
            }
        }

        /// GPU 纹理路径失败时从纹理回读到 RGBA 再编码（绝不静默丢帧）。
        fn encode_from_readback(
            &mut self,
            texture: &capture::CapturedTexture,
            force_keyframe: bool,
        ) -> RemoteResult<EncodedPacket> {
            let (width, height, bgra) = texture
                .read_bgra()
                .map_err(|err| RemoteError::Encode(format!("GPU 回读失败: {err}")))?;
            let (dst_width, dst_height) = self.encoder.dimensions();
            let rgba = if (width, height) == (dst_width, dst_height) {
                bgra_to_rgba(&bgra)
            } else {
                scale_bgra_to_rgba(&bgra, width, height, dst_width, dst_height)
                    .map_err(|err| RemoteError::Encode(format!("回读下采样失败: {err}")))?
            };
            self.encoder
                .encode_rgba(&rgba, force_keyframe)
                .map_err(|err| RemoteError::Encode(format!("MFT 编码失败: {err}")))
        }

        /// 记一次编码结果（毫秒）。
        fn record(&self, started: Instant, packet: &EncodedPacket) {
            self.metrics.record(
                u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
                packet.keyframe,
                packet.data.len(),
            );
        }
    }

    impl VideoEncoderApi for MftAdapter {
        fn dimensions(&self) -> (u32, u32) {
            self.encoder.dimensions()
        }

        fn bitrate_kbps(&self) -> u32 {
            self.encoder.bitrate_kbps()
        }

        fn fps(&self) -> u32 {
            self.fps
        }

        fn set_bitrate(&mut self, kbps: u32) -> RemoteResult<()> {
            self.encoder
                .set_bitrate(kbps)
                .map_err(|err| RemoteError::Encode(format!("MFT 在线改码率失败: {err}")))
        }

        fn set_fps(&mut self, fps: u8) -> RemoteResult<()> {
            let fps = u32::from(fps.max(1));
            self.fps = fps;
            if let Err(err) = self.encoder.set_fps(fps) {
                eprintln!("MFT 不支持在线改帧率（保持内部帧率，采集节奏仍按 QoS）: {err}");
            }
            Ok(())
        }

        fn info(&self) -> EncoderInfo {
            EncoderInfo {
                backend: mft_backend_name(self.encoder.mft_name()).to_owned(),
                name: self.encoder.mft_name().to_owned(),
                adapter: self.encoder.adapter_name().to_owned(),
                gpu_input: self.gpu && !self.gpu_failed,
            }
        }

        fn encode(
            &mut self,
            rgba: &[u8],
            src_width: u32,
            src_height: u32,
            force_keyframe: bool,
        ) -> RemoteResult<EncodedVideo> {
            let started = Instant::now();
            let packet = self.encode_rgba_packet(rgba, src_width, src_height, force_keyframe)?;
            self.record(started, &packet);
            Ok(EncodedVideo {
                data: packet.data,
                keyframe: packet.keyframe,
            })
        }

        fn encode_frame(
            &mut self,
            frame: &RawFrame,
            force_keyframe: bool,
        ) -> RemoteResult<EncodedVideo> {
            let started = Instant::now();
            let Some(gpu) = frame.gpu.as_deref() else {
                let packet = self.encode_rgba_packet(
                    &frame.rgba,
                    frame.width,
                    frame.height,
                    force_keyframe,
                )?;
                self.record(started, &packet);
                return Ok(EncodedVideo {
                    data: packet.data,
                    keyframe: packet.keyframe,
                });
            };
            let gpu_frame = gpu
                .as_any()
                .downcast_ref::<BenchGpuFrame>()
                .ok_or_else(|| RemoteError::Encode("收到无法识别的 GPU 帧".to_owned()))?;
            let packet = if self.gpu && !self.gpu_failed {
                match self
                    .encoder
                    .encode_texture(&gpu_frame.texture, force_keyframe)
                {
                    Ok(packet) => packet,
                    Err(err) => {
                        self.gpu_failed = true;
                        eprintln!("GPU 纹理直通失败，本会话回退 CPU 回读: {err}");
                        self.encode_from_readback(&gpu_frame.texture, force_keyframe)?
                    }
                }
            } else {
                self.encode_from_readback(&gpu_frame.texture, force_keyframe)?
            };
            self.record(started, &packet);
            Ok(EncodedVideo {
                data: packet.data,
                keyframe: packet.keyframe,
            })
        }
    }

    // ─────────────────────────── 解码器（MF + DXVA） ───────────────────────────

    /// 带自采指标的解码器工厂。
    #[derive(Debug, Clone)]
    struct BenchDecoderFactory {
        /// 与 sink 共享的指标。
        metrics: Arc<CtrlMetrics>,
    }

    impl VideoDecoderFactory for BenchDecoderFactory {
        fn codec(&self) -> CodecId {
            CodecId::H264
        }

        fn create(&self) -> RemoteResult<Box<dyn VideoDecoderApi>> {
            Ok(Box::new(BenchDecoder {
                decoder: None,
                configured: None,
                metrics: Arc::clone(&self.metrics),
                info: DecoderInfo::default(),
            }))
        }
    }

    /// 惰性 MF 解码适配器（DXVA 优先，输出紧包 NV12）。
    struct BenchDecoder {
        /// 解码器实例。
        decoder: Option<H264Decoder>,
        /// 当前适配的尺寸。
        configured: Option<(u32, u32)>,
        /// 指标。
        metrics: Arc<CtrlMetrics>,
        /// 最近一次上报的后端信息。
        info: DecoderInfo,
    }

    impl VideoDecoderApi for BenchDecoder {
        fn configure(&mut self, width: u32, height: u32) -> RemoteResult<()> {
            if self.decoder.is_some() && self.configured == Some((width, height)) {
                return Ok(());
            }
            let decoder = H264Decoder::new_with_preference_and_output(
                width,
                height,
                H264HardwarePreference::Auto,
                H264OutputMode::Nv12,
            )
            .map_err(|err| RemoteError::Encode(format!("H.264 解码器初始化失败: {err}")))?;
            self.info = DecoderInfo {
                backend: match decoder.path() {
                    H264DecodePath::Dxva => "MF DXVA",
                    H264DecodePath::Software => "MF Software",
                }
                .to_owned(),
                name: "MSH264DecoderMFT".to_owned(),
                adapter: decoder.adapter_name().unwrap_or_default().to_owned(),
                hardware: decoder.path() == H264DecodePath::Dxva,
            };
            println!(
                "解码器已创建: {} {width}x{height} backend={} adapter={}",
                self.info.name, self.info.backend, self.info.adapter
            );
            self.decoder = Some(decoder);
            self.configured = Some((width, height));
            Ok(())
        }

        fn decode(&mut self, data: &[u8]) -> RemoteResult<Option<DecodedVideoFrame>> {
            let decoder = self
                .decoder
                .as_mut()
                .ok_or_else(|| RemoteError::Encode("H.264 解码器尚未 configure".to_owned()))?;
            let started = Instant::now();
            let frame = decoder
                .decode(data)
                .map_err(|err| RemoteError::Encode(format!("H.264 解码失败: {err}")))?;
            let elapsed_ms = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
            let timings = decoder.last_phase_timings();
            let Some(frame) = frame else {
                return Ok(None);
            };
            self.metrics.record_decode(
                elapsed_ms,
                u32::try_from(timings.readback_us / 1000).unwrap_or(u32::MAX),
            );
            let nv12 = frame.nv12.map(|nv12| DecodedNv12 {
                y: nv12.y,
                uv: nv12.uv,
            });
            Ok(Some(DecodedVideoFrame {
                width: frame.width,
                height: frame.height,
                rgba: frame.rgba,
                nv12,
            }))
        }

        fn info(&self) -> DecoderInfo {
            self.info.clone()
        }

        fn last_decode_timings(&self) -> DecodeTimings {
            let Some(decoder) = self.decoder.as_ref() else {
                return DecodeTimings::default();
            };
            let DecodePhaseTimings {
                submit_us,
                output_us,
                readback_us,
                color_us,
            } = decoder.last_phase_timings();
            DecodeTimings {
                submit_us,
                output_us,
                readback_us,
                color_us,
            }
        }
    }

    // ─────────────────────────── 画面 sink ───────────────────────────

    /// 控制器侧 sink：只记指标（不解码、不渲染）。
    struct BenchSink {
        /// 指标。
        metrics: Arc<CtrlMetrics>,
        /// 被控端编码完成时刻（按键拆分端到端用）。
        encode_timeline: Arc<CtldMetrics>,
        /// 会话关闭原因（收尾打印用）。
        closed: Mutex<Option<String>>,
    }

    impl BenchSink {
        /// 用 `seq` 找到该包的编码完成时刻，记两段分解。
        fn record_split(&self, seq: u64, capture_ts_us: u64, now: u64) {
            let index = usize::try_from(seq.saturating_sub(1)).ok();
            let encoded_at = index.and_then(|index| {
                self.encode_timeline
                    .encode_done_us
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get(index)
                    .copied()
            });
            let Some(encoded_at) = encoded_at else {
                return;
            };
            if capture_ts_us != 0 && encoded_at >= capture_ts_us {
                self.metrics.capture_to_encode_ms.record(
                    u32::try_from((encoded_at - capture_ts_us) / 1000).unwrap_or(u32::MAX),
                );
            }
            if now >= encoded_at {
                self.metrics
                    .encode_to_sink_ms
                    .record(u32::try_from((now - encoded_at) / 1000).unwrap_or(u32::MAX));
            }
        }
    }

    impl FrameSink for BenchSink {
        fn on_displays(&self, displays: Vec<DisplayInfo>) {
            println!(
                "控制器收到显示器列表: {}",
                displays
                    .iter()
                    .map(|display| format!("#{} {}x{}", display.id, display.width, display.height))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }

        fn on_frame(&self, frame: DecodedFrame) {
            // 两端同进程、同一个 `now_us()` 起点：直接相减就是采集 → 解码完成。
            let now = now_us();
            let e2e_ms = if frame.capture_ts_us == 0 {
                0
            } else {
                u32::try_from(now.saturating_sub(frame.capture_ts_us) / 1000).unwrap_or(u32::MAX)
            };
            self.record_split(frame.seq, frame.capture_ts_us, now);
            self.metrics.record_frame(e2e_ms);
        }

        fn on_patch(&self, patch: DecodedPatch) {
            let e2e_ms = if patch.capture_ts_us == 0 {
                0
            } else {
                u32::try_from(now_us().saturating_sub(patch.capture_ts_us) / 1000)
                    .unwrap_or(u32::MAX)
            };
            self.metrics.record_patch(e2e_ms);
        }

        fn on_closed(&self, reason: String) {
            *self.closed.lock().unwrap_or_else(PoisonError::into_inner) = Some(reason);
        }
    }

    // ─────────────────────────── 延迟链路 ───────────────────────────

    /// 一对端点 + 画面方向的写统计。
    struct LinkEnds {
        /// 控制器端。
        controller: BoxDuplex,
        /// 被控端。
        controlled: BoxDuplex,
        /// 被控端 → 控制器方向的写统计（画面方向）。
        video_write: Arc<WriteStats>,
    }

    /// 写端计时包装：`poll_write` 返回 `Pending` 的时长就是链路背压暴露给引擎的时间。
    struct TimedWriter<W> {
        /// 内层写端。
        inner: W,
        /// 统计。
        stats: Arc<WriteStats>,
        /// 首次进入 `Pending` 的时刻。
        pending_since: Option<Instant>,
    }

    impl<W> TimedWriter<W> {
        /// 包装一个写端。
        fn new(inner: W, stats: Arc<WriteStats>) -> Self {
            Self {
                inner,
                stats,
                pending_since: None,
            }
        }
    }

    impl<W: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for TimedWriter<W> {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            let started = Instant::now();
            let result = Pin::new(&mut this.inner).poll_write(cx, buf);
            match result {
                Poll::Pending => {
                    if this.pending_since.is_none() {
                        this.pending_since = Some(started);
                    }
                    Poll::Pending
                }
                Poll::Ready(Ok(written)) => {
                    if let Some(since) = this.pending_since.take() {
                        let us = u64::try_from(started.saturating_duration_since(since).as_micros())
                            .unwrap_or(u64::MAX);
                        this.stats.record_stall(us);
                    }
                    this.stats.record_write(written);
                    Poll::Ready(Ok(written))
                }
                Poll::Ready(Err(err)) => {
                    this.pending_since = None;
                    Poll::Ready(Err(err))
                }
            }
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_flush(cx)
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
        }
    }

    /// 建立带固定单向延迟的双向链路。
    fn delayed_link(delay: Duration) -> LinkEnds {
        // 控制器 → 被控端：控制器写 ctrl_write，管道从 peer 端读、延迟后写进 c2s_write。
        let (ctrl_write, ctrl_pipe_read) = tokio::io::duplex(LINK_BUF);
        let (c2s_write, ctld_read) = tokio::io::duplex(LINK_BUF);
        spawn_delay_pipe(ctrl_pipe_read, c2s_write, delay);
        // 被控端 → 控制器：同上，反向。
        let (ctld_write, ctld_pipe_read) = tokio::io::duplex(LINK_BUF);
        let (s2c_write, ctrl_read) = tokio::io::duplex(LINK_BUF);
        spawn_delay_pipe(ctld_pipe_read, s2c_write, delay);

        let video_write = Arc::new(WriteStats::default());
        LinkEnds {
            controller: Box::new(tokio::io::join(
                ctrl_read,
                TimedWriter::new(ctrl_write, Arc::new(WriteStats::default())),
            )),
            controlled: Box::new(tokio::io::join(
                ctld_read,
                TimedWriter::new(ctld_write, Arc::clone(&video_write)),
            )),
            video_write,
        }
    }

    /// 延迟管道：读 → 带截止时间的队列 → 睡到截止时间 → 写对端。
    ///
    /// 队列有界，对端不读时背压会一路传回写端；所有块的截止时间按入队顺序单调递增，
    /// 因此顺序天然保持（不需要重排缓冲）。
    fn spawn_delay_pipe(mut from: DuplexStream, mut to: DuplexStream, delay: Duration) {
        let (tx, mut rx) = mpsc::channel::<(tokio::time::Instant, Vec<u8>)>(PIPE_QUEUE);
        tokio::spawn(async move {
            let mut buf = vec![0u8; PIPE_CHUNK];
            loop {
                match from.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        let deadline = tokio::time::Instant::now() + delay;
                        if tx.send((deadline, buf[..read].to_vec())).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
        tokio::spawn(async move {
            while let Some((deadline, data)) = rx.recv().await {
                tokio::time::sleep_until(deadline).await;
                if to.write_all(&data).await.is_err() {
                    break;
                }
                let _ = to.flush().await;
            }
        });
    }

    // ─────────────────────────── 统计聚合与打印 ───────────────────────────

    /// 被控端引擎每秒上报的读数（`SessionStats::Encode` / `Host`）。
    #[derive(Default)]
    struct EngineStats {
        /// 最近一秒实测编码帧率。
        fps: u32,
        /// 累计编码帧数（每秒 fps 之和）。
        fps_sum: u64,
        /// 最近一秒单帧编码耗时（毫秒）。
        encode_ms: u32,
        /// 最近一秒单帧采集耗时（微秒）。
        capture_us: u32,
        /// 最近一秒实际出码速率。
        sent_kbps: u32,
        /// 当前生效目标码率。
        bitrate_kbps: u32,
        /// 累计跳过的帧（无变化）。
        skipped: u64,
        /// 累计关键帧（引擎口径）。
        keyframes: u64,
        /// 累计关键帧字节（引擎口径）。
        idr_bytes: u64,
        /// 累计因出站队列满而在编码前跳过的帧。
        out_queue_skips: u64,
        /// 累计编码器重建次数。
        encoder_rebuilds: u64,
        /// 累计强制 IDR 次数。
        forced_idrs: u64,
        /// 出站队列深度最大值。
        out_queue_depth_max: u32,
        /// 处于补丁模式的秒数。
        patch_seconds: u64,
        /// 收到 `Host` 的秒数。
        host_seconds: u64,
        /// 最近的变化面积（千分比）。
        changed_permille: u32,
        /// 编码器后端。
        encoder_backend: String,
        /// 编码器名称。
        encoder_name: String,
        /// 编码器适配器。
        encoder_adapter: String,
        /// 采集后端。
        capture_backend: String,
        /// 采集适配器。
        capture_adapter: String,
        /// 采集是否走 GPU。
        gpu_capture: bool,
        /// 编码器是否吃 GPU 纹理。
        gpu_encode: bool,
        /// 编码宽度。
        encode_width: u32,
        /// 编码高度。
        encode_height: u32,
        /// 显示宽度。
        display_width: u32,
        /// 显示高度。
        display_height: u32,
        /// 生效缩放（千分比）。
        scale_permille: u16,
        /// 内容活跃档。
        activity: String,
    }

    impl EngineStats {
        /// 吸收一条 `SessionStats`。
        fn absorb(&mut self, event: &SessionStats) {
            match event {
                SessionStats::Encode {
                    encode_ms,
                    capture_us,
                    fps,
                    bitrate_kbps,
                    skipped,
                    keyframes,
                    idr_bytes,
                    changed_permille,
                    ..
                } => {
                    self.encode_ms = *encode_ms;
                    self.capture_us = *capture_us;
                    self.fps = *fps;
                    self.fps_sum = self.fps_sum.saturating_add(u64::from(*fps));
                    self.bitrate_kbps = *bitrate_kbps;
                    self.skipped = self.skipped.saturating_add(u64::from(*skipped));
                    self.keyframes = self.keyframes.saturating_add(u64::from(*keyframes));
                    self.idr_bytes = self.idr_bytes.saturating_add(*idr_bytes);
                    self.changed_permille = *changed_permille;
                }
                SessionStats::Host(host) => {
                    self.host_seconds = self.host_seconds.saturating_add(1);
                    self.patch_seconds =
                        self.patch_seconds.saturating_add(u64::from(host.patch_mode));
                    self.out_queue_depth_max = self.out_queue_depth_max.max(host.out_queue_depth);
                    self.out_queue_skips = self
                        .out_queue_skips
                        .saturating_add(u64::from(host.out_queue_skips));
                    self.encoder_rebuilds = self
                        .encoder_rebuilds
                        .saturating_add(u64::from(host.encoder_rebuilds));
                    self.forced_idrs = self
                        .forced_idrs
                        .saturating_add(u64::from(host.forced_idrs));
                    self.sent_kbps = host.sent_kbps;
                    self.capture_us = host.capture_us;
                    self.encode_ms = host.encode_ms;
                    self.fps = host.fps;
                    self.bitrate_kbps = host.bitrate_kbps;
                    self.changed_permille = host.changed_permille;
                    self.encoder_backend.clone_from(&host.encoder_backend);
                    self.encoder_name.clone_from(&host.encoder_name);
                    self.encoder_adapter.clone_from(&host.encoder_adapter);
                    self.capture_backend.clone_from(&host.capture_backend);
                    self.capture_adapter.clone_from(&host.capture_adapter);
                    self.gpu_capture = host.gpu_capture;
                    self.gpu_encode = host.gpu_encode;
                    self.encode_width = host.encode_width;
                    self.encode_height = host.encode_height;
                    self.display_width = host.display_width;
                    self.display_height = host.display_height;
                    self.scale_permille = host.scale_permille;
                    self.activity = format!("{:?}", host.activity);
                }
                _ => {}
            }
        }
    }

    /// 控制器引擎每秒上报的读数。
    #[derive(Default)]
    struct ControllerEngineStats {
        /// 累计收包数。
        packets: u64,
        /// 累计丢包数。
        dropped: u64,
        /// 累计补丁批数（引擎口径）。
        patches: u64,
        /// 最近一帧解码耗时（引擎口径）。
        decode_ms: u32,
        /// 最近一帧端到端（引擎口径）。
        e2e_ms: u32,
        /// 解码队列积压。
        backlog: u32,
        /// 最近一次 RTT。
        rtt_ms: u32,
        /// 关键帧请求按原因累计。
        keyframe_requests: [u64; KeyframeReason::COUNT],
        /// 解码器信息。
        decoder: DecoderInfo,
        /// 协商到的 codec。
        codec: Option<CodecId>,
    }

    /// 一次压测的累积结果（打印机返回给 main 用于最终汇总）。
    #[derive(Default)]
    struct Accumulator {
        /// 会话秒数。
        seconds: u64,
        /// 端到端样本（毫秒）。
        e2e: Vec<u32>,
        /// 解码耗时样本（毫秒）。
        decode: Vec<u32>,
        /// DXVA 回读样本（毫秒）。
        readback: Vec<u32>,
        /// 补丁端到端样本（毫秒）。
        patch_e2e: Vec<u32>,
        /// 「采集 → 编码完成」样本（毫秒）。
        capture_to_encode: Vec<u32>,
        /// 「编码完成 → sink」样本（毫秒）。
        encode_to_sink: Vec<u32>,
        /// 每帧编码耗时样本（毫秒）。
        encode: Vec<u32>,
        /// 链路写阻塞样本（微秒）。
        write_stall_us: Vec<u32>,
        /// 帧间隔 ≥500 ms 的次数。
        gaps_ge_500: u64,
        /// 帧间隔 ≥100 ms 的次数。
        gaps_ge_100: u64,
        /// 最大帧间隔（微秒）。
        max_gap_us: u64,
        /// 自采编码帧数（有码流输出的调用）。
        encoded_frames: u64,
        /// 自采关键帧张数。
        encoder_keyframes: u64,
        /// 自采关键帧字节。
        encoder_idr_bytes: u64,
        /// 自采出码总字节。
        encoder_bytes: u64,
        /// 控制器引擎读数。
        controller: ControllerEngineStats,
        /// 被控端引擎读数。
        host: EngineStats,
        /// 会话关闭原因。
        closed: Option<String>,
        /// 会话任务提前结束的原因。
        session_errors: Vec<String>,
    }

    /// 计算分位（`percent` 为 0..=100，样本必须已排序）。
    fn percentile(sorted: &[u32], percent: usize) -> u32 {
        if sorted.is_empty() {
            return 0;
        }
        let index = (sorted.len() - 1).saturating_mul(percent) / 100;
        sorted[index]
    }

    /// 样本均值。
    fn mean(values: &[u32]) -> f64 {
        if values.is_empty() {
            return 0.0;
        }
        let sum: u64 = values.iter().map(|value| u64::from(*value)).sum();
        sum as f64 / values.len() as f64
    }

    /// 补丁模式时间占比（%）。
    fn patch_percent(host: &EngineStats) -> u32 {
        if host.host_seconds == 0 {
            return 0;
        }
        u32::try_from(host.patch_seconds.saturating_mul(100) / host.host_seconds).unwrap_or(0)
    }

    /// 每秒一行 + 收尾后返回累积结果。
    #[allow(
        clippy::too_many_arguments,
        reason = "打印机要同时读两侧指标、两侧统计通道与取消令牌；打包成结构体只是换个地方堆字段"
    )]
    fn spawn_printer(
        ctrl_metrics: Arc<CtrlMetrics>,
        ctld_metrics: Arc<CtldMetrics>,
        video_write: Arc<WriteStats>,
        mut ctrl_stats: mpsc::UnboundedReceiver<SessionStats>,
        mut ctld_stats: mpsc::UnboundedReceiver<SessionStats>,
        cancel: CancellationToken,
    ) -> tokio::task::JoinHandle<Accumulator> {
        tokio::spawn(async move {
            let mut accumulator = Accumulator::default();
            let started = Instant::now();
            // 首个 tick 从「现在 + 1s」开始：`interval` 的第一次 tick 会立刻完成，
            // 那会打出一条 0 秒、全零的噪声行。
            let mut tick = tokio::time::interval_at(
                tokio::time::Instant::now() + Duration::from_secs(1),
                Duration::from_secs(1),
            );
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut previous_packets = 0_u64;
            let mut previous_dropped = 0_u64;
            let mut previous_patches = 0_u64;
            let mut previous_frames = 0_u64;
            let mut previous_stall_us = 0_u64;
            let mut previous_keyframe_requests = 0_u64;
            let mut printed_identity = false;

            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    _ = tick.tick() => {}
                }
                accumulator.seconds = started.elapsed().as_secs();
                drain_engine_events(&mut accumulator, &mut ctrl_stats, &mut ctld_stats);
                // `Host.encoder_*` 来自编码任务的每秒快照，会话第一秒还是空的——
                // 等它填上再打印身份行（否则会打出一行 `编码= ()`）。
                if !printed_identity && !accumulator.host.encoder_backend.is_empty() {
                    printed_identity = true;
                    println!(
                        "被控端身份: 采集={} 适配器={} gpu_capture={} | 编码={} ({}) 适配器={} \
                         gpu_encode={} | 编码尺寸={}x{} 显示={}x{} 缩放={}‰ 活跃={}",
                        accumulator.host.capture_backend,
                        accumulator.host.capture_adapter,
                        accumulator.host.gpu_capture,
                        accumulator.host.encoder_backend,
                        accumulator.host.encoder_name,
                        accumulator.host.encoder_adapter,
                        accumulator.host.gpu_encode,
                        accumulator.host.encode_width,
                        accumulator.host.encode_height,
                        accumulator.host.display_width,
                        accumulator.host.display_height,
                        accumulator.host.scale_permille,
                        accumulator.host.activity,
                    );
                }

                // 本秒窗口样本。
                let e2e_second = ctrl_metrics.e2e_second.take();
                let decode_second = ctrl_metrics.decode_second.take();
                let encode_second = ctld_metrics.encode_second.take();
                let mut e2e_sorted = e2e_second.clone();
                e2e_sorted.sort_unstable();
                let mut decode_sorted = decode_second.clone();
                decode_sorted.sort_unstable();
                let mut encode_sorted = encode_second.clone();
                encode_sorted.sort_unstable();
                accumulator.e2e.extend_from_slice(&e2e_second);
                accumulator.decode.extend_from_slice(&decode_second);
                accumulator.encode.extend_from_slice(&encode_second);

                let frames = ctrl_metrics.frames.load(Ordering::Relaxed);
                let stall_us = video_write.stall_us.load(Ordering::Relaxed);
                let delta_frames = frames.saturating_sub(previous_frames);
                let delta_packets = accumulator.controller.packets.saturating_sub(previous_packets);
                let delta_dropped = accumulator.controller.dropped.saturating_sub(previous_dropped);
                let delta_patches = accumulator.controller.patches.saturating_sub(previous_patches);
                let delta_stall_us = stall_us.saturating_sub(previous_stall_us);
                let keyframe_requests: u64 = accumulator.controller.keyframe_requests.iter().sum();
                let delta_keyframe_requests =
                    keyframe_requests.saturating_sub(previous_keyframe_requests);
                previous_frames = frames;
                previous_packets = accumulator.controller.packets;
                previous_dropped = accumulator.controller.dropped;
                previous_patches = accumulator.controller.patches;
                previous_stall_us = stall_us;
                previous_keyframe_requests = keyframe_requests;

                println!(
                    "[{:>4}s] ctrl 帧 {delta_frames}/s e2e p50 {} p95 {} p99 {} max {} ms | \
                     dec p50 {} p95 {} max {} ms | 停顿 {} 收包 {delta_packets} 丢 {delta_dropped} \
                     补丁 {delta_patches} 关键帧请求 {delta_keyframe_requests} 积压 {} | ctld 编码 {} fps enc {:.1} ms \
                     cap {:.1} ms {}/{} kbps 补丁 {}% IDR {} 张 {} KB | 写阻塞 {:.1} ms/s(max {:.1})",
                    accumulator.seconds,
                    percentile(&e2e_sorted, 50),
                    percentile(&e2e_sorted, 95),
                    percentile(&e2e_sorted, 99),
                    e2e_sorted.last().copied().unwrap_or(0),
                    percentile(&decode_sorted, 50),
                    percentile(&decode_sorted, 95),
                    decode_sorted.last().copied().unwrap_or(0),
                    decode_sorted.iter().filter(|value| **value >= 100).count(),
                    accumulator.controller.backlog,
                    accumulator.host.fps,
                    mean(&encode_sorted),
                    f64::from(accumulator.host.capture_us) / 1000.0,
                    accumulator.host.sent_kbps,
                    accumulator.host.bitrate_kbps,
                    patch_percent(&accumulator.host),
                    ctld_metrics.keyframes.load(Ordering::Relaxed),
                    ctld_metrics.idr_bytes.load(Ordering::Relaxed) / 1024,
                    delta_stall_us as f64 / 1000.0,
                    video_write.max_stall_us.load(Ordering::Relaxed) as f64 / 1000.0,
                );
            }

            // 收尾：把剩余样本与引擎事件一并拿走。
            accumulator.seconds = started.elapsed().as_secs().max(1);
            drain_engine_events(&mut accumulator, &mut ctrl_stats, &mut ctld_stats);
            accumulator
                .e2e
                .extend_from_slice(&ctrl_metrics.e2e_second.take());
            accumulator
                .decode
                .extend_from_slice(&ctrl_metrics.decode_second.take());
            accumulator
                .encode
                .extend_from_slice(&ctld_metrics.encode_second.take());
            accumulator.readback = ctrl_metrics.readback_ms.take();
            accumulator.patch_e2e = ctrl_metrics.patch_e2e_ms.take();
            accumulator.capture_to_encode = ctrl_metrics.capture_to_encode_ms.take();
            accumulator.encode_to_sink = ctrl_metrics.encode_to_sink_ms.take();
            accumulator.write_stall_us = video_write.stall_samples.take();
            accumulator.gaps_ge_500 = ctrl_metrics.gaps_ge_500.load(Ordering::Relaxed);
            accumulator.gaps_ge_100 = ctrl_metrics.gaps_ge_100.load(Ordering::Relaxed);
            accumulator.max_gap_us = ctrl_metrics.max_gap_us.load(Ordering::Relaxed);
            accumulator.encoded_frames = ctld_metrics.encoded_frames.load(Ordering::Relaxed);
            accumulator.encoder_keyframes = ctld_metrics.keyframes.load(Ordering::Relaxed);
            accumulator.encoder_idr_bytes = ctld_metrics.idr_bytes.load(Ordering::Relaxed);
            accumulator.encoder_bytes = ctld_metrics.encoded_bytes.load(Ordering::Relaxed);
            accumulator
        })
    }

    /// 把两侧统计通道里已到达的事件吸收进累积结果。
    fn drain_engine_events(
        accumulator: &mut Accumulator,
        ctrl_stats: &mut mpsc::UnboundedReceiver<SessionStats>,
        ctld_stats: &mut mpsc::UnboundedReceiver<SessionStats>,
    ) {
        while let Ok(event) = ctrl_stats.try_recv() {
            match &event {
                SessionStats::Decode {
                    decode_ms,
                    packets,
                    dropped,
                    patches,
                    e2e_ms,
                    backlog,
                    ..
                } => {
                    accumulator.controller.packets = accumulator
                        .controller
                        .packets
                        .saturating_add(u64::from(*packets));
                    accumulator.controller.dropped = accumulator
                        .controller
                        .dropped
                        .saturating_add(u64::from(*dropped));
                    accumulator.controller.patches = accumulator
                        .controller
                        .patches
                        .saturating_add(u64::from(*patches));
                    accumulator.controller.decode_ms = *decode_ms;
                    accumulator.controller.e2e_ms = *e2e_ms;
                    accumulator.controller.backlog = *backlog;
                }
                SessionStats::KeyframeRequest { reason } => {
                    let index = reason.index();
                    accumulator.controller.keyframe_requests[index] =
                        accumulator.controller.keyframe_requests[index].saturating_add(1);
                }
                SessionStats::DecoderInfo {
                    backend,
                    adapter,
                    hardware,
                } => {
                    accumulator.controller.decoder = DecoderInfo {
                        backend: backend.clone(),
                        name: "MSH264DecoderMFT".to_owned(),
                        adapter: adapter.clone(),
                        hardware: *hardware,
                    };
                }
                SessionStats::Codec { codec } => accumulator.controller.codec = Some(*codec),
                SessionStats::Rtt(rtt) => {
                    accumulator.controller.rtt_ms =
                        u32::try_from(rtt.as_millis()).unwrap_or(u32::MAX);
                }
                _ => {}
            }
        }
        while let Ok(event) = ctld_stats.try_recv() {
            accumulator.host.absorb(&event);
        }
    }

    // ─────────────────────────── 会话编排 ───────────────────────────

    /// 跑完整条回路。
    async fn run_session(args: &Args, display: &DisplayInfo) -> Result<Accumulator, String> {
        let screen = Arc::new(BenchScreen::new(args.capture));
        screen.warmup(display.id)?;
        let mut link = delayed_link(Duration::from_millis(args.delay_ms));
        let plan = plan_controller_handshake(SESSION_ID, TOKEN, "111111111")
            .map_err(|err| format!("构造控制器握手失败: {err}"))?;
        // 真实链路里 `Hello` 由建立方在 `RequestRelay` 之后流水线写出；这里没有中继帧，
        // 直接先写进链路（会话循环随后只等 `HelloAck`）。
        link.controller
            .write_all(&plan.hello_frame)
            .await
            .map_err(|err| format!("写出 Hello 失败: {err}"))?;
        link.controller
            .flush()
            .await
            .map_err(|err| format!("flush Hello 失败: {err}"))?;

        let pending = Arc::new(PendingSessions::new());
        pending.insert(PendingSession::new(
            SESSION_ID.to_owned(),
            TOKEN.to_owned(),
            "loopback-controller".to_owned(),
            RemoteMode::Control,
        ));

        let ctrl_metrics = Arc::new(CtrlMetrics::default());
        let ctld_metrics = Arc::new(CtldMetrics::default());
        let (ctrl_stats_tx, ctrl_stats_rx) = mpsc::unbounded_channel();
        let (ctld_stats_tx, ctld_stats_rx) = mpsc::unbounded_channel();

        let cancel = CancellationToken::new();
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let (ctrl_transfer_events, _ctrl_transfer_rx) = mpsc::unbounded_channel();
        let (ctld_transfer_events, _ctld_transfer_rx) = mpsc::unbounded_channel();

        let sink = Arc::new(BenchSink {
            metrics: Arc::clone(&ctrl_metrics),
            encode_timeline: Arc::clone(&ctld_metrics),
            closed: Mutex::new(None),
        });

        let mut controller_join = tokio::spawn(run_controller(
            link.controller,
            ControllerConfig {
                session_id: SESSION_ID.to_owned(),
                token: TOKEN.to_owned(),
                local_rd_id: "111111111".to_owned(),
                mode: RemoteMode::Control,
                stats: SessionStatsReporter::new(ctrl_stats_tx),
                h264_decoder_factory: Some(Arc::new(BenchDecoderFactory {
                    metrics: Arc::clone(&ctrl_metrics),
                })),
                h265_decoder_factory: None,
                prefer_h265: true,
                reliable: None,
            },
            plan,
            cmd_rx,
            TransferHub::new(None, std::env::temp_dir(), ctrl_transfer_events),
            sink.clone(),
            cancel.clone(),
        ));

        let mut host_join = tokio::spawn(run_controlled(
            link.controlled,
            ControlledConfig {
                session_id: SESSION_ID.to_owned(),
                local_rd_id: "222222222".to_owned(),
                mode: RemoteMode::Control,
                pending,
                encode: EncodeOptions {
                    target_bitrate_kbps: args.bitrate_kbps,
                    scale: 1.0,
                    max_fps: args.fps,
                },
                native_resolution: false,
                capture_watchdog: CAPTURE_WATCHDOG,
                unchanged_resend: UNCHANGED_RESEND_INTERVAL,
                stats: SessionStatsReporter::new(ctld_stats_tx),
                h264_encoder_factory: Some(Arc::new(MftOnlyFactory {
                    metrics: Arc::clone(&ctld_metrics),
                })),
                h265_encoder_factory: None,
                prefer_h265: true,
                prefer_444: false,
                lock_on_disconnect: Arc::new(AtomicBool::new(false)),
            },
            screen,
            Arc::new(NullInputSink),
            None,
            TransferHub::new(None, std::env::temp_dir(), ctld_transfer_events),
            cancel.clone(),
        ));

        let printer = spawn_printer(
            Arc::clone(&ctrl_metrics),
            Arc::clone(&ctld_metrics),
            Arc::clone(&link.video_write),
            ctrl_stats_rx,
            ctld_stats_rx,
            cancel.clone(),
        );

        let deadline = tokio::time::sleep(Duration::from_secs(args.seconds));
        tokio::pin!(deadline);
        let mut session_errors = Vec::new();
        println!("会话开始（{} 秒）…", args.seconds);
        tokio::select! {
            () = &mut deadline => println!("到点，取消会话"),
            result = &mut controller_join => {
                session_errors.push(format!("控制器提前结束: {result:?}"));
            }
            result = &mut host_join => {
                session_errors.push(format!("被控端提前结束: {result:?}"));
            }
        }
        cancel.cancel();
        // 两个任务都应在取消后很快收尾；给 10 秒兜底。
        let _ = tokio::time::timeout(Duration::from_secs(10), &mut controller_join).await;
        let _ = tokio::time::timeout(Duration::from_secs(10), &mut host_join).await;
        let closed = sink
            .closed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let mut accumulator = printer
            .await
            .map_err(|err| format!("打印机任务失败: {err}"))?;
        accumulator.closed = closed;
        accumulator.session_errors = session_errors;
        drop(cmd_tx);
        Ok(accumulator)
    }

    // ─────────────────────────── 最终汇总 ───────────────────────────

    /// 打印最终汇总，并逐条对照 BRIEF §4 的验收口径。
    fn print_summary(args: &Args, display: &DisplayInfo, accumulator: &Accumulator) {
        let mut e2e = accumulator.e2e.clone();
        e2e.sort_unstable();
        let mut decode = accumulator.decode.clone();
        decode.sort_unstable();
        let mut readback = accumulator.readback.clone();
        readback.sort_unstable();
        let mut patch_e2e = accumulator.patch_e2e.clone();
        patch_e2e.sort_unstable();
        let mut encode = accumulator.encode.clone();
        encode.sort_unstable();
        let mut write_stall = accumulator.write_stall_us.clone();
        write_stall.sort_unstable();
        let mut capture_to_encode = accumulator.capture_to_encode.clone();
        capture_to_encode.sort_unstable();
        let mut encode_to_sink = accumulator.encode_to_sink.clone();
        encode_to_sink.sort_unstable();

        let seconds = accumulator.seconds.max(1);
        let frames = u64::try_from(accumulator.e2e.len()).unwrap_or(0);
        let controller = &accumulator.controller;
        let host = &accumulator.host;
        let drop_rate = if controller.packets == 0 {
            0.0
        } else {
            controller.dropped as f64 * 100.0 / controller.packets as f64
        };
        let idr_per_second = accumulator.host.keyframes as f64 / seconds as f64;
        let decode_stalls = accumulator
            .decode
            .iter()
            .filter(|value| **value >= 100)
            .count();
        let stalls_per_30s = decode_stalls as f64 * 30.0 / seconds as f64;
        let encode_fps = accumulator.host.fps_sum as f64 / seconds as f64;

        println!(
            "\n=== 汇总（{seconds} s，显示器 #{} {}x{}，单向延迟 {} ms）===",
            display.id, display.width, display.height, args.delay_ms
        );
        println!(
            "控制器: 解出帧 {frames}（{:.1} fps）· 收包 {} · 丢包 {}（{drop_rate:.1}%）· 补丁批 {}",
            frames as f64 / seconds as f64,
            controller.packets,
            controller.dropped,
            controller.patches,
        );
        println!(
            "e2e(ms): p50 {} p95 {} p99 {} max {} · 分段: 采集→编码完成 p50 {} / 编码完成→sink p50 {}",
            percentile(&e2e, 50),
            percentile(&e2e, 95),
            percentile(&e2e, 99),
            e2e.last().copied().unwrap_or(0),
            percentile(&capture_to_encode, 50),
            percentile(&encode_to_sink, 50),
        );
        println!(
            "帧间隔: ≥100ms {} 次 · ≥500ms {} 次 · max {:.0} ms",
            accumulator.gaps_ge_100,
            accumulator.gaps_ge_500,
            accumulator.max_gap_us as f64 / 1000.0,
        );
        println!(
            "解码(ms): p50 {} p95 {} max {} · 停顿(≥100ms) {decode_stalls} 次（{stalls_per_30s:.2} 次/30s）\
             · DXVA 回读 p95 {} max {} ms",
            percentile(&decode, 50),
            percentile(&decode, 95),
            decode.last().copied().unwrap_or(0),
            percentile(&readback, 95),
            readback.last().copied().unwrap_or(0),
        );
        let keyframe_requests: u64 = controller.keyframe_requests.iter().sum();
        let reasons = (0..KeyframeReason::COUNT)
            .filter_map(|index| {
                let count = controller.keyframe_requests[index];
                (count > 0).then(|| {
                    format!(
                        "{}={count}",
                        KeyframeReason::from_index(index).map_or("?", KeyframeReason::label)
                    )
                })
            })
            .collect::<Vec<_>>()
            .join(" ");
        println!("关键帧请求: {keyframe_requests} 次（{reasons}）");
        println!(
            "被控端: 编码帧 {}（{encode_fps:.1} fps）· enc 均 {:.1} / p95 {} / max {} ms · 采集均 {:.2} ms",
            accumulator.encoded_frames,
            mean(&encode),
            percentile(&encode, 95),
            encode.last().copied().unwrap_or(0),
            f64::from(accumulator.host.capture_us) / 1000.0,
        );
        println!(
            "IDR: {} 张（{idr_per_second:.2} 张/s）· {} KB / 出码 {} KB（引擎口径 {} 张 / {} KB）\
             · 强制 IDR {} · 重建 {}",
            accumulator.encoder_keyframes,
            accumulator.encoder_idr_bytes / 1024,
            accumulator.encoder_bytes / 1024,
            accumulator.host.keyframes,
            accumulator.host.idr_bytes / 1024,
            accumulator.host.forced_idrs,
            accumulator.host.encoder_rebuilds,
        );
        println!(
            "补丁模式: {:.0}%（{} / {} s）· 补丁 e2e p95 {} ms · 出站队列峰值 {} · 编码前跳过 {} 帧",
            f64::from(patch_percent(host)),
            host.patch_seconds,
            host.host_seconds,
            percentile(&patch_e2e, 95),
            host.out_queue_depth_max,
            host.out_queue_skips,
        );
        println!(
            "链路写阻塞: {} 次 · p95 {:.2} ms · max {:.2} ms",
            write_stall.len(),
            f64::from(percentile(&write_stall, 95)) / 1000.0,
            f64::from(write_stall.last().copied().unwrap_or(0)) / 1000.0,
        );
        if let Some(reason) = &accumulator.closed {
            println!("会话关闭: {reason}");
        }
        for error in &accumulator.session_errors {
            println!("会话异常: {error}");
        }

        // ── BRIEF §4 验收口径 ──
        let p95 = percentile(&e2e, 95);
        let p99 = percentile(&e2e, 99);
        let gate = |ok: bool| if ok { "PASS" } else { "FAIL" };
        println!("\n--- BRIEF §4 验收对照（当前场景：1080p30 连续内容）---");
        println!(
            "1) e2e p95<120ms: {} ({}ms) · p99<200ms: {} ({}ms) · 丢帧<2%: {} ({drop_rate:.1}%) · \
             无 0.5s 冻结: {} ({} 次)",
            gate(p95 < 120),
            p95,
            gate(p99 < 200),
            p99,
            gate(drop_rate < 2.0),
            gate(accumulator.gaps_ge_500 == 0),
            accumulator.gaps_ge_500,
        );
        println!(
            "2) 补丁占比>50%: {} ({:.0}%) · 补丁 e2e<20ms: {} ({}ms) · IDR<0.2/s: {} ({idr_per_second:.2}/s)  \
             [动态场景按设计不进补丁模式，此项预期 FAIL]",
            gate(patch_percent(host) > 50),
            f64::from(patch_percent(host)),
            gate(percentile(&patch_e2e, 95) < 20),
            percentile(&patch_e2e, 95),
            gate(idr_per_second < 0.2),
        );
        println!(
            "3) 解码停顿<1 次/30s: {}（{stalls_per_30s:.2} 次/30s）· 每次停顿摊到的 IDR: {:.2} 张",
            gate(stalls_per_30s < 1.0),
            if decode_stalls == 0 {
                0.0
            } else {
                accumulator.host.keyframes as f64 / decode_stalls as f64
            },
        );
        println!(
            "4) 编码 fps≥25: {}（{encode_fps:.1}） · 写阻塞 p95<50ms: {}（{:.2}ms）",
            gate(encode_fps >= 25.0),
            gate(f64::from(percentile(&write_stall, 95)) / 1000.0 < 50.0),
            f64::from(percentile(&write_stall, 95)) / 1000.0,
        );
    }

    // ─────────────────────────── Win32 负载窗口 ───────────────────────────

    /// 窗口状态（GDI 对象 + 动画计数）。单窗口，用一对全局静态。
    struct WindowState {
        /// 双缓冲 DC。
        back_dc: HDC,
        /// 双缓冲位图。
        back_bitmap: HBITMAP,
        /// DC 里换出的原位图（销毁前还原）。
        old_bitmap: HGDIOBJ,
        /// 字体（`DEFAULT_GUI_FONT` 的句柄）。
        font: HGDIOBJ,
        /// DC 里换出的原字体。
        old_font: HGDIOBJ,
        /// 背景刷。
        background: HBRUSH,
        /// 色块刷（三色轮换）。
        blocks: [HBRUSH; 3],
        /// 客户区宽。
        width: i32,
        /// 客户区高。
        height: i32,
        /// 动画计数。
        tick: u64,
    }

    // SAFETY: `WindowState` 只在窗口线程里被访问（`WINDOW_STATE` 的唯一读写点都在 `wnd_proc`
    // 与 `window_thread` 中，而它们跑在同一条线程上）。放进 `Mutex` 静态只是为了让窗口过程
    // 能访问它，不存在跨线程共享 GDI 句柄的情况。
    unsafe impl Send for WindowState {}

    /// 单窗口的全局状态。
    static WINDOW_STATE: Mutex<Option<WindowState>> = Mutex::new(None);

    /// 动画累计帧数（主线程用它确认窗口真的在重绘）。
    static ANIMATION_TICKS: AtomicU64 = AtomicU64::new(0);

    /// 已创建的窗口（供 `--seconds` 到点后关闭）。
    struct BenchWindow {
        /// 窗口句柄（以 `usize` 保存，避免把非 `Send` 的 `HWND` 跨线程传来传去）。
        hwnd: usize,
        /// 窗口线程。
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl BenchWindow {
        /// 关闭窗口并等待窗口线程退出。
        fn stop(&mut self) {
            if self.hwnd != 0 {
                // SAFETY: 句柄由本进程创建、尚未销毁。
                unsafe {
                    PostMessageW(self.hwnd as HWND, WM_CLOSE, 0, 0);
                }
            }
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    impl Drop for BenchWindow {
        fn drop(&mut self) {
            self.stop();
        }
    }

    /// 把一个 Rust 字符串转成 NUL 结尾的 UTF-16。
    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// 颜色（`COLORREF` 是 `0x00BBGGRR`）。
    fn rgb(red: u8, green: u8, blue: u8) -> u32 {
        u32::from(red) | (u32::from(green) << 8) | (u32::from(blue) << 16)
    }

    /// 滚动文字（每帧向上移动）。
    const TEXT_LINES: [&str; 8] = [
        "RUST-TUNNEL LOOPBACK BENCH 0123456789",
        "The quick brown fox jumps over the lazy dog",
        "abcdefghijklmnopqrstuvwxyz ABCDEFGHIJKLMNOPQRSTUVWXYZ",
        "column1  column2  column3  column4  column5",
        "lorem ipsum dolor sit amet consectetur adipiscing",
        "ffi 0xdeadbeef 0xcafebabe 0x12345678 0x90abcdef",
        "the five boxing wizards jump quickly 42 3.14159",
        "00112233445566778899 AABBCCDDEEFF",
    ];

    /// 创建负载窗口（30 Hz 重绘）并返回句柄。`area` = 目标显示器的 `(left, top, width, height)`。
    fn spawn_window(area: (i32, i32, i32, i32)) -> Result<BenchWindow, String> {
        let (tx, rx) = std::sync::mpsc::channel::<Result<usize, String>>();
        let thread = std::thread::Builder::new()
            .name("bench-window".to_owned())
            .spawn(move || {
                if let Err(err) = window_thread(&tx, area) {
                    eprintln!("窗口线程失败: {err}");
                }
            })
            .map_err(|err| format!("创建窗口线程失败: {err}"))?;
        let hwnd = rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|err| format!("等待窗口创建超时: {err}"))??;
        Ok(BenchWindow {
            hwnd,
            thread: Some(thread),
        })
    }

    /// 窗口线程：注册类、建窗、30 Hz 消息循环。
    fn window_thread(
        tx: &std::sync::mpsc::Sender<Result<usize, String>>,
        area: (i32, i32, i32, i32),
    ) -> Result<(), String> {
        let hinstance = unsafe { GetModuleHandleW(std::ptr::null()) };
        // 类名必须活到进程结束（系统会保留指向它的引用）。
        let class_name: &'static [u16] = Box::leak(wide("LoopbackBenchWindow").into_boxed_slice());
        let window_title = wide("rust-tunnel loopback bench");
        let cursor = unsafe { LoadCursorW(std::ptr::null_mut(), IDC_ARROW) };
        let class = WNDCLASSW {
            style: CS_OWNDC,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: std::ptr::null_mut(),
            hCursor: cursor,
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name.as_ptr(),
        };
        // SAFETY: `class` 在本次调用期间有效，字符串都是 NUL 结尾的 UTF-16。
        let atom = unsafe { RegisterClassW(&raw const class) };
        if atom == 0 {
            return Err(format!(
                "RegisterClassW 失败: {}",
                io::Error::last_os_error()
            ));
        }
        // 客户区 1280×720 → 外层窗口尺寸。
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: WINDOW_W,
            bottom: WINDOW_H,
        };
        // SAFETY: rect 合法；样式与建窗时一致。
        unsafe {
            let _ = AdjustWindowRect(&raw mut rect, WS_OVERLAPPEDWINDOW, 0);
        }
        let outer_w = rect.right - rect.left;
        let outer_h = rect.bottom - rect.top;
        // 放在目标显示器中央（目标由 `--display` 选定，默认主显示器）。
        let (left, top, area_w, area_h) = area;
        let x = left + (area_w - outer_w) / 2;
        let y = top + (area_h - outer_h) / 2;
        // SAFETY: 类已注册；参数都是本进程持有的有效句柄/指针。
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST,
                class_name.as_ptr(),
                window_title.as_ptr(),
                WS_OVERLAPPEDWINDOW,
                x,
                y,
                outer_w,
                outer_h,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                hinstance,
                std::ptr::null(),
            )
        };
        if hwnd.is_null() {
            return Err(format!(
                "CreateWindowExW 失败: {}",
                io::Error::last_os_error()
            ));
        }
        // SAFETY: hwnd 有效。
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
            let _ = UpdateWindow(hwnd);
        }
        let _ = tx.send(Ok(hwnd as usize));

        let frame_time = Duration::from_millis(1000 / ANIMATION_HZ);
        let mut next_frame = Instant::now();
        loop {
            // 处理所有待处理消息。
            let mut message = MSG::default();
            loop {
                // SAFETY: message 是栈上的合法 MSG；只取本线程的窗口消息。
                let has = unsafe { PeekMessageW(&raw mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) };
                if has == 0 {
                    break;
                }
                if message.message == WM_QUIT {
                    return Ok(());
                }
                // SAFETY: 消息由 PeekMessageW 填充。
                unsafe {
                    TranslateMessage(&raw const message);
                    DispatchMessageW(&raw const message);
                }
            }
            // 推进动画并立刻重绘（UpdateWindow 同步走 WM_PAINT）。
            {
                let mut state = WINDOW_STATE.lock().unwrap_or_else(PoisonError::into_inner);
                if let Some(state) = state.as_mut() {
                    state.tick = state.tick.wrapping_add(1);
                }
            }
            // SAFETY: hwnd 有效且由本线程持有。
            unsafe {
                InvalidateRect(hwnd, std::ptr::null(), 0);
                UpdateWindow(hwnd);
            }
            ANIMATION_TICKS.fetch_add(1, Ordering::Relaxed);
            next_frame += frame_time;
            let now = Instant::now();
            if next_frame > now {
                std::thread::sleep(next_frame - now);
            } else {
                next_frame = now;
            }
        }
    }

    /// 绘制一帧负载：滚动文字 + 移动色块 + 计数。
    fn paint(hwnd: HWND) {
        let mut paint_struct = PAINTSTRUCT::default();
        // SAFETY: hwnd 有效；paint_struct 是栈上的合法结构。
        let target = unsafe { BeginPaint(hwnd, &raw mut paint_struct) };
        if target.is_null() {
            return;
        }
        {
            let mut guard = WINDOW_STATE.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(state) = guard.as_mut() {
                let width = state.width;
                let height = state.height;
                let tick = state.tick;
                let dc = state.back_dc;
                let clamp = |value: u64, limit: i32| -> i32 {
                    i32::try_from(value % u64::try_from(limit.max(1)).unwrap_or(1)).unwrap_or(0)
                };
                // SAFETY: dc 是本对象持有的内存 DC；GDI 句柄都来自本对象。
                unsafe {
                    let background = RECT {
                        left: 0,
                        top: 0,
                        right: width,
                        bottom: height,
                    };
                    FillRect(dc, &raw const background, state.background);
                    SelectObject(dc, state.font);
                    SetBkMode(dc, TRANSPARENT.cast_signed());
                    // 滚动文字：每条以不同相位向上滚，铺满整个高度。
                    let scroll = clamp(tick * 5, height + 80);
                    for (index, line) in TEXT_LINES.iter().enumerate() {
                        let offset = clamp(u64::try_from(index * 90).unwrap_or(0), height + 80);
                        let y = (scroll + offset) % (height + 80) - 80;
                        let text = wide(&format!("{line}  #{tick}"));
                        SetTextColor(
                            dc,
                            rgb(
                                200,
                                u8::try_from(120 + index * 15).unwrap_or(200),
                                u8::try_from(255 - index * 12).unwrap_or(120),
                            ),
                        );
                        TextOutW(
                            dc,
                            24,
                            y,
                            text.as_ptr(),
                            i32::try_from(text.len().saturating_sub(1)).unwrap_or(0),
                        );
                    }
                    // 移动色块。
                    let block_w = 220;
                    let block_h = 160;
                    let bx = clamp(tick * 17, width - block_w);
                    let by = 60 + clamp(tick * 9, height - 240);
                    let rect = RECT {
                        left: bx,
                        top: by,
                        right: bx + block_w,
                        bottom: by + block_h,
                    };
                    let brush = state.blocks[usize::try_from(tick % 3).unwrap_or(0)];
                    FillRect(dc, &raw const rect, brush);
                    // 右下角的帧计数（保证每帧都有像素变化）。
                    SetTextColor(dc, rgb(255, 255, 255));
                    let counter = wide(&format!("FRAME {tick}  {ANIMATION_HZ} Hz"));
                    TextOutW(
                        dc,
                        width - 260,
                        height - 40,
                        counter.as_ptr(),
                        i32::try_from(counter.len().saturating_sub(1)).unwrap_or(0),
                    );
                    SelectObject(dc, state.old_font);
                    // SAFETY: 两个 DC 都有效；把整块内存位图拷到窗口。
                    BitBlt(target, 0, 0, width, height, dc, 0, 0, SRCCOPY);
                }
            }
        }
        // SAFETY: 与 BeginPaint 配对。
        unsafe {
            EndPaint(hwnd, &raw const paint_struct);
        }
    }

    /// 窗口过程。
    ///
    /// # Safety
    /// 由 Win32 调用的标准窗口过程；`hwnd` 必须是由本模块 `CreateWindowExW` 创建的窗口。
    unsafe extern "system" fn wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_CREATE => {
                let mut client = RECT::default();
                // SAFETY: hwnd 有效。
                unsafe {
                    let _ = GetClientRect(hwnd, &raw mut client);
                }
                let width = (client.right - client.left).max(2);
                let height = (client.bottom - client.top).max(2);
                // SAFETY: 创建与窗口兼容的内存 DC/位图；句柄全部由本进程持有。
                let state = unsafe {
                    let back_dc = CreateCompatibleDC(std::ptr::null_mut());
                    let back_bitmap = CreateCompatibleBitmap(back_dc, width, height);
                    let old_bitmap = SelectObject(back_dc, back_bitmap);
                    let font = GetStockObject(DEFAULT_GUI_FONT);
                    let old_font = SelectObject(back_dc, font);
                    WindowState {
                        back_dc,
                        back_bitmap,
                        old_bitmap,
                        font,
                        old_font,
                        background: CreateSolidBrush(rgb(24, 28, 36)),
                        blocks: [
                            CreateSolidBrush(rgb(220, 60, 60)),
                            CreateSolidBrush(rgb(60, 200, 120)),
                            CreateSolidBrush(rgb(70, 130, 240)),
                        ],
                        width,
                        height,
                        tick: 0,
                    }
                };
                *WINDOW_STATE.lock().unwrap_or_else(PoisonError::into_inner) = Some(state);
                0
            }
            WM_ERASEBKGND => 1,
            WM_PAINT => {
                paint(hwnd);
                0
            }
            WM_CLOSE => {
                // SAFETY: hwnd 有效。
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                0
            }
            WM_DESTROY => {
                let mut guard = WINDOW_STATE.lock().unwrap_or_else(PoisonError::into_inner);
                if let Some(state) = guard.take() {
                    // SAFETY: 这些 GDI 对象由 WM_CREATE 创建，且本进程仍持有。
                    unsafe {
                        SelectObject(state.back_dc, state.old_bitmap);
                        SelectObject(state.back_dc, state.old_font);
                        let _ = DeleteObject(state.back_bitmap);
                        let _ = DeleteDC(state.back_dc);
                        for brush in state.blocks {
                            let _ = DeleteObject(brush);
                        }
                        let _ = DeleteObject(state.background);
                    }
                }
                drop(guard);
                // SAFETY: 标准退出消息。
                unsafe {
                    PostQuitMessage(0);
                }
                0
            }
            _ => {
                // SAFETY: 默认处理。
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
        }
    }
}
