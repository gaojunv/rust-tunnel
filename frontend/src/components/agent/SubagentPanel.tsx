import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Bot, ChevronDown, ChevronUp, Loader2 } from 'lucide-react';
import { cn } from '@/lib/utils';
import type { SubagentSummary } from './subagent';

interface Props {
  /** 'top'：移动端顶部固定可折叠面板；'sidebar'：桌面端右侧固定栏 */
  variant: 'top' | 'sidebar';
  summaries: SubagentSummary[];
  /** 点击行：滚动对话到对应 subagent 卡片并联动展开（展开状态由外部持有） */
  onSelect: (index: number) => void;
  /** 已展开的 subagent toolId 集合（面板行高亮标记，与对话卡联动） */
  expandedIds: ReadonlySet<string>;
}

/** 状态图标：运行中 spinner / 完成 ✓ / 失败 ✗（与 SubagentTaskCard 头部徽章一致）。 */
function StatusIcon({ status }: { status: SubagentSummary['status'] }) {
  if (status === 'failed') return <span className="shrink-0 text-xs text-destructive">✗</span>;
  if (status === 'completed') return <span className="shrink-0 text-xs text-green-600">✓</span>;
  return <Loader2 className="h-3 w-3 shrink-0 animate-spin text-muted-foreground" />;
}

/**
 * subagent 固定状态面板：把散落在对话流里的子代理（SubagentTaskCard 父卡）聚合到
 * 固定位置常驻显示，避免被大量消息淹没。
 * - 'sidebar'：桌面端右侧固定栏（占文档流，不覆盖对话滚动条），可折叠为窄图标条；
 * - 'top'：移动端顶部固定可折叠面板（内容限宽与消息流对齐）。
 * 每行展示状态图标 + label + 类型徽标 + 实时进度摘要；点击行由外部滚动定位到对话
 * 中对应卡片并联动展开（expandedIds 高亮已展开条目）。
 */
export default function SubagentPanel({ variant, summaries, onSelect, expandedIds }: Props) {
  const { t } = useTranslation();
  const [collapsed, setCollapsed] = useState(false);
  const runningCount = summaries.filter(
    (s) => s.status !== 'completed' && s.status !== 'failed',
  ).length;
  const collapseLabel = collapsed ? t('agent.subagentExpand') : t('agent.subagentCollapse');

  const row = (s: SubagentSummary) => {
    const progress =
      s.toolCount === 0
        ? null
        : `${s.toolCount} ${t('agent.tools')}${s.runningToolLabel ? ` · ${s.runningToolLabel}` : ''}`;
    return (
      <button
        key={s.toolId ?? s.index}
        type="button"
        data-testid="subagent-panel-row"
        onClick={() => onSelect(s.index)}
        aria-label={t('agent.subagentJump')}
        className={cn(
          'flex w-full items-center gap-2 px-3 py-1.5 text-left text-xs transition-colors hover:bg-accent/60',
          s.toolId && expandedIds.has(s.toolId) && 'bg-accent/40',
        )}
      >
        <StatusIcon status={s.status} />
        <span className="min-w-0 truncate font-medium text-foreground/90">
          {s.label || t('agent.subagent')}
        </span>
        {s.subagentType && (
          <span className="shrink-0 rounded bg-muted px-1.5 py-0.5 text-[10px] text-muted-foreground">
            {s.subagentType}
          </span>
        )}
        {progress && (
          <span className="ml-auto shrink-0 truncate font-mono text-[10px] text-muted-foreground">
            {progress}
          </span>
        )}
      </button>
    );
  };

  if (variant === 'sidebar') {
    // 折叠：收为右侧窄图标条 + 运行中计数徽章（仍可见，点击展开）
    if (collapsed) {
      return (
        <div
          data-testid="subagent-panel"
          className="flex w-9 shrink-0 flex-col items-center border-l border-border/60 bg-card/80 py-2"
        >
          <button
            type="button"
            onClick={() => setCollapsed(false)}
            aria-label={collapseLabel}
            title={t('agent.subagents')}
            className="relative flex h-9 w-9 items-center justify-center rounded-md text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
          >
            <Bot className="h-4 w-4 text-primary" />
            {runningCount > 0 && (
              <span className="absolute -right-0.5 -top-0.5 flex h-4 min-w-4 items-center justify-center rounded-full bg-primary px-1 text-[9px] font-medium text-primary-foreground">
                {runningCount}
              </span>
            )}
          </button>
        </div>
      );
    }
    return (
      <div
        data-testid="subagent-panel"
        className="flex w-72 shrink-0 flex-col border-l border-border/60 bg-card/80"
      >
        <div className="flex items-center gap-2 border-b border-border/60 px-3 py-2">
          <Bot className="h-4 w-4 shrink-0 text-primary" />
          <span className="text-xs font-medium text-foreground/90">{t('agent.subagents')}</span>
          {runningCount > 0 && (
            <span className="rounded bg-primary/10 px-1.5 py-0.5 text-[10px] font-medium text-primary">
              {t('agent.subagentRunningCount', { count: runningCount })}
            </span>
          )}
          <button
            type="button"
            onClick={() => setCollapsed(true)}
            aria-label={collapseLabel}
            className="ml-auto rounded p-1 text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
          >
            <ChevronUp className="h-3.5 w-3.5" />
          </button>
        </div>
        <ul className="min-h-0 flex-1 divide-y divide-border/40 overflow-y-auto py-1">
          {summaries.map((s) => row(s))}
        </ul>
      </div>
    );
  }

  // top：移动端顶部固定面板（横向全宽，内容限宽与消息流对齐），折叠仅剩头部一行
  return (
    <div
      data-testid="subagent-panel"
      className="shrink-0 border-b border-border/60 bg-card/95 backdrop-blur-md"
    >
      <div className="mx-auto w-full max-w-3xl px-3 py-1.5 md:px-5">
        <div className="flex items-center gap-2">
          <Bot className="h-4 w-4 shrink-0 text-primary" />
          <span className="text-xs font-medium text-foreground/90">{t('agent.subagents')}</span>
          {runningCount > 0 && (
            <span className="rounded bg-primary/10 px-1.5 py-0.5 text-[10px] font-medium text-primary">
              {t('agent.subagentRunningCount', { count: runningCount })}
            </span>
          )}
          <button
            type="button"
            onClick={() => setCollapsed((v) => !v)}
            aria-label={collapseLabel}
            className="ml-auto rounded p-1 text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
          >
            {collapsed ? (
              <ChevronDown className="h-3.5 w-3.5" />
            ) : (
              <ChevronUp className="h-3.5 w-3.5" />
            )}
          </button>
        </div>
        {!collapsed && summaries.length > 0 && (
          <ul className="max-h-[40vh] divide-y divide-border/40 overflow-y-auto pb-1 pt-1">
            {summaries.map((s) => row(s))}
          </ul>
        )}
      </div>
    </div>
  );
}
