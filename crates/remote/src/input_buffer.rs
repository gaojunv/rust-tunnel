//! 控制器侧输入缓冲：**按键/抬起永不丢，鼠标移动合并且可丢**。
//!
//! # 为什么需要它
//!
//! UI 线程原先把每个 [`InputEvent`] 直接投进会话命令通道（容量 256），通道满就 `try_send`
//! 静默丢弃。鼠标移动丢了无妨，但 `Key { pressed: false }` / `MouseButton { pressed: false }`
//! 是**有状态**的——丢一条抬起，远端就会永久按住那个键/鼠标键（用户看到的是「远端自己一直
//! 在打字」）。故这里按**事件类型**区分可丢性：
//!
//! - `MouseMove` 无状态：连续两条只保留最后一条（last-wins，光标只需要最新落点）；队列满时
//!   先挤掉**最旧**的一条移动。
//! - 其余事件（按键、鼠标键、滚轮）**永不丢弃**；队列里没有移动可挤时允许超界一条。
//!
//! 这套丢弃语义与 `crates/client-gui/src/remote/input_queue.rs`（被控端注入队列）一致，但
//! 本 crate **不能**依赖 `client-gui`（方向相反），故在这里保留控制器侧需要的精简版本。
//!
//! # 唤醒语义
//!
//! [`InputBuffer::push`] 由 UI 线程同步调用（可能不在 Tokio 运行时内），故内部用
//! `std::sync::Mutex` + [`tokio::sync::Notify`]。`Notify` **会存一个 permit**：消费者在
//! `has_pending()` 与 `notified()` 之间被抢占、生产者恰好在这时 push，唤醒也不会丢。
//!
//! # 顺序
//!
//! [`InputBuffer::drain`] 严格按入队顺序返回全部事件，因此「先移到某点、再按下」的先后关系
//! 不会被合并或挤占打乱（只可能少掉中间那些更早的移动落点）。

use crate::screen::InputEvent;
use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// 队列容量上限（对**全部**事件计数；满时只挤掉鼠标移动）。
///
/// 与旧命令通道的容量一致。按住不放的键/鼠标键最多几个，正常会话里队列远达不到这个深度；
/// 只有在写侧被链路背压卡住时它才有意义（那时它把最坏内存压在 256 条事件以内）。
pub(crate) const INPUT_BUFFER_CAPACITY: usize = 256;

/// 一条事件是否**可丢**（只有鼠标移动）。
#[must_use]
fn droppable(event: &InputEvent) -> bool {
    matches!(event, InputEvent::MouseMove { .. })
}

/// 队列里的一个事件 + 入队时刻（入队时刻只用于诊断：本地缓冲等待时长）。
struct Queued {
    /// 入队时刻（`Instant`，本进程单调）。
    at: Instant,
    /// 输入事件本身。
    event: InputEvent,
}

/// 控制器侧输入缓冲（`Send + Sync`，UI 线程与会话循环共享）。
pub(crate) struct InputBuffer {
    /// 容量上限（至少为 1，避免 0 容量把每条事件都判成「满」）。
    cap: usize,
    /// 待发事件（保持到达顺序）。
    events: Mutex<VecDeque<Queued>>,
    /// 有新事件时唤醒会话循环。
    notify: Notify,
}

impl InputBuffer {
    /// 建一个容量为 [`INPUT_BUFFER_CAPACITY`] 的缓冲。
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::with_capacity(INPUT_BUFFER_CAPACITY)
    }

    /// 建一个指定容量的缓冲（单测用小容量把「满」的路径逼出来）。
    #[must_use]
    fn with_capacity(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            events: Mutex::new(VecDeque::new()),
            notify: Notify::new(),
        }
    }

    /// 非阻塞入队并唤醒消费者。
    ///
    /// 满时只会挤掉 `MouseMove`（见模块文档）：**抬起事件绝不丢**，否则远端会永久卡键。
    pub(crate) fn push(&self, event: InputEvent) {
        let now = Instant::now();
        let mut queue = self.lock();
        // 连续两条移动合并成一条（last-wins）：移动无状态，中间的落点没有意义。
        if droppable(&event) {
            if let Some(back) = queue.back_mut() {
                if matches!(back.event, InputEvent::MouseMove { .. }) {
                    back.event = event;
                    back.at = now;
                    drop(queue);
                    self.notify.notify_one();
                    return;
                }
            }
        }
        if queue.len() < self.cap {
            queue.push_back(Queued { at: now, event });
        } else if let Some(index) = queue.iter().position(|queued| droppable(&queued.event)) {
            // 满：挤掉**最旧**的一条移动腾位置。挤最旧（而不是最新）是为了让光标尽快追上
            // 用户当下的位置。
            queue.remove(index);
            queue.push_back(Queued { at: now, event });
        } else if droppable(&event) {
            // 满、队列里一条移动都没有，来的也是可丢的移动：丢它，保留队列现状。
            return;
        } else {
            // 满、全是不可丢事件：为「抬起绝不丢」破例超界一条。现实中不该发生（队列只可能
            // 被鼠标移动填满），真发生了说明写侧长时间卡住，超界一条也好过丢一条抬起。
            queue.push_back(Queued { at: now, event });
        }
        drop(queue);
        self.notify.notify_one();
    }

    /// 取走全部待发事件（保持入队顺序）。
    ///
    /// 生产路径已改用 [`Self::drain_with_oldest_age`]（需要诊断读数）；本方法保留给单测，
    /// 避免测试为「不需要的等待时长」写样板。
    #[cfg(test)]
    #[must_use]
    pub(crate) fn drain(&self) -> Vec<InputEvent> {
        self.lock().drain(..).map(|queued| queued.event).collect()
    }

    /// 取走全部待发事件，并给出**最老一条**在本地缓冲里等了多久（诊断用）。
    ///
    /// 空队列时等待时长为 `None`。等待时长只进延迟日志（见 [`crate::latency`] 的
    /// `Stage::InputQueue`），不影响投递语义——与 [`Self::drain`] 完全同序、同内容。
    #[must_use]
    pub(crate) fn drain_with_oldest_age(&self) -> (Vec<InputEvent>, Option<Duration>) {
        let mut queue = self.lock();
        let oldest = queue.front().map(|queued| queued.at.elapsed());
        (queue.drain(..).map(|queued| queued.event).collect(), oldest)
    }

    /// 是否有待发事件（`select!` 分支的启用条件）。
    #[must_use]
    pub(crate) fn has_pending(&self) -> bool {
        !self.lock().is_empty()
    }

    /// 清空缓冲（会话收尾时调用，避免上一场的残留事件被发到下一场对端）。
    pub(crate) fn clear(&self) {
        self.lock().clear();
    }

    /// 等到有新事件入队（`Notify` 会存 permit，不会丢唤醒）。
    pub(crate) async fn notified(&self) {
        self.notify.notified().await;
    }

    /// 取队列锁；中毒时恢复内部数据（本模块不 panic，中毒只可能来自外部）。
    fn lock(&self) -> MutexGuard<'_, VecDeque<Queued>> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Default for InputBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    //! 表格驱动地锁住「哪些事件可丢、哪些绝不丢」以及 drain 的顺序。

    use super::*;
    use crate::screen::{KeyEvent, MouseButton};

    fn mv(x: i32) -> InputEvent {
        InputEvent::MouseMove {
            x,
            y: 0,
            display: 0,
        }
    }

    fn key(code: u32, pressed: bool) -> InputEvent {
        InputEvent::Key(KeyEvent {
            code,
            pressed,
            text: None,
            modifiers: 0,
        })
    }

    fn button(pressed: bool) -> InputEvent {
        InputEvent::MouseButton {
            button: MouseButton::Left,
            pressed,
        }
    }

    fn is_move(event: &InputEvent) -> bool {
        matches!(event, InputEvent::MouseMove { .. })
    }

    /// 连续两条移动只保留最后一条（last-wins）。
    #[test]
    fn consecutive_moves_merge_to_the_last_one() {
        let buffer = InputBuffer::new();
        buffer.push(mv(1));
        buffer.push(mv(2));
        buffer.push(mv(3));
        assert_eq!(buffer.drain(), vec![mv(3)]);
    }

    /// 按下/抬起与滚轮**既不被合并，也不会被丢弃**。
    #[test]
    fn durable_events_are_never_merged() {
        let buffer = InputBuffer::with_capacity(2);
        buffer.push(key(65, true));
        // 队列已满（全是不可丢事件）：第二条仍是不可丢事件 → 允许超界一条。
        buffer.push(key(65, true));
        buffer.push(button(false));
        let drained = buffer.drain();
        assert_eq!(drained.len(), 3, "不可丢事件不得被容量挤掉：{drained:?}");
        assert!(drained.iter().all(|e| !is_move(e)));
    }

    /// 满队列时挤掉**最旧**的一条移动，其他事件原样保留。
    #[test]
    fn a_full_queue_evicts_the_oldest_move() {
        let buffer = InputBuffer::with_capacity(4);
        buffer.push(key(17, true)); // Ctrl 按下
        buffer.push(mv(10));
        buffer.push(mv(11)); // 与上一条合并 → 队列变成 [Ctrl, move(11)]
        buffer.push(key(65, true));
        buffer.push(button(true));
        // 此刻 len = 4（已满），队列中唯一的移动是 move(11)。
        buffer.push(key(17, false)); // 抬起 Ctrl：必须进队列
        let drained = buffer.drain();
        assert_eq!(drained.len(), 4, "挤掉一条移动后仍应恰好满：{drained:?}");
        assert!(
            drained.contains(&key(17, false)),
            "抬起必须送达：{drained:?}"
        );
        assert!(
            !drained.iter().any(is_move),
            "被挤掉的应是那条移动，其余事件原样保留：{drained:?}"
        );
        assert_eq!(
            drained,
            vec![key(17, true), key(65, true), button(true), key(17, false)],
            "除被挤掉的移动外，其余事件的相对顺序必须不变"
        );
    }

    /// 满队列、没有移动可挤时，不可丢事件破例超界一条；可丢的移动则被丢弃。
    #[test]
    fn without_an_evictable_move_only_durable_events_overflow() {
        let buffer = InputBuffer::with_capacity(2);
        buffer.push(key(17, true));
        buffer.push(button(true));
        // 满且全是不可丢事件：移动被丢弃，队列保持一致。
        buffer.push(mv(1));
        assert_eq!(buffer.drain(), vec![key(17, true), button(true)]);

        let buffer = InputBuffer::with_capacity(2);
        buffer.push(key(17, true));
        buffer.push(button(true));
        // 不可丢事件：超界一条，绝不丢抬起。
        buffer.push(key(17, false));
        let drained = buffer.drain();
        assert_eq!(drained.len(), 3, "不可丢事件应超界一条：{drained:?}");
        assert!(drained.contains(&key(17, false)));
    }

    /// `drain` 保持「先移动、后按下/抬起」的先后关系。
    #[test]
    fn drain_preserves_move_then_press_order() {
        let buffer = InputBuffer::new();
        buffer.push(mv(5));
        buffer.push(button(true));
        buffer.push(button(false));
        assert_eq!(
            buffer.drain(),
            vec![mv(5), button(true), button(false)],
            "「移到某点 → 按下」的先后关系必须保住"
        );
        assert!(!buffer.has_pending(), "drain 之后队列应为空");
    }

    /// `push` 必须唤醒等待中的消费者，且 `has_pending` 随之变真。
    #[tokio::test]
    async fn push_wakes_the_consumer() {
        let buffer = std::sync::Arc::new(InputBuffer::new());
        let producer = std::sync::Arc::clone(&buffer);
        tokio::spawn(async move {
            producer.push(mv(7));
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), buffer.notified())
            .await
            .expect("push 应唤醒 notified");
        assert!(buffer.has_pending());
        assert_eq!(buffer.drain(), vec![mv(7)]);
    }

    /// `clear` 丢弃全部残留（会话收尾用）。
    #[test]
    fn clear_drops_pending_events() {
        let buffer = InputBuffer::new();
        buffer.push(mv(1));
        buffer.push(key(17, false));
        buffer.clear();
        assert!(!buffer.has_pending());
        assert!(buffer.drain().is_empty());
    }
}
