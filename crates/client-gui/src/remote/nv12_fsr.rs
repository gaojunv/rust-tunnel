//! FSR1（EASU + RCAS）放大路径的 GPU 资源与着色器（仅控制端本机上屏用）。
//!
//! # 为什么是两 pass
//!
//! 源是 NV12：Y 全分辨率、UV 半分辨率。只对 **Y** 做 EASU（边缘自适应放大）+ RCAS（对比度
//! 自适应锐化），UV 继续用双线性，最后按 BT.709 limited 转 R'G'B'。EASU 与 RCAS 采样模型不同
//! （EASU 是 12-tap 方向核、RCAS 是 3×3 十字 5-tap），拆成：
//!
//! 1. **EASU pass（离屏）**：`tex_src`(R8Unorm) → `easu_tex`(R8Unorm，尺寸 = 回调 rect 的物理
//!    像素)。此 pass 的目标就是 `easu_tex`、视口铺满，故片元的 `@builtin(position)` 正是目标像素。
//! 2. **Present pass（画进 egui 目标，视口 = rect）**：对 `easu_tex` 做 RCAS（`textureLoad`，
//!    坐标 `floor(uv * textureDimensions(easu_tex))`，clamp），采样 `tex_uv` 拿色度，再转 RGB。
//!
//! RCAS 天然作用在 EASU 输出上（等价参考里的「先 EASU 再 RCAS」），只需一张离屏纹理。
//!
//! # 许可
//!
//! EASU/RCAS 的算法来自 AMD FidelityFX-FSR 1.20210629（MIT，Copyright (c) 2021 Advanced
//! Micro Devices, Inc.），本文件是**移植**（自写 WGSL，未拷贝参考 `.h`）。MIT 声明全文见同目录
//! `fsr1_NOTICE.txt`。本仓不引入任何 GPL/AGPL 代码。

use std::num::NonZeroU64;

use egui_wgpu::wgpu;

/// 判断当前帧该不该走 FSR：用户开启、强度 > 0、且画面处于 **1.01×–4× 的放大**区间。
///
/// EASU 的规格支持 1×–4×；更大倍率（或 1:1 / 缩小）回退到既有的单 pass（双线性 / 盒式），
/// 避免在不适用的缩放比上出伪影。纯函数，便于单测与 harness 复用。
#[must_use]
#[expect(
    clippy::cast_precision_loss,
    reason = "纹理尺寸远小于 2^24，u32→f32 精度足够（只做放大倍率判定）"
)]
pub(crate) fn fsr_active(
    use_fsr: bool,
    sharpen: f32,
    in_size: (u32, u32),
    out_size: (u32, u32),
) -> bool {
    if !use_fsr || sharpen <= 0.0 {
        return false;
    }
    let (iw, ih) = in_size;
    let (ow, oh) = out_size;
    if iw == 0 || ih == 0 || ow == 0 || oh == 0 {
        return false;
    }
    let rw = ow as f32 / iw as f32;
    let rh = oh as f32 / ih as f32;
    rw >= 1.01 && rh >= 1.01 && rw <= 4.0 && rh <= 4.0
}

/// EASU 着色器：12-tap 方向核，把 `tex_src` 放大到离屏目标尺寸。
///
/// 与参考 `FsrEasuF` 的对应关系写在代码里；参考里 `APrxLoRcpF1`/`APrxLoRsqF1` 是近似倒数/逆平方根，
/// 这里一律换成精确的 `1/max(x,1e-8)` 与 `inverseSqrt(max(x,1e-16))`（精度更高，代价可忽略）。
const EASU_SHADER: &str = r"
struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// [in_w, in_h, out_w, out_h]
struct EasuParams { in_w: f32, in_h: f32, out_w: f32, out_h: f32 };

@group(0) @binding(0) var tex_src: texture_2d<f32>;
@group(0) @binding(1) var<uniform> easu_params: EasuParams;

@vertex
fn vs_easu(@builtin(vertex_index) index: u32) -> VOut {
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
    var out: VOut;
    out.pos = vec4<f32>(positions[index], 0.0, 1.0);
    out.uv = uvs[index];
    return out;
}

// 取一个 Y texel（整数坐标，clamp 到边界），并把 BT.709 limited 的码值展开到 [0,1]。
//
// EASU 的权重对仿射变换不变（dir/len 是比值、tap 是加权平均），但 RCAS 的 `hitMax`
// 里 `1.0` 是按「白 = 1」标定的，必须在展开域里算；顺带让 R8 离屏纹理用满 0..255
// 的码值（原始 Y 只占 16..235，会浪费约 8% 的量化级）。
fn load_y(p: vec2<i32>) -> f32 {
    let dims = vec2<i32>(textureDimensions(tex_src));
    let hi = max(dims - vec2<i32>(1, 1), vec2<i32>(0, 0));
    let raw = textureLoad(tex_src, clamp(p, vec2<i32>(0, 0), hi), 0).r;
    return (raw * 255.0 - 16.0) / 219.0;
}

// FsrEasuSetF 的标量版：给定双线性权重 w 与五元组 (lA..lE)，返回 (dirX*w, dirY*w, lenDelta)。
// 参考里是 inout 累加；WGSL 用返回值在调用方累加，避免指针参数。
fn quad_term(w: f32, lA: f32, lB: f32, lC: f32, lD: f32, lE: f32) -> vec3<f32> {
    let dc = lD - lC;
    let cb = lC - lB;
    let len_x0 = 1.0 / max(max(abs(dc), abs(cb)), 1e-8);
    let dir_x = lD - lB;
    var wxl = clamp(abs(dir_x) * len_x0, 0.0, 1.0);
    wxl *= wxl;
    let ec = lE - lC;
    let ca = lC - lA;
    let len_y0 = 1.0 / max(max(abs(ec), abs(ca)), 1e-8);
    let dir_y = lE - lA;
    var wyl = clamp(abs(dir_y) * len_y0, 0.0, 1.0);
    wyl *= wyl;
    return vec3<f32>(dir_x * w, dir_y * w, (wxl + wyl) * w);
}

// FsrEasuTapF 的标量版：返回 (luma*w, w)。
fn tap_term(
    off: vec2<f32>,
    dir: vec2<f32>,
    len2: vec2<f32>,
    lob: f32,
    clp: f32,
    luma: f32,
) -> vec2<f32> {
    var v: vec2<f32>;
    v.x = off.x * dir.x + off.y * dir.y;
    v.y = off.x * (-dir.y) + off.y * dir.x;
    v *= len2;
    var d2 = v.x * v.x + v.y * v.y;
    d2 = min(d2, clp);
    var wb = (2.0 / 5.0) * d2 - 1.0;
    var wa = lob * d2 - 1.0;
    wb *= wb;
    wa *= wa;
    wb = (25.0 / 16.0) * wb - (25.0 / 16.0 - 1.0);
    let w = wb * wa;
    return vec2<f32>(luma * w, w);
}

@fragment
fn fs_easu(in: VOut) -> @location(0) vec4<f32> {
    let in_size = vec2<f32>(easu_params.in_w, easu_params.in_h);
    let out_size = vec2<f32>(easu_params.out_w, easu_params.out_h);
    let scale = in_size / out_size;
    // pp 是输出像素中心映射回输入像素空间的位置（参考 con0.xy*ip + con0.zw 的等价式）。
    let pp = in.pos.xy * scale - 0.5;
    let fp = floor(pp);
    let f = pp - fp;
    let ip = vec2<i32>(fp);

    // 12-tap 邻域，偏移相对 F（= fp 处 texel）：b/c/e/f/g/h/i/j/k/l/n/o。
    let b_l = load_y(ip + vec2<i32>(0, -1));
    let c_l = load_y(ip + vec2<i32>(1, -1));
    let e_l = load_y(ip + vec2<i32>(-1, 0));
    let f_l = load_y(ip);
    let g_l = load_y(ip + vec2<i32>(1, 0));
    let h_l = load_y(ip + vec2<i32>(2, 0));
    let i_l = load_y(ip + vec2<i32>(-1, 1));
    let j_l = load_y(ip + vec2<i32>(0, 1));
    let k_l = load_y(ip + vec2<i32>(1, 1));
    let l_l = load_y(ip + vec2<i32>(2, 1));
    let n_l = load_y(ip + vec2<i32>(0, 2));
    let o_l = load_y(ip + vec2<i32>(1, 2));

    // 四个双线性子象限的权重（参考里的 biS/biT/biU/biV）。
    let w_s = (1.0 - f.x) * (1.0 - f.y);
    let w_t = f.x * (1.0 - f.y);
    let w_u = (1.0 - f.x) * f.y;
    let w_v = f.x * f.y;

    var dir = vec2<f32>(0.0, 0.0);
    var len = 0.0;
    let s = quad_term(w_s, b_l, e_l, f_l, g_l, j_l);
    dir += s.xy;
    len += s.z;
    let t = quad_term(w_t, c_l, f_l, g_l, h_l, k_l);
    dir += t.xy;
    len += t.z;
    let u = quad_term(w_u, f_l, i_l, j_l, k_l, n_l);
    dir += u.xy;
    len += u.z;
    let vq = quad_term(w_v, g_l, j_l, k_l, l_l, o_l);
    dir += vq.xy;
    len += vq.z;

    // 归一化方向（长度平方过小则默认水平方向，避免除零）。
    let dir_r = dot(dir, dir);
    let zro = dir_r < (1.0 / 32768.0);
    let inv = inverseSqrt(max(dir_r, 1e-16));
    dir = select(dir * inv, vec2<f32>(1.0, 0.0), zro);

    // 长度映射到 [0,1] 并平方；随后计算各向异性拉伸、负瓣强度与距离截断点。
    let half_len = len * 0.5;
    let len_sq = half_len * half_len;
    let stretch = dot(dir, dir) / max(max(abs(dir.x), abs(dir.y)), 1e-8);
    let len2 = vec2<f32>(1.0 + (stretch - 1.0) * len_sq, 1.0 - 0.5 * len_sq);
    let lob = 0.5 + ((0.25 - 0.04) - 0.5) * len_sq;
    let clp = 1.0 / max(lob, 1e-8);

    // 4 个最近 texel（f,g,j,k）的极值，用于 dering。
    let min4 = min(min(f_l, g_l), min(j_l, k_l));
    let max4 = max(max(f_l, g_l), max(j_l, k_l));

    var a_c = 0.0;
    var a_w = 0.0;
    // tap 的 off = 整数偏移 - f（等价参考里的 tap_pos - pp）。
    let t0 = tap_term(vec2<f32>(0.0, -1.0) - f, dir, len2, lob, clp, b_l);
    a_c += t0.x;
    a_w += t0.y;
    let t1 = tap_term(vec2<f32>(1.0, -1.0) - f, dir, len2, lob, clp, c_l);
    a_c += t1.x;
    a_w += t1.y;
    let t2 = tap_term(vec2<f32>(-1.0, 1.0) - f, dir, len2, lob, clp, i_l);
    a_c += t2.x;
    a_w += t2.y;
    let t3 = tap_term(vec2<f32>(0.0, 1.0) - f, dir, len2, lob, clp, j_l);
    a_c += t3.x;
    a_w += t3.y;
    let t4 = tap_term(vec2<f32>(0.0, 0.0) - f, dir, len2, lob, clp, f_l);
    a_c += t4.x;
    a_w += t4.y;
    let t5 = tap_term(vec2<f32>(-1.0, 0.0) - f, dir, len2, lob, clp, e_l);
    a_c += t5.x;
    a_w += t5.y;
    let t6 = tap_term(vec2<f32>(1.0, 1.0) - f, dir, len2, lob, clp, k_l);
    a_c += t6.x;
    a_w += t6.y;
    let t7 = tap_term(vec2<f32>(2.0, 1.0) - f, dir, len2, lob, clp, l_l);
    a_c += t7.x;
    a_w += t7.y;
    let t8 = tap_term(vec2<f32>(2.0, 0.0) - f, dir, len2, lob, clp, h_l);
    a_c += t8.x;
    a_w += t8.y;
    let t9 = tap_term(vec2<f32>(1.0, 0.0) - f, dir, len2, lob, clp, g_l);
    a_c += t9.x;
    a_w += t9.y;
    let t10 = tap_term(vec2<f32>(1.0, 2.0) - f, dir, len2, lob, clp, o_l);
    a_c += t10.x;
    a_w += t10.y;
    let t11 = tap_term(vec2<f32>(0.0, 2.0) - f, dir, len2, lob, clp, n_l);
    a_c += t11.x;
    a_w += t11.y;

    // 归一化 + dering：限到 4 近邻极值之间，杜绝过冲。
    let y = clamp(a_c / max(a_w, 1e-8), min4, max4);
    return vec4<f32>(y, y, y, 1.0);
}
";

/// Present 着色器：对 EASU 输出做 RCAS，采样色度，转 BT.709 limited → R'G'B'。
const PRESENT_SHADER: &str = r"
struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// [sharpness, 0, 0, 0]
struct PresentParams { sharp: f32, _a: f32, _b: f32, _c: f32 };

@group(0) @binding(0) var tex_easu: texture_2d<f32>;
@group(0) @binding(1) var tex_uv: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;
@group(0) @binding(3) var<uniform> params: PresentParams;

@vertex
fn vs_present(@builtin(vertex_index) index: u32) -> VOut {
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
    var out: VOut;
    out.pos = vec4<f32>(positions[index], 0.0, 1.0);
    out.uv = uvs[index];
    return out;
}

// EASU 输出已经是展开后的 [0,1]，这里直接读（不再做 limited range 变换）。
fn easu_load(p: vec2<i32>) -> f32 {
    let dims = vec2<i32>(textureDimensions(tex_easu));
    let hi = max(dims - vec2<i32>(1, 1), vec2<i32>(0, 0));
    return textureLoad(tex_easu, clamp(p, vec2<i32>(0, 0), hi), 0).r;
}

// FsrRcasF 的标量版（只处理 Y）。`con` = 强度（0..1，0 = 关闭，1 = FSR 最大锐化）。
//
// 与参考的一处精确化：参考用 `ARcpF1`（可处理负数）求 `hitMax` 的分母 `4*mn4 - 4`，该值恒 ≤ 0。
// 这里用**保号**的安全倒数：分母接近 0 时取 0，其余直接 1/x（绝不能 `max(denom, 1e-8)`——那会把
// 负分母翻成一个大正数，令 lobe 恒为 0、RCAS 完全失效）。
fn rcas(ip: vec2<i32>, con: f32) -> f32 {
    let b = easu_load(ip + vec2<i32>(0, -1));
    let d = easu_load(ip + vec2<i32>(-1, 0));
    let e = easu_load(ip);
    let f = easu_load(ip + vec2<i32>(1, 0));
    let h = easu_load(ip + vec2<i32>(0, 1));
    let mn4 = min(min(b, d), min(f, h));
    let mx4 = max(max(b, d), max(f, h));
    let hit_min = min(mn4, e) / max(4.0 * mx4, 1e-8);
    let denom_h = 4.0 * mn4 - 4.0;
    let inv_h = select(1.0 / denom_h, 0.0, abs(denom_h) < 1e-8);
    let hit_max = (1.0 - max(mx4, e)) * inv_h;
    var lobe = max(-hit_min, hit_max);
    lobe = max(-0.1875, min(lobe, 0.0)) * con;
    let rcp_l = 1.0 / max(4.0 * lobe + 1.0, 1e-8);
    return (lobe * (b + d + f + h) + e) * rcp_l;
}

fn linear_from_gamma_rgb(gamma: vec3<f32>) -> vec3<f32> {
    let cutoff = gamma < vec3<f32>(0.04045);
    let lower = gamma / vec3<f32>(12.92);
    let higher = pow((gamma + vec3<f32>(0.055)) / vec3<f32>(1.055), vec3<f32>(2.4));
    return select(higher, lower, cutoff);
}

// 把 EASU+RCAS 的 Y 与半分辨率 UV 合成 gamma 编码的 R'G'B'。
fn compose(in: VOut) -> vec3<f32> {
    let dims = vec2<f32>(textureDimensions(tex_easu));
    let ip = vec2<i32>(floor(in.uv * dims));
    // EASU 输出与 RCAS 都在展开后的 BT.709 limited [0,1] 域里，直接用。
    let yv = rcas(ip, params.sharp);
    let chroma = textureSampleLevel(tex_uv, samp, in.uv, 0.0).rg;
    let u = (chroma.x * 255.0 - 128.0) / 224.0;
    let v = (chroma.y * 255.0 - 128.0) / 224.0;
    let r = yv + 1.5748 * v;
    let g = yv - 0.1873 * u - 0.4681 * v;
    let b = yv + 1.8556 * u;
    return clamp(vec3<f32>(r, g, b), vec3<f32>(0.0), vec3<f32>(1.0));
}

@fragment
fn fs_fsr_gamma(in: VOut) -> @location(0) vec4<f32> {
    return vec4<f32>(compose(in), 1.0);
}

@fragment
fn fs_fsr_linear(in: VOut) -> @location(0) vec4<f32> {
    return vec4<f32>(linear_from_gamma_rgb(compose(in)), 1.0);
}
";

/// FSR1 的 GPU 资源：两条管线（EASU / Present）、离屏 `easu_tex` 与配套绑定组。
///
/// **每次尺寸变化重建**：`out_size` 变了重建 `easu_tex`；源/UV 纹理变了（`force_rebuild`）重建
/// 引用它们的绑定组。供线上 `Nv12Resources` 与 A/B harness 共用同一份实现。
pub(crate) struct FsrRenderer {
    easu_pipeline: wgpu::RenderPipeline,
    present_pipeline: wgpu::RenderPipeline,
    easu_layout: wgpu::BindGroupLayout,
    present_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    easu_params_buf: wgpu::Buffer,
    present_params_buf: wgpu::Buffer,
    /// 离屏 EASU 输出视图（纹理由视图持有，无需单独存 `Texture` 句柄）。
    easu_view: Option<wgpu::TextureView>,
    easu_bind_group: Option<wgpu::BindGroup>,
    present_bind_group: Option<wgpu::BindGroup>,
    out_size: Option<(u32, u32)>,
}

impl FsrRenderer {
    /// 建立管线、布局、采样器与 uniform 缓冲（离屏纹理按 `out_size` 懒建）。
    #[expect(
        clippy::too_many_lines,
        reason = "两条管线 + 两套绑定布局 + 采样器/均匀缓冲集中在一处，拆开只会让资源所有权来回传"
    )]
    pub(crate) fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        let easu_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("fsr_easu_shader"),
            source: wgpu::ShaderSource::Wgsl(EASU_SHADER.into()),
        });
        let present_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("fsr_present_shader"),
            source: wgpu::ShaderSource::Wgsl(PRESENT_SHADER.into()),
        });

        let easu_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("fsr_easu_layout"),
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
                uniform_entry(1),
            ],
        });
        let present_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("fsr_present_layout"),
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
                uniform_entry(3),
            ],
        });

        let easu_pipeline = build_pipeline(
            device,
            &easu_module,
            &easu_layout,
            wgpu::TextureFormat::R8Unorm,
            "vs_easu",
            "fs_easu",
        );
        let present_entry = if target_format.is_srgb() {
            "fs_fsr_linear"
        } else {
            "fs_fsr_gamma"
        };
        let present_pipeline = build_pipeline(
            device,
            &present_module,
            &present_layout,
            target_format,
            "vs_present",
            present_entry,
        );

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("fsr_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..wgpu::SamplerDescriptor::default()
        });

        let easu_params_buf = create_uniform(device, "fsr_easu_params");
        let present_params_buf = create_uniform(device, "fsr_present_params");

        Self {
            easu_pipeline,
            present_pipeline,
            easu_layout,
            present_layout,
            sampler,
            easu_params_buf,
            present_params_buf,
            easu_view: None,
            easu_bind_group: None,
            present_bind_group: None,
            out_size: None,
        }
    }

    /// 配置本帧：必要时重建离屏纹理与绑定组，并写入 uniform。
    ///
    /// `force_rebuild` = 源/UV 纹理刚变过（帧尺寸变化/首帧），必须重建引用它们的绑定组。
    #[expect(
        clippy::too_many_arguments,
        reason = "内部配置入口，参数都是本帧已知的标量/视图，打包成结构体反而更啰嗦"
    )]
    #[expect(
        clippy::cast_precision_loss,
        reason = "纹理尺寸远小于 2^24，u32→f32 精度足够（只用于写 uniform）"
    )]
    pub(crate) fn configure(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        source_y: &wgpu::TextureView,
        uv: &wgpu::TextureView,
        in_size: (u32, u32),
        out_size: (u32, u32),
        sharpen: f32,
        force_rebuild: bool,
    ) {
        let size_changed = self.out_size != Some(out_size);
        if size_changed || self.easu_view.is_none() {
            let tex = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("fsr_easu_tex"),
                size: wgpu::Extent3d {
                    width: out_size.0.max(1),
                    height: out_size.1.max(1),
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R8Unorm,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            // 视图持有底层纹理（Arc），故无需再单独保存 `tex` 句柄。
            self.easu_view = Some(tex.create_view(&wgpu::TextureViewDescriptor::default()));
            self.out_size = Some(out_size);
        }

        // uniform：[in_w,in_h,out_w,out_h] 与 [sharpen,0,0,0]。
        let mut easu_bytes = [0u8; 16];
        easu_bytes[0..4].copy_from_slice(&(in_size.0 as f32).to_le_bytes());
        easu_bytes[4..8].copy_from_slice(&(in_size.1 as f32).to_le_bytes());
        easu_bytes[8..12].copy_from_slice(&(out_size.0 as f32).to_le_bytes());
        easu_bytes[12..16].copy_from_slice(&(out_size.1 as f32).to_le_bytes());
        queue.write_buffer(&self.easu_params_buf, 0, &easu_bytes);

        let mut present_bytes = [0u8; 16];
        present_bytes[0..4].copy_from_slice(&sharpen.clamp(0.0, 1.0).to_le_bytes());
        queue.write_buffer(&self.present_params_buf, 0, &present_bytes);

        let rebuild = size_changed || force_rebuild;
        if rebuild || self.easu_bind_group.is_none() {
            let Some(view) = self.easu_view.as_ref() else {
                return;
            };
            self.easu_bind_group = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("fsr_easu_bind_group"),
                layout: &self.easu_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(source_y),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: self.easu_params_buf.as_entire_binding(),
                    },
                ],
            }));
            // 复用上面取到的 view 变量避免重复取 Option（present 需要 easu_view）。
            self.present_bind_group = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("fsr_present_bind_group"),
                layout: &self.present_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(uv),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: self.present_params_buf.as_entire_binding(),
                    },
                ],
            }));
        }
    }

    /// 把 EASU pass 编码进给定 encoder（清屏 + 全屏三角形）。未就绪返回 `false`。
    pub(crate) fn encode_easu(&self, encoder: &mut wgpu::CommandEncoder) -> bool {
        let (Some(view), Some(bind_group)) = (self.easu_view.as_ref(), self.easu_bind_group.as_ref())
        else {
            return false;
        };
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("fsr_easu_pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_pipeline(&self.easu_pipeline);
        pass.set_bind_group(0, bind_group, &[]);
        pass.draw(0..3, 0..1);
        true
    }

    /// Present 管线（目标格式对应的 gamma / linear 入口已在创建时选定）。
    #[must_use]
    pub(crate) fn present_pipeline(&self) -> &wgpu::RenderPipeline {
        &self.present_pipeline
    }

    /// Present 绑定组（未配置时为 `None`）。
    #[must_use]
    pub(crate) fn present_bind_group(&self) -> Option<&wgpu::BindGroup> {
        self.present_bind_group.as_ref()
    }
}

/// uniform 绑定条目（16 字节，fragment 可见）。
fn uniform_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: NonZeroU64::new(16),
        },
        count: None,
    }
}

/// 建一个 16 字节的 uniform 缓冲（内容每帧由 `write_buffer` 覆盖）。
fn create_uniform(device: &wgpu::Device, label: &str) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: 16,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// 建一条全屏三角形渲染管线（固定顶点/片元入口，REPLACE 混合）。
fn build_pipeline(
    device: &wgpu::Device,
    module: &wgpu::ShaderModule,
    layout: &wgpu::BindGroupLayout,
    format: wgpu::TextureFormat,
    vs_entry: &str,
    fs_entry: &str,
) -> wgpu::RenderPipeline {
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("fsr_pipeline_layout"),
        bind_group_layouts: &[layout],
        push_constant_ranges: &[],
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("fsr_pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module,
            entry_point: Some(vs_entry),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &[],
        },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some(fs_entry),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::REPLACE),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview: None,
        cache: None,
    })
}

#[cfg(test)]
mod tests {
    //! 只覆盖**纯逻辑**：两条 WGSL 的 naga 语义校验、`fsr_active` 的放大区间判定，以及
    //! RCAS 标量公式的规格锁（不需要 GPU）。

    use super::*;

    /// EASU 着色器必须能通过 naga 的解析与语义校验。
    #[test]
    fn easu_shader_is_valid_wgsl() {
        let module = naga::front::wgsl::parse_str(EASU_SHADER).expect("EASU 着色器应能解析");
        let mut validator = naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::empty(),
        );
        validator.validate(&module).expect("EASU 着色器应通过语义校验");
    }

    /// Present 着色器必须能通过 naga 的解析与语义校验。
    #[test]
    fn present_shader_is_valid_wgsl() {
        let module = naga::front::wgsl::parse_str(PRESENT_SHADER).expect("Present 着色器应能解析");
        let mut validator = naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::empty(),
        );
        validator
            .validate(&module)
            .expect("Present 着色器应通过语义校验");
    }

    /// 放大区间判定：1.01×–4× 之间才激活；1:1 / 缩小 / 超过 4× 都回退。
    #[test]
    fn fsr_active_only_for_upscale_within_four_x() {
        assert!(fsr_active(true, 0.42, (1920, 1080), (3840, 2160)));
        // 约 1.01× 略微越过阈值即激活。
        assert!(fsr_active(true, 0.42, (1920, 1080), (1940, 1091)));
        assert!(!fsr_active(true, 0.42, (1920, 1080), (1930, 1080)), "未到 1.01× 不激活");
        assert!(!fsr_active(true, 0.42, (1920, 1080), (1920, 1080)), "1:1 不激活");
        assert!(!fsr_active(true, 0.42, (1920, 1080), (960, 540)), "缩小不激活");
        assert!(
            !fsr_active(true, 0.42, (1000, 1000), (4100, 4100)),
            "超过 4× 回退"
        );
        assert!(!fsr_active(false, 0.42, (1920, 1080), (3840, 2160)), "关闭不激活");
        assert!(!fsr_active(true, 0.0, (1920, 1080), (3840, 2160)), "强度 0 不激活");
    }

    /// RCAS 标量版本（与 Present 着色器里的 `rcas` 逐行对应），作为规格锁。
    /// 变量名与 WGSL 一致（b/d/e/f/h），故豁免单字母命名。
    #[expect(
        clippy::many_single_char_names,
        reason = "变量名与 RCAS 着色器逐字对应，改名破坏逐行比对"
    )]
    fn rcas_scalar(b: f32, d: f32, e: f32, f: f32, h: f32, con: f32) -> f32 {
        let mn4 = b.min(d).min(f).min(h);
        let mx4 = b.max(d).max(f).max(h);
        let hit_min = mn4.min(e) / (4.0 * mx4).max(1e-8);
        let denom_h = 4.0 * mn4 - 4.0;
        let inv_h = if denom_h.abs() < 1e-8 { 0.0 } else { 1.0 / denom_h };
        let hit_max = (1.0 - mx4.max(e)) * inv_h;
        let mut lobe = (-hit_min).max(hit_max);
        lobe = (-0.1875_f32).max(lobe.min(0.0)) * con;
        let rcp_l = 1.0 / (4.0 * lobe + 1.0).max(1e-8);
        (lobe * (b + d + f + h) + e) * rcp_l
    }

    /// 常数场：RCAS 的 DC 增益为 1（整幅画面亮度不被锐化推偏）。
    #[test]
    fn rcas_is_dc_neutral_on_a_constant_field() {
        let out = rcas_scalar(0.5, 0.5, 0.5, 0.5, 0.5, 0.42);
        assert!((out - 0.5).abs() < 1e-5, "常数场应仍为 0.5，实际 {out}");
    }

    /// 硬边：十字邻域跨满 0/1 时 lobe 归零，输出等于中心值——不出 overshoot / halo。
    #[test]
    fn rcas_does_not_overshoot_a_hard_edge() {
        let out = rcas_scalar(0.0, 1.0, 0.5, 0.0, 1.0, 0.42);
        assert!((out - 0.5).abs() < 1e-6, "硬边处应保持中心值 0.5，实际 {out}");
    }

    /// 软边：中心高于十字均值 → 被推得更高（正方向锐化）。
    #[test]
    fn rcas_sharpens_a_soft_edge() {
        // b/d/f/h 均值 0.4，中心 0.55 明显偏高 → 输出应更高。
        let out = rcas_scalar(0.4, 0.4, 0.55, 0.4, 0.4, 0.42);
        assert!(out > 0.55, "软边高值应更亮（>0.55），实际 {out}");
        assert!(out <= 1.0, "不得越界");
    }

    /// con=0（强度 0）= 关闭：输出严格等于中心。
    #[test]
    fn rcas_zero_strength_is_identity() {
        let out = rcas_scalar(0.2, 0.8, 0.5, 0.3, 0.9, 0.0);
        assert!((out - 0.5).abs() < 1e-6, "con=0 应原样返回中心，实际 {out}");
    }
}
