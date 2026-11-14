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

use std::sync::Arc;

use egui_wgpu::wgpu;
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
const NV12_SHADER: &str = r"
struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@group(0) @binding(0) var tex_y: texture_2d<f32>;
@group(0) @binding(1) var tex_uv: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;

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

// 采样 NV12 并按 BT.709 limited range 转成 gamma 编码的 R'G'B'。
fn sample_rgb(uv: vec2<f32>) -> vec3<f32> {
    // 两张纹理都是**非 sRGB** 格式，采样值就是原始字节的归一化值。
    let yv = (textureSample(tex_y, samp, uv).r * 255.0 - 16.0) / 219.0;
    let chroma = textureSample(tex_uv, samp, uv).rg;
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
    return vec4<f32>(linear_from_gamma_rgb(sample_rgb(in.uv)), 1.0);
}

// 非 sRGB 目标：片元输出 gamma 编码值（egui 的约定）。
@fragment
fn fs_gamma_target(in: VertexOutput) -> @location(0) vec4<f32> {
    return vec4<f32>(sample_rgb(in.uv), 1.0);
}
";

/// 一帧 NV12 的绘制回调（`egui_wgpu::Callback` 的数据载体）。
///
/// `seq` 用于去重：同一帧会被每帧的 egui pass 反复 `prepare`，但只有 `seq` 变化时才真正
/// 上传纹理；没有新帧时 GPU 继续采样上一帧的内容，画面不会闪。
pub struct Nv12Callback {
    /// 解码后的 NV12 平面（`Arc` 让 `FramePaint` 能跨帧复用同一份数据）。
    pub frame: Arc<DecodedNv12>,
    /// 可见画面宽度（像素，等于 Y 平面宽）。
    pub width: u32,
    /// 可见画面高度（像素，等于 Y 平面高）。
    pub height: u32,
    /// 帧序号（去重）。
    pub seq: u64,
    /// 当前 egui 渲染目标格式（决定片元入口与管线格式）。
    pub target_format: wgpu::TextureFormat,
}

/// 存在 [`egui_wgpu::CallbackResources`] 里的 GPU 资源（管线、纹理、绑定组）。
///
/// 纹理与绑定组只在**尺寸变化**时重建；尺寸不变时每帧只做 `queue.write_texture`。
struct Nv12Resources {
    /// 管线编译时使用的目标格式；与回调不一致时整组资源重建。
    target_format: wgpu::TextureFormat,
    /// 渲染管线（片元入口已按 sRGB/非 sRGB 选定）。
    pipeline: wgpu::RenderPipeline,
    /// 绑定组布局（Y、UV、采样器）。
    bind_group_layout: wgpu::BindGroupLayout,
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
    /// 已上传帧的序号（同一 `seq` 不重复上传）。
    last_seq: Option<u64>,
}

impl Nv12Resources {
    /// 创建管线、布局与采样器（纹理等尺寸已知时再建）。
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

        Self {
            target_format,
            pipeline,
            bind_group_layout,
            sampler,
            texture_y: None,
            texture_uv: None,
            bind_group: None,
            size: None,
            last_seq: None,
        }
    }

    /// 把一帧 NV12 上传到 GPU；尺寸变化时先重建纹理与绑定组。
    ///
    /// 任何前置条件不满足（奇偶/长度/零尺寸）都直接跳过——保留上一帧比画错或崩溃好。
    fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        frame: &DecodedNv12,
        width: u32,
        height: u32,
        seq: u64,
    ) {
        // 4:2:0 要求偶数尺寸；零尺寸无法建纹理。
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return;
        }
        // 同一帧只上传一次（egui 每个 pass 都会调 prepare）。
        if self.last_seq == Some(seq) && self.size == Some((width, height)) {
            return;
        }
        let (Ok(pixel_w), Ok(pixel_h)) = (usize::try_from(width), usize::try_from(height)) else {
            return;
        };
        let Some(pixels) = pixel_w.checked_mul(pixel_h) else {
            return;
        };
        if frame.y.len() != pixels || frame.uv.len() != pixels / 2 {
            return;
        }

        if self.size != Some((width, height)) {
            let (uv_w, uv_h) = (width / 2, height / 2);
            let texture_y = create_texture(device, "nv12_y", width, height, wgpu::TextureFormat::R8Unorm);
            let texture_uv =
                create_texture(device, "nv12_uv", uv_w, uv_h, wgpu::TextureFormat::Rg8Unorm);
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
                ],
            });
            self.texture_y = Some(texture_y);
            self.texture_uv = Some(texture_uv);
            self.bind_group = Some(bind_group);
            self.size = Some((width, height));
        }

        let (Some(texture_y), Some(texture_uv)) = (self.texture_y.as_ref(), self.texture_uv.as_ref())
        else {
            return;
        };

        // Y：每行 `width` 字节，共 `height` 行。
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: texture_y,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &frame.y,
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

        // UV：每像素 2 字节，每行 `uv_w` 像素 → `uv_w * 2 == width` 字节。
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: texture_uv,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &frame.uv,
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

        self.last_seq = Some(seq);
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
            resources.upload(device, queue, &self.frame, self.width, self.height, self.seq);
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
