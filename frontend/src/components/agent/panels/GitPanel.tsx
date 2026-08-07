import { useState } from 'react';
import type { ReactNode } from 'react';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import { ChevronDown, ChevronRight, RefreshCw } from 'lucide-react';
import { cn } from '@/lib/utils';
import {
  getAgentGitDiff,
  getAgentGitStatus,
  listAgentMessages,
} from '../../../api/client';
import type { AgentMessage } from '../../../types';

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

function headerBranch(status: string): string | null {
  const line = status.split('\n').find((l) => l.startsWith('## '));
  return line ? line.slice(3).trim() : null;
}

function diffLineClass(line: string): string {
  if (line.startsWith('@@')) return 'text-blue-500';
  if (line.startsWith('+') && !line.startsWith('+++')) {
    return 'text-green-600 dark:text-green-400';
  }
  if (line.startsWith('-') && !line.startsWith('---')) return 'text-red-600';
  return '';
}

const BADGE: Record<GitStatusKind, { label: string; className: string }> = {
  modified: {
    label: 'M',
    className: 'bg-yellow-500/15 text-yellow-600 dark:text-yellow-400',
  },
  added: {
    label: 'A',
    className: 'bg-green-500/15 text-green-600 dark:text-green-400',
  },
  deleted: {
    label: 'D',
    className: 'bg-red-500/15 text-red-600 dark:text-red-400',
  },
  renamed: {
    label: 'R',
    className: 'bg-blue-500/15 text-blue-600 dark:text-blue-400',
  },
  untracked: {
    label: 'U',
    className: 'bg-gray-500/15 text-gray-500 dark:text-gray-400',
  },
  other: {
    label: '?',
    className: 'bg-gray-500/15 text-gray-500 dark:text-gray-400',
  },
};

interface ToolLog {
  name: string;
  args: string;
  result: string;
}

function latestGitStatus(messages: AgentMessage[]): string | null {
  let latest: string | null = null;
  for (const m of messages ?? []) {
    if (m.kind === 'tool_result' && m.name === 'git_status') {
      latest = m.content;
    } else if ((m.kind === 'tool' || m.role === 'tool') && m.tool_calls) {
      // 旧格式兼容
      try {
        const logs = JSON.parse(m.tool_calls) as ToolLog[];
        for (const log of logs) {
          if (log.name === 'git_status') latest = log.result;
        }
      } catch {
        /* ignore malformed tool_calls */
      }
    }
  }
  return latest;
}

function FallbackGitStatus({ messages }: { messages: AgentMessage[] }) {
  const { t } = useTranslation();
  const latest = latestGitStatus(messages);
  return latest ? (
    <pre className="whitespace-pre-wrap rounded-md bg-muted p-2 font-mono text-xs">
      {latest}
    </pre>
  ) : (
    <p className="text-xs text-muted-foreground">{t('agent.noGitStatus')}</p>
  );
}

function GitToolbar({
  onRefresh,
  refreshLabel,
}: {
  onRefresh: () => void;
  refreshLabel: string;
}) {
  const { t } = useTranslation();
  return (
    <div className="flex items-center justify-between px-1 py-0.5">
      <span className="text-[10px] font-semibold uppercase tracking-wide text-muted-foreground/70">
        {t('agent.git')}
      </span>
      <button
        type="button"
        aria-label={refreshLabel}
        title={refreshLabel}
        onClick={onRefresh}
        className="rounded p-1 text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
      >
        <RefreshCw className="h-3.5 w-3.5" />
      </button>
    </div>
  );
}

function DiffView({ workspaceId, path }: { workspaceId: string; path: string }) {
  const { t } = useTranslation();
  const diffQuery = useQuery({
    queryKey: ['agent-git-diff', workspaceId, path],
    queryFn: () => getAgentGitDiff(workspaceId, path),
    retry: false,
  });

  if (diffQuery.isLoading) {
    return (
      <div className="px-2 py-1 text-xs text-muted-foreground">{t('common.loading')}</div>
    );
  }
  if (diffQuery.isError) {
    return (
      <div className="px-2 py-1 text-xs text-muted-foreground">{t('agent.noGitStatus')}</div>
    );
  }

  const lines = (diffQuery.data ?? '').split('\n');
  if (lines.length <= 1 && lines[0].trim() === '') {
    return (
      <div className="px-2 py-1 text-xs text-muted-foreground">{t('agent.diffEmpty')}</div>
    );
  }

  return (
    <pre className="overflow-x-auto rounded-md bg-muted/40 px-2 py-1 font-mono text-xs leading-relaxed">
      {lines.map((line, i) => (
        <div key={i} className={diffLineClass(line)}>
          {line === '' ? ' ' : line}
        </div>
      ))}
    </pre>
  );
}

function EntryRow({
  entry,
  expanded,
  onToggle,
  children,
}: {
  entry: GitEntry;
  expanded: boolean;
  onToggle: () => void;
  children?: ReactNode;
}) {
  const badge = BADGE[entry.status];
  return (
    <div>
      <button
        type="button"
        onClick={onToggle}
        aria-expanded={expanded}
        className="flex w-full items-center gap-1.5 rounded px-1 py-0.5 text-left hover:bg-accent/50"
      >
        {expanded ? (
          <ChevronDown className="h-3 w-3 shrink-0 text-muted-foreground" />
        ) : (
          <ChevronRight className="h-3 w-3 shrink-0 text-muted-foreground" />
        )}
        <span
          className={cn(
            'inline-flex h-4 w-4 shrink-0 items-center justify-center rounded text-[10px] font-bold',
            badge.className
          )}
        >
          {badge.label}
        </span>
        <span className="truncate font-mono text-xs">{entry.path}</span>
      </button>
      {expanded && children}
    </div>
  );
}

function GitGroup({
  label,
  count,
  collapsed,
  onToggle,
  children,
}: {
  label: string;
  count: number;
  collapsed: boolean;
  onToggle: () => void;
  children: ReactNode;
}) {
  return (
    <div>
      <button
        type="button"
        onClick={onToggle}
        aria-expanded={!collapsed}
        className="flex w-full items-center gap-1 px-1 py-0.5 text-left text-xs font-semibold text-foreground/80 hover:bg-accent/50"
      >
        {collapsed ? (
          <ChevronRight className="h-3 w-3 shrink-0 text-muted-foreground" />
        ) : (
          <ChevronDown className="h-3 w-3 shrink-0 text-muted-foreground" />
        )}
        <span className="truncate">{label}</span>
        <span className="ml-auto text-muted-foreground/70">{count}</span>
      </button>
      {!collapsed && <div className="space-y-0.5">{children}</div>}
    </div>
  );
}

type GroupKey = 'staged' | 'changes' | 'untracked';

type GitGroupI18nKey = 'agent.stagedChanges' | 'agent.changes' | 'agent.untracked';

const GROUPS: { key: GroupKey; i18nKey: GitGroupI18nKey }[] = [
  { key: 'staged', i18nKey: 'agent.stagedChanges' },
  { key: 'changes', i18nKey: 'agent.changes' },
  { key: 'untracked', i18nKey: 'agent.untracked' },
];

function groupOf(entry: GitEntry): GroupKey {
  if (entry.status === 'untracked') return 'untracked';
  return entry.staged ? 'staged' : 'changes';
}

export default function GitPanel({
  sessionId,
  workspaceId,
}: {
  sessionId: string;
  workspaceId: string;
}) {
  // 面板容器（ActivityBar）已无内边距/滚动，组件自补。
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [expandedPath, setExpandedPath] = useState<string | null>(null);
  const [collapsed, setCollapsed] = useState<Record<string, boolean>>({});

  const statusQuery = useQuery({
    queryKey: ['agent-git-status', workspaceId],
    queryFn: () => getAgentGitStatus(workspaceId),
    retry: false,
  });

  // 仅回退路径需要消息：主 API 不可用（如客户端离线 503）时才拉取
  const messagesQuery = useQuery<AgentMessage[]>({
    queryKey: ['agent-messages', sessionId],
    queryFn: () => listAgentMessages(sessionId),
    enabled: statusQuery.isError,
    retry: false,
  });

  const refresh = () => {
    queryClient.invalidateQueries({ queryKey: ['agent-git-status'] });
    queryClient.invalidateQueries({ queryKey: ['agent-git-diff'] });
  };

  // 回退：保留旧行为——展示缓存里最近一次 git_status 工具结果原文
  if (statusQuery.isError) {
    return (
      <div className="overflow-y-auto p-2">
        <FallbackGitStatus messages={messagesQuery.data ?? []} />
      </div>
    );
  }

  if (statusQuery.isLoading) {
    return (
      <div className="px-1 py-2 text-xs text-muted-foreground">{t('common.loading')}</div>
    );
  }

  const { status, stderr } = statusQuery.data ?? { status: '', stderr: '' };

  if (status.trim() === '' && stderr.trim() !== '') {
    return (
      <div className="space-y-2">
        <GitToolbar onRefresh={refresh} refreshLabel={t('agent.refresh')} />
        <p className="px-1 text-xs text-muted-foreground">{t('agent.notGitRepo')}</p>
      </div>
    );
  }

  const entries = parsePorcelainEntries(status);
  const branch = headerBranch(status);

  return (
    <div className="space-y-1 overflow-y-auto p-2">
      <GitToolbar onRefresh={refresh} refreshLabel={t('agent.refresh')} />
      {branch && (
        <p className="truncate px-1 text-[10px] uppercase tracking-wide text-muted-foreground/70">
          {branch}
        </p>
      )}
      {entries.length === 0 && !branch && (
        <p className="px-1 text-xs text-muted-foreground">{t('agent.noGitStatus')}</p>
      )}
      {GROUPS.map(({ key, i18nKey }) => {
        const items = entries.filter((e) => groupOf(e) === key);
        if (items.length === 0) return null;
        return (
          <GitGroup
            key={key}
            label={t(i18nKey)}
            count={items.length}
            collapsed={!!collapsed[key]}
            onToggle={() => setCollapsed((c) => ({ ...c, [key]: !c[key] }))}
          >
            {items.map((entry) => (
              <EntryRow
                key={entry.path}
                entry={entry}
                expanded={expandedPath === entry.path}
                onToggle={() =>
                  setExpandedPath((cur) => (cur === entry.path ? null : entry.path))
                }
              >
                {expandedPath === entry.path && (
                  <DiffView workspaceId={workspaceId} path={entry.path} />
                )}
              </EntryRow>
            ))}
          </GitGroup>
        );
      })}
    </div>
  );
}
