import { formatRelativeTime, type TranslateFn } from '../../formatRelativeTime';

export type GitStatusKind =
  | 'modified'
  | 'added'
  | 'deleted'
  | 'renamed'
  | 'untracked'
  | 'other';

export interface GitEntry {
  path: string;
  x: string;
  y: string;
  status: GitStatusKind;
  staged: boolean;
}

function normalizeStatus(x: string, y: string): GitStatusKind {
  if (x === '?' && y === '?') return 'untracked';
  if (x === 'R' || y === 'R') return 'renamed';
  if (x === 'M' || y === 'M') return 'modified';
  if (x === 'A' || y === 'A') return 'added';
  if (x === 'D' || y === 'D') return 'deleted';
  return 'other';
}

/**
 * 解析 `git status --porcelain=v1 -b` 原文为条目列表。
 * 跳过 `## ` 分支头行；`?? path` → untracked；`XY path` 两字符状态；
 * 重命名行 `R  old -> new` 的 path 取 new。
 */
export function parsePorcelainEntries(status: string): GitEntry[] {
  const entries: GitEntry[] = [];
  for (const rawLine of status.split('\n')) {
    const line = rawLine.endsWith('\r') ? rawLine.slice(0, -1) : rawLine;
    if (line === '' || line.startsWith('## ')) continue;

    if (line.startsWith('?? ')) {
      entries.push({
        path: line.slice(3),
        x: '?',
        y: '?',
        status: 'untracked',
        staged: false,
      });
      continue;
    }

    // porcelain 行最小形态为 "XY path"（至少 4 字符）
    if (line.length < 4) continue;
    const x = line[0];
    const y = line[1];
    let path = line.slice(3);
    // 重命名：`R  old -> new`（仅重命名行才按 ` -> ` 拆分，避免误伤普通路径）
    if ((x === 'R' || y === 'R') && path.includes(' -> ')) {
      path = path.slice(path.lastIndexOf(' -> ') + 4);
    }
    entries.push({
      path,
      x,
      y,
      status: normalizeStatus(x, y),
      staged: x !== ' ' && x !== '?',
    });
  }
  return entries;
}

/** 从 status 原文取 `## <分支名>` 头行（含 upstream 部分，如 `main...origin/main`）。 */
export function headerBranch(status: string): string | null {
  const line = status.split('\n').find((l) => l.startsWith('## '));
  return line ? line.slice(3).trim() : null;
}

/** 分支头行的纯分支名（`main...origin/main` → `main`；含 `[ahead 1]` 时取 `#` 前段）。 */
export function branchNameFromHeader(header: string | null): string | null {
  if (!header) return null;
  const name = header.split('...')[0] ?? header;
  const hashIdx = name.indexOf('#');
  return hashIdx >= 0 ? name.slice(0, hashIdx).trim() : name.trim();
}

/** 从分支头行解析 ahead/behind 计数（`[ahead 2, behind 1]`），缺省为 0。 */
export function parseAheadBehind(header: string | null): { ahead: number; behind: number } {
  if (!header) return { ahead: 0, behind: 0 };
  const aheadMatch = header.match(/ahead (\d+)/);
  const behindMatch = header.match(/behind (\d+)/);
  return {
    ahead: aheadMatch ? parseInt(aheadMatch[1], 10) : 0,
    behind: behindMatch ? parseInt(behindMatch[1], 10) : 0,
  };
}

/** 相对时间格式化 key（分钟/小时/天档）。 */
export function formatCommitDate(date: string, now: number, t: TranslateFn): string {
  const ts = Date.parse(date);
  if (Number.isNaN(ts)) return '';
  return formatRelativeTime(ts, now, t);
}
