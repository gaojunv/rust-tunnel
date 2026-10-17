//! Windows 句柄的 RAII 包装（**仅 Windows 编译**）。
//!
//! 探针与服务控制会同时持有好几个内核对象（进程、令牌、作业、服务句柄、桌面、DC、
//! 位图）。裸 `HANDLE` 在这些路径上很容易漏关——一旦失败分支提前返回就漏了。
//! 统一收进本模块，让「谁创建谁关闭」由 `Drop` 兜住。
//!
//! # 为什么 `from_raw` 是安全函数
//!
//! 双关句柄是**资源**错误，不是 Rust 的内存安全错误（Win32 的对象是引用计数的，
//! 重复 `CloseHandle` 只会失败并置 last-error）。因此它是一个普通的契约，
//! 用文档说清而不是 `unsafe`。真正的内存安全问题（悬垂指针、别名）不在这一层。

use crate::WinResult;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};

/// 独占拥有的 Windows 句柄；`Drop` 时 `CloseHandle`。
#[derive(Debug)]
pub struct OwnedHandle(HANDLE);

impl OwnedHandle {
    /// 接管一个句柄。**所有权从调用方转移给本对象**：不要再对它调 `CloseHandle`，
    /// 也不要再造第二个 `OwnedHandle` 包同一个句柄。
    ///
    /// `NULL` 与 `INVALID_HANDLE_VALUE` 都被视为「空句柄」，`Drop` 时不会去关。
    #[must_use]
    pub fn from_raw(handle: HANDLE) -> Self {
        Self(handle)
    }

    /// 裸句柄，用于传给 Win32。
    #[must_use]
    pub fn raw(&self) -> HANDLE {
        self.0
    }

    /// 交出所有权，不做 `CloseHandle`（要把句柄交给别的拥有者时用）。
    #[must_use]
    pub fn into_raw(self) -> HANDLE {
        let raw = self.0;
        std::mem::forget(self);
        raw
    }

    /// 句柄是否为空（`NULL` 或 `INVALID_HANDLE_VALUE`）。
    #[must_use]
    pub fn is_null(&self) -> bool {
        is_null_handle(self.0)
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !is_null_handle(self.0) {
            // SAFETY: 句柄由本对象独占（见 `from_raw` 的契约），未在本对象之外关闭过。
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

/// 句柄是否为空：`NULL` 或 `INVALID_HANDLE_VALUE`。
#[must_use]
pub fn is_null_handle(handle: HANDLE) -> bool {
    handle.is_null() || handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE
}

/// `GetLastError()` 是否仍等于成功（`ERROR_SUCCESS`）。
///
/// 专为 `AdjustTokenPrivileges` 设计：**它在什么都没改的情况下也返回 `TRUE`**，
/// 唯一的判据是随后的 `GetLastError() == ERROR_SUCCESS`（否则是
/// `ERROR_NOT_ALL_ASSIGNED` = 1300）。同样的模式在 `Reg*` 系列以返回码直接给出。
///
/// 注意调用时机：本函数必须紧跟在那个 Win32 调用之后，中间不能插入别的 Win32 调用。
#[must_use]
pub fn last_error_is_success() -> bool {
    // SAFETY: 无参数、无副作用，只读当前线程的 last-error 槽。
    unsafe { windows_sys::Win32::Foundation::GetLastError() == 0 }
}

/// 校验一个 Win32 调用返回的句柄非空，否则带上 `GetLastError()` 报错。
///
/// # Errors
///
/// 句柄为空时返回带当前 `GetLastError()` 的 [`crate::WinError`]。
pub fn require_handle(api: &'static str, handle: HANDLE) -> WinResult<HANDLE> {
    if is_null_handle(handle) {
        Err(crate::error::last_error(api))
    } else {
        Ok(handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_and_invalid_are_both_treated_as_empty() {
        assert!(is_null_handle(std::ptr::null_mut()));
        assert!(is_null_handle(
            windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE
        ));
        // 一个「看起来像句柄」的非空值不算空。用 wrapping_add 造一个带 provenance
        // 的假句柄（不是 1usize as HANDLE 那种凭空造的指针），且**不**交给
        // OwnedHandle，避免真去关一个不存在的对象。
        let fake: HANDLE = std::ptr::null_mut::<std::ffi::c_void>().wrapping_add(1);
        assert!(!is_null_handle(fake));
    }

    #[test]
    fn owned_handle_does_not_close_an_empty_handle() {
        // 空句柄在 Drop 时必须被跳过：`CloseHandle(INVALID_HANDLE_VALUE)` 在某些
        // Windows 版本上会真的去关掉「当前进程」这个伪句柄，属于经典事故。
        drop(OwnedHandle::from_raw(std::ptr::null_mut()));
        drop(OwnedHandle::from_raw(
            windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE,
        ));
    }
}
