import { useState } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import { ArrowDownToLine, ArrowUpFromLine, GitCommitHorizontal } from 'lucide-react';
import { cn } from '@/lib/utils';
import {
  postAgentGitCommit,
  postAgentGitPull,
  postAgentGitPush,
  postAgentGitStage,
  postAgentGitUnstage,
} from '../../../../api/client';
import { useGitMutation } from './useGitMutation';
import { DiffView, EntryRow, GitApprovalDialog, GitGroup, type EntryRowAction } from './shared';
import { branchNameFromHeader, type GitEntry } from './gitUtils';

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

/**
 * Changes Tab：staged / changes / untracked 三组，每行 hover 有暂存/取消暂存按钮，
 * 组头有「全部暂存 / 全部取消暂存」；staged 组点开看 cached diff，其余看工作区 diff；
 * 底部为提交区（多行 message + commit），顶栏为 pull / push。
 *
 * entries 由 GitPanel 容器解析后传入（status query 归属容器，保证非 git 仓库/
 * 回退路径统一）；写操作成功后 invalidate 容器持有的 status query。
 */
export function GitChangesTab({
  workspaceId,
  entries,
  branch,
}: {
  workspaceId: string;
  entries: GitEntry[];
  branch: string | null;
}) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [expandedPath, setExpandedPath] = useState<string | null>(null);
  const [collapsed, setCollapsed] = useState<Record<string, boolean>>({});
  const [message, setMessage] = useState('');

  // 写操作成功后统一刷新：status（容器）+ log/stash 等（commit 会推进历史）
  const invalidate = () => {
    queryClient.invalidateQueries({ queryKey: ['agent-git-status', workspaceId] });
    queryClient.invalidateQueries({ queryKey: ['agent-git-log', workspaceId] });
    queryClient.invalidateQueries({ queryKey: ['agent-git-stash', workspaceId] });
  };

  const stageMutation = useGitMutation(
    (approved, paths: string[]) => postAgentGitStage(workspaceId, paths, approved),
    { onSuccess: invalidate },
  );
  const unstageMutation = useGitMutation(
    (approved, paths: string[]) => postAgentGitUnstage(workspaceId, paths, approved),
    { onSuccess: invalidate },
  );
  const commitMutation = useGitMutation(
    (approved, msg: string) => postAgentGitCommit(workspaceId, msg, approved),
    {
      onSuccess: () => {
        invalidate();
        setMessage('');
        setExpandedPath(null);
      },
    },
  );
  const pullMutation = useGitMutation(
    (approved) => postAgentGitPull(workspaceId, approved),
    { onSuccess: invalidate },
  );
  const pushMutation = useGitMutation(
    (approved) => postAgentGitPush(workspaceId, approved),
    { onSuccess: invalidate },
  );

  const staged = entries.filter((e) => groupOf(e) === 'staged');
  const changes = entries.filter((e) => groupOf(e) === 'changes');
  const untracked = entries.filter((e) => groupOf(e) === 'untracked');

  const stageAll = (paths: string[]) => {
    if (paths.length === 0) return;
    stageMutation.mutate(paths);
  };
  const unstageAll = (paths: string[]) => {
    if (paths.length === 0) return;
    unstageMutation.mutate(paths);
  };

  const canCommit = message.trim() !== '' && !commitMutation.isPending;
  const branchName = branchNameFromHeader(branch);

  const rowAction = (entry: GitEntry): EntryRowAction => {
    if (groupOf(entry) === 'staged') {
      return {
        label: t('agent.gitUnstage'),
        icon: <ArrowUpFromLine className="h-3 w-3" />,
        onClick: () => unstageMutation.mutate([entry.path]),
      };
    }
    return {
      label: t('agent.gitStage'),
      icon: <ArrowDownToLine className="h-3 w-3" />,
      onClick: () => stageMutation.mutate([entry.path]),
    };
  };

  return (
    <div className="space-y-1.5">
      {/* 顶栏：当前分支 + pull / push（写操作走审批流） */}
      <div className="flex items-center gap-1 px-1">
        {branchName && (
          <span className="truncate font-mono text-[10px] text-muted-foreground/80">
            {branchName}
          </span>
        )}
        <div className="ml-auto flex items-center gap-1">
          <button
            type="button"
            onClick={() => pullMutation.mutate()}
            disabled={pullMutation.isPending}
            title={t('agent.gitPull')}
            aria-label={t('agent.gitPull')}
            className="rounded px-1.5 py-0.5 text-[10px] font-medium text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
          >
            {t('agent.gitPull')}
          </button>
          <button
            type="button"
            onClick={() => pushMutation.mutate()}
            disabled={pushMutation.isPending}
            title={t('agent.gitPush')}
            aria-label={t('agent.gitPush')}
            className="rounded px-1.5 py-0.5 text-[10px] font-medium text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
          >
            {t('agent.gitPush')}
          </button>
        </div>
      </div>

      {entries.length === 0 && (
        <p className="px-1 text-xs text-muted-foreground">{t('agent.noGitStatus')}</p>
      )}

      {GROUPS.map(({ key, i18nKey }) => {
        const items = key === 'staged' ? staged : key === 'changes' ? changes : untracked;
        if (items.length === 0) return null;
        const groupAction =
          key === 'staged'
            ? {
                label: t('agent.gitUnstageAll'),
                onClick: () => unstageAll(items.map((e) => e.path)),
              }
            : {
                label: t('agent.gitStageAll'),
                onClick: () => stageAll(items.map((e) => e.path)),
              };
        return (
          <GitGroup
            key={key}
            label={t(i18nKey)}
            count={items.length}
            collapsed={!!collapsed[key]}
            onToggle={() => setCollapsed((c) => ({ ...c, [key]: !c[key] }))}
            action={groupAction}
          >
            {items.map((entry) => (
              <EntryRow
                key={entry.path}
                path={entry.path}
                status={entry.status}
                expanded={expandedPath === entry.path}
                onToggle={() =>
                  setExpandedPath((cur) => (cur === entry.path ? null : entry.path))
                }
                action={rowAction(entry)}
              >
                {expandedPath === entry.path && (
                  <div className="pl-4">
                    <DiffView
                      workspaceId={workspaceId}
                      path={entry.path}
                      cached={groupOf(entry) === 'staged'}
                    />
                  </div>
                )}
              </EntryRow>
            ))}
          </GitGroup>
        );
      })}

      {/* 提交区 */}
      <div className="space-y-1 border-t border-border/60 pt-1.5">
        <textarea
          value={message}
          onChange={(e) => setMessage(e.target.value)}
          rows={2}
          spellCheck={false}
          placeholder={t('agent.gitCommitPlaceholder')}
          aria-label={t('agent.gitCommit')}
          className={cn(
            'w-full resize-none rounded-md border border-input bg-background px-2 py-1.5 font-mono text-xs outline-none focus:border-primary',
          )}
        />
        <div className="flex items-center justify-end">
          <button
            type="button"
            onClick={() => commitMutation.mutate(message.trim())}
            disabled={!canCommit}
            className={cn(
              'inline-flex items-center gap-1 rounded px-2 py-1 text-[11px] font-medium',
              canCommit
                ? 'bg-primary text-primary-foreground hover:bg-primary/90'
                : 'cursor-not-allowed bg-muted text-muted-foreground/60'
            )}
          >
            <GitCommitHorizontal className="h-3 w-3" />
            {t('agent.gitCommit')}
          </button>
        </div>
      </div>

      {/* 操作错误提示（非审批类：升级提示 / git 命令失败等） */}
      {(stageMutation.error ||
        unstageMutation.error ||
        commitMutation.error ||
        pullMutation.error ||
        pushMutation.error) && (
        <p
          className="px-1 text-xs text-destructive"
          role="alert"
          data-testid="git-operation-error"
        >
          {stageMutation.error ||
            unstageMutation.error ||
            commitMutation.error ||
            pullMutation.error ||
            pushMutation.error}
        </p>
      )}

      <GitApprovalDialog
        approval={stageMutation.approval}
        onConfirm={stageMutation.confirmApproval}
        onCancel={stageMutation.cancelApproval}
      />
      <GitApprovalDialog
        approval={unstageMutation.approval}
        onConfirm={unstageMutation.confirmApproval}
        onCancel={unstageMutation.cancelApproval}
      />
      <GitApprovalDialog
        approval={commitMutation.approval}
        onConfirm={commitMutation.confirmApproval}
        onCancel={commitMutation.cancelApproval}
      />
      <GitApprovalDialog
        approval={pullMutation.approval}
        onConfirm={pullMutation.confirmApproval}
        onCancel={pullMutation.cancelApproval}
      />
      <GitApprovalDialog
        approval={pushMutation.approval}
        onConfirm={pushMutation.confirmApproval}
        onCancel={pushMutation.cancelApproval}
      />
    </div>
  );
}
