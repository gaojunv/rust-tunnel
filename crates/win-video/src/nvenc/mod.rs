//! NVENC H.264 4:2:0 硬件编码器的运行期加载与能力查询。
//!
//! 驱动库 `nvEncodeAPI64.dll` 在运行期用 `libloading` 打开：没有 NVIDIA 驱动的
//! 机器上本 crate 仍能编译、也能正常回退到软件编码（调用方据此选择 VP9）。
//!
//! 绑定是 vendored 的（见 [`sys`]），对应 NVENC API 12.1。打开会话时请求
//! `min(驱动支持的最大版本, 12.1)`，而不是硬编码 13.x——旧驱动（例如 470 系列）
//! 也能工作。

mod encoder;
mod sys;

use crate::d3d11::D3d11;
use crate::WinVideoError;
use std::ffi::{c_int, c_void};
use std::ptr;
use windows::core::Interface as _;

pub use encoder::{
    EncodedPacket, EncoderConfig, InputFormat, NvencEncoder, MAX_ENCODE_HEIGHT, MAX_ENCODE_WIDTH,
    MAX_SOURCE_DIMENSION,
};
pub use sys::{
    NV_ENC_BUFFER_FORMAT, NV_ENC_CAPS, NV_ENC_CODEC_H264_GUID, NV_ENC_H264_PROFILE_HIGH_444_GUID,
    NV_ENC_H264_PROFILE_HIGH_GUID,
};

/// 本 crate 绑定对应的 NVENC API 版本。
#[must_use]
pub fn bound_api_version() -> (u32, u32) {
    (sys::NVENCAPI_MAJOR_VERSION, sys::NVENCAPI_MINOR_VERSION)
}

fn status_ok(status: sys::NVENCSTATUS) -> bool {
    status == sys::_NVENCSTATUS::NV_ENC_SUCCESS
}

fn status_err(context: &str, status: sys::NVENCSTATUS) -> WinVideoError {
    WinVideoError::Nvenc(format!("{context} 失败（NVENCSTATUS={status:?}）"))
}

/// 已加载的 NVENC 驱动函数表。
pub struct NvencApi {
    /// 驱动库必须活到函数表失效之后。
    _lib: libloading::Library,
    /// 驱动填充的函数指针表。
    fns: sys::NV_ENCODE_API_FUNCTION_LIST,
    /// 驱动支持的最大 NVENC API 版本 `(major, minor)`。
    driver_api: (u32, u32),
}

impl std::fmt::Debug for NvencApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NvencApi")
            .field("driver_api", &self.driver_api)
            .finish_non_exhaustive()
    }
}

impl NvencApi {
    /// 打开 `nvEncodeAPI64.dll` 并取得函数表。
    ///
    /// # Errors
    /// 驱动库不存在、入口点缺失或函数表创建失败时返回 [`WinVideoError::Nvenc`]。
    pub fn load() -> Result<Self, WinVideoError> {
        // SAFETY: 只加载系统目录下的 NVIDIA 驱动 DLL；符号在库存活期间有效。
        let lib = unsafe { libloading::Library::new("nvEncodeAPI64.dll") }
            .map_err(|e| WinVideoError::Nvenc(format!("加载 nvEncodeAPI64.dll 失败: {e}")))?;

        // 驱动支持的最大版本（用于选择 apiVersion）。
        let mut driver_version: u32 = 0;
        // SAFETY: 入口点是 NVIDIA 导出的稳定 C ABI。
        let get_max: libloading::Symbol<unsafe extern "C" fn(*mut u32) -> sys::NVENCSTATUS> =
            unsafe { lib.get(b"NvEncodeAPIGetMaxSupportedVersion") }.map_err(|e| {
                WinVideoError::Nvenc(format!("缺少 NvEncodeAPIGetMaxSupportedVersion: {e}"))
            })?;
        // SAFETY: out 指针指向栈上 u32。
        let status = unsafe { get_max(&raw mut driver_version) };
        if !status_ok(status) {
            return Err(status_err("NvEncodeAPIGetMaxSupportedVersion", status));
        }
        // `NvEncodeAPIGetMaxSupportedVersion` 的编码规则与 `NVENCAPI_VERSION`
        // 不同：低 4 位是 minor，其余位是 major（见 nvEncodeAPI.h 的注释）。
        let driver_api = (driver_version >> 4, driver_version & 0xF);

        // SAFETY: `NvEncodeAPICreateInstance` 是 NVIDIA 导出的稳定 C ABI。
        let create: libloading::Symbol<
            unsafe extern "C" fn(*mut sys::NV_ENCODE_API_FUNCTION_LIST) -> sys::NVENCSTATUS,
        > = unsafe { lib.get(b"NvEncodeAPICreateInstance") }
            .map_err(|e| WinVideoError::Nvenc(format!("缺少 NvEncodeAPICreateInstance: {e}")))?;

        // SAFETY: 结构体是 POD；驱动只写入函数指针，并检查 version。
        let mut fns: sys::NV_ENCODE_API_FUNCTION_LIST = unsafe { std::mem::zeroed() };
        fns.version = sys::NV_ENCODE_API_FUNCTION_LIST_VER;
        // SAFETY: 指针指向栈上结构体，调用期间有效。
        let status = unsafe { create(&raw mut fns) };
        if !status_ok(status) {
            return Err(status_err("NvEncodeAPICreateInstance", status));
        }

        Ok(Self {
            _lib: lib,
            fns,
            driver_api,
        })
    }

    /// 驱动支持的最大 NVENC API 版本。
    #[must_use]
    pub fn driver_api_version(&self) -> (u32, u32) {
        self.driver_api
    }

    /// 实际请求给驱动的 API 版本：本 crate 绑定版本与驱动版本的较小者。
    #[must_use]
    pub fn requested_api_version(&self) -> u32 {
        let (bound_major, bound_minor) = bound_api_version();
        let (drv_major, drv_minor) = self.driver_api;
        if (drv_major, drv_minor) < (bound_major, bound_minor) {
            (drv_major & 0xFF) | ((drv_minor & 0xFF) << 24)
        } else {
            sys::NVENCAPI_VERSION
        }
    }

    /// 在给定的 D3D11 设备上打开一个 H.264 编码会话。
    ///
    /// # Errors
    /// 设备不是 NVIDIA 适配器、或驱动拒绝会话参数时返回 [`WinVideoError::Nvenc`]。
    pub fn open_session<'a>(&'a self, d3d: &D3d11) -> Result<NvencSession<'a>, WinVideoError> {
        let open = self.fns.nvEncOpenEncodeSessionEx.ok_or_else(|| {
            WinVideoError::Nvenc("驱动未导出 nvEncOpenEncodeSessionEx".to_owned())
        })?;

        // SAFETY: 结构体是 POD；`device` 是有效的 ID3D11Device 指针，且 d3d 在本会话
        // 存活期间不会被释放（借用检查保证）。
        let mut params: sys::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS = unsafe { std::mem::zeroed() };
        params.version = sys::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
        params.deviceType = sys::_NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_DIRECTX;
        params.device = d3d.device.as_raw();
        params.apiVersion = self.requested_api_version();

        let mut handle: *mut c_void = ptr::null_mut();
        // SAFETY: 两个指针都指向本栈帧内的有效存储。
        let status = unsafe { open(&raw mut params, &raw mut handle) };
        if !status_ok(status) {
            return Err(status_err("nvEncOpenEncodeSessionEx", status));
        }
        if handle.is_null() {
            return Err(WinVideoError::Nvenc("NVENC 返回空会话句柄".to_owned()));
        }
        Ok(NvencSession { api: self, handle })
    }

    /// 查询某个编码能力（`NV_ENC_CAPS`）。
    fn cap(
        &self,
        session: &NvencSession<'_>,
        codec: sys::GUID,
        cap: sys::NV_ENC_CAPS,
    ) -> Result<i32, WinVideoError> {
        let query = self
            .fns
            .nvEncGetEncodeCaps
            .ok_or_else(|| WinVideoError::Nvenc("驱动未导出 nvEncGetEncodeCaps".to_owned()))?;
        // SAFETY: POD；capsToQuery 是普通枚举值。
        let mut params: sys::NV_ENC_CAPS_PARAM = unsafe { std::mem::zeroed() };
        params.version = sys::NV_ENC_CAPS_PARAM_VER;
        params.capsToQuery = cap;
        let mut value: c_int = 0;
        // SAFETY: 会话句柄有效；参数与输出指针指向栈上存储。
        let status = unsafe { query(session.handle, codec, &raw mut params, &raw mut value) };
        if !status_ok(status) {
            return Err(status_err("nvEncGetEncodeCaps", status));
        }
        Ok(value)
    }

    /// 查询 H.264 编码器的关键能力。
    ///
    /// # Errors
    /// 能力查询失败时返回 [`WinVideoError::Nvenc`]。
    pub fn h264_caps(&self, session: &NvencSession<'_>) -> Result<H264Caps, WinVideoError> {
        let codec = sys::NV_ENC_CODEC_H264_GUID;
        Ok(H264Caps {
            max_width: self.cap(session, codec, sys::_NV_ENC_CAPS::NV_ENC_CAPS_WIDTH_MAX)?,
            max_height: self.cap(session, codec, sys::_NV_ENC_CAPS::NV_ENC_CAPS_HEIGHT_MAX)?,
            max_bframes: self.cap(
                session,
                codec,
                sys::_NV_ENC_CAPS::NV_ENC_CAPS_NUM_MAX_BFRAMES,
            )?,
            yuv444_encode: self.cap(
                session,
                codec,
                sys::_NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_YUV444_ENCODE,
            )?,
            ten_bit_encode: self.cap(
                session,
                codec,
                sys::_NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_10BIT_ENCODE,
            )?,
            dyn_bitrate: self.cap(
                session,
                codec,
                sys::_NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_DYN_BITRATE_CHANGE,
            )?,
            dyn_resolution: self.cap(
                session,
                codec,
                sys::_NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_DYN_RES_CHANGE,
            )?,
            async_encode: self.cap(
                session,
                codec,
                sys::_NV_ENC_CAPS::NV_ENC_CAPS_ASYNC_ENCODE_SUPPORT,
            )?,
            lookahead: self.cap(
                session,
                codec,
                sys::_NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_LOOKAHEAD,
            )?,
            temporal_aq: self.cap(
                session,
                codec,
                sys::_NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_TEMPORAL_AQ,
            )?,
            mb_per_sec_max: self.cap(
                session,
                codec,
                sys::_NV_ENC_CAPS::NV_ENC_CAPS_MB_PER_SEC_MAX,
            )?,
        })
    }

    /// 查询 H.264 支持的 profile GUID。
    ///
    /// # Errors
    /// 查询失败时返回 [`WinVideoError::Nvenc`]。
    pub fn h264_profiles(
        &self,
        session: &NvencSession<'_>,
    ) -> Result<Vec<sys::GUID>, WinVideoError> {
        let count_fn = self
            .fns
            .nvEncGetEncodeProfileGUIDCount
            .ok_or_else(|| WinVideoError::Nvenc("驱动未导出 profile 计数入口".to_owned()))?;
        let guids_fn = self
            .fns
            .nvEncGetEncodeProfileGUIDs
            .ok_or_else(|| WinVideoError::Nvenc("驱动未导出 profile 枚举入口".to_owned()))?;

        let codec = sys::NV_ENC_CODEC_H264_GUID;
        let mut count: u32 = 0;
        // SAFETY: 输出指针指向栈上 u32。
        let status = unsafe { count_fn(session.handle, codec, &raw mut count) };
        if !status_ok(status) {
            return Err(status_err("nvEncGetEncodeProfileGUIDCount", status));
        }
        let mut guids = vec![sys::GUID::default(); count as usize];
        let mut actual: u32 = 0;
        // SAFETY: 缓冲区大小与 count 一致。
        let status = unsafe {
            guids_fn(
                session.handle,
                codec,
                guids.as_mut_ptr(),
                count,
                &raw mut actual,
            )
        };
        if !status_ok(status) {
            return Err(status_err("nvEncGetEncodeProfileGUIDs", status));
        }
        guids.truncate(actual as usize);
        Ok(guids)
    }

    /// 查询 H.264 支持的输入图像格式。
    ///
    /// # Errors
    /// 查询失败时返回 [`WinVideoError::Nvenc`]。
    pub fn h264_input_formats(
        &self,
        session: &NvencSession<'_>,
    ) -> Result<Vec<sys::NV_ENC_BUFFER_FORMAT>, WinVideoError> {
        let count_fn = self
            .fns
            .nvEncGetInputFormatCount
            .ok_or_else(|| WinVideoError::Nvenc("驱动未导出输入格式计数入口".to_owned()))?;
        let formats_fn = self
            .fns
            .nvEncGetInputFormats
            .ok_or_else(|| WinVideoError::Nvenc("驱动未导出输入格式枚举入口".to_owned()))?;
        let codec = sys::NV_ENC_CODEC_H264_GUID;

        let mut count: u32 = 0;
        // SAFETY: 输出指针指向栈上 u32。
        let status = unsafe { count_fn(session.handle, codec, &raw mut count) };
        if !status_ok(status) {
            return Err(status_err("nvEncGetInputFormatCount", status));
        }
        let mut formats =
            vec![sys::_NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_UNDEFINED; count as usize];
        let mut actual: u32 = 0;
        // SAFETY: 缓冲区大小与 count 一致。
        let status = unsafe {
            formats_fn(
                session.handle,
                codec,
                formats.as_mut_ptr(),
                count,
                &raw mut actual,
            )
        };
        if !status_ok(status) {
            return Err(status_err("nvEncGetInputFormats", status));
        }
        formats.truncate(actual as usize);
        Ok(formats)
    }
}

/// H.264 编码器能力快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct H264Caps {
    /// 最大编码宽度。
    pub max_width: i32,
    /// 最大编码高度。
    pub max_height: i32,
    /// 最大 B 帧数。
    pub max_bframes: i32,
    /// 是否支持 4:4:4 编码（H.264 通常为 0；保留展示）。
    pub yuv444_encode: i32,
    /// 是否支持 10-bit 编码。
    pub ten_bit_encode: i32,
    /// 是否支持运行期改码率。
    pub dyn_bitrate: i32,
    /// 是否支持运行期改分辨率。
    pub dyn_resolution: i32,
    /// 是否支持异步编码。
    pub async_encode: i32,
    /// 是否支持 lookahead。
    pub lookahead: i32,
    /// 是否支持 temporal AQ。
    pub temporal_aq: i32,
    /// 每秒最大宏块数。
    pub mb_per_sec_max: i32,
}

/// 一个打开的 NVENC 编码会话。
///
/// 生命周期绑定到 [`NvencApi`]：驱动函数表必须先于会话销毁。
pub struct NvencSession<'a> {
    api: &'a NvencApi,
    handle: *mut c_void,
}

impl std::fmt::Debug for NvencSession<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NvencSession")
            .field("handle", &self.handle)
            .finish_non_exhaustive()
    }
}

impl NvencSession<'_> {
    /// 原始会话句柄（编码器模块内部使用）。
    #[must_use]
    pub(crate) fn handle(&self) -> *mut c_void {
        self.handle
    }

    /// 所属函数表。
    #[must_use]
    pub(crate) fn api(&self) -> &NvencApi {
        self.api
    }
}

impl Drop for NvencSession<'_> {
    fn drop(&mut self) {
        if let Some(destroy) = self.api.fns.nvEncDestroyEncoder {
            // SAFETY: 句柄由本对象独占，只销毁一次。
            unsafe { destroy(self.handle) };
        }
        self.handle = ptr::null_mut();
    }
}
