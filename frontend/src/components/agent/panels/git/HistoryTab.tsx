import { useState } from 'react';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import { ChevronDown, ChevronRight, GitCommitHorizontal, RotateCcw, Undo2 } from 'lucide-react';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '../../../../components/ui/dropdown-menu';
import { Button } from '../../../../components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '../../../../components/ui/dialog';
import { cn } from '@/lib/utils';
import { getAgentGitLog, getAgentGitShow, postAgentGitReset, postAgentGitRevert } from '../../../../api/client';
import type { GitCommit } from '../../../../types';
import { useGitMutation } from './useGitMutation';
import { GitApprovalDialog } from './shared';
import { formatCommitDate } from './gitUtils';
import type { TranslateFn } from '../../formatRelativeTime';

export type ResetMode = 'soft' | 'mixed' | 'hard';

const RESET_MODES: ResetMode[] = ['soft', 'mixed', 'hard'];

/** 提交详情 diff：git show <rev>（含提交元信息 + diff）。 */
function CommitDiff({ workspaceId, rev }: { workspaceId: string; rev: string }) {
  const { t } = useTranslation();
  const showQuery = useQuery({
    queryKey: ['agent-git-show', workspaceId, rev],
    queryFn: () => getAgentGitShow(workspaceId, rev),
    retry: false,
  });

  if (showQuery.isLoading) {
    return <div className="px-2 py-1 text-xs text-muted-foreground">{t('common.loading')}</div>;
  }
  if (showQuery.isError) {
    return (
      <div className="px-2 py-1 text-xs text-muted-foreground">{t('agent.noGitStatus')}</div>
    );
  }
  const lines = (showQuery.data ?? '').split('\n');
  if (lines.length <= 1 && lines[0].trim() === '') {
    return <div className="px-2 py-1 text-xs text-muted-foreground">{t('agent.diffEmpty')}</div>;
  }
  return (
    <pre className="overflow-x-auto whitespace-pre-wrap break-words rounded-md bg-muted/40 px-2 py-1 font-mono text-[11px] leading-relaxed">
      {lines.map((line, i) => (
        <div key={i} className={diffLineClass(line)}>
          {line === '' ? ' ' : line}
        </div>
      ))}
    </pre>
  );
}

function diffLineClass(line: string): string {
  if (line.startsWith('@@')) return 'text-blue-500';
  if (line.startsWith('+') && !line.startsWith('+++')) {
    return 'text-green-600 dark:text-green-400';
  }
  if (line.startsWith('-') && !line.startsWith('---')) return 'text-red-600';
  return '';
}

/**
 * History Tab：提交列表（short hash / subject / author / 相对日期）；点开看 git show
 * diff；每条目操作：revert（确认框）、reset 到此处（soft/mixed/hard，hard 强确认）。
 */
export function GitHistoryTab({ workspaceId }: { workspaceId: string }) {
  const { t } = useTranslation();
  // 严格类型化 t（key 为字面量联合）无法直接满足 formatCommitDate 的宽签
  // （key: string），与 SessionBar 同法做窄化转型。
  const translate = t as unknown as TranslateFn;
  const queryClient = useQueryClient();
  const [expanded, setExpanded] = useState<string | null>(null);
  const [revertTarget, setRevertTarget] = useState<GitCommit | null>(null);
  const [resetTarget, setResetTarget] = useState<{ commit: GitCommit; mode: ResetMode } | null>(null);

  const logQuery = useQuery<GitCommit[]>({
    queryKey: ['agent-git-log', workspaceId],
    queryFn: () => getAgentGitLog(workspaceId, 50),
    retry: false,
  });

  const invalidate = () => {
    queryClient.invalidateQueries({ queryKey: ['agent-git-status', workspaceId] });
    queryClient.invalidateQueries({ queryKey: ['agent-git-log', workspaceId] });
    queryClient.invalidateQueries({ queryKey: ['agent-git-stash', workspaceId] });
  };

  const revertMutation = useGitMutation(
    (approved, rev: string) => postAgentGitRevert(workspaceId, rev, approved),
    {
      onSuccess: () => {
        invalidate();
        setRevertTarget(null);
      },
    },
  );
  const resetMutation = useGitMutation(
    (approved, rev: string, mode: ResetMode) => postAgentGitReset(workspaceId, mode, rev, approved),
    {
      onSuccess: () => {
        invalidate();
        setResetTarget(null);
      },
    },
  );

  const confirmRevert = () => {
    if (!revertTarget) return;
    setRevertTarget(null);
    revertMutation.mutate(revertTarget.hash);
  };

  const confirmReset = () => {
    if (!resetTarget) return;
    const { commit, mode } = resetTarget;
    setResetTarget(null);
    resetMutation.mutate(commit.hash, mode);
  };

  return (
    <div className="space-y-1.5">
      {logQuery.isLoading ? (
        <p className="px-1 py-2 text-xs text-muted-foreground">{t('common.loading')}</p>
      ) : logQuery.isError ? (
        <p className="px-1 text-xs text-muted-foreground">{t('agent.clientOffline')}</p>
      ) : (logQuery.data?.length ?? 0) === 0 ? (
        <p className="px-1 text-xs text-muted-foreground">{t('agent.gitNoCommits')}</p>
      ) : (
        <div className="space-y-0.5">
          {(logQuery.data ?? []).map((commit) => (
            <div
              key={commit.hash}
              className={cn(
                'group rounded px-1 py-0.5 hover:bg-accent/50',
                expanded === commit.hash && 'bg-accent/40'
              )}
            >
              <div className="flex items-center gap-1">
                <button
                  type="button"
                  onClick={() => setExpanded((cur) => (cur === commit.hash ? null : commit.hash))}
                  aria-expanded={expanded === commit.hash}
                  aria-label={t('agent.gitShowCommit')}
                  className="flex min-w-0 flex-1 items-center gap-1.5 py-0.5 text-left"
                >
                  {expanded === commit.hash ? (
                    <ChevronDown className="h-3 w-3 shrink-0 text-muted-foreground" />
                  ) : (
                    <ChevronRight className="h-3 w-3 shrink-0 text-muted-foreground" />
                  )}
                  <GitCommitHorizontal className="h-3 w-3 shrink-0 text-muted-foreground" />
                  <span className="shrink-0 font-mono text-[10px] text-muted-foreground">
                    {commit.short}
                  </span>
                  <span className="min-w-0 flex-1 truncate text-xs">{commit.subject}</span>
                </button>
                {/* 操作：reset 菜单 + revert */}
                <DropdownMenu>
                  <DropdownMenuTrigger asChild>
                    <button
                      type="button"
                      aria-label={t('agent.gitReset')}
                      title={t('agent.gitReset')}
                      className="shrink-0 rounded p-1 text-muted-foreground opacity-100 md:opacity-0 md:group-hover:opacity-100 transition-opacity hover:bg-accent hover:text-foreground"
                    >
                      <RotateCcw className="h-3 w-3" />
                    </button>
                  </DropdownMenuTrigger>
                  <DropdownMenuContent align="end">
                    <DropdownMenuLabel>{t('agent.gitResetTo', { short: commit.short })}</DropdownMenuLabel>
                    <DropdownMenuSeparator />
                    {RESET_MODES.map((mode) => (
                      <DropdownMenuItem
                        key={mode}
                        onSelect={() => setResetTarget({ commit, mode })}
                      >
                        <span className="font-mono">{mode}</span>
                        <span className="ml-1 text-xs text-muted-foreground">
                          {t(`agent.gitResetMode_${mode}`)}
                        </span>
                      </DropdownMenuItem>
                    ))}
                  </DropdownMenuContent>
                </DropdownMenu>
                <button
                  type="button"
                  onClick={() => setRevertTarget(commit)}
                  aria-label={t('agent.gitRevert')}
                  title={t('agent.gitRevert')}
                  className="shrink-0 rounded p-1 text-muted-foreground opacity-100 md:opacity-0 md:group-hover:opacity-100 transition-opacity hover:bg-accent hover:text-destructive"
                >
                  <Undo2 className="h-3 w-3" />
                </button>
              </div>

              {expanded === commit.hash && (
                <div className="space-y-0.5 pb-1 pl-4">
                  <p className="truncate text-[10px] text-muted-foreground/70">
                    {commit.author} · {formatCommitDate(commit.date, Date.now(), translate)}
                  </p>
                  <CommitDiff workspaceId={workspaceId} rev={commit.hash} />
                </div>
              )}
            </div>
          ))}
        </div>
      )}

      {revertMutation.error && (
        <p className="px-1 text-xs text-destructive" role="alert" data-testid="git-operation-error">
          {revertMutation.error}
        </p>
      )}
      {resetMutation.error && (
        <p className="px-1 text-xs text-destructive" role="alert" data-testid="git-operation-error">
          {resetMutation.error}
        </p>
      )}

      {/* revert 确认框 */}
      <Dialog open={revertTarget !== null} onOpenChange={(open) => !open && setRevertTarget(null)}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>{t('agent.gitConfirmRevertTitle')}</DialogTitle>
            <DialogDescription>
              {t('agent.gitConfirmRevertDesc', { short: revertTarget?.short ?? '' })}
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="ghost" size="sm" onClick={() => setRevertTarget(null)}>
              {t('agent.cancel')}
            </Button>
            <Button
              variant="default"
              size="sm"
              onClick={confirmRevert}
              disabled={revertMutation.isPending}
            >
              {t('agent.gitRevert')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* reset 确认框：hard 模式强提示 */}
      <Dialog open={resetTarget !== null} onOpenChange={(open) => !open && setResetTarget(null)}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>{t('agent.gitConfirmResetTitle')}</DialogTitle>
            <DialogDescription>
              {t(
                resetTarget?.mode === 'hard'
                  ? 'agent.gitConfirmHardResetDesc'
                  : 'agent.gitConfirmResetDesc',
                {
                  short: resetTarget?.commit.short ?? '',
                  mode: resetTarget?.mode ?? '',
                },
              )}
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="ghost" size="sm" onClick={() => setResetTarget(null)}>
              {t('agent.cancel')}
            </Button>
            <Button
              variant={resetTarget?.mode === 'hard' ? 'destructive' : 'default'}
              size="sm"
              onClick={confirmReset}
              disabled={resetMutation.isPending}
            >
              {t('agent.gitReset')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <GitApprovalDialog
        approval={revertMutation.approval}
        onConfirm={revertMutation.confirmApproval}
        onCancel={revertMutation.cancelApproval}
      />
      <GitApprovalDialog
        approval={resetMutation.approval}
        onConfirm={resetMutation.confirmApproval}
        onCancel={resetMutation.cancelApproval}
      />
    </div>
  );
}
