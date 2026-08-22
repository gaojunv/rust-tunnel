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

/** 基于 alignLines 的左右配对视图：context 行左右同显；连续 del/add 段 zip 对齐。 */
interface PairedRow {
  left: { type: 'del' | 'context'; text: string } | null;
  right: { type: 'add' | 'context'; text: string } | null;
}

function toPairedRows(lines: DiffLine[]): PairedRow[] {
  const rows: PairedRow[] = [];
  for (let i = 0; i < lines.length; ) {
    if (lines[i].type === 'context') {
      rows.push({
        left: { type: 'context', text: lines[i].text },
        right: { type: 'context', text: lines[i].text },
      });
      i++;
    } else {
      const dels: string[] = [];
      const adds: string[] = [];
      while (i < lines.length && lines[i].type !== 'context') {
        if (lines[i].type === 'del') dels.push(lines[i].text);
        else adds.push(lines[i].text);
        i++;
      }
      const n = Math.max(dels.length, adds.length);
      for (let k = 0; k < n; k++) {
        rows.push({
          left: k < dels.length ? { type: 'del' as const, text: dels[k] } : null,
          right: k < adds.length ? { type: 'add' as const, text: adds[k] } : null,
        });
      }
    }
  }
  return rows;
}

const CELL_BASE = 'min-w-[16rem] whitespace-pre px-2 font-mono text-xs';

function sideClass(
  side: PairedRow['left'] | PairedRow['right'],
): string {
  if (!side) return 'bg-muted/20';
  return LINE_CLASS[side.type];
}

function sidePrefix(
  side: PairedRow['left'] | PairedRow['right'],
): string {
  if (!side) return '';
  return LINE_PREFIX[side.type];
}

/** 单文件 diff 块：路径头 + old/new 行级对齐。桌面（≥md）双栏 grid；移动（<md）单列 unified。 */
function DiffBlock({ diff }: { diff: ToolDiff }) {
  const { t } = useTranslation();
  const lines = useMemo(() => alignLines(diff.old_text, diff.new_text), [diff]);
  const rows = useMemo(() => toPairedRows(lines), [lines]);
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
      {/* 桌面双栏（≥md）：单一 grid 容器 + 扁平格子（列宽跨行统一——按行各起一个
          grid 会让长行撑宽自己那一行，divide 竖线锯齿）；min-w-max 让 grid 按内容
          撑开，横向滚动在外层容器。中缝竖线由左格 border-r 承担（divide-x 在扁平
          格子上会跨行错位）。 */}
      <div className="hidden max-h-72 overflow-auto md:block">
        <div className="grid w-full min-w-max grid-cols-2">
          {rows.flatMap((row, i) => [
            <div key={`l${i}`} className={`${CELL_BASE} border-r border-border/60 ${sideClass(row.left)}`}>
              {row.left ? `${sidePrefix(row.left)}${row.left.text}` : ''}
            </div>,
            <div key={`r${i}`} className={`${CELL_BASE} ${sideClass(row.right)}`}>
              {row.right ? `${sidePrefix(row.right)}${row.right.text}` : ''}
            </div>,
          ])}
        </div>
      </div>
      {/* 移动单列 unified（<md）：既有样式原样保留，容器加 md:hidden。 */}
      <pre className="max-h-72 overflow-auto px-2 py-1 font-mono text-xs leading-relaxed md:hidden">
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
