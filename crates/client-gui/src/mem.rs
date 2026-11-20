//! 进程内存遥测：让现场说清「那 200 MB 到底是谁」。
//!
//! 无人值守机器上同时存在三个可能被误认为「客户端本体」的进程：
//!
//! - `rust-tunnel-client-service.exe`（SYSTEM 服务，哑监督者）；
//! - `rust-tunnel-client-gui.exe --instance`（无头实例，**真正承载客户端身份**的那个）；
//! - 用户桌面的 `rust-tunnel-client-gui.exe`（GUI，服务在跑时它不注册）。
//!
//! 三者同名/同源，任务管理器里只看得到一个数字。把读数写进各自的 `instance.log`，
//! 现场就能按标签（`内存 …`）grep 出**本进程**的占用，再与任务管理器里的进程对号。
//!
//! 读数口径（与任务管理器列名对应）：
//!
//! - `working_set` —— 任务管理器「内存（活动的专用工作集）」附近的口径；
//! - `private_bytes` —— 提交大小里**私有**的那部分（`PROCESS_MEMORY_COUNTERS_EX::PrivateUsage`）；
//! - `peak_working_set` —— 启动峰值，用来判断「启动时高、随后回落」还是「一直高」；
//! - `commit` —— pagefile usage（提交总量，含共享页）；
//! - `threads` —— 线程数。它与启动占用最直接相关（tokio worker、WGC/MFT 探测、
//!   音频播放线程都会各占一份栈与初始化状态），所以启动后那一条必须带上它。

/// 一次进程内存采样（字节数均为 `usize`，与 Win32 返回值同宽）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemSnapshot {
    /// 工作集（working set）。
    pub working_set: usize,
    /// 私有提交字节数。
    pub private_bytes: usize,
    /// 峰值工作集。
    pub peak_working_set: usize,
    /// 提交总量（pagefile usage）。
    pub commit: usize,
    /// 本进程的线程数。
    pub threads: u32,
}

impl MemSnapshot {
    /// 中文标签的一行描述（日志与现场 grep 共用）。
    #[must_use]
    #[expect(
        clippy::cast_precision_loss,
        reason = "只用于日志展示，字节数转 f64 的精度损失对 MB 级读数无意义"
    )]
    pub fn describe_cn(self) -> String {
        let mb = |bytes: usize| bytes as f64 / (1024.0 * 1024.0);
        format!(
            "working set {:.1} MB · private {:.1} MB · 峰值 working set {:.1} MB · \
             commit {:.1} MB · {} 线程",
            mb(self.working_set),
            mb(self.private_bytes),
            mb(self.peak_working_set),
            mb(self.commit),
            self.threads
        )
    }
}

/// 采样本进程内存；平台不支持或调用失败时返回 `None`。
#[must_use]
pub fn snapshot() -> Option<MemSnapshot> {
    imp::snapshot()
}

/// 以 `内存 <tag>：…` 的固定形状记一条 info 日志。
///
/// 中文标签 + 固定前缀是刻意的：现场只需要 `findstr 内存 instance.log` 就能拿到
/// 全部读数，不必理解日志格式。
pub fn log(tag: &str) {
    if let Some(snap) = snapshot() {
        tracing::info!("内存 {tag}：{}", snap.describe_cn());
    } else {
        // 非 Windows 平台没有 `GetProcessMemoryInfo`；实例本身也只在 Windows 上存在，
        // 这里保持静默的 debug，避免单测/交叉编译时刷无意义的 info。
        tracing::debug!("内存 {tag}：本平台不可用");
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use super::MemSnapshot;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    pub(super) fn snapshot() -> Option<MemSnapshot> {
        // `PROCESS_MEMORY_COUNTERS_EX` 是 `PROCESS_MEMORY_COUNTERS` 的超集（多一个
        // `PrivateUsage`），psapi 按 `cb` 判断调用方给的是哪一种，所以这里如实传 EX 的
        // 尺寸并强转指针即可拿到私有提交字节。
        let mut counters = PROCESS_MEMORY_COUNTERS_EX {
            cb: u32::try_from(std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>()).unwrap_or(0),
            ..Default::default()
        };
        // SAFETY: 当前进程的伪句柄由 `GetCurrentProcess` 返回，不需要关闭；`counters` 是
        // 栈上有效、且 `cb` 已按结构体实际尺寸填好的出参。
        let ok = unsafe {
            GetProcessMemoryInfo(
                GetCurrentProcess(),
                std::ptr::addr_of_mut!(counters).cast(),
                counters.cb,
            )
        };
        if ok == 0 {
            return None;
        }
        Some(MemSnapshot {
            working_set: counters.WorkingSetSize,
            private_bytes: counters.PrivateUsage,
            peak_working_set: counters.PeakWorkingSetSize,
            commit: counters.PagefileUsage,
            threads: thread_count(),
        })
    }

    /// 用 Toolhelp 线程快照数本进程的线程数。
    ///
    /// 不走 `NtQueryInformationProcess` 读 PEB：那要跨结构体偏移，随 Windows 版本漂移；
    /// Toolhelp 是文档化的稳定接口，且一次快照的代价是微秒级（本函数只被每分钟一次
    /// 的遥测与启动/会话结束调用）。
    fn thread_count() -> u32 {
        // SAFETY: 只读快照，不涉及其它进程内存；句柄由本函数关闭。
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot.is_null() || snapshot == INVALID_HANDLE_VALUE {
            return 0;
        }
        let mut entry = THREADENTRY32 {
            dwSize: u32::try_from(std::mem::size_of::<THREADENTRY32>()).unwrap_or(0),
            ..Default::default()
        };
        let pid = std::process::id();
        let mut count = 0_u32;
        // SAFETY: 快照句柄有效，`entry` 是栈上有效出参且 `dwSize` 已填。
        let mut ok = unsafe { Thread32First(snapshot, &raw mut entry) };
        while ok != 0 {
            if entry.th32OwnerProcessID == pid {
                count = count.saturating_add(1);
            }
            // SAFETY: 同上；`Thread32Next` 在枚举结束后返回 0（此时 GetLastError 是
            // ERROR_NO_MORE_FILES），循环随之退出。
            ok = unsafe { Thread32Next(snapshot, &raw mut entry) };
        }
        // SAFETY: 快照句柄由上面成功创建，且只在此处关闭一次。
        unsafe {
            CloseHandle(snapshot);
        }
        count
    }
}

#[cfg(not(target_os = "windows"))]
mod imp {
    use super::MemSnapshot;

    pub(super) fn snapshot() -> Option<MemSnapshot> {
        None
    }
}
