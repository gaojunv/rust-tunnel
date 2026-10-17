//! Win32 调用失败的统一错误类型（可移植）。
//!
//! # 设计取舍
//!
//! **不引入 `thiserror`，也不调 `FormatMessageW`。** 系统错误文本直接由
//! `std::io::Error::from_raw_os_error(code)` 给出（标准库自带 Win32 错误码→英文文本的
//! 映射表），于是这个错误类型是**纯 std** 的，能在 macOS 上单测，也不必为
//! `FormatMessageW` 多开一个 `windows-sys` feature。
//!
//! 错误里**保留原始 `GetLastError()` 码**而不是当场转成字符串：探针报告需要把
//! 数字原样贴回来（`87` = `ERROR_INVALID_PARAMETER`、`5` = `ERROR_ACCESS_DENIED`、
//! `1314` = `ERROR_PRIVILEGE_NOT_HELD`、`183` = `ERROR_ALREADY_EXISTS`），
//! 这些数字本身就是诊断结论。

use std::fmt;

/// Win32 调用失败：失败的 API 名 + 原样的 `GetLastError()` 码。
///
/// `api` 是**诊断用**的静态字符串（如 `"OpenInputDesktop"`），不是本地化文案；
/// 文案在 [`WinError::message`] 里由系统错误码生成。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WinError {
    api: &'static str,
    code: u32,
}

impl WinError {
    /// 用 API 名与原始错误码构造。
    #[must_use]
    pub const fn new(api: &'static str, code: u32) -> Self {
        Self { api, code }
    }

    /// 失败的 API 名。
    #[must_use]
    pub const fn api(&self) -> &'static str {
        self.api
    }

    /// 原始的 `GetLastError()` 码（探针报告要原样贴出这个数字）。
    #[must_use]
    pub const fn code(&self) -> u32 {
        self.code
    }

    /// 系统给的错误文本。
    ///
    /// 码为 0 时说明调用方在**未真正失败**的路径上构造了错误（例如
    /// `SendInput` 返回值偏短但 `GetLastError` 被上一次调用留下的 0 覆盖），
    /// 这种情况在 UIPI 拦截时是**常态**，故单独给一句话而不是打印 "Success"。
    #[must_use]
    pub fn message(&self) -> String {
        if self.code == 0 {
            return "GetLastError() = 0（UIPI 拦截时是常态，见 SendInput 返回值）".to_owned();
        }
        i32::try_from(self.code).map_or_else(
            |_| format!("错误码 {} 超出 i32 范围，无系统文本", self.code),
            |code| std::io::Error::from_raw_os_error(code).to_string(),
        )
    }
}

impl fmt::Display for WinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} 失败（Win32 {}）：{}",
            self.api,
            self.code,
            self.message()
        )
    }
}

impl std::error::Error for WinError {}

/// 本 crate 的统一返回类型。
pub type WinResult<T> = Result<T, WinError>;

/// 读取当前线程的 `GetLastError()` 并包装成 [`WinError`]。
///
/// # 调用时机
///
/// 必须在**刚失败的调用之后立刻**调，中间不能插入任何可能覆盖 last-error 的 Win32 调用
/// （包括分配、日志、`tracing`）。因此本函数是 `#[inline]` 的裸转发，且**不做任何
/// 格式化工作**——真正的文本只在 [`WinError::message`] 里按需生成。
#[cfg(windows)]
#[must_use]
pub fn last_error(api: &'static str) -> WinError {
    // SAFETY: GetLastError 无参数、无副作用，只读当前线程的 last-error 槽。
    let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
    WinError::new(api, code)
}

/// 把「返回 `BOOL` 的 Win32 调用」的结果收敛成 [`WinResult`]。
///
/// # Errors
///
/// `ok == false` 时返回带当前 `GetLastError()` 的 [`WinError`]。
#[cfg(windows)]
pub fn check_bool(api: &'static str, ok: windows_sys::core::BOOL) -> WinResult<()> {
    if ok == 0 {
        Err(last_error(api))
    } else {
        Ok(())
    }
}

/// 把「返回句柄的 Win32 调用」的结果收敛成 [`WinResult`]。
///
/// Windows 的句柄 API 失败时返回 `NULL`（少数返回 `INVALID_HANDLE_VALUE`，那些调用点
/// 自己判断即可）。
///
/// # Errors
///
/// 句柄为 `NULL` 时返回带当前 `GetLastError()` 的 [`WinError`]。
#[cfg(windows)]
pub fn check_handle(
    api: &'static str,
    handle: windows_sys::Win32::Foundation::HANDLE,
) -> WinResult<windows_sys::Win32::Foundation::HANDLE> {
    if handle.is_null() {
        Err(last_error(api))
    } else {
        Ok(handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_carries_api_name_and_raw_code() {
        let err = WinError::new("OpenInputDesktop", 5);
        let text = err.to_string();
        assert!(text.contains("OpenInputDesktop"), "{text}");
        assert!(text.contains("Win32 5"), "{text}");
        // 系统文本随语言环境变化，只断言它不是空的、且不是那句「无法解析」兜底。
        assert!(!err.message().is_empty());
        assert_eq!(err.code(), 5);
        assert_eq!(err.api(), "OpenInputDesktop");
    }

    #[test]
    fn zero_code_is_explained_instead_of_printed_as_success() {
        // UIPI 拦截时 GetLastError 常常仍是 0，此时 std 的 from_raw_os_error(0)
        // 会给出 "Success"，那是误导性的，故单独给一句话。
        let text = WinError::new("SendInput", 0).message();
        assert!(text.contains("GetLastError() = 0"), "{text}");
        assert!(!text.contains("Success"), "{text}");
    }

    #[test]
    fn message_never_panics_on_absurd_code() {
        // 超过 i32 的错误码不会有系统文本，但也不能 panic。
        let text = WinError::new("SomeApi", u32::MAX).message();
        assert!(text.contains("超出 i32 范围"), "{text}");
    }

    #[test]
    fn error_is_a_std_error() {
        fn assert_std_error<E: std::error::Error + Send + Sync + 'static>() {}
        assert_std_error::<WinError>();
    }
}
