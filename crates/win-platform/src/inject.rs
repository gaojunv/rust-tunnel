//! 输入注入：把「要与不要」表达成步骤序列，再由 `SendInput` 落到当前桌面。
//!
//! # 为什么要有「步骤」这一层
//!
//! 探针阶段最大的不确定性是「**在 Winlogon 桌面上 `SendInput` 到底落不落地**」。
//! 要回答它，需要的是**可判定的证据**（发了几个事件、被接受了几个），而不是
//! 一个「好像点到了」的印象。所以这里把注入拆成两步：
//!
//! 1. **[`InjectStep`] 序列**：纯数据，可在 macOS 上单测（几何、顺序、边界）；
//! 2. **[`send_steps`]（Windows）**：逐步调 `SendInput`，**逐步核对返回值**。
//!
//! # UIPI 那个坑（探针报告必须能看出来）
//!
//! 被 UIPI 拦下时 `SendInput` 返回**插入的事件数**（比请求的少、常见是 0），
//! 但 `GetLastError()` 往往是 **0**（上一次调用的残留）。也就是说「看错误码」
//! 永远看不出被拦，**只有返回计数**才是判据。故 [`InjectOutcome`] 同时记
//! attempted 与 accepted，且 `first_failure` 里保留原始错误码（哪怕它是 0）。
//!
//! # 坐标
//!
//! `MOUSEEVENTF_ABSOLUTE` 吃的是 0..=65535 的归一化坐标，必须与
//! `MOUSEEVENTF_VIRTUALDESK` 一起用才是「整个虚拟桌面」的坐标系。换算在
//! [`crate::monitor::VirtualScreen::normalize_absolute`]。

use crate::WinError;

/// `MOUSEEVENTF_MOVE`。
pub const MOUSEEVENTF_MOVE: u32 = 0x0001;
/// `MOUSEEVENTF_LEFTDOWN`。
pub const MOUSEEVENTF_LEFTDOWN: u32 = 0x0002;
/// `MOUSEEVENTF_LEFTUP`。
pub const MOUSEEVENTF_LEFTUP: u32 = 0x0004;
/// `MOUSEEVENTF_RIGHTDOWN`。
pub const MOUSEEVENTF_RIGHTDOWN: u32 = 0x0008;
/// `MOUSEEVENTF_RIGHTUP`。
pub const MOUSEEVENTF_RIGHTUP: u32 = 0x0010;
/// `MOUSEEVENTF_MIDDLEDOWN`。
pub const MOUSEEVENTF_MIDDLEDOWN: u32 = 0x0020;
/// `MOUSEEVENTF_MIDDLEUP`。
pub const MOUSEEVENTF_MIDDLEUP: u32 = 0x0040;
/// `MOUSEEVENTF_WHEEL`。
pub const MOUSEEVENTF_WHEEL: u32 = 0x0800;
/// `MOUSEEVENTF_HWHEEL`：横向滚轮。
pub const MOUSEEVENTF_HWHEEL: u32 = 0x1000;
/// `MOUSEEVENTF_VIRTUALDESK`：把绝对坐标解释为整个虚拟桌面。
pub const MOUSEEVENTF_VIRTUALDESK: u32 = 0x4000;
/// `MOUSEEVENTF_ABSOLUTE`：坐标是归一化的绝对坐标而非相对位移。
pub const MOUSEEVENTF_ABSOLUTE: u32 = 0x8000;
/// `KEYEVENTF_KEYUP`：抬起事件。
pub const KEYEVENTF_KEYUP: u32 = 0x0002;
/// `KEYEVENTF_UNICODE`：`wScan` 里放的字面 UTF-16 码元（不受键盘布局影响）。
pub const KEYEVENTF_UNICODE: u32 = 0x0004;
/// `WHEEL_DELTA`：一格滚轮的量。
pub const WHEEL_DELTA: i32 = 120;
/// 空格键的虚拟键码（探针用来做「按键有没有落地」的实验，落在输入框里也只是个空格）。
pub const VK_SPACE: u16 = 0x20;

/// 鼠标按键。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectButton {
    /// 左键。
    Left,
    /// 右键。
    Right,
    /// 中键。
    Middle,
}

impl InjectButton {
    /// 按下时用的 `MOUSEEVENTF_*` 位。
    #[must_use]
    pub const fn down_flag(self) -> u32 {
        match self {
            Self::Left => MOUSEEVENTF_LEFTDOWN,
            Self::Right => MOUSEEVENTF_RIGHTDOWN,
            Self::Middle => MOUSEEVENTF_MIDDLEDOWN,
        }
    }

    /// 抬起时用的 `MOUSEEVENTF_*` 位。
    #[must_use]
    pub const fn up_flag(self) -> u32 {
        match self {
            Self::Left => MOUSEEVENTF_LEFTUP,
            Self::Right => MOUSEEVENTF_RIGHTUP,
            Self::Middle => MOUSEEVENTF_MIDDLEUP,
        }
    }
}

/// 一个注入步骤。**一步 = 一次 `SendInput` 调用**，因此失败能定位到具体某一步。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InjectStep {
    /// 把鼠标移到虚拟桌面坐标 `(x, y)`。
    MoveTo {
        /// 虚拟桌面坐标 x（可为负）。
        x: i32,
        /// 虚拟桌面坐标 y（可为负）。
        y: i32,
    },
    /// 按下或抬起某个鼠标键（位置不变）。
    Button {
        /// 哪个键。
        button: InjectButton,
        /// `true` = 按下，`false` = 抬起。
        pressed: bool,
    },
    /// 竖向滚轮。
    Wheel {
        /// 带符号的滚动量，正数向上，单位是 `WHEEL_DELTA`。
        delta: i32,
    },
    /// 横向滚轮（`MOUSEEVENTF_HWHEEL`）。
    ///
    /// 单独一个变体而不是复用 [`Self::Wheel`]：两者在 `SendInput` 里是不同的 `dwFlags`，
    /// 合在一起就得在数据里额外塞一个「哪个轴」的字段。阶段 1 的实例用它转发控制器的
    /// 横向滚动（`enigo` 的横向路径发的是 `MOUSEEVENTF_HWHEEL`）。
    HWheel {
        /// 带符号的滚动量，正数向右，单位是 `WHEEL_DELTA`。
        delta: i32,
    },
    /// 按下或抬起一个虚拟键码（**走扫描码表**，受当前键盘布局影响）。
    KeyVk {
        /// 虚拟键码。
        vk: u16,
        /// `true` = 按下，`false` = 抬起。
        pressed: bool,
    },
    /// 逐字符输入一段文本（`KEYEVENTF_UNICODE`，**不受键盘布局影响**）。
    ///
    /// 每个 UTF-16 码元会展开成「按下 + 抬起」两个事件。
    Text {
        /// 要输入的文本的 UTF-16 码元。
        utf16: Vec<u16>,
    },
}

/// 某一步失败的位置与原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InjectFailure {
    /// 是第几步（从 0 开始）。
    pub step_index: usize,
    /// 该步失败时的错误（**错误码可能是 0**，见模块文档的 UIPI 说明）。
    pub error: WinError,
}

/// 一次 [`send_steps`] 的结果。`attempted` 与 `accepted` 都对不上就是被拦了。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InjectOutcome {
    /// 尝试发送的步数。
    pub steps_attempted: usize,
    /// 被接受（该步的事件全部插入）的步数。
    pub steps_accepted: usize,
    /// 尝试发送的事件数。
    pub inputs_attempted: u32,
    /// 被系统实际插入的事件数。
    pub inputs_accepted: u32,
    /// 第一处失败（有的话）。
    pub first_failure: Option<InjectFailure>,
}

impl InjectOutcome {
    /// 所有步骤的所有事件都被接受。
    #[must_use]
    pub fn fully_accepted(&self) -> bool {
        self.first_failure.is_none()
            && self.steps_accepted == self.steps_attempted
            && self.inputs_accepted == self.inputs_attempted
    }

    /// 一行摘要（探针报告用）。
    #[must_use]
    pub fn summary(&self) -> String {
        let base = format!(
            "{}/{} 步、{}/{} 个事件被接受",
            self.steps_accepted, self.steps_attempted, self.inputs_accepted, self.inputs_attempted
        );
        match &self.first_failure {
            None => base,
            Some(f) => format!("{base}；第 {} 步起被拒（{}）", f.step_index, f.error),
        }
    }

    /// 记录一步被拒（内部用；`pub` 是为了让别的注入实现（阶段 1 的采集线程）复用）。
    pub fn record_failure(&mut self, step_index: usize, error: WinError) {
        if self.first_failure.is_none() {
            self.first_failure = Some(InjectFailure { step_index, error });
        }
    }
}

/// 规划一次「肉眼可见的鼠标扫动」：从画面宽度 1/8 处扫到 7/8 处，纵居中。
///
/// 用于探针：光标**真的会动**是「注入落地」最直观的证据。至少两个点，
/// 调用方在两点之间 sleep 几十毫秒就能看见。
#[must_use]
pub fn plan_sweep(vs: &crate::monitor::VirtualScreen, steps: u32) -> Vec<InjectStep> {
    let steps = steps.max(2);
    let w = i64::from(vs.width);
    let y = i64::from(vs.origin_y) + i64::from(vs.height) / 2;
    let x0 = i64::from(vs.origin_x) + w / 8;
    let x1 = i64::from(vs.origin_x) + w * 7 / 8;
    (0..steps)
        .map(|i| {
            let num = i64::from(i) * (x1 - x0);
            let den = i64::from(steps - 1).max(1);
            InjectStep::MoveTo {
                x: i32::try_from(x0 + num / den).unwrap_or(0),
                y: i32::try_from(y).unwrap_or(0),
            }
        })
        .collect()
}

/// 规划一次点击：移到目标 → 按下 → 抬起（三步，各自一次 `SendInput`）。
#[must_use]
pub fn plan_click(x: i32, y: i32, button: InjectButton) -> Vec<InjectStep> {
    vec![
        InjectStep::MoveTo { x, y },
        InjectStep::Button {
            button,
            pressed: true,
        },
        InjectStep::Button {
            button,
            pressed: false,
        },
    ]
}

/// 规划一次按键：按下 + 抬起。
#[must_use]
pub fn plan_key_press(vk: u16) -> Vec<InjectStep> {
    vec![
        InjectStep::KeyVk { vk, pressed: true },
        InjectStep::KeyVk { vk, pressed: false },
    ]
}

/// 规划一段文本输入（一个步骤，内部展开成逐字符的按下/抬起）。
#[must_use]
pub fn plan_text(text: &str) -> Vec<InjectStep> {
    vec![InjectStep::Text {
        utf16: text.encode_utf16().collect(),
    }]
}

/// 发送一串步骤，**逐步核对 `SendInput` 的返回值**，遇到第一处不符即停。
///
/// # 实现契约
///
/// 对每一步：
/// 1. 按 [`InjectStep`] 构造 `INPUT` 数组（鼠标事件用 `MOUSEINPUT`，键盘/文本用
///    `KEYBDINPUT`）；`MouseMove` 的 `dx/dy` 取
///    `vs.normalize_absolute(x, y)`，`dwFlags = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE
///    | MOUSEEVENTF_VIRTUALDESK`；
/// 2. `Wheel` 的 `mouseData` 是 `delta` 的 **u32 位模式**（`i32` → `u32` 不能写 `as`，
///    用 `u32::from_ne_bytes(delta.to_ne_bytes())`）；
/// 3. 调 `SendInput(count, inputs.as_ptr(), size_of::<INPUT>() as i32)`
///    （`cbSize` 必须是 `size_of::<INPUT>()`，传错会整个调用失败）；
/// 4. `attempted += count`，`accepted += 返回值`；返回值 < count 就
///    `record_failure(step, last_error("SendInput"))` 并**立即停止**（后面的步骤
///    会落到错误的状态上，比如只有按下没有抬起）。
///
/// **调用线程必须已经挂到目标桌面上**（见 [`crate::desktop::InputDesktop`]），
/// 否则事件会落到别的桌面——而且**不会报错**。
///
/// # Errors
///
/// 本函数只在「构造 INPUT 时就无法进行」的情况下返回 `Err`（当前实现不会），
/// 注入被拒本身通过 [`InjectOutcome`] 报告而不是 `Err`。
#[cfg(windows)]
pub fn send_steps(
    steps: &[InjectStep],
    vs: &crate::monitor::VirtualScreen,
) -> crate::WinResult<InjectOutcome> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT};

    let mut outcome = InjectOutcome::default();
    for (index, step) in steps.iter().enumerate() {
        let inputs = build_inputs(step, vs);
        let count = u32::try_from(inputs.len()).unwrap_or(0);
        outcome.steps_attempted += 1;
        outcome.inputs_attempted += count;
        // SAFETY: inputs 是本次调用期间存活的 INPUT 数组，count 与长度一致，
        // cbSize 用 size_of::<INPUT>() 如实告知（传错会整调用失败）。
        let sent = unsafe {
            SendInput(
                count,
                inputs.as_ptr(),
                i32::try_from(std::mem::size_of::<INPUT>()).unwrap_or(0),
            )
        };
        if sent == count {
            outcome.inputs_accepted += sent;
            outcome.steps_accepted += 1;
        } else {
            outcome.inputs_accepted += sent;
            outcome.record_failure(index, crate::error::last_error("SendInput"));
            break;
        }
    }
    Ok(outcome)
}

/// 把一步展开成 `INPUT` 数组。
#[cfg(windows)]
fn build_inputs(
    step: &InjectStep,
    vs: &crate::monitor::VirtualScreen,
) -> Vec<windows_sys::Win32::UI::Input::KeyboardAndMouse::INPUT> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, MOUSEINPUT,
    };

    let mouse = |dx: i32, dy: i32, mouse_data: u32, flags: u32| INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: mouse_data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let key = |vk: u16, scan: u16, flags: u32| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };

    match step {
        InjectStep::MoveTo { x, y } => {
            let (nx, ny) = vs.normalize_absolute(*x, *y);
            vec![mouse(
                nx,
                ny,
                0,
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            )]
        }
        InjectStep::Button { button, pressed } => {
            let flags = if *pressed {
                button.down_flag()
            } else {
                button.up_flag()
            };
            vec![mouse(0, 0, 0, flags)]
        }
        InjectStep::Wheel { delta } => vec![mouse(
            0,
            0,
            u32::from_ne_bytes(delta.to_ne_bytes()),
            MOUSEEVENTF_WHEEL,
        )],
        InjectStep::HWheel { delta } => vec![mouse(
            0,
            0,
            u32::from_ne_bytes(delta.to_ne_bytes()),
            MOUSEEVENTF_HWHEEL,
        )],
        InjectStep::KeyVk { vk, pressed } => {
            let flags = if *pressed { 0 } else { KEYEVENTF_KEYUP };
            vec![key(*vk, 0, flags)]
        }
        InjectStep::Text { utf16 } => {
            let mut out = Vec::with_capacity(utf16.len() * 2);
            for unit in utf16 {
                out.push(key(0, *unit, KEYEVENTF_UNICODE));
                out.push(key(0, *unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::VirtualScreen;

    fn vs() -> VirtualScreen {
        VirtualScreen {
            origin_x: 0,
            origin_y: 0,
            width: 1600,
            height: 900,
        }
    }

    #[test]
    fn button_flags_match_the_win32_pairs() {
        assert_eq!(InjectButton::Left.down_flag(), 0x0002);
        assert_eq!(InjectButton::Left.up_flag(), 0x0004);
        assert_eq!(InjectButton::Right.down_flag(), 0x0008);
        assert_eq!(InjectButton::Right.up_flag(), 0x0010);
        assert_eq!(InjectButton::Middle.down_flag(), 0x0020);
        assert_eq!(InjectButton::Middle.up_flag(), 0x0040);
    }

    #[test]
    fn click_is_move_then_down_then_up() {
        let steps = plan_click(100, 200, InjectButton::Left);
        assert_eq!(
            steps,
            vec![
                InjectStep::MoveTo { x: 100, y: 200 },
                InjectStep::Button {
                    button: InjectButton::Left,
                    pressed: true
                },
                InjectStep::Button {
                    button: InjectButton::Left,
                    pressed: false
                },
            ]
        );
    }

    #[test]
    fn key_press_is_down_then_up() {
        assert_eq!(
            plan_key_press(VK_SPACE),
            vec![
                InjectStep::KeyVk {
                    vk: VK_SPACE,
                    pressed: true
                },
                InjectStep::KeyVk {
                    vk: VK_SPACE,
                    pressed: false
                },
            ]
        );
        assert_eq!(VK_SPACE, 0x20, "空格的虚拟键码是 0x20");
    }

    #[test]
    fn sweep_spans_from_one_eighth_to_seven_eighths_on_the_middle_row() {
        let steps = plan_sweep(&vs(), 8);
        assert_eq!(steps.len(), 8);
        let xs: Vec<i32> = steps
            .iter()
            .map(|s| match s {
                InjectStep::MoveTo { x, .. } => *x,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(xs.first(), Some(&200), "1600/8 = 200");
        assert_eq!(xs.last(), Some(&1400), "1600*7/8 = 1400");
        assert!(xs.windows(2).all(|w| w[0] < w[1]), "必须单调递增：{xs:?}");
        for step in &steps {
            match step {
                InjectStep::MoveTo { y, .. } => assert_eq!(*y, 450, "纵居中"),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn sweep_honours_a_negative_virtual_origin() {
        let vs = VirtualScreen {
            origin_x: -1280,
            origin_y: -200,
            width: 3200,
            height: 1280,
        };
        let steps = plan_sweep(&vs, 4);
        match steps.first() {
            Some(InjectStep::MoveTo { x, y }) => {
                assert_eq!(*x, -1280 + 400);
                assert_eq!(*y, -200 + 640);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn sweep_always_produces_at_least_two_points() {
        // 一个点看不出「在动」，故最少两个。
        assert_eq!(plan_sweep(&vs(), 0).len(), 2);
        assert_eq!(plan_sweep(&vs(), 1).len(), 2);
    }

    #[test]
    fn sweep_on_a_degenerate_screen_does_not_panic() {
        let vs = VirtualScreen {
            origin_x: 0,
            origin_y: 0,
            width: 0,
            height: 0,
        };
        assert_eq!(plan_sweep(&vs, 4).len(), 4);
    }

    #[test]
    fn text_is_one_step_carrying_utf16_units() {
        let steps = plan_text("中a");
        assert_eq!(
            steps,
            vec![InjectStep::Text {
                utf16: vec![0x4E2D, 0x0061]
            }]
        );
    }

    #[test]
    fn outcome_fully_accepted_only_when_counts_match() {
        let ok = InjectOutcome {
            steps_attempted: 3,
            steps_accepted: 3,
            inputs_attempted: 5,
            inputs_accepted: 5,
            first_failure: None,
        };
        assert!(ok.fully_accepted());

        // UIPI 拦截的典型形态：事件一个没进去，但没有「错误」。
        let mut blocked = InjectOutcome {
            steps_attempted: 3,
            inputs_attempted: 5,
            ..InjectOutcome::default()
        };
        blocked.record_failure(0, WinError::new("SendInput", 0));
        assert!(!blocked.fully_accepted());
        assert!(blocked.summary().contains("0/3 步、0/5 个事件被接受"));
        assert!(blocked.summary().contains("第 0 步起被拒"));
        assert!(
            blocked.summary().contains("Win32 0"),
            "错误码 0 必须原样保留"
        );
    }

    #[test]
    fn only_the_first_failure_is_kept() {
        let mut outcome = InjectOutcome::default();
        outcome.record_failure(1, WinError::new("SendInput", 5));
        outcome.record_failure(2, WinError::new("SendInput", 87));
        assert_eq!(
            outcome
                .first_failure
                .map(|f| (f.step_index, f.error.code())),
            Some((1, 5))
        );
    }

    /// 常量是**手抄** windows-sys 的值（为了可移植与可测），所以钉一条机械锁：
    /// 一旦上游改了这些值，Windows 上的 CI 会立刻发现。
    #[cfg(all(test, windows))]
    #[test]
    fn flag_constants_match_windows_sys() {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse as km;
        assert_eq!(MOUSEEVENTF_MOVE, km::MOUSEEVENTF_MOVE);
        assert_eq!(MOUSEEVENTF_LEFTDOWN, km::MOUSEEVENTF_LEFTDOWN);
        assert_eq!(MOUSEEVENTF_LEFTUP, km::MOUSEEVENTF_LEFTUP);
        assert_eq!(MOUSEEVENTF_RIGHTDOWN, km::MOUSEEVENTF_RIGHTDOWN);
        assert_eq!(MOUSEEVENTF_RIGHTUP, km::MOUSEEVENTF_RIGHTUP);
        assert_eq!(MOUSEEVENTF_MIDDLEDOWN, km::MOUSEEVENTF_MIDDLEDOWN);
        assert_eq!(MOUSEEVENTF_MIDDLEUP, km::MOUSEEVENTF_MIDDLEUP);
        assert_eq!(MOUSEEVENTF_WHEEL, km::MOUSEEVENTF_WHEEL);
        assert_eq!(MOUSEEVENTF_HWHEEL, km::MOUSEEVENTF_HWHEEL);
        assert_eq!(MOUSEEVENTF_ABSOLUTE, km::MOUSEEVENTF_ABSOLUTE);
        assert_eq!(MOUSEEVENTF_VIRTUALDESK, km::MOUSEEVENTF_VIRTUALDESK);
        assert_eq!(KEYEVENTF_KEYUP, km::KEYEVENTF_KEYUP);
        assert_eq!(KEYEVENTF_UNICODE, km::KEYEVENTF_UNICODE);
        assert_eq!(
            u32::from_ne_bytes(WHEEL_DELTA.to_ne_bytes()),
            windows_sys::Win32::UI::WindowsAndMessaging::WHEEL_DELTA
        );
    }
}
