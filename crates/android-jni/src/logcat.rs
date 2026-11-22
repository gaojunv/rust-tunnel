//! `tracing` → Android logcat 的轻量 writer。
//!
//! 不用 `android_logger`（只为一个函数引入整条依赖不划算）：这里用 `MakeWriter` 把每条
//! 格式化好的日志整行交给 `__android_log_write`，标签固定 `rust-tunnel`。
//! 非 Android 目标（主机单测）回退到 `stderr`，保证 `install` 在两边都能调用。
//!
//! **只安装一次**：`nativeStart` 可能被调用多次（用户改配置重连），重复 `init` 会 panic，
//! 故用 [`std::sync::Once`] 兜住。级别取 [`ControllerConfig`](rust_tunnel_controller_core::ControllerConfig)
//! 的 `log_level`（缺省 `info`），非法值回退 `info`。

use std::io::{self, Write};
use std::sync::Once;

use tracing_subscriber::EnvFilter;

/// logcat 标签。
const LOG_TAG: &str = "rust-tunnel";

/// 单条日志的最大字节数：`__android_log_write` 对超长消息会截断，这里主动截断以避免
/// 在半行处产生无效 UTF-8。
const MAX_LINE_BYTES: usize = 4000;

static INSTALL: Once = Once::new();

/// 安装全局日志订阅者（重复调用无副作用）。
///
/// # Errors
///
/// 本函数不返回错误：订阅者安装失败（例如已被其他库占用）只写一行到 stderr/logcat，
/// 不影响宿主运行。
pub(crate) fn install(level: &str) {
    INSTALL.call_once(|| {
        let filter = EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("info"));
        let result = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(LogcatWriter)
            .try_init();
        if result.is_err() {
            // 已被其他订阅者占用：不覆盖，保留原样。
            eprintln!("rust-tunnel: tracing 订阅者已存在，跳过 logcat 安装");
        }
    });
}

/// `MakeWriter` 实现，每条事件一个 [`LogcatLine`]。
struct LogcatWriter;

/// 累积一行日志，drop/flush 时一次性交给 logcat。
struct LogcatLine {
    /// 尚未写出的字节。
    buf: Vec<u8>,
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogcatWriter {
    type Writer = LogcatLine;

    fn make_writer(&'a self) -> Self::Writer {
        LogcatLine { buf: Vec::new() }
    }
}

impl Write for LogcatLine {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        emit(&self.buf);
        self.buf.clear();
        Ok(())
    }
}

impl Drop for LogcatLine {
    fn drop(&mut self) {
        // `tracing` 的 fmt layer 写完一条记录后不保证显式 flush，故在 drop 里补一次。
        let _ = self.flush();
    }
}

/// 把一整行字节交给平台日志。
fn emit(line: &[u8]) {
    let line = &line[..line.len().min(MAX_LINE_BYTES)];
    // 去掉尾随换行，logcat 自己会分行。
    let line = match line.last() {
        Some(b'\n') => &line[..line.len() - 1],
        _ => line,
    };
    let text = String::from_utf8_lossy(line);

    #[cfg(target_os = "android")]
    {
        use std::ffi::{c_char, c_int, CString};

        // liblog 的优先级常量：3 = ANDROID_LOG_DEBUG，4 = INFO。
        const ANDROID_LOG_INFO: c_int = 4;
        unsafe extern "C" {
            fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
        }

        // 去掉内嵌 NUL（CString 不接受），否则整行会被丢弃。
        let sanitized: String = text
            .chars()
            .map(|c| if c == '\0' { ' ' } else { c })
            .collect();
        let Ok(tag) = CString::new(LOG_TAG) else {
            return;
        };
        let Ok(message) = CString::new(sanitized) else {
            return;
        };
        // SAFETY: 两个 CString 在本调用期间都存活，指针非空且以 NUL 结尾。
        unsafe {
            __android_log_write(ANDROID_LOG_INFO, tag.as_ptr(), message.as_ptr());
        }
    }

    #[cfg(not(target_os = "android"))]
    {
        eprintln!("[{LOG_TAG}] {text}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writer_accumulates_and_flushes() {
        let mut line = LogcatLine { buf: Vec::new() };
        line.write_all(b"hello ").expect("写入");
        line.write_all(b"world\n").expect("写入");
        assert_eq!(line.buf, b"hello world\n");
        line.flush().expect("flush");
        assert!(line.buf.is_empty());
    }

    #[test]
    fn empty_line_is_noop() {
        let mut line = LogcatLine { buf: Vec::new() };
        line.flush().expect("空行 flush 不报错");
    }

    #[test]
    fn install_is_idempotent() {
        // 安装两次不应 panic。
        install("info");
        install("debug");
    }
}
