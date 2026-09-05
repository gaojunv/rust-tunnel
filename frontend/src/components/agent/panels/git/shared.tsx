import { useCallback, useMemo, useRef, useState, type ReactNode } from 'react';
import { useQuery } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import {
  ChevronDown,
  ChevronRight,
  Copy,
  FileText,
  MoreHorizontal,
  RefreshCw,
} from 'lucide-react';
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
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from '../../../../components/ui/dropdown-menu';
import type { GitStatusKind } from './gitUtils';
import type { ApprovalState } from '../useApprovalMutation';

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
      <span className="text-xs font-semibold uppercase tracking-wide text-muted-foreground/70">
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
            'flex-1 rounded-t px-1 py-1 text-xs font-medium text-muted-foreground transition-colors hover:text-foreground',
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

/** 复制文本到剪贴板（兼容非安全上下文回退）。 */
async function copyToClipboard(text: string): Promise<void> {
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text);
      return;
    }
  } catch {
    // ignore and fallback
  }
  const ta = document.createElement('textarea');
  ta.value = text;
  ta.style.position = 'fixed';
  ta.style.opacity = '0';
  document.body.appendChild(ta);
  ta.select();
  document.execCommand('copy');
  ta.remove();
}

/** diff 行号元信息：解析 hunk 头推算左右行号。 */
interface DiffLineInfo {
  text: string;
  leftNum: number | null;
  rightNum: number | null;
}

function buildDiffLinesClean(raw: string): DiffLineInfo[] {
  const rawLines = raw.split('\n');
  const out: DiffLineInfo[] = [];
  let left = 0;
  let right = 0;
  let inHunk = false;
  for (const line of rawLines) {
    if (line.startsWith('@@')) {
      const m = line.match(/@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/);
      if (m) {
        left = parseInt(m[1], 10);
        right = parseInt(m[2], 10);
        inHunk = true;
      }
      out.push({ text: line, leftNum: null, rightNum: null });
      continue;
    }
    if (!inHunk) {
      out.push({ text: line, leftNum: null, rightNum: null });
      continue;
    }
    if (line.startsWith('+') && !line.startsWith('+++')) {
      out.push({ text: line, leftNum: null, rightNum: right });
      right += 1;
      continue;
    }
    if (line.startsWith('-') && !line.startsWith('---')) {
      out.push({ text: line, leftNum: left, rightNum: null });
      left += 1;
      continue;
    }
    if (line.startsWith(' ')) {
      out.push({ text: line, leftNum: left, rightNum: right });
      left += 1;
      right += 1;
      continue;
    }
    if (line === '') {
      // 空行视为上下文空行（git diff 尾部常见）
      out.push({ text: line, leftNum: null, rightNum: null });
      continue;
    }
    out.push({ text: line, leftNum: null, rightNum: null });
  }
  return out;
}

/** 单文件 diff 渲染：cached=true 取 staged diff（GitExec 路径）。含行号列与变更色条。 */
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

  const raw = diffQuery.data ?? '';
  const lines = raw.split('\n');
  if (lines.length <= 1 && lines[0].trim() === '') {
    return (
      <div className="px-2 py-1 text-xs text-muted-foreground">{t('agent.diffEmpty')}</div>
    );
  }

  const infos = buildDiffLinesClean(raw);

  return (
    <pre className="overflow-x-auto rounded-md bg-muted/40 px-2 py-1 font-mono text-xs leading-relaxed">
      {infos.map((info, i) => {
        const isAdd = info.text.startsWith('+') && !info.text.startsWith('+++');
        const isDel = info.text.startsWith('-') && !info.text.startsWith('---');
        const isHunk = info.text.startsWith('@@');
        return (
          <div
            key={i}
            className={cn('flex gap-1', diffLineClass(info.text))}
          >
            {/* 行号列：左/右各 3 字符宽，灰色 */}
            <span className="flex shrink-0 select-none gap-1 text-[11px] text-muted-foreground/60">
              <span className="inline-block w-7 text-right tabular-nums">
                {info.leftNum != null ? String(info.leftNum) : ''}
              </span>
              <span className="inline-block w-7 text-right tabular-nums">
                {info.rightNum != null ? String(info.rightNum) : ''}
              </span>
              {/* 变更色条 */}
              <span
                className={cn(
                  'inline-block w-0.5 self-stretch rounded',
                  isAdd && 'bg-green-500',
                  isDel && 'bg-red-500',
                  isHunk && 'bg-blue-500',
                )}
              />
            </span>
            <span className="min-w-0 flex-1 whitespace-pre-wrap break-all">
              {info.text === '' ? ' ' : info.text}
            </span>
          </div>
        );
      })}
    </pre>
  );
}

/** 状态条目行 hover 操作按钮的定义。 */
export interface EntryRowAction {
  label: string;
  icon: ReactNode;
  onClick: () => void;
}

/** 右键/⋯ 菜单项定义。 */
export interface EntryRowMenuItem {
  label: string;
  icon?: ReactNode;
  onClick: () => void;
}

/** 状态条目行：主按钮点击展开/折叠；hover/常显操作按钮；右键菜单与移动端 ⋯ 菜单。 */
export function EntryRow({
  path,
  status,
  expanded,
  onToggle,
  action,
  menuItems,
  onOpenFile,
  children,
}: {
  path: string;
  status: GitStatusKind;
  expanded: boolean;
  onToggle: () => void;
  /** hover 操作按钮；action.label 为无障碍名。 */
  action?: EntryRowAction;
  /** 右键/⋯ 菜单项（未提供时按默认三项：打开文件/暂存切换/复制路径）。 */
  menuItems?: EntryRowMenuItem[];
  /** 打开文件回调（供默认菜单项使用）；未提供则打开文件项不显示。 */
  onOpenFile?: () => void;
  children?: ReactNode;
}) {
  const { t } = useTranslation();
  const badge = BADGE[status];
  const [menuOpen, setMenuOpen] = useState(false);
  const rowRef = useRef<HTMLDivElement>(null);

  const defaultMenuItems: EntryRowMenuItem[] = useMemo(() => {
    if (menuItems) return menuItems;
    const items: EntryRowMenuItem[] = [];
    if (onOpenFile) {
      items.push({ label: t('agent.gitOpenFile'), icon: <FileText className="h-3 w-3" />, onClick: onOpenFile });
    }
    if (action) {
      items.push({ label: action.label, icon: action.icon, onClick: action.onClick });
    }
    items.push({
      label: t('agent.gitCopyPath'),
      icon: <Copy className="h-3 w-3" />,
      onClick: () => void copyToClipboard(path),
    });
    return items;
  }, [menuItems, onOpenFile, action, path, t]);

  const handleContextMenu = useCallback(
    (e: React.MouseEvent) => {
      if (defaultMenuItems.length === 0) return;
      e.preventDefault();
      setMenuOpen(true);
    },
    [defaultMenuItems.length],
  );

  // 点击外部关闭由 DropdownMenu 自身处理；此处仅需右键触发

  return (
    <div>
      <div
        ref={rowRef}
        className="group flex items-center gap-0.5 rounded px-0.5 hover:bg-accent/50"
        onContextMenu={handleContextMenu}
      >
        <button
          type="button"
          onClick={onToggle}
          aria-expanded={expanded}
          className="flex min-w-0 flex-1 items-center gap-1.5 rounded py-0.5 text-left min-h-[28px] md:min-h-[28px]"
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
            className="shrink-0 rounded p-1 text-muted-foreground opacity-100 md:opacity-0 md:group-hover:opacity-100 transition-opacity hover:bg-accent hover:text-foreground"
          >
            {action.icon}
          </button>
        )}
        {/* 移动端 ⋯ 菜单按钮（桌面 hover 揭示，移动端常显） */}
        {defaultMenuItems.length > 0 && (
          <DropdownMenu open={menuOpen} onOpenChange={setMenuOpen}>
            <DropdownMenuTrigger asChild>
              <button
                type="button"
                aria-label={t('agent.gitMoreActions')}
                title={t('agent.gitMoreActions')}
                onClick={(e) => e.stopPropagation()}
                className="shrink-0 rounded p-1 text-muted-foreground opacity-100 md:opacity-0 md:group-hover:opacity-100 transition-opacity hover:bg-accent hover:text-foreground"
              >
                <MoreHorizontal className="h-3 w-3" />
              </button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end" className="min-w-36">
              {defaultMenuItems.map((item, idx) => (
                <DropdownMenuItem
                  key={idx}
                  onSelect={() => item.onClick()}
                  className="gap-2 text-xs"
                >
                  {item.icon}
                  {item.label}
                </DropdownMenuItem>
              ))}
            </DropdownMenuContent>
          </DropdownMenu>
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
        <span className="text-muted-foreground/70 text-[11px]">{count}</span>
        {action && (
          <button
            type="button"
            aria-label={action.label}
            title={action.label}
            onClick={action.onClick}
            className="rounded px-1 text-[11px] text-muted-foreground opacity-100 md:opacity-0 md:group-hover:opacity-100 transition-opacity hover:text-foreground"
          >
            {action.label}
          </button>
        )}
      </div>
      {!collapsed && <div className="space-y-0.5">{children}</div>}
    </div>
  );
}

/** 审批确认对话框：显示后端返回的写操作摘要，确认后带 approved=true 重发。
 *  git / GitHub Actions 面板复用；`descKey` 可覆盖说明文案（默认 git）。 */
export function ApprovalDialog({
  approval,
  onConfirm,
  onCancel,
  descKey = 'agent.gitApprovalDesc',
}: {
  approval: ApprovalState | null;
  onConfirm: () => void;
  onCancel: () => void;
  /** 说明文案 i18n key（git 面板默认；GitHub 面板传 github 专属文案）。 */
  descKey?: string;
}) {
  const { t } = useTranslation();
  // descKey 为调用方传入的动态 i18n key，需宽签名 t
  const translate = t as (key: string) => string;
  return (
    <Dialog open={approval !== null} onOpenChange={(open) => !open && onCancel()}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>{t('agent.approvalRequired')}</DialogTitle>
          <DialogDescription>{translate(descKey)}</DialogDescription>
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

/** 向后兼容导出（既有 git 调用方/测试引用 GitApprovalDialog）。 */
export const GitApprovalDialog = ApprovalDialog;
