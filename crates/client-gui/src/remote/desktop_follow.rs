//! 桌面跟随状态机（**纯逻辑，无任何 Win32 依赖**）。
//!
//! # 为什么单独抽出来
//!
//! 实例（`--instance`）的采集与注入必须落在**当时的输入桌面**上：锁屏时输入桌面换成
//! `WinSta0\Winlogon`，挂在 `Default` 上的线程既采不到锁屏画面、也点不动它。
//! 「该不该切、切到哪个」这件事本身与 Win32 无关，抽成纯函数后就能在 macOS 上单测
//! ——而 `crates/client-gui` 里任何 `#[cfg(target_os = "windows")]` 的东西在本机
//! 根本不参与编译，一个测试也跑不到。
//!
//! # 与 `win-platform` 的分工
//!
//! `rust_tunnel_win_platform::desktop::DesktopKind` 是 Windows 侧的分类结果；本模块
//! 用 [`follow_of_name`] 按**桌面名**做同一套分类（可挂目标是 `Default` /
//! `Winlogon` / `Screen-saver` 三个，其余——`Disconnect` / 未知名 / 空名——一律
//! [`FollowDesktop::Unsupported`]）。之所以不复用 `DesktopKind`：
//! `rust-tunnel-win-platform` 在本 crate 里是 **windows-only 依赖**，纯逻辑模块引用它
//! 就会在 macOS 上编译不过，可测性随之消失。

// 唯一的调用方是 Windows 专用的 `remote::win_desktop`；在非 Windows 构建里这些纯逻辑
// 只有测试用得到，故按 target 豁免 `dead_code`（Windows 构建上照常检查）。
#![cfg_attr(
    not(target_os = "windows"),
    allow(dead_code, reason = "只有 Windows 实例路径（win_desktop）调用")
)]

/// 状态机识别的桌面。
///
/// **可挂目标是 [`Self::Default`]、[`Self::Winlogon`] 与 [`Self::ScreenSaver`]**；
/// 其余桌面（`Disconnect` / 未知名 / 空名）一律落到 [`Self::Unsupported`]——不挂，
/// 且保持当前挂载。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowDesktop {
    /// `Default`：普通用户桌面（登录后、未锁屏）。
    Default,
    /// `Winlogon`：**锁屏与登录界面**都在这里（本特性唯一必须拿下的桌面）。
    Winlogon,
    /// `Screen-saver`：屏保抢走输入时切换到的**安全桌面**（`WinSta0\Screen-saver`）。
    ///
    /// **必须挂**：屏保激活后输入桌面从这里离开 `Default`，而采集依赖「调用线程所在的
    /// 桌面」（GDI `BitBlt` 的作用域）。不挂的话线程留在**已不活动**的 `Default` 上，
    /// `BitBlt` 取不到有效画面（失败或阻塞），控制器就一直停在「等待画面…」；等用户
    /// 解锁（或用 ToDesk 登录）让 `Default` 重新活动，同一条线程下一拍就采到了，画面
    /// **突然出现**——这正是现场症状。屏保桌面同样是安全桌面（System 完整性），
    /// 也意味着它只有 SYSTEM 实例挂得上。
    ScreenSaver,
    /// 其余桌面（`Disconnect` / 未知名 / 空名）——**不是**挂载目标。
    Unsupported,
}

impl FollowDesktop {
    /// 稳定名（日志与报告用，别依赖 `Debug`）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "Default",
            Self::Winlogon => "Winlogon",
            Self::ScreenSaver => "Screen-saver",
            Self::Unsupported => "Unsupported",
        }
    }

    /// 是否是**会挂上去的**目标桌面。
    #[must_use]
    pub const fn is_attachable(self) -> bool {
        matches!(self, Self::Default | Self::Winlogon | Self::ScreenSaver)
    }
}

/// 按桌面名判定跟随目标（**大小写不敏感**，两端空白容忍，与
/// `win-platform` 的 `DesktopKind::from_name` 同规则）。
///
/// 名字来自 `GetUserObjectInformationW(UOI_NAME)`，只带桌面名、不带 `WinSta0\` 前缀。
#[must_use]
pub fn follow_of_name(name: &str) -> FollowDesktop {
    let name = name.trim();
    if name.eq_ignore_ascii_case("Default") {
        FollowDesktop::Default
    } else if name.eq_ignore_ascii_case("Winlogon") {
        FollowDesktop::Winlogon
    } else if name.eq_ignore_ascii_case("Screen-saver") || name.eq_ignore_ascii_case("Screensaver")
    {
        // 屏保桌面的名字在 Win32 里是 `Screen-saver`；不带连字符的写法也一并认下，
        // 与 `win-platform::DesktopKind::from_name` 保持同规则（分类结论不能因调用点而异）。
        FollowDesktop::ScreenSaver
    } else {
        FollowDesktop::Unsupported
    }
}

/// 一次跟随决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowAction {
    /// 维持现状。涵盖四种情形：探测失败（`observed == None`）、目标不可挂
    /// （[`FollowDesktop::Unsupported`]）、已经挂在目标上、以及尚无目标。
    NoChange,
    /// 应把线程切到该桌面（保证 `is_attachable()` 为真）。
    Attach(FollowDesktop),
}

/// 决定是否切换桌面。
///
/// `attached` 是**当前实际挂着**的桌面（`None` = 尚未挂载）；`observed` 是本轮探测到的
/// 输入桌面（`None` = 探测失败，例如非 SYSTEM 进程拿不到 `OpenInputDesktop`）。
///
/// # 为什么探测失败要保持现状
///
/// 探测失败（`observed == None`）不代表桌面变了——它更可能是**暂时的**失败（权限、
/// 竞态）。此时若去「切回 Default」就会在锁屏期间把线程从 Winlogon 踢走，锁屏立刻点不动。
/// 保持现状是唯一安全的选择。
#[must_use]
pub fn decide(attached: Option<FollowDesktop>, observed: Option<FollowDesktop>) -> FollowAction {
    match observed {
        None => FollowAction::NoChange,
        Some(target) if !target.is_attachable() => FollowAction::NoChange,
        Some(target) if attached == Some(target) => FollowAction::NoChange,
        Some(target) => FollowAction::Attach(target),
    }
}

#[cfg(test)]
mod tests {
    //! 表格驱动的状态机单测：**这台机器（macOS）上唯一能验证桌面跟随逻辑的地方**。

    use super::FollowDesktop::{Default, ScreenSaver, Unsupported, Winlogon};
    use super::*;

    #[test]
    fn follow_matrix_is_table_driven() {
        // (attached, observed, expected)
        let cases = [
            // 尚未挂载：见到可挂目标就挂。
            (None, Some(Default), FollowAction::Attach(Default)),
            (None, Some(Winlogon), FollowAction::Attach(Winlogon)),
            (None, Some(ScreenSaver), FollowAction::Attach(ScreenSaver)),
            // 不可挂目标 / 探测失败：保持现状（不挂）。
            (None, Some(Unsupported), FollowAction::NoChange),
            (None, None, FollowAction::NoChange),
            // 已挂在目标上：幂等，不重复切。
            (Some(Default), Some(Default), FollowAction::NoChange),
            (Some(Winlogon), Some(Winlogon), FollowAction::NoChange),
            (Some(ScreenSaver), Some(ScreenSaver), FollowAction::NoChange),
            // 桌面真的变了：切过去（锁屏 / 解锁 / 屏保进出，三个方向两两都要）。
            (Some(Default), Some(Winlogon), FollowAction::Attach(Winlogon)),
            (Some(Winlogon), Some(Default), FollowAction::Attach(Default)),
            (Some(Default), Some(ScreenSaver), FollowAction::Attach(ScreenSaver)),
            (Some(ScreenSaver), Some(Default), FollowAction::Attach(Default)),
            (Some(Winlogon), Some(ScreenSaver), FollowAction::Attach(ScreenSaver)),
            (Some(ScreenSaver), Some(Winlogon), FollowAction::Attach(Winlogon)),
            // 探测失败：绝不因为「探测不到」就把线程从 Winlogon 踢回 Default。
            (Some(Winlogon), None, FollowAction::NoChange),
            (Some(Default), None, FollowAction::NoChange),
            (Some(ScreenSaver), None, FollowAction::NoChange),
            // 输入桌面跑到不支持/不认识的桌面上（断开 / 未知名）：保持现状，不挂。
            (Some(Default), Some(Unsupported), FollowAction::NoChange),
            (Some(Winlogon), Some(Unsupported), FollowAction::NoChange),
            (Some(ScreenSaver), Some(Unsupported), FollowAction::NoChange),
        ];
        for (attached, observed, expected) in cases {
            assert_eq!(
                decide(attached, observed),
                expected,
                "attached={attached:?} observed={observed:?}"
            );
        }
    }

    /// 状态机一旦「挂上」，对同一观测再决策必然是 `NoChange`——否则每个轮询周期都会
    /// 重建一次线程（桌面变化是低频事件，重建不该变成常驻开销）。
    #[test]
    fn deciding_again_after_attach_is_a_no_op() {
        for target in [Default, Winlogon, ScreenSaver] {
            let action = decide(None, Some(target));
            assert_eq!(action, FollowAction::Attach(target));
            let FollowAction::Attach(target) = action else {
                unreachable!("上面刚断言过");
            };
            assert_eq!(
                decide(Some(target), Some(target)),
                FollowAction::NoChange,
                "{target:?} 挂上后不该再切"
            );
        }
    }

    /// `Attach` 的目标**必然**是可挂的桌面——否则 Windows 侧会去 `OpenInputDesktop`
    /// 挂一个不该挂的桌面。
    #[test]
    fn attach_targets_are_always_attachable() {
        for observed in [Default, Winlogon, ScreenSaver, Unsupported] {
            for attached in [None, Some(Default), Some(Winlogon), Some(ScreenSaver)] {
                if let FollowAction::Attach(target) = decide(attached, Some(observed)) {
                    assert!(target.is_attachable(), "{target:?} 不该被 Attach");
                }
            }
        }
    }

    #[test]
    fn name_classification_matches_win32_names() {
        assert_eq!(follow_of_name("Default"), Default);
        assert_eq!(follow_of_name("Winlogon"), Winlogon);
        assert_eq!(follow_of_name("Screen-saver"), ScreenSaver);
        assert_eq!(follow_of_name("Screensaver"), ScreenSaver);
        // 大小写与空白容忍。
        assert_eq!(follow_of_name("  winlogon\n"), Winlogon);
        assert_eq!(follow_of_name("default"), Default);
        assert_eq!(follow_of_name("\tSCREEN-SAVER "), ScreenSaver);
        // 其余一切（含空名）都不是挂载目标。
        for other in ["", "Disconnect", "SomeOther"] {
            assert_eq!(follow_of_name(other), Unsupported, "{other:?}");
        }
    }

    #[test]
    fn as_str_is_stable_and_round_trips_for_targets() {
        assert_eq!(FollowDesktop::Default.as_str(), "Default");
        assert_eq!(FollowDesktop::Winlogon.as_str(), "Winlogon");
        assert_eq!(FollowDesktop::ScreenSaver.as_str(), "Screen-saver");
        for target in [Default, Winlogon, ScreenSaver] {
            assert_eq!(follow_of_name(target.as_str()), target);
        }
    }
}
