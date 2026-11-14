//! DXVA/D3D11 硬件解码的 D3D11 侧支持。
//!
//! # 为什么需要设备管理器
//!
//! `MSH264DecoderMFT` 只有在收到 `MFT_MESSAGE_SET_D3D_MANAGER`（且此前已用
//! `MFCreateDXGIDeviceManager` + `ResetDevice` 绑定好 D3D11 设备）时才会走
//! DXVA 硬件解码，输出 `IMFDXGIBuffer` 而不是系统内存 NV12。少了这一步，
//! 解码完全落在 CPU 上——这正是 Intel HD 630 控制端 CPU 占用高的原因。
//!
//! # 为什么还要 staging 拷贝
//!
//! DXGI 输出是 GPU 纹理，CPU 读不了；本模块把它 `CopySubresourceRegion` 到
//! 一块 `D3D11_USAGE_STAGING` 的 NV12 纹理，再 `Map` 拿到 `RowPitch`。解码本身
//! 已由 GPU 完成，这里剩下的只是一次 NV12 回读 + BT.709→RGBA 转换。
//!
//! # 适配器选择
//!
//! 优先 Intel 核显（用户场景 HD 630，且多数笔记本的显示输出挂在核显上），
//! 没有核显时退回任意非软件适配器。设备必须用
//! `D3D11_CREATE_DEVICE_VIDEO_SUPPORT`（见 [`crate::d3d11::create`]）。

use super::{decode_err, mf_api};
use crate::d3d11::{self, D3d11};
use crate::WinVideoError;
use std::ffi::c_void;
use std::ptr;
use windows::core::Interface as _;
use windows::Win32::Graphics::Direct3D10::ID3D10Multithread;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::{
    IMFDXGIBuffer, IMFDXGIDeviceManager, IMFTransform, MFT_MESSAGE_SET_D3D_MANAGER,
};

/// DXVA 解码所需的 D3D11 设备、设备管理器与可复用的 staging 纹理。
pub(super) struct HardwareContext {
    /// D3D11 设备 / 立即上下文 / 适配器信息。
    device: D3d11,
    /// MFT 通过 `MFT_MESSAGE_SET_D3D_MANAGER` 引用的设备管理器。
    ///
    /// 只被持有、从不直接读取：MFT 内部保存了它的指针，必须活到 MFT 销毁，
    /// 提前释放会让后续解码访问悬空设备管理器。
    _manager: IMFDXGIDeviceManager,
    /// 从 DXGI 纹理回读用的 staging 纹理（按编码尺寸复用）。
    staging: Option<ID3D11Texture2D>,
    /// `staging` 对应的编码尺寸。
    staging_size: Option<(u32, u32)>,
}

impl std::fmt::Debug for HardwareContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HardwareContext")
            .field("adapter", &self.device.adapter.name)
            .field("staging_size", &self.staging_size)
            .finish_non_exhaustive()
    }
}

/// 选一块适合 DXVA 的硬件适配器并建设备：Intel 核显优先，否则任意非软件适配器。
pub(super) fn create_device() -> Result<D3d11, WinVideoError> {
    let adapters = d3d11::adapters()?;
    let chosen = adapters
        .iter()
        .find(|adapter| !adapter.software && adapter.vendor_id == d3d11::VENDOR_INTEL)
        .or_else(|| adapters.iter().find(|adapter| !adapter.software))
        .ok_or_else(|| {
            WinVideoError::Decode("没有可用的硬件适配器（DXVA 需要 D3D11）".to_owned())
        })?;
    let device = d3d11::create(Some(chosen.index))?;
    // D3D11_CREATE_DEVICE_VIDEO_SUPPORT 的文档要求开启多线程保护；解码器 MFT
    // 可能在自己的工作线程上访问设备。开启失败不致命（单线程仍可用），只记日志。
    match device.device.cast::<ID3D10Multithread>() {
        Ok(multithread) => {
            // SAFETY: `multithread` 是设备上的有效接口；设置是幂等的设备级开关。
            let was_protected = unsafe { multithread.SetMultithreadProtected(true) };
            if !was_protected.as_bool() {
                tracing::debug!("D3D11 设备多线程保护已开启");
            }
        }
        Err(err) => {
            tracing::debug!(%err, "D3D11 设备不支持 ID3D10Multithread，跳过多线程保护");
        }
    }
    tracing::debug!(adapter = %device.adapter.name, "DXVA 解码设备已创建");
    Ok(device)
}

impl HardwareContext {
    /// 创建设备管理器并把它交给 MFT；必须在设置输入/输出类型之前调用。
    pub(super) fn new(device: D3d11, transform: &IMFTransform) -> Result<Self, WinVideoError> {
        let manager = create_device_manager(&device)?;
        // SAFETY: `manager` 是有效的 COM 接口，`transform` 是有效 MFT；
        // `MFT_MESSAGE_SET_D3D_MANAGER` 的 ulParam 约定为设备管理器的裸指针。
        // manager 随后由本结构体持有，生命周期覆盖 MFT 的使用期。
        unsafe { transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize) }
            .map_err(|err| decode_err("MFT_MESSAGE_SET_D3D_MANAGER", &err))?;
        Ok(Self {
            device,
            _manager: manager,
            staging: None,
            staging_size: None,
        })
    }

    /// 实际绑定的 D3D11 适配器名。
    #[must_use]
    pub(super) fn adapter_name(&self) -> &str {
        &self.device.adapter.name
    }

    /// 把 DXGI 输出纹理拷到 staging、映射后执行 `convert`，最后保证解映射。
    ///
    /// `convert` 收到的是 NV12 连续内存指针与 `RowPitch`（字节）；UV 平面位于
    /// `pitch * coded_height` 之后，由调用方的转换函数按该约定读取。
    pub(super) fn with_mapped<T>(
        &mut self,
        coded_width: u32,
        coded_height: u32,
        buffer: &IMFDXGIBuffer,
        convert: impl FnOnce(*const u8, usize) -> Result<T, WinVideoError>,
    ) -> Result<T, WinVideoError> {
        let texture = Self::dxgi_texture(buffer)?;
        // SAFETY: `buffer` 有效，返回本栈帧内的 subresource 索引。
        let subresource = unsafe { buffer.GetSubresourceIndex() }
            .map_err(|err| decode_err("IMFDXGIBuffer::GetSubresourceIndex", &err))?;

        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `texture` 有效；`desc` 指向本栈帧。
        unsafe { texture.GetDesc(&raw mut desc) };
        if desc.Format != DXGI_FORMAT_NV12 {
            return Err(WinVideoError::Decode(format!(
                "DXGI 解码输出不是 NV12（{:?}）",
                desc.Format
            )));
        }
        if desc.Width != coded_width || desc.Height != coded_height {
            return Err(WinVideoError::Decode(format!(
                "DXGI 解码输出 {}x{} 与协商的编码尺寸 {coded_width}x{coded_height} 不一致",
                desc.Width, desc.Height
            )));
        }

        let staging = self.ensure_staging(coded_width, coded_height)?;
        // SAFETY: 源纹理来自本设备管理器绑定的同一 D3D11 设备；staging 是同设备的
        // STAGING 纹理，尺寸/格式一致；`None` 表示复制整个子资源。
        unsafe {
            self.device.context.CopySubresourceRegion(
                &staging,
                0,
                0,
                0,
                0,
                &texture,
                subresource,
                None,
            );
        }

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: `staging` 是 STAGING 纹理且带 CPU_ACCESS_READ；`mapped` 指向本栈帧。
        unsafe {
            self.device
                .context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&raw mut mapped))
        }
        .map_err(|err| decode_err("ID3D11DeviceContext::Map", &err))?;

        let pitch = usize::try_from(mapped.RowPitch)
            .map_err(|_| WinVideoError::Decode("DXGI RowPitch 无法转为 usize".to_owned()))?;
        let converted = convert(mapped.pData.cast::<u8>(), pitch);
        // SAFETY: 与上面的 Map 严格配对；无论转换成败都必须解映射。
        unsafe { self.device.context.Unmap(&staging, 0) };
        converted
    }

    /// 从 `IMFDXGIBuffer` 取出底层 `ID3D11Texture2D`。
    fn dxgi_texture(buffer: &IMFDXGIBuffer) -> Result<ID3D11Texture2D, WinVideoError> {
        let mut raw: *mut c_void = ptr::null_mut();
        // SAFETY: IID 与 `ID3D11Texture2D` 匹配；`raw` 指向本栈帧，成功后由
        // `Type::from_abi` 接管引用计数。
        unsafe { buffer.GetResource(&ID3D11Texture2D::IID, &raw mut raw) }
            .map_err(|err| decode_err("IMFDXGIBuffer::GetResource", &err))?;
        if raw.is_null() {
            return Err(WinVideoError::Decode(
                "IMFDXGIBuffer::GetResource 返回空纹理".to_owned(),
            ));
        }
        // SAFETY: 成功路径的 `raw` 非空且引用计数为 1。
        unsafe { windows::core::Type::from_abi(raw) }
            .map_err(|err| decode_err("IMFDXGIBuffer 资源接口转换", &err))
    }

    /// 取得（必要时创建）与编码尺寸匹配的 staging 纹理。
    fn ensure_staging(
        &mut self,
        width: u32,
        height: u32,
    ) -> Result<ID3D11Texture2D, WinVideoError> {
        if self.staging_size != Some((width, height)) || self.staging.is_none() {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: u32::try_from(D3D11_CPU_ACCESS_READ.0).unwrap_or_default(),
                MiscFlags: 0,
            };
            let mut texture: Option<ID3D11Texture2D> = None;
            // SAFETY: `desc` 指向本栈帧；`texture` 是输出槽，成功后由返回值接管。
            unsafe {
                self.device
                    .device
                    .CreateTexture2D(&raw const desc, None, Some(&raw mut texture))
            }
            .map_err(|err| decode_err("ID3D11Device::CreateTexture2D（staging）", &err))?;
            let texture = texture
                .ok_or_else(|| WinVideoError::Decode("创建 staging 纹理返回空对象".to_owned()))?;
            self.staging = Some(texture);
            self.staging_size = Some((width, height));
        }
        self.staging
            .clone()
            .ok_or_else(|| WinVideoError::Decode("staging 纹理缺失".to_owned()))
    }
}

/// `MFCreateDXGIDeviceManager` + `ResetDevice`（运行期符号，见 [`super::MfApi`]）。
fn create_device_manager(device: &D3d11) -> Result<IMFDXGIDeviceManager, WinVideoError> {
    let api = mf_api()?;
    let create = api.create_dxgi_device_manager.ok_or_else(|| {
        WinVideoError::Decode(
            "mfplat.dll 未导出 MFCreateDXGIDeviceManager（DXVA 不可用）".to_owned(),
        )
    })?;

    let mut reset_token = 0u32;
    let mut raw: *mut c_void = ptr::null_mut();
    // SAFETY: 两个输出指针都指向本栈帧；成功后返回引用计数为 1 的接口指针。
    let result = unsafe { create(&raw mut reset_token, &raw mut raw) };
    result
        .ok()
        .map_err(|err| decode_err("MFCreateDXGIDeviceManager", &err))?;
    if raw.is_null() {
        return Err(WinVideoError::Decode(
            "MFCreateDXGIDeviceManager 返回空管理器".to_owned(),
        ));
    }
    // SAFETY: `raw` 非空且引用计数为 1，所有权交给接口。
    let manager: IMFDXGIDeviceManager = unsafe { windows::core::Type::from_abi(raw) }
        .map_err(|err| decode_err("MFCreateDXGIDeviceManager（接口转换）", &err))?;
    // SAFETY: `manager` 与 `device.device` 都有效；reset token 与本管理器匹配。
    unsafe { manager.ResetDevice(&device.device, reset_token) }
        .map_err(|err| decode_err("IMFDXGIDeviceManager::ResetDevice", &err))?;
    Ok(manager)
}
