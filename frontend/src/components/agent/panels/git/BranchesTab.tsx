import { useState } from 'react';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import { GitBranchPlus, Trash2 } from 'lucide-react';
import { cn } from '@/lib/utils';
import { useImeGuard } from '@/hooks/useImeGuard';
import {
  getAgentGitBranches,
  postAgentGitBranchDelete,
  postAgentGitCheckout,
} from '../../../../api/client';
import type { GitBranch } from '../../../../types';
import { Button } from '../../../../components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '../../../../components/ui/dialog';
import { useGitMutation } from './useGitMutation';
import { GitApprovalDialog } from './shared';

/**
 * Branches Tab：当前分支高亮 + upstream；新建分支（checkout create:true）；
 * 点击非当前分支切换；删除分支（确认框 + force 复选）。
 */
export function GitBranchesTab({ workspaceId }: { workspaceId: string }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [newBranch, setNewBranch] = useState('');
  const ime = useImeGuard();
  const [deleteTarget, setDeleteTarget] = useState<GitBranch | null>(null);
  const [forceDelete, setForceDelete] = useState(false);

  const branchesQuery = useQuery<GitBranch[]>({
    queryKey: ['agent-git-branches', workspaceId],
    queryFn: () => getAgentGitBranches(workspaceId),
    retry: false,
  });

  const invalidate = () => {
    queryClient.invalidateQueries({ queryKey: ['agent-git-status', workspaceId] });
    queryClient.invalidateQueries({ queryKey: ['agent-git-branches', workspaceId] });
  };

  const checkoutMutation = useGitMutation(
    (approved, name: string, create: boolean) =>
      postAgentGitCheckout(workspaceId, name, create, approved),
    { onSuccess: invalidate },
  );
  const createMutation = useGitMutation(
    (approved, name: string) => postAgentGitCheckout(workspaceId, name, true, approved),
    {
      onSuccess: () => {
        invalidate();
        setNewBranch('');
      },
    },
  );
  const deleteMutation = useGitMutation(
    (approved, branch: string, force: boolean) =>
      postAgentGitBranchDelete(workspaceId, branch, force, approved),
    {
      onSuccess: () => {
        invalidate();
        setDeleteTarget(null);
        setForceDelete(false);
      },
    },
  );

  const confirmDelete = () => {
    if (!deleteTarget) return;
    setDeleteTarget(null);
    setForceDelete(false);
    deleteMutation.mutate(deleteTarget.name, forceDelete);
  };

  const busy =
    checkoutMutation.isPending || createMutation.isPending || deleteMutation.isPending;
  const canCreate = newBranch.trim() !== '' && !busy;

  return (
    <div className="space-y-1.5">
      {/* 新建分支：输入名 → checkout -b */}
      <div className="flex items-center gap-1 px-1">
        <input
          value={newBranch}
          onChange={(e) => setNewBranch(e.target.value)}
          {...ime.bind}
          onKeyDown={(e) => {
            // IME 组词中回车是确认候选，不触发建分支
            if (ime.isComposing(e)) return;
            if (e.key === 'Enter' && canCreate) createMutation.mutate(newBranch.trim());
          }}
          placeholder={t('agent.gitNewBranchPlaceholder')}
          aria-label={t('agent.gitNewBranch')}
          className="h-6 min-w-0 flex-1 rounded border border-input bg-background px-1.5 font-mono text-xs outline-none focus:border-primary"
        />
        <button
          type="button"
          onClick={() => canCreate && createMutation.mutate(newBranch.trim())}
          disabled={!canCreate}
          title={t('agent.gitCreateBranch')}
          aria-label={t('agent.gitCreateBranch')}
          className={cn(
            'inline-flex h-6 w-6 shrink-0 items-center justify-center rounded',
            canCreate
              ? 'text-primary hover:bg-accent'
              : 'cursor-not-allowed text-muted-foreground/50'
          )}
        >
          <GitBranchPlus className="h-3.5 w-3.5" />
        </button>
      </div>

      {branchesQuery.isLoading ? (
        <p className="px-1 py-2 text-xs text-muted-foreground">{t('common.loading')}</p>
      ) : branchesQuery.isError ? (
        <p className="px-1 text-xs text-muted-foreground">{t('agent.clientOffline')}</p>
      ) : (branchesQuery.data?.length ?? 0) === 0 ? (
        <p className="px-1 text-xs text-muted-foreground">{t('agent.gitNoBranches')}</p>
      ) : (
        <div className="space-y-0.5">
          {(branchesQuery.data ?? []).map((b) => (
            <div
              key={b.name}
              className={cn(
                'group flex items-center gap-1 rounded px-1 py-0.5 hover:bg-accent/50',
                b.current && 'bg-accent/40'
              )}
            >
              <button
                type="button"
                onClick={() => !b.current && checkoutMutation.mutate(b.name, false)}
                disabled={b.current || busy}
                aria-label={t('agent.gitCheckoutBranch')}
                title={b.current ? t('agent.gitCurrentBranch') : t('agent.gitCheckoutBranch')}
                className="flex min-w-0 flex-1 items-center gap-1.5 text-left"
              >
                <span
                  className={cn(
                    'h-2 w-2 shrink-0 rounded-full',
                    b.current ? 'bg-primary' : 'border border-muted-foreground/50'
                  )}
                />
                <span
                  className={cn(
                    'truncate font-mono text-xs',
                    b.current ? 'font-semibold text-primary' : 'text-foreground'
                  )}
                >
                  {b.name}
                </span>
                {b.upstream && (
                  <span className="truncate text-[10px] text-muted-foreground/70">
                    → {b.upstream}
                  </span>
                )}
              </button>
              {!b.current && (
                <button
                  type="button"
                  onClick={() => setDeleteTarget(b)}
                  disabled={busy}
                  aria-label={t('agent.gitDeleteBranch')}
                  title={t('agent.gitDeleteBranch')}
                  className="shrink-0 rounded p-1 text-muted-foreground opacity-0 transition-opacity group-hover:opacity-100 hover:bg-accent hover:text-destructive"
                >
                  <Trash2 className="h-3 w-3" />
                </button>
              )}
            </div>
          ))}
        </div>
      )}

      {checkoutMutation.error && (
        <p className="px-1 text-xs text-destructive" role="alert" data-testid="git-operation-error">
          {checkoutMutation.error}
        </p>
      )}
      {createMutation.error && (
        <p className="px-1 text-xs text-destructive" role="alert" data-testid="git-operation-error">
          {createMutation.error}
        </p>
      )}
      {deleteMutation.error && (
        <p className="px-1 text-xs text-destructive" role="alert" data-testid="git-operation-error">
          {deleteMutation.error}
        </p>
      )}

      {/* 删除分支确认框（force 复选项） */}
      <Dialog open={deleteTarget !== null} onOpenChange={(open) => !open && setDeleteTarget(null)}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>{t('agent.gitConfirmDeleteBranchTitle')}</DialogTitle>
            <DialogDescription>
              {t('agent.gitConfirmDeleteBranchDesc', { branch: deleteTarget?.name ?? '' })}
            </DialogDescription>
          </DialogHeader>
          <label className="flex items-center gap-2 text-sm">
            <input
              type="checkbox"
              checked={forceDelete}
              onChange={(e) => setForceDelete(e.target.checked)}
              className="h-4 w-4"
            />
            {t('agent.gitForceDelete')}
          </label>
          <DialogFooter>
            <Button variant="ghost" size="sm" onClick={() => setDeleteTarget(null)}>
              {t('agent.cancel')}
            </Button>
            <Button
              variant="destructive"
              size="sm"
              onClick={confirmDelete}
              disabled={deleteMutation.isPending}
            >
              {t('agent.gitDeleteBranch')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <GitApprovalDialog
        approval={checkoutMutation.approval}
        onConfirm={checkoutMutation.confirmApproval}
        onCancel={checkoutMutation.cancelApproval}
      />
      <GitApprovalDialog
        approval={createMutation.approval}
        onConfirm={createMutation.confirmApproval}
        onCancel={createMutation.cancelApproval}
      />
      <GitApprovalDialog
        approval={deleteMutation.approval}
        onConfirm={deleteMutation.confirmApproval}
        onCancel={deleteMutation.cancelApproval}
      />
    </div>
  );
}
