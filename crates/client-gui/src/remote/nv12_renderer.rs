//! NV12 帧的 **wgpu 着色器上屏**（GPU 路径）。
//!
//! # 为什么需要它
//!
//! Windows H.264 硬件解码器直接产出紧包 NV12（Y 平面 + 交织 UV 平面）。CPU 回退要先把
//! 整帧转成 RGBA（1080p 约 8 MB、4K 约 33 MB），再交给 egui 上传纹理——转换本身就在 UI
//! 线程上，是控制器侧最贵的一步。这里把 Y/UV 两个平面分别上传成 `R8Unorm` / `Rg8Unorm`
//! 纹理，由片元着色器按 BT.709 limited 转 RGB，**缩放也交给 GPU**（采样器线性过滤）。
//!
//! # 颜色空间约定（与 egui 自身的管线严格一致）
//!
//! egui 对 sRGB 目标的片元输出要求**线性**值（硬件负责编码），对非 sRGB 目标要求**gamma
//! 编码**值。故 shader 有两个片元入口：`fs_linear_target`（把 BT.709 的 R'G'B' 转线性）与
//! `fs_gamma_target`（原样输出），按 `target_format.is_srgb()` 在创建管线时二选一。
//!
//! # 降级
//!
//! 本模块只负责「资源齐备时把一帧画出来」：纹理尺寸/平面长度不合法、资源缺失、或管线
//! 目标格式与当前回调不一致时，`prepare` 跳过上传、`paint` 直接返回——画面上保留上一帧
//! （或什么都不画），**绝不 panic**。Glow/无 wgpu 的路径根本不会创建这个回调，仍走
//! [`super::RemoteView::upload`] 的 CPU 回退。
//!
//! # 增量上传（脏矩形）
//!
//! 全帧与补丁共用同一个 [`CompositeFrame`]（`Arc` 跨线程共享的持久 NV12 缓冲）：
//! - **解码线程**（`FrameSink::on_frame` / `on_patch`）写入并登记脏矩形；
//! - **渲染线程**（[`Nv12Callback::prepare`]）通过 [`CompositeFrame::take_upload`] 取走脏矩形，
//!   只把**这些矩形**按行拷进 staging 后 `write_texture`——补丁路径不再上传整帧，也不再整帧
//!   clone 缓冲。整帧到达（或脏面积过大被合并）时才走整帧上传。
//!
//! 锁的边界：`take_upload` 在锁内完成「判定 + 拷贝 staging」，**锁外**才真正上传；临界区只与
//! 脏面积成正比（整帧上传时才与整帧成正比）。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use egui_wgpu::wgpu;
use rust_tunnel_remote::screen::ScreenPatch;
use rust_tunnel_remote::DecodedNv12;

/// NV12 上屏的内联 WGSL。
///
/// 顶点阶段用 `@builtin(vertex_index)` 生成一个覆盖视口的大三角形（无顶点缓冲）：
/// NDC `(-1,-1) → (3,-1) → (-1,3)` 对应 uv `(0,1) → (2,1) → (0,-1)`。
/// 于是屏幕左下角取纹理底行（v=1）、左上角取纹理首行（v=0）——纹理数据是自顶向下排列的，
/// 方向与 CPU 回退（以及 VP9 纹理路径）一致。
///
/// BT.709 limited range 的定点系数与 `win-video` 的 NV12→NV12 颜色转换同源：
/// `y ∈ [16,235]`、`uv ∈ [16,240]`（按 8 位偏移 128 表达）。
pub(crate) const NV12_SHADER: &str = r"
struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@group(0) @binding(0) var tex_y: texture_2d<f32>;
@group(0) @binding(1) var tex_uv: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;

// 上屏参数（binding 3）：`sharpen` 是 CAS 锐化强度，0 = 关闭。
// 其余三个 `_pad` 字段把结构体凑到 16 字节，满足 uniform 缓冲的对齐要求（WebGPU 对 uniform
// 结构体按 16 字节对齐；只声明一个 f32 会让 min_binding_size 与实际布局对不上）。
struct Params { sharpen: f32, _pad0: f32, _pad1: f32, _pad2: f32 };
@group(0) @binding(3) var<uniform> params: Params;

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VertexOutput {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    var uvs = array<vec2<f32>, 3>(
        vec2<f32>(0.0, 1.0),
        vec2<f32>(2.0, 1.0),
        vec2<f32>(0.0, -1.0),
    );
    var out: VertexOutput;
    out.position = vec4<f32>(positions[index], 0.0, 1.0);
    out.uv = uvs[index];
    return out;
}

// 采样亮度并转成 BT.709 limited 的 [0,1]。
//
// 用 `textureSampleLevel(..., 0.0)` 而不是 `textureSample`：纹理只有 1 个 mip，采样结果不变，
// 但 `textureSample` 要求**统一控制流**（隐式求导），放进 `if` 分支会被 naga 拒绝；显式给 mip
// 级别的版本没有这个限制，CAS 才能在锐化分支里安全调用。
fn sample_y(uv: vec2<f32>) -> f32 {
    return (textureSampleLevel(tex_y, samp, uv, 0.0).r * 255.0 - 16.0) / 219.0;
}

// 一个输出像素在纹理上覆盖多少纹素（放大时 < 1）。
fn footprint(uv: vec2<f32>) -> f32 {
    let dims = vec2<f32>(textureDimensions(tex_y));
    return max(length(dpdx(uv) * dims), length(dpdy(uv) * dims));
}

// CAS（Contrast Adaptive Sharpening）对**亮度平面**做 3x3 锐化，`step` 是一个输出像素在 uv 上的
// 间距。只在放大时调用（见 `sample_rgb`）。
//
// 算法来自 AMD FidelityFX CAS 的公开实现（MIT 许可），此处是自写 WGSL，只取它的核心思想：
//   - `mn`/`mx` 是 8 邻域 + 中心的局部极值，`amp` 度量「这附近有多少已经存在的对比度」：
//     强边缘处 mn≈0、mx≈1 → `min(mn, 1-mx)≈0` → amp≈0 → 不锐化（不出 halo）；
//     文字反锯齿的中间灰处 `min(mn,1-mx)` 大 → amp 大 → 正好补回双线性抹平的边缘。
//   - `peak = -1/(8-3*strength)` 是负的锐化系数（strength∈[0,1] 时落在 -0.125..-0.2）。
//   - 输出是中心值减去「中心与十字邻域均值的差」的一部分：e > 邻域均值 → 更亮，反之更暗。
fn cas_y(uv: vec2<f32>, step: vec2<f32>, strength: f32) -> f32 {
    let e  = sample_y(uv);
    let n  = sample_y(uv + vec2(0.0, -step.y));
    let s  = sample_y(uv + vec2(0.0,  step.y));
    let w  = sample_y(uv + vec2(-step.x, 0.0));
    let ea = sample_y(uv + vec2( step.x, 0.0));
    let nw = sample_y(uv + vec2(-step.x, -step.y));
    let ne = sample_y(uv + vec2( step.x, -step.y));
    let sw = sample_y(uv + vec2(-step.x,  step.y));
    let se = sample_y(uv + vec2( step.x,  step.y));
    let mn = clamp(min(min(min(nw, ne), min(sw, se)), min(min(n, s), min(w, ea))), 0.0, 1.0);
    let mx = clamp(max(max(max(nw, ne), max(sw, se)), max(max(n, s), max(w, ea))), 0.0, 1.0);
    let amp  = sqrt(clamp(min(mn, 1.0 - mx) / max(mx, 1e-4), 0.0, 1.0));
    let peak = -1.0 / (8.0 - 3.0 * clamp(strength, 0.0, 1.0));
    let k = amp * peak;
    return clamp((e + k * (n + s + w + ea)) / (1.0 + 4.0 * k), 0.0, 1.0);
}

// 采样 NV12 并按 BT.709 limited range 转成 gamma 编码的 R'G'B'。
//
// **缩小时的亮度预滤波**：单个双线性 tap 只覆盖 4 个纹素，而缩小（远端 4K 画进小窗口）
// 时一个输出像素实际覆盖更多，欠采样会让文字发虚且随缩放比闪烁。这里按导数估算覆盖
// 面积，缩小时额外做 4 个对角 tap 的近似盒式平均；放大时保持原来的单 tap。
// 色度保持单 tap：它本来就是半分辨率，多 tap 的收益远小于代价。
fn sample_rgb(uv: vec2<f32>, foot: f32) -> vec3<f32> {
    let dims = vec2<f32>(textureDimensions(tex_y));
    let step = foot * 0.25 / dims;
    let boxed = (sample_y(uv + vec2<f32>(-step.x, -step.y))
        + sample_y(uv + vec2<f32>(step.x, -step.y))
        + sample_y(uv + vec2<f32>(-step.x, step.y))
        + sample_y(uv + vec2<f32>(step.x, step.y))) * 0.25;
    var yv = mix(sample_y(uv), boxed, smoothstep(1.2, 2.0, foot));

    // 放大锐化（CAS-Y）。只在**放大**时生效：1:1 与缩小时双线性/盒式采样已经足够，
    // 锐化只会放大欠采样噪声。`params.sharpen` 是 uniform，故这个分支是统一控制流；
    // 导数 `fwidth(uv)` 也在这里**无条件**算好再传进去，避免在分支里求导。
    //
    // 过渡带取 `smoothstep(0.90, 0.99, foot)` 而非更靠上的值：1:1 时 `foot` 恰好是 1.0
    // （`1920 * (1/1920)`），若把过渡带套在 1.0 附近，1:1 也会吃到约 50% 的锐化。整段压到
    // 1.0 以下，才能保证 1:1 与缩小时「逐字节不变」（放大 1.01× 起才开始介入）。
    let ostep = fwidth(uv);
    if (params.sharpen > 0.0) {
        let sharp = cas_y(uv, ostep, params.sharpen);
        yv = mix(yv, sharp, 1.0 - smoothstep(0.90, 0.99, foot));
    }

    let chroma = textureSampleLevel(tex_uv, samp, uv, 0.0).rg;
    let u = (chroma.x * 255.0 - 128.0) / 224.0;
    let v = (chroma.y * 255.0 - 128.0) / 224.0;
    let r = yv + 1.5748 * v;
    let g = yv - 0.1873 * u - 0.4681 * v;
    let b = yv + 1.8556 * u;
    return clamp(vec3<f32>(r, g, b), vec3<f32>(0.0), vec3<f32>(1.0));
}

// 0-1 sRGB gamma → 0-1 线性（与 egui.wgsl 的 linear_from_gamma_rgb 同一条曲线）。
fn linear_from_gamma_rgb(gamma: vec3<f32>) -> vec3<f32> {
    let cutoff = gamma < vec3<f32>(0.04045);
    let lower = gamma / vec3<f32>(12.92);
    let higher = pow((gamma + vec3<f32>(0.055)) / vec3<f32>(1.055), vec3<f32>(2.4));
    return select(higher, lower, cutoff);
}

// sRGB 目标：片元输出线性值，由硬件编码。
@fragment
fn fs_linear_target(in: VertexOutput) -> @location(0) vec4<f32> {
    return vec4<f32>(linear_from_gamma_rgb(sample_rgb(in.uv, footprint(in.uv))), 1.0);
}

// 非 sRGB 目标：片元输出 gamma 编码值（egui 的约定）。
@fragment
fn fs_gamma_target(in: VertexOutput) -> @location(0) vec4<f32> {
    return vec4<f32>(sample_rgb(in.uv, footprint(in.uv)), 1.0);
}
";

/// 一个脏矩形（编码尺寸坐标系）。
///
/// `x`/`y`/`width`/`height` 全部为偶数：NV12 的 UV 平面是半分辨率，奇数边界会让色度块错位。
/// 合法性在写入时已由 [`rust_tunnel_remote::screen::DecodedNv12::validate_patch`] 保证。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirtyRect {
    /// 左上角 X（偶数）。
    pub x: u32,
    /// 左上角 Y（偶数）。
    pub y: u32,
    /// 宽度（偶数）。
    pub width: u32,
    /// 高度（偶数）。
    pub height: u32,
}

impl DirtyRect {
    /// 从线上补丁矩形转过来（字段一一对应；校验在写入时完成）。
    #[must_use]
    pub fn of(patch: &ScreenPatch) -> Self {
        Self {
            x: patch.x,
            y: patch.y,
            width: patch.width,
            height: patch.height,
        }
    }

    /// UV 纹理上的原点（半分辨率）。
    #[must_use]
    pub fn uv_origin(&self) -> wgpu::Origin3d {
        wgpu::Origin3d {
            x: self.x / 2,
            y: self.y / 2,
            z: 0,
        }
    }

    /// UV 纹理上的尺寸（半分辨率，单位是 texel；`Rg8Unorm` 每 texel 2 字节）。
    #[must_use]
    pub fn uv_extent(&self) -> wgpu::Extent3d {
        wgpu::Extent3d {
            width: self.width / 2,
            height: self.height / 2,
            depth_or_array_layers: 1,
        }
    }

    /// 矩形面积（luma 像素数）。
    #[must_use]
    pub fn area(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }
}

/// 脏矩形合并阈值：矩形块数超过它就直接整帧上传（小块 `write_texture` 的固定开销开始占主导）。
const MAX_DIRTY_RECTS: usize = 32;
/// 脏面积占整帧的千分比超过它就直接整帧上传：此时按矩形上传省下的字节抵不过多次
/// `write_texture`（每次都有内部 staging/命令开销）与零散访问。
const FULL_UPLOAD_PERMILLE: u64 = 500;

/// 与渲染线程共享的持久 NV12 合成帧。
///
/// **尺寸在实例生命周期内固定**：尺寸变了就整体换一个新实例（渲染侧据此重建纹理并整帧上传）。
/// 每个实例的 `generation` 唯一（跨会话也不复用），用于让渲染侧区分「还是同一块缓冲」与
/// 「换了一帧」。
pub struct CompositeFrame {
    /// 实例唯一编号（渲染侧据此判断是否需要无条件整帧上传）。
    generation: u64,
    /// 编码宽度（偶数 luma 像素数）。
    width: u32,
    /// 编码高度（偶数 luma 像素数）。
    height: u32,
    /// 数据 + 待上传脏区；两个字段必须**同一把锁**：写入数据与登记脏矩形要原子，
    /// 否则渲染线程可能取走脏区却漏掉刚写进去的数据。
    inner: Mutex<CompositeInner>,
}

/// [`CompositeFrame`] 的锁内状态。
struct CompositeInner {
    /// 当前整帧（Y + UV）。
    data: DecodedNv12,
    /// 内容版本（同实例内单调递增；CPU 回退路径据此决定是否需要重新转换）。
    version: u64,
    /// 自上次被渲染线程取走以来的变化区域。
    pending: PendingUpload,
}

/// 待上传内容。
enum PendingUpload {
    /// 整帧（新全帧到达 / 脏区太多被合并）。
    Full,
    /// 只上传这些矩形（可能为空 = 没有变化）。
    Rects(Vec<DirtyRect>),
}

/// 渲染线程一次上传所需的全部数据（已在锁内拷贝，锁外直接交给 wgpu）。
pub enum UploadChunk {
    /// 整帧（Y 紧包、UV 紧包）。
    Full {
        /// Y 平面（`width * height`）。
        y: Vec<u8>,
        /// 交织 UV 平面（`width * height / 2`）。
        uv: Vec<u8>,
    },
    /// 一批脏矩形（各自紧包）。
    Rects(Vec<StagedRect>),
}

/// 单个脏矩形的紧包 staging（Y 行宽 = `rect.width`，UV 行宽也是 `rect.width`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedRect {
    /// 目标矩形。
    pub rect: DirtyRect,
    /// Y 平面（`rect.width * rect.height`）。
    pub y: Vec<u8>,
    /// 交织 UV 平面（`rect.width * rect.height / 2`）。
    pub uv: Vec<u8>,
}

impl CompositeFrame {
    /// 用一帧整帧数据构造；尺寸/长度不自洽（含奇数边长）时返回 `None`。
    ///
    /// 新实例的 `pending` 是 [`PendingUpload::Full`]：渲染侧第一次见到它必须整帧上传。
    #[must_use]
    pub fn new(width: u32, height: u32, data: DecodedNv12) -> Option<Self> {
        static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return None;
        }
        let (w, h) = (usize::try_from(width).ok()?, usize::try_from(height).ok()?);
        let pixels = w.checked_mul(h)?;
        if data.y.len() != pixels || data.uv.len() != pixels / 2 {
            return None;
        }
        Some(Self {
            generation: NEXT_GENERATION.fetch_add(1, Ordering::Relaxed),
            width,
            height,
            inner: Mutex::new(CompositeInner {
                data,
                version: 1,
                pending: PendingUpload::Full,
            }),
        })
    }

    /// 编码尺寸 `(宽, 高)`。
    #[must_use]
    pub fn dims(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// 实例唯一编号（渲染侧据此判断「换了一帧缓冲」）。
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// 内容版本（同实例内单调递增；无变化时不推进）。
    #[must_use]
    pub fn version(&self) -> u64 {
        self.inner.lock().map_or(0, |inner| inner.version)
    }

    /// 在锁内只读访问整帧数据（CPU 回退路径做 NV12→RGBA 用）。
    pub fn with_data<R>(&self, f: impl FnOnce(&DecodedNv12) -> R) -> R {
        let inner = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&inner.data)
    }

    /// 把一批补丁**原子地**拼进缓冲：先逐块校验，全部合法才写；任一非法则一字节都不改。
    ///
    /// 与旧的「克隆整帧做回滚副本」不同，这里靠 `validate_patch`（与 `apply_patch` 同一套
    /// 前置条件）实现全有或全无，**没有任何整帧拷贝**。
    #[must_use]
    pub fn apply_patches(&self, rects: &[ScreenPatch]) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if rects.is_empty() {
            // 保活批：不改缓冲、不推进版本（渲染侧无需任何动作）。
            return true;
        }
        if !inner.data.apply_patches(self.width, rects) {
            return false;
        }
        inner.version = inner.version.wrapping_add(1);
        if let PendingUpload::Rects(list) = &mut inner.pending {
            list.extend(rects.iter().map(DirtyRect::of));
            if should_upload_full(list, self.width, self.height) {
                inner.pending = PendingUpload::Full;
            }
        }
        true
    }

    /// 渲染线程取一次待上传内容（锁内拷贝 staging）；没有变化时 `None`。
    ///
    /// 取走即清空待上传标记：内容已经拷进返回的 `Vec`，之后新到的补丁会重新登记。若 `force_full`
    /// （纹理刚重建 / 首次上传），无论待上传标记是什么都按整帧处理。
    #[must_use]
    pub fn take_upload(&self, force_full: bool) -> Option<UploadChunk> {
        let mut inner = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if force_full {
            inner.pending = PendingUpload::Full;
        }
        let pending = std::mem::replace(&mut inner.pending, PendingUpload::Rects(Vec::new()));
        match pending {
            // 整帧上传要在锁内拷出整块（代价 ∝ 整帧），锁外再交给 wgpu。
            PendingUpload::Full => Some(UploadChunk::Full {
                y: inner.data.y.clone(),
                uv: inner.data.uv.clone(),
            }),
            PendingUpload::Rects(list) if !list.is_empty() => {
                let mut staged = Vec::with_capacity(list.len());
                for rect in list {
                    let Some(rect) = stage_rect(&inner.data, self.width, rect) else {
                        continue;
                    };
                    staged.push(rect);
                }
                (!staged.is_empty()).then_some(UploadChunk::Rects(staged))
            }
            PendingUpload::Rects(_) => None,
        }
    }
}

/// 脏区是否该合并成整帧上传（矩形过多或面积过大）。
fn should_upload_full(rects: &[DirtyRect], width: u32, height: u32) -> bool {
    if rects.len() > MAX_DIRTY_RECTS {
        return true;
    }
    let area: u64 = rects.iter().map(DirtyRect::area).sum();
    let total = u64::from(width) * u64::from(height);
    total > 0 && area.saturating_mul(1000) >= total.saturating_mul(FULL_UPLOAD_PERMILLE)
}

/// 从紧包整帧里抽出一个矩形，按行拷进紧包 staging。
///
/// 与 [`rust_tunnel_remote::screen::DecodedNv12::apply_patch`] 的坐标约定一致：
/// - Y 行宽 = `frame_width`，取 `(x + col, y + row)`；
/// - UV 行宽也是 `frame_width` **字节**（每个色度对 2 字节覆盖 2 个 luma 像素），
///   取 `(x + col, y/2 + row)`，行数 `height/2`。
///
/// 返回 `None` 表示矩形越界/与缓冲尺寸不自洽（调用方跳过这一块，绝不越界读）。
#[must_use]
pub fn stage_rect(data: &DecodedNv12, frame_width: u32, rect: DirtyRect) -> Option<StagedRect> {
    let frame_width = usize::try_from(frame_width).ok()?;
    if frame_width == 0 {
        return None;
    }
    let frame_height = data.y.len().checked_div(frame_width)?;
    if frame_height == 0 || data.y.len() != frame_width.checked_mul(frame_height)? {
        return None;
    }
    if data.uv.len() != frame_width.checked_mul(frame_height / 2)? {
        return None;
    }
    let (x, y) = (usize::try_from(rect.x).ok()?, usize::try_from(rect.y).ok()?);
    let (w, h) = (
        usize::try_from(rect.width).ok()?,
        usize::try_from(rect.height).ok()?,
    );
    if w == 0 || h == 0 || !w.is_multiple_of(2) || !h.is_multiple_of(2) {
        return None;
    }
    if !x.is_multiple_of(2) || !y.is_multiple_of(2) {
        return None;
    }
    if x.checked_add(w)? > frame_width || y.checked_add(h)? > frame_height {
        return None;
    }

    let mut y_bytes = Vec::with_capacity(w.checked_mul(h)?);
    for row in 0..h {
        let start = (y + row).checked_mul(frame_width)?.checked_add(x)?;
        y_bytes.extend_from_slice(data.y.get(start..start.checked_add(w)?)?);
    }
    let mut uv_bytes = Vec::with_capacity(w.checked_mul(h / 2)?);
    for row in 0..(h / 2) {
        let start = (y / 2 + row).checked_mul(frame_width)?.checked_add(x)?;
        uv_bytes.extend_from_slice(data.uv.get(start..start.checked_add(w)?)?);
    }
    Some(StagedRect {
        rect,
        y: y_bytes,
        uv: uv_bytes,
    })
}

/// 放大锐化的默认强度（百分比；0.3–0.5 是桌面文本的合适区间，再高会出描边感）。
///
/// 这是**单一事实来源**：`prefs.rs` 的默认值也引用它，改这里即可同时改「全新安装」与「旧配置
/// 缺键」的默认观感。只作用于控制端本机上屏的亮度平面，不上线、不影响对端。
pub const CAS_SHARPEN_DEFAULT_PERCENT: u8 = 42;

/// 百分比 → CAS 的 `sharpness`（0.0–1.0，钳到 100）。
///
/// 持久化用 0–100 的整数（与 `remote_audio_volume` 一致，避免 f32 serde），只在写 uniform 前
/// 转成着色器要的浮点；越界值（手改 `gui.toml`）一并收敛。
#[must_use]
pub fn cas_sharpen_strength(percent: u8) -> f32 {
    f32::from(percent.min(100)) / 100.0
}

/// 一帧 NV12 的绘制回调（`egui_wgpu::Callback` 的数据载体）。
///
/// `frame` 是**共享**的合成缓冲：解码线程原地打补丁，渲染侧每次 `prepare` 取走自上次以来累积的
/// 脏矩形（见 [`CompositeFrame::take_upload`]）。因此同一份 `Arc` 可以被反复绘制而不重复上传。
pub struct Nv12Callback {
    /// 与解码线程共享的持久合成缓冲（含全帧 + 已应用的补丁）。
    pub frame: Arc<CompositeFrame>,
    /// 当前 egui 渲染目标格式（决定片元入口与管线格式）。
    pub target_format: wgpu::TextureFormat,
    /// 上屏锐化强度（CAS 的 `sharpness`）；`0.0` = 关闭。调用方按用户开关传常量，
    /// 由 `prepare` 写进 binding 3 的 uniform（着色器只在画面被放大时真正使用它）。
    pub sharpen: f32,
}

/// 存在 [`egui_wgpu::CallbackResources`] 里的 GPU 资源（管线、纹理、绑定组）。
///
/// 纹理与绑定组只在**尺寸变化**时重建；尺寸不变时每帧只做 `queue.write_texture`。
struct Nv12Resources {
    /// 管线编译时使用的目标格式；与回调不一致时整组资源重建。
    target_format: wgpu::TextureFormat,
    /// 渲染管线（片元入口已按 sRGB/非 sRGB 选定）。
    pipeline: wgpu::RenderPipeline,
    /// 绑定组布局（Y、UV、采样器、锐化参数）。
    bind_group_layout: wgpu::BindGroupLayout,
    /// 上屏参数（binding 3）：每帧写入 `[sharpen, 0, 0, 0]`。开着才写非零值。
    params_buffer: wgpu::Buffer,
    /// 采样器：线性过滤 + ClampToEdge（GPU 缩放靠它）。
    sampler: wgpu::Sampler,
    /// Y 平面纹理（`R8Unorm`，`width × height`）。
    texture_y: Option<wgpu::Texture>,
    /// UV 平面纹理（`Rg8Unorm`，`width/2 × height/2`）。
    texture_uv: Option<wgpu::Texture>,
    /// 与上面两张纹理对应的绑定组。
    bind_group: Option<wgpu::BindGroup>,
    /// 当前纹理尺寸；变化时重建纹理与绑定组。
    size: Option<(u32, u32)>,
}

impl Nv12Resources {
    /// 创建管线、布局与采样器（纹理等尺寸已知时再建）。
    #[expect(
        clippy::too_many_lines,
        reason = "管线/绑定布局/采样器/参数缓冲的创建集中在一处，拆开只会让资源所有权来回传"
    )]
    fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("nv12_shader"),
            source: wgpu::ShaderSource::Wgsl(NV12_SHADER.into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("nv12_bind_group_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                // binding 3：锐化强度 uniform（16 字节，见着色器里的 `Params`）。
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: std::num::NonZeroU64::new(16),
                    },
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("nv12_pipeline_layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        // sRGB 目标要求片元输出线性值；非 sRGB 目标要求 gamma 编码值（见 egui.wgsl）。
        let fragment_entry = if target_format.is_srgb() {
            "fs_linear_target"
        } else {
            "fs_gamma_target"
        };

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("nv12_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..wgpu::PrimitiveState::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some(fragment_entry),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("nv12_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..wgpu::SamplerDescriptor::default()
        });

        // 锐化参数缓冲（binding 3）：内容每帧由 `prepare` 覆盖，故只给 UNIFORM | COPY_DST。
        let params_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("nv12_params"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            target_format,
            pipeline,
            bind_group_layout,
            params_buffer,
            sampler,
            texture_y: None,
            texture_uv: None,
            bind_group: None,
            size: None,
        }
    }

    /// 把共享合成帧的**增量**上传到 GPU。
    ///
    /// 上传内容由 [`CompositeFrame::take_upload`] 决定：全帧到达（或脏区被合并）时整帧上传，
    /// 否则只上传自上次以来累积的脏矩形。任何前置条件不满足（零尺寸/奇数尺寸）都直接跳过——
    /// 保留上一帧比画错或崩溃好。
    fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, frame: &CompositeFrame) {
        let (width, height) = frame.dims();
        // 4:2:0 要求偶数尺寸；零尺寸无法建纹理。
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return;
        }
        // 纹理重建（尺寸变化 / 首次）后必须整帧上传：新纹理里什么都没有。
        let force_full = self.ensure_textures(device, width, height);
        let (Some(texture_y), Some(texture_uv)) = (self.texture_y.as_ref(), self.texture_uv.as_ref())
        else {
            return;
        };
        // 取脏矩形（锁内只拷贝 staging，锁外上传）。
        let Some(chunk) = frame.take_upload(force_full) else {
            return;
        };
        match chunk {
            UploadChunk::Full { y, uv } => {
                write_full(queue, texture_y, texture_uv, width, height, &y, &uv);
            }
            UploadChunk::Rects(rects) => write_rects(queue, texture_y, texture_uv, &rects),
        }
    }

    /// 尺寸变化时重建纹理与绑定组；返回是否重建过（重建过 = 必须整帧上传）。
    fn ensure_textures(&mut self, device: &wgpu::Device, width: u32, height: u32) -> bool {
        if self.size == Some((width, height)) {
            return false;
        }
        let (uv_w, uv_h) = (width / 2, height / 2);
        let texture_y = create_texture(device, "nv12_y", width, height, wgpu::TextureFormat::R8Unorm);
        let texture_uv = create_texture(device, "nv12_uv", uv_w, uv_h, wgpu::TextureFormat::Rg8Unorm);
        let view_y = texture_y.create_view(&wgpu::TextureViewDescriptor::default());
        let view_uv = texture_uv.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("nv12_bind_group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view_y),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&view_uv),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: self.params_buffer.as_entire_binding(),
                },
            ],
        });
        self.texture_y = Some(texture_y);
        self.texture_uv = Some(texture_uv);
        self.bind_group = Some(bind_group);
        self.size = Some((width, height));
        true
    }
}

/// 整帧上传（Y/UV 两张纹理各自 `width` 字节行宽）。
fn write_full(
    queue: &wgpu::Queue,
    texture_y: &wgpu::Texture,
    texture_uv: &wgpu::Texture,
    width: u32,
    height: u32,
    y: &[u8],
    uv: &[u8],
) {
    // Y：每行 `width` 字节，共 `height` 行。
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: texture_y,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        y,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(width),
            rows_per_image: Some(height),
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );

    // UV：每行 `uv_w * 2 == width` 字节。
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: texture_uv,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        uv,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(width),
            rows_per_image: Some(height / 2),
        },
        wgpu::Extent3d {
            width: width / 2,
            height: height / 2,
            depth_or_array_layers: 1,
        },
    );
}

/// 只上传脏矩形（Y 与 UV 各一块）。
///
/// `write_texture` 的行距不要求 256 字节对齐（它内部会 staging 到对齐布局），故紧包行宽
/// `rect.width` 直接可用；整帧路径的 `bytes_per_row = width` 也一直是这么做的。
fn write_rects(
    queue: &wgpu::Queue,
    texture_y: &wgpu::Texture,
    texture_uv: &wgpu::Texture,
    rects: &[StagedRect],
) {
    for staged in rects {
        let rect = staged.rect;
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: texture_y,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: rect.x,
                    y: rect.y,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            &staged.y,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(rect.width),
                rows_per_image: Some(rect.height),
            },
            wgpu::Extent3d {
                width: rect.width,
                height: rect.height,
                depth_or_array_layers: 1,
            },
        );
        // UV 平面是半分辨率：原点是 `(x/2, y/2)`、尺寸 `w/2 × h/2`；
        // 行距仍是 `rect.width` 字节（与 Y 相同，见 `stage_rect`）。
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: texture_uv,
                mip_level: 0,
                origin: rect.uv_origin(),
                aspect: wgpu::TextureAspect::All,
            },
            &staged.uv,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(rect.width),
                rows_per_image: Some(rect.height / 2),
            },
            rect.uv_extent(),
        );
    }
}

/// 建一张只用于采样（`TEXTURE_BINDING | COPY_DST`）的二维纹理。
fn create_texture(
    device: &wgpu::Device,
    label: &str,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

impl egui_wgpu::CallbackTrait for Nv12Callback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen_descriptor: &egui_wgpu::ScreenDescriptor,
        _egui_encoder: &mut wgpu::CommandEncoder,
        callback_resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        // 资源不存在或目标格式变了（理论上一个进程内不会变）→ 重建整组资源。
        let needs_rebuild = callback_resources
            .get::<Nv12Resources>()
            .is_none_or(|resources| resources.target_format != self.target_format);
        if needs_rebuild {
            callback_resources.insert(Nv12Resources::new(device, self.target_format));
        }
        if let Some(resources) = callback_resources.get_mut::<Nv12Resources>() {
            // 每帧写锐化强度：`sharpen` 被钳到 [0,1]（手改偏好/调用方传错值也不会让 CAS 系数跑飞）。
            // 16 字节 = [sharpen, 0, 0, 0] 的小端字节，前 4 字节即 `Params.sharpness` 的位置。
            let mut params = [0u8; 16];
            params[..4].copy_from_slice(&self.sharpen.clamp(0.0, 1.0).to_le_bytes());
            queue.write_buffer(&resources.params_buffer, 0, &params);
            // 资源刚重建时纹理是空的，必须整帧上传（`upload` 内部按 `size` 判断）。
            resources.upload(device, queue, &self.frame);
        }
        Vec::new()
    }

    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        callback_resources: &egui_wgpu::CallbackResources,
    ) {
        // egui-wgpu 在调用 `paint` 前已把视口设成本回调的 `rect`、scissor 设成 clip_rect；
        // 这里只绑管线与资源。资源缺失（上传被跳过、尚未建出绑定组）→ 什么都不画。
        let Some(resources) = callback_resources.get::<Nv12Resources>() else {
            return;
        };
        if resources.target_format != self.target_format {
            return;
        }
        let Some(bind_group) = resources.bind_group.as_ref() else {
            return;
        };
        render_pass.set_pipeline(&resources.pipeline);
        render_pass.set_bind_group(0, bind_group, &[]);
        render_pass.draw(0..3, 0..1);
    }
}

#[cfg(test)]
mod tests {
    //! 只覆盖**纯逻辑**：脏矩形几何、按行 staging 拷贝、合成缓冲的原子应用与脏区合并，
    //! 以及内联 WGSL 的语义校验。不需要 GPU（`take_upload` 只做 CPU 侧拷贝）。

    use super::*;

    /// 内联着色器必须能通过 naga 的解析与语义校验。
    ///
    /// wgpu 只在**运行期**创建管线时校验 WGSL：写错一个函数重载（例如 `mix`/`smoothstep`
    /// 的标量重载）不会有编译错误，而是用户一开会话就 panic。这条把它提前到测试里。
    #[test]
    fn nv12_shader_is_valid_wgsl() {
        let module = naga::front::wgsl::parse_str(NV12_SHADER)
            .expect("NV12 着色器应能通过 WGSL 解析");
        let mut validator = naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::empty(),
        );
        validator
            .validate(&module)
            .expect("NV12 着色器应通过语义校验");
    }

    /// 百分比 → `sharpness` 的映射（越界钳到 100）。
    #[test]
    fn cas_strength_percent_maps_to_sharpness() {
        assert!(cas_sharpen_strength(0).abs() < f32::EPSILON);
        assert!((cas_sharpen_strength(42) - 0.42).abs() < 1e-6);
        assert!((cas_sharpen_strength(100) - 1.0).abs() < f32::EPSILON);
        assert!(
            (cas_sharpen_strength(150) - 1.0).abs() < f32::EPSILON,
            "越界应钳到 100"
        );
    }

    /// CAS 公式的**标量版本**，与着色器的 `cas_y` 逐行对应（3×3 网格按 `[行][列]`，`[1][1]`
    /// 是中心）。这份实现是「规格锁」：GPU 着色器写错时这一侧先拦下，不必等真机看画面。
    ///
    /// 变量名（`e/n/s/w/ea/nw/...`）刻意与 WGSL 完全一致，方便逐行比对；故豁免单字母命名。
    #[expect(
        clippy::many_single_char_names,
        reason = "变量名与 WGSL `cas_y` 逐字对应，改名反而破坏逐行比对"
    )]
    fn cas_y_scalar(g: [[f32; 3]; 3], strength: f32) -> f32 {
        let e = g[1][1];
        let n = g[0][1];
        let s = g[2][1];
        let w = g[1][0];
        let ea = g[1][2];
        let nw = g[0][0];
        let ne = g[0][2];
        let sw = g[2][0];
        let se = g[2][2];
        let mn = nw
            .min(ne)
            .min(sw)
            .min(se)
            .min(n)
            .min(s)
            .min(w)
            .min(ea)
            .clamp(0.0, 1.0);
        let mx = nw
            .max(ne)
            .max(sw)
            .max(se)
            .max(n)
            .max(s)
            .max(w)
            .max(ea)
            .clamp(0.0, 1.0);
        let amp = (mn.min(1.0 - mx) / mx.max(1e-4)).clamp(0.0, 1.0).sqrt();
        let peak = -1.0 / (8.0 - 3.0 * strength.clamp(0.0, 1.0));
        let k = amp * peak;
        ((e + k * (n + s + w + ea)) / (1.0 + 4.0 * k)).clamp(0.0, 1.0)
    }

    /// 着色器**实际生效**的路径：`strength <= 0` 时原样返回中心值，对应 `if (params.sharpen > 0.0)`。
    ///
    /// 规格锁必须把这道闸门一起建模：AMD 原式在 strength=0 时 `peak = -1/8` 仍会锐化，
    /// 让「0 = 关闭」的是候选着色器里的分支，而非公式本身。
    fn cas_y_applied(g: [[f32; 3]; 3], strength: f32) -> f32 {
        if strength <= 0.0 {
            return g[1][1];
        }
        cas_y_scalar(g, strength)
    }

    /// 常数场：DC 增益必须为 1（否则整幅画面的亮度/对比度会被锐化整体推偏）。
    #[test]
    fn cas_is_dc_neutral_on_a_constant_field() {
        let g = [[0.5f32; 3]; 3];
        let out = cas_y_applied(g, cas_sharpen_strength(CAS_SHARPEN_DEFAULT_PERCENT));
        assert!((out - 0.5).abs() < 1e-6, "常数场输出应仍为 0.5，实际 {out}");
    }

    /// 硬边：8 邻域跨满 0..1 时 amp=0，输出严格等于中心——强边缘不出 halo（白边/黑边）。
    #[test]
    fn cas_does_not_halo_at_a_hard_edge() {
        let g = [
            [0.0, 1.0, 0.0],
            [1.0, 0.5, 1.0],
            [0.0, 1.0, 0.0],
        ];
        let out = cas_y_applied(g, cas_sharpen_strength(CAS_SHARPEN_DEFAULT_PERCENT));
        assert!((out - 0.5).abs() < 1e-6, "硬边处应保持中心值 0.5，实际 {out}");
    }

    /// 软边（0.4–0.6 的缓坡）：中心低于邻域均值处更暗、高于均值处更亮——这才是真正补锐。
    #[test]
    fn cas_sharpens_soft_edges_toward_the_local_extremes() {
        // 中心 0.42，十字邻域全是 0.50，角上是 0.40..0.60 的缓坡：中心低于均值 → 应更暗。
        let low = [
            [0.40, 0.50, 0.60],
            [0.50, 0.42, 0.50],
            [0.60, 0.50, 0.40],
        ];
        let out_low = cas_y_applied(low, cas_sharpen_strength(CAS_SHARPEN_DEFAULT_PERCENT));
        assert!(
            out_low < 0.42 - 1e-3,
            "软边低值应被压得更低（<0.42），实际 {out_low}"
        );

        // 对称：中心 0.58，高于均值 → 应更亮。
        let high = [
            [0.40, 0.50, 0.60],
            [0.50, 0.58, 0.50],
            [0.60, 0.50, 0.40],
        ];
        let out_high = cas_y_applied(high, cas_sharpen_strength(CAS_SHARPEN_DEFAULT_PERCENT));
        assert!(
            out_high > 0.58 + 1e-3,
            "软边高值应被推得更高（>0.58），实际 {out_high}"
        );
    }

    /// strength=0 = 关闭：输出必须逐位等于中心（分支闸门生效，而非公式的 -1/8 底噪）。
    #[test]
    fn cas_strength_zero_is_a_no_op() {
        let g = [
            [0.0, 1.0, 0.0],
            [1.0, 0.5, 1.0],
            [0.0, 1.0, 0.0],
        ];
        // 不直接比较 f32 相等（`clippy::float_cmp`）；闸门是原样返回中心值，差值必为 0。
        let out = cas_y_applied(g, 0.0);
        assert!((out - 0.5).abs() < f32::EPSILON, "strength=0 必须原样返回中心值，实际 {out}");
    }

    /// 8×8 的紧包 NV12：Y = `0..64`、UV = `100..132`（取值便于手算期望字节）。
    fn frame_8x8() -> DecodedNv12 {
        DecodedNv12 {
            y: (0..64).collect(),
            uv: (100..132).collect(),
        }
    }

    fn frame_16x16() -> DecodedNv12 {
        DecodedNv12 {
            y: vec![0; 16 * 16],
            uv: vec![0; 16 * 8],
        }
    }

    fn patch(x: u32, y: u32, width: u32, height: u32) -> ScreenPatch {
        let len = usize::try_from(width * height * 3 / 2).unwrap();
        ScreenPatch {
            x,
            y,
            width,
            height,
            nv12: vec![0x5A; len],
        }
    }

    #[test]
    fn dirty_rect_uv_geometry_is_half_resolution() {
        let rect = DirtyRect {
            x: 100,
            y: 40,
            width: 20,
            height: 10,
        };
        assert_eq!(
            rect.uv_origin(),
            wgpu::Origin3d {
                x: 50,
                y: 20,
                z: 0
            }
        );
        assert_eq!(
            rect.uv_extent(),
            wgpu::Extent3d {
                width: 10,
                height: 5,
                depth_or_array_layers: 1
            }
        );
        assert_eq!(rect.area(), 200);
    }

    /// staging 必须只拷矩形内的行：Y 行宽 = `rect.width`，UV 行偏移用 `(y/2)*width + x`。
    #[test]
    fn stage_rect_copies_only_the_rect_rows() {
        let data = frame_8x8();
        let rect = DirtyRect {
            x: 2,
            y: 2,
            width: 4,
            height: 2,
        };
        let staged = stage_rect(&data, 8, rect).expect("合法矩形应能取出");
        // Y：第 2/3 行从 x=2 起各 4 字节。
        assert_eq!(staged.y, vec![18, 19, 20, 21, 26, 27, 28, 29]);
        // UV：y=2 → UV 行 1，起始字节偏移 = 1*8 + 2 = 10（每行 8 字节 = 整帧宽度）。
        assert_eq!(staged.uv, vec![110, 111, 112, 113]);
        assert_eq!(staged.rect, rect);
    }

    #[test]
    fn stage_rect_rejects_illegal_rects() {
        let data = frame_8x8();
        // 越界（x+w > 8）、越界（y+h > 8）、奇数坐标、奇数尺寸、零尺寸。
        for rect in [
            DirtyRect { x: 6, y: 0, width: 4, height: 2 },
            DirtyRect { x: 0, y: 6, width: 2, height: 4 },
            DirtyRect { x: 1, y: 0, width: 2, height: 2 },
            DirtyRect { x: 0, y: 1, width: 2, height: 2 },
            DirtyRect { x: 0, y: 0, width: 3, height: 2 },
            DirtyRect { x: 0, y: 0, width: 2, height: 3 },
            DirtyRect { x: 0, y: 0, width: 0, height: 2 },
        ] {
            assert!(stage_rect(&data, 8, rect).is_none(), "{rect:?} 应被拒");
        }
        // 帧宽与缓冲不自洽时也不越界读。
        assert!(stage_rect(&data, 7, DirtyRect { x: 0, y: 0, width: 2, height: 2 }).is_none());
    }

    /// 全帧构造时 `pending = Full`；应用补丁后只登记脏矩形，**不替换/不拷贝**整帧。
    #[test]
    fn apply_patches_marks_dirty_rects_instead_of_full_frame() {
        let composite = CompositeFrame::new(8, 8, frame_8x8()).expect("8×8 合法");
        assert!(matches!(composite.take_upload(false), Some(UploadChunk::Full { .. })));

        assert!(composite.apply_patches(&[patch(2, 2, 4, 2)]));
        match composite.take_upload(false) {
            Some(UploadChunk::Rects(rects)) => {
                assert_eq!(rects.len(), 1);
                assert_eq!(
                    rects[0].rect,
                    DirtyRect {
                        x: 2,
                        y: 2,
                        width: 4,
                        height: 2
                    }
                );
            }
            _ => panic!("补丁应只登记脏矩形"),
        }
        // 数据确实原地写进共享缓冲，矩形外不动。
        composite.with_data(|data| {
            assert_eq!(data.y[2 * 8 + 2], 0x5A);
            assert_eq!(data.y[3 * 8 + 5], 0x5A);
            assert_eq!(data.y[0], 0, "矩形外不应被写");
            assert_eq!(data.uv[0], 100, "矩形外不应被写");
        });
        // 取走即清：没有新变化就不再上传。
        assert!(composite.take_upload(false).is_none());
    }

    /// 一批补丁要么全应用、要么一字节不改；非法批次不得推进版本（否则渲染侧会白跑）。
    #[test]
    fn apply_patches_is_all_or_nothing() {
        let original_y = frame_8x8().y;
        let composite = CompositeFrame::new(8, 8, frame_8x8()).expect("8×8 合法");
        let before = composite.version();
        assert!(composite.take_upload(false).is_some());

        // 第二块越界：第一块也不能留下。
        assert!(!composite.apply_patches(&[patch(2, 2, 4, 2), patch(6, 0, 4, 2)]));
        assert_eq!(composite.version(), before, "失败批次不应推进版本");
        composite.with_data(|data| assert_eq!(data.y, original_y, "整批失败时不得留半截"));
        assert!(composite.take_upload(false).is_none(), "失败批次不应登记脏区");
    }

    #[test]
    fn empty_patch_batch_is_a_noop() {
        let composite = CompositeFrame::new(8, 8, frame_8x8()).expect("8×8 合法");
        assert!(composite.take_upload(false).is_some());
        let before = composite.version();
        assert!(composite.apply_patches(&[]));
        assert_eq!(composite.version(), before, "保活空批不应推进版本");
        assert!(composite.take_upload(false).is_none());
    }

    /// 累积脏面积达到整帧 50% 时直接整帧上传（小块 `write_texture` 的固定开销不划算）。
    ///
    /// 脏区在 `take_upload` 时清空，所以「累积」必须发生在两次上屏之间。
    #[test]
    fn dirty_area_over_half_the_frame_coalesces_to_full() {
        let composite = CompositeFrame::new(16, 16, frame_16x16()).expect("16×16 合法");
        assert!(composite.take_upload(false).is_some());
        // 每块 8×4 = 32 像素，整帧 256；阈值是 128（50%）。
        assert!(composite.apply_patches(&[patch(0, 0, 8, 4)]));
        assert!(composite.apply_patches(&[patch(8, 0, 8, 4)]));
        assert!(composite.apply_patches(&[patch(0, 4, 8, 4)]));
        assert!(composite.apply_patches(&[patch(8, 4, 8, 4)]));
        assert!(
            matches!(composite.take_upload(false), Some(UploadChunk::Full { .. })),
            "128/256 = 50% 应合并成整帧上传"
        );
        // 清空后单块小补丁仍按矩形上传。
        assert!(composite.apply_patches(&[patch(0, 0, 8, 4)]));
        assert!(matches!(
            composite.take_upload(false),
            Some(UploadChunk::Rects(_))
        ));
    }

    #[test]
    fn should_upload_full_caps_rect_count_and_area() {
        let tiny = [DirtyRect {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        }; MAX_DIRTY_RECTS + 1];
        assert!(should_upload_full(&tiny, 1920, 1080), "块数超限应整帧");
        let one = [DirtyRect {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        }];
        assert!(!should_upload_full(&one, 1920, 1080));
        let half = [DirtyRect {
            x: 0,
            y: 0,
            width: 1920,
            height: 540,
        }];
        assert!(should_upload_full(&half, 1920, 1080), "面积达到 50% 应整帧");
    }
}
