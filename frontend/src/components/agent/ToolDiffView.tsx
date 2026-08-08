import { memo, useMemo } from 'react';
import { FileDiff } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import type { ToolDiff } from './types';

/** 渲染行：add/del/context（对齐 LCS 输出）。 */
interface DiffLine {
  type: 'add' | 'del' | 'context';
  text: string;
}

/** old/new 按行做 LCS 对齐 → 渲染行序列。纯文本行数有限（工具 diff 片段），
 *  O(n*m) DP 足够；超 500 行降级为「全删全加」避免卡顿。 */
function alignLines(oldText: string | null, newText: string | null): DiffLine[] {
  const oldLines = oldText?.split('\n') ?? [];
  const newLines = newText?.split('\n') ?? [];
  if (oldLines.length === 0) return newLines.map((t) => ({ type: 'add', text: t }));
  if (newLines.length === 0) return oldLines.map((t) => ({ type: 'del', text: t }));
  if (oldLines.length * newLines.length > 500 * 500) {
    return [
      ...oldLines.map((t): DiffLine => ({ type: 'del', text: t })),
      ...newLines.map((t): DiffLine => ({ type: 'add', text: t })),
    ];
  }
  // LCS 长度表
  const n = oldLines.length;
  const m = newLines.length;
  const dp: number[][] = Array.from({ length: n + 1 }, () => new Array<number>(m + 1).fill(0));
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      dp[i][j] =
        oldLines[i] === newLines[j]
          ? dp[i + 1][j + 1] + 1
          : Math.max(dp[i + 1][j], dp[i][j + 1]);
    }
  }
  const out: DiffLine[] = [];
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (oldLines[i] === newLines[j]) {
      out.push({ type: 'context', text: oldLines[i] });
      i++;
      j++;
    } else if (dp[i + 1][j] >= dp[i][j + 1]) {
      out.push({ type: 'del', text: oldLines[i] });
      i++;
    } else {
      out.push({ type: 'add', text: newLines[j] });
      j++;
    }
  }
  while (i < n) out.push({ type: 'del', text: oldLines[i++] });
  while (j < m) out.push({ type: 'add', text: newLines[j++] });
  return out;
}

const LINE_CLASS: Record<DiffLine['type'], string> = {
  add: 'diff-line-add bg-green-500/10 text-green-700 dark:text-green-400',
  del: 'diff-line-del bg-red-500/10 text-red-700 dark:text-red-400',
  context: 'text-muted-foreground',
};

const LINE_PREFIX: Record<DiffLine['type'], string> = { add: '+ ', del: '- ', context: '  ' };

/** 单文件 diff 块：路径头 + old/new 行级对齐视图。 */
function DiffBlock({ diff }: { diff: ToolDiff }) {
  const { t } = useTranslation();
  const lines = useMemo(() => alignLines(diff.old_text, diff.new_text), [diff]);
  const badge =
    diff.old_text === null
      ? t('agent.diffNewFile')
      : diff.new_text === null
        ? t('agent.diffRemovedFile')
        : null;
  return (
    <div className="overflow-hidden rounded-md border border-border/60">
      <div className="flex items-center gap-1.5 border-b border-border/60 bg-muted/50 px-2 py-1 font-mono text-xs">
        <FileDiff className="h-3 w-3 shrink-0 text-muted-foreground" />
        <span className="min-w-0 truncate">{diff.path}</span>
        {badge && <span className="ml-auto shrink-0 text-muted-foreground">{badge}</span>}
      </div>
      <pre className="max-h-72 overflow-auto px-2 py-1 font-mono text-xs leading-relaxed">
        {lines.map((line, i) => (
          <div key={i} className={LINE_CLASS[line.type]}>
            {LINE_PREFIX[line.type]}
            {line.text}
          </div>
        ))}
      </pre>
    </div>
  );
}

/** 工具卡片里的 diff 列表（每个受影响文件一块）。 */
export default memo(function ToolDiffView({ diffs }: { diffs: ToolDiff[] }) {
  return (
    <div className="space-y-2">
      {diffs.map((d, i) => (
        <DiffBlock key={`${d.path}-${i}`} diff={d} />
      ))}
    </div>
  );
});
