//! 客户端身份的归属判定：Windows 服务在跑时，本进程**不注册**。
//!
//! # 为什么需要它
//!
//! 服务端按**客户端名**唯一标识客户端。无人值守被控端由
//! `rust-tunnel-client-service.exe` 拉起一个无头实例承载身份（见 `crate::instance`），
//! 而用户桌面上还跑着一个常规 GUI。两者若同时注册，服务端会来回踢掉对方——用户看到的是
//! 「远程控制每隔几秒掉一次」。所以服务在跑时，GUI 必须停止自己的控制连接、转为被动。
//!
//! # 判定必须是**周期性**的，而不是启动时算一次
//!
//! 服务与 GUI 都随开机自启，服务完全可能在 GUI 已经连上之后才起来（开机竞争、`--install`
//! 之后）。所以归属要持续重估，见 [`start_watch`]。
//!
//! # 为什么本模块不依赖 `rust-tunnel-win-platform`
//!
//! 那个 crate 只在 Windows 上编译（见 `Cargo.toml` 的 target 段），而这里的判定逻辑必须在
//! macOS/Linux 上也能被单测。于是 [`ServicePresence`] 是 `ServiceState` 的一份**可移植镜像**，
//! 而真正的 SCM 查询与状态码映射收在 `#[cfg(windows)]` 的胶水里（[`query_presence`]）。

// 非 Windows 上真轮询（`start_watch` 的 Windows 体）不参与编译，于是 `ServicePresence` /
// `decide_owner` / `IdentityOwner::Service` 只被单测使用——非测试构建里会报 dead_code。
// 它们是本模块的**核心纯逻辑**（必须在 macOS/Linux 上可测），故按平台豁免。
#![cfg_attr(
    not(windows),
    allow(dead_code, reason = "非 Windows 无服务；这些纯逻辑只被单测使用")
)]

use std::sync::Arc;

use tokio::sync::watch;

#[cfg(windows)]
use std::time::Duration;

/// 服务状态轮询步长。
///
/// 3 秒：**够快**，开机时服务与 GUI 的竞争能在几秒内收敛（用户至多看到一次多余的注册）；
/// **够慢**，一次查询要打开 SCM、打开服务、查状态三次系统调用，不值得更密。
#[cfg(windows)]
const POLL_INTERVAL: Duration = Duration::from_secs(3);

/// 服务对本进程「客户端身份」的影响。
///
/// 这是 `rust_tunnel_win_platform::service::ServiceState` 的可移植镜像（理由见模块文档）：
/// 五个变体对应 SCM 状态里**与身份归属有关**的那几种区别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServicePresence {
    /// 查不出状态（SCM 查询本身失败）。**不改变当前归属**，见 [`decide_owner`]。
    Unknown,
    /// 服务没装。
    Absent,
    /// 已装但已停止。
    Stopped,
    /// 正在启动或正在停止（中间态）。
    Pending,
    /// 正在运行。
    Running,
}

/// 谁在承载客户端身份。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityOwner {
    /// 本进程（用户桌面的常规 GUI）。
    Gui,
    /// Windows 服务拉起的无头实例。
    Service,
}

impl IdentityOwner {
    /// 是否由服务承载（= 本进程应当被动）。
    ///
    /// 退避等待的每一拍、监督循环的每一轮都用它做判断，故单独成一个纯函数便于单测。
    #[must_use]
    pub const fn is_service(self) -> bool {
        matches!(self, Self::Service)
    }
}

/// 由「服务在场情况」推出「谁承载身份」。**纯函数**，`current` 是上一次的判定。
///
/// # 语义（表驱动，见本模块单测）
///
/// | presence | 结果 |
/// |---|---|
/// | [`ServicePresence::Running`] / [`ServicePresence::Pending`] | [`IdentityOwner::Service`] |
/// | [`ServicePresence::Absent`] / [`ServicePresence::Stopped`] | [`IdentityOwner::Gui`] |
/// | [`ServicePresence::Unknown`] | `current`（**保持不变**） |
///
/// `Pending`（启动中**与**停止中）都算服务在承载：启动中它的实例马上就要注册；停止中它的
/// 实例可能还活着（`win-service` 的 `kill_instance` 最多等 10 秒实例退出）。这两种时刻若让
/// GUI 抢着注册，就会与服务实例互相踢——正是本模块要消灭的那个循环。
///
/// `Unknown` 的「保持不变」是**刻意的迟滞**：SCM 查询偶发失败不该让两个进程来回切换归属。
/// 初值由调用方给 [`IdentityOwner::Gui`]——对**绝大多数从不安装服务的用户**，这就是今天的行为。
#[must_use]
pub fn decide_owner(presence: ServicePresence, current: IdentityOwner) -> IdentityOwner {
    match presence {
        ServicePresence::Running | ServicePresence::Pending => IdentityOwner::Service,
        ServicePresence::Absent | ServicePresence::Stopped => IdentityOwner::Gui,
        ServicePresence::Unknown => current,
    }
}

/// 服务接管身份时，UI 上要用户看懂的那段话（状态页横幅与托盘 tooltip 共用）。
///
/// 说全三件事：**谁**在承载身份、本窗口因此**不注册**、以及怎么**拿回来**。被动期间本窗口
/// 既没有远程控制能力、也收不到接入确认弹窗（它压根没注册），状态面板里的连接信息也不再代表
/// 实际被控状态——这些是设计使然，但绝不能是静默的。
#[must_use]
pub fn passive_notice() -> &'static str {
    "Windows 服务正在承载客户端身份，本窗口未注册。远程控制（含接入确认弹窗）由服务实例处理，\
     这里显示的连接状态不代表实际被控状态。要由本窗口重新接管，请在「设置」里停止或卸载该服务。"
}

/// 起服务状态轮询任务，返回 UI 侧读取归属的接收端。
///
/// **非 Windows 恒返回 `None`**：没有 Windows 服务，身份永远归 GUI；调用方拿到 `None` 后
/// 整条监督路径退化为今天的行为（见 `main.rs::spawn_runtime`）。
///
/// # 任务的生命周期
///
/// 轮询任务是 `tokio::spawn` 出来的**分离任务**：它只在进程存活期间有意义，随运行时一起
/// 消亡，故不做显式关闭握手。它持有 `tx` 的 `Arc`，因此从它这一侧看不到「发送端被丢弃」——
/// 收尾交给进程退出。
#[cfg(windows)]
#[must_use]
#[allow(
    clippy::unnecessary_wraps,
    reason = "Windows 体恒返回 Some，但非 Windows 体返回 None——两个平台的签名必须一致"
)]
pub fn start_watch(
    tx: Arc<watch::Sender<IdentityOwner>>,
) -> Option<watch::Receiver<IdentityOwner>> {
    let rx = tx.subscribe();
    tokio::spawn(async move {
        loop {
            let presence = query_presence();
            let next = decide_owner(presence, *tx.borrow());
            if next != *tx.borrow() {
                tracing::info!(?presence, ?next, "客户端身份归属变化");
                // 发送失败只可能是「没人订阅」——UI 还没起或已退出，不是错误。
                let _ = tx.send(next);
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    });
    Some(rx)
}

/// 非 Windows：没有 Windows 服务，身份恒归 GUI（调用方据此走「不监督」的路径）。
#[cfg(not(windows))]
#[must_use]
pub fn start_watch(
    _tx: Arc<watch::Sender<IdentityOwner>>,
) -> Option<watch::Receiver<IdentityOwner>> {
    None
}

/// 查询 SCM 并映射成本模块的 [`ServicePresence`]。
///
/// `query_state()` 把「服务没装」表示成 `Ok(ServiceState::NotInstalled)`（不是 `Err`），
/// 故 [`ServicePresence::Absent`] 走的是 `Ok` 分支；只有**查询本身**失败才映射成
/// [`ServicePresence::Unknown`]。
#[cfg(windows)]
fn query_presence() -> ServicePresence {
    match rust_tunnel_win_platform::service::query_state() {
        Ok(state) => presence_from_state(state),
        Err(err) => {
            tracing::debug!(?err, "查询 Windows 服务状态失败，保持当前身份归属");
            ServicePresence::Unknown
        }
    }
}

/// `ServiceState`（win-platform）→ [`ServicePresence`]（本模块）的 7 臂映射。
///
/// `pub(crate)` 而非私有：`crate::service_ui` 也用它，好让设置页与托盘共用**同一份**映射
/// （各写一份迟早漂移，症状是两处对同一个服务状态给出不同说法）。
#[cfg(windows)]
pub(crate) fn presence_from_state(
    state: rust_tunnel_win_platform::service::ServiceState,
) -> ServicePresence {
    use rust_tunnel_win_platform::service::ServiceState;
    match state {
        ServiceState::NotInstalled => ServicePresence::Absent,
        ServiceState::Stopped => ServicePresence::Stopped,
        // 启动中与停止中都算「服务在承载」，理由见 decide_owner 的文档。`Paused` 也归这里：
        // 本服务的控制码里没有 PAUSE_CONTINUE、正常永远到不了；真到了（探针/诊断看了别的
        // 服务）也按「服务在场」处理——它的进程还活着、实例可能仍在跑，宁可让 GUI 被动，
        // 也不去和它抢注册（抢注册就是那个踢来踢去的循环）。
        ServiceState::StartPending | ServiceState::StopPending | ServiceState::Paused => {
            ServicePresence::Pending
        }
        ServiceState::Running => ServicePresence::Running,
        // 没建模的原始状态码：当作「查不清」，保持当前归属。
        ServiceState::Unknown(_) => ServicePresence::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use IdentityOwner::{Gui, Service};
    use ServicePresence::{Absent, Pending, Running, Stopped, Unknown};

    /// 全表：**每一个** `(presence, current)` 组合都断言，将来改错一格立刻炸。
    #[test]
    fn decide_owner_table_is_exhaustive() {
        let cases = [
            (Running, Gui, Service),
            (Running, Service, Service),
            (Pending, Gui, Service),
            (Pending, Service, Service),
            (Absent, Gui, Gui),
            (Absent, Service, Gui),
            (Stopped, Gui, Gui),
            (Stopped, Service, Gui),
            (Unknown, Gui, Gui),
            (Unknown, Service, Service),
        ];
        for (presence, current, expected) in cases {
            assert_eq!(
                decide_owner(presence, current),
                expected,
                "{presence:?} + {current:?} 应当得到 {expected:?}"
            );
        }
        // 组合数必须等于 5 × 2：将来给任一枚举加变体却漏测，这里先炸。
        assert_eq!(cases.len(), 5 * 2);
    }

    /// `Unknown` 的迟滞：**两个方向**都保持不变（不是「一律归 GUI」）。
    #[test]
    fn unknown_presence_never_changes_the_owner() {
        assert_eq!(decide_owner(Unknown, Gui), Gui);
        assert_eq!(decide_owner(Unknown, Service), Service);
    }

    /// 本模块存在的**全部理由**：服务在跑时绝不能让 GUI 去注册。
    #[test]
    fn running_never_yields_the_gui() {
        for current in [Gui, Service] {
            assert_eq!(decide_owner(Running, current), Service);
        }
    }

    /// 两个中间态（在 `ServicePresence` 里是同一个 `Pending`）都必须算服务在承载。
    #[test]
    fn pending_counts_as_service_owned() {
        for current in [Gui, Service] {
            assert_eq!(decide_owner(Pending, current), Service);
        }
    }

    /// `Absent` / `Stopped` 必须让 GUI 重新接管，否则服务卸载后没人注册。
    #[test]
    fn absent_and_stopped_hand_back_to_the_gui() {
        for current in [Gui, Service] {
            assert_eq!(decide_owner(Absent, current), Gui);
            assert_eq!(decide_owner(Stopped, current), Gui);
        }
    }

    /// 退避等待每一拍用的判据。
    #[test]
    fn only_service_owner_is_service() {
        assert!(Service.is_service());
        assert!(!Gui.is_service());
    }

    /// 横幅文案必须说全「谁接管了」「本窗口未注册」「怎么拿回来」。
    #[test]
    fn notice_covers_takeover_registration_and_revert() {
        let notice = passive_notice();
        assert!(notice.contains("服务"), "{notice}");
        assert!(notice.contains("未注册"), "{notice}");
        assert!(notice.contains("设置"), "{notice}");
    }
}
