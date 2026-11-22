//! EGL 生命周期（display / config / context / window surface）。
//!
//! 只在渲染线程上使用：EGL 的 `Display`/`Context`/`Surface` 句柄都不是线程安全的，
//! 但渲染线程独占它们，故无需加锁。
//!
//! 上下文**优先 GLES3**（NV12 的 `GL_R8`/`GL_RG8` 平面纹理要求它），拿不到时回退 GLES2；
//! 两条路径共用同一套 EGLSurface 生命周期。
//!
//! 关键约束（见 `android/JNI_CONTRACT.md` §6）：窗口销毁时必须**先**
//! `eglMakeCurrent(..., EGL_NO_SURFACE, ...)` **再** `eglDestroySurface`——部分驱动在
//! 上下文仍绑定到已销毁 surface 时行为未定义；渲染线程退出时同样先解绑再销毁。

use std::ffi::c_void;

use khronos_egl as egl;
use ndk::native_window::NativeWindow;

/// 动态加载的 EGL 1.4 实例（Android 上即系统 `libEGL.so`）。
pub(crate) type EglInstance = egl::DynamicInstance<egl::EGL1_4>;

/// 请求的 EGL 配置：带窗口的渲染目标，8 位通道；`renderable` 为 ES2 / ES3 能力位。
///
/// NV12 平面纹理（`GL_R8` / `GL_RG8`）是 GLES3 才有的内部格式，所以**优先要 ES3**；
/// 拿不到 ES3 配置时回退 ES2（RGBA/VP9 路径不受影响，NV12 路径会在 `gl.rs` 里报不支持）。
const fn config_attrs(renderable: egl::Int) -> [egl::Int; 11] {
    [
        egl::SURFACE_TYPE,
        egl::WINDOW_BIT,
        egl::RENDERABLE_TYPE,
        renderable,
        egl::RED_SIZE,
        8,
        egl::GREEN_SIZE,
        8,
        egl::BLUE_SIZE,
        8,
        egl::NONE,
    ]
}

/// 一条 EGL 会话（display + config + context + 可选的 window surface）。
pub(crate) struct Egl {
    /// 动态库实例。
    instance: EglInstance,
    /// EGLDisplay。
    display: egl::Display,
    /// 选中的 EGLConfig。
    config: egl::Config,
    /// GLES 上下文（ES2 或 ES3）。
    context: egl::Context,
    /// 上下文实际使用的 GLES 主版本（2 或 3）。
    es_version: u32,
    /// 当前窗口 surface（`nativeSetSurface(null)` 或换 surface 时为 `None`）。
    surface: Option<egl::Surface>,
    /// `libGLESv2.so` 句柄：`eglGetProcAddress` 在部分驱动上拿不到核心函数，用它兜底。
    gles: Option<libloading::Library>,
}

impl Egl {
    /// 初始化 EGLDisplay / Config / Context（不需要 surface）。
    ///
    /// 先按**GLES3** 申请配置与上下文（NV12 硬解路径需要 `GL_R8`/`GL_RG8` 纹理），
    /// 任一步失败就回退 GLES2 并记一条 warning——GLES2 下 VP9/RGBA 照常工作，只有
    /// H.264/H.265 的 NV12 上屏不可用。
    ///
    /// # Errors
    ///
    /// 加载 `libEGL.so`、`eglInitialize`、两个版本都拿不到配置/上下文时返回可读的原因
    /// （渲染线程只记日志、不 panic）。
    pub(crate) fn new() -> Result<Self, String> {
        // SAFETY: 加载系统自带的 libEGL；接口版本由类型参数 EGL1_4 约束。
        let instance = unsafe { EglInstance::load_required() }
            .map_err(|err| format!("加载 libEGL.so 失败：{err:?}"))?;

        // SAFETY: `DEFAULT_DISPLAY` 是 EGL 规定的空指针。
        let display = unsafe { instance.get_display(egl::DEFAULT_DISPLAY) }
            .ok_or_else(|| "eglGetDisplay 返回 EGL_NO_DISPLAY".to_owned())?;

        instance
            .initialize(display)
            .map_err(|err| format!("eglInitialize 失败：{err:?}"))?;

        // Android 的默认客户端 API 就是 GLES，但显式绑定一次更稳妥。
        instance
            .bind_api(egl::OPENGL_ES_API)
            .map_err(|err| format!("eglBindAPI(GLES) 失败：{err:?}"))?;

        let (config, context, es_version) = match create_es_context(&instance, display, 3) {
            Ok(pair) => pair,
            Err(es3_err) => {
                tracing::warn!("GLES3 不可用（{es3_err}），回退 GLES2（NV12 上屏将不可用）");
                create_es_context(&instance, display, 2).map_err(|es2_err| {
                    format!("GLES2 也不可用（ES3 时：{es3_err}；ES2 时：{es2_err}）")
                })?
            }
        };

        // 兜底 loader：libGLESv2.so 在 Android 上总是存在；加载失败只影响 eglGetProcAddress
        // 拿不到的那部分符号（多数驱动会用得到）。
        let gles = unsafe { libloading::Library::new("libGLESv2.so") }.ok();
        tracing::info!("EGL 已初始化（GLES{es_version}）");

        Ok(Self {
            instance,
            display,
            config,
            context,
            es_version,
            surface: None,
            gles,
        })
    }

    /// 当前上下文是否为 GLES3（决定 NV12 平面纹理是否可用）。
    pub(crate) fn is_gles3(&self) -> bool {
        self.es_version >= 3
    }

    /// 为一个新的 `ANativeWindow` 创建 EGLSurface 并把它设为当前渲染目标。
    ///
    /// 调用前旧 surface 必须已通过 [`Egl::drop_surface`] 释放。
    ///
    /// # Errors
    ///
    /// `eglCreateWindowSurface` 或随后的 `eglMakeCurrent` 失败时返回原因（此时不会留下
    /// 半初始化的 surface）。
    pub(crate) fn create_window_surface(&mut self, window: &NativeWindow) -> Result<(), String> {
        let native: egl::NativeWindowType = window.ptr().as_ptr().cast();
        // SAFETY: `native` 来自 `ANativeWindow_fromSurface`（由 ndk 保活），生命周期覆盖本调用；
        // 渲染线程独占 EGL。
        let surface = unsafe {
            self.instance.create_window_surface(
                self.display,
                self.config,
                native,
                Some(&[egl::NONE]),
            )
        }
        .map_err(|err| format!("eglCreateWindowSurface 失败：{err:?}"))?;

        self.surface = Some(surface);
        self.make_current().inspect_err(|_err| {
            // 绑定失败时不要留下「已创建但没绑上」的 surface：下次进来会先 release。
            self.drop_surface();
        })
    }

    /// 解绑并销毁当前 surface（没有 surface 时为空操作）。
    pub(crate) fn drop_surface(&mut self) {
        let Some(surface) = self.surface.take() else {
            return;
        };
        // 先解绑（EGL_NO_SURFACE + 现有 context），再销毁 surface。
        if let Err(err) = self.make_context_only() {
            tracing::warn!("eglMakeCurrent(EGL_NO_SURFACE) 失败：{err}");
        }
        if let Err(err) = self.instance.destroy_surface(self.display, surface) {
            tracing::warn!("eglDestroySurface 失败：{err:?}");
        }
    }

    /// 把「context + 当前 surface」设为当前（要求 `surface` 存在）。
    ///
    /// # Errors
    ///
    /// 未创建 surface 或 `eglMakeCurrent` 失败时返回原因。
    pub(crate) fn make_current(&self) -> Result<(), String> {
        let Some(surface) = self.surface else {
            return Err("尚未创建 EGLSurface".to_owned());
        };
        self.instance
            .make_current(
                self.display,
                Some(surface),
                Some(surface),
                Some(self.context),
            )
            .map_err(|err| format!("eglMakeCurrent(surface) 失败：{err:?}"))
    }

    /// 只把 context 设为当前（不绑定 drawable）：用于销毁 GL 对象 / 解绑 surface。
    ///
    /// # Errors
    ///
    /// `eglMakeCurrent` 失败时返回原因。
    pub(crate) fn make_context_only(&self) -> Result<(), String> {
        self.instance
            .make_current(self.display, None, None, Some(self.context))
            .map_err(|err| format!("eglMakeCurrent(context) 失败：{err:?}"))
    }

    /// 查询当前 surface 的像素尺寸（窗口/旋转变化后由 `eglQuerySurface` 反映）。
    pub(crate) fn surface_size(&self) -> Option<(i32, i32)> {
        let surface = self.surface?;
        let width = self
            .instance
            .query_surface(self.display, surface, egl::WIDTH)
            .ok()?;
        let height = self
            .instance
            .query_surface(self.display, surface, egl::HEIGHT)
            .ok()?;
        (width > 0 && height > 0).then_some((width, height))
    }

    /// 提交一帧到窗口。
    ///
    /// # Errors
    ///
    /// `eglSwapBuffers` 失败时返回原因（常见于窗口销毁/退后台）。
    pub(crate) fn swap_buffers(&self) -> Result<(), String> {
        let Some(surface) = self.surface else {
            return Err("尚未创建 EGLSurface".to_owned());
        };
        self.instance
            .swap_buffers(self.display, surface)
            .map_err(|err| format!("eglSwapBuffers 失败：{err:?}"))
    }

    /// 解析一个 GLES 函数地址：先问 `eglGetProcAddress`，再兜底 `dlsym(libGLESv2.so)`。
    ///
    /// 返回空指针表示解析失败（glow 对该函数会给出「未加载」的空实现）。
    pub(crate) fn proc_address(&self, name: &str) -> *const c_void {
        if let Some(symbol) = self.instance.get_proc_address(name) {
            return symbol as *const c_void;
        }
        let Some(library) = self.gles.as_ref() else {
            return std::ptr::null();
        };
        // SAFETY: 符号是 GLES 函数入口；glow 只按同名函数的真实签名调用它。
        unsafe {
            library
                .get::<unsafe extern "C" fn()>(name.as_bytes())
                .map_or(std::ptr::null(), |symbol| *symbol as *const c_void)
        }
    }
}

/// 按指定 GLES 主版本申请 EGLConfig 与上下文（`version` 只会是 2 或 3）。
///
/// 返回选中的配置、上下文与实际版本，供 [`Egl::new`] 在 ES3 失败时整体重试 ES2。
fn create_es_context(
    instance: &EglInstance,
    display: egl::Display,
    version: u32,
) -> Result<(egl::Config, egl::Context, u32), String> {
    let renderable = if version >= 3 {
        egl::OPENGL_ES3_BIT
    } else {
        egl::OPENGL_ES2_BIT
    };
    let config = instance
        .choose_first_config(display, &config_attrs(renderable))
        .map_err(|err| format!("eglChooseConfig(ES{version}) 失败：{err:?}"))?
        .ok_or_else(|| format!("没有匹配 ES{version} 的 EGLConfig"))?;

    let attrs = [
        egl::CONTEXT_CLIENT_VERSION,
        i32::try_from(version).unwrap_or(2),
        egl::NONE,
    ];
    let context = instance
        .create_context(display, config, None, &attrs)
        .map_err(|err| format!("eglCreateContext(ES{version}) 失败：{err:?}"))?;
    Ok((config, context, version))
}

impl Drop for Egl {
    fn drop(&mut self) {
        self.drop_surface();
        if let Err(err) = self.instance.destroy_context(self.display, self.context) {
            tracing::warn!("eglDestroyContext 失败：{err:?}");
        }
        if let Err(err) = self.instance.terminate(self.display) {
            tracing::warn!("eglTerminate 失败：{err:?}");
        }
    }
}
