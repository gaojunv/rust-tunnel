//! `win-video-capture-probe`：真机采集探针。
//!
//! 打印显示器列表，用 [`capture::open_best`]（WGC → DXGI）连续采 10 帧并统计
//! 耗时，把第一帧写成 32 位 BMP（负高度 = 自顶向下），最后分别强制尝试 DXGI
//! 与 WGC，便于对比两个后端在本机上的可用性。
//!
//! # 为什么先打印进程身份
//!
//! 无人值守实例的混合采集只在**普通桌面（`Default`）**上用 WGC/DXGI；锁屏/登录界面
//! （`Winlogon`）仍走 BitBlt。所以同一条探针在「SYSTEM + 控制台会话 + Default」与
//! 「普通用户 + 桌面」下跑出来的结论含义不同——报告里必须能看出**是什么身份、在哪个
//! 桌面**上测的，否则把 Winlogon 上的失败当成 Default 不可用，或反过来。
//!
//! # 报告落盘
//!
//! 除了 stdout，同时写一份文本报告（默认
//! `%ProgramData%\rust-tunnel\probe\capture-probe-<时间戳>.txt`，不可写时退回临时目录）。
//! 以 SYSTEM 身份被服务拉起时 stdout 没人看得到，文件才是现场能带走的证据。可用
//! `--out <路径>` 指定位置。

#[cfg(windows)]
use rust_tunnel_win_video::capture::{self, CaptureSource, CapturedFrame, MonitorInfo};
#[cfg(windows)]
use rust_tunnel_win_video::WinVideoError;
#[cfg(windows)]
use std::collections::hash_map::DefaultHasher;
#[cfg(windows)]
use std::fmt;
#[cfg(windows)]
use std::hash::{Hash, Hasher};
#[cfg(windows)]
use std::io::Write as _;
#[cfg(windows)]
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
#[cfg(windows)]
use windows::Win32::Foundation::HANDLE;
#[cfg(windows)]
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
#[cfg(windows)]
use windows::Win32::System::StationsAndDesktops::{
    GetThreadDesktop, GetUserObjectInformationW, UOI_NAME,
};
#[cfg(windows)]
use windows::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};
#[cfg(windows)]
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};

/// 连续采集的帧数。
#[cfg(windows)]
const FRAME_COUNT: usize = 10;

#[cfg(windows)]
fn main() {
    let (out, remaining) = match parse_args(std::env::args().skip(1)) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };
    if let Err(err) = run(out, remaining) {
        eprintln!("采集探针失败: {err}");
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("win-video-capture-probe 只能在 Windows 上运行");
}

/// 解析参数：只接受 `--out <路径>` 与 `--help`。
///
/// 本二进制是诊断工具，参数写错时宁可报错也不要静默忽略——静默忽略会让现场
/// 「以为指定了报告路径，实际写到了默认位置」。
#[cfg(windows)]
fn parse_args(
    args: impl IntoIterator<Item = String>,
) -> Result<(Option<PathBuf>, Vec<String>), String> {
    let mut out = None;
    let mut remaining = Vec::new();
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--out" => {
                let Some(path) = iter.next() else {
                    return Err("--out 需要一个路径参数".to_owned());
                };
                out = Some(PathBuf::from(path));
            }
            "--help" | "-h" => {
                return Err("用法：win-video-capture-probe [--out <报告路径>]".to_owned());
            }
            other => remaining.push(other.to_owned()),
        }
    }
    if !remaining.is_empty() {
        return Err(format!(
            "不认识的参数：{}\n用法：win-video-capture-probe [--out <报告路径>]",
            remaining.join(" ")
        ));
    }
    Ok((out, remaining))
}

/// 探针报告：stdout + 可选的落盘文件。
#[cfg(windows)]
struct Report {
    /// 已经输出过的内容（写文件用）。
    lines: String,
    /// 落盘路径；`None` 表示只打印（连默认目录都建不出来）。
    path: Option<PathBuf>,
}

#[cfg(windows)]
impl Report {
    /// 建报告：优先用 `--out`，否则落到 `%ProgramData%\rust-tunnel\probe\`。
    fn new(requested: Option<PathBuf>) -> Self {
        let path = requested.or_else(default_report_path);
        Self {
            lines: String::new(),
            path,
        }
    }

    /// 输出一行（同时写 stdout 与内存缓冲）。
    fn line(&mut self, args: fmt::Arguments<'_>) {
        println!("{args}");
        self.lines.push_str(&args.to_string());
        self.lines.push('\n');
    }

    /// 把缓冲写盘；失败只提示，不让探针本身失败（stdout 已经打印过）。
    fn finish(&self) {
        let Some(path) = &self.path else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::File::create(path).and_then(|mut file| file.write_all(self.lines.as_bytes()))
        {
            Ok(()) => println!("\n报告已写入: {}", path.display()),
            Err(err) => println!("\n报告写入失败（{}）: {err}", path.display()),
        }
    }
}

/// 把 `report!` 宏展开成一行输出。
#[cfg(windows)]
macro_rules! report {
    ($report:expr, $($arg:tt)*) => {
        $report.line(format_args!($($arg)*))
    };
}

/// 默认报告路径：`%ProgramData%\rust-tunnel\probe\capture-probe-<秒级时间戳>.txt`。
///
/// 目录不可写（极少数受限环境）时退回临时目录；两者都拿不到时返回 `None`（只打印）。
#[cfg(windows)]
fn default_report_path() -> Option<PathBuf> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let file_name = format!("capture-probe-{stamp}.txt");
    let base = std::env::var_os("ProgramData")
        .map_or_else(std::env::temp_dir, PathBuf::from)
        .join("rust-tunnel")
        .join("probe");
    if std::fs::create_dir_all(&base).is_ok() {
        // **只建目录成功 ≠ 可写**：目录可能已存在但当前用户没有写权限（尤其在
        // 以受限令牌/%ProgramData% 被收紧的机器上）。写一个探针文件再删，失败就
        // 走临时目录；否则报告会在 `finish()` 里静默写不出去。
        let writable = base.join(format!(".write-test-{}", std::process::id()));
        if std::fs::write(&writable, b"").is_ok() {
            let _ = std::fs::remove_file(&writable);
            return Some(base.join(file_name));
        }
    }
    let fallback = std::env::temp_dir().join("rust-tunnel").join("probe");
    std::fs::create_dir_all(&fallback)
        .ok()
        .map(|()| fallback.join(file_name))
}

#[cfg(windows)]
fn run(out: Option<PathBuf>, _remaining: Vec<String>) -> Result<(), WinVideoError> {
    let mut report = Report::new(out);
    // **失败也要落盘**：探针最常见的用法就是在测试体失败时拿报告看原因（例如
    // Winlogon 上 WGC 打不开）。若这里用 `?` 直接返回，身份/显示器那几行就随
    // `report` 一起丢了。
    let result = run_and_report(&mut report);
    report.finish();
    result
}

#[cfg(windows)]
fn run_and_report(report: &mut Report) -> Result<(), WinVideoError> {
    // 必须在枚举显示器与打开采集源之前：否则 150% 缩放的 4K 屏会被
    // `GetMonitorInfoW` 报成 2560x1440，而 WGC 实际采到 3840x2160。
    // 实例进程在 `run_app` 入口已经做过同一次声明，探针单独跑时必须自己补。
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    report_identity(report);

    let monitors = capture::enumerate_monitors()?;
    print_monitors(report, &monitors);

    let monitor = monitors
        .iter()
        .find(|m| m.is_primary)
        .or_else(|| monitors.first())
        .ok_or_else(|| WinVideoError::Config("没有可用显示器".to_owned()))?;

    report!(report, "\n== open_best ==");
    let mut source = capture::open_best(monitor)?;
    report!(
        report,
        "  目标显示器: [{}] {} {}x{}",
        monitor.index,
        monitor.device_name,
        monitor.width,
        monitor.height
    );
    report!(report, "  实际后端: {}", source.backend());

    report!(report, "\n== 连续采集 {FRAME_COUNT} 帧 ==");
    let (durations, first_frame) = collect_frames(report, source.as_mut());
    print_timings(report, &durations);

    report!(report, "\n== 写入 BMP ==");
    match &first_frame {
        Some(frame) => write_bmp_report(report, frame)?,
        None => report!(report, "  没有可写的帧"),
    }

    report!(report, "\n== GPU 纹理采集（不落 CPU，阶段 2a）==");
    probe_gpu_textures(report, source.as_mut());

    // 先释放首个采集源，再做两个后端的独立可用性对比，避免同时占用多个会话。
    drop(source);
    probe_forced_backends(report, monitor);
    Ok(())
}

/// 打印进程身份：PID / 用户 / 会话 / 当前线程桌面。
///
/// 桌面名是这份报告最关键的上下文：`Default` 上 WGC/DXGI 的结论才适用于混合采集的
/// 快路径；`Winlogon` / `Screen-saver` 上失败是**预期**，不构成「GPU 不可用」的结论。
#[cfg(windows)]
fn report_identity(report: &mut Report) {
    let pid = unsafe { GetCurrentProcessId() };
    let mut session_id = 0_u32;
    let session = match unsafe { ProcessIdToSessionId(pid, &raw mut session_id) } {
        Ok(()) => session_id.to_string(),
        Err(err) => format!("未知（{err}）"),
    };
    let desktop = current_thread_desktop_name().unwrap_or_else(|| "未知".to_owned());
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "未知".to_owned());
    report!(report, "== 进程身份 ==");
    report!(
        report,
        "  PID={pid} 用户={user} 会话={session} 当前线程桌面={desktop}"
    );
    if std::env::var_os("RUST_TUNNEL_DISABLE_GPU_CAPTURE").is_some() {
        report!(
            report,
            "  ⚠ 已设置 RUST_TUNNEL_DISABLE_GPU_CAPTURE（不影响本探针，但会影响被控端混合采集）"
        );
    }
}

/// 当前线程挂载的桌面名（`Default` / `Winlogon` / `Screen-saver` …）。
#[cfg(windows)]
fn current_thread_desktop_name() -> Option<String> {
    let thread_id = unsafe { GetCurrentThreadId() };
    let desktop = unsafe { GetThreadDesktop(thread_id) }.ok()?;
    // 桌面名最长 256 个 UTF-16 单元；`GetUserObjectInformationW` 会在需要时回填长度。
    let mut buffer = [0_u16; 256];
    let mut needed = 0_u32;
    unsafe {
        GetUserObjectInformationW(
            HANDLE(desktop.0),
            UOI_NAME,
            Some(buffer.as_mut_ptr().cast()),
            u32::try_from(buffer.len() * std::mem::size_of::<u16>()).ok()?,
            Some(&raw mut needed),
        )
    }
    .ok()?;
    let len = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..len]))
}

/// 打印显示器列表。
#[cfg(windows)]
fn print_monitors(report: &mut Report, monitors: &[MonitorInfo]) {
    report!(report, "== 显示器（{} 块）==", monitors.len());
    for monitor in monitors {
        report!(
            report,
            "  [{}] HMONITOR={:?} 位置=({}, {}) 尺寸={}x{} 主屏={} 设备={}",
            monitor.index,
            monitor.hmonitor,
            monitor.left,
            monitor.top,
            monitor.width,
            monitor.height,
            monitor.is_primary,
            monitor.device_name
        );
    }
}

/// 连续采集并打印每帧耗时，返回耗时列表与第一帧。
#[cfg(windows)]
fn collect_frames(
    report: &mut Report,
    source: &mut dyn CaptureSource,
) -> (Vec<Duration>, Option<CapturedFrame>) {
    let mut durations = Vec::with_capacity(FRAME_COUNT);
    let mut first_frame: Option<CapturedFrame> = None;
    let mut previous_hash: Option<u64> = None;
    for index in 0..FRAME_COUNT {
        let started = Instant::now();
        match source.capture() {
            Ok(frame) => {
                let elapsed = started.elapsed();
                durations.push(elapsed);
                let hash = hash_bytes(&frame.bgra);
                let relation = if previous_hash == Some(hash) {
                    "与上一帧相同"
                } else {
                    "新帧"
                };
                previous_hash = Some(hash);
                report!(
                    report,
                    "  #{:02} {:>7.2} ms {}x{} {relation} hash={hash:016x}",
                    index + 1,
                    elapsed.as_secs_f64() * 1000.0,
                    frame.width,
                    frame.height
                );
                if first_frame.is_none() {
                    first_frame = Some(frame);
                }
            }
            Err(err) => {
                report!(report, "  #{:02} 采集失败: {err}", index + 1);
                break;
            }
        }
    }
    (durations, first_frame)
}

/// 打印耗时统计。
#[cfg(windows)]
fn print_timings(report: &mut Report, durations: &[Duration]) {
    if durations.is_empty() {
        report!(report, "  没有成功采集到任何帧");
        return;
    }
    let min = durations.iter().min().copied().unwrap_or_default();
    let max = durations.iter().max().copied().unwrap_or_default();
    let median = median(durations).unwrap_or_default();
    report!(
        report,
        "  中位数 {:.2} ms / 最小 {:.2} ms / 最大 {:.2} ms（成功 {} 帧）",
        median.as_secs_f64() * 1000.0,
        min.as_secs_f64() * 1000.0,
        max.as_secs_f64() * 1000.0,
        durations.len()
    );
}

/// 写 BMP 并打印路径/尺寸。
#[cfg(windows)]
fn write_bmp_report(report: &mut Report, frame: &CapturedFrame) -> Result<(), WinVideoError> {
    let path = std::env::temp_dir().join("rust-tunnel-capture-probe.bmp");
    write_bmp(&path, frame)?;
    report!(report, "  路径: {}", path.display());
    report!(
        report,
        "  尺寸: {}x{}，像素 {} 字节（32 位 BGRA，负高度 = 自顶向下）",
        frame.width,
        frame.height,
        frame.bgra.len()
    );
    Ok(())
}

/// 连续取 3 帧 GPU 纹理，打印尺寸、适配器与耗时。
///
/// 失败只报告、不让探针退出：静止桌面下 WGC 可能不给新帧，属于预期情况。
#[cfg(windows)]
fn probe_gpu_textures(report: &mut Report, source: &mut dyn CaptureSource) {
    match source.device() {
        Some(device) => report!(report, "  采集设备: {}", device.adapter.name),
        None => report!(report, "  采集设备: 未提供（后端不支持 GPU 纹理）"),
    }
    for index in 0..3 {
        let started = Instant::now();
        match source.capture_texture() {
            Ok(texture) => report!(
                report,
                "  #{:02} {:6.2} ms {}x{} subresource={} adapter={}",
                index + 1,
                started.elapsed().as_secs_f64() * 1000.0,
                texture.width,
                texture.height,
                texture.subresource,
                texture.device.adapter.name,
            ),
            Err(err) => {
                report!(
                    report,
                    "  #{:02} {:6.2} ms 不可用: {err}",
                    index + 1,
                    started.elapsed().as_secs_f64() * 1000.0
                );
                break;
            }
        }
    }
}

/// 分别强制尝试 DXGI 与 WGC，打印可用性与首帧结果。
#[cfg(windows)]
fn probe_forced_backends(report: &mut Report, monitor: &MonitorInfo) {
    report!(report, "\n== 强制 DXGI ==");
    match capture::open_dxgi(monitor) {
        Ok(mut source) => {
            report!(report, "  DXGI 可用，后端={}", source.backend());
            match source.capture() {
                Ok(frame) => report!(report, "  首帧 {}x{}", frame.width, frame.height),
                Err(err) => report!(report, "  首帧采集失败: {err}"),
            }
        }
        Err(err) => report!(report, "  DXGI 不可用: {err}"),
    }

    report!(report, "\n== 强制 WGC ==");
    match capture::open_wgc(monitor) {
        Ok(mut source) => {
            report!(report, "  WGC 可用，后端={}", source.backend());
            match source.capture() {
                Ok(frame) => report!(report, "  首帧 {}x{}", frame.width, frame.height),
                Err(err) => report!(report, "  首帧采集失败: {err}"),
            }
        }
        Err(err) => report!(report, "  WGC 不可用: {err}"),
    }
}

/// 对像素做进程内哈希，用来判断相邻两帧是否字节相同（探针诊断用）。
#[cfg(windows)]
fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// 中位数（拷贝后排序）。
#[cfg(windows)]
fn median(durations: &[Duration]) -> Option<Duration> {
    if durations.is_empty() {
        return None;
    }
    let mut sorted = durations.to_vec();
    sorted.sort_unstable();
    sorted.get(sorted.len() / 2).copied()
}

/// 写 32 位 BMP：`biHeight` 取负表示自顶向下，像素按 BGRA 原样写入。
#[cfg(windows)]
fn write_bmp(path: &Path, frame: &CapturedFrame) -> Result<(), WinVideoError> {
    let width = i32::try_from(frame.width)
        .map_err(|_| WinVideoError::Config(format!("帧宽 {} 超出 BMP 范围", frame.width)))?;
    let height = i32::try_from(frame.height)
        .map_err(|_| WinVideoError::Config(format!("帧高 {} 超出 BMP 范围", frame.height)))?;
    let image_size = u32::try_from(frame.bgra.len())
        .map_err(|_| WinVideoError::Config("像素数据超过 4 GiB".to_owned()))?;
    // 14 字节文件头 + 40 字节信息头。
    let file_size = image_size
        .checked_add(54)
        .ok_or_else(|| WinVideoError::Config("BMP 文件大小溢出".to_owned()))?;

    let mut out = Vec::with_capacity(usize::try_from(file_size).unwrap_or_default());
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&file_size.to_le_bytes());
    out.extend_from_slice(&0_u32.to_le_bytes()); // bfReserved1 + bfReserved2
    out.extend_from_slice(&54_u32.to_le_bytes()); // bfOffBits

    out.extend_from_slice(&40_u32.to_le_bytes()); // biSize
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&(-height).to_le_bytes()); // 负高度 = 自顶向下
    out.extend_from_slice(&1_u16.to_le_bytes()); // biPlanes
    out.extend_from_slice(&32_u16.to_le_bytes()); // biBitCount
    out.extend_from_slice(&0_u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&image_size.to_le_bytes());
    out.extend_from_slice(&2835_i32.to_le_bytes()); // 约 72 DPI
    out.extend_from_slice(&2835_i32.to_le_bytes());
    out.extend_from_slice(&0_u32.to_le_bytes()); // biClrUsed
    out.extend_from_slice(&0_u32.to_le_bytes()); // biClrImportant

    out.extend_from_slice(&frame.bgra);

    std::fs::write(path, &out).map_err(|err| {
        WinVideoError::Capture(format!("写入 BMP 失败（{}）: {err}", path.display()))
    })
}
