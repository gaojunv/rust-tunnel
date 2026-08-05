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

/** 单条消息气泡：user / assistant（Markdown）/ tool（参数 + 结果卡片）。
 *  memo 化：流式 chunk 每帧更新列表 state，内容未变的气泡跳过重渲染。 */
export default memo(function MessageBubble({ item }: { item: ChatItem }) {
  const { t } = useTranslation();
  const cls =
    item.kind === 'user'
      ? 'ml-auto max-w-[80%] rounded-lg bg-primary/10 px-3 py-2'
      : item.kind === 'assistant'
        ? 'mr-auto max-w-[80%] rounded-lg bg-muted px-3 py-2'
        : 'mr-auto max-w-[90%] rounded-lg border bg-background px-3 py-2 text-sm font-mono';

  return (
    <div className={cls}>
      {item.kind === 'tool' ? (
        <div>
          <div className="mb-1 flex items-center gap-1 text-xs font-semibold">
            <Wrench className="h-3.5 w-3.5 text-primary" />
            {item.toolName}
          </div>
          {item.toolArgs && <CollapsiblePre text={item.toolArgs} />}
          {item.toolResult ? (
            <CollapsiblePre text={item.toolResult} className="mt-2 border-t pt-2" />
          ) : (
            <div className="mt-1 flex items-center gap-1 text-xs text-muted-foreground">
              <Loader2 className="h-3 w-3 animate-spin" />
              {t('agent.toolRunning')}
            </div>
          )}
        </div>
      ) : item.kind === 'assistant' ? (
        <Markdown content={item.content} />
      ) : (
        <div className="whitespace-pre-wrap">{item.content}</div>
      )}
    </div>
  );
});
