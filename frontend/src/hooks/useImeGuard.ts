import { useCallback, useRef } from 'react';

/** IME 组词守卫：判定一次 keydown 是否发生在输入法组词过程中（拼音候选窗打开时），
 *  用于让「回车 = 确认候选」而不是触发发送/提交等动作。
 *
 *  单靠 `e.nativeEvent.isComposing` 不够，三种真实场景各漏一路：
 *  - Chrome/多数输入法：确认 Enter 的 keydown `isComposing=true`（这一路 isComposing 够用）
 *  - Safari：`compositionend` 先于确认 Enter 的 keydown 触发，该 keydown 的
 *    `isComposing=false`、`keyCode=13` → 只能靠手动跟踪的 composing 标记兜住
 *  - 部分输入法：全程不发 composition 事件，把确认键的 `keyCode` 报成 229
 *
 *  故三路并查。Safari 那一路要求 `compositionend` 后延迟一个事件循环再清标记：
 *  同一 tick 内人类不可能再敲一次键，因此只会影响「确认候选」这一次回车。
 *
 *  用法：把 `bind` 展开到输入元素上，在 keydown 里先调 `isComposing()` 短路。
 *  ```tsx
 *  const ime = useImeGuard();
 *  <input {...ime.bind} onKeyDown={(e) => {
 *    if (ime.isComposing(e)) return;
 *    if (e.key === 'Enter') submit();
 *  }} />
 *  ```
 */
export function useImeGuard() {
  const composingRef = useRef(false);

  const onCompositionStart = useCallback(() => {
    composingRef.current = true;
  }, []);

  const onCompositionEnd = useCallback(() => {
    setTimeout(() => {
      composingRef.current = false;
    }, 0);
  }, []);

  const isComposing = useCallback((e: React.KeyboardEvent) => {
    return composingRef.current || e.nativeEvent.isComposing || e.nativeEvent.keyCode === 229;
  }, []);

  return { bind: { onCompositionStart, onCompositionEnd }, isComposing };
}
