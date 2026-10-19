//! 单实例锁：一个 `Global\` 命名互斥体，**在服务侧**串行化「拉起被控实例」这件事。
//!
//! # 它到底防住了什么、没防住什么（**别高估它**）
//!
//! 实例承载的是客户端身份，而服务端按**客户端名**唯一：两个实例同时注册，后一个会把
//! 前一个踢下线，用户看到的是「远程控制每隔几秒掉一次」。最容易撞出两个实例的窗口是
//! **注销→登录**：服务收到会话变化就重建实例，若旧实例还没死透、新实例已经起来，两者
//! 就会并存一小段时间。
//!
//! 服务侧的主防线是「先杀旧实例、确认它退出、再拉新的」（见 `win-service` 的监督循环）。
//! **本锁只是它的补充，覆盖面比名字听起来小得多**——把边界写清楚，免得有人把它当成兜底：
//!
//! - 锁的**唯一**调用点在服务侧（`win-service` 的 `run_instance`），且只在**那一次**
//!   `run_instance` 的存续期内持有（`run_instance` 一返回就 `Drop`，锁随之释放）。
//! - **实例进程自己从不取这把锁**。所以它挡住的只有「服务侧的两次拉起相互撞车」
//!   （同一个服务进程的重建循环，或两个服务进程同时监督）：拿不到锁的那一次会在
//!   `CreateMutexW` 上撞到 `ERROR_ALREADY_EXISTS`，从而**跳过这一次拉起**。
//! - **它挡不住孤儿实例**：服务在「`CreateProcessAsUserW` 成功」与「`assign_to_job`
//!   成功」之间崩溃、或 `assign_to_job` 直接失败（`win-service` 那边只 warn、不致命）、
//!   或 `kill_instance` 等实例退出超时后放弃，都会留下一个**既不在作业对象里、也没人
//!   持有这把锁**的实例在跑。下一次拉起时锁是空的，新实例照起，两者并存——这种孤儿
//!   只能靠服务端「按客户端名唯一」把它踢掉，本锁无能为力。
//!
//! # 为什么名字必须是 `Global\`
//!
//! 不加前缀的命名对象落在**调用进程所在会话**的命名空间里（`Local\`）。快速用户切换后
//! 两个会话各有各的 `rust-tunnel-instance`，互不冲突——那正好是这个锁要防的场景。
//! `Global\` 前缀把对象放进**机器级**命名空间，跨会话可见。
//!
//! # 为什么是互斥体而不是事件/文件
//!
//! 互斥体由**内核**维护：持有者进程一死（正常退出、崩溃、被 `TerminateProcess`），
//! 它的句柄被回收，对象随之消失（若无别的句柄）。不需要任何「清理陈旧锁」的逻辑——
//! 那是基于文件的锁最容易写错的地方（崩溃后留下一个永不过期的锁文件）。
//!
//! # 创建 `Global\` 对象需要 `SeCreateGlobalPrivilege`
//!
//! 这个特权对服务账号（含 LocalSystem）默认持有且启用，所以服务侧正常能建。
//! 但**调用方不要把 [`InstanceGuard::acquire`] 的 `Err` 当成致命错误**：拿不到这个
//! 特权时退化成「没有这道防线」，比「服务永远拉不起实例」好得多——「先杀旧实例、
//! 等它真的退出、再拉新的」那条主路径本来就独立成立。

/// 单实例互斥体的名字（`Global\` = 机器级命名空间，跨会话可见）。
///
/// **一旦发布就不能改**：改名等于让新旧两代实例互相看不见，锁就形同虚设。
pub const INSTANCE_MUTEX_NAME: &str = r"Global\rust-tunnel-client-instance";

/// 获取单实例锁的结果。
#[cfg(windows)]
#[derive(Debug)]
pub enum LockOutcome {
    /// 拿到了锁（此前没有别的持有者）。锁随返回的 [`InstanceGuard`] 存活。
    Acquired(InstanceGuard),
    /// **已被别的进程持有**：此时**不要**再拉起实例。
    AlreadyHeld,
}

/// 单实例锁的 RAII 持有者：`Drop` 时 `CloseHandle`，锁随之释放。
///
/// 拿不到锁（[`LockOutcome::AlreadyHeld`]）时**不会**构造出它。
#[cfg(windows)]
#[derive(Debug)]
pub struct InstanceGuard {
    /// 互斥体句柄（拥有）。只靠它的存在与否表达「有人持有」——服务**不**用
    /// `WaitForSingleObject` 去等它（那会在旧实例卡死时把服务一起拖住），
    /// 而是「拿不到就跳过这一次拉起」。
    _handle: crate::handle::OwnedHandle,
}

#[cfg(windows)]
impl InstanceGuard {
    /// 尝试获取单实例锁。
    ///
    /// # Errors
    ///
    /// `CreateMutexW` 本身失败（极少见）时返回 [`crate::WinError`]。
    /// 「已被别人持有」**不是**错误，走 [`LockOutcome::AlreadyHeld`]。
    pub fn acquire() -> crate::WinResult<LockOutcome> {
        use windows_sys::Win32::Foundation::ERROR_ALREADY_EXISTS;
        use windows_sys::Win32::System::Threading::CreateMutexW;

        let name = crate::wide::to_wide(INSTANCE_MUTEX_NAME);
        // SAFETY: 安全属性传 null = 默认（句柄不继承，普通用户也打得开——本锁不承载
        // 任何秘密，只表达「有没有人在跑」）；binitialowner = 0（**不**要求所有权：
        // 我们要的只是「对象存在与否」，且这样进程死掉时不会留下「被放弃的互斥体」语义）；
        // name 是 NUL 结尾的 UTF-16 缓冲。
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        // `ERROR_ALREADY_EXISTS` 必须在**紧接**调用的地方读——`require_handle` 与
        // `to_wide` 都不调 Win32，但把读取提前到这一行最稳妥。
        let already_held = crate::error::last_error("CreateMutexW").code() == ERROR_ALREADY_EXISTS;
        let handle = crate::handle::require_handle("CreateMutexW", handle)?;
        let owned = crate::handle::OwnedHandle::from_raw(handle);

        if already_held {
            // 关键：`CreateMutexW` 在「对象已存在」时**仍然返回一个有效句柄**。
            // 直接丢弃 `owned` 会把那个句柄关掉（`Drop`），**不影响**真正持有者的锁
            // ——互斥体对象的生命周期由「还有没有句柄开着」决定，别人那个句柄还开着。
            drop(owned);
            return Ok(LockOutcome::AlreadyHeld);
        }
        Ok(LockOutcome::Acquired(Self { _handle: owned }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mutex_lives_in_the_global_namespace() {
        // 不加 `Global\` 前缀就落进调用进程所在会话的命名空间：快速用户切换后两个会话
        // 各有一个同名对象、互不冲突——那正是这个锁要防的场景。
        assert!(
            INSTANCE_MUTEX_NAME.starts_with(r"Global\"),
            "单实例锁必须在 Global 命名空间：{INSTANCE_MUTEX_NAME}"
        );
        assert!(INSTANCE_MUTEX_NAME.contains("rust-tunnel"));
        // 名字里不能有反斜杠以外的分隔歧义：`Global\` 之后应当是一段普通名字。
        assert_eq!(INSTANCE_MUTEX_NAME.matches('\\').count(), 1);
    }

    #[test]
    fn the_mutex_name_is_frozen() {
        // 改名等于让新旧两代实例互相看不见，锁形同虚设——机械锁住字面值。
        assert_eq!(
            INSTANCE_MUTEX_NAME,
            r"Global\rust-tunnel-client-instance"
        );
    }
}
