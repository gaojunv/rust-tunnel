//! 桌面（desktop）识别与切换：`WinSta0\Default` vs `WinSta0\Winlogon`。
//!
//! # 这一层为什么是整个特性的关键
//!
//! 「锁屏点不动」的本质是：锁屏时**输入桌面**换成了 `Winlogon`，而普通进程的线程
//! 仍挂在 `Default` 上。`SendInput` 只作用在**调用线程所在的桌面**，所以往 `Default`
//! 上发多少事件都到不了锁屏界面。要修就两步：
//!
//! 1. `OpenInputDesktop` 拿到当前输入桌面的句柄；
//! 2. 在**同一个线程**里 `SetThreadDesktop` 挂上去，然后采集与注入都在这条线程上做。
//!
//! 采集与注入必须在**同一条线程**上：`SetThreadDesktop` 是**线程级**状态，
//! 一条线程挂到 Winlogon 而另一条还挂在 Default 上，就会出现「拍到的是锁屏、
//! 点的是桌面」（或反过来）。桌面名一变（锁屏↔解锁↔切换用户）就**销毁整条线程重开**：
//! `SetThreadDesktop` 要求线程不拥有任何窗口，且**不能**在有窗口/钩子之后再反复重设，
//! 复用线程会偶发失败且极难排查。
//!
//! # 可移植部分
//!
//! [`DesktopKind`] 只是名字分类，纯逻辑，在 macOS 上有单测（探针报告就是靠它
//! 一眼看出「当时在哪个桌面」，这决定了后面所有结论）。

/// 输入桌面的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopKind {
    /// `Default`：普通用户桌面（登录后、未锁屏）。
    Default,
    /// `Winlogon`：**锁屏与登录界面**都在这里（这也是本特性唯一必须拿下的桌面）。
    Winlogon,
    /// `Screen-saver`：屏保把输入抢过去时的桌面。
    ScreenSaver,
    /// `Disconnect`：会话被断开（如 RDP 掉线）时显示的桌面。
    Disconnect,
    /// 其它（含空名或未识别名）。
    Other,
}

impl DesktopKind {
    /// 按桌面名分类（**大小写不敏感**，两端空白容忍）。
    ///
    /// 名字来自 `GetUserObjectInformationW(UOI_NAME)`，只带桌面名、不带
    /// `WinSta0\` 前缀。
    #[must_use]
    pub fn from_name(name: &str) -> Self {
        let name = name.trim();
        if name.eq_ignore_ascii_case("Default") {
            Self::Default
        } else if name.eq_ignore_ascii_case("Winlogon") {
            Self::Winlogon
        } else if name.eq_ignore_ascii_case("Screen-saver")
            || name.eq_ignore_ascii_case("Screensaver")
        {
            Self::ScreenSaver
        } else if name.eq_ignore_ascii_case("Disconnect") {
            Self::Disconnect
        } else {
            Self::Other
        }
    }

    /// 稳定名（写进探针报告与日志，别用 `Debug` 输出）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "Default",
            Self::Winlogon => "Winlogon",
            Self::ScreenSaver => "Screen-saver",
            Self::Disconnect => "Disconnect",
            Self::Other => "Other",
        }
    }

    /// 是否是**安全桌面**：只有 SYSTEM 完整性级别的进程能挂上去。
    ///
    /// 探针用它决定「要不要做注入实验」——在安全桌面上注入才有诊断价值。
    #[must_use]
    pub const fn is_secure(self) -> bool {
        matches!(self, Self::Winlogon | Self::ScreenSaver)
    }

    /// 是否就是锁屏/登录界面所在的那个桌面。
    #[must_use]
    pub const fn is_winlogon(self) -> bool {
        matches!(self, Self::Winlogon)
    }
}

/// 当前**输入桌面**的句柄与识别结果。
///
/// 生命周期：`open()` 拿到句柄 → `attach_to_current_thread()` 把**当前线程**挂上去
/// → 采集/注入 → 线程结束。句柄在 `Drop` 时按规则关闭（见 `Drop` 实现）。
#[cfg(windows)]
#[derive(Debug)]
pub struct InputDesktop {
    handle: windows_sys::Win32::System::StationsAndDesktops::HDESK,
    name: String,
    kind: DesktopKind,
    attached: bool,
}

#[cfg(windows)]
impl InputDesktop {
    /// 打开当前输入桌面（`OpenInputDesktop`）。
    ///
    /// 申请的权限是 `DESKTOP_READOBJECTS | DESKTOP_WRITEOBJECTS | DESKTOP_SWITCHDESKTOP
    /// | DESKTOP_ENUMERATE`：`SetThreadDesktop` 需要切换权，读取名字需要读对象权。
    /// **不要**改成 `GENERIC_ALL`——在非 SYSTEM 的调用者上它会失败，而我们希望
    /// 失败信息尽可能精确。
    ///
    /// # Errors
    ///
    /// 句柄为空时返回 [`crate::WinError`]。**注意**：非 SYSTEM 进程在锁屏时这里会
    /// 返回 `ERROR_ACCESS_DENIED`(5)，那正是「必须有 SYSTEM 服务」的实证，探针要把
    /// 这个码原样记进报告。
    pub fn open() -> crate::WinResult<Self> {
        use windows_sys::Win32::System::StationsAndDesktops::{
            OpenInputDesktop, DESKTOP_ENUMERATE, DESKTOP_READOBJECTS, DESKTOP_SWITCHDESKTOP,
            DESKTOP_WRITEOBJECTS,
        };

        let access =
            DESKTOP_READOBJECTS | DESKTOP_WRITEOBJECTS | DESKTOP_SWITCHDESKTOP | DESKTOP_ENUMERATE;
        // SAFETY: 三个参数都是标量语义：dwFlags 必须为 0，fInherit = FALSE（不继承），
        // dwDesiredAccess 是上面那几个位的或。无指针参数。
        let handle = unsafe { OpenInputDesktop(0, 0, access) };
        let handle = crate::handle::require_handle("OpenInputDesktop", handle)?;
        let name = window_object_name(handle).unwrap_or_default();
        let kind = DesktopKind::from_name(&name);
        Ok(Self {
            handle,
            name,
            kind,
            attached: false,
        })
    }

    /// 桌面名（如 `Default` / `Winlogon`）。读名字失败时是空串。
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 分类结果。
    #[must_use]
    pub const fn kind(&self) -> DesktopKind {
        self.kind
    }

    /// 是否已经挂到当前线程上。
    #[must_use]
    pub const fn is_attached(&self) -> bool {
        self.attached
    }

    /// 把**当前线程**切换到本桌面（`SetThreadDesktop`）。
    ///
    /// # 调用约定（三条硬约束）
    ///
    /// 1. 必须在**专用线程**上调用，且是那条线程的**第一批动作**（线程不得拥有任何
    ///    窗口、不得挂任何钩子，否则失败 `ERROR_INVALID_PARAMETER`(87) / `ERROR_BUSY`(170)）。
    /// 2. **绝不要**在 `spawn_blocking`/线程池线程上调用：池线程会被复用，
    ///    而 `SetThreadDesktop` 在挂过一次之后不能再挂到别的桌面。
    /// 3. 挂上之后**不要**再调 `SetThreadDesktop` 换回来复用线程——要换桌面就
    ///    **销毁整条线程重开**。
    ///
    /// # Errors
    ///
    /// `SetThreadDesktop` 返回 0 时返回 [`crate::WinError`]（探针要把返回码原样记录：
    /// 这是「SYSTEM 令牌 + 目标会话」到底成没成的直接证据）。
    pub fn attach_to_current_thread(&mut self) -> crate::WinResult<()> {
        // SAFETY: handle 由 OpenInputDesktop 返回且在本对象存活期间有效；
        // SetThreadDesktop 只影响调用线程，不改动桌面对象本身。
        let ok = unsafe {
            windows_sys::Win32::System::StationsAndDesktops::SetThreadDesktop(self.handle)
        };
        crate::error::check_bool("SetThreadDesktop", ok)?;
        self.attached = true;
        Ok(())
    }

    /// 关闭句柄，**即使它曾 [`Self::attach_to_current_thread`] 成功**。
    ///
    /// # 调用约定（与 `Drop` 的规则配套）
    ///
    /// 只能在**该线程即将退出、且不再使用这个桌面**时调用。`CloseDesktop` 对「仍被本线程
    /// 使用」的桌面会失败（常见是 170 `ERROR_BUSY`）——那只是一次**尽力回收**，失败不报错，
    /// 句柄最终随进程退出交还系统。调用方若想彻底回收，需要先让线程不再使用该桌面（退出，
    /// 或由**另一条**线程在它退出后关——但那要求句柄能跨线程传递，见下）。
    ///
    /// # 为什么需要它
    ///
    /// `Drop` 对已挂载的句柄**刻意不关**（见其注释），那是为「一次性探针进程」设计的。
    /// 阶段 1 的实例会**按桌面变化销毁并重建**专用线程，若不在退出前回收，每次锁屏/解锁都会
    /// 留下一个永不回收的 `HDESK`。这与 `Drop` 注释里写的「阶段 1 的专用线程在退出前自行
    /// 关闭」正是同一件事。
    ///
    /// **注意**：`InputDesktop` 目前**不是 `Send`**（`HDESK` 在当前 `windows-sys` 里是
    /// `*mut c_void`），所以句柄**无法**移交另一条线程去关。想做成「退出后由监管线程关」
    /// 需要给本类型加 `unsafe impl Send`（Win32 句柄是进程级的，线程级的只是「挂载」），
    /// 或另开一个关原始句柄的入口。
    pub fn close_on_exit(&mut self) {
        if self.handle.is_null() {
            return;
        }
        // SAFETY: handle 由 OpenInputDesktop 返回、本对象独占且已确认非空；
        // CloseDesktop 只影响本对象持有的这个引用，不动桌面对象本身。
        unsafe {
            windows_sys::Win32::System::StationsAndDesktops::CloseDesktop(self.handle);
        }
        self.handle = std::ptr::null_mut();
        // 标记为「未挂载」，让 Drop 不再重复关闭（句柄已置空，Drop 也会自行跳过）。
        self.attached = false;
    }
}

#[cfg(windows)]
impl Drop for InputDesktop {
    fn drop(&mut self) {
        // 已经挂在线程上的桌面**不能**在这里关：`SetThreadDesktop` 成功的桌面句柄
        // 在该线程仍使用它时 `CloseDesktop` 会失败，而强行关掉会让线程的桌面悬空。
        // 阶段 0 的探针进程做完一次实验就退出，句柄随进程回收；阶段 1 的
        // 专用线程在**退出前**自行关闭（那时线程已经不再使用该桌面）。
        // `close_on_exit` 已把句柄置空并置 `attached = false`，这里必须一并跳过，
        // 否则会拿一个空句柄去 CloseDesktop。
        if self.attached || self.handle.is_null() {
            return;
        }
        // SAFETY: handle 非空（open() 里已校验）且未挂到任何线程上，本对象独占它。
        unsafe {
            windows_sys::Win32::System::StationsAndDesktops::CloseDesktop(self.handle);
        }
    }
}

/// 读一个窗口站/桌面对象的对象名（`GetUserObjectInformationW(UOI_NAME)`）。
///
/// 自己写而不是用 `InputDesktop::name` 的内部逻辑，是因为探针还要问
/// **当前线程**所在桌面的名字（`GetThreadDesktop`），两者共用这一段。
#[cfg(windows)]
fn window_object_name(handle: windows_sys::Win32::Foundation::HANDLE) -> crate::WinResult<String> {
    use windows_sys::Win32::System::StationsAndDesktops::{GetUserObjectInformationW, UOI_NAME};

    // 第一趟：问需要多少字节。缓冲区传 null 时按文档会失败并给出长度，
    // 因此这一趟**不看返回值**，只看 `needed`。
    let mut needed: u32 = 0;
    // SAFETY: pvInfo 传 null 表示「只要长度」；nLength = 0；needed 是本栈上的 u32。
    unsafe {
        GetUserObjectInformationW(handle, UOI_NAME, std::ptr::null_mut(), 0, &raw mut needed);
    }
    if needed == 0 {
        // 拿不到长度说明句柄不可读；退回一个固定上限，让第二趟自己报错。
        needed = 512;
    }

    let mut units: Vec<u16> = vec![0; usize::try_from(needed.div_ceil(2)).unwrap_or(256)];
    let bytes = u32::try_from(units.len() * 2).unwrap_or(u32::MAX);
    // SAFETY: units 是本栈上的 u16 缓冲，长度按字节数如实告知；needed 可空。
    let ok = unsafe {
        GetUserObjectInformationW(
            handle,
            UOI_NAME,
            units.as_mut_ptr().cast(),
            bytes,
            std::ptr::null_mut(),
        )
    };
    crate::error::check_bool("GetUserObjectInformationW(UOI_NAME)", ok)?;
    Ok(crate::wide::from_wide(&units))
}

/// **当前线程**所在桌面的名字（`GetThreadDesktop` + `UOI_NAME`）。
///
/// 与 [`InputDesktop::open`] 的名字对照着看，就能一眼判断「线程是不是还挂在
/// `Default` 上而输入已经跑到 `Winlogon`」——探针报告里最关键的一对读数。
///
/// `GetThreadDesktop` 返回的句柄**不属于调用方**，**不得** `CloseDesktop`。
///
/// # Errors
///
/// 句柄为空或读名字失败时返回 [`crate::WinError`]。
#[cfg(windows)]
pub fn current_thread_desktop_name() -> crate::WinResult<String> {
    use windows_sys::Win32::System::StationsAndDesktops::GetThreadDesktop;
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;

    // SAFETY: 无参数、无副作用。
    let tid = unsafe { GetCurrentThreadId() };
    // SAFETY: 传本线程 id，返回本线程的桌面句柄（借用语义，不能关闭）。
    let handle = unsafe { GetThreadDesktop(tid) };
    let handle = crate::handle::require_handle("GetThreadDesktop", handle)?;
    window_object_name(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_the_two_desktops_that_matter() {
        assert_eq!(DesktopKind::from_name("Default"), DesktopKind::Default);
        assert_eq!(DesktopKind::from_name("Winlogon"), DesktopKind::Winlogon);
    }

    #[test]
    fn classification_is_case_and_whitespace_tolerant() {
        assert_eq!(DesktopKind::from_name("winlogon"), DesktopKind::Winlogon);
        assert_eq!(
            DesktopKind::from_name("  WINLOGON\n"),
            DesktopKind::Winlogon
        );
        assert_eq!(DesktopKind::from_name("default"), DesktopKind::Default);
    }

    #[test]
    fn screensaver_is_matched_with_and_without_the_hyphen() {
        assert_eq!(
            DesktopKind::from_name("Screen-saver"),
            DesktopKind::ScreenSaver
        );
        assert_eq!(
            DesktopKind::from_name("Screensaver"),
            DesktopKind::ScreenSaver
        );
    }

    #[test]
    fn unknown_and_empty_names_fall_back_to_other() {
        assert_eq!(DesktopKind::from_name(""), DesktopKind::Other);
        assert_eq!(DesktopKind::from_name("Disconnected"), DesktopKind::Other);
        assert_eq!(
            DesktopKind::from_name("SomeOtherDesktop"),
            DesktopKind::Other
        );
        assert_eq!(
            DesktopKind::from_name("Disconnect"),
            DesktopKind::Disconnect
        );
    }

    #[test]
    fn secure_desktops_are_exactly_the_two_privileged_ones() {
        assert!(DesktopKind::Winlogon.is_secure());
        assert!(DesktopKind::Winlogon.is_winlogon());
        assert!(DesktopKind::ScreenSaver.is_secure());
        assert!(!DesktopKind::Default.is_secure());
        assert!(!DesktopKind::Disconnect.is_secure());
        assert!(!DesktopKind::Other.is_secure());
    }

    #[test]
    fn as_str_round_trips_through_from_name() {
        for kind in [
            DesktopKind::Default,
            DesktopKind::Winlogon,
            DesktopKind::ScreenSaver,
            DesktopKind::Disconnect,
        ] {
            assert_eq!(DesktopKind::from_name(kind.as_str()), kind, "{kind:?}");
        }
    }
}
