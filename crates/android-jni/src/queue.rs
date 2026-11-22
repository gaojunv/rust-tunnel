//! 渲染消息的有界 FIFO（平台无关的纯逻辑，主机上可单测）。
//!
//! # 为什么单独成模块
//!
//! 队列本体只做「分类 + 淘汰策略」，不碰 `Condvar`、`ANativeWindow` 或 EGL；渲染线程
//! （Android 目标）在它外面包一层 `Mutex + Condvar`。这样淘汰顺序、容量边界、关闭语义
//! 都能在主机上单测——真机只验证「有没有画出来」，查不出「丢错了哪一条」。
//!
//! # 淘汰策略
//!
//! - **控制消息**（设置 surface / 关闭）永不淘汰：丢了会让渲染目标错乱或线程不退出；
//! - **整帧**受 [`BoundedQueue::max_frames`] 约束，满则丢**最旧**的一帧：画面只关心最新；
//! - **补丁**受 [`BoundedQueue::max_patches`] 约束，满则丢最旧的补丁并**记录**（见下）。
//!
//! 补丁本身不可丢（要合成进持久帧缓冲），所以它的上限放得比整帧宽得多；触到上限只可能是
//! 渲染线程长时间停摆（退后台、驱动卡住），此时丢最旧的补丁与丢整帧一样只影响新旧，但
//! **必须能被日志看见**——调用方把 [`PushOutcome::evicted`] 里的消息记一条 warning 即可。

use std::collections::VecDeque;

/// 一条排队消息的类别（决定它受哪一个容量约束）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MsgKind {
    /// 控制消息（设置 surface、关闭）：永不淘汰。
    Control,
    /// 整帧画面：受 `max_frames` 约束，满则丢最旧。
    Frame,
    /// 脏矩形补丁：受 `max_patches` 约束，满则丢最旧。
    Patch,
}

/// 可入队的消息。
pub(crate) trait QueuedMsg {
    /// 本消息的类别（决定容量与淘汰策略）。
    fn kind(&self) -> MsgKind;
}

/// 一次投递的结果。
///
/// `accepted == false` 表示队列已关闭、消息被丢弃（调用方只能记日志）；
/// `evicted` 是为腾出位置而被淘汰的旧消息（调用方负责记日志，通常是最旧的那一条）。
#[derive(Debug)]
pub(crate) struct PushOutcome<T> {
    /// 本次消息是否已入队（`false` = 队列已关闭）。
    pub(crate) accepted: bool,
    /// 为腾出位置而被淘汰的旧消息（按淘汰顺序）。
    pub(crate) evicted: Vec<T>,
}

/// 有界 FIFO 队列（不含任何同步原语）。
#[derive(Debug)]
pub(crate) struct BoundedQueue<T> {
    /// 队列本体。
    msgs: VecDeque<T>,
    /// 是否已关闭（关闭后投递一律被拒，剩余消息仍可取出）。
    closed: bool,
    /// 整帧上限。
    max_frames: usize,
    /// 补丁上限。
    max_patches: usize,
}

impl<T> BoundedQueue<T> {
    /// 新建空队列。
    ///
    /// `max_frames` / `max_patches` 为 0 时按 1 处理（0 容量会让每一次投递都先淘汰再入队，
    /// 语义上等于「永远只留最新一条」，但显式兜底更不容易误读）。
    pub(crate) fn new(max_frames: usize, max_patches: usize) -> Self {
        Self {
            msgs: VecDeque::new(),
            closed: false,
            max_frames: max_frames.max(1),
            max_patches: max_patches.max(1),
        }
    }

    /// 当前消息数。
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.msgs.len()
    }

    /// 是否已关闭。
    pub(crate) fn is_closed(&self) -> bool {
        self.closed
    }

    /// 关闭队列：后续投递返回 `accepted: false`，已有的消息仍然可以被 [`Self::pop_front`] 取走。
    pub(crate) fn close(&mut self) {
        self.closed = true;
    }
}

impl<T: QueuedMsg> BoundedQueue<T> {
    /// 投递一条消息（不阻塞；满则淘汰同类的最旧消息）。
    pub(crate) fn push(&mut self, msg: T) -> PushOutcome<T> {
        if self.closed {
            return PushOutcome {
                accepted: false,
                evicted: Vec::new(),
            };
        }

        let (kind, limit) = match msg.kind() {
            MsgKind::Control => {
                self.msgs.push_back(msg);
                return PushOutcome {
                    accepted: true,
                    evicted: Vec::new(),
                };
            }
            MsgKind::Frame => (MsgKind::Frame, self.max_frames),
            MsgKind::Patch => (MsgKind::Patch, self.max_patches),
        };

        let mut evicted = Vec::new();
        while self.count(kind) >= limit {
            let Some(index) = self.msgs.iter().position(|queued| queued.kind() == kind) else {
                break;
            };
            if let Some(old) = self.msgs.remove(index) {
                evicted.push(old);
            }
        }
        self.msgs.push_back(msg);
        PushOutcome {
            accepted: true,
            evicted,
        }
    }

    /// 取一条消息（不阻塞）；队列为空时返回 `None`（关闭状态由 [`Self::is_closed`] 表达）。
    pub(crate) fn pop_front(&mut self) -> Option<T> {
        self.msgs.pop_front()
    }

    /// 某一类消息的当前数量。
    fn count(&self, kind: MsgKind) -> usize {
        self.msgs.iter().filter(|msg| msg.kind() == kind).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用消息：只带类别与编号，便于断言顺序。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct TestMsg {
        kind: MsgKind,
        id: u32,
    }

    impl TestMsg {
        fn frame(id: u32) -> Self {
            Self {
                kind: MsgKind::Frame,
                id,
            }
        }

        fn patch(id: u32) -> Self {
            Self {
                kind: MsgKind::Patch,
                id,
            }
        }

        fn control(id: u32) -> Self {
            Self {
                kind: MsgKind::Control,
                id,
            }
        }
    }

    impl QueuedMsg for TestMsg {
        fn kind(&self) -> MsgKind {
            self.kind
        }
    }

    /// FIFO 顺序：不淘汰时严格按投递顺序取出（补丁与整帧共享同一条队列）。
    #[test]
    fn pop_preserves_fifo_order_across_kinds() {
        let mut queue = BoundedQueue::new(4, 4);
        let pushed = [
            TestMsg::frame(1),
            TestMsg::patch(2),
            TestMsg::control(3),
            TestMsg::frame(4),
            TestMsg::patch(5),
        ];
        for msg in pushed {
            let outcome = queue.push(msg);
            assert!(outcome.accepted);
            assert!(outcome.evicted.is_empty());
        }
        let mut popped = Vec::new();
        while let Some(msg) = queue.pop_front() {
            popped.push(msg);
        }
        assert_eq!(popped, pushed.to_vec());
    }

    /// 整帧满时只淘汰**最旧的一帧**，补丁与控制消息原样保留（类别隔离）。
    #[test]
    fn frame_overflow_evicts_oldest_frame_only() {
        let mut queue = BoundedQueue::new(2, 4);
        assert!(queue.push(TestMsg::frame(1)).evicted.is_empty());
        assert!(queue.push(TestMsg::patch(10)).accepted);
        assert!(queue.push(TestMsg::frame(2)).evicted.is_empty());

        let outcome = queue.push(TestMsg::frame(3));
        assert!(outcome.accepted);
        assert_eq!(outcome.evicted, vec![TestMsg::frame(1)]);

        let mut popped = Vec::new();
        while let Some(msg) = queue.pop_front() {
            popped.push(msg);
        }
        // 补丁 10 仍在（它不被整帧的容量约束挤掉）。
        assert_eq!(
            popped,
            vec![TestMsg::patch(10), TestMsg::frame(2), TestMsg::frame(3)]
        );
    }

    /// 补丁满时淘汰最旧的补丁，且淘汰结果**返回给调用方记日志**（补丁丢了必须可见）。
    #[test]
    fn patch_overflow_evicts_oldest_patch_and_reports_it() {
        let mut queue = BoundedQueue::new(2, 2);
        assert!(queue.push(TestMsg::patch(1)).evicted.is_empty());
        assert!(queue.push(TestMsg::frame(100)).accepted);
        assert!(queue.push(TestMsg::patch(2)).evicted.is_empty());

        let outcome = queue.push(TestMsg::patch(3));
        assert_eq!(outcome.evicted, vec![TestMsg::patch(1)]);
        let mut popped = Vec::new();
        while let Some(msg) = queue.pop_front() {
            popped.push(msg);
        }
        assert_eq!(
            popped,
            vec![TestMsg::frame(100), TestMsg::patch(2), TestMsg::patch(3)]
        );
    }

    /// 控制消息永不被淘汰，也永远能入队（容量只约束帧/补丁）。
    #[test]
    fn control_messages_are_never_evicted() {
        let mut queue = BoundedQueue::new(1, 1);
        queue.push(TestMsg::control(1));
        queue.push(TestMsg::frame(2));
        queue.push(TestMsg::patch(3));
        let outcome = queue.push(TestMsg::control(4));
        assert!(outcome.evicted.is_empty());
        let outcome = queue.push(TestMsg::frame(5));
        assert_eq!(outcome.evicted, vec![TestMsg::frame(2)]);
        // 两个控制消息都还在。
        let mut popped = Vec::new();
        while let Some(msg) = queue.pop_front() {
            popped.push(msg);
        }
        assert!(popped.contains(&TestMsg::control(1)));
        assert!(popped.contains(&TestMsg::control(4)));
    }

    /// 关闭后投递被拒（消息被丢弃、不再增长），但已排队消息仍可取完。
    #[test]
    fn close_rejects_new_messages_but_drains_old_ones() {
        let mut queue = BoundedQueue::new(2, 2);
        queue.push(TestMsg::frame(1));
        queue.close();
        assert!(queue.is_closed());
        let outcome = queue.push(TestMsg::frame(2));
        assert!(!outcome.accepted);
        assert!(outcome.evicted.is_empty());
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.pop_front(), Some(TestMsg::frame(1)));
        assert_eq!(queue.pop_front(), None);
    }
}
