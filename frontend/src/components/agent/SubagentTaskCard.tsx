import { memo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Bot, ChevronDown, ChevronUp, Loader2 } from 'lucide-react';
import type { ChatItem } from './types';
import MessageBubble, { CollapsiblePre, resolveToolStatus, splitToolTitle } from './MessageBubble';
import SubagentTypeBadge from './SubagentTypeBadge';
import { extractSubagentMeta } from './subagent';

interface Props {
  /** 子 agent 父 Task 卡（isSubagent 或带 children 的 tool 卡） */
  item: ChatItem;
  /** 当前正在流式写入的子气泡下标（children 内索引）；无则 undefined */
  streamingChildIdx?: number;
  /** 受控展开：提供时展开态由外部持有（点击调 onToggle），缺省走内部 useState */
  open?: boolean;
  /** 受控展开时的点击回调（外部负责翻转 open） */
  onToggle?: () => void;
}

/**
 * 子 agent（Task）父卡：卡片容器（与 MessageBubble 工具卡同构：圆角细线边框 +
 * muted 淡底）+ 折叠态一行头部（名称/描述 + 类型徽标 + 状态徽章 + 实时进度摘要），
 * 展开态嵌套渲染 children（迷你 ToolCard + 文本/思考气泡，左侧边条缩进分层），
 * 末尾展示父卡自身 toolResult（Task 最终结果）。运行中卡片底部显示与 ToolCard
 * 相同的不确定进度条（呼吸动画），保证两卡视觉一致。
 * children 的文本/思考气泡在流式期间走 Markdown streaming 降级（streamingChildIdx
 * 命中）。
 * 展开支持受控（open/onToggle，供 subagent 固定面板联动展开）与非受控（内部
 * useState，历史行为不变）双路径。
 */
function SubagentTaskCard({ item, streamingChildIdx, open, onToggle }: Props) {
  const { t } = useTranslation();
  const [openState, setOpenState] = useState(false);
  // 受控优先：open 提供时外部持有展开态；否则回退内部 state（既有行为不变）
  const isControlled = open !== undefined;
  const expanded = isControlled ? open : openState;
  const toggle = () => {
    if (isControlled) onToggle?.();
    else setOpenState((v) => !v);
  };
  const meta = extractSubagentMeta(item.toolArgs, item.toolName);
  const label = meta.label ?? t('agent.subagent');
  const children = item.children ?? [];
  const status = resolveToolStatus(item);
  const tools = children.filter((c) => c.kind === 'tool');
  // 当前运行工具 = 最后一个未完成的子工具卡（用 resolveToolStatus：显式
  // running/in_progress/pending 即未完成——result 到达不覆盖，同父卡状态语义）
  const running = [...tools]
    .reverse()
    .find((c) => {
      const s = resolveToolStatus(c);
      return s !== 'completed' && s !== 'failed';
    });
  const finished = status === 'completed' || status === 'failed';
  const isRunning = !finished;
  // 进度文案不再显示 done/total（分母只统计已开始的工具、永远接近完成，无意义），
  // 改为「N 个工具 · 当前工具名」（运行中）/「N 个工具」（已完成）。无工具不显示。
  const progress =
    tools.length === 0
      ? null
      : finished
        ? `${tools.length} ${t('agent.tools')}`
        : running
          ? `${tools.length} ${t('agent.tools')} · ${splitToolTitle(running.toolName, running.toolKind).label}`
          : `${tools.length} ${t('agent.tools')}`;

  return (
    <div className="relative w-full overflow-hidden rounded-lg border border-border/70 bg-muted/30 px-3 py-2 text-sm">
      <button
        type="button"
        onClick={toggle}
        className="flex w-full items-center gap-2 text-left text-xs"
        aria-expanded={expanded}
      >
        <Bot className="h-3.5 w-3.5 shrink-0 text-primary" />
        <span className="font-medium text-foreground/90">{label}</span>
        {meta.subagentType && <SubagentTypeBadge type={meta.subagentType} />}
        {progress && (
          <span className="min-w-0 truncate font-mono text-muted-foreground">{progress}</span>
        )}
        <span className="ml-auto flex shrink-0 items-center gap-1.5">
          {status === 'failed' ? (
            <span className="shrink-0 text-xs text-destructive">✗</span>
          ) : status === 'completed' ? (
            <span className="shrink-0 text-xs text-green-600">✓</span>
          ) : (
            <Loader2 className="h-3 w-3 shrink-0 animate-spin text-muted-foreground" />
          )}
          {open ? (
            <ChevronUp className="h-3.5 w-3.5 text-muted-foreground" />
          ) : (
            <ChevronDown className="h-3.5 w-3.5 text-muted-foreground" />
          )}
        </span>
      </button>
      {/* 进度条绝对定位在卡片底部边缘（同 ToolCard）：运行中呼吸动画，完成后
          透明淡出。不占文档流高度；容器常驻避免出现/消失时 DOM 抖动。 */}
      <div
        className={`absolute inset-x-0 bottom-0 h-0.5 overflow-hidden rounded transition-colors duration-300 ${
          isRunning ? 'bg-muted' : 'bg-transparent'
        }`}
        aria-hidden={!isRunning}
      >
        {isRunning && <div className="h-full w-1/3 animate-pulse rounded bg-primary/60" />}
      </div>
      {expanded && (
        <div className="mt-2 space-y-2 border-t border-border/60 pt-2">
          {children.length > 0 && (
            <div className="space-y-2 border-l border-border/40 pl-3">
              {children.map((child, i) => (
                <MessageBubble
                  key={child.kind === 'tool' && child.toolId ? child.toolId : `c${i}`}
                  item={child}
                  streaming={streamingChildIdx === i}
                />
              ))}
            </div>
          )}
          {children.length === 0 && !finished && (
            <div className="flex items-center gap-1 text-xs text-muted-foreground">
              <Loader2 className="h-3 w-3 animate-spin" />
              {t('agent.subagentWaiting')}
            </div>
          )}
          {item.toolResult ? (
            <CollapsiblePre
              text={item.toolResult}
              className={children.length > 0 ? 'border-t border-border/60 pt-2' : undefined}
            />
          ) : null}
        </div>
      )}
    </div>
  );
}

export default memo(SubagentTaskCard);
