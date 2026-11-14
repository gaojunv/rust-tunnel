//! 硬件编码器需要的 D3D11 设备与设备管理器。
//!
//! 硬件编码器 MFT（Intel Quick Sync / NVIDIA / AMD）不是一个纯软件变换器：
//! 它需要一块 D3D11 设备来做 GPU 编码。客户端必须：
//!
//! 1. 在硬件适配器上创建 D3D11 设备（`D3D11_CREATE_DEVICE_VIDEO_SUPPORT`）；
//! 2. `MFCreateDXGIDeviceManager` + `ResetDevice`；
//! 3. 在设置媒体类型**之前**用 `MFT_MESSAGE_SET_D3D_MANAGER` 把管理器交给 MFT。
//!
//! 少了这一步，Intel QSV MFT 会在 `ProcessOutput` 返回 `E_UNEXPECTED`
//! （解码侧也有同样的设备管理器要求，见 `decode::dxva`）。

use super::mfplat;
use crate::d3d11::{self, D3d11};
use crate::WinVideoError;
use windows::core::Interface as _;
use windows::Win32::Graphics::Direct3D10::ID3D10Multithread;
use windows::Win32::Media::MediaFoundation::{
    IMFDXGIDeviceManager, IMFTransform, MFT_MESSAGE_SET_D3D_MANAGER,
};

/// 编码器绑定的 D3D11 设备与设备管理器。
pub(super) struct EncodeDevice {
    /// D3D11 设备 / 立即上下文 / 适配器信息。
    pub device: D3d11,
    /// 交给 MFT 的设备管理器；必须活到 MFT 销毁。
    pub manager: IMFDXGIDeviceManager,
}

impl std::fmt::Debug for EncodeDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncodeDevice")
            .field("adapter", &self.device.adapter.name)
            .finish_non_exhaustive()
    }
}

/// 在首选的硬件适配器上建设备并创建管理器。
pub(super) fn create() -> Result<EncodeDevice, WinVideoError> {
    let adapters = d3d11::adapters()?;
    let chosen = adapters
        .iter()
        .find(|adapter| !adapter.software && adapter.vendor_id == d3d11::VENDOR_INTEL)
        .or_else(|| adapters.iter().find(|adapter| !adapter.software))
        .ok_or_else(|| {
            WinVideoError::Encode("没有可用的硬件适配器（硬件编码需要 D3D11）".to_owned())
        })?;
    let device = d3d11::create(Some(chosen.index))?;

    // D3D11_CREATE_DEVICE_VIDEO_SUPPORT 要求开启多线程保护；编码器 MFT 可能
    // 在自己的工作线程上访问设备。
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

    let (manager, reset_token) = mfplat::create_dxgi_device_manager()?;
    // SAFETY: `manager` 与 `device.device` 都有效；reset token 与本管理器匹配。
    unsafe { manager.ResetDevice(&device.device, reset_token) }.map_err(|err| {
        WinVideoError::Encode(format!("ID3D11DeviceManager::ResetDevice 失败: {err}"))
    })?;
    tracing::debug!(adapter = %device.adapter.name, "硬件编码 D3D11 设备已就绪");
    Ok(EncodeDevice { device, manager })
}

/// 把设备管理器交给 MFT（必须在设置媒体类型之前）。
pub(super) fn bind(
    encode_device: &EncodeDevice,
    transform: &IMFTransform,
) -> Result<(), WinVideoError> {
    // SAFETY: `manager` 是有效 COM 接口；`MFT_MESSAGE_SET_D3D_MANAGER` 的
    // ulParam 约定为设备管理器的裸指针。管理器由调用方持有，生命周期覆盖 MFT。
    unsafe {
        transform.ProcessMessage(
            MFT_MESSAGE_SET_D3D_MANAGER,
            encode_device.manager.as_raw() as usize,
        )
    }
    .map_err(|err| WinVideoError::Encode(format!("MFT_MESSAGE_SET_D3D_MANAGER 失败: {err}")))
}
