import { memo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Bot, ChevronDown, ChevronUp, Loader2 } from 'lucide-react';
import type { ChatItem } from './types';
import MessageBubble, { CollapsiblePre, resolveToolStatus, splitToolTitle } from './MessageBubble';
import { extractSubagentMeta } from './subagent';

interface Props {
  /** 子 agent 父 Task 卡（isSubagent 或带 children 的 tool 卡） */
  item: ChatItem;
  /** 当前正在流式写入的子气泡下标（children 内索引）；无则 undefined */
  streamingChildIdx?: number;
}

/**
 * 子 agent（Task）父卡：折叠态一行头部（名称/描述 + 类型徽标 + 状态徽章 +
 * 实时进度摘要），展开态嵌套渲染 children（迷你 ToolCard + 文本/思考气泡，
 * 左侧边条缩进分层），末尾展示父卡自身 toolResult（Task 最终结果）。
 * children 的文本/思考气泡在流式期间走 PlainBody 降级（streamingChildIdx 命中）。
 */
function SubagentTaskCard({ item, streamingChildIdx }: Props) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const meta = extractSubagentMeta(item.toolArgs, item.toolName);
  const label = meta.label ?? t('agent.subagent');
  const children = item.children ?? [];
  const status = resolveToolStatus(item);
  const tools = children.filter((c) => c.kind === 'tool');
  const done = tools.filter(
    (c) => c.toolResult != null || c.toolStatus === 'completed' || c.toolStatus === 'failed',
  ).length;
  const running = [...tools]
    .reverse()
    .find(
      (c) =>
        c.toolResult == null && c.toolStatus !== 'completed' && c.toolStatus !== 'failed',
    );
  const finished = status === 'completed' || status === 'failed';
  const count = tools.length > 0 ? `${done}/${tools.length} ${t('agent.tools')}` : null;
  const progress = count
    ? running && !finished
      ? `${count} · ${splitToolTitle(running.toolName, running.toolKind).label}`
      : count
    : null;

  return (
    <div>
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex w-full items-center gap-2 text-left text-xs"
        aria-expanded={open}
      >
        <Bot className="h-3.5 w-3.5 shrink-0 text-primary" />
        <span className="font-medium text-foreground/90">{label}</span>
        {meta.subagentType && (
          <span className="shrink-0 rounded bg-muted px-1.5 py-0.5 text-[10px] text-muted-foreground">
            {meta.subagentType}
          </span>
        )}
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
      {open && (
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
