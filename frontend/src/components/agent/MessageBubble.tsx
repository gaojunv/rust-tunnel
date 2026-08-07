import { memo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { ChevronDown, ChevronUp, Loader2, Wrench } from 'lucide-react';
import type { ChatItem } from './types';
import Markdown from './Markdown';

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

/** 工具调用卡片：默认收起为一行头部（工具名 + 摘要），点击展开 args/result。 */
function ToolCard({ item }: { item: ChatItem }) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const summary = toolSummary(item.toolName, item.toolArgs);

  return (
    <div>
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex w-full items-center gap-2 text-left text-xs"
        aria-expanded={open}
      >
        <Wrench className="h-3.5 w-3.5 shrink-0 text-primary" />
        <span className="font-medium text-foreground/90">{item.toolName}</span>
        {summary && (
          <span className="min-w-0 truncate font-mono text-muted-foreground">{summary}</span>
        )}
        <span className="ml-auto flex shrink-0 items-center gap-1.5">
          {!item.toolResult && <Loader2 className="h-3 w-3 animate-spin text-muted-foreground" />}
          {open ? (
            <ChevronUp className="h-3.5 w-3.5 text-muted-foreground" />
          ) : (
            <ChevronDown className="h-3.5 w-3.5 text-muted-foreground" />
          )}
        </span>
      </button>
      {open && (
        <div className="mt-2 border-t border-border/60 pt-2">
          {item.toolArgs && <CollapsiblePre text={item.toolArgs} />}
          {item.toolResult ? (
            <CollapsiblePre text={item.toolResult} className={item.toolArgs ? 'mt-2 border-t border-border/60 pt-2' : undefined} />
          ) : (
            <div className="mt-1 flex items-center gap-1 text-xs text-muted-foreground">
              <Loader2 className="h-3 w-3 animate-spin" />
              {t('agent.toolRunning')}
            </div>
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
        : 'w-full rounded-lg border border-border/70 bg-muted/30 px-3 py-2 text-sm';

  return (
    <div className={cls}>
      {item.kind === 'tool' ? (
        <ToolCard item={item} />
      ) : item.kind === 'assistant' ? (
        <Markdown content={item.content} />
      ) : (
        <div className="whitespace-pre-wrap">{item.content}</div>
      )}
    </div>
  );
});
