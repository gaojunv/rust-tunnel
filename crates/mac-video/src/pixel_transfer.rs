//! GPU 像素搬运：`VTPixelTransferSession` 的薄封装 + 目标 `CVPixelBuffer` 工厂。
//!
//! # 为什么需要它
//!
//! ScreenCaptureKit 交给我们的是**采集尺寸**的 `32BGRA` 帧（4K 一帧 33 MB），而编码
//! 会话按**编码尺寸**（默认宽 1920）配置。把 4K 帧留在 CPU 上缩放再上传，等于把整帧
//! 抄两遍；`VTPixelTransferSession` 可以在 GPU 上一步完成「缩放 + BGRA→NV12（`420v`）」，
//! 编码器直接吃转换后的 `CVPixelBuffer`，全程不落 CPU。
//!
//! - [`create_pixel_buffer`]：按编码尺寸建一个可复用的目标缓冲（`IOSurface` 背书，
//!   硬件编码器可直接用它，不需要再拷贝）；
//! - [`PixelTransfer::transfer`]：源缓冲 → 目标缓冲，**默认就是「整帧缩放到目标整帧」**；
//! - 颜色口径固定 **BT.709 limited**：目标格式用 `kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange`
//!   （`420v` 本身就是 video range），转换会话的 `Destination*` 三个属性也钉在 BT.709，
//!   与控制端 wgpu 着色器、Windows NV12 路径一致。**不要**改成 `420f`/full range：两端
//!   口径不一致的表现是「黑色发灰 / 白色过曝」，只在真机上看得到。
//!
//! # 线程
//!
//! `VTPixelTransferSession` 与 `CVPixelBuffer` 一样是引用计数的 CF 对象，**不是**
//! `Send`/`Sync`。需要跨线程持有它们的类型各自用 `unsafe impl Send` 包裹并说明理由
//! （本 crate 的 `VtEncoder`、`mac-capture` 的变化检测缩放器）。
//!
//! # 调用成本
//!
//! 一次 4K→1080p 的 BGRA→420v 转换走硬件视频处理器，实测亚毫秒到数毫秒量级（取决于
//! 机型与是否跨 GPU/CPU 边界）。会话创建一次、目标缓冲复用，**不要**每帧新建。

use crate::error::{MacVideoError, MacVideoResult};
use objc2_core_foundation::{
    kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFBoolean, CFDictionary,
    CFNumber, CFRetained, CFString, CFType,
};
use objc2_core_video::{
    kCVImageBufferColorPrimariesKey, kCVImageBufferColorPrimaries_ITU_R_709_2,
    kCVImageBufferTransferFunctionKey, kCVImageBufferTransferFunction_ITU_R_709_2,
    kCVImageBufferYCbCrMatrixKey, kCVImageBufferYCbCrMatrix_ITU_R_709_2, kCVPixelBufferHeightKey,
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
    kCVPixelFormatType_32BGRA, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, CVPixelBuffer,
    CVPixelBufferCreate,
};
use objc2_video_toolbox::{
    kVTDownsamplingMode_Average, kVTPixelTransferPropertyKey_DestinationColorPrimaries,
    kVTPixelTransferPropertyKey_DestinationTransferFunction,
    kVTPixelTransferPropertyKey_DestinationYCbCrMatrix,
    kVTPixelTransferPropertyKey_DownsamplingMode, kVTPixelTransferPropertyKey_RealTime,
    kVTPixelTransferPropertyKey_ScalingMode, kVTScalingMode_Normal, VTPixelTransferSession,
};
use std::ptr::{self, NonNull};
use tracing::debug;

/// 编码器可直接接受的输入格式（其余格式必须先过 [`PixelTransfer`]）。
///
/// VideoToolbox 的压缩会话接受 `32BGRA`（自己转 4:2:0）与 `420v`（已经是 4:2:0）。
/// 判断它只是为了避免**不必要的**转换：同尺寸 + 这两种格式时零拷贝直通。
#[must_use]
pub fn is_encoder_input_format(format: u32) -> bool {
    format == kCVPixelFormatType_32BGRA || format == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
}

/// 创建一块可复用的 `CVPixelBuffer`。
///
/// `format` 用 CoreVideo 的像素格式码（例如 [`kCVPixelFormatType_32BGRA`] 与
/// [`kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange`]）。`color_709` 为真时附带
/// BT.709 limited 的颜色三件套（`420v` 目标缓冲必须为真；BGRA 源/暂存缓冲可省略）。
///
/// 缓冲**带 `IOSurface` 属性**：硬件编码器与 `VTPixelTransferSession` 都能直接用它，
/// 否则 VT 可能退回「先拷到自己的 surface 再处理」。
///
/// # Errors
/// 尺寸为零、超出 `i32`、像素格式为 0、`CVPixelBufferCreate` 返回错误时返回
/// [`MacVideoError`]。
pub fn create_pixel_buffer(
    width: u32,
    height: u32,
    format: u32,
    color_709: bool,
) -> MacVideoResult<CFRetained<CVPixelBuffer>> {
    if width == 0 || height == 0 {
        return Err(MacVideoError::Config(format!(
            "像素缓冲尺寸不能为 0（{width}x{height}）"
        )));
    }
    let attributes = pixel_buffer_attributes(width, height, format, color_709)?;
    let mut buffer_ptr: *mut CVPixelBuffer = ptr::null_mut();
    let status = unsafe {
        CVPixelBufferCreate(
            None,
            usize::try_from(width).map_err(|_| MacVideoError::Config("缓冲宽度溢出".to_owned()))?,
            usize::try_from(height)
                .map_err(|_| MacVideoError::Config("缓冲高度溢出".to_owned()))?,
            format,
            Some(&attributes),
            NonNull::from(&mut buffer_ptr),
        )
    };
    if status != 0 {
        return Err(MacVideoError::os_status("CVPixelBufferCreate", status));
    }
    let buffer =
        unsafe {
            CFRetained::from_raw(NonNull::new(buffer_ptr).ok_or_else(|| {
                MacVideoError::Encode("CVPixelBufferCreate 返回空缓冲".to_owned())
            })?)
        };
    Ok(buffer)
}

/// 一次「缩放 + 格式/颜色转换」的 GPU 会话。
///
/// 会话创建与目标缓冲分配都是重操作，**每个采集/编码端持有并复用一份**。
pub struct PixelTransfer {
    /// 转换会话（`Drop` 里 invalidate 后再释放）。
    session: CFRetained<VTPixelTransferSession>,
}

impl std::fmt::Debug for PixelTransfer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PixelTransfer").finish_non_exhaustive()
    }
}

impl Drop for PixelTransfer {
    fn drop(&mut self) {
        // SAFETY: `session` 有效；invalidate 之后 VT 不再使用它，随后 `CFRetained` 析构
        // 释放引用。invalidate 是幂等的。
        unsafe { self.session.invalidate() };
    }
}

impl PixelTransfer {
    /// 创建一个转换会话并配置为「缩放 + BT.709 目标 + 实时优先」。
    ///
    /// # Errors
    /// `VTPixelTransferSessionCreate` 失败时返回 [`MacVideoError`]。
    pub fn new() -> MacVideoResult<Self> {
        let mut session_ptr: *mut VTPixelTransferSession = ptr::null_mut();
        let status =
            unsafe { VTPixelTransferSession::create(None, NonNull::from(&mut session_ptr)) };
        if status != 0 {
            return Err(MacVideoError::os_status(
                "VTPixelTransferSessionCreate",
                status,
            ));
        }
        let session = unsafe {
            CFRetained::from_raw(NonNull::new(session_ptr).ok_or_else(|| {
                MacVideoError::Encode("VTPixelTransferSessionCreate 返回空会话".to_owned())
            })?)
        };
        let transfer = Self { session };
        transfer.configure();
        Ok(transfer)
    }

    /// 把 `source` **整帧缩放**并转换进 `destination`。
    ///
    /// 目标缓冲由调用方按编码尺寸预建并复用；本方法只做搬运，不分配。
    ///
    /// # Errors
    /// `VTPixelTransferSessionTransferImage` 返回非 `noErr`（常见的是
    /// `kVTPixelTransferNotSupportedErr`：某个体素格式组合不被支持）时返回错误。
    pub fn transfer(
        &self,
        source: &CVPixelBuffer,
        destination: &CVPixelBuffer,
    ) -> MacVideoResult<()> {
        let status = unsafe { self.session.transfer_image(source, destination) };
        if status != 0 {
            return Err(MacVideoError::os_status(
                "VTPixelTransferSessionTransferImage",
                status,
            ));
        }
        Ok(())
    }

    /// 配置会话属性；**全部容错**——某些属性在旧系统/某些机型上不被支持，缺失它们只
    /// 意味着「用默认值」，格式/尺寸转换本身不受影响。
    fn configure(&self) {
        // 默认就是 Normal（整帧缩放到整帧），显式写出来是为了不依赖默认值。
        let scaling: &CFType = unsafe { kVTScalingMode_Normal };
        self.set(unsafe { kVTPixelTransferPropertyKey_ScalingMode }, scaling);
        let real_time: &CFType = CFBoolean::new(true);
        self.set(unsafe { kVTPixelTransferPropertyKey_RealTime }, real_time);
        // 下采样用区域平均（而非抽点）：桌面文字缩小后靠它保持可读，与 CPU 侧
        // `scale_bgra_box` 的盒式均值同口径。
        let downsample: &CFType = unsafe { kVTDownsamplingMode_Average };
        self.set(
            unsafe { kVTPixelTransferPropertyKey_DownsamplingMode },
            downsample,
        );
        // 目标颜色口径：BT.709 limited（420v 的色度本来就是 limited）。
        let primaries: &CFType = unsafe { kCVImageBufferColorPrimaries_ITU_R_709_2 };
        let transfer: &CFType = unsafe { kCVImageBufferTransferFunction_ITU_R_709_2 };
        let matrix: &CFType = unsafe { kCVImageBufferYCbCrMatrix_ITU_R_709_2 };
        self.set(
            unsafe { kVTPixelTransferPropertyKey_DestinationColorPrimaries },
            primaries,
        );
        self.set(
            unsafe { kVTPixelTransferPropertyKey_DestinationTransferFunction },
            transfer,
        );
        self.set(
            unsafe { kVTPixelTransferPropertyKey_DestinationYCbCrMatrix },
            matrix,
        );
    }

    /// 写一个会话属性（失败只记 debug）。
    fn set(&self, key: &CFString, value: &CFType) {
        let session: &CFType = &self.session;
        // SAFETY: 会话有效；key/value 都是 VideoToolbox 文档定义的静态常量或本函数
        // 构造的 CF 值。属性不被支持时只返回错误码，不会破坏会话。
        let status =
            unsafe { objc2_video_toolbox::VTSessionSetProperty(session, key, Some(value)) };
        if status != 0 {
            debug!(status, "像素转换会话不接受某个属性，沿用默认值");
        }
    }
}

/// 组装像素缓冲属性（格式/尺寸 + IOSurface + 可选 BT.709 颜色）。
fn pixel_buffer_attributes(
    width: u32,
    height: u32,
    format: u32,
    color_709: bool,
) -> MacVideoResult<CFRetained<CFDictionary>> {
    if format == 0 {
        return Err(MacVideoError::Config("像素格式码不能为 0".to_owned()));
    }
    let format_value = CFNumber::new_i32(
        i32::try_from(format)
            .map_err(|_| MacVideoError::Config("像素格式码超出 i32".to_owned()))?,
    );
    let width_value = CFNumber::new_i32(
        i32::try_from(width).map_err(|_| MacVideoError::Config("宽度超出 i32".to_owned()))?,
    );
    let height_value = CFNumber::new_i32(
        i32::try_from(height).map_err(|_| MacVideoError::Config("高度超出 i32".to_owned()))?,
    );
    let surface_properties = cf_dictionary(&[])?;

    let format_value: &CFType = &format_value;
    let width_value: &CFType = &width_value;
    let height_value: &CFType = &height_value;
    let surface_properties: &CFType = &surface_properties;
    let mut pairs: Vec<(&CFString, &CFType)> = vec![
        (unsafe { kCVPixelBufferPixelFormatTypeKey }, format_value),
        (unsafe { kCVPixelBufferWidthKey }, width_value),
        (unsafe { kCVPixelBufferHeightKey }, height_value),
        (
            unsafe { kCVPixelBufferIOSurfacePropertiesKey },
            surface_properties,
        ),
    ];
    if color_709 {
        let primaries: &CFType = unsafe { kCVImageBufferColorPrimaries_ITU_R_709_2 };
        let transfer: &CFType = unsafe { kCVImageBufferTransferFunction_ITU_R_709_2 };
        let matrix: &CFType = unsafe { kCVImageBufferYCbCrMatrix_ITU_R_709_2 };
        pairs.push((unsafe { kCVImageBufferColorPrimariesKey }, primaries));
        pairs.push((unsafe { kCVImageBufferTransferFunctionKey }, transfer));
        pairs.push((unsafe { kCVImageBufferYCbCrMatrixKey }, matrix));
    }
    cf_dictionary(&pairs)
}

/// 用 `CFType` 键值对建一个不可变字典（调用方保证键值数量一致）。
fn cf_dictionary(pairs: &[(&CFString, &CFType)]) -> MacVideoResult<CFRetained<CFDictionary>> {
    let count = isize::try_from(pairs.len())
        .map_err(|_| MacVideoError::Config("字典条目过多".to_owned()))?;
    let mut keys: Vec<*const std::ffi::c_void> = pairs
        .iter()
        .map(|(key, _)| {
            let key: *const CFString = *key;
            key.cast::<std::ffi::c_void>()
        })
        .collect();
    let mut values: Vec<*const std::ffi::c_void> = pairs
        .iter()
        .map(|(_, value)| {
            let value: *const CFType = *value;
            value.cast::<std::ffi::c_void>()
        })
        .collect();
    // SAFETY: keys/values 各含 `count` 个有效指针，生命周期覆盖本次调用；类型回调是
    // CoreFoundation 的标准 CF 回调，字典会自己 retain 每个值。
    let dictionary = unsafe {
        CFDictionary::new(
            None,
            keys.as_mut_ptr(),
            values.as_mut_ptr(),
            count,
            &raw const kCFTypeDictionaryKeyCallBacks,
            &raw const kCFTypeDictionaryValueCallBacks,
        )
    };
    dictionary.ok_or_else(|| MacVideoError::Encode("CFDictionaryCreate 失败".to_owned()))
}
