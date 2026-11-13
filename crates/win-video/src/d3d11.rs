//! 最小 D3D11 设备管理：枚举适配器、在指定厂商的适配器上建设备。
//!
//! # 为什么必须选适配器
//!
//! 本机（以及大量台式机/笔记本）同时有 Intel 核显与 NVIDIA 独显。`D3D11CreateDevice`
//! 传 `None` 时拿到的默认适配器不一定是独显——NVENC 会话开在 Intel 设备上会直接
//! 失败（`NV_ENC_ERR_INVALID_DEVICE`），WGC 也可能把画面复制到一块与 NVENC 无关的
//! 设备上。所以 NVENC 路径必须显式选 `VendorId == 0x10DE` 的适配器。
//!
//! 采集路径则需要按「显示器挂在哪个适配器」来选：WGC 的 frame pool 建在哪个设备上，
//! 它就把合成结果复制到哪个设备。跨适配器复制由 DWM 完成（可用但更贵），因此探针
//! 会同时打印适配器与输出的对应关系。

use crate::WinVideoError;
use windows::core::Interface as _;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_FLAG, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIAdapter1, IDXGIFactory1, DXGI_ADAPTER_DESC1,
};

/// NVIDIA 的 PCI 厂商 ID。
pub const VENDOR_NVIDIA: u32 = 0x10DE;
/// Intel 的 PCI 厂商 ID。
pub const VENDOR_INTEL: u32 = 0x8086;
/// AMD 的 PCI 厂商 ID。
pub const VENDOR_AMD: u32 = 0x1002;

/// 一块 DXGI 适配器的摘要信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterInfo {
    /// DXGI 枚举序号（与 `IDXGIFactory1::EnumAdapters1` 的顺序一致）。
    pub index: u32,
    /// 适配器名称。
    pub name: String,
    /// PCI 厂商 ID。
    pub vendor_id: u32,
    /// PCI 设备 ID。
    pub device_id: u32,
    /// 是否软件适配器（WARP）。
    pub software: bool,
    /// 适配器 LUID（跨进程/跨 API 关联用）。
    pub luid: u64,
    /// 显存字节数（`DedicatedVideoMemory`）。
    pub dedicated_video_memory: usize,
}

impl AdapterInfo {
    /// 是否 NVIDIA 独显。
    #[must_use]
    pub fn is_nvidia(&self) -> bool {
        self.vendor_id == VENDOR_NVIDIA && !self.software
    }
}

/// 解码 UTF-16 设备描述（去掉结尾的 NUL）。
fn wide_to_string(wide: &[u16]) -> String {
    let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    String::from_utf16_lossy(&wide[..end])
}

fn adapter_info(index: u32, adapter: &IDXGIAdapter1) -> Result<AdapterInfo, WinVideoError> {
    // SAFETY: `adapter` 是本函数刚枚举出的有效 COM 指针。
    let desc: DXGI_ADAPTER_DESC1 = unsafe { adapter.GetDesc1() }?;
    Ok(AdapterInfo {
        index,
        name: wide_to_string(&desc.Description),
        vendor_id: desc.VendorId,
        device_id: desc.DeviceId,
        // `DXGI_ADAPTER_FLAG_SOFTWARE` = 0x2；windows 0.62 把 Flags 暴露为 u32。
        software: desc.Flags & 0x2 != 0,
        // LUID 在 DXGI_ADAPTER_DESC1 里是 `HighPart << 32 | LowPart`；
        // struct 字段名与 windows crate 版本相关，这里用 GetDesc1 后再拼。
        luid: (u64::from(desc.AdapterLuid.HighPart.cast_unsigned()) << 32)
            | u64::from(desc.AdapterLuid.LowPart),
        dedicated_video_memory: desc.DedicatedVideoMemory,
    })
}

/// 枚举全部 DXGI 适配器（含软件适配器，调用方自行过滤）。
///
/// # Errors
/// 创建 DXGI factory 失败时返回 [`WinVideoError::Windows`]。
pub fn adapters() -> Result<Vec<AdapterInfo>, WinVideoError> {
    // SAFETY: 无参数、返回新建的 COM 对象。
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }?;
    let mut out = Vec::new();
    for index in 0.. {
        // SAFETY: 枚举到末尾时 DXGI 返回 `DXGI_ERROR_NOT_FOUND`，属于正常的
        // 终止条件，不是错误。
        match unsafe { factory.EnumAdapters1(index) } {
            Ok(adapter) => out.push(adapter_info(index, &adapter)?),
            Err(_) => break,
        }
    }
    Ok(out)
}

/// 一个已创建的 D3D11 设备与它的立即上下文。
pub struct D3d11 {
    /// 设备。
    pub device: ID3D11Device,
    /// 立即上下文。
    pub context: ID3D11DeviceContext,
    /// 设备所在适配器。
    pub adapter: AdapterInfo,
}

impl std::fmt::Debug for D3d11 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("D3d11")
            .field("adapter", &self.adapter)
            .finish_non_exhaustive()
    }
}

/// 在指定适配器上创建 D3D11 设备。
///
/// 传 `adapter_index == None` 时选**第一块非软件适配器**；需要 NVENC 时应显式传
/// NVIDIA 适配器的序号（见 [`adapters`]）。
///
/// 设备同时打开 `BGRA_SUPPORT`（WGC 的帧纹理是 `B8G8R8A8_UNORM`）与
/// `VIDEO_SUPPORT`（D3D11 VideoProcessor / DXVA 解码需要）。
///
/// # Errors
/// 适配器不存在或 `D3D11CreateDevice` 失败时返回 [`WinVideoError::Windows`]。
pub fn create(adapter_index: Option<u32>) -> Result<D3d11, WinVideoError> {
    let all = adapters()?;
    let info = match adapter_index {
        Some(index) => all
            .iter()
            .find(|a| a.index == index)
            .cloned()
            .ok_or_else(|| WinVideoError::Config(format!("适配器序号 {index} 不存在")))?,
        None => all
            .iter()
            .find(|a| !a.software)
            .cloned()
            .ok_or_else(|| WinVideoError::Config("没有可用的硬件适配器".to_owned()))?,
    };

    // 重新枚举一次拿到对应的 COM 适配器指针。
    // SAFETY: 与 `adapters` 相同的枚举路径。
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }?;
    let adapter: IDXGIAdapter1 = unsafe { factory.EnumAdapters1(info.index) }?;
    // `D3D11CreateDevice` 要基接口 `IDXGIAdapter`，不是 `IDXGIAdapter1`。
    let adapter: IDXGIAdapter = adapter.cast()?;

    let levels = [D3D_FEATURE_LEVEL_11_0];
    let flags: D3D11_CREATE_DEVICE_FLAG =
        D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    let mut feature_level: D3D_FEATURE_LEVEL = D3D_FEATURE_LEVEL_11_0;

    // SAFETY: 传入的 adapter 在调用期间有效；out 指针指向栈上 Option。
    unsafe {
        D3D11CreateDevice(
            Some(&adapter),
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            flags,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&raw mut device),
            Some(&raw mut feature_level),
            Some(&raw mut context),
        )?;
    }

    let device =
        device.ok_or_else(|| WinVideoError::Config("D3D11CreateDevice 返回空设备".to_owned()))?;
    let context = context
        .ok_or_else(|| WinVideoError::Config("D3D11CreateDevice 返回空上下文".to_owned()))?;

    tracing::debug!(
        adapter = %info.name,
        feature_level = ?feature_level,
        "D3D11 设备已创建"
    );
    Ok(D3d11 {
        device,
        context,
        adapter: info,
    })
}

/// 选 NVIDIA 适配器并建设备（NVENC 专用）。
///
/// # Errors
/// 没有 NVIDIA 适配器或设备创建失败时返回 [`WinVideoError`]。
pub fn create_nvidia() -> Result<D3d11, WinVideoError> {
    let all = adapters()?;
    let nvidia = all
        .iter()
        .find(|a| a.is_nvidia())
        .ok_or_else(|| WinVideoError::Nvenc("未找到 NVIDIA 适配器".to_owned()))?;
    create(Some(nvidia.index))
}
