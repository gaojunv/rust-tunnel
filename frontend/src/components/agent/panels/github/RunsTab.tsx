import { useState } from 'react';
import { useQuery } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import { ChevronDown, ChevronRight, ExternalLink, Loader2, RotateCcw, Square } from 'lucide-react';
import { cn } from '@/lib/utils';
import {
  getAgentGithubJobLogs,
  getAgentGithubRunJobs,
  postAgentGithubCancel,
  postAgentGithubRerun,
} from '../../../../api/client';
import type { GhJob, GhWorkflow, GhWorkflowRun } from '../../../../types';
import { useApprovalMutation } from '../useApprovalMutation';
import { Dialog, DialogContent, DialogHeader, DialogTitle } from '../../../../components/ui/dialog';
import { ApprovalDialog } from '../git/shared';
import type { TranslateFn } from '../../formatRelativeTime';
import {
  conclusionBadgeClass,
  conclusionLabel,
  formatGhTime,
  isRunActive,
} from './githubUtils';
import { GithubErrorBanner, GithubMutationError } from './shared';

/** 单次运行的作业列表：点开展开时懒加载（query 挂在展开处，收起即卸载）。 */
function RunJobsList({ workspaceId, runId }: { workspaceId: string; runId: number }) {
  const { t } = useTranslation();
  // 动态 key（agent.githubRunStatus_<status>）需宽签名 t（i18next 严格 key 联合不适用）
  const translate = t as unknown as TranslateFn;
  const [logJob, setLogJob] = useState<GhJob | null>(null);
  const jobsQuery = useQuery({
    queryKey: ['agent-github-jobs', workspaceId, runId],
    queryFn: () => getAgentGithubRunJobs(workspaceId, String(runId)),
    retry: false,
  });

  if (jobsQuery.isLoading) {
    return (
      <div className="flex items-center gap-1 px-1 py-0.5 text-[10px] text-muted-foreground">
        <Loader2 className="h-3 w-3 animate-spin" />
        {t('common.loading')}
      </div>
    );
  }
  if (jobsQuery.isError) {
    return (
      <div className="py-0.5">
        <GithubErrorBanner error={jobsQuery.error} />
      </div>
    );
  }
  const jobs = jobsQuery.data?.jobs ?? [];
  if (jobs.length === 0) {
    return <p className="px-1 py-0.5 text-[10px] text-muted-foreground">{t('agent.githubNoJobs')}</p>;
  }

  return (
    <div className="space-y-0.5 pl-3">
      {jobs.map((job) => (
        <div
          key={job.id}
          className="group flex items-center gap-1 rounded px-1 py-0.5 hover:bg-accent/40"
        >
          <span
            className={cn(
              'h-1.5 w-1.5 shrink-0 rounded-full',
              isRunActive(job.status)
                ? 'bg-yellow-500'
                : job.conclusion === 'success'
                  ? 'bg-green-500'
                  : job.conclusion === 'failure' || job.conclusion === 'timed_out'
                    ? 'bg-red-500'
                    : 'bg-muted-foreground/40'
            )}
          />
          <span className="min-w-0 flex-1 truncate text-[11px]">{job.name}</span>
          <span className="shrink-0 text-[10px] text-muted-foreground">
            {isRunActive(job.status)
              ? translate(`agent.githubRunStatus_${job.status}`)
              : conclusionLabel(job.conclusion, translate)}
          </span>
          <button
            type="button"
            aria-label={`${t('agent.githubViewLogs')} ${job.name}`}
            title={t('agent.githubViewLogs')}
            onClick={() => setLogJob(job)}
            className="shrink-0 rounded p-0.5 text-muted-foreground opacity-0 transition-opacity group-hover:opacity-100 hover:bg-accent hover:text-foreground"
          >
            <span className="text-[10px]">{t('agent.githubViewLogs')}</span>
          </button>
        </div>
      ))}
      {logJob && <JobLogsDialog workspaceId={workspaceId} job={logJob} onClose={() => setLogJob(null)} />}
    </div>
  );
}

/** 作业日志弹层：打开时懒加载（enabled=open），truncated 时提示仅含尾部。 */
function JobLogsDialog({
  workspaceId,
  job,
  onClose,
}: {
  workspaceId: string;
  job: GhJob;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const logsQuery = useQuery({
    queryKey: ['agent-github-logs', workspaceId, job.id],
    queryFn: () => getAgentGithubJobLogs(workspaceId, String(job.id)),
    retry: false,
  });
  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="max-w-2xl">
        <DialogHeader>
          <DialogTitle>
            {t('agent.githubLogsTitle')} · {job.name}
          </DialogTitle>
        </DialogHeader>
        {logsQuery.isLoading && (
          <div className="flex items-center gap-1 px-1 py-2 text-xs text-muted-foreground">
            <Loader2 className="h-3.5 w-3.5 animate-spin" />
            {t('common.loading')}
          </div>
        )}
        {logsQuery.isError && <GithubErrorBanner error={logsQuery.error} />}
        {logsQuery.data && (
          <div className="space-y-1">
            {logsQuery.data.truncated && (
              <p className="text-xs text-muted-foreground">{t('agent.githubLogsTruncated')}</p>
            )}
            <pre className="max-h-[55vh] overflow-auto whitespace-pre-wrap break-all rounded-md bg-muted p-2 font-mono text-xs leading-relaxed">
              {logsQuery.data.logs || t('agent.githubLogsEmpty')}
            </pre>
          </div>
        )}
      </DialogContent>
    </Dialog>
  );
}

/** 单条运行行：点击展开作业列表；hover 显示 rerun / cancel（进行中）/ 外链。 */
function RunRow({
  workspaceId,
  run,
  expanded,
  onToggle,
  onRerun,
  onCancel,
}: {
  workspaceId: string;
  run: GhWorkflowRun;
  expanded: boolean;
  onToggle: () => void;
  onRerun: (runId: number) => void;
  onCancel: (runId: number) => void;
}) {
  const { t } = useTranslation();
  const translate = t as unknown as TranslateFn;
  const active = isRunActive(run.status);
  const title = run.display_title || run.name || `#${run.id}`;
  const time = formatGhTime(run.run_started_at, translate);
  return (
    <div className="group rounded border border-border/60">
      <button
        type="button"
        aria-expanded={expanded}
        onClick={onToggle}
        className="flex w-full items-center gap-1.5 rounded px-1.5 py-1 text-left hover:bg-accent/50"
      >
        {expanded ? (
          <ChevronDown className="h-3 w-3 shrink-0 text-muted-foreground" />
        ) : (
          <ChevronRight className="h-3 w-3 shrink-0 text-muted-foreground" />
        )}
        {active ? (
          <Loader2 className="h-3 w-3 shrink-0 animate-spin text-yellow-500" />
        ) : (
          <span
            className={cn(
              'inline-flex h-4 shrink-0 items-center rounded px-1 text-[10px] font-medium',
              conclusionBadgeClass(run.conclusion)
            )}
          >
            {conclusionLabel(run.conclusion, translate) || t('agent.githubStatusUnknown')}
          </span>
        )}
        <span className="min-w-0 flex-1">
          <span className="block truncate text-xs font-medium">{title}</span>
          <span className="block truncate text-[10px] text-muted-foreground">
            {run.head_branch && <span className="font-mono">{run.head_branch}</span>}
            {run.head_branch && time && <span> · </span>}
            {time}
          </span>
        </span>
      </button>
      <div className="flex items-center justify-end gap-0.5 px-1 pb-0.5">
        <button
          type="button"
          aria-label={`${t('agent.githubRerun')} ${title}`}
          title={t('agent.githubRerun')}
          onClick={() => onRerun(run.id)}
          className="flex items-center gap-0.5 rounded px-1 py-0.5 text-[10px] text-muted-foreground opacity-0 transition-opacity hover:bg-accent hover:text-foreground group-hover:opacity-100"
        >
          <RotateCcw className="h-2.5 w-2.5" />
          {t('agent.githubRerun')}
        </button>
        {active && (
          <button
            type="button"
            aria-label={`${t('agent.githubCancel')} ${title}`}
            title={t('agent.githubCancel')}
            onClick={() => onCancel(run.id)}
            className="flex items-center gap-0.5 rounded px-1 py-0.5 text-[10px] text-muted-foreground opacity-0 transition-opacity hover:bg-destructive/10 hover:text-destructive group-hover:opacity-100"
          >
            <Square className="h-2.5 w-2.5" />
            {t('agent.githubCancel')}
          </button>
        )}
        {run.html_url && (
          <a
            href={run.html_url}
            target="_blank"
            rel="noreferrer"
            aria-label={`${t('agent.githubOpenRun')} ${title}`}
            title={t('agent.githubOpenRun')}
            className="flex items-center gap-0.5 rounded px-1 py-0.5 text-[10px] text-muted-foreground opacity-0 transition-opacity hover:bg-accent hover:text-foreground group-hover:opacity-100"
          >
            <ExternalLink className="h-2.5 w-2.5" />
          </a>
        )}
      </div>
      {expanded && (
        <div className="border-t border-border/40 pb-1 pt-0.5">
          <RunJobsList workspaceId={workspaceId} runId={run.id} />
        </div>
      )}
    </div>
  );
}

/**
 * Runs Tab：运行列表 + 顶部按工作流过滤下拉。运行行点开作业列表，作业可查看日志；
 * rerun / cancel 走 409 审批流（复用 useApprovalMutation），成功后刷新容器 runs query。
 */
export function RunsTab({
  workspaceId,
  runs,
  isLoading,
  isError,
  error,
  workflows,
  workflowFilter,
  onFilterChange,
  invalidateRuns,
}: {
  workspaceId: string;
  runs?: GhWorkflowRun[];
  isLoading: boolean;
  isError: boolean;
  error: unknown;
  workflows?: GhWorkflow[];
  workflowFilter: string;
  onFilterChange: (workflowId: string) => void;
  invalidateRuns: () => void;
}) {
  const { t } = useTranslation();
  const [expandedId, setExpandedId] = useState<number | null>(null);

  const rerunMutation = useApprovalMutation(
    (approved, runId: number) => postAgentGithubRerun(workspaceId, String(runId), approved),
    { onSuccess: invalidateRuns },
  );
  const cancelMutation = useApprovalMutation(
    (approved, runId: number) => postAgentGithubCancel(workspaceId, String(runId), approved),
    { onSuccess: invalidateRuns },
  );

  return (
    <div className="space-y-1.5">
      {/* 顶部：工作流过滤下拉 */}
      <div className="px-0.5">
        <select
          value={workflowFilter}
          onChange={(e) => onFilterChange(e.target.value)}
          aria-label={t('agent.githubFilterWorkflow')}
          className="h-8 w-full rounded-md border border-input bg-background px-2 text-xs"
        >
          <option value="">{t('agent.githubAllWorkflows')}</option>
          {(workflows ?? []).map((wf) => (
            <option key={wf.id} value={String(wf.id)}>
              {wf.name}
            </option>
          ))}
        </select>
      </div>

      {isError && <GithubErrorBanner error={error} />}

      {isLoading && (
        <div className="flex items-center gap-1 px-1 py-1 text-xs text-muted-foreground">
          <Loader2 className="h-3.5 w-3.5 animate-spin" />
          {t('common.loading')}
        </div>
      )}

      {!isLoading && !isError && (runs ?? []).length === 0 && (
        <p className="px-1 text-xs text-muted-foreground">{t('agent.githubNoRuns')}</p>
      )}

      {(runs ?? []).map((run) => (
        <RunRow
          key={run.id}
          workspaceId={workspaceId}
          run={run}
          expanded={expandedId === run.id}
          onToggle={() => setExpandedId((cur) => (cur === run.id ? null : run.id))}
          onRerun={(id) => rerunMutation.mutate(id)}
          onCancel={(id) => cancelMutation.mutate(id)}
        />
      ))}

      <GithubMutationError error={rerunMutation.error ?? cancelMutation.error} />
      <ApprovalDialog
        approval={rerunMutation.approval}
        onConfirm={rerunMutation.confirmApproval}
        onCancel={rerunMutation.cancelApproval}
        descKey="agent.githubApprovalDesc"
      />
      <ApprovalDialog
        approval={cancelMutation.approval}
        onConfirm={cancelMutation.confirmApproval}
        onCancel={cancelMutation.cancelApproval}
        descKey="agent.githubApprovalDesc"
      />
    </div>
  );
}
