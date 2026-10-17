//! 令牌（token）：完整性级别语义（可移植）+ 取令牌/改会话/启用特权（Windows FFI）。
//!
//! # 为什么探针必须把令牌摊开来看
//!
//! 「注入到锁屏界面」这件事的成败由三层共同决定，其中两层的判据都在**令牌**上：
//!
//! 1. 会话隔离 → `TokenSessionId` 是不是**控制台会话**（服务进程在会话 0，
//!    不把它改成控制台会话，`OpenInputDesktop` 永远看不到 `Winlogon` 桌面）；
//! 2. UIPI → 令牌的**完整性级别**必须 ≥ 目标进程（LogonUI 是 `System` 级别，
//!    所以被控端自己也必须是 `System`，`High`（管理员提权）**不够**）；
//! 3. 桌面 ACL → 判据是令牌的 **SID 列表**（`Winlogon` 桌面只放行 LocalSystem
//!    等少数 SID，`Administrators` 不在其中）。
//!
//! 所以 [`token_sid_strings`] 与 [`token_integrity_level`] 不是「顺手记一下」，
//! 它们是把「为什么不行」钉死在证据上的那一环。
//!
//! # 可移植部分
//!
//! [`IntegrityLevel`] 是**纯 RID → 枚举**的映射，以及 UIPI 的 `>=` 规则
//! （[`IntegrityLevel::can_inject_into`]），在 macOS 上有单测。RID 常量抄自
//! WinNT.h（`SECURITY_MANDATORY_*_RID`）。

// 注意：与 monitor.rs 同理，顶层的 `use crate::WinResult` 会在 macOS 上变成
// 未使用导入（本模块的 Windows 函数全在 cfg 后面），故签名里写全路径。

/// 不受信任级别（`SECURITY_MANDATORY_UNTRUSTED_RID`）。
pub const SECURITY_MANDATORY_UNTRUSTED_RID: u32 = 0x0000;
/// 低完整性（`SECURITY_MANDATORY_LOW_RID`）。
pub const SECURITY_MANDATORY_LOW_RID: u32 = 0x1000;
/// 中完整性（`SECURITY_MANDATORY_MEDIUM_RID`）：普通用户进程。
pub const SECURITY_MANDATORY_MEDIUM_RID: u32 = 0x2000;
/// 中+ 完整性（`SECURITY_MANDATORY_MEDIUM_PLUS_RID`）。
pub const SECURITY_MANDATORY_MEDIUM_PLUS_RID: u32 = 0x2100;
/// 高完整性（`SECURITY_MANDATORY_HIGH_RID`）：**提权后的**管理员进程。
pub const SECURITY_MANDATORY_HIGH_RID: u32 = 0x3000;
/// System 完整性（`SECURITY_MANDATORY_SYSTEM_RID`）：SYSTEM 服务、LogonUI。
pub const SECURITY_MANDATORY_SYSTEM_RID: u32 = 0x4000;
/// 受保护进程完整性（`SECURITY_MANDATORY_PROTECTED_PROCESS_RID`）。
pub const SECURITY_MANDATORY_PROTECTED_PROCESS_RID: u32 = 0x5000;

/// 令牌的完整性级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum IntegrityLevel {
    /// 不受信任（AppContainer 等）。
    Untrusted,
    /// 低（IE 保护模式、部分沙箱）。
    Low,
    /// 中（**普通用户进程**，未提权的管理员也是这里）。
    Medium,
    /// 中+（较少见）。
    MediumPlus,
    /// 高（UAC 提权后的管理员）。
    High,
    /// System（**SYSTEM 服务与 LogonUI**）。
    System,
    /// 受保护进程（PPL，如反恶意软件服务）。
    ProtectedProcess,
}

impl IntegrityLevel {
    /// 由 SID 的**最后一个子权威**（RID）归类。
    ///
    /// 用的是区间判定而不是等值：真机上常见的是 `0x3100` 这类「高完整性但带自定义
    /// RID」的令牌，等值判定会把它们全归到「未知」。区间与 MSDN 的建议一致。
    #[must_use]
    pub const fn from_rid(rid: u32) -> Self {
        if rid < SECURITY_MANDATORY_LOW_RID {
            Self::Untrusted
        } else if rid < SECURITY_MANDATORY_MEDIUM_RID {
            Self::Low
        } else if rid < SECURITY_MANDATORY_MEDIUM_PLUS_RID {
            Self::Medium
        } else if rid < SECURITY_MANDATORY_HIGH_RID {
            Self::MediumPlus
        } else if rid < SECURITY_MANDATORY_SYSTEM_RID {
            Self::High
        } else if rid < SECURITY_MANDATORY_PROTECTED_PROCESS_RID {
            Self::System
        } else {
            Self::ProtectedProcess
        }
    }

    /// 稳定名（写进探针报告，别用 `Debug`）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Untrusted => "Untrusted",
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::MediumPlus => "Medium-Plus",
            Self::High => "High",
            Self::System => "System",
            Self::ProtectedProcess => "Protected-Process",
        }
    }

    /// 该级别能不能往 `target` 级别的进程里注入输入（UIPI 规则：**同级或更高**）。
    ///
    /// 探针用它预判结论：控制器注入的是**被控端自己的线程**，故被控端令牌级别
    /// 必须 ≥ LogonUI 的级别（`System`）——这正是「以管理员身份运行没用」的形式化表述。
    #[must_use]
    pub const fn can_inject_into(self, target: Self) -> bool {
        (self as u8) >= (target as u8)
    }
}

/// 取**当前进程**的令牌，权限开到本 crate 用得到的全部。
///
/// 申请的权限：`TOKEN_QUERY`（读会话/完整性/SID）、`TOKEN_DUPLICATE`（复制成
/// 可改会话的主令牌）、`TOKEN_ADJUST_PRIVILEGES`（提权 `SeTcbPrivilege` 等）、
/// `TOKEN_ADJUST_SESSIONID`（改会话 id）、`TOKEN_ASSIGN_PRIMARY`（`CreateProcessAsUser`
/// 要求）。
///
/// # Errors
///
/// `OpenProcessToken` 失败时返回 [`crate::WinError`]（正常权限下不会失败）。
#[cfg(windows)]
pub fn current_process_token() -> crate::WinResult<crate::handle::OwnedHandle> {
    use windows_sys::Win32::Security::{
        TOKEN_ADJUST_PRIVILEGES, TOKEN_ADJUST_SESSIONID, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE,
        TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let access = TOKEN_QUERY
        | TOKEN_DUPLICATE
        | TOKEN_ADJUST_PRIVILEGES
        | TOKEN_ADJUST_SESSIONID
        | TOKEN_ASSIGN_PRIMARY;
    let mut token = std::ptr::null_mut();
    // SAFETY: GetCurrentProcess 返回当前进程的伪句柄（无需关闭、恒有效）；
    // token 是本栈帧上的 HANDLE，成功时由 OwnedHandle 接管。
    let ok = unsafe { OpenProcessToken(GetCurrentProcess(), access, &raw mut token) };
    crate::error::check_bool("OpenProcessToken", ok)?;
    let token = crate::handle::require_handle("OpenProcessToken", token)?;
    Ok(crate::handle::OwnedHandle::from_raw(token))
}

/// 调 `GetTokenInformation` 两趟，返回原始字节。
#[cfg(windows)]
fn token_info_bytes(
    token: windows_sys::Win32::Foundation::HANDLE,
    class: windows_sys::Win32::Security::TOKEN_INFORMATION_CLASS,
    what: &'static str,
) -> crate::WinResult<Vec<u8>> {
    use windows_sys::Win32::Security::GetTokenInformation;

    let mut needed: u32 = 0;
    // 第一趟：只问长度。按文档此时返回 0 并置 last-error 为 ERROR_INSUFFICIENT_BUFFER，
    // 这是**预期**路径，故不看返回值。
    // SAFETY: tokenInformation 传 null + 长度 0 即「只要长度」；needed 是本栈帧变量。
    unsafe {
        GetTokenInformation(token, class, std::ptr::null_mut(), 0, &raw mut needed);
    }
    if needed == 0 {
        return Err(crate::error::last_error(what));
    }
    let mut buf: Vec<u8> = vec![0; usize::try_from(needed).unwrap_or(0)];
    let mut got: u32 = 0;
    // SAFETY: buf 是本栈帧上的可写缓冲且长度如实告知；got 同样在本栈帧上。
    let ok =
        unsafe { GetTokenInformation(token, class, buf.as_mut_ptr().cast(), needed, &raw mut got) };
    crate::error::check_bool(what, ok)?;
    Ok(buf)
}

/// 读令牌的会话 id（`TokenSessionId`）。
///
/// **服务进程在会话 0**；要够到用户桌面就必须把它改成控制台会话
/// （[`set_token_session_id`]）。
///
/// # Errors
///
/// 读不到（极少见）时返回 [`crate::WinError`]。
#[cfg(windows)]
pub fn token_session_id(token: &crate::handle::OwnedHandle) -> crate::WinResult<u32> {
    use windows_sys::Win32::Security::TokenSessionId;

    let buf = token_info_bytes(
        token.raw(),
        TokenSessionId,
        "GetTokenInformation(TokenSessionId)",
    )?;
    let mut bytes = [0u8; 4];
    if buf.len() < bytes.len() {
        return Err(crate::WinError::new(
            "GetTokenInformation(TokenSessionId)",
            0,
        ));
    }
    bytes.copy_from_slice(&buf[..4]);
    Ok(u32::from_ne_bytes(bytes))
}

/// 把令牌的会话 id 改成 `session`（`SetTokenInformation(TokenSessionId)`）。
///
/// 令牌需要 `TOKEN_ADJUST_SESSIONID`（[`current_process_token`] 已申请）。
///
/// # Errors
///
/// 权限不足或令牌不可写时返回 [`crate::WinError`]（错误码原样保留，是探针的诊断依据）。
#[cfg(windows)]
pub fn set_token_session_id(
    token: &crate::handle::OwnedHandle,
    session: u32,
) -> crate::WinResult<()> {
    use windows_sys::Win32::Security::{SetTokenInformation, TokenSessionId};

    let size = u32::try_from(std::mem::size_of::<u32>()).unwrap_or(4);
    // SAFETY: tokenInformation 指向本栈帧上的 u32，长度如实告知为 4。
    let ok = unsafe {
        SetTokenInformation(
            token.raw(),
            TokenSessionId,
            std::ptr::addr_of!(session).cast(),
            size,
        )
    };
    crate::error::check_bool("SetTokenInformation(TokenSessionId)", ok)
}

/// 复制出一个**主令牌**并把它改到 `session` 会话。
///
/// 这是把被控端进程拉进控制台会话的那一步：`DuplicateTokenEx` 拿到可改的主令牌，
/// 再 [`set_token_session_id`]，最后交给 `CreateProcessAsUser`。
///
/// 用 `SecurityImpersonation` + `TokenPrimary`：`CreateProcessAsUser` 只吃主令牌，
/// 而 `SecurityImpersonation` 是主令牌复制的标准级别（`SecurityAnonymous`/
/// `SecurityIdentification` 复制出来的令牌不能用于创建进程）。
///
/// # Errors
///
/// 复制或改会话失败时返回 [`crate::WinError`]。
#[cfg(windows)]
pub fn duplicate_primary_for_session(
    token: &crate::handle::OwnedHandle,
    session: u32,
) -> crate::WinResult<crate::handle::OwnedHandle> {
    use windows_sys::Win32::Security::{
        DuplicateTokenEx, SecurityImpersonation, TokenPrimary, TOKEN_ALL_ACCESS,
    };

    let mut new_token = std::ptr::null_mut();
    // SAFETY: token 有效且带 TOKEN_DUPLICATE；lptokenattributes 传 null 表示默认
    // 安全描述符；phnewtoken 是本栈帧上的 HANDLE，成功时由 OwnedHandle 接管。
    let ok = unsafe {
        DuplicateTokenEx(
            token.raw(),
            TOKEN_ALL_ACCESS,
            std::ptr::null(),
            SecurityImpersonation,
            TokenPrimary,
            &raw mut new_token,
        )
    };
    crate::error::check_bool("DuplicateTokenEx", ok)?;
    let new_token = crate::handle::require_handle("DuplicateTokenEx", new_token)?;
    let owned = crate::handle::OwnedHandle::from_raw(new_token);
    set_token_session_id(&owned, session)?;
    Ok(owned)
}

/// 读令牌的完整性级别（`TokenIntegrityLevel`）。
///
/// 实现要点：返回的 `TOKEN_MANDATORY_LABEL` 里只有一个 **SID 指针**，
/// 完整性级别是该 SID 的**最后一个子权威**（`GetSidSubAuthority(count - 1)`），
/// 不是某个固定字段名——这是这个 API 最容易写错的地方。
///
/// # Errors
///
/// 结构不完整或 SID 不可读时返回 [`crate::WinError`]。
#[cfg(windows)]
pub fn token_integrity_level(
    token: &crate::handle::OwnedHandle,
) -> crate::WinResult<IntegrityLevel> {
    use windows_sys::Win32::Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, TokenIntegrityLevel, TOKEN_MANDATORY_LABEL,
    };

    let buf = token_info_bytes(
        token.raw(),
        TokenIntegrityLevel,
        "GetTokenInformation(TokenIntegrityLevel)",
    )?;
    if buf.len() < std::mem::size_of::<TOKEN_MANDATORY_LABEL>() {
        return Err(crate::WinError::new(
            "GetTokenInformation(TokenIntegrityLevel)",
            0,
        ));
    }
    // SAFETY: buf 至少有一个 TOKEN_MANDATORY_LABEL 那么大（上一行已校验）；
    // read_unaligned 避免对 Vec<u8> 的对齐做假设。TOKEN_MANDATORY_LABEL 是 POD。
    let label = unsafe { std::ptr::read_unaligned(buf.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()) };
    let sid = label.Label.Sid;
    if sid.is_null() {
        return Err(crate::WinError::new("TokenIntegrityLevel.Sid", 0));
    }
    // SAFETY: sid 是内核在 buf 里给出的有效 SID 指针。
    let count_ptr = unsafe { GetSidSubAuthorityCount(sid) };
    if count_ptr.is_null() {
        return Err(crate::WinError::new("GetSidSubAuthorityCount", 0));
    }
    // SAFETY: 非空且指向一个 u8（SID 的 SubAuthorityCount 字段）。
    let count = unsafe { *count_ptr };
    if count == 0 {
        return Err(crate::WinError::new("GetSidSubAuthorityCount", 0));
    }
    let last = u32::from(count) - 1;
    // SAFETY: last 是合法的子权威下标（count 由内核给出且 ≥ 1）。
    let rid_ptr = unsafe { GetSidSubAuthority(sid, last) };
    if rid_ptr.is_null() {
        return Err(crate::WinError::new("GetSidSubAuthority", 0));
    }
    // SAFETY: 非空且指向该子权威的 u32。
    Ok(IntegrityLevel::from_rid(unsafe { *rid_ptr }))
}

/// 令牌是否已提权（`TokenElevation`，即 UAC 里的「已提升」）。
///
/// 注意它与完整性级别是**两件事**：未提权的管理员是 `Medium`，提权后是 `High`。
/// 报告里两个都记，才能说清「用户以为自己以管理员跑了，其实没有」。
///
/// # Errors
///
/// 读不到时返回 [`crate::WinError`]。
#[cfg(windows)]
pub fn token_is_elevated(token: &crate::handle::OwnedHandle) -> crate::WinResult<bool> {
    use windows_sys::Win32::Security::{TokenElevation, TOKEN_ELEVATION};

    let buf = token_info_bytes(
        token.raw(),
        TokenElevation,
        "GetTokenInformation(TokenElevation)",
    )?;
    if buf.len() < std::mem::size_of::<TOKEN_ELEVATION>() {
        return Err(crate::WinError::new(
            "GetTokenInformation(TokenElevation)",
            0,
        ));
    }
    // SAFETY: 长度已校验；read_unaligned 避免对齐假设；TOKEN_ELEVATION 是 POD。
    let elevation = unsafe { std::ptr::read_unaligned(buf.as_ptr().cast::<TOKEN_ELEVATION>()) };
    Ok(elevation.TokenIsElevated != 0)
}

/// 把一个 SID 转成字符串形式（`S-1-5-18` 这样）。失败返回 `None`。
#[cfg(windows)]
fn sid_to_string(sid: windows_sys::Win32::Security::PSID) -> Option<String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;

    if sid.is_null() {
        return None;
    }
    let mut out: windows_sys::core::PWSTR = std::ptr::null_mut();
    // SAFETY: sid 是有效 SID 指针；out 是本栈帧上的 PWSTR，成功时由 LocalFree 释放。
    let ok = unsafe { ConvertSidToStringSidW(sid, &raw mut out) };
    if ok == 0 || out.is_null() {
        return None;
    }
    // 手动找 NUL 而不是用 CStr：PWSTR 是 UTF-16，且上限写死以防对方缓冲区畸形。
    let mut len = 0usize;
    while len < 1024 {
        // SAFETY: out 指向内核分配、以 NUL 结尾的 UTF-16 缓冲；len 有上限。
        if unsafe { *out.add(len) } == 0 {
            break;
        }
        len += 1;
    }
    // SAFETY: 上面已确认真实长度不超过 len；缓冲由本函数独占。
    let text = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(out, len) });
    // SAFETY: out 由 ConvertSidToStringSidW 经 LocalAlloc 分配，按文档要用 LocalFree。
    unsafe {
        LocalFree(out.cast());
    }
    Some(text)
}

/// 令牌的 SID 列表：**用户 SID 在前，组 SID 在后**。
///
/// 顺序是有意的：报告里第一个就是 `TokenUser`（`S-1-5-18` = LocalSystem），
/// 一眼可辨；后面是组（`S-1-5-32-544` = Administrators 等）。桌面 ACL 判定看的就是
/// 这一串里有没有对方要的那个 SID。
///
/// 读组失败（`TokenGroups` 在个别受限令牌上会拒绝）只让组列表为空，
/// **不影响**用户 SID——探针不该因为细节读不到就整体失败。
#[cfg(windows)]
#[must_use]
pub fn token_sid_strings(token: &crate::handle::OwnedHandle) -> Vec<String> {
    use windows_sys::Win32::Security::{
        TokenGroups, TokenUser, SID_AND_ATTRIBUTES, TOKEN_GROUPS, TOKEN_USER,
    };

    let mut out = Vec::new();

    if let Ok(buf) = token_info_bytes(token.raw(), TokenUser, "GetTokenInformation(TokenUser)") {
        if buf.len() >= std::mem::size_of::<TOKEN_USER>() {
            // SAFETY: 长度已校验；read_unaligned 避免对齐假设；TOKEN_USER 是 POD。
            let user = unsafe { std::ptr::read_unaligned(buf.as_ptr().cast::<TOKEN_USER>()) };
            if let Some(s) = sid_to_string(user.User.Sid) {
                out.push(s);
            }
        }
    }

    if let Ok(buf) = token_info_bytes(token.raw(), TokenGroups, "GetTokenInformation(TokenGroups)")
    {
        if buf.len() < std::mem::size_of::<TOKEN_GROUPS>() {
            return out;
        }
        // SAFETY: 长度已校验；read_unaligned 避免对齐假设；头部是 POD。
        let head = unsafe { std::ptr::read_unaligned(buf.as_ptr().cast::<TOKEN_GROUPS>()) };
        let count = usize::try_from(head.GroupCount).unwrap_or(0);
        // 数组紧跟在 GroupCount 之后，元素是定长结构，故可以用首元素地址 + 下标。
        let base = std::ptr::addr_of!(head.Groups).cast::<SID_AND_ATTRIBUTES>();
        let available = buf.len().saturating_sub(std::mem::size_of::<u32>())
            / std::mem::size_of::<SID_AND_ATTRIBUTES>();
        if count > available {
            // 内核给的长度与 GroupCount 不自洽：只信缓冲装得下的那部分，
            // 绝不越界读（这一条比「尽量多读几个组」重要得多）。
            return out;
        }
        for index in 0..count {
            // SAFETY: index < count ≤ available，落在 buf 之内；元素是 POD。
            let entry = unsafe { base.add(index).read_unaligned() };
            if let Some(s) = sid_to_string(entry.Sid) {
                out.push(s);
            }
        }
    }

    out
}

/// 启用令牌的一个特权（如 `SeTcbPrivilege`、`SeAssignPrimaryTokenPrivilege`）。
///
/// 返回 `true` 表示**确实**启用了。`AdjustTokenPrivileges` 的坑：它在
/// 「一个特权都没改」时也返回 `TRUE`，必须紧跟 `GetLastError()` 判定
/// （见 [`crate::handle::last_error_is_success`]）。
///
/// # Errors
///
/// 名字无效（`LookupPrivilegeValueW` 失败）或调用本身失败时返回 [`crate::WinError`]。
#[cfg(windows)]
pub fn enable_privilege(token: &crate::handle::OwnedHandle, name: &str) -> crate::WinResult<bool> {
    use windows_sys::Win32::Foundation::LUID;
    use windows_sys::Win32::Security::{
        AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED,
        TOKEN_PRIVILEGES,
    };

    let name = crate::wide::to_wide(name);
    let mut luid = LUID::default();
    // SAFETY: lpsystemname 传 null 表示本机；name 是 NUL 结尾的 UTF-16；
    // lpluid 是本栈帧上的 LUID。
    let ok = unsafe { LookupPrivilegeValueW(std::ptr::null(), name.as_ptr(), &raw mut luid) };
    crate::error::check_bool("LookupPrivilegeValueW", ok)?;

    let new_state = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES {
            Luid: luid,
            Attributes: SE_PRIVILEGE_ENABLED,
        }],
    };
    // SAFETY: newstate 指向本栈帧上的 TOKEN_PRIVILEGES（长度 1、结构定长）；
    // previousstate 传 null 时 bufferlength 必须为 0，这里正是如此。
    let ok = unsafe {
        AdjustTokenPrivileges(
            token.raw(),
            0,
            &raw const new_state,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    crate::error::check_bool("AdjustTokenPrivileges", ok)?;
    Ok(crate::handle::last_error_is_success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rid_boundaries_land_in_the_expected_buckets() {
        assert_eq!(
            IntegrityLevel::from_rid(SECURITY_MANDATORY_UNTRUSTED_RID),
            IntegrityLevel::Untrusted
        );
        assert_eq!(
            IntegrityLevel::from_rid(SECURITY_MANDATORY_LOW_RID),
            IntegrityLevel::Low
        );
        assert_eq!(
            IntegrityLevel::from_rid(SECURITY_MANDATORY_MEDIUM_RID),
            IntegrityLevel::Medium
        );
        assert_eq!(
            IntegrityLevel::from_rid(SECURITY_MANDATORY_MEDIUM_PLUS_RID),
            IntegrityLevel::MediumPlus
        );
        assert_eq!(
            IntegrityLevel::from_rid(SECURITY_MANDATORY_HIGH_RID),
            IntegrityLevel::High
        );
        assert_eq!(
            IntegrityLevel::from_rid(SECURITY_MANDATORY_SYSTEM_RID),
            IntegrityLevel::System
        );
        assert_eq!(
            IntegrityLevel::from_rid(SECURITY_MANDATORY_PROTECTED_PROCESS_RID),
            IntegrityLevel::ProtectedProcess
        );
    }

    #[test]
    fn one_below_a_boundary_stays_in_the_lower_bucket() {
        // 边界是「含下界」的区间：0x0FFF 属于 Untrusted，0x1000 才是 Low。
        assert_eq!(IntegrityLevel::from_rid(0x0FFF), IntegrityLevel::Untrusted);
        assert_eq!(IntegrityLevel::from_rid(0x1FFF), IntegrityLevel::Low);
        assert_eq!(IntegrityLevel::from_rid(0x2FFF), IntegrityLevel::MediumPlus);
        assert_eq!(IntegrityLevel::from_rid(0x3FFF), IntegrityLevel::High);
        assert_eq!(IntegrityLevel::from_rid(0x4FFF), IntegrityLevel::System);
    }

    #[test]
    fn custom_rids_inside_a_range_are_not_treated_as_unknown() {
        // 真机上常见带自定义 RID 的令牌（如 0x3100）；区间判定必须把它们归类，
        // 否则探针会打印一堆「未知」而看不出结论。
        assert_eq!(IntegrityLevel::from_rid(0x3100), IntegrityLevel::High);
        assert_eq!(IntegrityLevel::from_rid(0x4000), IntegrityLevel::System);
        assert_eq!(
            IntegrityLevel::from_rid(0xFFFF),
            IntegrityLevel::ProtectedProcess
        );
    }

    #[test]
    fn system_can_inject_into_logonui_but_high_cannot() {
        // 这就是「以管理员身份运行无效」的形式化表述：
        // LogonUI 是 System 级别，High（提权管理员）**不够**。
        assert!(IntegrityLevel::System.can_inject_into(IntegrityLevel::System));
        assert!(!IntegrityLevel::High.can_inject_into(IntegrityLevel::System));
        assert!(IntegrityLevel::System.can_inject_into(IntegrityLevel::High));
        assert!(IntegrityLevel::High.can_inject_into(IntegrityLevel::Medium));
    }

    #[test]
    fn can_inject_into_is_reflexive_for_every_level() {
        for level in [
            IntegrityLevel::Untrusted,
            IntegrityLevel::Low,
            IntegrityLevel::Medium,
            IntegrityLevel::MediumPlus,
            IntegrityLevel::High,
            IntegrityLevel::System,
            IntegrityLevel::ProtectedProcess,
        ] {
            assert!(level.can_inject_into(level), "{level:?}");
        }
    }

    #[test]
    fn as_str_is_stable_and_distinct() {
        let names: Vec<&str> = [
            IntegrityLevel::Untrusted,
            IntegrityLevel::Low,
            IntegrityLevel::Medium,
            IntegrityLevel::MediumPlus,
            IntegrityLevel::High,
            IntegrityLevel::System,
            IntegrityLevel::ProtectedProcess,
        ]
        .iter()
        .map(|l| l.as_str())
        .collect();
        assert_eq!(names[0], "Untrusted");
        assert_eq!(names[5], "System");
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "名字必须两两不同：{names:?}");
    }
}
