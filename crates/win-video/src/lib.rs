//! Windows 远程控制数据面的硬件加速层。
//!
//! 本 crate 只做两件与平台强绑定的事：
//!
//! 1. **采集**：`Windows.Graphics.Capture`（WGC）优先，失败回退 DXGI Desktop
//!    Duplication；两者都不可用时由调用方回退到既有的 GDI `BitBlt`（安全桌面/
//!    Windows 登录界面只有 BitBlt 能用）。
//! 2. **编码**：NVENC **H.264 / H.265 4:2:0**（NV12），运行期加载 `nvEncodeAPI64.dll`；
//!    无 NVIDIA 时可用 Media Foundation 硬件 H.264/H.265 编码 MFT。
//!
//! 它**不依赖** `rust-tunnel-remote`，也不定义会话协议；`client-gui` 把这里的
//! [`NvencEncoder`](nvenc::NvencEncoder) 与采集源适配成 remote 的编码/采集 trait。
//!
//! # 为什么固定 4:2:0
//!
//! 消费级 GPU 的 H.264/H.265 硬件编码器都以 4:2:0 为主；4:4:4 硬件编码只有 HEVC
//! 在较新 GPU 上提供，但 4:4:4 **解码**侧没有可用的消费级/系统解码器，混合部署会断。
//! 因此本 crate 不实现色度协商，H.264 与 H.265 都固定 NV12 输入，颜色按 BT.709
//! limited range 打 VUI 标签（`CodecKind` 只区分码流语法）。
//!
//! # 编译面
//!
//! 非 Windows 平台只编译一个空壳（`cargo test` / CI 的 macOS、Linux 任务不受
//! 影响）。所有真实实现都在 `#[cfg(windows)]` 下。

#![cfg_attr(
    not(windows),
    allow(
        unused_crate_dependencies,
        reason = "Windows-only crate：非 Windows 平台没有依赖使用点"
    )
)]

#[cfg(windows)]
pub mod capture;
/// 编码标准（H.264 / H.265），不依赖 Windows API。
pub mod codec;
#[cfg(windows)]
pub mod d3d11;
#[cfg(windows)]
pub mod decode;
#[cfg(windows)]
pub mod mft_encoder;
#[cfg(windows)]
pub mod nvenc;
/// 纯像素工具（BGRA→RGBA、盒式下采样），不依赖 Windows API。
#[cfg(windows)]
pub mod pixels;
#[cfg(windows)]
pub mod probe;
/// GPU 纹理缩放（BGRA → BGRA，VideoProcessor），供 NVENC 零拷贝直通缩放使用。
#[cfg(windows)]
pub mod scale;

/// 逐像素大循环按行并行时的 worker 数（1 = 单线程）。
///
/// 规则：
/// - 输出像素少于 512×512（约 0.26 M，覆盖 480p 以下）时单线程；
/// - 否则取 `available_parallelism().min(4)`；
/// - 再按 `rows` 收敛，保证每个 worker 至少约 64 行（或行对），避免为几行
///   去 spawn 线程。
#[cfg(windows)]
pub(crate) fn worker_count(pixels: usize, rows: usize) -> usize {
    /// 低于该像素数不值得起线程。
    const PARALLEL_MIN_PIXELS: usize = 512 * 512;
    /// 最多 4 个 worker：编码/解码任务本身已在线程池线程上，再多会与主循环抢核。
    const MAX_WORKERS: usize = 4;
    /// 每个 worker 至少处理的输出行（行对）数。
    const MIN_ROWS_PER_WORKER: usize = 64;

    if pixels < PARALLEL_MIN_PIXELS {
        return 1;
    }
    let cpus = std::thread::available_parallelism()
        .map_or(1, std::num::NonZero::get)
        .min(MAX_WORKERS);
    cpus.min((rows / MIN_ROWS_PER_WORKER).max(1))
}

/// 桌面文字编码的 **QP 上限**（画质地板），H.264 4:2:0 的取值范围 `0..=51`。
///
/// 20 ≈ H.264 的「视觉近无损」区间：文字笔画、1 像素表格线都能保住；22 已能看出边缘
/// 略软，26 在 4K 文本上明显发虚，30 以上出现成块的量化痕迹。**这是画质的下限**——
/// 编码器可以为了守住它临时超出目标码率（单帧上限仍由 VBV 约束），这正是 RDP
/// RemoteFX/Parsec 一类实现的做法：按 QP 保证质量，码率只当预算。
pub(crate) const MAX_QP: u32 = 20;

/// QP 上限的环境变量覆盖名（真机 A/B 用，改完重启 GUI 即可，不需要重新编译）。
pub(crate) const MAX_QP_ENV: &str = "RUST_TUNNEL_MAX_QP";

/// 本次进程使用的 QP 上限（见 [`MAX_QP`]）。
pub(crate) fn max_qp() -> u32 {
    parse_max_qp(std::env::var(MAX_QP_ENV).ok().as_deref())
}

/// 解析 QP 上限：缺省/非法/越界（`1..=51` 之外）一律回落到 [`MAX_QP`]。
pub(crate) fn parse_max_qp(raw: Option<&str>) -> u32 {
    raw.and_then(|value| value.trim().parse::<u32>().ok())
        .filter(|qp| (1..=51).contains(qp))
        .unwrap_or(MAX_QP)
}

/// 本 crate 的错误类型。
#[cfg(windows)]
#[derive(Debug, thiserror::Error)]
pub enum WinVideoError {
    /// NVENC 驱动/API 错误。
    #[error("NVENC: {0}")]
    Nvenc(String),
    /// 屏幕采集错误。
    #[error("采集: {0}")]
    Capture(String),
    /// 配置/参数错误（不是驱动或系统调用失败）。
    #[error("配置: {0}")]
    Config(String),
    /// H.264 解码错误（Media Foundation 的 HRESULT 或码流格式问题）。
    #[error("解码: {0}")]
    Decode(String),
    /// H.264 编码错误（Media Foundation 的 HRESULT 或码流格式问题）。
    #[error("编码: {0}")]
    Encode(String),
    /// Win32/WinRT 错误。
    #[error("Windows API: {0}")]
    Windows(#[from] windows::core::Error),
}

#[cfg(all(windows, test))]
mod tests {
    use super::{parse_max_qp, MAX_QP};

    /// QP 上限的环境变量解析：缺省/非法/越界都回落到 [`MAX_QP`]，合法值原样采用。
    #[test]
    fn max_qp_parsing_is_bounded() {
        assert_eq!(parse_max_qp(None), MAX_QP);
        assert_eq!(parse_max_qp(Some("")), MAX_QP);
        assert_eq!(parse_max_qp(Some("abc")), MAX_QP);
        assert_eq!(parse_max_qp(Some("0")), MAX_QP, "0 不是合法 QP");
        assert_eq!(parse_max_qp(Some("52")), MAX_QP, "H.264 QP 上限是 51");
        assert_eq!(parse_max_qp(Some(" 20 ")), 20);
        assert_eq!(parse_max_qp(Some("51")), 51);
    }
}
