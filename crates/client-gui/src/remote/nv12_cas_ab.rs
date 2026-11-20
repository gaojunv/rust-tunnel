//! 放大增强的 **A/B 截图 harness**（手动跑，`#[ignore]`，需要真 GPU）。
//!
//! 手动运行（Windows，GUI crate 必须用 GNU target 才能链接 libvpx 预编译库）：
//! ```text
//! $env:CARGO_TARGET_DIR = "F:\Projects\rust-tunnel\target"
//! cargo test --target x86_64-pc-windows-gnu -p rust-tunnel-client-gui --bin rust-tunnel-client-gui cas -- --ignored --nocapture
//! ```
//! 产物落到环境变量 `NV12_AB_OUT` 指定的目录，缺省 `<manifest>/../../target/nv12-cas-ab/`。
//!
//! # 它验证什么
//!
//! 用**同一份**上屏着色器与 [`FsrRenderer`]（生产代码，不复制管线）离屏渲染合成桌面：
//! - **2×** 档输出三条路径：双线性、CAS（`NV12_SHADER`）、FSR1（EASU+RCAS，走 [`FsrRenderer`]）；
//!   断言 FSR 与双线性不同、且梯度能量上升。
//! - **1× / 0.5×** 档：CAS 与 FSR 都必须与双线性**逐字节相同**（放大增强只在放大时生效）。
//!
//! 另外输出对照图（文本区最近邻 ×2）与整帧缩略图，以及一张 CAS 强度扫描 `2x_strength_sweep.png`。

// 手动 harness：像素/浮点换算里到处是 u8/f32 互转与下标循环，逐处 `#[expect]` 只会淹没代码。
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::too_many_lines,
    clippy::needless_range_loop,
    clippy::similar_names,
    clippy::many_single_char_names,
    clippy::suboptimal_flops,
    reason = "手动 A/B 截图 harness：图像换算的数值转换/单字母变量/乘加密集，且只在 `--ignored` 测试里编译执行"
)]

use std::path::PathBuf;
use std::sync::Arc;

use egui_wgpu::wgpu;
use font8x8::UnicodeFonts;
use image::{RgbaImage, imageops};

use super::nv12_fsr::{FsrRenderer, fsr_active};
use super::nv12_renderer::{
    CAS_SHARPEN_DEFAULT_PERCENT, NV12_SHADER, cas_sharpen_strength,
};

/// 合成源的尺寸（1080p 远端）。
const SRC_W: u32 = 1920;
/// 合成源的尺寸（1080p 远端）。
const SRC_H: u32 = 1080;

/// 目标尺寸档：`(标签, 宽, 高)`。
const TARGETS: &[(&str, u32, u32)] = &[
    ("2x", SRC_W * 2, SRC_H * 2),
    ("1x", SRC_W, SRC_H),
    ("0_5x", SRC_W / 2, SRC_H / 2),
];

/// 一条路径在某一档尺寸下的渲染结果。
struct Rendered {
    label: &'static str,
    w: u32,
    h: u32,
    bilinear: Vec<u8>,
    cas: Vec<u8>,
    fsr: Vec<u8>,
}

/// 一张合成的 NV12 帧：Y 平面（`width * height`）+ 交织 UV（`width * height / 2`）。
///
/// UV 的行宽是 `width` 字节（每对 U/V 覆盖 2×2 luma），与渲染侧 `write_texture` 的布局一致。
struct Nv12 {
    width: u32,
    height: u32,
    y: Vec<u8>,
    uv: Vec<u8>,
}

impl Nv12 {
    /// 白底（Y=235，UV=128/128，即 BT.709 limited 的白）。
    fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            y: vec![235; (width * height) as usize],
            uv: vec![128; (width * height / 2) as usize],
        }
    }

    /// 写一个亮度像素（越界忽略）。
    fn set_y(&mut self, x: u32, y: u32, v: u8) {
        if x < self.width && y < self.height {
            let i = (y * self.width + x) as usize;
            self.y[i] = v;
        }
    }

    /// 按 2×2 块写色度（越界忽略）。
    fn set_uv(&mut self, bx: u32, by: u32, u: u8, v: u8) {
        if bx * 2 < self.width && by * 2 < self.height {
            let i = (by * self.width + bx * 2) as usize;
            self.uv[i] = u;
            self.uv[i + 1] = v;
        }
    }

    /// 用 R'G'B'（0..1）填充一个矩形：Y 逐像素、UV 逐 2×2 块（块取矩形覆盖到的块）。
    fn fill_rect_rgb(&mut self, x: u32, y: u32, w: u32, h: u32, rgb: (f32, f32, f32)) {
        let (yv, u, v) = rgb_to_yuv(rgb);
        for py in y..(y + h).min(self.height) {
            for px in x..(x + w).min(self.width) {
                self.set_y(px, py, yv);
            }
        }
        for by in (y / 2)..u32::midpoint(y, h) {
            for bx in (x / 2)..u32::midpoint(x, w) {
                self.set_uv(bx, by, u, v);
            }
        }
    }

    /// 用 8×8 点阵画一行黑色文本（1× 字号，字符步进 8 像素）。
    fn draw_text(&mut self, x0: u32, y0: u32, text: &str) {
        let mut cx = x0;
        for ch in text.chars() {
            if let Some(glyph) = font8x8::BASIC_FONTS.get(ch) {
                for (row, bits) in glyph.iter().enumerate() {
                    for col in 0..8u32 {
                        // font8x8 的位序：bit 0 是最左侧像素。
                        if (bits >> col) & 1 != 0 {
                            self.set_y(cx + col, y0 + row as u32, 16);
                        }
                    }
                }
            }
            cx += 8;
        }
    }
}

/// BT.709 limited range 的 R'G'B'（0..1）→ `(Y8, U8, V8)`，与上屏着色器的逆变换严格互逆。
fn rgb_to_yuv((r, g, b): (f32, f32, f32)) -> (u8, u8, u8) {
    let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    let cb = (b - luma) / 1.8556;
    let cr = (r - luma) / 1.5748;
    let y8 = (16.0 + 219.0 * luma).round().clamp(0.0, 255.0) as u8;
    let u8 = (128.0 + 224.0 * cb).round().clamp(0.0, 255.0) as u8;
    let v8 = (128.0 + 224.0 * cr).round().clamp(0.0, 255.0) as u8;
    (y8, u8, v8)
}

/// 合成一张用于对照的「桌面」：白底 + 黑字 + 细线/棋盘 + 色块 + 灰阶渐变。
fn synth_desktop() -> Nv12 {
    let mut f = Nv12::new(SRC_W, SRC_H);

    // 三行 1× 文本（落在对照裁剪区 (160,80) 480×270 内）。
    f.draw_text(160, 80, "The quick brown fox jumps over the lazy dog 0123456789");
    f.draw_text(160, 108, "CAS/FSR sharpening turns bilinear-soft text crisp again.");
    f.draw_text(160, 136, "ABCDEFGHIJKLMNOPQRSTUVWXYZ abcdefghijklmnopqrstuvwxyz");

    // 1px 横线 / 竖线（检验放大后的边缘锐度与 halo）。
    for x in 100..900 {
        f.set_y(x, 500, 16);
    }
    for y in 500..900 {
        f.set_y(1000, y, 16);
    }
    // 45° 斜线。
    for i in 0..200u32 {
        f.set_y(1100 + i, 600 + i, 16);
    }

    // 1px 棋盘（400..600）：最强的高频，最能看出增强是否过冲。
    for y in 400..600u32 {
        for x in 400..600u32 {
            let v = if (x + y) % 2 == 0 { 16 } else { 235 };
            f.set_y(x, y, v);
        }
    }

    // 纯色块（检验色度子采样 + 亮度增强不会把颜色带歪）。
    f.fill_rect_rgb(1400, 100, 180, 180, (1.0, 0.0, 0.0));
    f.fill_rect_rgb(1600, 100, 180, 180, (0.0, 1.0, 0.0));
    f.fill_rect_rgb(1400, 300, 180, 180, (0.0, 0.0, 1.0));

    // 横向灰阶渐变（760..860）：检查增强不会在平滑区域制造台阶（DC 增益应为 1）。
    for x in 0..SRC_W {
        let t = x as f32 / (SRC_W - 1) as f32;
        let v = (16.0 + 219.0 * t).round() as u8;
        for y in 760..860u32 {
            f.set_y(x, y, v);
        }
    }

    f
}

/// 极简 `block_on`：wgpu 的 `request_adapter`/`request_device` 是 future，用线程 park/unpark 驱动。
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    use std::task::{Context, Poll, Wake, Waker};

    struct ThreadWaker(std::thread::Thread);
    impl Wake for ThreadWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }

    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut fut = std::pin::pin!(fut);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => std::thread::park(),
        }
    }
}

/// 建一条与 [`super::nv12_renderer`] 等价的单 pass 管线（同 shader、同绑定布局）。
fn make_pipeline(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
) -> (wgpu::RenderPipeline, wgpu::BindGroupLayout) {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("cas_ab_shader"),
        source: wgpu::ShaderSource::Wgsl(NV12_SHADER.into()),
    });
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("cas_ab_layout"),
        entries: &[
            texture_entry(0),
            texture_entry(1),
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
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
        label: Some("cas_ab_pipeline_layout"),
        bind_group_layouts: &[&layout],
        push_constant_ranges: &[],
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("cas_ab_pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &[],
        },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_gamma_target"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::REPLACE),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview: None,
        cache: None,
    });
    (pipeline, layout)
}

/// 可采样二维纹理的绑定布局条目。
fn texture_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

/// 合成源上传后的纹理视图 + 单 pass 绑定组（供 CAS/双线性路径；视图也供 FSR 复用）。
struct Source {
    view_y: wgpu::TextureView,
    view_uv: wgpu::TextureView,
    bind_group: wgpu::BindGroup,
}

/// 把合成 NV12 上传成纹理并建单 pass 绑定组（含 16 字节锐化 uniform）。
fn bind_source(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    frame: &Nv12,
    layout: &wgpu::BindGroupLayout,
    uniform: &wgpu::Buffer,
) -> Source {
    let tex_y = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("cas_ab_tex_y"),
        size: wgpu::Extent3d { width: frame.width, height: frame.height, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo { texture: &tex_y, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        &frame.y,
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(frame.width), rows_per_image: Some(frame.height) },
        wgpu::Extent3d { width: frame.width, height: frame.height, depth_or_array_layers: 1 },
    );

    let (uv_w, uv_h) = (frame.width / 2, frame.height / 2);
    let tex_uv = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("cas_ab_tex_uv"),
        size: wgpu::Extent3d { width: uv_w, height: uv_h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rg8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo { texture: &tex_uv, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        &frame.uv,
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(frame.width), rows_per_image: Some(uv_h) },
        wgpu::Extent3d { width: uv_w, height: uv_h, depth_or_array_layers: 1 },
    );

    let view_y = tex_y.create_view(&wgpu::TextureViewDescriptor::default());
    let view_uv = tex_uv.create_view(&wgpu::TextureViewDescriptor::default());
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("cas_ab_sampler"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        mipmap_filter: wgpu::FilterMode::Nearest,
        ..wgpu::SamplerDescriptor::default()
    });

    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("cas_ab_bind_group"),
        layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view_y) },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&view_uv) },
            wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&sampler) },
            wgpu::BindGroupEntry { binding: 3, resource: uniform.as_entire_binding() },
        ],
    });
    Source { view_y, view_uv, bind_group }
}

/// 建一张 `Rgba8Unorm` 离屏目标纹理。
fn create_target(device: &wgpu::Device, width: u32, height: u32) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("cas_ab_target"),
        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

/// 把目标纹理回读成紧包 RGBA8。
fn readback_rgba(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    target: &wgpu::Texture,
    width: u32,
    height: u32,
) -> Vec<u8> {
    let unpadded = width * 4;
    let padded = (unpadded + 255) & !255;
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("cas_ab_readback"),
        size: u64::from(padded) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("cas_ab_enc") });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo { texture: target, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(padded), rows_per_image: Some(height) },
        },
        wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
    );
    queue.submit([encoder.finish()]);

    let slice = readback.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device.poll(wgpu::Maintain::Wait);
    rx.recv().unwrap().unwrap();

    let mapped = slice.get_mapped_range();
    let mut out = Vec::with_capacity((unpadded * height) as usize);
    for row in 0..height {
        let start = (row * padded) as usize;
        out.extend_from_slice(&mapped[start..start + unpadded as usize]);
    }
    drop(mapped);
    out
}

/// 单 pass 渲染（双线性 / CAS），返回紧包 RGBA8。
fn render_single_pass(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipeline: &wgpu::RenderPipeline,
    bind_group: &wgpu::BindGroup,
    width: u32,
    height: u32,
) -> Vec<u8> {
    let target = create_target(device, width, height);
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("cas_ab_enc") });
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("cas_ab_pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
    queue.submit([encoder.finish()]);
    readback_rgba(device, queue, &target, width, height)
}

/// FSR 两 pass 渲染：EASU 到离屏 → RCAS + 色度 → 目标，返回紧包 RGBA8。
///
/// 与线上的一点差异：这里把 EASU 与 Present 编进**同一个 encoder**（两个 render pass），
/// 线上 EASU 在 `prepare` 的独立 CommandBuffer 里、Present 在 egui 主 pass 里；两者都靠
/// wgpu 在 pass 间自动插屏障，结果一致。
fn render_fsr(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    fsr: &mut FsrRenderer,
    source: &Source,
    in_size: (u32, u32),
    out_size: (u32, u32),
    sharpen: f32,
) -> Vec<u8> {
    let (w, h) = out_size;
    fsr.configure(device, queue, &source.view_y, &source.view_uv, in_size, out_size, sharpen, true);
    let target = create_target(device, w, h);
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("fsr_enc") });
    assert!(fsr.encode_easu(&mut encoder), "FSR 未就绪（EASU 绑定组缺失）");
    {
        let bind_group = fsr.present_bind_group().expect("present 绑定组应已配置");
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("fsr_present_pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_pipeline(fsr.present_pipeline());
        pass.set_bind_group(0, bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
    queue.submit([encoder.finish()]);
    readback_rgba(device, queue, &target, w, h)
}

/// 写进单 pass uniform 缓冲的 16 字节 `[sharpen, 0, 0, 0]`。
fn write_sharpen(queue: &wgpu::Queue, uniform: &wgpu::Buffer, sharpen: f32) {
    let mut bytes = [0u8; 16];
    bytes[..4].copy_from_slice(&sharpen.to_le_bytes());
    queue.write_buffer(uniform, 0, &bytes);
}

/// RGBA 缓冲 → `RgbaImage`。
fn to_image(rgba: &[u8], w: u32, h: u32) -> RgbaImage {
    RgbaImage::from_raw(w, h, rgba.to_vec()).unwrap()
}

/// 从一张 RGBA 缓冲里裁一块出来。
fn crop_rgba(src: &[u8], src_w: u32, x: u32, y: u32, w: u32, h: u32) -> RgbaImage {
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for row in 0..h {
        let start = (((y + row) * src_w + x) * 4) as usize;
        out.extend_from_slice(&src[start..start + (w * 4) as usize]);
    }
    RgbaImage::from_raw(w, h, out).unwrap()
}

/// 估计亮度（0..255，gamma 域的近似）在字节偏移 `i`（RGBA）处的值。
fn luma(rgba: &[u8], i: usize) -> f32 {
    0.2126 * f32::from(rgba[i]) + 0.7152 * f32::from(rgba[i + 1]) + 0.0722 * f32::from(rgba[i + 2])
}

/// 梯度能量 `Σ(|dx| + |dy|)`（亮度），越大表示边缘越锐。
fn gradient_energy(rgba: &[u8], w: u32, h: u32) -> f64 {
    let mut sum = 0.0f64;
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            let l = f64::from(luma(rgba, i));
            if x + 1 < w {
                let r = f64::from(luma(rgba, i + 4));
                sum += (r - l).abs();
            }
            if y + 1 < h {
                let d = f64::from(luma(rgba, i + (w * 4) as usize));
                sum += (d - l).abs();
            }
        }
    }
    sum
}

/// 输出目录（`NV12_AB_OUT` 或 `<manifest>/../../target/nv12-cas-ab/`），已创建。
fn out_dir() -> PathBuf {
    let dir = std::env::var_os("NV12_AB_OUT").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/nv12-cas-ab"),
        PathBuf::from,
    );
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(&dir).unwrap_or(dir)
}

/// A/B 截图 harness 本体。拿不到 GPU 时打印原因并返回，不 panic。
#[test]
#[ignore = "需要真 GPU（DX12/Vulkan）；手动 `--ignored` 跑，见模块文档"]
fn cas_ab_sharpens_only_when_upscaling() {
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
    let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: None,
    }));
    let Some(adapter) = adapter else {
        println!("[cas-ab] 没有可用的 GPU adapter（需要 DX12/Vulkan 显卡），跳过。");
        return;
    };
    let info = adapter.get_info();
    println!("[cas-ab] adapter: {} / {:?} / {:?}", info.name, info.backend, info.device_type);

    let (device, queue) = match block_on(adapter.request_device(
        &wgpu::DeviceDescriptor {
            label: Some("cas-ab"),
            required_features: wgpu::Features::empty(),
            required_limits: adapter.limits(),
            memory_hints: wgpu::MemoryHints::default(),
        },
        None,
    )) {
        Ok(pair) => pair,
        Err(e) => {
            println!("[cas-ab] request_device 失败（{e}），跳过。");
            return;
        }
    };

    let (pipeline, layout) = make_pipeline(&device, wgpu::TextureFormat::Rgba8Unorm);
    let frame = synth_desktop();
    let uniform = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("cas_ab_uniform"),
        size: 16,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let source = bind_source(&device, &queue, &frame, &layout, &uniform);
    let mut fsr = FsrRenderer::new(&device, wgpu::TextureFormat::Rgba8Unorm);

    let dir = out_dir();
    println!("[cas-ab] 输出目录: {}", dir.display());
    let sharpen = cas_sharpen_strength(CAS_SHARPEN_DEFAULT_PERCENT);

    let mut rendered = Vec::new();
    for &(label, w, h) in TARGETS {
        write_sharpen(&queue, &uniform, 0.0);
        let bilinear = render_single_pass(&device, &queue, &pipeline, &source.bind_group, w, h);
        write_sharpen(&queue, &uniform, sharpen);
        let cas = render_single_pass(&device, &queue, &pipeline, &source.bind_group, w, h);

        let upscaling = fsr_active(true, sharpen, (SRC_W, SRC_H), (w, h));
        let fsr_img = if upscaling {
            render_fsr(&device, &queue, &mut fsr, &source, (SRC_W, SRC_H), (w, h), sharpen)
        } else {
            // FSR 模式在非放大帧走单 pass（此时 CAS 因子为 0）→ 与双线性逐字节相同。
            cas.clone()
        };

        to_image(&bilinear, w, h).save(dir.join(format!("{label}_off.png"))).unwrap();
        to_image(&cas, w, h).save(dir.join(format!("{label}_on.png"))).unwrap();
        if label == "2x" {
            to_image(&fsr_img, w, h).save(dir.join("2x_fsr.png")).unwrap();
        }
        println!("[cas-ab] {label}: {w}x{h} 已写出 {label}_off.png / {label}_on.png{}", if label == "2x" { " / 2x_fsr.png" } else { "" });

        if upscaling {
            assert!(fsr_img != bilinear, "[{label}] FSR 与双线性应不同");
            let ge_b = gradient_energy(&bilinear, w, h);
            let ge_c = gradient_energy(&cas, w, h);
            let ge_f = gradient_energy(&fsr_img, w, h);
            println!(
                "[cas-ab] {label}: 梯度能量 双线性={ge_b:.0} CAS={ge_c:.0}（{:+.2}%） FSR={ge_f:.0}（{:+.2}%）",
                (ge_c / ge_b - 1.0) * 100.0,
                (ge_f / ge_b - 1.0) * 100.0
            );
            assert!(ge_f > ge_b, "[{label}] FSR 梯度能量应高于双线性");
        } else {
            assert!(cas == bilinear, "[{label}] 非放大时 CAS 应与双线性逐字节相同");
            assert!(fsr_img == bilinear, "[{label}] 非放大时 FSR 应回退到双线性");
            println!("[cas-ab] {label}: 双线性/CAS/FSR 三者逐字节相同（增强未生效，符合预期）");
        }
        rendered.push(Rendered { label, w, h, bilinear, cas, fsr: fsr_img });
    }

    write_cas_zoom(&rendered, &dir);
    write_triple_zoom(&rendered, &dir);
    write_previews(&rendered, &dir);
    write_strength_sweep(&device, &queue, &pipeline, &source.bind_group, &uniform, &dir);
    println!("[cas-ab] 完成。");
}

/// 取 2× 档文本区并最近邻放大 2×（每块 1920×1080）。
fn zoom_text(rgba: &[u8], w: u32) -> RgbaImage {
    let (cx, cy, cw, ch) = (160 * 2, 80 * 2, 480 * 2, 270 * 2);
    let crop = crop_rgba(rgba, w, cx, cy, cw, ch);
    imageops::resize(&crop, cw * 2, ch * 2, imageops::FilterType::Nearest)
}

/// 2× 文本区左右对照（左双线性 / 右 CAS），沿用旧的 `2x_ab_text_zoom.png`。
fn write_cas_zoom(rendered: &[Rendered], dir: &std::path::Path) {
    let Some(r) = rendered.iter().find(|r| r.label == "2x") else { return };
    let a = zoom_text(&r.bilinear, r.w);
    let b = zoom_text(&r.cas, r.w);
    let mut combo = RgbaImage::new(a.width() * 2, a.height());
    imageops::replace(&mut combo, &a, 0, 0);
    imageops::replace(&mut combo, &b, i64::from(a.width()), 0);
    combo.save(dir.join("2x_ab_text_zoom.png")).unwrap();
    println!("[cas-ab] 对照图: 2x_ab_text_zoom.png（左双线性 / 右 CAS）");
}

/// 2× 文本区三方纵向对照（上→下：双线性 / CAS / FSR）。
fn write_triple_zoom(rendered: &[Rendered], dir: &std::path::Path) {
    let Some(r) = rendered.iter().find(|r| r.label == "2x") else { return };
    let rows = [
        zoom_text(&r.bilinear, r.w),
        zoom_text(&r.cas, r.w),
        zoom_text(&r.fsr, r.w),
    ];
    let row_w = rows[0].width();
    let row_h = rows[0].height();
    let mut combo = RgbaImage::new(row_w, row_h * rows.len() as u32);
    for (i, img) in rows.iter().enumerate() {
        imageops::replace(&mut combo, img, 0, i64::from(row_h * i as u32));
    }
    combo.save(dir.join("2x_ab_bilinear_cas_fsr_zoom.png")).unwrap();
    println!("[cas-ab] 对照图: 2x_ab_bilinear_cas_fsr_zoom.png（上→下 双线性 / CAS / FSR）");
}

/// 2× 整帧缩略图（双线性 / CAS / FSR）。
fn write_previews(rendered: &[Rendered], dir: &std::path::Path) {
    let Some(r) = rendered.iter().find(|r| r.label == "2x") else { return };
    for (tag, buf) in [("off", &r.bilinear), ("on", &r.cas), ("fsr", &r.fsr)] {
        let full = to_image(buf, r.w, r.h);
        let thumb = imageops::resize(&full, 1280, 720, imageops::FilterType::Nearest);
        thumb.save(dir.join(format!("2x_{tag}_preview.png"))).unwrap();
    }
    println!("[cas-ab] 缩略图: 2x_off_preview.png / 2x_on_preview.png / 2x_fsr_preview.png");
}

/// 强度扫描：2× 档用 CAS 在 20% / 默认 / 70% 三档下裁同一文本区、最近邻放大 2×、纵向排列。
fn write_strength_sweep(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipeline: &wgpu::RenderPipeline,
    bind_group: &wgpu::BindGroup,
    uniform: &wgpu::Buffer,
    dir: &std::path::Path,
) {
    let (w, h) = (SRC_W * 2, SRC_H * 2);
    let mut rows = Vec::new();
    for pct in [20u8, CAS_SHARPEN_DEFAULT_PERCENT, 70] {
        write_sharpen(queue, uniform, cas_sharpen_strength(pct));
        let rgba = render_single_pass(device, queue, pipeline, bind_group, w, h);
        rows.push((pct, zoom_text(&rgba, w)));
    }
    let row_h = rows.first().map_or(0, |(_, img)| img.height());
    let cols = rows.first().map_or(0, |(_, img)| img.width());
    let mut combo = RgbaImage::new(cols, row_h * rows.len() as u32);
    for (i, (_, img)) in rows.iter().enumerate() {
        imageops::replace(&mut combo, img, 0, i64::from(row_h * i as u32));
    }
    combo.save(dir.join("2x_strength_sweep.png")).unwrap();
    let labels: Vec<String> = rows.iter().map(|(p, _)| format!("{p}%")).collect();
    println!(
        "[cas-ab] 强度扫描: 2x_strength_sweep.png（上→下 {}）",
        labels.join(" / ")
    );
}
