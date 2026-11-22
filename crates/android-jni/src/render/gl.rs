//! GLES 上屏管线：RGBA（VP9 软解）与 NV12（H.264/H.265 硬解）两条路径。
//!
//! 用 [`glow`] 的薄封装而不是手写 `extern "C"` 声明：函数表来自 EGL 的
//! `eglGetProcAddress`（见 [`super::egl::Egl::proc_address`]），本模块只关心着色器与绘制。
//!
//! # 两条路径
//!
//! - **RGBA**：一张 `GL_RGBA8` 纹理，片元着色器原样输出（VP9 4:4:4 路径，逐字节不变）；
//! - **NV12**：两张平面纹理（Y `GL_R8`、交织 UV `GL_RG8`），片元着色器按 **BT.709 limited**
//!   定点换算成 RGB。H.264/H.265 固定 4:2:0，解码器直接产出紧包 NV12，不需要 CPU 转换。
//!
//! NV12 的**持久合成缓冲**是本模块的状态：整帧替换两张纹理的全部内容；脏矩形补丁（协议 v6
//! `RegionUpdate`）用 `glTexSubImage2D` 只覆写矩形区域，叠在上一帧之上。几何全部由
//! [`crate::nv12_geom`] 校验并算出（纯逻辑、主机可单测），这里只负责喂给驱动。
//!
//! 全部对象（程序/顶点缓冲/纹理）归本结构所有，[`Gl::destroy`] 必须在 GL 上下文仍当前时
//! 调用（渲染线程的收尾顺序见 [`super::Renderer`]）。

use std::ffi::c_void;

use glow::HasContext;
use rust_tunnel_remote::screen::DecodedPatch;
use rust_tunnel_remote::DecodedFrame;

use crate::geometry::Rect;
use crate::nv12_geom::{nv12_frame_dims, patch_upload};

/// 顶点着色器（GLES2 / `#version 100`）：直接输出 NDC 坐标与纹理坐标。
///
/// `#version 100` 在 GLES3 上下文里同样被接受（ES 3.0 要求兼容 GLSL ES 1.00），
/// 故两条路径共用同一份顶点着色器与顶点缓冲。
const VERTEX_SHADER: &str = r"#version 100
attribute vec2 a_pos;
attribute vec2 a_uv;
varying vec2 v_uv;
void main() {
    gl_Position = vec4(a_pos, 0.0, 1.0);
    v_uv = a_uv;
}
";

/// RGBA 片元着色器：把纹理原样贴到屏幕（不透明，alpha 不参与混合）。
const RGBA_FRAGMENT_SHADER: &str = r"#version 100
precision mediump float;
varying vec2 v_uv;
uniform sampler2D u_tex;
void main() {
    gl_FragColor = texture2D(u_tex, v_uv);
}
";

/// NV12 片元着色器：Y 取 `.r`、交织 UV 取 `.rg`，按 **BT.709 limited** 转 RGB。
///
/// 系数与 `crates/client-gui/src/remote/nv12_renderer.rs`（以及 `win-video` 的
/// NV12→NV12 转换）同源：`y ∈ [16,235]`、`uv ∈ [16,240]`（8 位偏移 128 表达）。
/// 采样器用 `texture2D`（GLSL ES 1.00）：Y 纹理是 `GL_R8`、UV 是 `GL_RG8`，都要求 GLES3
/// 上下文（GLES2 的 `gl.rs` 直接拒绝 NV12 路径）。
const NV12_FRAGMENT_SHADER: &str = r"#version 100
precision mediump float;
varying vec2 v_uv;
uniform sampler2D u_tex_y;
uniform sampler2D u_tex_uv;
void main() {
    float yv = (texture2D(u_tex_y, v_uv).r * 255.0 - 16.0) / 219.0;
    vec2 chroma = texture2D(u_tex_uv, v_uv).rg;
    float u = (chroma.x * 255.0 - 128.0) / 224.0;
    float v = (chroma.y * 255.0 - 128.0) / 224.0;
    float r = yv + 1.5748 * v;
    float g = yv - 0.1873 * u - 0.4681 * v;
    float b = yv + 1.8556 * u;
    gl_FragColor = vec4(clamp(vec3(r, g, b), 0.0, 1.0), 1.0);
}
";

/// 每个顶点的 float 数（x, y, u, v）。
const FLOATS_PER_VERTEX: i32 = 4;
/// 一个顶点的字节数。
const VERTEX_STRIDE: i32 = FLOATS_PER_VERTEX * FLOAT_BYTES;
/// 单个 `f32` 的字节数。
const FLOAT_BYTES: i32 = 4;

/// 全屏四边形（TRIANGLE_STRIP）。纹理坐标上下翻转：CPU 侧 RGBA/NV12 都是**自顶向下**，
/// 而 `glTexImage2D` 把第一行当作纹理底部（v=0），故屏幕上边（NDC y=+1）取 v=0。
const QUAD: [f32; 16] = [
    // x,   y,   u, v
    -1.0, -1.0, 0.0, 1.0, // 左下
    1.0, -1.0, 1.0, 1.0, // 右下
    -1.0, 1.0, 0.0, 0.0, // 左上
    1.0, 1.0, 1.0, 0.0, // 右上
];

/// 当前绑定的 RGBA 纹理及其尺寸。
struct Texture {
    /// GL 纹理名。
    handle: glow::Texture,
    /// 宽（像素）。
    width: i32,
    /// 高（像素）。
    height: i32,
}

/// 持久 NV12 合成缓冲：Y 与交织 UV 两张平面纹理。
struct Nv12Textures {
    /// Y 平面纹理（`GL_R8`）。
    y: glow::Texture,
    /// 交织 UV 平面纹理（`GL_RG8`，尺寸为 luma 的一半）。
    uv: glow::Texture,
    /// 整帧宽度（luma 像素）。
    width: u32,
    /// 整帧高度（luma 像素）。
    height: u32,
    /// 是否已有可作补丁基底的整帧（重建纹理时清零）。
    has_base: bool,
}

/// RGBA 管线（程序 + 属性/采样器位置）。
struct RgbaPipe {
    /// 着色器程序。
    program: glow::Program,
    /// `a_pos` 属性位置。
    pos: u32,
    /// `a_uv` 属性位置。
    uv: u32,
    /// `u_tex` 采样器位置。
    u_tex: Option<glow::UniformLocation>,
}

/// NV12 管线（程序 + 两个采样器位置）。
struct Nv12Pipe {
    /// 着色器程序。
    program: glow::Program,
    /// `a_pos` 属性位置。
    pos: u32,
    /// `a_uv` 属性位置。
    uv: u32,
    /// Y 平面采样器位置。
    u_tex_y: Option<glow::UniformLocation>,
    /// UV 平面采样器位置。
    u_tex_uv: Option<glow::UniformLocation>,
}

/// 一批补丁的处理结果（由渲染线程决定要不要重绘/记日志）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PatchOutcome {
    /// 补丁已叠进持久缓冲，可以重绘。
    Applied,
    /// 空补丁（保活）：缓冲未变，不需要重绘。
    KeepAlive,
    /// 补丁被丢弃及原因（渲染线程记 warning；引擎侧会因尺寸不符请求关键帧）。
    Dropped(&'static str),
}

/// GLES 上屏管线。
pub(crate) struct Gl {
    /// glow 上下文（函数表）。
    context: glow::Context,
    /// 顶点缓冲（绑定后不再切换，属性指针在每次绘制前按当前程序重设）。
    vbo: glow::Buffer,
    /// RGBA 管线。
    rgba: RgbaPipe,
    /// NV12 管线（着色器链接失败时为 `None`，此时只影响硬解路径）。
    nv12: Option<Nv12Pipe>,
    /// 当前 RGBA 纹理（尺寸变化时重建）。
    texture: Option<Texture>,
    /// 持久 NV12 合成缓冲（首次收到 NV12 整帧时创建）。
    nv12_textures: Option<Nv12Textures>,
    /// GLES3 的顶点数组对象（ES3 下显式绑定比依赖默认 VAO 更稳妥；ES2 下为 `None`）。
    vao: Option<glow::VertexArray>,
    /// 上下文是否为 GLES3（NV12 平面纹理要求 `GL_R8`/`GL_RG8`）。
    gles3: bool,
    /// 是否已经记过「GLES2 不支持 NV12」的日志（避免每帧刷屏）。
    nv12_unsupported_logged: bool,
}

impl Gl {
    /// 在**已 make current** 的 EGL 上下文上建立管线。
    ///
    /// `loader` 把 GLES 函数名解析成地址（EGL 的 `getProcAddress` + `libGLESv2` 兜底）；
    /// `gles3` 取自 [`super::egl::Egl::is_gles3`]，决定 NV12 平面纹理是否可用。
    ///
    /// # Errors
    ///
    /// RGBA 着色器编译/链接失败、或 GL 对象创建失败时返回可读原因（此时整个渲染器不可用）；
    /// NV12 着色器失败只记 warning 并禁用硬解路径。
    pub(crate) fn new(
        mut loader: impl FnMut(&str) -> *const c_void,
        gles3: bool,
    ) -> Result<Self, String> {
        // SAFETY: 调用方保证当前线程已有 current 的 GLES 上下文，loader 解析的地址来自它。
        let gl = unsafe { glow::Context::from_loader_function(&mut loader) };

        // SAFETY: 以下 GL 调用都要求上下文 current（已由调用方保证），对象句柄由 glow 管理。
        unsafe {
            let rgba = build_rgba_pipe(&gl)?;
            let nv12 = match build_nv12_pipe(&gl) {
                Ok(pipe) => Some(pipe),
                Err(err) => {
                    tracing::warn!("NV12 着色器不可用（H.264/H.265 上屏将失败）：{err}");
                    None
                }
            };
            let vbo = gl
                .create_buffer()
                .map_err(|err| format!("glGenBuffers 失败：{err}"))?;
            // GLES3：显式持有一个 VAO（ES2 没有 VAO，保持 `None` 走默认状态）。
            // 本工程不使用客户端顶点数组，所有绘制都走 VBO，故 VAO 里只记录属性指针。
            let vao = if gles3 {
                match gl.create_vertex_array() {
                    Ok(vao) => {
                        gl.bind_vertex_array(Some(vao));
                        Some(vao)
                    }
                    Err(err) => {
                        tracing::warn!("创建 VAO 失败（沿用默认顶点数组）：{err}");
                        None
                    }
                }
            } else {
                None
            };
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            // 顶点数据以字节切片上传（避免引入 bytemuck 依赖）。
            let vertices = std::slice::from_raw_parts(
                QUAD.as_ptr().cast::<u8>(),
                std::mem::size_of_val(&QUAD),
            );
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, vertices, glow::STATIC_DRAW);

            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
            // 补丁的 UV 上传靠「行宽 = 上传宽度 × 2 字节」直接取源数据（见
            // `nv12_geom::patch_upload` 的说明）。显式把 `UNPACK_ROW_LENGTH` 置 0 才是
            // 「按 width 算行宽」；该枚举是 GLES3 才有的，ES2 上下文设置它会报 INVALID_ENUM。
            if gles3 {
                gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
            }
            gl.disable(glow::DEPTH_TEST);
            gl.disable(glow::BLEND);
            gl.disable(glow::SCISSOR_TEST);

            if gles3 {
                tracing::info!("GLES3 上屏管线已就绪（支持 NV12 平面纹理）");
            } else {
                tracing::warn!("GLES2 上屏管线（仅 RGBA/VP9；NV12 硬解上屏不可用）");
            }

            Ok(Self {
                context: gl,
                vbo,
                rgba,
                nv12,
                texture: None,
                nv12_textures: None,
                vao,
                gles3,
                nv12_unsupported_logged: false,
            })
        }
    }

    /// 上传一帧 RGBA 到纹理（尺寸变化时重建纹理对象）。
    ///
    /// 返回 `false` 表示这一帧不可用（RGBA 为空——例如 NV12 硬件路径——或长度不符），
    /// 调用方应跳过绘制。
    pub(crate) fn upload(&mut self, frame: &DecodedFrame) -> bool {
        let Some((width, height)) = frame_dims(frame) else {
            return false;
        };
        if !self.ensure_texture(width, height) {
            return false;
        }
        let Some(texture) = self.texture.as_ref() else {
            return false;
        };

        // SAFETY: 上下文 current；尺寸与长度已由 `frame_dims`/`ensure_texture` 校验。
        unsafe {
            self.context
                .bind_texture(glow::TEXTURE_2D, Some(texture.handle));
            self.context.tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                0,
                0,
                width,
                height,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(&frame.rgba)),
            );
        }
        true
    }

    /// 上传一帧紧包 NV12（H.264/H.265 硬解输出），替换持久合成缓冲的整帧内容。
    ///
    /// 返回 `false` 表示这一帧不可用（GLES2 上下文、着色器缺失、尺寸/长度非法），调用方跳过绘制。
    pub(crate) fn upload_nv12(&mut self, frame: &DecodedFrame) -> bool {
        let Some(nv12) = frame.nv12.as_ref() else {
            tracing::debug!("upload_nv12 收到没有 nv12 字段的帧");
            return false;
        };
        let Some((width, height)) =
            nv12_frame_dims(frame.width, frame.height, nv12.y.len(), nv12.uv.len())
        else {
            tracing::warn!(
                "NV12 帧尺寸/长度非法：{}x{}（y={} uv={}）",
                frame.width,
                frame.height,
                nv12.y.len(),
                nv12.uv.len()
            );
            return false;
        };
        if !self.ensure_nv12_textures(width, height) {
            return false;
        }
        let (Ok(width_i), Ok(height_i)) = (i32::try_from(width), i32::try_from(height)) else {
            tracing::warn!("NV12 帧尺寸超出 GLES 可表达范围：{width}x{height}");
            return false;
        };
        let Some(textures) = self.nv12_textures.as_ref() else {
            return false;
        };

        // SAFETY: 上下文 current；尺寸/长度已由 `nv12_frame_dims`/`ensure_nv12_textures` 校验。
        unsafe {
            self.context
                .bind_texture(glow::TEXTURE_2D, Some(textures.y));
            self.context.tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                0,
                0,
                width_i,
                height_i,
                glow::RED,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(&nv12.y)),
            );
            self.context
                .bind_texture(glow::TEXTURE_2D, Some(textures.uv));
            self.context.tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                0,
                0,
                width_i / 2,
                height_i / 2,
                glow::RG,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(&nv12.uv)),
            );
        }
        if let Some(textures) = self.nv12_textures.as_mut() {
            textures.has_base = true;
        }
        true
    }

    /// 把一批脏矩形补丁叠进持久 NV12 缓冲（**必须**已有一整帧基底且尺寸一致）。
    ///
    /// 先逐块校验几何、全部合法才写（与引擎侧 `DecodedNv12::apply_patches` 的
    /// 「先全验、再全写」一致），避免半截应用留下永久花屏。
    pub(crate) fn apply_nv12_patches(&mut self, patch: &DecodedPatch) -> PatchOutcome {
        if patch.rects.is_empty() {
            return PatchOutcome::KeepAlive;
        }
        let Some(textures) = self.nv12_textures.as_ref() else {
            return PatchOutcome::Dropped("尚无 NV12 基底帧");
        };
        if !textures.has_base {
            return PatchOutcome::Dropped("尚无 NV12 基底帧");
        }
        if textures.width != patch.width || textures.height != patch.height {
            return PatchOutcome::Dropped("补丁尺寸与当前缓冲不符");
        }
        let Some(_pipe) = self.nv12.as_ref() else {
            return PatchOutcome::Dropped("NV12 着色器不可用");
        };

        // 先全验：任一矩形非法就整批丢弃（不写一个字节）。
        let mut plans = Vec::with_capacity(patch.rects.len());
        for rect in &patch.rects {
            let Some(upload) = patch_upload(patch.width, patch.height, rect) else {
                return PatchOutcome::Dropped("补丁几何非法");
            };
            plans.push((upload, rect));
        }

        // SAFETY: 上下文 current；几何已全部校验，切片范围由 `patch_upload` 的偏移/长度给出。
        unsafe {
            self.context
                .bind_texture(glow::TEXTURE_2D, Some(textures.y));
            for (upload, rect) in &plans {
                let Some(src) = rect
                    .nv12
                    .get(upload.y.offset..upload.y.offset + upload.y.len)
                else {
                    return PatchOutcome::Dropped("补丁数据长度不符");
                };
                self.context.tex_sub_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    upload.y.x,
                    upload.y.y,
                    upload.y.width,
                    upload.y.height,
                    glow::RED,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(Some(src)),
                );
            }
            self.context
                .bind_texture(glow::TEXTURE_2D, Some(textures.uv));
            for (upload, rect) in &plans {
                let Some(src) = rect
                    .nv12
                    .get(upload.uv.offset..upload.uv.offset + upload.uv.len)
                else {
                    return PatchOutcome::Dropped("补丁数据长度不符");
                };
                // 源行宽 = 上传宽度（`rect.width/2` 对）× 2 字节 = `rect.width`；
                // `UNPACK_ROW_LENGTH` 保持默认 0 时 GL 正是这么算的，无需设置。
                self.context.tex_sub_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    upload.uv.x,
                    upload.uv.y,
                    upload.uv.width,
                    upload.uv.height,
                    glow::RG,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(Some(src)),
                );
            }
        }
        PatchOutcome::Applied
    }

    /// 当前持久 NV12 缓冲的整帧尺寸（仅在**已有基底帧**时返回）。
    pub(crate) fn nv12_dimensions(&self) -> Option<(u32, u32)> {
        let textures = self.nv12_textures.as_ref()?;
        textures
            .has_base
            .then_some((textures.width, textures.height))
    }

    /// 让持久 NV12 缓冲失效（收到 RGBA 整帧 = 会话换了 codec）。
    ///
    /// 只清「有基底」标志、不销毁纹理：同一尺寸的 NV12 会话再回来时可以复用，省掉一次重建。
    pub(crate) fn invalidate_nv12(&mut self) {
        if let Some(textures) = self.nv12_textures.as_mut() {
            textures.has_base = false;
        }
    }

    /// 保证存在一张尺寸匹配的 RGBA 纹理（尺寸变化时重建）。
    ///
    /// 返回 `false` 表示纹理不可用（创建失败）。
    fn ensure_texture(&mut self, width: i32, height: i32) -> bool {
        let needs_new = self
            .texture
            .as_ref()
            .is_none_or(|tex| tex.width != width || tex.height != height);
        if !needs_new {
            return true;
        }

        // SAFETY: 上下文 current（渲染线程在 make current 之后才调用本函数）。
        let handle = unsafe {
            if let Some(old) = self.texture.take() {
                self.context.delete_texture(old.handle);
            }
            let Ok(handle) = self.context.create_texture() else {
                tracing::error!("glGenTextures 失败");
                return false;
            };
            self.context.bind_texture(glow::TEXTURE_2D, Some(handle));
            for (parameter, value) in [
                (glow::TEXTURE_MIN_FILTER, glow::LINEAR),
                (glow::TEXTURE_MAG_FILTER, glow::LINEAR),
                (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
                (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
            ] {
                self.context
                    .tex_parameter_i32(glow::TEXTURE_2D, parameter, enum_i32(value));
            }
            self.context.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                enum_i32(glow::RGBA8),
                width,
                height,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(None),
            );
            handle
        };
        self.texture = Some(Texture {
            handle,
            width,
            height,
        });
        true
    }

    /// 保证存在尺寸匹配的 NV12 平面纹理（尺寸变化时重建并**重置基底标志**）。
    ///
    /// 返回 `false` 表示不可用：GLES2 上下文、NV12 着色器缺失、尺寸非法或创建失败。
    fn ensure_nv12_textures(&mut self, width: u32, height: u32) -> bool {
        if !self.gles3 {
            if !self.nv12_unsupported_logged {
                tracing::warn!("当前是 GLES2 上下文，无法创建 GL_R8/GL_RG8 平面纹理");
                self.nv12_unsupported_logged = true;
            }
            return false;
        }
        if self.nv12.is_none() {
            if !self.nv12_unsupported_logged {
                tracing::warn!("NV12 着色器不可用，跳过硬解帧");
                self.nv12_unsupported_logged = true;
            }
            return false;
        }
        let needs_new = self
            .nv12_textures
            .as_ref()
            .is_none_or(|tex| tex.width != width || tex.height != height);
        if !needs_new {
            return true;
        }
        let (Ok(width_i), Ok(height_i)) = (i32::try_from(width), i32::try_from(height)) else {
            return false;
        };
        if width_i <= 0 || height_i <= 0 {
            return false;
        }

        // SAFETY: 上下文 current；尺寸已校验（非零、偶数由 `nv12_frame_dims` 保证）。
        let created = unsafe {
            if let Some(old) = self.nv12_textures.take() {
                self.context.delete_texture(old.y);
                self.context.delete_texture(old.uv);
            }
            let y = match self.context.create_texture() {
                Ok(handle) => handle,
                Err(err) => {
                    tracing::error!("创建 NV12 Y 纹理失败：{err}");
                    return false;
                }
            };
            let uv = match self.context.create_texture() {
                Ok(handle) => handle,
                Err(err) => {
                    tracing::error!("创建 NV12 UV 纹理失败：{err}");
                    self.context.delete_texture(y);
                    return false;
                }
            };
            for (handle, internal, format, tex_width, tex_height) in [
                (y, glow::R8, glow::RED, width_i, height_i),
                (uv, glow::RG8, glow::RG, width_i / 2, height_i / 2),
            ] {
                self.context.bind_texture(glow::TEXTURE_2D, Some(handle));
                for (parameter, value) in [
                    (glow::TEXTURE_MIN_FILTER, glow::LINEAR),
                    (glow::TEXTURE_MAG_FILTER, glow::LINEAR),
                    (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
                    (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
                ] {
                    self.context
                        .tex_parameter_i32(glow::TEXTURE_2D, parameter, enum_i32(value));
                }
                self.context.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    enum_i32(internal),
                    tex_width,
                    tex_height,
                    0,
                    format,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(None),
                );
            }
            Nv12Textures {
                y,
                uv,
                width,
                height,
                // 新缓冲没有基底帧：补丁在整帧到来之前一律丢弃。
                has_base: false,
            }
        };
        self.nv12_textures = Some(created);
        tracing::info!("NV12 合成缓冲已重建：{width}x{height}");
        true
    }

    /// 把当前 RGBA 纹理按 `rect`（已含居中与黑边）绘制并清出其余区域。
    ///
    /// # Errors
    ///
    /// 还没有可用的 RGBA 纹理时返回原因（正常情况下 [`Gl::upload`] 成功后才调用）。
    pub(crate) fn draw(&self, surface_size: (i32, i32), rect: Rect) -> Result<(), String> {
        let Some(texture) = self.texture.as_ref() else {
            return Err("没有可绘制的纹理".to_owned());
        };
        // SAFETY: 上下文 current；对象句柄由本结构持有。
        unsafe {
            self.begin_frame(surface_size);
            self.context
                .viewport(rect.x, rect.y, rect.width, rect.height);
            self.context.use_program(Some(self.rgba.program));
            self.bind_vertex_layout(self.rgba.pos, self.rgba.uv);
            self.context.active_texture(glow::TEXTURE0);
            self.context
                .bind_texture(glow::TEXTURE_2D, Some(texture.handle));
            self.context.uniform_1_i32(self.rgba.u_tex.as_ref(), 0);
            self.context.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        }
        Ok(())
    }

    /// 把持久 NV12 合成缓冲按 `rect` 绘制（BT.709 limited → RGB）。
    ///
    /// # Errors
    ///
    /// 着色器缺失或还没有基底帧时返回原因。
    pub(crate) fn draw_nv12(&self, surface_size: (i32, i32), rect: Rect) -> Result<(), String> {
        let Some(pipe) = self.nv12.as_ref() else {
            return Err("NV12 着色器不可用".to_owned());
        };
        let Some(textures) = self.nv12_textures.as_ref() else {
            return Err("没有 NV12 纹理".to_owned());
        };
        if !textures.has_base {
            return Err("NV12 缓冲尚无基底帧".to_owned());
        }
        // SAFETY: 上下文 current；对象句柄由本结构持有。
        unsafe {
            self.begin_frame(surface_size);
            self.context
                .viewport(rect.x, rect.y, rect.width, rect.height);
            self.context.use_program(Some(pipe.program));
            self.bind_vertex_layout(pipe.pos, pipe.uv);
            self.context.active_texture(glow::TEXTURE0);
            self.context
                .bind_texture(glow::TEXTURE_2D, Some(textures.y));
            self.context.uniform_1_i32(pipe.u_tex_y.as_ref(), 0);
            self.context.active_texture(glow::TEXTURE1);
            self.context
                .bind_texture(glow::TEXTURE_2D, Some(textures.uv));
            self.context.uniform_1_i32(pipe.u_tex_uv.as_ref(), 1);
            self.context.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            // 复位纹理单元：RGBA 路径的 `u_tex` 也从 TEXTURE0 取。
            self.context.active_texture(glow::TEXTURE0);
        }
        Ok(())
    }

    /// 清屏（整屏黑边）并设回整屏视口。
    ///
    /// # Safety
    ///
    /// 上下文必须 current。
    unsafe fn begin_frame(&self, surface_size: (i32, i32)) {
        // SAFETY: 由调用方保证上下文 current。
        unsafe {
            self.context.viewport(0, 0, surface_size.0, surface_size.1);
            self.context.clear_color(0.0, 0.0, 0.0, 1.0);
            self.context.clear(glow::COLOR_BUFFER_BIT);
        }
    }

    /// 绑定顶点缓冲并按当前程序的属性位置重设指针。
    ///
    /// 每个程序用自己的 `a_pos`/`a_uv` 位置（GL 不保证不同程序拿到同一套索引），
    /// 故每次绘制前都要重设一次。
    ///
    /// # Safety
    ///
    /// 上下文必须 current。
    unsafe fn bind_vertex_layout(&self, pos: u32, uv: u32) {
        // SAFETY: 由调用方保证上下文 current；`vbo` 在 `new` 时已上传顶点数据。
        unsafe {
            // GLES3 下属性指针状态属于 VAO；显式绑定一次，避免依赖「创建后再没换过」。
            if let Some(vao) = self.vao {
                self.context.bind_vertex_array(Some(vao));
            }
            self.context.bind_buffer(glow::ARRAY_BUFFER, Some(self.vbo));
            self.context.enable_vertex_attrib_array(pos);
            self.context
                .vertex_attrib_pointer_f32(pos, 2, glow::FLOAT, false, VERTEX_STRIDE, 0);
            self.context.enable_vertex_attrib_array(uv);
            self.context.vertex_attrib_pointer_f32(
                uv,
                2,
                glow::FLOAT,
                false,
                VERTEX_STRIDE,
                2 * FLOAT_BYTES,
            );
        }
    }

    /// 删除全部 GL 对象（必须在上下文仍 current 时调用）。
    pub(crate) fn destroy(&mut self) {
        // SAFETY: 调用方保证上下文 current；句柄只删除一次（纹理先 take 走）。
        unsafe {
            if let Some(texture) = self.texture.take() {
                self.context.delete_texture(texture.handle);
            }
            if let Some(textures) = self.nv12_textures.take() {
                self.context.delete_texture(textures.y);
                self.context.delete_texture(textures.uv);
            }
            self.context.delete_buffer(self.vbo);
            if let Some(vao) = self.vao.take() {
                self.context.delete_vertex_array(vao);
            }
            self.context.delete_program(self.rgba.program);
            if let Some(pipe) = self.nv12.take() {
                self.context.delete_program(pipe.program);
            }
        }
    }
}

/// GL 枚举常量是 `u32`，而 glow 的部分入口按 `i32` 接收（internal format / 过滤模式）。
///
/// 这些常量都远小于 `i32::MAX`，转换不可能失败；失败时退化为 0（GL 会报 `INVALID_ENUM`
/// 而不是让进程崩溃）。
fn enum_i32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(0)
}

/// 校验一帧可上传为 RGBA：尺寸在 GLES 可表达范围内，且 RGBA 长度正好是 `宽 × 高 × 4`。
///
/// NV12 硬件路径的 `rgba` 为空，这里返回 `None`（调用方走 NV12 分支或跳过绘制）。
fn frame_dims(frame: &DecodedFrame) -> Option<(i32, i32)> {
    let (Ok(width), Ok(height)) = (i32::try_from(frame.width), i32::try_from(frame.height)) else {
        tracing::warn!(
            "画面尺寸超出 GLES 可表达范围：{}x{}",
            frame.width,
            frame.height
        );
        return None;
    };
    if width <= 0 || height <= 0 {
        return None;
    }
    let expected = usize::try_from(frame.width)
        .ok()
        .and_then(|w| {
            usize::try_from(frame.height)
                .ok()
                .and_then(|h| w.checked_mul(h))
        })
        .and_then(|pixels| pixels.checked_mul(4))?;
    if frame.rgba.len() != expected {
        // NV12 硬件路径或数据损坏：记一次日志并跳过。
        tracing::debug!(
            "跳帧：RGBA 长度 {} != {}x{}x4（可能是 NV12 硬件路径）",
            frame.rgba.len(),
            frame.width,
            frame.height
        );
        return None;
    }
    Some((width, height))
}

/// 编译并链接着色器程序。
///
/// # Safety
///
/// 调用方必须保证当前线程有 current 的 GLES 上下文。
unsafe fn link_program(
    gl: &glow::Context,
    vertex_source: &str,
    fragment_source: &str,
) -> Result<glow::Program, String> {
    // SAFETY: 由调用方保证上下文 current。
    unsafe {
        let vertex = compile_shader(gl, glow::VERTEX_SHADER, vertex_source)?;
        let fragment = match compile_shader(gl, glow::FRAGMENT_SHADER, fragment_source) {
            Ok(shader) => shader,
            Err(err) => {
                gl.delete_shader(vertex);
                return Err(err);
            }
        };

        let program = gl
            .create_program()
            .map_err(|err| format!("glCreateProgram 失败：{err}"))?;
        gl.attach_shader(program, vertex);
        gl.attach_shader(program, fragment);
        gl.link_program(program);
        let linked = gl.get_program_link_status(program);
        let log = if linked {
            String::new()
        } else {
            gl.get_program_info_log(program)
        };

        gl.detach_shader(program, vertex);
        gl.detach_shader(program, fragment);
        gl.delete_shader(vertex);
        gl.delete_shader(fragment);

        if linked {
            Ok(program)
        } else {
            gl.delete_program(program);
            Err(format!("着色器链接失败：{log}"))
        }
    }
}

/// 查询一个属性位置（缺失时给出可读错误）。
///
/// # Safety
///
/// 调用方必须保证上下文 current，且 `program` 已成功链接。
unsafe fn attrib_location(
    gl: &glow::Context,
    program: glow::Program,
    name: &str,
) -> Result<u32, String> {
    // SAFETY: 由调用方保证上下文 current。
    unsafe {
        gl.get_attrib_location(program, name)
            .ok_or_else(|| format!("顶点着色器缺少 {name} 属性"))
    }
}

/// 建立 RGBA 管线。
///
/// # Safety
///
/// 调用方必须保证上下文 current。
unsafe fn build_rgba_pipe(gl: &glow::Context) -> Result<RgbaPipe, String> {
    // SAFETY: 由调用方保证上下文 current。
    unsafe {
        let program = link_program(gl, VERTEX_SHADER, RGBA_FRAGMENT_SHADER)?;
        let pos = match attrib_location(gl, program, "a_pos") {
            Ok(location) => location,
            Err(err) => {
                gl.delete_program(program);
                return Err(err);
            }
        };
        let uv = match attrib_location(gl, program, "a_uv") {
            Ok(location) => location,
            Err(err) => {
                gl.delete_program(program);
                return Err(err);
            }
        };
        Ok(RgbaPipe {
            program,
            pos,
            uv,
            u_tex: gl.get_uniform_location(program, "u_tex"),
        })
    }
}

/// 建立 NV12 管线（失败只影响硬解路径）。
///
/// # Safety
///
/// 调用方必须保证上下文 current。
unsafe fn build_nv12_pipe(gl: &glow::Context) -> Result<Nv12Pipe, String> {
    // SAFETY: 由调用方保证上下文 current。
    unsafe {
        let program = link_program(gl, VERTEX_SHADER, NV12_FRAGMENT_SHADER)?;
        let pos = match attrib_location(gl, program, "a_pos") {
            Ok(location) => location,
            Err(err) => {
                gl.delete_program(program);
                return Err(err);
            }
        };
        let uv = match attrib_location(gl, program, "a_uv") {
            Ok(location) => location,
            Err(err) => {
                gl.delete_program(program);
                return Err(err);
            }
        };
        Ok(Nv12Pipe {
            program,
            pos,
            uv,
            u_tex_y: gl.get_uniform_location(program, "u_tex_y"),
            u_tex_uv: gl.get_uniform_location(program, "u_tex_uv"),
        })
    }
}

/// 编译单个着色器。
///
/// # Safety
///
/// 调用方必须保证当前线程有 current 的 GLES 上下文。
unsafe fn compile_shader(
    gl: &glow::Context,
    kind: u32,
    source: &str,
) -> Result<glow::Shader, String> {
    // SAFETY: 由调用方保证上下文 current。
    unsafe {
        let shader = gl
            .create_shader(kind)
            .map_err(|err| format!("glCreateShader 失败：{err}"))?;
        gl.shader_source(shader, source);
        gl.compile_shader(shader);
        if gl.get_shader_compile_status(shader) {
            Ok(shader)
        } else {
            let log = gl.get_shader_info_log(shader);
            gl.delete_shader(shader);
            Err(format!("着色器编译失败：{log}"))
        }
    }
}
