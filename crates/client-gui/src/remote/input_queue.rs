//! 输入注入队列与「按下集合」（**纯逻辑，无任何 Win32 依赖**）。
//!
//! # 为什么需要队列
//!
//! 注入跑在实例的专用桌面线程上（采集与注入必须同一条挂在目标桌面上的线程），而事件来自
//! 会话循环的 async worker。`crates/remote/src/controlled.rs` 明确假设 `InputSink::apply`
//! 是**微秒级**操作、直接留在 async worker 上（为它付一次 `spawn_blocking` 不划算），
//! 所以投递**绝不能阻塞**那个 worker——否则会饿死同一 worker 上的控制通道读循环与心跳。
//! 于是：非阻塞的有界队列 + 专用线程消费。
//!
//! # 硬约束：抬起事件绝不丢
//!
//! 队列满时**只能丢 `MouseMove`**。鼠标移动无状态，丢掉只是光标少经过一个中间点；而
//! `Key{pressed:false}` / `MouseButton{pressed:false}` 是**有状态**的：丢一条抬起，
//! 远端就永久按住那个键/鼠标键（用户看到的是「远端自己一直在打字」）。故满时先挤掉
//! **最旧的一条** `MouseMove` 腾位置——挤最旧的（而不是最新的）是为了让光标尽快追上
//! 用户当下的位置。
//!
//! # 与控制器侧 `InputGate` 的关系
//!
//! `super::InputGate` 是**控制器**侧的同源逻辑（按下门控、抬起无条件转达、退出时
//! `release_all`）。本模块是**被控端**侧的那一半：门控在控制器做完，这里只负责
//! 「不丢抬起」与「切换桌面/会话结束时补发全部抬起」（见 [`HeldInputs`]）。

// 唯一的调用方是 Windows 专用的 `remote::win_desktop`；在非 Windows 构建里这些纯逻辑
// 只有测试用得到，故按 target 豁免 `dead_code`（Windows 构建上照常检查）。
#![cfg_attr(
    not(target_os = "windows"),
    allow(dead_code, reason = "只有 Windows 实例路径（win_desktop）调用")
)]

use std::collections::{BTreeSet, VecDeque};

use rust_tunnel_remote::{InputEvent, KeyEvent, MouseButton};

use super::keycode;

/// 事件是否**可丢**。只有鼠标移动可丢（见模块文档）。
#[must_use]
fn droppable(event: &InputEvent) -> bool {
    matches!(event, InputEvent::MouseMove { .. })
}

/// 一次入队的结果（供日志/统计，也把「挤掉了什么」显式化）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    /// 直接入队。
    Queued,
    /// 队列满，挤掉了一条 `MouseMove` 后入队。
    EvictedMove,
    /// 队列满、无 `MouseMove` 可挤，而来的是可丢的 `MouseMove`：丢弃它（队列保持原样）。
    DroppedMove,
    /// 队列满、无 `MouseMove` 可挤，而来的是**不可丢**事件：为「抬起绝不丢」破例超界一条。
    ///
    /// 现实中不该发生（队列只可能被鼠标移动填满——按住不放的键/鼠标键最多几个）。
    /// 真发生了说明注入侧长时间卡住；此时超界一条总好过丢一条抬起让远端卡键。
    Overflowed,
}

/// 有界输入队列：非阻塞投递，满时只丢 [`PushOutcome::EvictedMove`] / [`PushOutcome::DroppedMove`]。
#[derive(Debug)]
pub struct InputQueue {
    /// 容量上限（对**全部**事件计数，但满时只会挤掉 `MouseMove`）。
    cap: usize,
    /// 待发事件（保持到达顺序——「移到某点 → 按下」的先后关系必须保住）。
    events: VecDeque<InputEvent>,
}

impl InputQueue {
    /// 建一个容量为 `cap` 的队列（`cap` 至少为 1，避免 0 容量把每条事件都判成满）。
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            events: VecDeque::new(),
        }
    }

    /// 是否没有待发事件。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// 非阻塞入队。返回值说明发生了什么（见 [`PushOutcome`]）。
    pub fn push(&mut self, event: InputEvent) -> PushOutcome {
        if self.events.len() < self.cap {
            self.events.push_back(event);
            return PushOutcome::Queued;
        }
        // 满：先挤掉**最旧**的一条 MouseMove 腾位置（保最新的移动，见模块文档）。
        if let Some(idx) = self.events.iter().position(droppable) {
            self.events.remove(idx);
            self.events.push_back(event);
            return PushOutcome::EvictedMove;
        }
        // 队列里一条 MouseMove 都没有。
        if droppable(&event) {
            // 来的也是可丢的：丢它，保留队列现状。
            return PushOutcome::DroppedMove;
        }
        // 不可丢事件 + 队列里全是不可丢事件：破例超界（见 [`PushOutcome::Overflowed`]）。
        self.events.push_back(event);
        PushOutcome::Overflowed
    }

    /// 取走全部待发事件（保持顺序）。
    pub fn drain(&mut self) -> Vec<InputEvent> {
        self.events.drain(..).collect()
    }
}

/// 鼠标键 → 稳定下标（用于 [`HeldInputs`] 的集合）。
#[must_use]
fn button_index(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
    }
}

/// 稳定下标 → 鼠标键。
#[must_use]
fn button_of(index: u8) -> MouseButton {
    match index {
        1 => MouseButton::Right,
        2 => MouseButton::Middle,
        _ => MouseButton::Left,
    }
}

/// 已注入且**仍被认为按下**的键 / 鼠标键。
///
/// # 为什么需要它
///
/// 桌面切换（锁屏↔解锁）会**销毁并重建**采集/注入线程；会话被硬切断（进程被杀、网络断）
/// 时控制器侧的 `InputGate::release_all` 补发的抬起事件也到不了。这两种情况下被控端若正
/// 按住某个键/鼠标键，远端（以及本机桌面）就会**永久**卡住它。故这里跟踪按下集合，
/// 在切换/结束时补发一次「全部抬起」。
///
/// 只跟踪**有配对抬起**的事件：`InputEvent::Key` 里 `code != TEXT` 的特殊键与修饰键，
/// 以及鼠标键。文本键（[`keycode::TEXT`]）是「点按」语义（注入侧展开成按下+抬起），
/// 没有配对抬起，不进集合。
#[derive(Debug, Default)]
pub struct HeldInputs {
    /// 跨平台键码（只含特殊键/修饰键）。
    keys: BTreeSet<u32>,
    /// 鼠标键下标（见 [`button_index`]）。
    buttons: BTreeSet<u8>,
}

impl HeldInputs {
    /// 按一条**已注入**的事件更新按下集合。
    pub fn observe(&mut self, event: &InputEvent) {
        match event {
            InputEvent::Key(KeyEvent { code, pressed, .. }) if *code != keycode::TEXT => {
                if *pressed {
                    self.keys.insert(*code);
                } else {
                    self.keys.remove(code);
                }
            }
            InputEvent::MouseButton { button, pressed } => {
                let idx = button_index(*button);
                if *pressed {
                    self.buttons.insert(idx);
                } else {
                    self.buttons.remove(&idx);
                }
            }
            _ => {}
        }
    }

    /// 是否有按住不放的项。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.buttons.is_empty()
    }

    /// 生成「全部抬起」事件序列并清空状态。
    ///
    /// 先抬鼠标键再抬键盘（顺序无所谓，只是固定下来便于测试）。**幂等**：没有按住项时
    /// 返回空，故可以在切换/结束时无条件调用。
    pub fn release_all(&mut self) -> Vec<InputEvent> {
        let mut out = Vec::new();
        for idx in std::mem::take(&mut self.buttons) {
            out.push(InputEvent::MouseButton {
                button: button_of(idx),
                pressed: false,
            });
        }
        for code in std::mem::take(&mut self.keys) {
            out.push(InputEvent::Key(KeyEvent {
                code,
                pressed: false,
                text: None,
                modifiers: 0,
            }));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    //! 表格驱动地锁住「哪些事件可丢、哪些绝不丢」。

    use super::*;

    fn mv(x: i32) -> InputEvent {
        InputEvent::MouseMove {
            x,
            y: 0,
            display: 0,
        }
    }

    fn key_down(code: u32) -> InputEvent {
        InputEvent::Key(KeyEvent {
            code,
            pressed: true,
            text: None,
            modifiers: 0,
        })
    }

    fn key_up(code: u32) -> InputEvent {
        InputEvent::Key(KeyEvent {
            code,
            pressed: false,
            text: None,
            modifiers: 0,
        })
    }

    fn btn(button: MouseButton, pressed: bool) -> InputEvent {
        InputEvent::MouseButton { button, pressed }
    }

    fn is_move(e: &InputEvent) -> bool {
        matches!(e, InputEvent::MouseMove { .. })
    }

    // ── 容量与丢弃策略 ──────────────────────────────────────────────

    #[test]
    fn moves_fill_the_queue_and_the_newest_survives() {
        let mut q = InputQueue::new(4);
        for i in 0..10 {
            q.push(mv(i));
        }
        let drained = q.drain();
        assert_eq!(drained.len(), 4, "队列不得越过容量上限");
        // 保最新的移动：挤掉的都是最旧的。
        assert_eq!(drained, vec![mv(6), mv(7), mv(8), mv(9)], "应当保留最新四条移动");
    }

    #[test]
    fn a_move_is_dropped_when_the_queue_is_full() {
        let mut q = InputQueue::new(2);
        assert_eq!(q.push(mv(1)), PushOutcome::Queued);
        assert_eq!(q.push(mv(2)), PushOutcome::Queued);
        // 满：来的也是可丢的移动 → 丢它，挤掉最旧的一条。
        assert_eq!(q.push(mv(3)), PushOutcome::EvictedMove);
        assert_eq!(q.drain(), vec![mv(2), mv(3)]);
    }

    // ── 硬约束：抬起绝不丢 ──────────────────────────────────────────

    #[test]
    fn a_durable_event_evicts_a_move_to_get_in() {
        let mut q = InputQueue::new(2);
        q.push(mv(1));
        q.push(mv(2));
        // 队列满且全是移动：抬起必须挤掉一条移动进来。
        assert_eq!(q.push(key_up(keycode::ENTER)), PushOutcome::EvictedMove);
        let drained = q.drain();
        assert!(
            drained.contains(&key_up(keycode::ENTER)),
            "抬起必须送达：{drained:?}"
        );
        assert_eq!(drained.len(), 2);
    }

    /// 表格驱动的核心不变量：**任何**不可丢事件在满队列下都必须最终出现在待发序列里，
    /// 而被牺牲的只能是 `MouseMove`。
    #[test]
    fn no_release_is_ever_lost_while_moves_are_droppable() {
        let durable = [
            key_down(keycode::CTRL),
            key_up(keycode::CTRL),
            key_down(keycode::ARROW_UP),
            key_up(keycode::ARROW_UP),
            btn(MouseButton::Left, true),
            btn(MouseButton::Left, false),
            btn(MouseButton::Right, false),
            InputEvent::Wheel { dx: 0, dy: -3 },
        ];
        for event in &durable {
            let mut q = InputQueue::new(4);
            // 先用移动把队列灌满。
            for i in 0..4 {
                q.push(mv(i));
            }
            let outcome = q.push(event.clone());
            assert_ne!(
                outcome,
                PushOutcome::DroppedMove,
                "{event:?} 是不可丢事件，不该被当成移动丢掉"
            );
            let drained = q.drain();
            assert!(drained.contains(event), "{event:?} 必须送达：{drained:?}");
            assert_eq!(drained.len(), 4, "挤掉一条移动后仍应恰好满：{drained:?}");
            assert!(
                drained.iter().filter(|e| is_move(e)).count() == 3,
                "只该挤掉一条移动：{drained:?}"
            );
        }
    }

    /// 极端情形：队列里一条移动都没有（全是不可丢事件）时，为「抬起绝不丢」破例超界。
    #[test]
    fn durable_events_overflow_rather_than_drop_a_release() {
        let mut q = InputQueue::new(2);
        assert_eq!(q.push(key_down(keycode::CTRL)), PushOutcome::Queued);
        assert_eq!(q.push(key_down(keycode::SHIFT)), PushOutcome::Queued);
        // 满且无可丢项：破例超界，绝不丢这条抬起。
        assert_eq!(q.push(key_up(keycode::CTRL)), PushOutcome::Overflowed);
        let drained = q.drain();
        assert!(drained.contains(&key_up(keycode::CTRL)), "{drained:?}");
        assert_eq!(drained.len(), 3);
    }

    /// 零容量被规整为 1，不会把每条事件都判成「满」。
    #[test]
    fn zero_capacity_is_normalised_to_one() {
        let mut q = InputQueue::new(0);
        assert_eq!(q.push(mv(1)), PushOutcome::Queued);
        assert_eq!(q.push(key_up(keycode::ENTER)), PushOutcome::EvictedMove);
        // 容量被规整为 1：抬起挤掉移动后只剩它自己。
        assert_eq!(q.drain(), vec![key_up(keycode::ENTER)]);
    }

    #[test]
    fn order_is_preserved_between_a_move_and_a_click() {
        let mut q = InputQueue::new(8);
        q.push(mv(5));
        q.push(btn(MouseButton::Left, true));
        q.push(btn(MouseButton::Left, false));
        assert_eq!(
            q.drain(),
            vec![
                mv(5),
                btn(MouseButton::Left, true),
                btn(MouseButton::Left, false),
            ],
            "「移到某点 → 按下」的先后关系必须保住"
        );
    }

    // ── HeldInputs：按下集合与「全部抬起」──────────────────────────

    #[test]
    fn held_inputs_track_press_and_release_per_key() {
        let mut held = HeldInputs::default();
        assert!(held.is_empty());
        held.observe(&key_down(keycode::CTRL));
        held.observe(&key_down(keycode::ARROW_UP));
        held.observe(&btn(MouseButton::Left, true));
        assert!(!held.is_empty());

        // 抬起其中一个：只剩另外两个。
        held.observe(&key_up(keycode::CTRL));
        let releases = held.release_all();
        assert!(releases.contains(&key_up(keycode::ARROW_UP)));
        assert!(releases.contains(&btn(MouseButton::Left, false)));
        assert!(
            !releases.contains(&key_up(keycode::CTRL)),
            "已经抬起过的键不该被重复补发：{releases:?}"
        );
        // 幂等：状态已清空。
        assert!(held.is_empty());
        assert!(held.release_all().is_empty());
    }

    /// 重复按下同一个键只记一次；抬起一次即清空（不会补发两条抬起）。
    #[test]
    fn repeated_press_of_the_same_key_is_a_set_not_a_list() {
        let mut held = HeldInputs::default();
        held.observe(&key_down(keycode::CTRL));
        held.observe(&key_down(keycode::CTRL));
        assert_eq!(held.release_all(), vec![key_up(keycode::CTRL)]);
    }

    /// 文本键（`TEXT`）是「点按」语义、没有配对抬起，不进按下集合。
    #[test]
    fn text_keys_are_not_tracked() {
        let mut held = HeldInputs::default();
        held.observe(&InputEvent::Key(KeyEvent {
            code: keycode::TEXT,
            pressed: true,
            text: Some("a".to_owned()),
            modifiers: 0,
        }));
        assert!(held.is_empty(), "文本键不该进按下集合");
        assert!(held.release_all().is_empty());
    }

    /// 鼠标三个键各自独立跟踪，且下标映射可往返。
    #[test]
    fn mouse_buttons_are_tracked_independently() {
        let mut held = HeldInputs::default();
        held.observe(&btn(MouseButton::Left, true));
        held.observe(&btn(MouseButton::Right, true));
        held.observe(&btn(MouseButton::Middle, true));
        let releases = held.release_all();
        for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
            assert!(releases.contains(&btn(button, false)), "{button:?}");
        }
        // 抬起一个再补发：只补发仍按住的那个。
        held.observe(&btn(MouseButton::Right, true));
        held.observe(&btn(MouseButton::Right, false));
        assert!(held.is_empty());
    }
}
