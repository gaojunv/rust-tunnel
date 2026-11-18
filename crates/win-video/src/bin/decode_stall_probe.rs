//! `decode-stall-probe`：DXVA 解码「停顿」的真机复现与定位探针。
//!
//! 现场日志（控制器侧 HD 630）里 `dec≈464ms = submit 0.06 + output 14.3 +
//! readback 445.8 + color 3.5`，40/40 次停顿都落在 staging 回读；而分桶显示停顿
//! 只在**距上一次解码完成约 200–300 ms** 的帧上出现（<200 ms 与 >300 ms 都是 0 次）。
//! 被控端的「保活补编」正好固定 250 ms 一张，所以静止桌面下几乎每张保活帧都可能
//! 换来一次 445 ms 停顿。
//!
//! 本探针复现那条链路的下半段（与真机同参）：
//!
//! - **编码**：`MftH264Encoder`（Intel QSV 硬件 MFT，**不是 NVENC**），
//!   1920x1080 @ 30 fps / 10 Mbps / H.264 4:2:0；
//! - **解码**：`decode::H264Decoder` 默认（DXVA/D3D11，优先 Intel 适配器）；
//! - **内容**：合成桌面样画面（窗口 + 文字块 + 缓慢滚动的「终端」小区域 +
//!   偶发整屏变化），P 帧几 KB、IDR 几十~几百 KB，量级贴近现场日志。
//!
//! ## 节奏矩阵（每种 20 s，逐帧打印）
//!
//! | 场景 | 节奏 | 内容 |
//! |---|---|---|
//! | A | 30 fps 连续 | 每帧小改动 |
//! | B | 5 fps（200 ms） | 每帧小改动 |
//! | C | 突发 5 帧 + 300 ms 空闲 | 每帧小改动 |
//! | D | 250 ms（保活节奏） | **逐字节静止**（= 现场保活补编） |
//! | E | 1 fps | 逐字节静止 |
//! | **F / F2** | **15 fps（66 ms）** | 动态 / 静止 |
//! | **G / G2** | **10 fps（100 ms）** | 动态 / 静止 |
//! | D2 | 250 ms | 每帧小改动（把节奏与内容分开） |
//! | D3 | 500 ms | 逐字节静止 |
//! | D4 | 250 ms + 每次两针 | 每帧小改动 |
//!
//! F/G 两档是**保活节奏决策**用的：若 66 ms 档已经 0 停顿，把
//! `UNCHANGED_RESEND_INTERVAL` 从 250 ms 降到 66 ms 才划算。
//!
//! 另有软件解码对照组（同码流、同节奏），以及这些候选实验（都是在 250 ms 静止
//! 节奏下 A/B，默认值 = 生产行为）：
//!
//! 1. `CopySubresourceRegion` 前 `Flush`；
//! 2. `Map` 用 `DO_NOT_WAIT` + `EVENT` 查询显式等待；
//! 3. 每帧重建 staging 纹理（vs 复用）；
//! 4. 每次解码换一个新线程（模拟 `spawn_blocking` 的线程轮换）；
//! 5. **不复用 MFT**：纯 D3D11 纹理 `Copy+Map`（判断停顿是否与解码器有关）；
//! 6. 1 fps 主节奏 + 每 100 ms 空跑一帧（判断「保持解码器热」是否消除停顿）。
//!
//! 判据：`readback >= 100 ms` 记一次停顿，并按 `ReadbackBreakdown` 的三段
//! （`GetResource` / `Copy` / `Map`）归因。**这台机器复现不出来的现象同样如实输出**。
//!
//! 运行：`cargo run -p rust-tunnel-win-video --bin decode-stall-probe --release`
//! （`STALL_SECONDS=10` 可缩短每种场景的时长；`STALL_VERBOSE=0` 关掉逐帧行；
//! `STALL_ONLY_CADENCE=1` 只跑节奏矩阵，跳过软件对照与 X 系列候选实验。）

// 探针是诊断程序：它按多种节奏跑、逐帧摊数字、把每段耗时直接打成表。pedantic 里
// 关于「函数行数 / 数值转换精度 / 相似变量名」的规则在一次性诊断输出里只制造噪音。
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::needless_range_loop,
    clippy::similar_names,
    clippy::too_many_lines,
    reason = "真机诊断探针：可读的逐帧数字优先，不是生产代码"
)]

#[cfg(windows)]
use rust_tunnel_win_video::decode::{
    H264Decoder, H264HardwarePreference, MapMode, ReadbackOptions, ReadbackPhase,
};
#[cfg(windows)]
use rust_tunnel_win_video::mft_encoder::MftH264Encoder;
#[cfg(windows)]
use rust_tunnel_win_video::WinVideoError;
#[cfg(windows)]
use std::time::{Duration, Instant};

/// 编码/解码尺寸（真机现场 1920x1080）。
#[cfg(windows)]
const WIDTH: u32 = 1920;
/// 编码/解码尺寸。
#[cfg(windows)]
const HEIGHT: u32 = 1080;
/// 编码帧率（与现场一致）。
#[cfg(windows)]
const FPS: u32 = 30;
/// 目标码率（kbps，与现场一致）。
#[cfg(windows)]
const BITRATE_KBPS: u32 = 10_000;
/// 一次停顿的判据（微秒）：回读耗时 ≥ 100 ms。
#[cfg(windows)]
const STALL_US: u64 = 100_000;
/// 默认每种场景的时长（秒）；可用 `STALL_SECONDS` 覆盖。
#[cfg(windows)]
const DEFAULT_SECONDS: u64 = 20;
/// 整屏重绘的间隔（帧）：制造与现场同量级的大 IDR。
#[cfg(windows)]
const FULL_CHANGE_EVERY: u32 = 300;

#[cfg(windows)]
fn main() {
    if let Err(err) = run() {
        eprintln!("解码停顿探针失败: {err}");
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("decode-stall-probe 只能在 Windows 上运行");
}

// ---------------------------------------------------------------- 合成内容

/// 合成「桌面样」画面：窗口 + 密集文字块 + 一个缓慢滚动的终端区。
///
/// 只改小区域（滚动的终端行 + 移动的选中块）让 P 帧保持在几 KB 量级；
/// 每 [`FULL_CHANGE_EVERY`] 帧整屏换内容，产出大 IDR（与现场 100–400 KB 对齐）。
#[cfg(windows)]
struct Scene {
    /// 静止底板（整屏重绘时换一份）。
    base: Vec<u8>,
    /// 当前帧。
    frame: Vec<u8>,
    /// 帧序号（决定移动块位置与整屏重绘）。
    tick: u32,
    /// 上一次 [`Scene::advance`] 是否发生了整屏重绘。
    regenerated: bool,
}

#[cfg(windows)]
impl Scene {
    fn new() -> Self {
        let base = build_desktop(WIDTH, HEIGHT, 0x5EED);
        Self {
            frame: base.clone(),
            base,
            tick: 0,
            regenerated: false,
        }
    }

    /// 推进一帧。`changed == false` 时内容**逐字节不变**（模拟保活补编）。
    fn advance(&mut self, changed: bool) {
        self.tick = self.tick.wrapping_add(1);
        self.regenerated = false;
        if !changed {
            return;
        }
        if self.tick.is_multiple_of(FULL_CHANGE_EVERY) {
            // 整屏换内容：新版文字 + 新窗口位置。
            self.base = build_desktop(WIDTH, HEIGHT, self.tick);
            self.frame.copy_from_slice(&self.base);
            self.regenerated = true;
            return;
        }
        // 终端区滚动两行 + 底部补一行新文字（小改动）。
        scroll_terminal(&mut self.frame, WIDTH, self.tick);
        // 移动的选中块（模拟光标/高亮），比整屏小几个数量级。
        let block_x = 60 + (self.tick.wrapping_mul(37) % 1500);
        fill_rect(
            &mut self.frame,
            WIDTH,
            block_x,
            120,
            220,
            26,
            [40, 72, 130, 255],
        );
    }

    fn frame(&self) -> &[u8] {
        &self.frame
    }
}

/// 画一整屏桌面样内容（RGBA）。
#[cfg(windows)]
fn build_desktop(width: u32, height: u32, seed: u32) -> Vec<u8> {
    let w = usize::try_from(width).unwrap_or(0);
    let h = usize::try_from(height).unwrap_or(0);
    let mut buffer = vec![0u8; w.saturating_mul(h).saturating_mul(4)];
    // 背景与任务栏。
    fill_rect(&mut buffer, width, 0, 0, width, height, [232, 235, 238, 255]);
    fill_rect(
        &mut buffer,
        width,
        0,
        height.saturating_sub(48),
        width,
        48,
        [32, 36, 44, 255],
    );
    // 两个窗口：标题栏 + 密集文字块（文字用 1–2 px 高的短横线堆出来）。
    let windows = [
        (40u32, 40u32, 880u32, 780u32, [56, 90, 160, 255]),
        (980, 70, 900, 660, [70, 70, 76, 255]),
    ];
    for (index, (x, y, ww, hh, title)) in windows.into_iter().enumerate() {
        fill_rect(&mut buffer, width, x, y, ww, hh, [250, 250, 252, 255]);
        fill_rect(&mut buffer, width, x, y, ww, 34, title);
        // 文字行：每 20 px 一行，行内是长短不一的短横线。
        let mut rng = seed
            .wrapping_add(u32::try_from(index).unwrap_or(0))
            .wrapping_mul(2_654_435_761)
            .max(1);
        let mut row_y = y + 56;
        while row_y + 20 < y + hh {
            let mut row_x = x + 20;
            while row_x + 24 < x + ww - 20 {
                rng ^= rng << 13;
                rng ^= rng >> 17;
                rng ^= rng << 5;
                let run = 6 + rng % 26;
                let shade = 40 + u8::try_from(rng % 60).unwrap_or(0);
                fill_rect(
                    &mut buffer,
                    width,
                    row_x,
                    row_y,
                    run,
                    2,
                    [shade, shade, shade.saturating_add(8), 255],
                );
                row_x = row_x.saturating_add(run).saturating_add(6 + rng % 10);
            }
            row_y = row_y.saturating_add(20);
        }
    }
    buffer
}

/// 终端区（窗口 2 内部）向上滚动两行并在底部补一行新文字。
#[cfg(windows)]
fn scroll_terminal(frame: &mut [u8], width: u32, tick: u32) {
    const X: u32 = 1000;
    const Y: u32 = 420;
    const W: u32 = 820;
    const H: u32 = 240;
    const SCROLL: u32 = 2;
    let w = usize::try_from(width).unwrap_or(0);
    let row_bytes = usize::try_from(W).unwrap_or(0).saturating_mul(4);
    let y0 = usize::try_from(Y).unwrap_or(0);
    let scroll = usize::try_from(SCROLL).unwrap_or(0);
    let rows = usize::try_from(H).unwrap_or(0);
    for row in 0..rows.saturating_sub(scroll) {
        let source = (y0 + row + scroll)
            .saturating_mul(w)
            .saturating_add(usize::try_from(X).unwrap_or(0));
        let destination = (y0 + row)
            .saturating_mul(w)
            .saturating_add(usize::try_from(X).unwrap_or(0));
        let start = source.saturating_mul(4);
        let end = start.saturating_add(row_bytes);
        if end <= frame.len() {
            // 行内左移：源行与目标行不重叠（row 与 row+SCROLL 不同行）。
            frame.copy_within(start..end, destination.saturating_mul(4));
        }
    }
    // 底部补的新行：随机长短的文字。
    let mut rng = tick.wrapping_mul(1_103_515_245).wrapping_add(12_345).max(1);
    let mut x = X + 12;
    let bottom = Y + H - 16;
    while x + 24 < X + W {
        rng ^= rng << 13;
        rng ^= rng >> 17;
        rng ^= rng << 5;
        let run = 5 + rng % 22;
        fill_rect(
            frame,
            width,
            x,
            bottom,
            run,
            2,
            [30, 34, 40, 255],
        );
        x = x.saturating_add(run).saturating_add(5 + rng % 9);
    }
}

/// 在 RGBA 缓冲里填一个矩形（自动裁剪到边界）。
#[cfg(windows)]
fn fill_rect(
    buffer: &mut [u8],
    width: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    color: [u8; 4],
) {
    let stride = usize::try_from(width).unwrap_or(0);
    let height = buffer.len() / stride.max(1) / 4;
    let x0 = usize::try_from(x).unwrap_or(0).min(stride);
    let y0 = usize::try_from(y).unwrap_or(0).min(height);
    let x1 = x0
        .saturating_add(usize::try_from(w).unwrap_or(0))
        .min(stride);
    let y1 = y0
        .saturating_add(usize::try_from(h).unwrap_or(0))
        .min(height);
    for row in y0..y1 {
        let start = row.saturating_mul(stride).saturating_add(x0).saturating_mul(4);
        let end = row.saturating_mul(stride).saturating_add(x1).saturating_mul(4);
        let Some(slice) = buffer.get_mut(start..end) else {
            continue;
        };
        for pixel in slice.as_chunks_mut::<4>().0 {
            pixel.copy_from_slice(&color);
        }
    }
}

// ---------------------------------------------------------------- 编码

/// 一条预编码好的码流（含每帧的 IDR 标志）。
#[cfg(windows)]
struct Stream {
    packets: Vec<Vec<u8>>,
    idr: Vec<bool>,
}

#[cfg(windows)]
impl Stream {
    /// 码流概况（首帧、IDR 与 P 帧的字节区间）。
    fn describe(&self, label: &str) {
        let mut idr: Vec<usize> = Vec::new();
        let mut p: Vec<usize> = Vec::new();
        for (packet, is_idr) in self.packets.iter().zip(&self.idr) {
            let bytes = packet.len().saturating_sub(4);
            if *is_idr {
                idr.push(bytes);
            } else {
                p.push(bytes);
            }
        }
        println!(
            "  {label}: {} 帧, IDR {} 张（{}..{} 字节）, P 帧 {} 张（{}..{} 字节）",
            self.packets.len(),
            idr.len(),
            idr.iter().min().copied().unwrap_or(0),
            idr.iter().max().copied().unwrap_or(0),
            p.len(),
            p.iter().min().copied().unwrap_or(0),
            p.iter().max().copied().unwrap_or(0),
        );
    }
}

/// 预编码一条件码流。`changed == false` 时每帧内容相同（保活补编）。
#[cfg(windows)]
fn encode_stream(
    encoder: &mut MftH264Encoder,
    count: usize,
    changed: bool,
) -> Result<Stream, WinVideoError> {
    let mut scene = Scene::new();
    let mut stream = Stream {
        packets: Vec::with_capacity(count),
        idr: Vec::with_capacity(count),
    };
    for index in 0..count {
        scene.advance(changed);
        // 首帧与整屏重绘帧强制 IDR（与引擎的「重建/大改必 IDR」一致）。
        let force = index == 0 || scene.regenerated;
        let packet = encoder.encode_rgba(scene.frame(), force)?;
        if packet.data.is_empty() {
            // 硬件编码器有流水线延迟：空包跳过（不影响参考链的连续性）。
            continue;
        }
        stream.packets.push(packet.data);
        stream.idr.push(packet.keyframe);
    }
    Ok(stream)
}

// ---------------------------------------------------------------- 回放与统计

/// 一帧的回放读数。
#[cfg(windows)]
#[derive(Clone, Copy)]
struct FrameStat {
    started: Instant,
    /// 距上一次解码**完成**的间隔（与现场日志同口径）。
    idle_ms: u64,
    /// 距上一次 **GPU 同步结束**（Map/Unmap 后）的间隔；仅在两帧都有回读时有意义。
    gap_ms: f64,
    bytes: usize,
    idr: bool,
    /// 本帧是否因回读超预算被放弃（`decode` 返回 `None`）。
    skipped: bool,
    submit_us: u64,
    output_us: u64,
    get_us: u64,
    copy_us: u64,
    map_us: u64,
    readback_us: u64,
    color_us: u64,
    total_us: u64,
}

#[cfg(windows)]
impl FrameStat {
    /// `CopySubresourceRegion` 大约在解码开始后多久（µs）。
    fn copy_start_offset_us(self) -> u64 {
        self.submit_us
            .saturating_add(self.output_us)
            .saturating_add(self.get_us)
    }

    /// 本帧 GPU 同步结束大约在解码开始后多久（µs）。
    fn gpu_end_offset_us(self) -> u64 {
        self.submit_us
            .saturating_add(self.output_us)
            .saturating_add(self.get_us)
            .saturating_add(self.copy_us)
            .saturating_add(self.map_us)
    }
}

/// 一次回放的统计。
#[cfg(windows)]
struct Run {
    label: String,
    stats: Vec<FrameStat>,
    hardware: bool,
}

#[cfg(windows)]
fn env_u64(name: &str, fallback: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(fallback)
}

#[cfg(windows)]
fn verbose() -> bool {
    std::env::var("STALL_VERBOSE").map_or(true, |value| value.trim() != "0")
}

/// 等间隔节奏的到达时刻。
#[cfg(windows)]
fn offsets_regular(count: usize, interval: Duration) -> Vec<Duration> {
    (0..count)
        .map(|index| interval.saturating_mul(u32::try_from(index).unwrap_or(0)))
        .collect()
}

/// 突发节奏：每轮 `per_burst` 帧按 `intra` 排布，轮间空 `idle`。
#[cfg(windows)]
fn offsets_burst(
    bursts: usize,
    per_burst: usize,
    intra: Duration,
    idle: Duration,
) -> Vec<Duration> {
    let mut out = Vec::with_capacity(bursts.saturating_mul(per_burst));
    let burst_span = intra.saturating_mul(u32::try_from(per_burst).unwrap_or(1));
    for burst in 0..bursts {
        let base = burst_span
            .saturating_add(idle)
            .saturating_mul(u32::try_from(burst).unwrap_or(0));
        for index in 0..per_burst {
            out.push(
                base.saturating_add(
                    intra.saturating_mul(u32::try_from(index).unwrap_or(0)),
                ),
            );
        }
    }
    out
}

/// 按到达时刻回放一条码流，逐帧记录四段读数与三分段细分。
#[cfg(windows)]
fn replay(
    label: &str,
    decoder: &mut H264Decoder,
    stream: &Stream,
    offsets: &[Duration],
) -> Result<Run, WinVideoError> {
    let count = stream.packets.len().min(offsets.len());
    let start = Instant::now();
    let mut stats: Vec<FrameStat> = Vec::with_capacity(count);
    let mut previous_end: Option<Instant> = None;
    for index in 0..count {
        let target = start.checked_add(offsets[index]).unwrap_or(start);
        let now = Instant::now();
        if target > now {
            std::thread::sleep(target.duration_since(now));
        }
        let idle_ms = previous_end.map_or(0, |ended| {
            u64::try_from(Instant::now().saturating_duration_since(ended).as_millis())
                .unwrap_or(u64::MAX)
        });
        let packet = &stream.packets[index];
        let began = Instant::now();
        let _frame = decoder.decode(packet)?;
        let total_us = u64::try_from(began.elapsed().as_micros()).unwrap_or(u64::MAX);
        let phase = decoder.last_phase_timings();
        let breakdown = decoder.last_readback_breakdown();
        let skipped = decoder.last_readback_skipped();
        previous_end = Some(began + Duration::from_micros(total_us));
        let mut stat = FrameStat {
            started: began,
            idle_ms,
            gap_ms: 0.0,
            bytes: packet.len().saturating_sub(4),
            idr: stream.idr.get(index).copied().unwrap_or(false),
            skipped,
            submit_us: phase.submit_us,
            output_us: phase.output_us,
            get_us: breakdown.get_resource_us,
            copy_us: breakdown.copy_us,
            map_us: breakdown.map_us,
            readback_us: phase.readback_us,
            color_us: phase.color_us,
            total_us,
        };
        // 距上一次 GPU 同步结束的间隔（上一帧 Map/Unmap 之后到本帧 Copy 之前）。
        if let Some(previous) = stats.last().copied() {
            let previous_gpu_end = previous
                .started
                .checked_add(Duration::from_micros(previous.gpu_end_offset_us()))
                .unwrap_or(previous.started);
            let this_copy = began
                .checked_add(Duration::from_micros(stat.copy_start_offset_us()))
                .unwrap_or(began);
            stat.gap_ms = this_copy
                .saturating_duration_since(previous_gpu_end)
                .as_secs_f64()
                * 1000.0;
        }
        if verbose() {
            println!(
                "  #{index:>4} idle={:>5}ms gap={:>7.1}ms bytes={:>7} idr={:<5} skip={:<5} \
                 sub={:>6} out={:>7} get={:>6} copy={:>7} map={:>8} rb={:>8} col={:>6} dec={:>8}",
                stat.idle_ms,
                stat.gap_ms,
                stat.bytes,
                stat.idr,
                stat.skipped,
                stat.submit_us,
                stat.output_us,
                stat.get_us,
                stat.copy_us,
                stat.map_us,
                stat.readback_us,
                stat.color_us,
                stat.total_us,
            );
        }
        stats.push(stat);
    }
    Ok(Run {
        label: label.to_owned(),
        stats,
        hardware: decoder.path() != rust_tunnel_win_video::decode::H264DecodePath::Software,
    })
}

/// 1 fps 主节奏 + 每 `keepalive` 间隔空跑一帧（「保持解码器热」实验）。
#[cfg(windows)]
fn replay_with_keepalive(
    label: &str,
    decoder: &mut H264Decoder,
    stream: &Stream,
    seconds: u64,
    keepalive: Duration,
) -> Result<Run, WinVideoError> {
    let start = Instant::now();
    let mut stats = Vec::new();
    let mut cursor = 0usize;
    let mut next_main = Duration::ZERO;
    let mut next_keep = keepalive;
    while next_main < Duration::from_secs(seconds) {
        let event = next_main.min(next_keep);
        let target = start.checked_add(event).unwrap_or(start);
        let now = Instant::now();
        if target > now {
            std::thread::sleep(target.duration_since(now));
        }
        let Some(packet) = stream.packets.get(cursor) else {
            break;
        };
        let measure = event == next_main;
        let began = Instant::now();
        let _frame = decoder.decode(packet)?;
        let total_us = u64::try_from(began.elapsed().as_micros()).unwrap_or(u64::MAX);
        if measure {
            let phase = decoder.last_phase_timings();
            let breakdown = decoder.last_readback_breakdown();
            stats.push(FrameStat {
                started: began,
                idle_ms: keepalive.as_millis() as u64,
                gap_ms: 0.0,
                bytes: packet.len().saturating_sub(4),
                idr: stream.idr.get(cursor).copied().unwrap_or(false),
                skipped: decoder.last_readback_skipped(),
                submit_us: phase.submit_us,
                output_us: phase.output_us,
                get_us: breakdown.get_resource_us,
                copy_us: breakdown.copy_us,
                map_us: breakdown.map_us,
                readback_us: phase.readback_us,
                color_us: phase.color_us,
                total_us,
            });
            println!(
                "  #{:>3}（主节奏）rb={}us get={} copy={} map={} dec={}us",
                stats.len().saturating_sub(1),
                stats
                    .last()
                    .map_or(0, |stat| stat.readback_us),
                stats.last().map_or(0, |stat| stat.get_us),
                stats.last().map_or(0, |stat| stat.copy_us),
                stats.last().map_or(0, |stat| stat.map_us),
                total_us,
            );
            next_main = next_main.saturating_add(Duration::from_secs(1));
        } else {
            next_keep = next_keep.saturating_add(keepalive);
        }
        cursor = cursor.saturating_add(1);
        if cursor >= stream.packets.len() {
            break;
        }
    }
    Ok(Run {
        label: label.to_owned(),
        stats,
        hardware: true,
    })
}

/// 百分位（`percent` 取 0..=100）。
#[cfg(windows)]
fn percentile(values: &[u64], percent: usize) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let index = sorted
        .len()
        .saturating_sub(1)
        .saturating_mul(percent.min(100))
        / 100;
    sorted.get(index).copied().unwrap_or(0)
}

/// 打印一次回放的汇总（停顿次数、三段归因、按 gap 分桶）。
#[cfg(windows)]
fn summarize(run: &Run) {
    // 桶宽在 250 ms 以下取 25 ms：66 ms / 100 ms 两档的安全区边界就落在这里，
    // 粗桶（0–99 / 100–199）会把结论糊掉。
    const BUCKETS: [&str; 12] = [
        "0–24 ms",
        "25–49 ms",
        "50–74 ms",
        "75–99 ms",
        "100–124 ms",
        "125–149 ms",
        "150–199 ms",
        "200–249 ms",
        "250–349 ms",
        "350–499 ms",
        "500–799 ms",
        "≥800 ms",
    ];
    let readbacks: Vec<u64> = run.stats.iter().map(|stat| stat.readback_us).collect();
    let maps: Vec<u64> = run.stats.iter().map(|stat| stat.map_us).collect();
    let stalls: Vec<&FrameStat> = run
        .stats
        .iter()
        .filter(|stat| stat.readback_us >= STALL_US)
        .collect();
    println!(
        "  [{}] 帧 {}{} 回读 p50={}us p95={}us max={}us | Map p50={}us p95={}us max={}us | 停顿 {} 次（{:.0}%）| 超预算跳过 {} 帧",
        run.label,
        run.stats.len(),
        if run.hardware { "" } else { "（软件）" },
        percentile(&readbacks, 50),
        percentile(&readbacks, 95),
        readbacks.iter().copied().max().unwrap_or(0),
        percentile(&maps, 50),
        percentile(&maps, 95),
        maps.iter().copied().max().unwrap_or(0),
        stalls.len(),
        if run.stats.is_empty() {
            0.0
        } else {
            100.0 * f64::from(u32::try_from(stalls.len()).unwrap_or(0))
                / f64::from(u32::try_from(run.stats.len()).unwrap_or(1))
        },
        run.stats.iter().filter(|stat| stat.skipped).count(),
    );
    if !stalls.is_empty() {
        let mut map = 0usize;
        let mut copy = 0usize;
        let mut get = 0usize;
        let mut map_us = 0u64;
        let mut copy_us = 0u64;
        let mut get_us = 0u64;
        for stat in &stalls {
            let breakdown = rust_tunnel_win_video::decode::ReadbackBreakdown {
                get_resource_us: stat.get_us,
                copy_us: stat.copy_us,
                map_us: stat.map_us,
            };
            match breakdown.dominant() {
                ReadbackPhase::Map => {
                    map = map.saturating_add(1);
                    map_us = map_us.saturating_add(stat.map_us);
                }
                ReadbackPhase::Copy => {
                    copy = copy.saturating_add(1);
                    copy_us = copy_us.saturating_add(stat.copy_us);
                }
                ReadbackPhase::GetResource | ReadbackPhase::None => {
                    get = get.saturating_add(1);
                    get_us = get_us.saturating_add(stat.get_us);
                }
            }
        }
        println!(
            "    停顿归因: Map {map} 次（均 {:.1} ms）| Copy {copy} 次（均 {:.1} ms）| GetResource {get} 次（均 {:.1} ms）",
            if map == 0 { 0.0 } else { map_us as f64 / 1000.0 / map as f64 },
            if copy == 0 { 0.0 } else { copy_us as f64 / 1000.0 / copy as f64 },
            if get == 0 { 0.0 } else { get_us as f64 / 1000.0 / get as f64 },
        );
    }
    // 按「距上一次 GPU 同步结束」分桶——这是判断「空闲甜点区间」的直接证据。
    let mut total = [0usize; BUCKETS.len()];
    let mut stalled = [0usize; BUCKETS.len()];
    for stat in &run.stats {
        let bucket = match stat.gap_ms {
            value if value < 25.0 => 0,
            value if value < 50.0 => 1,
            value if value < 75.0 => 2,
            value if value < 100.0 => 3,
            value if value < 125.0 => 4,
            value if value < 150.0 => 5,
            value if value < 200.0 => 6,
            value if value < 250.0 => 7,
            value if value < 350.0 => 8,
            value if value < 500.0 => 9,
            value if value < 800.0 => 10,
            _ => 11,
        };
        total[bucket] = total[bucket].saturating_add(1);
        if stat.readback_us >= STALL_US {
            stalled[bucket] = stalled[bucket].saturating_add(1);
        }
    }
    println!("    按「距上次 GPU 同步结束」分桶（帧数/停顿）:");
    for (index, label) in BUCKETS.iter().enumerate() {
        if total[index] > 0 {
            println!(
                "      {label:>12}: {:>4} / {:>3} 次停顿（{:.0}%）",
                total[index],
                stalled[index],
                100.0 * stalled[index] as f64 / total[index] as f64,
            );
        }
    }
}

// ---------------------------------------------------------------- 实验

/// 纯 D3D11 的 `CopySubresourceRegion + Map` 装置（**不经过解码器 MFT**）。
///
/// 用来回答两个问题：① 停顿是否与 DXVA 解码器有关（对照：无关则这里同样停顿）；
/// ② 节奏/预热对停顿的影响（不依赖编码器，跑得飞快）。
#[cfg(windows)]
struct PlainRig {
    /// 设备（后台预热线程也要用）。
    device: std::sync::Arc<rust_tunnel_win_video::d3d11::D3d11>,
    /// 源纹理（模拟解码器输出）。
    source: windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
    /// staging（模拟回读目标）。
    staging: windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
}

#[cfg(windows)]
impl PlainRig {
    /// 在 Intel（其次任意硬件）适配器上建一套 1920x1088 NV12 纹理。
    fn new() -> Result<Self, WinVideoError> {
        use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;
        Self::new_sized(DXGI_FORMAT_NV12, WIDTH, 1088)
    }

    /// 指定格式/尺寸的一套纹理（同样在 Intel 适配器上）。
    fn new_sized(
        format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
        width: u32,
        height: u32,
    ) -> Result<Self, WinVideoError> {
        use windows::Win32::Graphics::Direct3D11::{
            ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
            D3D11_USAGE_STAGING,
        };
        use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;

        let adapters = rust_tunnel_win_video::d3d11::adapters()?;
        let chosen = adapters
            .iter()
            .find(|adapter| {
                !adapter.software
                    && adapter.vendor_id == rust_tunnel_win_video::d3d11::VENDOR_INTEL
            })
            .or_else(|| adapters.iter().find(|adapter| !adapter.software))
            .ok_or_else(|| WinVideoError::Config("没有可用的硬件适配器".to_owned()))?;
        let device = std::sync::Arc::new(rust_tunnel_win_video::d3d11::create(Some(chosen.index))?);
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: 0,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut source: Option<ID3D11Texture2D> = None;
        // SAFETY: `desc` 指向本栈帧，`source` 是输出槽。
        unsafe { device.device.CreateTexture2D(&raw const desc, None, Some(&raw mut source)) }
            .map_err(|err| WinVideoError::Decode(format!("创建源纹理失败: {err}")))?;
        let source = source.ok_or_else(|| WinVideoError::Decode("源纹理为空".to_owned()))?;
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Usage: D3D11_USAGE_STAGING,
            CPUAccessFlags: u32::try_from(D3D11_CPU_ACCESS_READ.0).unwrap_or(0),
            ..desc
        };
        let mut staging: Option<ID3D11Texture2D> = None;
        // SAFETY: 同上。
        unsafe {
            device
                .device
                .CreateTexture2D(&raw const staging_desc, None, Some(&raw mut staging))
        }
        .map_err(|err| WinVideoError::Decode(format!("创建 staging 纹理失败: {err}")))?;
        let staging = staging.ok_or_else(|| WinVideoError::Decode("staging 纹理为空".to_owned()))?;
        Ok(Self {
            device,
            source,
            staging,
        })
    }

    /// 一次 `Copy + Map + Unmap`，返回 `(copy_us, map_us)`。
    fn iteration(&self) -> Result<(u64, u64), WinVideoError> {
        use windows::Win32::Graphics::Direct3D11::{
            D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
        };
        let began = Instant::now();
        // SAFETY: 两块纹理都在同一设备上、尺寸/格式一致；`None` = 整个子资源。
        unsafe {
            self.device.context.CopySubresourceRegion(
                &self.staging,
                0,
                0,
                0,
                0,
                &self.source,
                0,
                None,
            );
        }
        let copy_end = Instant::now();
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: staging 是 STAGING + CPU_ACCESS_READ 纹理。
        let map = unsafe {
            self.device
                .context
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&raw mut mapped))
        };
        let map_end = Instant::now();
        map.map_err(|err| WinVideoError::Decode(format!("Map 失败: {err}")))?;
        // SAFETY: 与上面的 Map 配对。
        unsafe { self.device.context.Unmap(&self.staging, 0) };
        Ok((
            u64::try_from(copy_end.duration_since(began).as_micros()).unwrap_or(0),
            u64::try_from(map_end.duration_since(copy_end).as_micros()).unwrap_or(0),
        ))
    }
}

/// 后台预热线程：每 `period` 在**同一设备**上向一块 16x16 staging 纹理发一次
/// `CopySubresourceRegion + Flush`，让 GPU 不进入「空闲后第一条命令巨慢」的状态。
///
/// 只给探针用（真机结论见输出）：它验证「周期保活能否消除停顿」，而不是生产实现。
#[cfg(windows)]
struct Warmer {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

#[cfg(windows)]
impl Warmer {
    /// `size` × `size` 的 BGRA 拷贝；`sync` 为真时再 `Map/Unmap`（真正等 GPU 完成）。
    fn start_sized(
        device: std::sync::Arc<rust_tunnel_win_video::d3d11::D3d11>,
        period: Duration,
        size: u32,
        sync: bool,
    ) -> Self {
        use windows::Win32::Graphics::Direct3D11::{
            ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
            D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
        };
        use windows::Win32::Graphics::Dxgi::Common::{
            DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
        };
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            // NV12 有视频格式的尺寸约束（16x16 会被拒绝），预热只是「让引擎跑一下」，
            // 用普通 BGRA 纹理即可。
            let small = |usage| D3D11_TEXTURE2D_DESC {
                Width: size,
                Height: size,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: usage,
                BindFlags: 0,
                CPUAccessFlags: u32::try_from(D3D11_CPU_ACCESS_READ.0).unwrap_or(0),
                MiscFlags: 0,
            };
            let mut source: Option<ID3D11Texture2D> = None;
            let mut staging: Option<ID3D11Texture2D> = None;
            let source_desc = small(D3D11_USAGE_DEFAULT);
            let staging_desc = small(D3D11_USAGE_STAGING);
            // SAFETY: 两块 16x16 NV12 纹理；输出指针指向本栈帧。
            let created = unsafe {
                device
                    .device
                    .CreateTexture2D(&raw const source_desc, None, Some(&raw mut source))
                    .and_then(|()| {
                        device.device.CreateTexture2D(
                            &raw const staging_desc,
                            None,
                            Some(&raw mut staging),
                        )
                    })
            };
            let (Some(source), Some(staging), Ok(())) = (source, staging, created) else {
                println!("  ⚠ 预热线程：创建预热纹理失败，本次不做周期预热");
                return;
            };
            while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                // SAFETY: 同一设备上的两块同格式同尺寸纹理。
                unsafe {
                    device.context.CopySubresourceRegion(
                        &staging,
                        0,
                        0,
                        0,
                        0,
                        &source,
                        0,
                        None,
                    );
                    device.context.Flush();
                    if sync {
                        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                        if device
                            .context
                            .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&raw mut mapped))
                            .is_ok()
                        {
                            device.context.Unmap(&staging, 0);
                        }
                    }
                }
                std::thread::sleep(period);
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }
}

#[cfg(windows)]
impl Drop for Warmer {
    fn drop(&mut self) {
        self.stop
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// 跑一段纯 D3D11 的 `Copy+Map` 节奏，打印 p50/p95/max 与停顿数。
#[cfg(windows)]
fn plain_copy_map(label: &str, iterations: usize, interval: Duration) -> Result<(), WinVideoError> {
    let rig = PlainRig::new()?;
    plain_run(&rig, label, iterations, interval)
}

/// 在一套已经建好的装置上按固定节奏跑。
#[cfg(windows)]
fn plain_run(
    rig: &PlainRig,
    label: &str,
    iterations: usize,
    interval: Duration,
) -> Result<(), WinVideoError> {
    let start = Instant::now();
    let mut readbacks = Vec::with_capacity(iterations);
    for index in 0..iterations {
        let target = start
            .checked_add(interval.saturating_mul(u32::try_from(index).unwrap_or(0)))
            .unwrap_or(start);
        let now = Instant::now();
        if target > now {
            std::thread::sleep(target.duration_since(now));
        }
        let (copy_us, map_us) = rig.iteration()?;
        let readback = copy_us.saturating_add(map_us);
        readbacks.push(readback);
        if verbose() {
            println!("  #{index:>4} copy={copy_us:>7} map={map_us:>8} rb={readback:>8}");
        }
    }
    println!(
        "  [{label}] 纯 D3D11（无 MFT）{iterations} 次: 回读 p50={}us p95={}us max={}us | 停顿 {} 次",
        percentile(&readbacks, 50),
        percentile(&readbacks, 95),
        readbacks.iter().copied().max().unwrap_or(0),
        readbacks.iter().filter(|value| **value >= STALL_US).count(),
    );
    Ok(())
}

/// 节奏扫描：同一套装置、不同间隔，给出「间隔 → 停顿率」曲线。
#[cfg(windows)]
fn plain_sweep(
    rig: &PlainRig,
    intervals: &[Duration],
    per_interval: usize,
) -> Result<(), WinVideoError> {
    println!("\n== 节奏扫描（纯 D3D11，每个间隔 {per_interval} 次）==");
    for interval in intervals {
        let start = Instant::now();
        let mut readbacks = Vec::with_capacity(per_interval);
        for index in 0..per_interval {
            let target = start
                .checked_add(interval.saturating_mul(u32::try_from(index).unwrap_or(0)))
                .unwrap_or(start);
            let now = Instant::now();
            if target > now {
                std::thread::sleep(target.duration_since(now));
            }
            let (copy_us, map_us) = rig.iteration()?;
            readbacks.push(copy_us.saturating_add(map_us));
        }
        let stalls = readbacks.iter().filter(|value| **value >= STALL_US).count();
        println!(
            "  间隔 {:>5} ms: p50={:>7}us p95={:>7}us max={:>8}us | 停顿 {:>2}/{:<2}（{:.0}%）",
            interval.as_millis(),
            percentile(&readbacks, 50),
            percentile(&readbacks, 95),
            readbacks.iter().copied().max().unwrap_or(0),
            stalls,
            per_interval,
            100.0 * stalls as f64 / per_interval as f64,
        );
    }
    Ok(())
}

/// 格式/尺寸对照：停顿是不是 NV12 staging 拷贝特有的？与面积有关吗？
///
/// 现场日志里「2.8 KB 的帧同样卡 445 ms」说明与 **压缩后** 大小无关；这里看的是
/// *纹理* 格式与面积。
#[cfg(windows)]
fn plain_format_compare(iterations: usize, interval: Duration) -> Result<(), WinVideoError> {
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12,
    };
    println!("\n== 格式/尺寸对照（纯 D3D11，间隔 {} ms，每个 {iterations} 次）==", interval.as_millis());
    let cases = [
        ("NV12 1920x1088（真机同款）", DXGI_FORMAT_NV12, 1920u32, 1088u32),
        ("BGRA 1920x1080", DXGI_FORMAT_B8G8R8A8_UNORM, 1920, 1080),
        ("NV12 320x240", DXGI_FORMAT_NV12, 320, 240),
        ("BGRA 320x240", DXGI_FORMAT_B8G8R8A8_UNORM, 320, 240),
    ];
    for (label, format, width, height) in cases {
        let rig = PlainRig::new_sized(format, width, height)?;
        let start = Instant::now();
        let mut readbacks = Vec::with_capacity(iterations);
        for index in 0..iterations {
            let target = start
                .checked_add(interval.saturating_mul(u32::try_from(index).unwrap_or(0)))
                .unwrap_or(start);
            let now = Instant::now();
            if target > now {
                std::thread::sleep(target.duration_since(now));
            }
            let (copy_us, map_us) = rig.iteration()?;
            readbacks.push(copy_us.saturating_add(map_us));
        }
        let stalls = readbacks.iter().filter(|value| **value >= STALL_US).count();
        println!(
            "  {label:<26}: p50={:>7}us p95={:>7}us max={:>8}us | 停顿 {stalls}/{iterations}",
            percentile(&readbacks, 50),
            percentile(&readbacks, 95),
            readbacks.iter().copied().max().unwrap_or(0),
        );
    }
    Ok(())
}

/// 设备创建差异对照：D3D11 **多线程保护** 与 `VIDEO_SUPPORT` 是否造成停顿。
///
/// `crate::d3d11::create` 对所有设备都开 `SetMultithreadProtected(true)`（`VIDEO_SUPPORT`
/// 的官方要求）。这里手工建同款设备、只改这两个开关，看停顿是否随之消失。
#[cfg(windows)]
fn plain_device_variants(iterations: usize, interval: Duration) -> Result<(), WinVideoError> {
    use windows::core::Interface as _;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
    use windows::Win32::Graphics::Direct3D10::ID3D10Multithread;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
        D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_FLAG,
        D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
        D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
    };
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
    };
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1};

    let adapters = rust_tunnel_win_video::d3d11::adapters()?;
    let index = adapters
        .iter()
        .find(|adapter| {
            !adapter.software && adapter.vendor_id == rust_tunnel_win_video::d3d11::VENDOR_INTEL
        })
        .or_else(|| adapters.iter().find(|adapter| !adapter.software))
        .map(|adapter| adapter.index)
        .ok_or_else(|| WinVideoError::Config("没有可用的硬件适配器".to_owned()))?;
    // SAFETY: 与 `d3d11::create` 相同的枚举路径。
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }?;
    // SAFETY: 同上。
    let adapter: IDXGIAdapter = unsafe { factory.EnumAdapters1(index) }?.cast()?;

    let variants: [(&str, D3D11_CREATE_DEVICE_FLAG, bool); 3] = [
        (
            "VIDEO+BGRA，多线程保护（生产同款）",
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            true,
        ),
        (
            "VIDEO+BGRA，无多线程保护",
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            false,
        ),
        ("仅 BGRA，无多线程保护", D3D11_CREATE_DEVICE_BGRA_SUPPORT, false),
    ];
    println!(
        "\n== 设备创建差异（纯 D3D11，间隔 {} ms，每个 {iterations} 次）==",
        interval.as_millis()
    );
    for (label, flags, mt) in variants {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let mut level = D3D_FEATURE_LEVEL_11_0;
        // SAFETY: `adapter` 在调用期间有效；输出指针指向本栈帧。
        let created = unsafe {
            D3D11CreateDevice(
                Some(&adapter),
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                flags,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&raw mut device),
                Some(&raw mut level),
                Some(&raw mut context),
            )
        };
        let (Some(device), Some(context), Ok(())) = (device, context, created) else {
            println!("  {label}: 设备创建失败，跳过");
            continue;
        };
        if mt {
            // SAFETY: 本设备上的有效接口；幂等开关。
            if let Ok(multithread) = device.cast::<ID3D10Multithread>() {
                unsafe { let _ = multithread.SetMultithreadProtected(true); };
            }
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: 1920,
            Height: 1088,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: 0,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut source: Option<ID3D11Texture2D> = None;
        // SAFETY: `desc` 与输出指针都指向本栈帧。
        unsafe { device.CreateTexture2D(&raw const desc, None, Some(&raw mut source)) }.ok();
        let Some(source) = source else {
            println!("  {label}: 源纹理创建失败，跳过");
            continue;
        };
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Usage: D3D11_USAGE_STAGING,
            CPUAccessFlags: u32::try_from(D3D11_CPU_ACCESS_READ.0).unwrap_or(0),
            ..desc
        };
        let mut staging: Option<ID3D11Texture2D> = None;
        // SAFETY: 同上。
        unsafe { device.CreateTexture2D(&raw const staging_desc, None, Some(&raw mut staging)) }
            .ok();
        let Some(staging) = staging else {
            println!("  {label}: staging 创建失败，跳过");
            continue;
        };

        let start = Instant::now();
        let mut readbacks = Vec::with_capacity(iterations);
        for index in 0..iterations {
            let target = start
                .checked_add(interval.saturating_mul(u32::try_from(index).unwrap_or(0)))
                .unwrap_or(start);
            let now = Instant::now();
            if target > now {
                std::thread::sleep(target.duration_since(now));
            }
            let began = Instant::now();
            // SAFETY: 同一设备上的两块同格式同尺寸纹理。
            unsafe {
                context.CopySubresourceRegion(&staging, 0, 0, 0, 0, &source, 0, None);
            }
            let copy_end = Instant::now();
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            // SAFETY: staging 是 STAGING + CPU_ACCESS_READ。
            let map = unsafe {
                context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&raw mut mapped))
            };
            let map_end = Instant::now();
            map.map_err(|err| WinVideoError::Decode(format!("Map 失败: {err}")))?;
            // SAFETY: 与上面的 Map 配对。
            unsafe { context.Unmap(&staging, 0) };
            readbacks.push(
                u64::try_from(copy_end.duration_since(began).as_micros())
                    .unwrap_or(0)
                    .saturating_add(u64::try_from(map_end.duration_since(copy_end).as_micros()).unwrap_or(0)),
            );
        }
        let stalls = readbacks.iter().filter(|value| **value >= STALL_US).count();
        println!(
            "  {label:<34}: p50={:>7}us p95={:>7}us max={:>8}us | 停顿 {stalls}/{iterations}",
            percentile(&readbacks, 50),
            percentile(&readbacks, 95),
            readbacks.iter().copied().max().unwrap_or(0),
        );
    }
    Ok(())
}

/// 每次解码换一个新线程（模拟 `spawn_blocking` 的线程轮换）。
#[cfg(windows)]
fn replay_on_fresh_threads(
    decoder: &mut H264Decoder,
    stream: &Stream,
    offsets: &[Duration],
) -> Result<Run, WinVideoError> {
    let count = stream.packets.len().min(offsets.len());
    let start = Instant::now();
    let mut stats = Vec::with_capacity(count);
    for index in 0..count {
        let target = start
            .checked_add(offsets[index])
            .unwrap_or(start);
        let now = Instant::now();
        if target > now {
            std::thread::sleep(target.duration_since(now));
        }
        let packet = stream.packets[index].clone();
        let began = Instant::now();
        let decoder_ref = &mut *decoder;
        let total_us = std::thread::scope(|scope| {
            scope
                .spawn(move || {
                    let result = decoder_ref.decode(&packet);
                    (
                        result,
                        u64::try_from(began.elapsed().as_micros()).unwrap_or(u64::MAX),
                    )
                })
                .join()
                .map_err(|_| WinVideoError::Decode("解码线程 panic".to_owned()))
        })?;
        let (decoded, total_us) = total_us;
        decoded?;
        let phase = decoder.last_phase_timings();
        let breakdown = decoder.last_readback_breakdown();
        stats.push(FrameStat {
            started: began,
            idle_ms: 0,
            gap_ms: 0.0,
            bytes: stream.packets[index].len().saturating_sub(4),
            idr: stream.idr.get(index).copied().unwrap_or(false),
            skipped: decoder.last_readback_skipped(),
            submit_us: phase.submit_us,
            output_us: phase.output_us,
            get_us: breakdown.get_resource_us,
            copy_us: breakdown.copy_us,
            map_us: breakdown.map_us,
            readback_us: phase.readback_us,
            color_us: phase.color_us,
            total_us,
        });
        if verbose() {
            println!(
                "  #{index:>4} rb={:>8} get={:>6} copy={:>7} map={:>8} dec={:>8}（新线程）",
                stats[index].readback_us,
                stats[index].get_us,
                stats[index].copy_us,
                stats[index].map_us,
                total_us,
            );
        }
    }
    Ok(Run {
        label: "换线程".to_owned(),
        stats,
        hardware: true,
    })
}

// ---------------------------------------------------------------- 主流程

#[cfg(windows)]
fn run() -> Result<(), WinVideoError> {
    let seconds = env_u64("STALL_SECONDS", DEFAULT_SECONDS);
    println!(
        "== decode-stall-probe：{WIDTH}x{HEIGHT} @ {FPS} fps / {BITRATE_KBPS} kbps，每种场景 {seconds} s =="
    );
    println!("== 适配器 ==");
    for adapter in rust_tunnel_win_video::d3d11::adapters()? {
        println!(
            "  [{}] {} vendor=0x{:04X} software={}",
            adapter.index, adapter.name, adapter.vendor_id, adapter.software
        );
    }

    // ---- 编码：Intel QSV 硬件 MFT（不是 NVENC）----
    let mut encoder = MftH264Encoder::new(WIDTH, HEIGHT, FPS, BITRATE_KBPS)?;
    println!(
        "\n== 编码器: {}（CLSID {}）| 设备 {} | {:?} @ {} fps, {} kbps ==",
        encoder.mft_name(),
        encoder.mft_clsid(),
        encoder.adapter_name(),
        encoder.dimensions(),
        encoder.fps(),
        encoder.bitrate_kbps(),
    );

    let animated_count = usize::try_from(seconds.saturating_mul(u64::from(FPS))).unwrap_or(600);
    let animated = encode_stream(&mut encoder, animated_count, true)?;
    animated.describe("动态内容");
    let static_count = usize::try_from(seconds.saturating_mul(20)).unwrap_or(400);
    let still = encode_stream(&mut encoder, static_count, false)?;
    still.describe("静止内容（保活）");
    drop(encoder);

    // ---- 解码器 ----
    let mut decoder = H264Decoder::new_hardware(WIDTH, HEIGHT).or_else(|err| {
        println!("  硬件解码器初始化失败（{err}），回退 Auto");
        H264Decoder::new(WIDTH, HEIGHT)
    })?;
    println!(
        "\n== 解码器: path={} 适配器={} 输出={:?} ==",
        decoder.path().as_str(),
        decoder.adapter_name().unwrap_or("-"),
        decoder.requested_dimensions(),
    );
    if decoder.path() != rust_tunnel_win_video::decode::H264DecodePath::Dxva {
        println!("  ⚠ 本机没有走 DXVA 硬件解码，下面的停顿数字不能代表真机");
    }
    // 特征化阶段先关掉回读预算（= 修复前的阻塞行为），预算的效果在后面单独 A/B。
    decoder.set_readback_options(ReadbackOptions::unbounded());

    let frame_interval = Duration::from_micros(1_000_000 / u64::from(FPS));
    let mut runs: Vec<Run> = Vec::new();

    // A: 30 fps 连续
    println!("\n== A：30 fps 连续（动态）==");
    let offsets = offsets_regular(animated.packets.len(), frame_interval);
    runs.push(replay("A 30fps", &mut decoder, &animated, &offsets)?);

    // B: 5 fps
    println!("\n== B：5 fps（200 ms，动态）==");
    let count = usize::try_from(seconds.saturating_mul(5)).unwrap_or(100);
    let offsets = offsets_regular(count, Duration::from_millis(200));
    runs.push(replay("B 5fps", &mut decoder, &animated, &offsets)?);

    // F/G：决定「保活节奏该不该降到 66 ms」的两档（15 fps = 66.7 ms、10 fps = 100 ms）。
    // 两档都跑动态与静止：静止是保活补编的真实内容，动态用来分离「节奏」与「内容」。
    for (label, fps, stream, kind) in [
        ("F 15fps 动态", 15u64, &animated, "动态"),
        ("F2 15fps 静止", 15, &still, "静止"),
        ("G 10fps 动态", 10, &animated, "动态"),
        ("G2 10fps 静止", 10, &still, "静止"),
    ] {
        let interval = Duration::from_micros(1_000_000 / fps);
        println!("\n== {label}（{} ms，{kind}）==", interval.as_millis());
        let count = usize::try_from(seconds.saturating_mul(fps)).unwrap_or(300);
        let offsets = offsets_regular(count, interval);
        runs.push(replay(label, &mut decoder, stream, &offsets)?);
    }

    // C: 突发 5 帧 + 300 ms 空闲
    println!("\n== C：突发 5 帧 @30fps + 300 ms 空闲（动态）==");
    let cycle = frame_interval.saturating_mul(5).saturating_add(Duration::from_millis(300));
    let bursts = usize::try_from(
        Duration::from_secs(seconds).as_micros() / cycle.as_micros().max(1),
    )
    .unwrap_or(42);
    let offsets = offsets_burst(bursts, 5, frame_interval, Duration::from_millis(300));
    runs.push(replay("C 突发+300ms", &mut decoder, &animated, &offsets)?);

    // D: 250 ms 保活节奏（静止内容）
    println!("\n== D：250 ms（保活节奏，逐字节静止）==");
    let count = usize::try_from(seconds.saturating_mul(4)).unwrap_or(80);
    let offsets = offsets_regular(count, Duration::from_millis(250));
    runs.push(replay("D 250ms 静止", &mut decoder, &still, &offsets)?);

    // E: 1 fps（完全没有保活）
    println!("\n== E：1 fps（静止内容）==");
    let count = usize::try_from(seconds).unwrap_or(20);
    let offsets = offsets_regular(count, Duration::from_secs(1));
    runs.push(replay("E 1fps 静止", &mut decoder, &still, &offsets)?);

    // D2: 250 ms + 动态内容（把节奏与内容分开）
    println!("\n== D2：250 ms（动态内容）==");
    let count = usize::try_from(seconds.saturating_mul(4)).unwrap_or(80);
    let offsets = offsets_regular(count, Duration::from_millis(250));
    runs.push(replay("D2 250ms 动态", &mut decoder, &animated, &offsets)?);

    // D3: 500 ms 保活
    println!("\n== D3：500 ms（静止内容）==");
    let count = usize::try_from(seconds.saturating_mul(2)).unwrap_or(40);
    let offsets = offsets_regular(count, Duration::from_millis(500));
    runs.push(replay("D3 500ms 静止", &mut decoder, &still, &offsets)?);

    // D4: 250 ms 每次两针
    println!("\n== D4：250 ms 每次两针（动态）==");
    let ticks = usize::try_from(seconds.saturating_mul(4)).unwrap_or(80);
    let mut offsets = Vec::with_capacity(ticks.saturating_mul(2));
    for tick in 0..ticks {
        let base = Duration::from_millis(250).saturating_mul(
            u32::try_from(tick).unwrap_or(0),
        );
        offsets.push(base);
        offsets.push(base.saturating_add(Duration::from_millis(1)));
    }
    runs.push(replay("D4 两针", &mut decoder, &animated, &offsets)?);

    // `STALL_ONLY_CADENCE=1`：只跑节奏矩阵（用来快速迭代/现场只关心节奏时）。
    if std::env::var("STALL_ONLY_CADENCE").is_ok_and(|value| value.trim() == "1") {
        println!("\n== 汇总（仅节奏矩阵）==");
        for run in &runs {
            summarize(run);
        }
        return Ok(());
    }

    // ---- 软件解码对照 ----
    println!("\n== 对照组：软件解码（系统内存）==");
    let mut software = H264Decoder::new_with_preference(
        WIDTH,
        HEIGHT,
        H264HardwarePreference::Software,
    )?;
    let count = usize::try_from(seconds.saturating_mul(15)).unwrap_or(300);
    let offsets = offsets_regular(count, frame_interval);
    runs.push(replay("软件 30fps", &mut software, &animated, &offsets)?);
    let count = usize::try_from(seconds.saturating_mul(4)).unwrap_or(80);
    let offsets = offsets_regular(count, Duration::from_millis(250));
    runs.push(replay("软件 250ms", &mut software, &still, &offsets)?);

    // ---- A/B 实验（250 ms 静止节奏）----
    println!("\n== 实验：250 ms 静止节奏下的回读候选 ==");
    let experiment_count = usize::try_from(seconds.saturating_mul(4) / 2).unwrap_or(40);
    let offsets = offsets_regular(experiment_count, Duration::from_millis(250));

    let flush_options = ReadbackOptions {
        flush_before_copy: true,
        ..ReadbackOptions::unbounded()
    };
    decoder.set_readback_options(flush_options);
    runs.push(replay("X1 拷前 Flush", &mut decoder, &still, &offsets)?);

    let poll_options = ReadbackOptions {
        map_mode: MapMode::PollEvent,
        ..ReadbackOptions::unbounded()
    };
    decoder.set_readback_options(poll_options);
    runs.push(replay("X2 DO_NOT_WAIT+EVENT", &mut decoder, &still, &offsets)?);

    let recreate_options = ReadbackOptions {
        recreate_staging: true,
        ..ReadbackOptions::unbounded()
    };
    decoder.set_readback_options(recreate_options);
    runs.push(replay("X3 每帧重建 staging", &mut decoder, &still, &offsets)?);

    decoder.set_readback_options(ReadbackOptions::unbounded());
    runs.push(replay_on_fresh_threads(&mut decoder, &still, &offsets)?);

    // X5：不复用 MFT（纯 D3D11）
    plain_copy_map("X5 纯 D3D11", experiment_count, Duration::from_millis(250))?;

    // X7：拷贝前发一条异步 16x16 预热拷贝
    let warm_options = ReadbackOptions {
        warm_up: rust_tunnel_win_video::decode::WarmUp::OnDemandCopy,
        ..ReadbackOptions::unbounded()
    };
    decoder.set_readback_options(warm_options);
    runs.push(replay("X7 拷前预热", &mut decoder, &still, &offsets)?);
    decoder.set_readback_options(ReadbackOptions::unbounded());

    // X8：后台每 100 ms 一次预热（纯 D3D11 装置，验证「周期保活」）
    {
        let rig = PlainRig::new()?;
        for (label, size, sync) in [
            ("X8 250ms+100ms 预热 64x64", 64u32, false),
            ("X8b 250ms+100ms 预热 1920 全尺寸", 1920, false),
            ("X8c 250ms+100ms 预热 1920+同步", 1920, true),
        ] {
            let warmer = Warmer::start_sized(
                std::sync::Arc::clone(&rig.device),
                Duration::from_millis(100),
                size,
                sync,
            );
            plain_run(&rig, label, experiment_count, Duration::from_millis(250))?;
            drop(warmer);
        }
    }

    // X8d：700 ms 节奏 + 全尺寸同步预热（最坏节奏）
    {
        let rig = PlainRig::new()?;
        let warmer = Warmer::start_sized(
            std::sync::Arc::clone(&rig.device),
            Duration::from_millis(100),
            1920,
            true,
        );
        plain_run(
            &rig,
            "X8d 700ms+100ms 预热 1920+同步",
            experiment_count.min(16),
            Duration::from_millis(700),
        )?;
        drop(warmer);
    }

    // X9：节奏扫描（纯 D3D11），给出「间隔 → 停顿率」曲线
    {
        let rig = PlainRig::new()?;
        let intervals: Vec<Duration> = [100u64, 150, 200, 250, 300, 350, 400, 500, 700, 1000]
            .into_iter()
            .map(Duration::from_millis)
            .collect();
        plain_sweep(&rig, &intervals, 24)?;
    }

    // X9b：格式/尺寸对照——停顿是不是 NV12 staging 拷贝特有的？
    plain_format_compare(24, Duration::from_millis(250))?;

    // X9c：设备创建差异（多线程保护 / VIDEO_SUPPORT）
    plain_device_variants(24, Duration::from_millis(250))?;

    // X10：1 fps 主节奏 + 每 100 ms 空跑
    let hot_seconds = seconds / 2;
    runs.push(replay_with_keepalive(
        "X10 1fps+100ms 空跑",
        &mut decoder,
        &still,
        hot_seconds.max(8),
        Duration::from_millis(100),
    )?);

    // X11/X12：回读预算（修复本身）——250 ms 节奏下 100 ms 预算 vs 不限
    for (label, budget_us) in [
        ("X11 预算 100ms", 100_000u64),
        ("X12 预算 200ms", 200_000),
    ] {
        decoder.set_readback_options(ReadbackOptions {
            readback_budget_us: budget_us,
            ..ReadbackOptions::default()
        });
        runs.push(replay(label, &mut decoder, &still, &offsets)?);
    }
    decoder.set_readback_options(ReadbackOptions::unbounded());

    // ---- 汇总 ----
    println!("\n== 汇总 ==");    for run in &runs {
        summarize(run);
    }
    Ok(())
}
