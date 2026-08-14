import type { ReactNode } from 'react';
import { useQuery } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import { ChevronDown, ChevronRight, RefreshCw } from 'lucide-react';
import { cn } from '@/lib/utils';
import { getAgentGitDiff } from '../../../../api/client';
import { Button } from '../../../../components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '../../../../components/ui/dialog';
import type { GitStatusKind } from './gitUtils';
import type { GitApprovalState } from './useGitMutation';

export const BADGE: Record<GitStatusKind, { label: string; className: string }> = {
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

export function diffLineClass(line: string): string {
  if (line.startsWith('@@')) return 'text-blue-500';
  if (line.startsWith('+') && !line.startsWith('+++')) {
    return 'text-green-600 dark:text-green-400';
  }
  if (line.startsWith('-') && !line.startsWith('---')) return 'text-red-600';
  return '';
}

/** 面板通用工具行：标题（可选）+ 刷新按钮（可选）+ 右侧自定义操作。 */
export function GitToolbar({
  title,
  onRefresh,
  refreshLabel,
  right,
}: {
  title?: string;
  onRefresh?: () => void;
  refreshLabel?: string;
  right?: ReactNode;
}) {
  const { t } = useTranslation();
  return (
    <div className="flex items-center justify-between px-1 py-0.5">
      <span className="text-[10px] font-semibold uppercase tracking-wide text-muted-foreground/70">
        {title ?? t('agent.git')}
      </span>
      <div className="flex items-center gap-0.5">
        {right}
        {onRefresh && (
          <button
            type="button"
            aria-label={refreshLabel}
            title={refreshLabel}
            onClick={onRefresh}
            className="rounded p-1 text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
          >
            <RefreshCw className="h-3.5 w-3.5" />
          </button>
        )}
      </div>
    </div>
  );
}

/** 面板 Tab 栏：Changes / Branches / History / Stash（紧凑分段控件）。 */
export function GitTabBar<T extends string>({
  tabs,
  active,
  onChange,
}: {
  tabs: { key: T; label: string }[];
  active: T;
  onChange: (key: T) => void;
}) {
  return (
    <div
      role="tablist"
      aria-label="git-panel-tabs"
      className="flex border-b border-border/60 px-1"
    >
      {tabs.map((tab) => (
        <button
          key={tab.key}
          type="button"
          role="tab"
          aria-selected={active === tab.key}
          onClick={() => onChange(tab.key)}
          className={cn(
            'flex-1 rounded-t px-1 py-1 text-[11px] font-medium text-muted-foreground transition-colors hover:text-foreground',
            active === tab.key &&
              'border-b-2 border-primary bg-accent/40 text-foreground'
          )}
        >
          {tab.label}
        </button>
      ))}
    </div>
  );
}

/** 单文件 diff 渲染：cached=true 取 staged diff（GitExec 路径）。 */
export function DiffView({
  workspaceId,
  path,
  cached,
}: {
  workspaceId: string;
  path: string;
  cached?: boolean;
}) {
  const { t } = useTranslation();
  const diffQuery = useQuery({
    queryKey: ['agent-git-diff', workspaceId, path, cached ? 'cached' : 'worktree'],
    queryFn: () => getAgentGitDiff(workspaceId, path, cached),
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

/** 状态条目行 hover 操作按钮的定义。 */
export interface EntryRowAction {
  label: string;
  icon: ReactNode;
  onClick: () => void;
}

/** 状态条目行：主按钮点击展开/折叠；hover 出现可选的操作小按钮（如暂存/取消暂存）。 */
export function EntryRow({
  path,
  status,
  expanded,
  onToggle,
  action,
  children,
}: {
  path: string;
  status: GitStatusKind;
  expanded: boolean;
  onToggle: () => void;
  /** hover 操作按钮；action.label 为无障碍名。 */
  action?: EntryRowAction;
  children?: ReactNode;
}) {
  const badge = BADGE[status];
  return (
    <div>
      <div className="group flex items-center gap-0.5 rounded px-0.5 hover:bg-accent/50">
        <button
          type="button"
          onClick={onToggle}
          aria-expanded={expanded}
          className="flex min-w-0 flex-1 items-center gap-1.5 rounded py-0.5 text-left"
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
          <span className="truncate font-mono text-xs">{path}</span>
        </button>
        {action && (
          <button
            type="button"
            aria-label={action.label}
            title={action.label}
            onClick={action.onClick}
            className="shrink-0 rounded p-1 text-muted-foreground opacity-0 transition-opacity group-hover:opacity-100 hover:bg-accent hover:text-foreground"
          >
            {action.icon}
          </button>
        )}
      </div>
      {expanded && children}
    </div>
  );
}

/** 分组容器：组头可折叠 + 计数 + 可选「全部操作」小按钮。 */
export function GitGroup({
  label,
  count,
  collapsed,
  onToggle,
  action,
  children,
}: {
  label: string;
  count: number;
  collapsed: boolean;
  onToggle: () => void;
  /** 组头「全部暂存 / 全部取消暂存」等操作。 */
  action?: { label: string; onClick: () => void };
  children: ReactNode;
}) {
  return (
    <div>
      <div className="group flex items-center gap-1 rounded px-1 py-0.5 hover:bg-accent/50">
        <button
          type="button"
          onClick={onToggle}
          aria-expanded={!collapsed}
          className="flex min-w-0 flex-1 items-center gap-1 text-left text-xs font-semibold text-foreground/80"
        >
          {collapsed ? (
            <ChevronRight className="h-3 w-3 shrink-0 text-muted-foreground" />
          ) : (
            <ChevronDown className="h-3 w-3 shrink-0 text-muted-foreground" />
          )}
          <span className="truncate">{label}</span>
        </button>
        <span className="text-muted-foreground/70">{count}</span>
        {action && (
          <button
            type="button"
            aria-label={action.label}
            title={action.label}
            onClick={action.onClick}
            className="rounded px-1 text-[10px] text-muted-foreground opacity-0 transition-opacity group-hover:opacity-100 hover:text-foreground"
          >
            {action.label}
          </button>
        )}
      </div>
      {!collapsed && <div className="space-y-0.5">{children}</div>}
    </div>
  );
}

/** 审批确认对话框：显示后端返回的 git 命令摘要，确认后带 approved=true 重发。 */
export function GitApprovalDialog({
  approval,
  onConfirm,
  onCancel,
}: {
  approval: GitApprovalState | null;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  const { t } = useTranslation();
  return (
    <Dialog open={approval !== null} onOpenChange={(open) => !open && onCancel()}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>{t('agent.approvalRequired')}</DialogTitle>
          <DialogDescription>{t('agent.gitApprovalDesc')}</DialogDescription>
        </DialogHeader>
        <pre className="max-h-40 overflow-auto rounded-md bg-muted p-2 font-mono text-xs whitespace-pre-wrap break-all">
          {approval?.summary ?? ''}
        </pre>
        <DialogFooter>
          <Button variant="ghost" size="sm" onClick={onCancel}>
            {t('agent.cancel')}
          </Button>
          <Button variant="default" size="sm" onClick={onConfirm}>
            {t('agent.approveOnce')}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
