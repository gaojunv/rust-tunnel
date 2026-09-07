/**
 * SlashMenu —— 输入框上方浮层斜杠命令菜单。
 * 由 AgentPanel（2E 接线）渲染在输入框相对定位容器内：当输入以 "/" 开头时展示。
 * 交互模式参照 QuickSwitcher：↑↓ 移动高亮（循环）、hover 同步高亮、Enter 选中、Esc 关闭；
 * 本组件不引 portal，面板内绝对定位。
 *
 * 键盘事件归 textarea 所有：2E 接线时在 textarea onKeyDown 中，若菜单打开则优先调用
 * ref.handleKeyDown(e)；返回 true 表示已消费（须 preventDefault，且不再执行发送/中断逻辑）。
 */
import { forwardRef, useEffect, useImperativeHandle, useRef, useState } from "react";
import type { Command } from "@/lib/agent/slash-commands";

export type SlashMenuProps = {
  /** 过滤后的命令（如无匹配，上层应直接不渲染本菜单） */
  commands: Command[];
  /**
   * 当前输入中解析出的命令 token（不含 /），用于 "/" 前缀展示。
   * 由 AgentPanel 经 parseSlashInput 计算后传入。
   */
  query: string;
  onSelect: (cmd: Command) => void;
  onClose: () => void;
};

/** 经 ref 暴露给 2E 的键盘转发接口 */
export type SlashMenuHandle = {
  /**
   * 处理 ↑↓/Enter/Esc；返回 true 表示已消费，
   * 上层须 preventDefault 且跳过发送/中断逻辑。
   */
  handleKeyDown: (e: React.KeyboardEvent) => boolean;
};

export const SlashMenu = forwardRef<SlashMenuHandle, SlashMenuProps>(function SlashMenu(
  { commands, query, onSelect, onClose },
  ref,
) {
  const [activeIndex, setActiveIndex] = useState(0);
  const itemRefs = useRef<Map<number, HTMLButtonElement>>(new Map());

  // 命令列表变化时重置高亮到首项
  useEffect(() => {
    setActiveIndex(0);
  }, [commands.length, query]);

  // 保持活动项可见
  useEffect(() => {
    const el = itemRefs.current.get(activeIndex);
    el?.scrollIntoView({ block: "nearest" });
  }, [activeIndex]);

  useImperativeHandle(
    ref,
    () => ({
      handleKeyDown: (e: React.KeyboardEvent): boolean => {
        if (commands.length === 0) return false;
        if (e.key === "ArrowDown") {
          setActiveIndex((i) => (i + 1) % commands.length);
          return true;
        }
        if (e.key === "ArrowUp") {
          setActiveIndex((i) => (i - 1 + commands.length) % commands.length);
          return true;
        }
        if (e.key === "Enter") {
          const cur = commands[activeIndex];
          if (cur) onSelect(cur);
          return true;
        }
        if (e.key === "Escape") {
          onClose();
          return true;
        }
        return false;
      },
    }),
    [commands, activeIndex, onSelect, onClose],
  );

  if (commands.length === 0) return null;

  return (
    <div
      role="listbox"
      aria-label="斜杠命令"
      className="absolute bottom-full left-0 right-0 z-10 mb-1 overflow-hidden rounded-lg border border-border bg-popover shadow-xl"
    >
      <ul className="max-h-56 overflow-y-auto p-1.5">
        {commands.map((cmd, idx) => {
          const active = idx === activeIndex;
          return (
            <li key={cmd.name} role="option" aria-selected={active}>
              <button
                type="button"
                ref={(el) => {
                  if (el) itemRefs.current.set(idx, el);
                  else itemRefs.current.delete(idx);
                }}
                onMouseEnter={() => setActiveIndex(idx)}
                onClick={() => onSelect(cmd)}
                className={`flex w-full items-baseline gap-2 rounded-md px-3 py-1.5 text-left transition-colors ${
                  active ? "bg-accent" : "hover:bg-accent/60"
                }`}
              >
                <span className="shrink-0 font-mono text-sm font-medium text-primary">
                  /{cmd.name}
                </span>
                {cmd.argsHint && (
                  <span className="shrink-0 font-mono text-xs text-muted-foreground">
                    {cmd.argsHint}
                  </span>
                )}
                <span className="min-w-0 flex-1 truncate text-xs text-muted-foreground">
                  {cmd.description}
                </span>
              </button>
            </li>
          );
        })}
      </ul>
      <div className="border-t border-border/60 px-3 py-1.5 text-xs text-muted-foreground">
        ↑↓ 选择 · Enter 执行{query ? `“/${query}”` : ""} · Esc 关闭
      </div>
    </div>
  );
});
