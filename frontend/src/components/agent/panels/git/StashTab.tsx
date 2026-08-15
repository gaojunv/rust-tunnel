import { useState } from 'react';
import type { ReactNode } from 'react';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import { Archive, ArrowDownToLine, Play, Trash2 } from 'lucide-react';
import { cn } from '@/lib/utils';
import { useImeGuard } from '@/hooks/useImeGuard';
import {
  getAgentGitStashes,
  postAgentGitStashApply,
  postAgentGitStashDrop,
  postAgentGitStashPop,
  postAgentGitStashPush,
} from '../../../../api/client';
import type { GitStashEntry } from '../../../../types';
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
 * Stash Tab：stash 列表（stash@{index}: message）；操作：push（可选 message）、
 * apply / pop / drop（带 index，drop 确认框）。
 */
export function GitStashTab({ workspaceId }: { workspaceId: string }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [stashMsg, setStashMsg] = useState('');
  const ime = useImeGuard();
  const [dropTarget, setDropTarget] = useState<GitStashEntry | null>(null);

  const stashesQuery = useQuery<GitStashEntry[]>({
    queryKey: ['agent-git-stash', workspaceId],
    queryFn: () => getAgentGitStashes(workspaceId),
    retry: false,
  });

  const invalidate = () => {
    queryClient.invalidateQueries({ queryKey: ['agent-git-status', workspaceId] });
    queryClient.invalidateQueries({ queryKey: ['agent-git-stash', workspaceId] });
    queryClient.invalidateQueries({ queryKey: ['agent-git-log', workspaceId] });
  };

  const pushMutation = useGitMutation(
    (approved, message: string) =>
      postAgentGitStashPush(workspaceId, message.trim() !== '' ? message.trim() : undefined, approved),
    {
      onSuccess: () => {
        invalidate();
        setStashMsg('');
      },
    },
  );
  const applyMutation = useGitMutation(
    (approved, index: number) => postAgentGitStashApply(workspaceId, index, approved),
    { onSuccess: invalidate },
  );
  const popMutation = useGitMutation(
    (approved, index: number) => postAgentGitStashPop(workspaceId, index, approved),
    { onSuccess: invalidate },
  );
  const dropMutation = useGitMutation(
    (approved, index: number) => postAgentGitStashDrop(workspaceId, index, approved),
    {
      onSuccess: () => {
        invalidate();
        setDropTarget(null);
      },
    },
  );

  const confirmDrop = () => {
    if (!dropTarget) return;
    setDropTarget(null);
    dropMutation.mutate(dropTarget.index);
  };

  const busy =
    pushMutation.isPending || applyMutation.isPending || popMutation.isPending || dropMutation.isPending;
  const canPush = !busy;

  const actionButton = (
    label: string,
    icon: ReactNode,
    onClick: () => void,
    disabled: boolean,
  ) => (
    <button
      type="button"
      aria-label={label}
      title={label}
      onClick={onClick}
      disabled={disabled}
      className="shrink-0 rounded p-1 text-muted-foreground opacity-0 transition-opacity group-hover:opacity-100 hover:bg-accent hover:text-foreground disabled:pointer-events-none disabled:opacity-30"
    >
      {icon}
    </button>
  );

  return (
    <div className="space-y-1.5">
      {/* 新建 stash：可选 message */}
      <div className="flex items-center gap-1 px-1">
        <input
          value={stashMsg}
          onChange={(e) => setStashMsg(e.target.value)}
          {...ime.bind}
          onKeyDown={(e) => {
            // IME 组词中回车是确认候选，不触发 stash push
            if (ime.isComposing(e)) return;
            if (e.key === 'Enter' && canPush) pushMutation.mutate(stashMsg);
          }}
          placeholder={t('agent.gitStashPushPlaceholder')}
          aria-label={t('agent.gitStashPush')}
          className="h-6 min-w-0 flex-1 rounded border border-input bg-background px-1.5 font-mono text-xs outline-none focus:border-primary"
        />
        <button
          type="button"
          onClick={() => canPush && pushMutation.mutate(stashMsg)}
          disabled={!canPush}
          title={t('agent.gitStashPush')}
          aria-label={t('agent.gitStashPush')}
          className={cn(
            'inline-flex h-6 w-6 shrink-0 items-center justify-center rounded',
            canPush
              ? 'text-primary hover:bg-accent'
              : 'cursor-not-allowed text-muted-foreground/50'
          )}
        >
          <Archive className="h-3.5 w-3.5" />
        </button>
      </div>

      {stashesQuery.isLoading ? (
        <p className="px-1 py-2 text-xs text-muted-foreground">{t('common.loading')}</p>
      ) : stashesQuery.isError ? (
        <p className="px-1 text-xs text-muted-foreground">{t('agent.clientOffline')}</p>
      ) : (stashesQuery.data?.length ?? 0) === 0 ? (
        <p className="px-1 text-xs text-muted-foreground">{t('agent.gitNoStashes')}</p>
      ) : (
        <div className="space-y-0.5">
          {(stashesQuery.data ?? []).map((stash) => (
            <div key={stash.index} className="group flex items-center gap-1 rounded px-1 py-0.5 hover:bg-accent/50">
              <span className="w-14 shrink-0 font-mono text-[10px] text-muted-foreground">
                stash@{stash.index}
              </span>
              <span className="min-w-0 flex-1 truncate text-xs">
                {stash.message || t('agent.gitStashNoMessage')}
              </span>
              {actionButton(
                t('agent.gitStashApply'),
                <ArrowDownToLine className="h-3 w-3" />,
                () => applyMutation.mutate(stash.index),
                busy,
              )}
              {actionButton(
                t('agent.gitStashPop'),
                <Play className="h-3 w-3" />,
                () => popMutation.mutate(stash.index),
                busy,
              )}
              {actionButton(
                t('agent.gitStashDrop'),
                <Trash2 className="h-3 w-3" />,
                () => setDropTarget(stash),
                busy,
              )}
            </div>
          ))}
        </div>
      )}

      {(pushMutation.error || applyMutation.error || popMutation.error || dropMutation.error) && (
        <p className="px-1 text-xs text-destructive" role="alert" data-testid="git-operation-error">
          {pushMutation.error ||
            applyMutation.error ||
            popMutation.error ||
            dropMutation.error}
        </p>
      )}

      {/* drop 确认框 */}
      <Dialog open={dropTarget !== null} onOpenChange={(open) => !open && setDropTarget(null)}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>{t('agent.gitConfirmStashDropTitle')}</DialogTitle>
            <DialogDescription>
              {t('agent.gitConfirmStashDropDesc', { index: dropTarget?.index ?? '' })}
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="ghost" size="sm" onClick={() => setDropTarget(null)}>
              {t('agent.cancel')}
            </Button>
            <Button
              variant="destructive"
              size="sm"
              onClick={confirmDrop}
              disabled={dropMutation.isPending}
            >
              {t('agent.gitStashDrop')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <GitApprovalDialog
        approval={pushMutation.approval}
        onConfirm={pushMutation.confirmApproval}
        onCancel={pushMutation.cancelApproval}
      />
      <GitApprovalDialog
        approval={applyMutation.approval}
        onConfirm={applyMutation.confirmApproval}
        onCancel={applyMutation.cancelApproval}
      />
      <GitApprovalDialog
        approval={popMutation.approval}
        onConfirm={popMutation.confirmApproval}
        onCancel={popMutation.cancelApproval}
      />
      <GitApprovalDialog
        approval={dropMutation.approval}
        onConfirm={dropMutation.confirmApproval}
        onCancel={dropMutation.cancelApproval}
      />
    </div>
  );
}
