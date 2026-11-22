//! 跨平台键位码与 Android `KeyEvent` 键码/修饰键的互转。
//!
//! 协议侧键位码（`keycode` / `modifier`）与桌面控制器
//! `crates/client-gui/src/remote/mod.rs` 中的同名模块**逐值一致**（协议 v5 的线上契约）：
//! 可打印键也发完整键位码，最终字符由**被控端**的键盘布局与输入法决定。
//! 这里额外提供 Android `android.view.KeyEvent.KEYCODE_*` 到同一空间的映射，
//! 以及 `KeyEvent.getMetaState()` 到协议 modifiers 位图的转换。
//!
//! 本模块是纯函数（不触碰 JNI），故在主机上可完整单测。

/// 协议键位码。
///
/// 数值与 `crates/client-gui/src/remote/mod.rs` 的 `keycode` 模块逐项相同；
/// 若发现不一致，以本模块的注释为准并同步两处（协议不变量）。
#[allow(
    clippy::module_inception,
    reason = "模块名与文件名一致，便于与 client-gui 的同名模块逐值对照"
)]
pub(crate) mod keycode {
    /// 可打印文本（键值在 `KeyEvent.text` 里）。**遗留通路**：仅 `nativeSendText` 使用。
    pub(crate) const TEXT: u32 = 0;
    /// 回车。
    pub(crate) const ENTER: u32 = 1;
    /// Tab。
    pub(crate) const TAB: u32 = 2;
    /// 退格。
    pub(crate) const BACKSPACE: u32 = 3;
    /// Esc。
    pub(crate) const ESCAPE: u32 = 4;
    /// Delete。
    pub(crate) const DELETE: u32 = 5;
    /// 左方向键。
    pub(crate) const ARROW_LEFT: u32 = 6;
    /// 右方向键。
    pub(crate) const ARROW_RIGHT: u32 = 7;
    /// 上方向键。
    pub(crate) const ARROW_UP: u32 = 8;
    /// 下方向键。
    pub(crate) const ARROW_DOWN: u32 = 9;
    /// Home。
    pub(crate) const HOME: u32 = 10;
    /// End。
    pub(crate) const END: u32 = 11;
    /// PageUp。
    pub(crate) const PAGE_UP: u32 = 12;
    /// PageDown。
    pub(crate) const PAGE_DOWN: u32 = 13;
    /// 空格。
    pub(crate) const SPACE: u32 = 14;
    /// Insert。
    pub(crate) const INSERT: u32 = 15;
    /// F1（F1..=F12 是连续区间 20..=31）。
    pub(crate) const F1: u32 = 20;
    /// Ctrl。
    pub(crate) const CTRL: u32 = 40;
    /// Shift。
    pub(crate) const SHIFT: u32 = 41;
    /// Alt / Option。
    pub(crate) const ALT: u32 = 42;
    /// Meta / Cmd / Win。
    pub(crate) const META: u32 = 43;
    /// 字母 A（`LETTER_A + (b'A'..=b'Z')` 的偏移，0x100..=0x119）。
    pub(crate) const LETTER_A: u32 = 0x100;
    /// 字母 Z（区间上界，映射按 `LETTER_A + 偏移` 计算，本常量供测试/文档断言用）。
    #[allow(
        dead_code,
        reason = "区间上界：只用于测试断言与文档，映射本身按偏移计算"
    )]
    pub(crate) const LETTER_Z: u32 = 0x119;
    /// 数字 0（`DIGIT_0 + b'0'..=b'9'` 的偏移，0x120..=0x129）。
    pub(crate) const DIGIT_0: u32 = 0x120;
    /// 数字 9（区间上界，同 [`LETTER_Z`]）。
    #[allow(
        dead_code,
        reason = "区间上界：只用于测试断言与文档，映射本身按偏移计算"
    )]
    pub(crate) const DIGIT_9: u32 = 0x129;
    /// `-`。
    pub(crate) const MINUS: u32 = 0x12A;
    /// `=`。
    pub(crate) const EQUALS: u32 = 0x12B;
    /// `[`。
    pub(crate) const OPEN_BRACKET: u32 = 0x12C;
    /// `]`。
    pub(crate) const CLOSE_BRACKET: u32 = 0x12D;
    /// `\`。
    pub(crate) const BACKSLASH: u32 = 0x12E;
    /// `;`。
    pub(crate) const SEMICOLON: u32 = 0x12F;
    /// `'`。
    pub(crate) const QUOTE: u32 = 0x130;
    /// `` ` ``。
    pub(crate) const BACKTICK: u32 = 0x131;
    /// `,`。
    pub(crate) const COMMA: u32 = 0x132;
    /// `.`。
    pub(crate) const PERIOD: u32 = 0x133;
    /// `/`。
    pub(crate) const SLASH: u32 = 0x134;
}

/// 协议修饰键位掩码（`KeyEvent.modifiers`）。
///
/// **必须与桌面控制器 `crates/client-gui/src/remote/mod.rs` 的 `modifier` 模块逐位一致**：
/// `ALT=1<<0`、`CTRL=1<<1`、`SHIFT=1<<2`、`META=1<<3`。当前引擎注入侧不读取该字段
/// （修饰键由独立的键位事件模型承载），所以位序写反也不会改变注入结果；但这是一个
/// 线上字段，两侧一旦分叉，日后开始消费时会静默错位。故这里对齐桌面端的既有约定，
/// 而不是另起一套。
pub(crate) mod modifier {
    /// Alt / Option。
    pub(crate) const ALT: u8 = 1 << 0;
    /// Ctrl。
    pub(crate) const CTRL: u8 = 1 << 1;
    /// Shift。
    pub(crate) const SHIFT: u8 = 1 << 2;
    /// Meta / Cmd / Win。
    pub(crate) const META: u8 = 1 << 3;
}

/// Android `android.view.KeyEvent.KEYCODE_*` 常量（只收录映射用到的子集）。
///
/// 数值来自 Android SDK 的 `android.view.KeyEvent`（API 21 起稳定）。
pub(crate) mod android_keycode {
    /// 字母 A（A..=Z 是连续区间 29..=54）。
    pub(crate) const A: i32 = 29;
    /// 字母 Z。
    pub(crate) const Z: i32 = 54;
    /// 数字 0（0..=9 是连续区间 7..=16）。
    pub(crate) const DIGIT_0: i32 = 7;
    /// 数字 9。
    pub(crate) const DIGIT_9: i32 = 16;
    /// 方向键上。
    pub(crate) const DPAD_UP: i32 = 19;
    /// 方向键下。
    pub(crate) const DPAD_DOWN: i32 = 20;
    /// 方向键左。
    pub(crate) const DPAD_LEFT: i32 = 21;
    /// 方向键右。
    pub(crate) const DPAD_RIGHT: i32 = 22;
    /// `,`。
    pub(crate) const COMMA: i32 = 55;
    /// `.`。
    pub(crate) const PERIOD: i32 = 56;
    /// 左 Alt。
    pub(crate) const ALT_LEFT: i32 = 57;
    /// 右 Alt。
    pub(crate) const ALT_RIGHT: i32 = 58;
    /// 左 Shift。
    pub(crate) const SHIFT_LEFT: i32 = 59;
    /// 右 Shift。
    pub(crate) const SHIFT_RIGHT: i32 = 60;
    /// Tab。
    pub(crate) const TAB: i32 = 61;
    /// 空格。
    pub(crate) const SPACE: i32 = 62;
    /// 回车。
    pub(crate) const ENTER: i32 = 66;
    /// 退格。
    pub(crate) const DEL: i32 = 67;
    /// `` ` ``。
    pub(crate) const GRAVE: i32 = 68;
    /// `-`。
    pub(crate) const MINUS: i32 = 69;
    /// `=`。
    pub(crate) const EQUALS: i32 = 70;
    /// `[`。
    pub(crate) const LEFT_BRACKET: i32 = 71;
    /// `]`。
    pub(crate) const RIGHT_BRACKET: i32 = 72;
    /// `\`。
    pub(crate) const BACKSLASH: i32 = 73;
    /// `;`。
    pub(crate) const SEMICOLON: i32 = 74;
    /// `'`。
    pub(crate) const APOSTROPHE: i32 = 75;
    /// `/`。
    pub(crate) const SLASH: i32 = 76;
    /// PageUp。
    pub(crate) const PAGE_UP: i32 = 92;
    /// PageDown。
    pub(crate) const PAGE_DOWN: i32 = 93;
    /// Esc。
    pub(crate) const ESCAPE: i32 = 111;
    /// Delete（向后删除）。
    pub(crate) const FORWARD_DEL: i32 = 112;
    /// 左 Ctrl。
    pub(crate) const CTRL_LEFT: i32 = 113;
    /// 右 Ctrl。
    pub(crate) const CTRL_RIGHT: i32 = 114;
    /// 左 Meta / Win。
    pub(crate) const META_LEFT: i32 = 117;
    /// 右 Meta / Win。
    pub(crate) const META_RIGHT: i32 = 118;
    /// Home。
    pub(crate) const MOVE_HOME: i32 = 122;
    /// End。
    pub(crate) const MOVE_END: i32 = 123;
    /// Insert。
    pub(crate) const INSERT: i32 = 124;
    /// F1（F1..=F12 是连续区间 131..=142）。
    pub(crate) const F1: i32 = 131;
    /// F12。
    pub(crate) const F12: i32 = 142;
}

/// Android `KeyEvent.getMetaState()` 的位常量（只收录转换用到的子集）。
pub(crate) mod android_meta {
    /// Shift 按下。
    pub(crate) const SHIFT_ON: i32 = 0x0000_0001;
    /// Alt 按下。
    pub(crate) const ALT_ON: i32 = 0x0000_0002;
    /// 左 Alt 按下。
    pub(crate) const ALT_LEFT_ON: i32 = 0x0000_0010;
    /// 右 Alt 按下。
    pub(crate) const ALT_RIGHT_ON: i32 = 0x0000_0020;
    /// 左 Shift 按下。
    pub(crate) const SHIFT_LEFT_ON: i32 = 0x0000_0040;
    /// 右 Shift 按下。
    pub(crate) const SHIFT_RIGHT_ON: i32 = 0x0000_0080;
    /// Ctrl 按下。
    pub(crate) const CTRL_ON: i32 = 0x0000_1000;
    /// 左 Ctrl 按下。
    pub(crate) const CTRL_LEFT_ON: i32 = 0x0000_2000;
    /// 右 Ctrl 按下。
    pub(crate) const CTRL_RIGHT_ON: i32 = 0x0000_4000;
    /// Meta 按下。
    pub(crate) const META_ON: i32 = 0x0001_0000;
    /// 左 Meta 按下。
    pub(crate) const META_LEFT_ON: i32 = 0x0002_0000;
    /// 右 Meta 按下。
    pub(crate) const META_RIGHT_ON: i32 = 0x0004_0000;
}

/// 把 Android `KeyEvent.getKeyCode()` 转成协议键位码；不支持时返回 `None`。
///
/// 未映射的键（返回 `None`）在 JNI 层表现为 `-1`，Kotlin 应跳过发送。
#[must_use]
pub(crate) fn map_android_key(code: i32) -> Option<u32> {
    use android_keycode as ak;
    use keycode as kc;

    let mapped = match code {
        ak::A..=ak::Z => kc::LETTER_A + u32::try_from(code - ak::A).ok()?,
        ak::DIGIT_0..=ak::DIGIT_9 => kc::DIGIT_0 + u32::try_from(code - ak::DIGIT_0).ok()?,
        ak::F1..=ak::F12 => kc::F1 + u32::try_from(code - ak::F1).ok()?,
        ak::ENTER => kc::ENTER,
        ak::TAB => kc::TAB,
        ak::DEL => kc::BACKSPACE,
        ak::ESCAPE => kc::ESCAPE,
        ak::FORWARD_DEL => kc::DELETE,
        ak::DPAD_LEFT => kc::ARROW_LEFT,
        ak::DPAD_RIGHT => kc::ARROW_RIGHT,
        ak::DPAD_UP => kc::ARROW_UP,
        ak::DPAD_DOWN => kc::ARROW_DOWN,
        ak::MOVE_HOME => kc::HOME,
        ak::MOVE_END => kc::END,
        ak::PAGE_UP => kc::PAGE_UP,
        ak::PAGE_DOWN => kc::PAGE_DOWN,
        ak::SPACE => kc::SPACE,
        ak::INSERT => kc::INSERT,
        ak::SHIFT_LEFT | ak::SHIFT_RIGHT => kc::SHIFT,
        ak::CTRL_LEFT | ak::CTRL_RIGHT => kc::CTRL,
        ak::ALT_LEFT | ak::ALT_RIGHT => kc::ALT,
        ak::META_LEFT | ak::META_RIGHT => kc::META,
        ak::MINUS => kc::MINUS,
        ak::EQUALS => kc::EQUALS,
        ak::LEFT_BRACKET => kc::OPEN_BRACKET,
        ak::RIGHT_BRACKET => kc::CLOSE_BRACKET,
        ak::BACKSLASH => kc::BACKSLASH,
        ak::SEMICOLON => kc::SEMICOLON,
        ak::APOSTROPHE => kc::QUOTE,
        ak::GRAVE => kc::BACKTICK,
        ak::COMMA => kc::COMMA,
        ak::PERIOD => kc::PERIOD,
        ak::SLASH => kc::SLASH,
        _ => return None,
    };
    Some(mapped)
}

/// 把 Android `KeyEvent.getMetaState()` 转成协议 `modifier` 位图。
///
/// 同时接受「合并位」（`META_SHIFT_ON` 等）与「左右分侧位」（`META_SHIFT_LEFT_ON` 等）：
/// Android 官方文档说明 `getMetaState()` 返回合并位，但部分输入法与硬件键盘会带分侧位，
/// 两者都认才不漏。
#[must_use]
pub(crate) fn modifiers_from_meta_state(meta: i32) -> u8 {
    use android_meta as am;

    let mut bits = 0_u8;
    if meta & (am::SHIFT_ON | am::SHIFT_LEFT_ON | am::SHIFT_RIGHT_ON) != 0 {
        bits |= modifier::SHIFT;
    }
    if meta & (am::CTRL_ON | am::CTRL_LEFT_ON | am::CTRL_RIGHT_ON) != 0 {
        bits |= modifier::CTRL;
    }
    if meta & (am::ALT_ON | am::ALT_LEFT_ON | am::ALT_RIGHT_ON) != 0 {
        bits |= modifier::ALT;
    }
    if meta & (am::META_ON | am::META_LEFT_ON | am::META_RIGHT_ON) != 0 {
        bits |= modifier::META;
    }
    bits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_and_digits_are_contiguous() {
        assert_eq!(map_android_key(android_keycode::A), Some(keycode::LETTER_A));
        assert_eq!(
            map_android_key(android_keycode::A + 2),
            Some(keycode::LETTER_A + 2)
        );
        assert_eq!(map_android_key(android_keycode::Z), Some(keycode::LETTER_Z));
        assert_eq!(
            map_android_key(android_keycode::DIGIT_0),
            Some(keycode::DIGIT_0)
        );
        assert_eq!(
            map_android_key(android_keycode::DIGIT_9),
            Some(keycode::DIGIT_9)
        );
        // 字母区间与数字区间之间没有错位。
        assert_eq!(keycode::LETTER_Z - keycode::LETTER_A, 25);
        assert_eq!(keycode::DIGIT_9 - keycode::DIGIT_0, 9);
    }

    #[test]
    fn function_keys_map_to_contiguous_range() {
        assert_eq!(map_android_key(android_keycode::F1), Some(keycode::F1));
        assert_eq!(
            map_android_key(android_keycode::F1 + 11),
            Some(keycode::F1 + 11)
        );
        assert_eq!(
            map_android_key(android_keycode::F12),
            Some(keycode::F1 + 11)
        );
        // F13 之后的协议键位码没有被 Android 常量覆盖。
        assert_eq!(map_android_key(android_keycode::F12 + 1), None);
    }

    #[test]
    fn special_keys_and_modifiers_map() {
        assert_eq!(
            map_android_key(android_keycode::ENTER),
            Some(keycode::ENTER)
        );
        assert_eq!(
            map_android_key(android_keycode::DEL),
            Some(keycode::BACKSPACE)
        );
        assert_eq!(
            map_android_key(android_keycode::FORWARD_DEL),
            Some(keycode::DELETE)
        );
        assert_eq!(
            map_android_key(android_keycode::SHIFT_LEFT),
            Some(keycode::SHIFT)
        );
        assert_eq!(
            map_android_key(android_keycode::SHIFT_RIGHT),
            Some(keycode::SHIFT)
        );
        assert_eq!(
            map_android_key(android_keycode::CTRL_LEFT),
            Some(keycode::CTRL)
        );
        assert_eq!(
            map_android_key(android_keycode::ALT_RIGHT),
            Some(keycode::ALT)
        );
        assert_eq!(
            map_android_key(android_keycode::META_LEFT),
            Some(keycode::META)
        );
        assert_eq!(
            map_android_key(android_keycode::SLASH),
            Some(keycode::SLASH)
        );
        assert_eq!(
            map_android_key(android_keycode::APOSTROPHE),
            Some(keycode::QUOTE)
        );
        assert_eq!(
            map_android_key(android_keycode::GRAVE),
            Some(keycode::BACKTICK)
        );
    }

    #[test]
    fn unknown_keys_return_none() {
        for code in [0, 1, 2, 3, 4, 17, 18, 23, 24, 85, 143, 200, -1] {
            assert_eq!(map_android_key(code), None, "keycode {code} 应不支持");
        }
    }

    #[test]
    fn no_protocol_keycode_collisions() {
        // 把全部支持的 Android 键码映射一遍，确认没有两个（非修饰键的）键落到同一个协议码
        // （例如 F1 区间与修饰键撞号）。左右修饰键**有意**映射到同一个协议码，故豁免。
        let mut seen = std::collections::HashMap::new();
        for code in 0..=300 {
            if let Some(mapped) = map_android_key(code) {
                if mapped == keycode::TEXT {
                    continue;
                }
                if matches!(
                    mapped,
                    keycode::SHIFT | keycode::CTRL | keycode::ALT | keycode::META
                ) {
                    continue;
                }
                assert!(
                    seen.insert(mapped, code).is_none(),
                    "协议码 {mapped} 被多个 Android 键码映射"
                );
            }
        }
    }

    #[test]
    fn modifier_bits_match_contract() {
        assert_eq!(modifier::ALT, 0x01);
        assert_eq!(modifier::CTRL, 0x02);
        assert_eq!(modifier::SHIFT, 0x04);
        assert_eq!(modifier::META, 0x08);
    }

    #[test]
    fn meta_state_conversion() {
        assert_eq!(modifiers_from_meta_state(0), 0);
        assert_eq!(
            modifiers_from_meta_state(android_meta::SHIFT_ON),
            modifier::SHIFT
        );
        assert_eq!(
            modifiers_from_meta_state(android_meta::CTRL_ON),
            modifier::CTRL
        );
        assert_eq!(
            modifiers_from_meta_state(android_meta::ALT_ON),
            modifier::ALT
        );
        assert_eq!(
            modifiers_from_meta_state(android_meta::META_ON),
            modifier::META
        );
        assert_eq!(
            modifiers_from_meta_state(android_meta::SHIFT_LEFT_ON | android_meta::CTRL_RIGHT_ON),
            modifier::SHIFT | modifier::CTRL
        );
        assert_eq!(
            modifiers_from_meta_state(
                android_meta::SHIFT_ON | android_meta::ALT_ON | android_meta::CTRL_ON
            ),
            modifier::SHIFT | modifier::ALT | modifier::CTRL
        );
        // 未启用/不相关位（SYM/FUNCTION）不产生修饰键。
        assert_eq!(modifiers_from_meta_state(0x08 | 0x04), 0);
    }
}
