import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Loader2, Play, X } from 'lucide-react';
import { cn } from '@/lib/utils';
import { postAgentGithubDispatch } from '../../../../api/client';
import type { GhWorkflow } from '../../../../types';
import { useApprovalMutation } from '../useApprovalMutation';
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '../../../../components/ui/dialog';
import { Button } from '../../../../components/ui/button';
import { Input } from '../../../../components/ui/input';
import { Label } from '../../../../components/ui/label';
import { ApprovalDialog } from '../git/shared';
import { serializeGhInputs } from './githubUtils';
import { GithubErrorBanner, GithubMutationError } from './shared';

/** workflow_dispatch 触发对话框：ref 必填（默认探测分支/main），inputs 可选 KV 行。 */
function DispatchDialog({
  workspaceId,
  workflow,
  defaultRef,
  onClose,
  onSuccess,
}: {
  workspaceId: string;
  workflow: GhWorkflow;
  defaultRef: string;
  onClose: () => void;
  onSuccess: () => void;
}) {
  const { t } = useTranslation();
  const [ref, setRef] = useState(defaultRef);
  const [rows, setRows] = useState<{ key: string; value: string }[]>([]);
  const [dispatched, setDispatched] = useState(false);

  const dispatchMutation = useApprovalMutation(
    (approved, payload: { ref: string; inputs: Record<string, string> }) =>
      postAgentGithubDispatch(
        workspaceId,
        String(workflow.id),
        payload.ref,
        payload.inputs,
        approved,
      ),
    {
      onSuccess: () => {
        setDispatched(true);
        onSuccess();
      },
    },
  );

  const canDispatch = ref.trim() !== '' && !dispatchMutation.isPending;
  const submit = () => {
    setDispatched(false);
    dispatchMutation.mutate({ ref: ref.trim(), inputs: serializeGhInputs(rows) });
  };

  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>
            {t('agent.githubDispatchTitle')} · {workflow.name}
          </DialogTitle>
        </DialogHeader>
        <div className="space-y-3">
          <div className="space-y-1">
            <Label>{t('agent.githubRef')}</Label>
            <Input
              value={ref}
              onChange={(e) => setRef(e.target.value)}
              placeholder={t('agent.githubRefPlaceholder')}
            />
          </div>
          <div className="space-y-1">
            <Label>{t('agent.githubInputs')}</Label>
            {rows.map((row, i) => (
              <div key={i} className="flex items-center gap-1.5">
                <Input
                  value={row.key}
                  onChange={(e) =>
                    setRows((rs) => rs.map((r, j) => (j === i ? { ...r, key: e.target.value } : r)))
                  }
                  placeholder={t('agent.githubInputKeyPlaceholder')}
                  aria-label={`${t('agent.githubInputs')} key ${i + 1}`}
                  className="w-36"
                />
                <Input
                  value={row.value}
                  onChange={(e) =>
                    setRows((rs) => rs.map((r, j) => (j === i ? { ...r, value: e.target.value } : r)))
                  }
                  placeholder={t('agent.githubInputValuePlaceholder')}
                  aria-label={`${t('agent.githubInputs')} value ${i + 1}`}
                />
                <button
                  type="button"
                  aria-label={`${t('agent.githubInputRemove')} ${i + 1}`}
                  onClick={() => setRows((rs) => rs.filter((_, j) => j !== i))}
                  className="shrink-0 rounded p-1 text-muted-foreground hover:bg-accent hover:text-foreground"
                >
                  <X className="h-3.5 w-3.5" />
                </button>
              </div>
            ))}
            <button
              type="button"
              onClick={() => setRows((rs) => [...rs, { key: '', value: '' }])}
              className="rounded px-1 py-0.5 text-[11px] text-muted-foreground hover:bg-accent hover:text-foreground"
            >
              {t('agent.githubInputAdd')}
            </button>
            <p className="text-xs text-muted-foreground">{t('agent.githubInputsHint')}</p>
          </div>
          {dispatched && (
            <p className="text-xs font-medium text-green-600 dark:text-green-400" role="status">
              {t('agent.githubDispatchSuccess')}
            </p>
          )}
          <GithubMutationError error={dispatchMutation.error} />
        </div>
        <DialogFooter>
          <Button variant="outline" size="sm" onClick={onClose}>
            {t('common.cancel')}
          </Button>
          <Button size="sm" onClick={submit} disabled={!canDispatch}>
            {dispatchMutation.isPending && <Loader2 className="mr-1 h-3.5 w-3.5 animate-spin" />}
            <Play className="mr-1 h-3.5 w-3.5" />
            {t('agent.githubDispatch')}
          </Button>
        </DialogFooter>
        <ApprovalDialog
          approval={dispatchMutation.approval}
          onConfirm={dispatchMutation.confirmApproval}
          onCancel={dispatchMutation.cancelApproval}
          descKey="agent.githubApprovalDesc"
        />
      </DialogContent>
    </Dialog>
  );
}

/**
 * Workflows Tab：工作流列表（name / state / path），每条「触发」→ dispatch 对话框
 * （ref 必填、inputs 可选 KV），写操作走 409 审批流；成功后刷新 runs（新运行出现）。
 */
export function WorkflowsTab({
  workspaceId,
  workflows,
  isLoading,
  isError,
  error,
  defaultRef,
  invalidateRuns,
}: {
  workspaceId: string;
  workflows?: GhWorkflow[];
  isLoading: boolean;
  isError: boolean;
  error: unknown;
  /** dispatch 对话框 ref 默认值：仓库探测分支或 main。 */
  defaultRef: string;
  invalidateRuns: () => void;
}) {
  const { t } = useTranslation();
  // 状态 key 由 wf.state 动态拼接，需宽签名 t
  const translate = t as (key: string) => string;
  const [dispatchWorkflow, setDispatchWorkflow] = useState<GhWorkflow | null>(null);

  return (
    <div className="space-y-1">
      {isError && <GithubErrorBanner error={error} />}
      {isLoading && (
        <div className="flex items-center gap-1 px-1 py-1 text-xs text-muted-foreground">
          <Loader2 className="h-3.5 w-3.5 animate-spin" />
          {t('common.loading')}
        </div>
      )}
      {!isLoading && !isError && (workflows ?? []).length === 0 && (
        <p className="px-1 text-xs text-muted-foreground">{t('agent.githubNoWorkflows')}</p>
      )}
      {(workflows ?? []).map((wf) => (
        <div
          key={wf.id}
          className="group flex items-center gap-1.5 rounded border border-border/60 px-1.5 py-1 min-h-[36px] md:min-h-[28px]"
        >
          <span className="min-w-0 flex-1">
            <span className="block truncate text-xs font-medium">{wf.name}</span>
            <span className="block truncate font-mono text-[11px] text-muted-foreground">
              {wf.path}
            </span>
          </span>
          <span
            className={cn(
              'shrink-0 rounded px-1 py-0.5 text-xs font-medium',
              wf.state === 'active'
                ? 'bg-green-500/15 text-green-600 dark:text-green-400'
                : 'bg-muted text-muted-foreground'
            )}
          >
            {translate(`agent.githubWorkflowState_${wf.state}`)}
          </span>
          <button
            type="button"
            onClick={() => setDispatchWorkflow(wf)}
            aria-label={`${t('agent.githubDispatch')} ${wf.name}`}
            className="flex shrink-0 items-center gap-0.5 rounded px-1.5 py-0.5 text-xs font-medium text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
          >
            <Play className="h-3 w-3" />
            {t('agent.githubDispatch')}
          </button>
        </div>
      ))}
      {dispatchWorkflow && (
        <DispatchDialog
          workspaceId={workspaceId}
          workflow={dispatchWorkflow}
          defaultRef={defaultRef}
          onClose={() => setDispatchWorkflow(null)}
          onSuccess={invalidateRuns}
        />
      )}
    </div>
  );
}
