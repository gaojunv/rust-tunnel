import { memo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import {
  Brain,
  ChevronDown,
  ChevronUp,
  FileText,
  Globe,
  ListChecks,
  Loader2,
  Pencil,
  Search,
  TerminalSquare,
  Wrench,
} from 'lucide-react';
import type { ChatItem, ToolKind, PlanEntryItem } from './types';
import Markdown from './Markdown';
import ToolDiffView from './ToolDiffView';

/** 折叠阈值：tool 参数/结果超过该行数时只显示前 3 行，可手动展开。 */
const COLLAPSE_LINE_THRESHOLD = 6;
const COLLAPSE_VISIBLE_LINES = 3;

function firstLines(text: string, n: number): string {
  const lines = text.split('\n');
  if (lines.length <= n) return text;
  return lines.slice(0, n).join('\n');
}

/** 工具调用的长文本（args/result）：超过阈值折叠为前 3 行 + 展开按钮。 */
function CollapsiblePre({ text, className }: { text: string; className?: string }) {
  const { t } = useTranslation();
  const [expanded, setExpanded] = useState(false);
  const lineCount = text.split('\n').length;
  const collapsible = lineCount > COLLAPSE_LINE_THRESHOLD;
  const shown = collapsible && !expanded ? firstLines(text, COLLAPSE_VISIBLE_LINES) : text;

  return (
    <div className={className}>
      <pre className="whitespace-pre-wrap text-xs text-muted-foreground">{shown}</pre>
      {collapsible && (
        <button
          type="button"
          onClick={() => setExpanded((v) => !v)}
          className="mt-0.5 flex items-center gap-0.5 text-xs text-primary hover:underline"
        >
          {expanded ? (
            <>
              <ChevronUp className="h-3 w-3" />
              {t('agent.collapse')}
            </>
          ) : (
            <>
              <ChevronDown className="h-3 w-3" />
              {t('agent.expandLines', { count: lineCount - COLLAPSE_VISIBLE_LINES })}
            </>
          )}
        </button>
      )}
    </div>
  );
}

/** 从 toolArgs JSON 提取一行摘要：shell→cmd；文件类→path；search→path + pattern。 */
function toolSummary(name: string | undefined, args: string | undefined): string | null {
  if (!args) return null;
  try {
    const parsed = JSON.parse(args) as Record<string, unknown>;
    const str = (k: string) => (typeof parsed[k] === 'string' ? (parsed[k] as string) : null);
    switch (name) {
      case 'shell':
        return str('cmd');
      case 'search': {
        const path = str('path') ?? '.';
        const pattern = str('pattern');
        return pattern ? `${path} ⌕ ${pattern}` : path;
      }
      case 'read_file':
      case 'write_file':
      case 'patch_file':
      case 'list_dir':
        return str('path');
      default:
        return null;
    }
  } catch {
    return null;
  }
}

/** toolKind → 图标（视觉分类；未知/缺省一律 Wrench）。 */
const KIND_ICON: Record<ToolKind, typeof Wrench> = {
  read: FileText,
  edit: Pencil,
  delete: Pencil,
  move: Pencil,
  search: Search,
  execute: TerminalSquare,
  think: Brain,
  fetch: Globe,
  switch_mode: Wrench,
  other: Wrench,
};

/** 工具执行状态徽章：显式 toolStatus 优先；缺省（旧数据）按 result 有无推断。 */
function StatusBadge({ item }: { item: ChatItem }) {
  const status = item.toolStatus ?? (item.toolResult != null ? 'completed' : 'in_progress');
  if (status === 'failed') return <span className="shrink-0 text-xs text-destructive">✗</span>;
  if (status === 'completed') return <span className="shrink-0 text-xs text-green-600">✓</span>;
  return <Loader2 className="h-3 w-3 shrink-0 animate-spin text-muted-foreground" />;
}

/** 工具调用卡片：默认收起为一行头部（图标 + 名称 + 摘要 + 状态），点击展开详情。
 *  详情优先级：diffs → args/result 文本（折叠）。 */
function ToolCard({ item }: { item: ChatItem }) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const summary = toolSummary(item.toolName, item.toolArgs);
  const Icon = KIND_ICON[item.toolKind ?? 'other'];
  const isError = (item.toolStatus ?? (item.toolResult != null ? 'completed' : 'in_progress')) === 'failed';

  return (
    <div>
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex w-full items-center gap-2 text-left text-xs"
        aria-expanded={open}
      >
        <Icon className="h-3.5 w-3.5 shrink-0 text-primary" />
        <span className="font-medium text-foreground/90">{item.toolName}</span>
        {summary && (
          <span className="min-w-0 truncate font-mono text-muted-foreground">{summary}</span>
        )}
        <span className="ml-auto flex shrink-0 items-center gap-1.5">
          <StatusBadge item={item} />
          {open ? (
            <ChevronUp className="h-3.5 w-3.5 text-muted-foreground" />
          ) : (
            <ChevronDown className="h-3.5 w-3.5 text-muted-foreground" />
          )}
        </span>
      </button>
      {open && (
        <div className="mt-2 space-y-2 border-t border-border/60 pt-2">
          {item.toolDiffs && item.toolDiffs.length > 0 && <ToolDiffView diffs={item.toolDiffs} />}
          {item.toolArgs && <CollapsiblePre text={item.toolArgs} />}
          {item.toolResult ? (
            <CollapsiblePre text={item.toolResult} className={item.toolArgs || item.toolDiffs ? 'border-t border-border/60 pt-2' : undefined} />
          ) : (
            <div className="mt-1 flex items-center gap-1 text-xs text-muted-foreground">
              <Loader2 className="h-3 w-3 animate-spin" />
              {t('agent.toolRunning')}
            </div>
          )}
          {isError && item.toolResult && (
            <div className="text-xs text-destructive">{t('agent.toolFailed')}</div>
          )}
        </div>
      )}
    </div>
  );
}

/** 单条消息气泡：user / assistant（Markdown）/ tool（默认收起的工具卡片）。
 *  memo 化：流式 chunk 每帧更新列表 state，内容未变的气泡跳过重渲染。
 *  布局策略（对标主流 AI 聊天 UI）：
 *  - user：右对齐小气泡（primary 淡底），用户消息一般短，气泡让双方身份一眼可辨
 *  - assistant：全宽无气泡正文。LLM 回复是长文（标题/列表/表格/代码块），
 *    套 max-w-[80%] 的圆角盒子会压缩排版空间、且圆角背景让长文显得拥挤
 *  - tool：全宽细线卡片，视觉上弱于正文（工具是过程，正文是结论） */
export default memo(function MessageBubble({ item }: { item: ChatItem }) {
  const cls =
    item.kind === 'user'
      ? 'ml-auto max-w-[85%] rounded-2xl rounded-br-md bg-primary/10 px-3.5 py-2 text-sm leading-relaxed'
      : item.kind === 'assistant'
        ? 'w-full py-0.5'
        : item.kind === 'thought'
          ? 'w-full rounded-lg border border-border/50 bg-muted/20 px-3 py-2 text-xs'
          : item.kind === 'plan'
            ? 'w-full rounded-lg border border-border/70 bg-muted/30 px-3 py-2 text-sm'
            : 'w-full rounded-lg border border-border/70 bg-muted/30 px-3 py-2 text-sm';

  return (
    <div className={cls}>
      {item.kind === 'tool' ? (
        <ToolCard item={item} />
      ) : item.kind === 'assistant' ? (
        <Markdown content={item.content} />
      ) : item.kind === 'thought' ? (
        <ThoughtBubble content={item.content} />
      ) : item.kind === 'plan' ? (
        <PlanBubble entries={item.planEntries ?? []} />
      ) : (
        <div className="whitespace-pre-wrap">{item.content}</div>
      )}
    </div>
  );
});

/** 思考过程气泡：默认折叠（思考是低信噪过程信息），浅灰斜体小字。 */
function ThoughtBubble({ content }: { content: string }) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  return (
    <div>
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex items-center gap-1.5 text-xs italic text-muted-foreground"
        aria-expanded={open}
      >
        <Brain className="h-3 w-3 shrink-0" />
        {t('agent.thought')}
        {open ? <ChevronUp className="h-3 w-3" /> : <ChevronDown className="h-3 w-3" />}
      </button>
      {open && (
        <div className="mt-1 whitespace-pre-wrap italic text-muted-foreground">{content}</div>
      )}
    </div>
  );
}

const PLAN_MARK: Record<string, { mark: string; cls: string }> = {
  completed: { mark: '✓', cls: 'text-green-600' },
  in_progress: { mark: '▶', cls: 'text-primary' },
  pending: { mark: '○', cls: 'text-muted-foreground' },
};

/** 计划气泡：checklist 样式；新 plan 帧由 ChatStream 就地更新本气泡内容。 */
function PlanBubble({ entries }: { entries: PlanEntryItem[] }) {
  const { t } = useTranslation();
  return (
    <div>
      <div className="mb-1 flex items-center gap-1.5 text-xs font-medium text-muted-foreground">
        <ListChecks className="h-3.5 w-3.5" />
        {t('agent.plan')}
      </div>
      <ul className="space-y-0.5">
        {entries.map((e, i) => {
          const mark = PLAN_MARK[e.status] ?? PLAN_MARK.pending;
          return (
            <li key={i} className="flex items-baseline gap-2 text-xs">
              <span className={mark.cls}>{mark.mark}</span>
              <span className={e.status === 'completed' ? 'text-muted-foreground line-through' : ''}>
                {e.content}
              </span>
            </li>
          );
        })}
      </ul>
    </div>
  );
}
