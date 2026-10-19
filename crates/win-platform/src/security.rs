//! 目录加固：把产品目录的 DACL 收敛到 **SYSTEM + Administrators 完全控制**，并去掉继承
//! （**整模块只在 Windows 上编译**）。
//!
//! # 威胁模型（为什么必须做）
//!
//! 服务以 LocalSystem 运行，它启动的实例 exe 与它自己读的 `host.toml` 都含特权信息
//! （`host.toml` 里是服务端**全局 token**）。只要这些文件放在**任何本机用户可写**的
//! 目录里，普通用户就能：
//!
//! 1. **替换服务 exe** → 下次服务启动时以 SYSTEM 运行攻击者的代码（本地提权）；
//! 2. 预建 **junction** 把 SYSTEM 的日志写入重定向到别处，或直接读走日志；
//! 3. 改 `host.toml` 把实例指向攻击者自己的服务端（劫持被控端身份）。
//!
//! `%ProgramFiles%` 与 `%ProgramData%` 本身都不是安全边界——`%ProgramData%` 对
//! `BUILTIN\Users` 明确给了 read/create。所以本模块**显式**给产品目录设一条
//! 「只有 SYSTEM 与 Administrators」的 DACL，并把它标成 protected（不继承父目录）。
//!
//! # 为什么是「设置」而不是「检查」
//!
//! 只读地**校验**一个目录「安不安全」需要枚举 ACE、理解 deny 语义、处理继承与
//! 对象型 ACE——判错的方向恰恰是「把不安全当成安全」（fail-open），而那是这个模块
//! 唯一不能犯的错。所以这里反过来：**自己写一条明确的 DACL 下去**，再读回来确认
//! 「确实只有这两条 ACE」。设置失败或校验失败都让调用方**整体失败**（fail-closed）。
//!
//! # 校验里 `AceCount == 2` 为什么等价于「继承已去掉」
//!
//! 我们的两条 ACE 是**允许**型、`FILE_ALL_ACCESS`。若 `PROTECTED_DACL_SECURITY_INFORMATION`
//! 没生效，父目录（`%ProgramFiles%` / `%ProgramData%`）里那些可继承的 ACE 会被合并进来、
//! 以 `INHERITED_ACE` 出现在同一条 DACL 里，`AceCount` 必然大于 2。所以「恰好两条」既是
//! 「没有多余的授权」，也是「继承确实被切断了」。

use crate::WinError;
use crate::WinResult;
use std::path::Path;

/// `SECURITY_MAX_SID_SIZE`（WinNT.h）：一个 SID 的最大字节数。
const SECURITY_MAX_SID_SIZE: usize = 68;

/// `FILE_ALL_ACCESS`（WinNT.h）：`STANDARD_RIGHTS_REQUIRED | SYNCHRONIZE | 0x1FF`。
const FILE_ALL_ACCESS: u32 = 0x001F_01FF;

/// `ACCESS_ALLOWED_ACE_TYPE`（WinNT.h）。windows-sys 0.61.2 没导出这个名字，故自带一份。
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

/// 期望出现在产品目录 DACL 里的两条 ACE 的 SID 数量。
const EXPECTED_ACE_COUNT: u32 = 2;

/// 建目录（若不存在）并把它（及之后在其中新建的子项）的 DACL 设为
/// 「仅 SYSTEM + Administrators 完全控制、不继承父目录」。
///
/// # 幂等的范围（**只对目录本身**）
///
/// 对**目录本身**是幂等的：重复调用写下的 DACL 一样，读回校验也一样。但**不递归**——
/// `SetNamedSecurityInfoW` 带 `PROTECTED_DACL_SECURITY_INFORMATION` 只作用于传入的那一个
/// 对象，不会重写**已经存在**的子项。所以「加固过这个目录」**不**意味着目录里既有的
/// 文件/子目录的 DACL 也是受限的：它们保留各自原有的描述符。只有**之后新建**的项才会
/// 继承这条限制（两条 ACE 都带 `OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE`）。
/// 要覆盖既有子项，得自己遍历；本函数不做那件事。
///
/// # Errors
///
/// 建目录、`SetNamedSecurityInfoW`、或读回校验任一步失败时返回 [`WinError`]。
/// **校验失败也要整体失败**：那说明我们没能证明目录已经安全。
pub fn harden_directory(path: &Path) -> WinResult<()> {
    std::fs::create_dir_all(path).map_err(|err| {
        tracing::error!(dir = %path.display(), error = %err, "创建目录失败");
        WinError::new("CreateDirectory", 0)
    })?;
    set_restricted_dacl(path)?;
    verify_restricted_dacl(path)?;
    tracing::info!(dir = %path.display(), "目录已加固（仅 SYSTEM + Administrators，无继承）");
    Ok(())
}

/// 给 `path` 设一条 protected DACL：SYSTEM 与 Administrators 完全控制，
/// 两条 ACE 都带 `OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE`，于是**之后**在目录里
/// 新建的文件/子目录会继承这条限制。
fn set_restricted_dacl(path: &Path) -> WinResult<()> {
    use windows_sys::Win32::Security::Authorization::{SetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        AddAccessAllowedAceEx, InitializeAcl, WinBuiltinAdministratorsSid, WinLocalSystemSid, ACL,
        ACCESS_ALLOWED_ACE, ACL_REVISION, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION,
        OBJECT_INHERIT_ACE, PROTECTED_DACL_SECURITY_INFORMATION,
    };

    let system = well_known_sid(WinLocalSystemSid)?;
    let admins = well_known_sid(WinBuiltinAdministratorsSid)?;

    // ACL 缓冲：两条 ACE，每条含一个最大长度的 SID。用 `Vec<u32>` 保证 4 字节对齐
    // （`ACL` 与 `ACCESS_ALLOWED_ACE` 都按 4 字节对齐）。
    let per_ace = std::mem::size_of::<ACCESS_ALLOWED_ACE>() + SECURITY_MAX_SID_SIZE;
    let needed = std::mem::size_of::<ACL>() + EXPECTED_ACE_COUNT as usize * per_ace;
    let words = needed.div_ceil(std::mem::size_of::<u32>());
    let mut storage = vec![0_u32; words];
    let acl = storage.as_mut_ptr().cast::<ACL>();
    let acl_len = u32::try_from(words * std::mem::size_of::<u32>()).unwrap_or(0);

    // SAFETY: storage 在本栈帧上、4 字节对齐、长度如实告知；InitializeAcl 只写这块缓冲。
    let ok = unsafe { InitializeAcl(acl, acl_len, ACL_REVISION) };
    crate::error::check_bool("InitializeAcl", ok)?;

    let inherit = OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE;
    for sid in [&system, &admins] {
        // SAFETY: acl 已初始化且容量足够容纳这两条 ACE；sid 是 CreateWellKnownSid 给出的
        // 有效 SID，在本调用期间存活（`system`/`admins` 在函数作用域内）。
        let ok = unsafe {
            AddAccessAllowedAceEx(
                acl,
                ACL_REVISION,
                inherit,
                FILE_ALL_ACCESS,
                sid.as_ptr().cast_mut().cast(),
            )
        };
        crate::error::check_bool("AddAccessAllowedAceEx", ok)?;
    }

    let wide = crate::wide::to_wide(&path.to_string_lossy());
    // SAFETY: wide 是 NUL 结尾的 UTF-16 缓冲（在本调用期间存活）；acl 是上面建好的
    // 有效 ACL；owner/group/sacl 传 null 表示「不改」。
    let code = unsafe {
        SetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            acl,
            std::ptr::null(),
        )
    };
    if code != 0 {
        return Err(WinError::new("SetNamedSecurityInfoW", code));
    }
    Ok(())
}

/// 读回 `path` 的 DACL，确认它恰好是「SYSTEM 与 Administrators 两条允许 ACE」。
fn verify_restricted_dacl(path: &Path) -> WinResult<()> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{ACL, DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};

    let wide = crate::wide::to_wide(&path.to_string_lossy());
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: wide 是 NUL 结尾的 UTF-16 缓冲；dacl/sd 是本栈帧上的输出参数。
    let code = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut dacl,
            std::ptr::null_mut(),
            &raw mut sd,
        )
    };
    if code != 0 {
        return Err(WinError::new("GetNamedSecurityInfoW", code));
    }
    let checked = inspect_dacl(dacl);
    // sd 由 GetNamedSecurityInfoW 经 LocalAlloc 分配，按文档必须用 LocalFree 释放；
    // dacl 指向 sd 内部，故必须在释放**之前**读完（inspect_dacl 已经读完）。
    // SAFETY: sd 是上面那个调用给出的、本函数独占的缓冲。
    unsafe {
        LocalFree(sd.cast());
    }
    checked
}

/// 检查 DACL 的内容（恰好两条 ACCESS_ALLOWED，SID 分别为 SYSTEM 与 Administrators）。
fn inspect_dacl(dacl: *mut windows_sys::Win32::Security::ACL) -> WinResult<()> {
    use windows_sys::Win32::Security::{
        EqualSid, GetAce, GetAclInformation, WinBuiltinAdministratorsSid, WinLocalSystemSid,
        ACCESS_ALLOWED_ACE, ACL_SIZE_INFORMATION, AclSizeInformation, ACE_HEADER,
    };

    if dacl.is_null() {
        return Err(WinError::new("GetNamedSecurityInfoW(dacl)", 0));
    }

    let mut size = ACL_SIZE_INFORMATION::default();
    // SAFETY: dacl 非空且有效；size 是本栈帧上的结构，长度如实告知。
    let ok = unsafe {
        GetAclInformation(
            dacl,
            std::ptr::addr_of_mut!(size).cast(),
            u32::try_from(std::mem::size_of::<ACL_SIZE_INFORMATION>()).unwrap_or(0),
            AclSizeInformation,
        )
    };
    crate::error::check_bool("GetAclInformation", ok)?;

    if size.AceCount != EXPECTED_ACE_COUNT {
        // 多于两条 = 父目录的可继承 ACE 被合并进来了（继承没切断）；
        // 少于两条 = 我们写下去的那条 DACL 没生效。两种都不能放行。
        return Err(WinError::new(
            "harden_directory(DACL 的 ACE 数不是 2)",
            size.AceCount,
        ));
    }

    let system = well_known_sid(WinLocalSystemSid)?;
    let admins = well_known_sid(WinBuiltinAdministratorsSid)?;
    let mut seen_system = false;
    let mut seen_admins = false;

    for index in 0..size.AceCount {
        let mut ace: *mut std::ffi::c_void = std::ptr::null_mut();
        // SAFETY: dacl 有效且 index < AceCount；ace 是本栈帧上的输出参数。
        let ok = unsafe { GetAce(dacl, index, &raw mut ace) };
        crate::error::check_bool("GetAce", ok)?;
        if ace.is_null() {
            return Err(WinError::new("GetAce(null)", 0));
        }
        // SAFETY: ace 指向 DACL 内一个完整的 ACE；用 read_unaligned 避免对 ACL 内部
        // 对齐做假设；ACE_HEADER 是 POD。
        let header = unsafe { std::ptr::read_unaligned(ace.cast::<ACE_HEADER>()) };
        if header.AceType != ACCESS_ALLOWED_ACE_TYPE {
            return Err(WinError::new(
                "harden_directory(ACE 类型不是允许型)",
                u32::from(header.AceType),
            ));
        }
        // SID 紧跟在 ACCESS_ALLOWED_ACE 的 `SidStart` 字段处。`SidStart` 是最后一个
        // 字段（u32），故它的偏移 = 结构大小 - 4——这是 C 侧读 ACE 的经典写法。
        let sid_offset = std::mem::size_of::<ACCESS_ALLOWED_ACE>() - std::mem::size_of::<u32>();
        // SAFETY: ace 指向一个至少 ACCESS_ALLOWED_ACE 大的 ACE，SidStart 之后是 SID。
        let sid = unsafe { ace.cast::<u8>().add(sid_offset).cast() };
        // SAFETY: 两个指针都指向有效的 SID（一个是 ACE 内的，一个来自 CreateWellKnownSid）。
        if unsafe { EqualSid(sid, system.as_ptr().cast_mut().cast()) } != 0 {
            seen_system = true;
        }
        // SAFETY: 同上。
        if unsafe { EqualSid(sid, admins.as_ptr().cast_mut().cast()) } != 0 {
            seen_admins = true;
        }
    }

    if seen_system && seen_admins {
        Ok(())
    } else {
        Err(WinError::new(
            "harden_directory(DACL 里不是 SYSTEM + Administrators)",
            0,
        ))
    }
}

/// 用 `CreateWellKnownSid` 造一个内置账号的 SID，返回它的字节缓冲。
fn well_known_sid(kind: windows_sys::Win32::Security::WELL_KNOWN_SID_TYPE) -> WinResult<Vec<u8>> {
    use windows_sys::Win32::Security::CreateWellKnownSid;

    let mut buf = vec![0_u8; SECURITY_MAX_SID_SIZE];
    let mut len = u32::try_from(buf.len()).unwrap_or(0);
    // SAFETY: domainsid 传 null 表示本机；buf 是 SECURITY_MAX_SID_SIZE 大的可写缓冲，
    // len 是本栈帧上的 u32（进=缓冲长度，出=实际 SID 长度）。
    let ok = unsafe {
        CreateWellKnownSid(
            kind,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &raw mut len,
        )
    };
    crate::error::check_bool("CreateWellKnownSid", ok)?;
    buf.truncate(usize::try_from(len).unwrap_or(0));
    Ok(buf)
}
