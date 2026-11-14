//! 运行期加载 `mfplat.dll`（编码器侧的 Media Foundation 入口）。
//!
//! 与 `decode` 的做法一致：**不在链接期导入 mfplat.dll**。Windows 10 N/LTSC
//! 或精简镜像可能没有 Media Foundation 组件，静态导入会让进程在 `main` 之前
//! 加载失败；这里缺符号时返回明确错误，由上层回退 VP9。
//!
//! [`MftH264Encoder`](super::MftH264Encoder) 需要 `MFTEnumEx`（枚举硬件编码器）
//! 与样本/缓冲区/媒体类型创建函数。`MFTEnumEx` 是 mfplat.dll 的导出，不能静态
//! 导入；decode 侧的 `MfApi` 只服务解码且不含它，故本模块自带一张函数表。

use crate::WinVideoError;
use std::ffi::c_void;
use std::ptr;
use std::sync::OnceLock;
use windows::core::{Interface as _, BOOL, GUID, HRESULT};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFDXGIDeviceManager, IMFMediaBuffer, IMFMediaType, IMFSample, MFSTARTUP_FULL,
    MFT_REGISTER_TYPE_INFO, MF_VERSION,
};

/// `MFTEnumEx(GUID, UINT, in, out, IMFActivate***, UINT*)`。
type MftEnumExFn = unsafe extern "system" fn(
    GUID,
    u32,
    *const MFT_REGISTER_TYPE_INFO,
    *const MFT_REGISTER_TYPE_INFO,
    *mut *mut *mut c_void,
    *mut u32,
) -> HRESULT;

/// 运行期解析出的 mfplat 入口表。
struct MfPlatformApi {
    /// 库必须活到函数指针失效之后。
    _lib: libloading::Library,
    /// `MFStartup(version, flags)`。
    startup: unsafe extern "system" fn(u32, u32) -> HRESULT,
    /// `MFTEnumEx(...)`；这是本模块存在的原因。
    enum_ex: MftEnumExFn,
    /// `MFCreateSample(&mut ptr)`。
    create_sample: unsafe extern "system" fn(*mut *mut c_void) -> HRESULT,
    /// `MFCreateMemoryBuffer(len, &mut ptr)`。
    create_memory_buffer: unsafe extern "system" fn(u32, *mut *mut c_void) -> HRESULT,
    /// `MFCreateMediaType(&mut ptr)`。
    create_media_type: unsafe extern "system" fn(*mut *mut c_void) -> HRESULT,
    /// `MFCreateDXGIDeviceManager(&mut token, &mut manager)`。
    ///
    /// 硬件编码器 MFT 需要它把 D3D11 设备交给 GPU 编码器；缺失时硬件编码
    /// 初始化失败并回退 VP9。
    create_dxgi_device_manager:
        Option<unsafe extern "system" fn(*mut u32, *mut *mut c_void) -> HRESULT>,
    /// `MFCreateDXGISurfaceBuffer(riid, surface, subresource, bottom_up, &buffer)`。
    ///
    /// GPU 纹理路径用它把 VideoProcessor 输出的 NV12 纹理包成 `IMFMediaBuffer`，
    /// 不经过 CPU 内存。
    create_dxgi_surface_buffer: Option<
        unsafe extern "system" fn(*const GUID, *mut c_void, u32, BOOL, *mut *mut c_void) -> HRESULT,
    >,
}

// SAFETY: 函数表在进程生命周期内只读；mfplat.dll 由 `OnceLock` 永久持有，
// 不会卸载；每次调用创建的 COM 对象仍由调用线程独占。
unsafe impl Send for MfPlatformApi {}
// SAFETY: 同上——只读函数表可以被多个线程同时读取。
unsafe impl Sync for MfPlatformApi {}

impl MfPlatformApi {
    fn load() -> Result<Self, String> {
        // SAFETY: 只加载系统目录下的 mfplat.dll；符号在库存活期间有效。
        let lib = unsafe { libloading::Library::new("mfplat.dll") }.map_err(|err| {
            format!("加载 mfplat.dll 失败（硬件编码需要 Media Foundation）: {err}")
        })?;

        // SAFETY: 符号名与函数指针签名必须和 Media Foundation 的 C ABI 一致。
        let startup = unsafe {
            *lib.get::<unsafe extern "system" fn(u32, u32) -> HRESULT>(b"MFStartup")
                .map_err(|err| format!("缺少 MFStartup: {err}"))?
        };
        // SAFETY: 同上。
        let enum_ex = unsafe {
            *lib.get::<MftEnumExFn>(b"MFTEnumEx")
                .map_err(|err| format!("缺少 MFTEnumEx: {err}"))?
        };
        // SAFETY: 同上。
        let create_sample = unsafe {
            *lib.get::<unsafe extern "system" fn(*mut *mut c_void) -> HRESULT>(b"MFCreateSample")
                .map_err(|err| format!("缺少 MFCreateSample: {err}"))?
        };
        // SAFETY: 同上。
        let create_memory_buffer = unsafe {
            *lib.get::<unsafe extern "system" fn(u32, *mut *mut c_void) -> HRESULT>(
                b"MFCreateMemoryBuffer",
            )
            .map_err(|err| format!("缺少 MFCreateMemoryBuffer: {err}"))?
        };
        // SAFETY: 同上。
        let create_media_type = unsafe {
            *lib.get::<unsafe extern "system" fn(*mut *mut c_void) -> HRESULT>(b"MFCreateMediaType")
                .map_err(|err| format!("缺少 MFCreateMediaType: {err}"))?
        };

        // SAFETY: 可选符号；缺失时硬件编码初始化失败并回退 VP9。
        let create_dxgi_device_manager = unsafe {
            lib.get::<unsafe extern "system" fn(*mut u32, *mut *mut c_void) -> HRESULT>(
                b"MFCreateDXGIDeviceManager",
            )
            .ok()
            .map(|symbol| *symbol)
        };
        // SAFETY: 可选符号；缺失时 GPU 纹理路径报错，CPU 路径不受影响。
        let create_dxgi_surface_buffer = unsafe {
            lib.get::<unsafe extern "system" fn(
                *const GUID,
                *mut c_void,
                u32,
                BOOL,
                *mut *mut c_void,
            ) -> HRESULT>(b"MFCreateDXGISurfaceBuffer")
                .ok()
                .map(|symbol| *symbol)
        };

        Ok(Self {
            _lib: lib,
            startup,
            enum_ex,
            create_sample,
            create_memory_buffer,
            create_media_type,
            create_dxgi_device_manager,
            create_dxgi_surface_buffer,
        })
    }
}

/// 取（或首次加载）mfplat 入口表。
fn api() -> Result<&'static MfPlatformApi, WinVideoError> {
    static API: OnceLock<Result<MfPlatformApi, String>> = OnceLock::new();
    match API.get_or_init(MfPlatformApi::load) {
        Ok(api) => Ok(api),
        Err(message) => Err(WinVideoError::Encode(message.clone())),
    }
}

/// 进程级 `MFStartup`（解码侧可能已经调用过；MFStartup 可重入）。
pub(super) fn ensure_started() -> Result<(), WinVideoError> {
    let api = api()?;
    // SAFETY: `MFStartup` 是进程级初始化，`MF_VERSION` 是编译期常数。
    unsafe { (api.startup)(MF_VERSION, MFSTARTUP_FULL) }
        .ok()
        .map_err(|err| WinVideoError::Encode(format!("MFStartup 失败: {err}")))
}

/// 用 `MFTEnumEx` 枚举编码器激活对象。
///
/// `input` / `output` 是登记类型过滤；返回的 `IMFActivate` 数组由本函数接管，
/// 底层 `CoTaskMemAlloc` 的内存在取出对象后立即释放。
pub(super) fn enumerate_encoders(
    flags: u32,
    input: &MFT_REGISTER_TYPE_INFO,
    output: &MFT_REGISTER_TYPE_INFO,
) -> Result<Vec<IMFActivate>, WinVideoError> {
    let api = api()?;
    let mut array: *mut *mut c_void = ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: 两个类型过滤与输出指针都指向本栈帧；成功后 `array` 是
    // CoTaskMemAlloc 分配的 `IMFActivate*` 数组。
    let result = unsafe {
        (api.enum_ex)(
            windows::Win32::Media::MediaFoundation::MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            ptr::from_ref(input),
            ptr::from_ref(output),
            &raw mut array,
            &raw mut count,
        )
    };
    result
        .ok()
        .map_err(|err| WinVideoError::Encode(format!("MFTEnumEx 失败: {err}")))?;

    let mut activates = Vec::new();
    if !array.is_null() {
        let len = usize::try_from(count).unwrap_or(0);
        // SAFETY: `MFTEnumEx` 承诺 `array` 指向 `count` 个接口指针。
        let pointers = unsafe { std::slice::from_raw_parts(array, len) };
        for &pointer in pointers {
            if pointer.is_null() {
                continue;
            }
            // SAFETY: 每个非空指针都是引用计数为 1 的 IMFActivate，所有权移交给 Vec。
            let converted: windows::core::Result<IMFActivate> =
                unsafe { windows::core::Type::from_abi(pointer) };
            match converted {
                Ok(activate) => activates.push(activate),
                Err(err) => {
                    tracing::debug!(%err, "枚举到的编码器激活对象无法转换，跳过");
                }
            }
        }
        // SAFETY: 数组由 MFTEnumEx 用 CoTaskMemAlloc 分配；对象引用已取走，
        // 这里只释放指针数组本身。
        unsafe {
            windows::Win32::System::Com::CoTaskMemFree(Some(array.cast::<c_void>().cast_const()));
        }
    }
    Ok(activates)
}

/// `MFCreateMediaType` 的运行期版本。
pub(super) fn create_media_type() -> Result<IMFMediaType, WinVideoError> {
    let api = api()?;
    let mut raw: *mut c_void = ptr::null_mut();
    // SAFETY: `raw` 指向本栈帧；成功后返回一个由 `from_abi` 接管的接口指针。
    unsafe { (api.create_media_type)(&raw mut raw) }
        .ok()
        .map_err(|err| WinVideoError::Encode(format!("MFCreateMediaType 失败: {err}")))?;
    // SAFETY: 成功路径的 `raw` 非空且引用计数为 1，所有权交给返回的接口。
    unsafe { windows::core::Type::from_abi(raw) }
        .map_err(|err| WinVideoError::Encode(format!("MFCreateMediaType 接口转换失败: {err}")))
}

/// `MFCreateMemoryBuffer` 的运行期版本。
pub(super) fn create_memory_buffer(length: u32) -> Result<IMFMediaBuffer, WinVideoError> {
    let api = api()?;
    let mut raw: *mut c_void = ptr::null_mut();
    // SAFETY: `raw` 指向本栈帧；成功后返回一个由 `from_abi` 接管的接口指针。
    unsafe { (api.create_memory_buffer)(length, &raw mut raw) }
        .ok()
        .map_err(|err| WinVideoError::Encode(format!("MFCreateMemoryBuffer 失败: {err}")))?;
    // SAFETY: 成功路径的 `raw` 非空且引用计数为 1，所有权交给返回的接口。
    unsafe { windows::core::Type::from_abi(raw) }
        .map_err(|err| WinVideoError::Encode(format!("MFCreateMemoryBuffer 接口转换失败: {err}")))
}

/// `MFCreateDXGIDeviceManager` 的运行期版本（只创建管理器，不绑定设备）。
pub(super) fn create_dxgi_device_manager() -> Result<(IMFDXGIDeviceManager, u32), WinVideoError> {
    let api = api()?;
    let create = api.create_dxgi_device_manager.ok_or_else(|| {
        WinVideoError::Encode(
            "mfplat.dll 未导出 MFCreateDXGIDeviceManager（硬件编码不可用）".to_owned(),
        )
    })?;
    let mut reset_token = 0u32;
    let mut raw: *mut c_void = ptr::null_mut();
    // SAFETY: 两个输出指针都指向本栈帧；成功后返回引用计数为 1 的接口指针。
    unsafe { create(&raw mut reset_token, &raw mut raw) }
        .ok()
        .map_err(|err| WinVideoError::Encode(format!("MFCreateDXGIDeviceManager 失败: {err}")))?;
    if raw.is_null() {
        return Err(WinVideoError::Encode(
            "MFCreateDXGIDeviceManager 返回空管理器".to_owned(),
        ));
    }
    // SAFETY: `raw` 非空且引用计数为 1，所有权交给接口。
    let manager: IMFDXGIDeviceManager = unsafe { windows::core::Type::from_abi(raw) }
        .map_err(|err| WinVideoError::Encode(format!("设备管理器接口转换失败: {err}")))?;
    Ok((manager, reset_token))
}

/// `MFCreateDXGISurfaceBuffer` 的运行期版本。
///
/// `riid` 用 **`ID3D11Texture2D::IID`**（这是「表面接口」的 IID，不是
/// `IMFMediaBuffer::IID`）；`bottom_up_when_linear` 只对线性格式有意义，NV12
/// 传 `false`。
pub(super) fn create_dxgi_surface_buffer(
    texture: &ID3D11Texture2D,
    subresource: u32,
    bottom_up_when_linear: bool,
) -> Result<IMFMediaBuffer, WinVideoError> {
    let api = api()?;
    let create = api.create_dxgi_surface_buffer.ok_or_else(|| {
        WinVideoError::Encode(
            "mfplat.dll 未导出 MFCreateDXGISurfaceBuffer（GPU 纹理输入不可用）".to_owned(),
        )
    })?;
    let mut raw: *mut c_void = ptr::null_mut();
    // SAFETY: `riid` 是 `'static`；`texture` 借用期间有效，底层对象由 COM 引用
    // 计数持有；`raw` 指向本栈帧。成功时返回引用计数为 1 的 IMFMediaBuffer。
    unsafe {
        create(
            &ID3D11Texture2D::IID,
            texture.as_raw(),
            subresource,
            BOOL(i32::from(bottom_up_when_linear)),
            &raw mut raw,
        )
    }
    .ok()
    .map_err(|err| WinVideoError::Encode(format!("MFCreateDXGISurfaceBuffer 失败: {err}")))?;
    if raw.is_null() {
        return Err(WinVideoError::Encode(
            "MFCreateDXGISurfaceBuffer 返回空缓冲区".to_owned(),
        ));
    }
    // SAFETY: `raw` 非空且引用计数为 1，所有权交给接口。
    unsafe { windows::core::Type::from_abi(raw) }
        .map_err(|err| WinVideoError::Encode(format!("DXGI 表面缓冲接口转换失败: {err}")))
}

/// `MFCreateSample` 的运行期版本。
pub(super) fn create_sample() -> Result<IMFSample, WinVideoError> {
    let api = api()?;
    let mut raw: *mut c_void = ptr::null_mut();
    // SAFETY: `raw` 指向本栈帧；成功后返回一个由 `from_abi` 接管的接口指针。
    unsafe { (api.create_sample)(&raw mut raw) }
        .ok()
        .map_err(|err| WinVideoError::Encode(format!("MFCreateSample 失败: {err}")))?;
    // SAFETY: 成功路径的 `raw` 非空且引用计数为 1，所有权交给返回的接口。
    unsafe { windows::core::Type::from_abi(raw) }
        .map_err(|err| WinVideoError::Encode(format!("MFCreateSample 接口转换失败: {err}")))
}
